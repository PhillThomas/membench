/*
 * mbstream.c -- independent Read / Write / Copy bandwidth reference for membench.
 *
 * Why this file exists: membench's definition of done is "within 10% of an
 * established tool on the same hardware", and Intel MLC / lmbench are not
 * installable here.  So this is a second, independent implementation of the
 * same measurement in plain C (no inline asm, nothing shared with the Rust
 * kernels) whose only job is to agree or to explain why it does not.
 *
 * Conventions mirrored from the membench plan (and visible in the output, so a
 * reader can tell what a number means without opening the source):
 *
 *   - SI units: 1 GB/s = 1e9 bytes per second.
 *   - Copy counts source bytes only unless --copy-counts-both is given.
 *   - Buffer sized as a fraction of the level under test: 0.5 x L1d per thread,
 *     0.5 x L2, 0.75 x L3 shared over the cores of the L3 domain (floor 2 x L2),
 *     and at least 1 GiB in total for memory.  Rounded down to the page size.
 *   - Pin to a CPU *before* allocating, so first-touch keeps pages on the local
 *     NUMA node.
 *   - Prefault every page before any timing: no page fault inside a timed window.
 *   - Request 2 MiB pages with madvise(MADV_HUGEPAGE), then VERIFY what the
 *     kernel actually delivered via /proc/self/smaps and print
 *     "huge pages: 0 of N bytes" when it refused.
 *   - Median of N timed trials after one untimed warm-up pass that only sizes
 *     the trials.
 *   - Refuse to present an unoptimised build as a measurement: __OPTIMIZE__ is
 *     checked, a warning is printed, and every number is labelled.
 *
 * Honest limitations: the "default" kernels are ordinary C loops that gcc is
 * expected to vectorise; the "scalar" kernels are portable loops that gcc is
 * still allowed to vectorise (see README.md).  There are no non-temporal paths
 * here at all, so this program never claims to measure them.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

/* SI, exactly as membench's bandwidth.rs defines it. */
#define GBPS 1000000000.0

#define PAGE_SZ 4096u
#define TWO_MIB (2u * 1024u * 1024u)
#define ONE_GIB (1024ull * 1024ull * 1024ull)
#define MIN_BW_BYTES 1024u
#define MAX_THREADS 256
#define MAX_TRIALS 64

/* Compile-time switch for the portable kernels; --isa scalar is the runtime
 * equivalent.  Both only select which C loop runs, they do not stop gcc from
 * vectorising it. */
#ifndef MB_FORCE_SCALAR
#define MB_FORCE_SCALAR 0
#endif

/* ------------------------------------------------------------------ */
/* build honesty                                                       */
/* ------------------------------------------------------------------ */

#ifdef __OPTIMIZE__
static const int optimised_build = 1;
#else
static const int optimised_build = 0;
#endif

/* ------------------------------------------------------------------ */
/* small utilities                                                     */
/* ------------------------------------------------------------------ */

static void die(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    fprintf(stderr, "mbstream: ");
    vfprintf(stderr, fmt, ap);
    fputc('\n', stderr);
    va_end(ap);
    exit(2);
}

static void warn_msg(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    fprintf(stderr, "mbstream: warning: ");
    vfprintf(stderr, fmt, ap);
    fputc('\n', stderr);
    va_end(ap);
}

static double now_s(void)
{
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0)
        die("clock_gettime: %s", strerror(errno));
    return (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
}

/* Read a whole small sysfs/procfs file into buf.  Returns 0 on success. */
static int read_small(const char *path, char *buf, size_t cap)
{
    FILE *f = fopen(path, "rb");
    if (!f)
        return -1;
    size_t n = fread(buf, 1, cap - 1, f);
    fclose(f);
    buf[n] = '\0';
    return 0;
}

/* "32K" / "16384K" / "4096" -> bytes, -1 when unparsable. */
static long long parse_cache_size(const char *s)
{
    char *end = NULL;
    long long v = strtoll(s, &end, 10);
    if (end == s || v < 0)
        return -1;
    while (*end == ' ' || *end == '\t' || *end == '\n')
        end++;
    if (*end == 'K' || *end == 'k')
        v *= 1024;
    else if (*end == 'M' || *end == 'm')
        v *= 1024 * 1024;
    else if (*end == 'G' || *end == 'g')
        v *= 1024 * 1024 * 1024;
    return v;
}

/* Binary human units for buffer and cache sizes; bandwidth stays SI. */
static const char *human_bytes(unsigned long long b)
{
    static const char *u[] = {"B", "KiB", "MiB", "GiB", "TiB"};
    static char buf[4][40];
    static int rot;
    char *out = buf[rot++ & 3];
    double v = (double)b;
    size_t i = 0;
    while (v >= 1024.0 && i + 1 < sizeof(u) / sizeof(u[0])) {
        v /= 1024.0;
        i++;
    }
    if (i == 0)
        snprintf(out, 40, "%llu B", b);
    else
        snprintf(out, 40, "%.1f %s", v, u[i]);
    return out;
}

static int cmp_double(const void *a, const void *b)
{
    double x = *(const double *)a, y = *(const double *)b;
    return (x > y) - (x < y);
}

/* Median exactly as membench's stats.rs defines it: sort, odd -> middle value,
 * even -> mean of the two middle values. */
static double median_of(double *v, size_t n)
{
    if (n == 0)
        return 0.0;
    qsort(v, n, sizeof(double), cmp_double);
    if (n % 2 == 1)
        return v[n / 2];
    return (v[n / 2 - 1] + v[n / 2]) / 2.0;
}

/* ------------------------------------------------------------------ */
/* machine description (read from sysfs; unknown stays unknown)        */
/* ------------------------------------------------------------------ */

typedef struct {
    long long l1d, l2, l3, line;
    int core_cpus[MAX_THREADS]; /* one logical CPU per physical core */
    int ncore;
    int all_cpus[MAX_THREADS];
    int nall;
    char model[128];
} Machine;

static void detect_topology(Machine *m)
{
    char path[256], buf[512];
    for (int cpu = 0; cpu < MAX_THREADS; cpu++) {
        snprintf(path, sizeof(path),
                 "/sys/devices/system/cpu/cpu%d/topology/thread_siblings_list", cpu);
        if (read_small(path, buf, sizeof(buf)) != 0)
            continue;
        if (m->nall < MAX_THREADS)
            m->all_cpus[m->nall++] = cpu;
        /* A CPU is its core's representative when it is the first id listed
         * among its SMT siblings. */
        if (strtoll(buf, NULL, 10) == cpu && m->ncore < MAX_THREADS)
            m->core_cpus[m->ncore++] = cpu;
    }
}

static void detect_caches(Machine *m)
{
    char path[256], buf[128];
    m->line = 64; /* the x86/ARM default; overwritten from sysfs when present */
    for (int idx = 0; idx < 16; idx++) {
        int level;
        long long size;
        int is_instr;

        snprintf(path, sizeof(path), "/sys/devices/system/cpu/cpu0/cache/index%d/level", idx);
        if (read_small(path, buf, sizeof(buf)) != 0)
            continue;
        level = (int)strtol(buf, NULL, 10);

        snprintf(path, sizeof(path), "/sys/devices/system/cpu/cpu0/cache/index%d/type", idx);
        if (read_small(path, buf, sizeof(buf)) != 0)
            continue;
        is_instr = strstr(buf, "Instruction") != NULL;

        snprintf(path, sizeof(path), "/sys/devices/system/cpu/cpu0/cache/index%d/size", idx);
        if (read_small(path, buf, sizeof(buf)) != 0)
            continue;
        size = parse_cache_size(buf);
        if (size <= 0)
            continue;

        if (level == 1 && !is_instr && (m->l1d == 0 || size > m->l1d))
            m->l1d = size;
        else if (level == 2 && (m->l2 == 0 || size > m->l2))
            m->l2 = size;
        else if (level == 3 && (m->l3 == 0 || size > m->l3))
            m->l3 = size;
    }
    if (read_small("/sys/devices/system/cpu/cpu0/cache/index0/coherency_line_size",
                   buf, sizeof(buf)) == 0) {
        long long l = strtoll(buf, NULL, 10);
        if (l >= 8)
            m->line = l;
    }

    if (read_small("/sys/devices/system/cpu/cpu0/model_name", m->model,
                   sizeof(m->model)) == 0) {
        size_t n = strlen(m->model);
        while (n && (m->model[n - 1] == '\n' || m->model[n - 1] == ' '))
            m->model[--n] = '\0';
    } else {
        FILE *f = fopen("/proc/cpuinfo", "r");
        if (f) {
            char line[256];
            while (fgets(line, sizeof(line), f)) {
                char *eq;
                if (!strstr(line, "model name"))
                    continue;
                eq = strchr(line, ':');
                if (!eq)
                    continue;
                eq++;
                while (*eq == ' ')
                    eq++;
                size_t n = strlen(eq);
                while (n && (eq[n - 1] == '\n' || eq[n - 1] == ' ' || eq[n - 1] == '\t'))
                    eq[--n] = '\0';
                snprintf(m->model, sizeof(m->model), "%s", eq);
                break;
            }
            fclose(f);
        }
    }
}

static unsigned long long mem_available_bytes(void)
{
    FILE *f = fopen("/proc/meminfo", "r");
    char line[256];
    unsigned long long kb = 0;
    if (!f)
        return 0;
    while (fgets(line, sizeof(line), f)) {
        if (strncmp(line, "MemAvailable:", 13) == 0) {
            kb = strtoull(line + 13, NULL, 10);
            break;
        }
    }
    fclose(f);
    return kb * 1024ull;
}

/* "always" / "madvise" / "never" from the THP knob, "" when unreadable. */
static void thp_setting(char *out, size_t cap)
{
    char buf[256];
    out[0] = '\0';
    if (read_small("/sys/kernel/mm/transparent_hugepage/enabled", buf, sizeof(buf)) != 0)
        return;
    char *open = strchr(buf, '[');
    char *close = open ? strchr(open, ']') : NULL;
    if (!open || !close)
        return;
    open++;
    size_t n = (size_t)(close - open);
    if (n >= cap)
        n = cap - 1;
    memcpy(out, open, n);
    out[n] = '\0';
}

/* ------------------------------------------------------------------ */
/* buffers: 2 MiB-aligned mmap, THP request, prefault, verification    */
/* ------------------------------------------------------------------ */

typedef struct {
    void *addr;
    size_t len;
    long long huge_bytes; /* AnonHugePages for this mapping; -1 = unknown */
    int madvise_errno;
} Buf;

/*
 * AnonHugePages of the VMA that contains `addr`, from /proc/self/smaps.
 * This is the only honest way to answer "did we get huge pages": the knob can
 * say `always` and the container can still refuse them.
 */
static long long smaps_anon_huge(const void *addr)
{
    FILE *f = fopen("/proc/self/smaps", "r");
    char line[512];
    unsigned long long target = (unsigned long long)(uintptr_t)addr;
    int inside = 0;
    int got = 0;
    long long bytes = -1;

    if (!f)
        return -1;
    while (fgets(line, sizeof(line), f)) {
        unsigned long long a, b;
        if (sscanf(line, "%llx-%llx", &a, &b) == 2) {
            if (inside)
                break; /* walked past our VMA */
            inside = (target >= a && target < b);
            continue;
        }
        if (!inside)
            continue;
        if (strncmp(line, "AnonHugePages:", 14) == 0) {
            bytes = strtoll(line + 14, NULL, 10) * 1024;
            got = 1;
            break;
        }
        if (strncmp(line, "HugetlbPages:", 13) == 0) {
            bytes = strtoll(line + 13, NULL, 10) * 1024;
            got = 1;
        }
    }
    fclose(f);
    if (!got)
        return -1;
    return bytes;
}

/*
 * Map `len` bytes 2 MiB aligned, ask for transparent huge pages, then fault
 * every page in.  The caller must already be pinned so that first-touch keeps
 * the pages on the local NUMA node.
 */
static Buf buf_alloc(size_t len)
{
    Buf b;
    size_t raw;
    void *base;
    uintptr_t aligned;
    size_t head, tail;

    b.addr = NULL;
    b.len = 0;
    b.huge_bytes = -1;
    b.madvise_errno = 0;

    len = (len + PAGE_SZ - 1) & ~(size_t)(PAGE_SZ - 1);
    if (len < sizeof(uint64_t))
        die("buffer too small to measure");

    raw = len + TWO_MIB;
    base = mmap(NULL, raw, PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    if (base == MAP_FAILED)
        die("mmap(%s): %s", human_bytes(len), strerror(errno));

    aligned = ((uintptr_t)base + TWO_MIB - 1) & ~(uintptr_t)(TWO_MIB - 1);
    head = aligned - (uintptr_t)base;
    if (head)
        munmap(base, head);
    tail = raw - head - len;
    if (tail)
        munmap((void *)(aligned + len), tail);

    b.addr = (void *)aligned;
    b.len = len;

    if (madvise(b.addr, len, MADV_HUGEPAGE) != 0)
        b.madvise_errno = errno;

    /* Prefault: one word per page, outside every timed region. */
    {
        volatile unsigned long long *w = (volatile unsigned long long *)b.addr;
        size_t stride = PAGE_SZ / sizeof(unsigned long long);
        size_t words = len / sizeof(unsigned long long);
        for (size_t i = 0; i < words; i += stride)
            w[i] = 0;
        w[words - 1] = 0;
    }

    b.huge_bytes = smaps_anon_huge(b.addr);
    return b;
}

static void buf_free(Buf *b)
{
    if (!b->addr)
        return;
    if (munmap(b->addr, b->len) != 0)
        warn_msg("munmap: %s", strerror(errno));
    b->addr = NULL;
    b->len = 0;
}

static volatile uint64_t sink; /* keeps every kernel and clock result observable */

/* ------------------------------------------------------------------ */
/* effective clock: measured, never read from cpufreq                   */
/* ------------------------------------------------------------------ */

/*
 * A dependent integer add chain retires exactly one add per cycle, so its
 * throughput is the core clock.  The volatile zero keeps gcc from folding the
 * chain into `x + n*inc` (without it the same source measured 10 000 000 MHz,
 * which is how this comment knows).  No cpufreq, no TSC, no guesses: this is
 * the clock the core actually ran at while the trial was in flight, which on a
 * boosting or shared host is not the number in /sys.
 */
static volatile uint64_t mb_clock_zero;

static __attribute__((noinline)) uint64_t add_chain(uint64_t n, uint64_t seed)
{
    uint64_t x = seed | 1;
    const uint64_t k = seed | 3;
    for (uint64_t i = 0; i < n; i++)
        x += k + mb_clock_zero;
    return x;
}

/* Measure the current core's clock over about `target_ms` milliseconds. */
static double measure_mhz(double target_ms)
{
    const uint64_t cal = 1u << 20;
    double t0 = now_s();
    sink = add_chain(cal, 0x1234567);
    double dt = now_s() - t0;
    if (dt <= 0.0)
        return 0.0;
    double per_add = dt / (double)cal;
    uint64_t n = (uint64_t)((target_ms * 1e-3) / (per_add > 1e-12 ? per_add : 1e-12));
    if (n < (1u << 14))
        n = 1u << 14;
    t0 = now_s();
    sink = add_chain(n, 0x1234567);
    dt = now_s() - t0;
    if (dt <= 0.0)
        return 0.0;
    return (double)n / dt / 1e6;
}

/* Plausibility, in the plan's spirit: say so when a number cannot be true. */
static const char *clock_sanity(double mhz)
{
    if (mhz <= 0.0)
        return "clock measurement failed";
    if (mhz < 100.0 || mhz > 12000.0)
        return "implausible clock, treat cycle figures as unreliable";
    return NULL;
}

/* ------------------------------------------------------------------ */
/* kernels                                                             */
/* ------------------------------------------------------------------ */
/*
 * `fast` is an ordinary C loop that gcc -O3 -march=native is expected to
 * vectorise.  `scalar` is a portable loop carrying a loop-carried dependency
 * so the reduction cannot be reassociated away; gcc may still vectorise it,
 * which is stated in the output and in README.md rather than hidden.
 * No inline asm: sharing the Rust project's asm kernels would make agreement
 * meaningless.
 */

typedef uint64_t word_t;

/*
 * Every kernel takes `reps` and must actually perform `reps` passes.  That is
 * worth stating because gcc 14 -O3 proved that the obvious write loop
 * (`for r { for i { w[i] = pat; } }`) stored the same value to the same
 * addresses every time and deleted the repetition loop -- which turned the
 * Write cell into 5 600 000 GB/s.  Each kernel therefore varies what it stores
 * by the repetition index, so the passes are no longer provably redundant, and
 * the read reduction keeps four independent accumulators so that a single
 * loop-carried dependency does not quietly become the bottleneck.
 * Neither trick changes what is measured: one load, or one store, or one load
 * plus one store per 8 bytes.
 */

/*
 * Two independent streams through each buffer (four for the write kernel).
 * This is not a micro-optimisation: a single stream in the same source file
 * measured 68 GB/s for a copy at *every* size, L1 included, which is a property
 * of the loop and not of the memory system.  With two streams the copy reaches
 * 132 GB/s in L1 and 51-57 GB/s beyond it, within a few percent of glibc memcpy
 * at 16 KiB / 64 KiB / 256 KiB / 2 MiB on this machine, which is what makes it
 * usable as a reference.
 *
 * Each kernel must also actually perform `reps` passes.  gcc 14 -O3 proved that
 * `for r { for i { w[i] = pat; } }` stored the same value to the same addresses
 * every time and deleted the repetition loop, which turned the Write cell into
 * 5 600 000 GB/s; the stored value therefore depends on the repetition index.
 * That changes the value written, never the traffic: one load, one store, or
 * one load plus one store per 8 bytes.
 */

static __attribute__((noinline)) word_t
k_read_fast(const unsigned char *p, size_t len, unsigned long long reps)
{
    const word_t *restrict w = (const word_t *)p;
    size_t n = len / sizeof(word_t);
    size_t h = (n & ~(size_t)1) / 2; /* first half vs second half */
    word_t a = 0, b = 0;
    for (unsigned long long r = 0; r < reps; r++) {
        for (size_t i = 0; i < h; i++) {
            a += w[i];
            b += w[h + i];
        }
        a += r; /* keeps the repetition loop live without touching memory */
        if (n & 1)
            a += w[n - 1];
    }
    return a ^ b;
}

/* Portable loop with a loop-carried dependency so the reduction cannot be
 * reassociated away.  gcc is still allowed to vectorise it, which is why the
 * output says "portable loop; gcc may still vectorise" and never "scalar peak". */
static __attribute__((noinline)) word_t
k_read_scalar(const unsigned char *p, size_t len, unsigned long long reps)
{
    const word_t *w = (const word_t *)p;
    size_t n = len / sizeof(word_t);
    word_t acc = 1;
    for (unsigned long long r = 0; r < reps; r++)
        for (size_t i = 0; i < n; i++) {
            acc ^= w[i] + r;
            acc = (acc << 1) | (acc >> 63);
        }
    return acc;
}

static __attribute__((noinline)) word_t
k_write_fast(unsigned char *p, size_t len, unsigned long long reps, word_t pat)
{
    word_t *restrict w = (word_t *)p;
    size_t n = len / sizeof(word_t);
    size_t q = n / 4; /* four store streams */
    for (unsigned long long r = 0; r < reps; r++) {
        word_t v = pat + r; /* not idempotent: gcc may not drop a pass */
        for (size_t i = 0; i < q; i++) {
            w[i] = v;
            w[q + i] = v + 1;
            w[2 * q + i] = v + 2;
            w[3 * q + i] = v + 3;
        }
        for (size_t i = 4 * q; i < n; i++)
            w[i] = v;
    }
    return pat;
}

/*
 * Volatile stores cannot be vectorised or merged, so this loop issues one
 * scalar store per iteration: its number is a floor, and the label says
 * "scalar", never "peak".
 */
static __attribute__((noinline)) word_t
k_write_scalar(unsigned char *p, size_t len, unsigned long long reps, word_t pat)
{
    volatile word_t *w = (volatile word_t *)p;
    size_t n = len / sizeof(word_t);
    for (unsigned long long r = 0; r < reps; r++) {
        word_t v = pat + r;
        for (size_t i = 0; i < n; i++)
            w[i] = v;
    }
    return pat;
}

static __attribute__((noinline)) word_t
k_copy_fast(const unsigned char *src, unsigned char *dst, size_t len,
            unsigned long long reps)
{
    const word_t *restrict s = (const word_t *)src;
    word_t *restrict d = (word_t *)dst;
    size_t n = len / sizeof(word_t);
    size_t h = (n & ~(size_t)1) / 2; /* two copy streams */
    for (unsigned long long r = 0; r < reps; r++) {
        word_t x = (word_t)r;
        for (size_t i = 0; i < h; i++) {
            d[i] = s[i] + x;
            d[h + i] = s[h + i] ^ x;
        }
        if (n & 1)
            d[n - 1] = s[n - 1];
    }
    return n ? s[n - 1] : 0;
}

static __attribute__((noinline)) word_t
k_copy_scalar(const unsigned char *src, unsigned char *dst, size_t len,
              unsigned long long reps)
{
    const volatile word_t *s = (const volatile word_t *)src;
    volatile word_t *d = (volatile word_t *)dst;
    size_t n = len / sizeof(word_t);
    for (unsigned long long r = 0; r < reps; r++) {
        word_t x = (word_t)r;
        for (size_t i = 0; i < n; i++)
            d[i] = s[i] + x;
    }
    return 0;
}

/* ------------------------------------------------------------------ */
/* options                                                             */
/* ------------------------------------------------------------------ */

typedef enum { L1D, L2, L3, MEM } Level;

static const char *level_json(Level l)
{
    switch (l) {
    case L1D: return "l1";   /* membench's JSON vocabulary */
    case L2:  return "l2";
    case L3:  return "l3";
    default:  return "memory";
    }
}

static const char *level_label(Level l)
{
    switch (l) {
    case L1D: return "L1d";
    case L2:  return "L2";
    case L3:  return "L3";
    default:  return "Memory";
    }
}

typedef enum { OP_READ, OP_WRITE, OP_COPY } Op;

static const char *op_name(Op o)
{
    switch (o) {
    case OP_READ: return "read";
    case OP_WRITE: return "write";
    default: return "copy";
    }
}

typedef struct {
    Level level;
    int threads;   /* 0 = as many as this level can use */
    int trials;
    int duration_ms;
    int scalar;
    int copy_counts_both;
    char json[4096];
    int want_json;
} Opt;

static void usage(FILE *f)
{
    fputs(
        "usage: mbstream [--level l1d|l2|l3|mem] [--threads N] [--trials N]\n"
        "                [--duration MS] [--isa default|scalar] [--json PATH]\n"
        "                [--copy-counts-both] [--help]\n"
        "\n"
        "Read / Write / Copy bandwidth in GB/s (SI, 1 GB/s = 1e9 B/s): median of\n"
        "--trials windows of about --duration ms, after one untimed warm-up pass.\n"
        "One thread per pinned CPU (pthread_setaffinity_np), buffers mmap'ed and\n"
        "prefaulted after pinning, 2 MiB pages requested and verified in smaps.\n"
        "Copy counts source bytes only unless --copy-counts-both is given.\n"
        "--isa scalar selects a portable loop; gcc may still vectorise it.\n",
        f);
}

static int parse_level(const char *s, Level *out)
{
    if (!strcmp(s, "l1d") || !strcmp(s, "l1")) { *out = L1D; return 0; }
    if (!strcmp(s, "l2"))                      { *out = L2;  return 0; }
    if (!strcmp(s, "l3"))                      { *out = L3;  return 0; }
    if (!strcmp(s, "mem") || !strcmp(s, "memory")) { *out = MEM; return 0; }
    return -1;
}

/* ------------------------------------------------------------------ */
/* one op for one level                                                */
/* ------------------------------------------------------------------ */

typedef struct {
    int index;
    int cpu;
    size_t bytes;
    Op op;
    int scalar;
    int copy_counts_both;
    int trials;
    int duration_ms;
    /* bar_warm has the parent as a participant (it decides `reps`); bar_trial
     * is worker-only.  They must be separate barriers: one barrier shared by
     * N+1 arrivals in the handshake and N arrivals per trial window would
     * release the wrong generation and deadlock the run. */
    pthread_barrier_t *bar_warm;
    pthread_barrier_t *bar_trial;
    /* written by the worker */
    Buf src, dst;
    unsigned long long reps;
    long long warm_ns;
    double t_start[MAX_TRIALS];
    double t_end[MAX_TRIALS];
    unsigned long long t_bytes[MAX_TRIALS];
    double own_gbps[MAX_TRIALS];   /* this thread's own rate, window and all */
    double clock_mhz;
    int ntrial;
    int pin_failed;
    /* Snapshot of the mappings, taken before they are unmapped: the parent
     * reads these after join(), and reading src.len afterwards would report
     * "0 of 0 bytes" for a mapping that no longer exists. */
    size_t src_len, dst_len;
    long long src_huge, dst_huge;
    int madvise_errno;
} Thread;

static void *worker(void *arg)
{
    Thread *t = (Thread *)arg;
    cpu_set_t set;
    word_t pat = (word_t)0x0123456789ABCDEFull;
    size_t len;
    void *dstp;
    unsigned long long reps;

    CPU_ZERO(&set);
    CPU_SET(t->cpu, &set);

    /* Pin BEFORE allocating so first-touch keeps pages on the local node. */
    if (pthread_setaffinity_np(pthread_self(), sizeof(set), &set) != 0) {
        warn_msg("cpu %d: pthread_setaffinity_np: %s", t->cpu, strerror(errno));
        t->pin_failed = 1;
    }

    t->src = buf_alloc(t->bytes);
    t->dst.addr = NULL;
    t->dst.len = 0;
    if (t->op == OP_COPY)
        t->dst = buf_alloc(t->bytes);
    /* Read and write both own exactly one mapping; copy owns two.  For write
     * the destination *is* that mapping, so `dstp` names whichever one the
     * stores go to and no aliasing between the two Buf records is possible. */
    dstp = (t->op == OP_COPY) ? t->dst.addr : t->src.addr;
    len = t->src.len;

    /* One untimed pass to give every line real contents. */
    {
        volatile word_t *w = (volatile word_t *)t->src.addr;
        size_t words = t->src.len / sizeof(word_t);
        for (size_t i = 0; i < words; i++)
            w[i] = (word_t)(0x9E3779B97F4A7C15ull ^ (i * 2654435761u) ^ (word_t)t->cpu);
        if (t->op == OP_COPY) {
            volatile word_t *d = (volatile word_t *)t->dst.addr;
            for (size_t i = 0; i < words; i++)
                d[i] = 0;
        }
    }

    /* Warm-up: not a trial.  Timed only to size the real windows. */
    {
        double w0 = now_s();
        switch (t->op) {
        case OP_READ:  sink = k_read_fast(t->src.addr, len, 1); break;
        case OP_WRITE: sink = k_write_fast(dstp, len, 1, pat); break;
        default:       sink = k_copy_fast(t->src.addr, t->dst.addr, len, 1); break;
        }
        t->warm_ns = (long long)((now_s() - w0) * 1e9);
    }
    if (t->warm_ns < 1)
        t->warm_ns = 1;

    pthread_barrier_wait(t->bar_warm);   /* parent picks reps from the slowest */
    pthread_barrier_wait(t->bar_warm);   /* ... and publishes it here */
    reps = t->reps ? t->reps : 1;

    for (int trial = 0; trial < t->trials && trial < MAX_TRIALS; trial++) {
        word_t r = 0;
        double s, e;
        unsigned long long mult = 1;

        pthread_barrier_wait(t->bar_trial); /* start the window together */

        /* Re-check affinity each window: a migration in mid-window is exactly
         * the silent error this project exists to avoid. */
        if (!t->pin_failed) {
            cpu_set_t have;
            CPU_ZERO(&have);
            if (pthread_getaffinity_np(pthread_self(), sizeof(have), &have) == 0 &&
                (!CPU_ISSET(t->cpu, &have) || CPU_COUNT(&have) != 1)) {
                warn_msg("cpu %d: affinity lost before trial %d, pinning again",
                         t->cpu, trial);
                CPU_ZERO(&set);
                CPU_SET(t->cpu, &set);
                if (pthread_setaffinity_np(pthread_self(), sizeof(set), &set) != 0)
                    t->pin_failed = 1;
            }
        }

        s = now_s();
        switch (t->op) {
        case OP_READ:
            r = t->scalar ? k_read_scalar(t->src.addr, len, reps)
                          : k_read_fast(t->src.addr, len, reps);
            break;
        case OP_WRITE:
            r = t->scalar ? k_write_scalar(dstp, len, reps, pat)
                          : k_write_fast(dstp, len, reps, pat);
            break;
        default:
            r = t->scalar ? k_copy_scalar(t->src.addr, t->dst.addr, len, reps)
                          : k_copy_fast(t->src.addr, t->dst.addr, len, reps);
            if (t->copy_counts_both)
                mult = 2; /* STREAM-style accounting */
            break;
        }
        e = now_s();
        sink = r;

        t->t_start[trial] = s;
        t->t_end[trial] = e;
        t->t_bytes[trial] = (unsigned long long)len * reps * mult;
        {
            double own = e - s;
            t->own_gbps[trial] = own > 1e-9
                ? (double)t->t_bytes[trial] / own / GBPS : 0.0;
        }
        t->ntrial = trial + 1;

        pthread_barrier_wait(t->bar_trial); /* nobody races ahead of the straggler */
    }

    /* Same core, same thermal moment as the trials (membench measures here for
     * exactly the same reason): the number below is what the core ran at. */
    t->clock_mhz = measure_mhz(20.0);

    t->src_len = t->src.len;
    t->dst_len = (t->op == OP_COPY) ? t->dst.len : 0;
    t->src_huge = t->src.huge_bytes;
    t->dst_huge = (t->op == OP_COPY) ? t->dst.huge_bytes : 0;
    t->madvise_errno = t->src.madvise_errno;
    buf_free(&t->src);
    if (t->op == OP_COPY)
        buf_free(&t->dst);
    return NULL;
}

typedef struct {
    Op op;
    double median;
    double best;
    double spread_pct;
    double trials[MAX_TRIALS];
    int ntrial;
    unsigned long long bytes_moved;
    unsigned long long duration_us;
    long long huge_bytes; /* -1 = unknown */
    unsigned long long mapped_bytes;
    int madvise_errno;
    double clock_min, clock_max;
    double thread_min, thread_max; /* per-thread medians: the straggler show */
    int cpus[MAX_THREADS];
} Cell;

static void run_op(const Opt *o, const int *cpus, int nthreads, size_t bytes_per_thread,
                   Op op, Cell *cell)
{
    pthread_barrier_t bar_warm, bar_trial;
    Thread *th;
    pthread_t *tid;
    int trials;
    int i;

    memset(cell, 0, sizeof(*cell));
    cell->op = op;
    cell->huge_bytes = 0;

    th = calloc((size_t)nthreads, sizeof(Thread));
    tid = calloc((size_t)nthreads, sizeof(pthread_t));
    if (!th || !tid)
        die("out of memory for %d worker descriptors", nthreads);

    /* +1 for the parent in the handshake barrier only. */
    if (pthread_barrier_init(&bar_warm, NULL, (unsigned)nthreads + 1) != 0)
        die("pthread_barrier_init: %s", strerror(errno));
    if (pthread_barrier_init(&bar_trial, NULL, (unsigned)nthreads) != 0)
        die("pthread_barrier_init: %s", strerror(errno));

    for (i = 0; i < nthreads; i++) {
        th[i].index = i;
        th[i].cpu = cpus[i];
        th[i].bytes = bytes_per_thread;
        th[i].op = op;
        th[i].scalar = o->scalar;
        th[i].copy_counts_both = o->copy_counts_both;
        th[i].trials = o->trials;
        th[i].duration_ms = o->duration_ms;
        th[i].bar_warm = &bar_warm;
        th[i].bar_trial = &bar_trial;
        if (pthread_create(&tid[i], NULL, worker, &th[i]) != 0)
            die("pthread_create: %s", strerror(errno));
    }

    pthread_barrier_wait(&bar_warm); /* every thread has a warm-up number */
    {
        long long slowest = 1;
        unsigned long long reps;
        for (i = 0; i < nthreads; i++)
            if (th[i].warm_ns > slowest)
                slowest = th[i].warm_ns;
        /* The same repetition count for every thread: a thread that finishes
         * early would idle and the window would stop meaning anything. */
        reps = (unsigned long long)((double)o->duration_ms * 1e6 / (double)slowest);
        if (reps < 1)
            reps = 1;
        for (i = 0; i < nthreads; i++)
            th[i].reps = reps;
    }
    pthread_barrier_wait(&bar_warm);

    for (i = 0; i < nthreads; i++)
        pthread_join(tid[i], NULL);
    pthread_barrier_destroy(&bar_warm);
    pthread_barrier_destroy(&bar_trial);

    /* Per-thread state is now read by the parent only; free the mappings. */
    trials = o->trials;
    for (i = 0; i < nthreads; i++) {
        if (th[i].ntrial < trials)
            trials = th[i].ntrial;
        if (th[i].src_len) {
            cell->mapped_bytes += (unsigned long long)th[i].src_len;
            if (th[i].src_huge < 0)
                cell->huge_bytes = -1;
            else if (cell->huge_bytes >= 0)
                cell->huge_bytes += th[i].src_huge;
            if (th[i].madvise_errno && !cell->madvise_errno)
                cell->madvise_errno = th[i].madvise_errno;
        }
        if (th[i].dst_len) {
            cell->mapped_bytes += (unsigned long long)th[i].dst_len;
            if (th[i].dst_huge < 0)
                cell->huge_bytes = -1;
            else if (cell->huge_bytes >= 0)
                cell->huge_bytes += th[i].dst_huge;
        }
    }
    if (trials > MAX_TRIALS)
        trials = MAX_TRIALS;
    if (trials < 1)
        die("no worker completed a trial");

    /* Aggregate each trial as earliest start -> latest end over all threads. */
    {
        double vals[MAX_TRIALS];
        double best = -1.0;
        int nv = 0;
        for (int tr = 0; tr < trials; tr++) {
            double s = th[0].t_start[tr], e = th[0].t_end[tr];
            unsigned long long bytes = 0;
            double secs;
            for (i = 0; i < nthreads; i++) {
                if (th[i].t_start[tr] < s)
                    s = th[i].t_start[tr];
                if (th[i].t_end[tr] > e)
                    e = th[i].t_end[tr];
                bytes += th[i].t_bytes[tr];
            }
            secs = e - s;
            if (secs < 1e-9)
                secs = 1e-9;
            vals[nv] = (double)bytes / secs / GBPS;
            if (vals[nv] > best) {
                best = vals[nv];
                cell->bytes_moved = bytes;
                cell->duration_us = (unsigned long long)(secs * 1e6);
            }
            nv++;
        }
        cell->ntrial = nv;
        memcpy(cell->trials, vals, sizeof(double) * (size_t)nv);
        cell->best = best;
        if (nv) {
            double sorted[MAX_TRIALS];
            double lo, hi;
            memcpy(sorted, vals, sizeof(double) * (size_t)nv);
            cell->median = median_of(sorted, (size_t)nv);
            lo = hi = sorted[0];
            for (int k = 1; k < nv; k++) {
                if (sorted[k] < lo)
                    lo = sorted[k];
                if (sorted[k] > hi)
                    hi = sorted[k];
            }
            cell->spread_pct = cell->median > 0.0 ? (hi - lo) / cell->median * 100.0 : 0.0;
        }
    }

    /* Per-thread medians and the measured clock: an aggregate window is only
     * as fast as its slowest thread, so without these two lines a single busy
     * core would look like a slow cache. */
    {
        double best = -1.0, worst = 1e30;
        double cmin = 1e30, cmax = 0.0;
        for (i = 0; i < nthreads; i++) {
            double own[MAX_TRIALS];
            int nown = th[i].ntrial;
            double m;
            if (nown > MAX_TRIALS)
                nown = MAX_TRIALS;
            if (nown < 1)
                continue;
            memcpy(own, th[i].own_gbps, sizeof(double) * (size_t)nown);
            m = median_of(own, (size_t)nown);
            if (m < worst)
                worst = m;
            if (m > best)
                best = m;
            if (th[i].clock_mhz > 0.0) {
                if (th[i].clock_mhz < cmin)
                    cmin = th[i].clock_mhz;
                if (th[i].clock_mhz > cmax)
                    cmax = th[i].clock_mhz;
            }
        }
        cell->thread_min = worst > 1e29 ? 0.0 : worst;
        cell->thread_max = best < 1e29 ? 0.0 : best;
        cell->clock_min = cmin < 1e29 ? cmin : 0.0;
        cell->clock_max = cmax;
        for (i = 0; i < nthreads && i < MAX_THREADS; i++)
            cell->cpus[i] = cpus[i];
    }

    free(th);
    free(tid);
}

/* ------------------------------------------------------------------ */
/* buffer sizing (the plan's fractions, derived independently)         */
/* ------------------------------------------------------------------ */

typedef struct {
    size_t bytes_per_thread;
    int threads;
    int cpus[MAX_THREADS];
    char derivation[512];
    char note[512];
} Plan;

static void plan_for(const Opt *o, const Machine *m, Plan *p)
{
    const int *pool;
    int npool, threads;
    size_t bytes;

    memset(p, 0, sizeof(*p));

    if (o->level == MEM) {
        /* Memory bandwidth scales with channels, not cores: use every logical
         * CPU, which is what STREAM does. */
        pool = m->all_cpus;
        npool = m->nall;
    } else {
        /* Two SMT siblings inside one L1d both measure something else, so the
         * cache levels get one thread per physical core. */
        pool = m->core_cpus;
        npool = m->ncore;
    }
    if (npool <= 0)
        die("no CPUs found under /sys/devices/system/cpu");

    threads = o->threads > 0 ? o->threads : npool;
    if (threads > npool) {
        warn_msg("%d threads requested but this level can use %d CPU(s); using %d",
                 o->threads, npool, npool);
        threads = npool;
    }
    if (threads < 1)
        threads = 1;

    bytes = MIN_BW_BYTES;

    switch (o->level) {
    case L1D: {
        if (m->l1d <= 0)
            die("L1d size is unknown, so the buffer cannot be sized honestly");
        bytes = (size_t)(m->l1d / 2);
        snprintf(p->derivation, sizeof(p->derivation),
                 "0.5 x %s L1d = %s per thread, %d thread(s) on %d physical core(s)",
                 human_bytes((unsigned long long)m->l1d), human_bytes(bytes),
                 threads, m->ncore);
        if ((long long)bytes > m->l1d)
            snprintf(p->note, sizeof(p->note),
                     "buffer exceeds L1d, so this cell is L2-bound");
        break;
    }
    case L2: {
        if (m->l2 <= 0)
            die("L2 size is unknown, so the buffer cannot be sized honestly");
        bytes = (size_t)(m->l2 / 2);
        snprintf(p->derivation, sizeof(p->derivation),
                 "0.5 x %s L2 = %s per thread, %d thread(s)",
                 human_bytes((unsigned long long)m->l2), human_bytes(bytes), threads);
        if (m->l1d > 0 && (long long)bytes > m->l1d)
            snprintf(p->note, sizeof(p->note),
                     "buffer is %.1fx L1d, so it is L2-resident as intended",
                     (double)bytes / (double)m->l1d);
        break;
    }
    case L3: {
        long long l2 = m->l2 > 0 ? m->l2 : m->l3 / 8;
        size_t lower;
        if (m->l3 <= 0)
            die("L3 size is unknown, so the buffer cannot be sized honestly");
        lower = (size_t)(2 * l2);
        /* A share below 2 x L2 never leaves L2, so drop threads rather than
         * inflate the buffer past the cache it is supposed to measure. */
        while (threads > 1 &&
               (size_t)((double)m->l3 * 0.75 / (double)threads) < lower)
            threads--;
        bytes = (size_t)((double)m->l3 * 0.75 / (double)threads);
        if (bytes < lower)
            bytes = lower;
        snprintf(p->derivation, sizeof(p->derivation),
                 "0.75 x %s L3 / %d core(s) = %s per thread (floor 2 x %s L2 = %s)",
                 human_bytes((unsigned long long)m->l3), threads, human_bytes(bytes),
                 human_bytes((unsigned long long)l2), human_bytes(lower));
        if ((double)bytes * (double)threads > (double)m->l3)
            snprintf(p->note, sizeof(p->note),
                     "the %d buffers together exceed the %s L3, so part of this cell is "
                     "memory traffic", threads, human_bytes((unsigned long long)m->l3));
        else if (bytes < lower)
            snprintf(p->note, sizeof(p->note),
                     "buffer is under 2 x L2, so some lines may still be caught by L2");
        break;
    }
    default: {
        unsigned long long target = ONE_GIB;
        unsigned long long avail = mem_available_bytes();
        int clamped = 0;
        if (m->l3 > 0 && (unsigned long long)(16 * (m->l3 + 1)) > target)
            target = (unsigned long long)(16 * (m->l3 + 1));
        /* Never ask for more than half of what is free: a run that starts
         * swapping measures the swap device, not DRAM. */
        if (avail && target > avail / 2) {
            target = avail / 2;
            clamped = 1;
        }
        bytes = (size_t)(target / (unsigned long long)threads);
        snprintf(p->derivation, sizeof(p->derivation),
                 "max(1 GiB, 16 x total L3) = %s / %d logical CPU(s) = %s per thread%s",
                 human_bytes(target), threads, human_bytes(bytes),
                 clamped ? " [clamped to half the RAM available]" : "");
        if (m->l3 > 0 && target < 4ull * (unsigned long long)m->l3)
            snprintf(p->note, sizeof(p->note),
                     "working set %s is under 4 x L3, so cache residency may inflate this "
                     "number", human_bytes(target));
        break;
    }
    }

    if (bytes < MIN_BW_BYTES)
        bytes = MIN_BW_BYTES;
    bytes &= ~(size_t)(PAGE_SZ - 1); /* round down to the page size */
    if (bytes < MIN_BW_BYTES)
        bytes = MIN_BW_BYTES;

    p->bytes_per_thread = bytes;
    p->threads = threads;
    for (int i = 0; i < threads; i++)
        p->cpus[i] = pool[i];
}

/* ------------------------------------------------------------------ */
/* JSON                                                                */
/* ------------------------------------------------------------------ */

static void json_puts(FILE *f, const char *s)
{
    for (const char *p = s; *p; p++) {
        switch (*p) {
        case '"':  fputs("\\\"", f); break;
        case '\\': fputs("\\\\", f); break;
        case '\n': fputs("\\n", f); break;
        case '\t': fputs("\\t", f); break;
        case '\r': fputs("\\r", f); break;
        default:
            if ((unsigned char)*p < 0x20)
                fprintf(f, "\\u%04x", (unsigned char)*p);
            else
                fputc(*p, f);
        }
    }
}

static void write_json(const char *path, const Opt *o, const Machine *m, const Plan *plan,
                       const Cell *cells, const char *thp)
{
    FILE *f = fopen(path, "wb");
    if (!f)
        die("--json %s: %s", path, strerror(errno));

    fputs("{\n", f);
    fputs("  \"tool\": \"mbstream\",\n", f);
    fputs("  \"tool_version\": \"1\",\n", f);
    fputs("  \"schema_version\": 1,\n", f);
    fputs("  \"units\": \"GB/s (SI, 1e9 bytes per second)\",\n", f);
    fprintf(f, "  \"optimised_build\": %s,\n", optimised_build ? "true" : "false");
    fprintf(f, "  \"isa\": \"%s\",\n", o->scalar ? "scalar" : "default");
    fprintf(f, "  \"isa_note\": \"%s\",\n",
            o->scalar ? "portable loop selected; gcc may still vectorise it"
                      : "plain C loops, gcc -march=native is expected to vectorise them");
    fprintf(f, "  \"level\": \"");
    json_puts(f, level_json(o->level));
    fputs("\",\n", f);
    fprintf(f, "  \"level_label\": \"%s\",\n", level_label(o->level));
    fprintf(f, "  \"cpu_model\": \"");
    json_puts(f, m->model[0] ? m->model : "unknown");
    fputs("\",\n", f);
    fprintf(f, "  \"cache_bytes\": {\"l1d\": %lld, \"l2\": %lld, \"l3\": %lld, \"line\": %lld},\n",
            m->l1d, m->l2, m->l3, m->line);
    fprintf(f, "  \"thp_setting\": \"%s\",\n", thp);
    fprintf(f, "  \"sizing\": \"");
    json_puts(f, plan->derivation);
    fputs("\",\n", f);
    if (plan->note[0])
        fprintf(f, "  \"sizing_note\": \"%s\",\n", plan->note);
    else
        fputs("  \"sizing_note\": null,\n", f);
    fprintf(f, "  \"buffer_bytes_per_thread\": %zu,\n", plan->bytes_per_thread);
    fprintf(f, "  \"threads\": %d,\n", plan->threads);
    fputs("  \"cpus\": [", f);
    for (int i = 0; i < plan->threads; i++)
        fprintf(f, "%s%d", i ? ", " : "", plan->cpus[i]);
    fputs("],\n", f);
    fprintf(f, "  \"trials\": %d,\n", o->trials);
    fprintf(f, "  \"duration_ms\": %d,\n", o->duration_ms);
    fprintf(f, "  \"copy_counts_both\": %s,\n", o->copy_counts_both ? "true" : "false");
    fputs("  \"cells\": [\n", f);
    for (int i = 0; i < 3; i++) {
        const Cell *c = &cells[i];
        fputs("    {\n", f);
        fprintf(f, "      \"level\": \"%s\",\n", level_json(o->level));
        fprintf(f, "      \"op\": \"%s\",\n", op_name(c->op));
        fprintf(f, "      \"unit\": \"GB/s\",\n");
        fprintf(f, "      \"median\": %.6f,\n", c->median);
        fprintf(f, "      \"best\": %.6f,\n", c->best);
        fprintf(f, "      \"spread_pct\": %.3f,\n", c->spread_pct);
        fputs("      \"trials\": [", f);
        for (int t = 0; t < c->ntrial; t++)
            fprintf(f, "%s%.6f", t ? ", " : "", c->trials[t]);
        fputs("],\n", f);
        fprintf(f, "      \"threads\": %d,\n", plan->threads);
        fprintf(f, "      \"buffer_bytes_per_thread\": %zu,\n", plan->bytes_per_thread);
        fprintf(f, "      \"buffers_per_thread\": %d,\n", c->op == OP_COPY ? 2 : 1);
        fprintf(f, "      \"bytes_moved\": %llu,\n", c->bytes_moved);
        fprintf(f, "      \"duration_us\": %llu,\n", c->duration_us);
        fprintf(f, "      \"pages_requested\": \"2m\",\n");
        fprintf(f, "      \"pages_actual\": \"%s\",\n", c->huge_bytes > 0 ? "2m" : "4k");
        if (c->huge_bytes < 0)
            fputs("      \"huge_pages_bytes\": null,\n", f);
        else
            fprintf(f, "      \"huge_pages_bytes\": %lld,\n", c->huge_bytes);
        fprintf(f, "      \"huge_mapped_bytes\": %llu,\n", c->mapped_bytes);
        fprintf(f, "      \"madvise_hugepage_errno\": %d\n", c->madvise_errno);
        fputs(i == 2 ? "    }\n" : "    },\n", f);
    }
    fputs("  ]\n}\n", f);

    if (fclose(f) != 0)
        die("fclose(%s): %s", path, strerror(errno));
}

/* ------------------------------------------------------------------ */
/* main                                                                */
/* ------------------------------------------------------------------ */

int main(int argc, char **argv)
{
    Opt o;
    Machine m;
    Plan plan;
    Cell cells[3];
    Op ops[3] = {OP_READ, OP_WRITE, OP_COPY};
    char thp[32];

    memset(&o, 0, sizeof(o));
    o.level = L2;
    o.trials = 7;   /* membench's default */
    o.duration_ms = 200;
    o.scalar = MB_FORCE_SCALAR ? 1 : 0;

    for (int i = 1; i < argc; i++) {
        const char *a = argv[i];
        const char *v = (i + 1 < argc) ? argv[i + 1] : NULL;
        if (!strcmp(a, "--help") || !strcmp(a, "-h")) {
            usage(stdout);
            return 0;
        } else if (!strcmp(a, "--level")) {
            if (!v || parse_level(v, &o.level)) {
                fprintf(stderr, "mbstream: --level must be l1d, l2, l3 or mem\n");
                usage(stderr);
                return 2;
            }
            i++;
        } else if (!strcmp(a, "--threads")) {
            if (!v) { usage(stderr); return 2; }
            o.threads = atoi(v);
            if (o.threads < 1 || o.threads > MAX_THREADS)
                die("--threads must be 1..%d", MAX_THREADS);
            i++;
        } else if (!strcmp(a, "--trials")) {
            if (!v) { usage(stderr); return 2; }
            o.trials = atoi(v);
            if (o.trials < 1 || o.trials > MAX_TRIALS)
                die("--trials must be 1..%d", MAX_TRIALS);
            i++;
        } else if (!strcmp(a, "--duration")) {
            if (!v) { usage(stderr); return 2; }
            o.duration_ms = atoi(v);
            if (o.duration_ms < 5)
                die("--duration must be at least 5 ms");
            i++;
        } else if (!strcmp(a, "--isa")) {
            if (!v) { usage(stderr); return 2; }
            if (!strcmp(v, "scalar")) {
                o.scalar = 1;
            } else if (!strcmp(v, "default")) {
                o.scalar = MB_FORCE_SCALAR ? 1 : 0;
            } else {
                fprintf(stderr, "mbstream: --isa must be default or scalar\n");
                return 2;
            }
            i++;
        } else if (!strcmp(a, "--json")) {
            if (!v) { usage(stderr); return 2; }
            snprintf(o.json, sizeof(o.json), "%s", v);
            o.want_json = 1;
            i++;
        } else if (!strcmp(a, "--copy-counts-both")) {
            o.copy_counts_both = 1;
        } else {
            fprintf(stderr, "mbstream: unknown argument '%s'\n", a);
            usage(stderr);
            return 2;
        }
    }

    memset(&m, 0, sizeof(m));
    detect_topology(&m);
    detect_caches(&m);

    if (!optimised_build)
        fprintf(stderr,
                "mbstream: WARNING: compiled without optimisation (__OPTIMIZE__ is not set).\n"
                "mbstream: WARNING: such a build measures the compiler, not the memory system.\n"
                "mbstream: WARNING: rebuild with -O2/-O3 (the Makefile default) before quoting\n"
                "mbstream: WARNING: anything below; every line is labelled UNOPTIMISED.\n");

    plan_for(&o, &m, &plan);
    thp_setting(thp, sizeof(thp));

    printf("mbstream -- independent read/write/copy bandwidth reference\n");
    printf("build          : %s isa=%s\n",
           optimised_build ? "optimised (-O)" : "UNOPTIMISED (-O0): NUMBERS ARE MEANINGLESS",
           o.scalar ? "scalar (portable loop; gcc may still vectorise)" : "default");
    printf("cpu            : %s\n", m.model[0] ? m.model : "unknown");
    printf("caches         : L1d %s, L2 %s, L3 %s, line %lld B\n",
           m.l1d ? human_bytes((unsigned long long)m.l1d) : "unknown",
           m.l2 ? human_bytes((unsigned long long)m.l2) : "unknown",
           m.l3 ? human_bytes((unsigned long long)m.l3) : "unknown", m.line);
    printf("sizing         : %s\n", plan.derivation);
    if (plan.note[0])
        printf("sizing note    : %s\n", plan.note);
    printf("buffer         : %s per thread x %d thread(s) = %s total%s\n",
           human_bytes((unsigned long long)plan.bytes_per_thread), plan.threads,
           human_bytes((unsigned long long)plan.bytes_per_thread * (unsigned long long)plan.threads),
           plan.threads > 1 ? " (copy uses a second buffer per thread)" : "");
    printf("threads        : %d, one per CPU, pinned with pthread_setaffinity_np before mmap\n",
           plan.threads);
    printf("cpus           : ");
    for (int i = 0; i < plan.threads; i++)
        printf("%s%d", i ? "," : "", plan.cpus[i]);
    printf("\n");
    printf("thp setting    : %s\n", thp[0] ? thp : "unknown");
    printf("windows        : %d timed trials of about %d ms after 1 untimed warm-up\n",
           o.trials, o.duration_ms);
    printf("copy accounting: %s\n",
           o.copy_counts_both ? "source + destination bytes (STREAM style)"
                              : "source bytes only (AIDA convention)");

    for (int i = 0; i < 3; i++) {
        memset(&cells[i], 0, sizeof(cells[i]));
        run_op(&o, plan.cpus, plan.threads, plan.bytes_per_thread, ops[i], &cells[i]);
    }

    printf("\n%-6s %12s %12s %8s %8s %14s\n",
           "op", "median GB/s", "best GB/s", "spread", "trials", "clock MHz");
    for (int i = 0; i < 3; i++) {
        const Cell *c = &cells[i];
        printf("%-6s %12.3f %12.3f %7.1f%% %8d %8.0f-%-5.0f\n",
               op_name(c->op), c->median, c->best, c->spread_pct, c->ntrial,
               c->clock_min, c->clock_max);
        if (c->clock_max > 0.0) {
            const char *bad = clock_sanity(c->clock_min);
            printf("       effective clock during the cell: %.0f-%.0f MHz, measured with a\n"
                   "       dependent add chain on the worker cores, not read from cpufreq\n",
                   c->clock_min, c->clock_max);
            if (bad)
                printf("       WARNING: %s (%.0f MHz)\n", bad, c->clock_min);
        }
        if (c->thread_max > 0.0 && c->thread_min < c->thread_max * 0.9)
            printf("       threads were not equal: fastest %.1f GB/s, slowest %.1f GB/s -- the\n"
                   "       cell is aggregated earliest-start to latest-end, so one busy core\n"
                   "       holds back the whole window (cpus are listed above)\n",
                   c->thread_max, c->thread_min);
    }

    printf("\nper-trial GB/s\n");
    for (int i = 0; i < 3; i++) {
        const Cell *c = &cells[i];
        printf("  %-5s", op_name(c->op));
        for (int t = 0; t < c->ntrial; t++)
            printf(" %9.3f", c->trials[t]);
        printf("\n");
    }

    /* Huge pages: verified per mapping from /proc/self/smaps, never assumed. */
    for (int i = 0; i < 3; i++) {
        const Cell *c = &cells[i];
        if (c->huge_bytes < 0)
            printf("huge pages (%s): unknown -- /proc/self/smaps had no AnonHugePages for "
                   "the mapping\n", op_name(c->op));
        else
            printf("huge pages (%s): %lld of %llu bytes granted%s\n",
                   op_name(c->op), c->huge_bytes, c->mapped_bytes,
                   c->huge_bytes == 0
                       ? " (MADV_HUGEPAGE was requested and the kernel did not grant it"
                         ": 4 KiB pages are what this cell used)"
                       : "");
        if (c->madvise_errno)
            printf("                 madvise(MADV_HUGEPAGE) failed: %s\n",
                   strerror(c->madvise_errno));
    }

    if (o.want_json) {
        write_json(o.json, &o, &m, &plan, cells, thp);
        printf("\nwrote %s\n", o.json);
    }
    return 0;
}
