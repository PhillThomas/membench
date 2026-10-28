/*
 * mblatency.c -- dependent pointer-chase latency reference for membench.
 *
 * Second half of the cross-check: membench's `latency.rs` measures how long a
 * load takes when the next load's address comes out of it, so the hardware has
 * nothing to overlap.  This is an independent implementation of the same idea
 * in plain C (no inline asm, nothing shared with the Rust kernels).
 *
 * Two details decide whether the number is real, and both are the reason this
 * file is not twenty lines long:
 *
 *   - ONE POINTER PER CACHE LINE.  If a line held eight pointers the chase
 *     would walk inside one line eight times and report L1 whatever the buffer
 *     size.
 *   - A SINGLE CLOSED CYCLE.  A shuffle that yields several short cycles lets
 *     the whole cycle live in L1 after a few thousand loads.  Sattolo's
 *     algorithm always produces exactly one cycle, so the chain is as long as
 *     the buffer.
 *
 * Cycles per load come from a core clock measured in this same process on this
 * same core with a dependent integer add chain -- never from cpufreq, which
 * reports what the governor would like rather than what the core did.
 *
 * Conventions mirrored from the membench plan: pin before allocating, prefault
 * every page before timing, request 2 MiB pages and then verify what the kernel
 * actually granted through /proc/self/smaps, median of N trials after one
 * untimed warm-up, and never print a number this program cannot justify.
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

#define PAGE_SZ 4096u
#define TWO_MIB (2u * 1024u * 1024u)
#define ONE_GIB (1024ull * 1024ull * 1024ull)
#define MAX_SIZES 64
#define MAX_TRIALS 64
#define DEFAULT_LINE 64

/* Latency windows are capped: a 1 GiB chain pass is already slow, and the
 * point is nanoseconds per load, not wall-clock endurance. */
#define TRIAL_MS_CAP 100

#ifdef __OPTIMIZE__
static const int optimised_build = 1;
#else
static const int optimised_build = 0;
#endif

/* ------------------------------------------------------------------ */
/* utilities                                                           */
/* ------------------------------------------------------------------ */

static void die(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    fprintf(stderr, "mblatency: ");
    vfprintf(stderr, fmt, ap);
    fputc('\n', stderr);
    va_end(ap);
    exit(2);
}

static void warn_msg(const char *fmt, ...)
{
    va_list ap;
    va_start(ap, fmt);
    fprintf(stderr, "mblatency: warning: ");
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

static long long parse_size_suffix(const char *s)
{
    char *end = NULL;
    long long v = strtoll(s, &end, 10);
    if (end == s || v <= 0)
        return -1;
    while (*end == ' ' || *end == '\t')
        end++;
    if (*end == 'K' || *end == 'k')
        v *= 1024;
    else if (*end == 'M' || *end == 'm')
        v *= 1024 * 1024;
    else if (*end == 'G' || *end == 'g')
        v *= 1024ull * 1024 * 1024;
    else if (*end != '\0' && *end != ',')
        return -1;
    return v;
}

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

/* Median exactly as membench's stats.rs defines it. */
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
/* sysfs                                                               */
/* ------------------------------------------------------------------ */

typedef struct {
    long long l1d, l2, l3, line;
    int core_cpus[256];
    int ncore;
    int nall;
} Machine;

static void detect(Machine *m)
{
    char path[256], buf[512];
    memset(m, 0, sizeof(*m));
    m->line = DEFAULT_LINE;

    for (int cpu = 0; cpu < 256; cpu++) {
        snprintf(path, sizeof(path),
                 "/sys/devices/system/cpu/cpu%d/topology/thread_siblings_list", cpu);
        if (read_small(path, buf, sizeof(buf)) != 0)
            continue;
        m->nall++;
        if (strtoll(buf, NULL, 10) == cpu && m->ncore < 256)
            m->core_cpus[m->ncore++] = cpu;
    }
    for (int idx = 0; idx < 16; idx++) {
        int level, is_instr;
        long long size;
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
        size = parse_size_suffix(buf);
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

static void thp_setting(char *out, size_t cap)
{
    char buf[256];
    out[0] = '\0';
    if (read_small("/sys/kernel/mm/transparent_hugepage/enabled", buf, sizeof(buf)) != 0)
        return;
    char *o = strchr(buf, '['), *c = o ? strchr(o, ']') : NULL;
    if (!o || !c)
        return;
    o++;
    size_t n = (size_t)(c - o);
    if (n >= cap)
        n = cap - 1;
    memcpy(out, o, n);
    out[n] = '\0';
}

/* ------------------------------------------------------------------ */
/* buffer + huge-page verification                                     */
/* ------------------------------------------------------------------ */

static volatile uint64_t sink64;

static long long smaps_anon_huge(const void *addr)
{
    FILE *f = fopen("/proc/self/smaps", "r");
    char line[512];
    unsigned long long target = (unsigned long long)(uintptr_t)addr;
    int inside = 0, got = 0;
    long long bytes = -1;
    if (!f)
        return -1;
    while (fgets(line, sizeof(line), f)) {
        unsigned long long a, b;
        if (sscanf(line, "%llx-%llx", &a, &b) == 2) {
            if (inside)
                break;
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
    return got ? bytes : -1;
}

typedef struct {
    void *addr;
    size_t len;
    long long huge_bytes;
    int madvise_errno;
} Buf;

static Buf buf_alloc(size_t len, int request_huge)
{
    Buf b = {NULL, 0, -1, 0};
    void *base;
    uintptr_t aligned;
    size_t head, tail;

    size_t raw;
    len = (len + PAGE_SZ - 1) & ~(size_t)(PAGE_SZ - 1);
    raw = len + TWO_MIB; /* over-map so an aligned interior window can be kept */
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

    if (request_huge && madvise(b.addr, len, MADV_HUGEPAGE) != 0)
        b.madvise_errno = errno;

    /* Prefault one word per page, before any timing and before the chase. */
    {
        volatile uint64_t *w = (volatile uint64_t *)b.addr;
        size_t stride = PAGE_SZ / sizeof(uint64_t);
        size_t words = len / sizeof(uint64_t);
        for (size_t i = 0; i < words; i += stride)
            w[i] = 0;
        w[words - 1] = 0;
    }
    b.huge_bytes = smaps_anon_huge(b.addr);
    return b;
}

static void buf_free(Buf *b)
{
    if (b->addr && munmap(b->addr, b->len) != 0)
        warn_msg("munmap: %s", strerror(errno));
    b->addr = NULL;
}

/* ------------------------------------------------------------------ */
/* measured core clock (never cpufreq)                                 */
/* ------------------------------------------------------------------ */

/*
 * A dependent integer add chain retires one add per cycle, so its rate is the
 * clock.  The volatile zero is load-bearing: without an operand gcc folds the
 * whole chain into a multiply and the "clock" comes out at 10 000 000 MHz.
 */
static volatile uint64_t clock_zero;

static __attribute__((noinline)) uint64_t add_chain(uint64_t n, uint64_t seed)
{
    uint64_t x = seed | 1;
    const uint64_t k = seed | 3;
    for (uint64_t i = 0; i < n; i++)
        x += k + clock_zero;
    return x;
}

static double measure_mhz(double target_ms)
{
    const uint64_t cal = 1u << 20;
    double t0 = now_s();
    sink64 = add_chain(cal, 0x5EED1234u);
    double dt = now_s() - t0;
    if (dt <= 0.0)
        return 0.0;
    double per_add = dt / (double)cal;
    uint64_t n = (uint64_t)((target_ms * 1e-3) / (per_add > 1e-12 ? per_add : 1e-12));
    if (n < (1u << 14))
        n = 1u << 14;
    t0 = now_s();
    sink64 = add_chain(n, 0x5EED1234u);
    dt = now_s() - t0;
    if (dt <= 0.0)
        return 0.0;
    return (double)n / dt / 1e6;
}


/*
 * Two samples, faster one kept.  Contamination is one-directional: being
 * preempted, or sharing the core with a busy SMT sibling, can only make the
 * chain look slower than the core really was, never faster.  So the larger
 * sample is the closer estimate, and the smaller one is thrown away rather than
 * averaged in.
 */
static double measure_mhz_best(double target_ms, int samples)
{
    double best = 0.0;
    for (int i = 0; i < samples; i++) {
        double m = measure_mhz(target_ms);
        if (m > best)
            best = m;
    }
    return best;
}

static const char *clock_sanity(double mhz)
{
    if (mhz <= 0.0)
        return "the clock measurement failed, so cycles/load is not reported";
    if (mhz < 100.0 || mhz > 12000.0)
        return "implausible clock, cycle figures are unreliable";
    return NULL;
}

/* ------------------------------------------------------------------ */
/* the chain                                                           */
/* ------------------------------------------------------------------ */

static uint64_t xorshift64star(uint64_t *state)
{
    uint64_t x = *state;
    if (x == 0)
        x = 0x2545F4914F6CDD1Dull;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    return x * 0x2545F4914F6CDD1Dull;
}

/*
 * Sattolo, not Fisher-Yates: Sattolo always yields exactly one cycle, which is
 * the whole point -- a Fisher-Yates shuffle yields many short cycles, and after
 * a few thousand loads the chase is bouncing around inside L1 and the number
 * stops meaning anything about the buffer size.
 */
static void sattolo(size_t *v, size_t n, uint64_t seed)
{
    uint64_t state = seed | 1;
    for (size_t i = 1; i < n; i++) {
        size_t j = (size_t)(xorshift64star(&state) % (uint64_t)i);
        size_t t = v[i];
        v[i] = v[j];
        v[j] = t;
    }
}

/* One dependent load per iteration; the next address comes out of this one.
 * gcc cannot vectorise, prefetch or shorten this without changing the program
 * semantics, which is exactly why the chase measures latency. */
static __attribute__((noinline)) void *chase(void *start, unsigned long long loads)
{
    void *p = start;
    for (unsigned long long i = 0; i < loads; i++)
        p = *(void **)p;
    return p;
}

/* ------------------------------------------------------------------ */
/* one measured point                                                  */
/* ------------------------------------------------------------------ */

typedef struct {
    size_t size;
    char label[16];   /* "l1d"/"l2"/"l3"/"memory" when --sizes auto produced it */
    size_t nodes;
    double ns;        /* median ns per dependent load */
    double best_ns;
    double spread_pct;
    double trials[MAX_TRIALS];
    int ntrial;
    double clock_mhz;
    long long huge_bytes;
    int madvise_errno;
    int too_small;
} Point;

static void measure_point(const Point *in, Point *out, int request_huge,
                          size_t line, uint64_t seed, int trials, int duration_ms)
{
    Buf b;
    size_t nodes;
    size_t *order;
    uintptr_t base;
    unsigned long long passes, loads, effective;
    double warm, target;
    void *head;

    memset(out, 0, sizeof(*out));
    out->size = in->size;
    memcpy(out->label, in->label, sizeof(out->label));
    out->madvise_errno = 0;
    out->huge_bytes = -1;

    if (line < 8 || (line & (line - 1)) != 0)
        die("cache line %zu is not a power of two >= 8", line);

    nodes = in->size / line;
    if (nodes < 2) {
        out->too_small = 1;
        return;
    }
    out->nodes = nodes;

    b = buf_alloc(nodes * line, request_huge);
    base = (uintptr_t)b.addr;

    /* Lay out the single closed cycle: one pointer per cache line. */
    order = malloc(nodes * sizeof(size_t));
    if (!order)
        die("out of memory for %zu chain indices", nodes);
    for (size_t i = 0; i < nodes; i++)
        order[i] = i;
    sattolo(order, nodes, seed);
    for (size_t k = 0; k < nodes; k++) {
        uintptr_t from = base + order[k] * line;
        uintptr_t to = base + order[(k + 1) % nodes] * line;
        *(uintptr_t *)(void *)from = to;
    }
    head = (void *)(base + order[0] * line);
    free(order);

    out->huge_bytes = b.huge_bytes;

    /* Warm-up: one full pass, untimed, only used to size the trials. */
    {
        double w0 = now_s();
        sink64 = (uint64_t)(uintptr_t)chase(head, nodes);
        warm = now_s() - w0;
    }
    if (warm <= 0.0)
        warm = 1e-6;

    target = (double)(duration_ms > TRIAL_MS_CAP ? TRIAL_MS_CAP : duration_ms) * 1e-3;
    passes = (unsigned long long)(target / warm);
    if (passes < 1)
        passes = 1;
    loads = nodes * passes;
    /* The chase below performs every load it is asked for; the mask keeps the
     * number of loads identical across trials, as membench's kernel does. */
    effective = loads & ~15ull;
    if (effective < nodes)
        effective = loads;

    for (int t = 0; t < trials && t < MAX_TRIALS; t++) {
        double s = now_s();
        sink64 = (uint64_t)(uintptr_t)chase(head, effective);
        double dt = now_s() - s;
        out->trials[t] = dt / (double)effective * 1e9;
        out->ntrial = t + 1;
    }

    if (out->ntrial) {
        double sorted[MAX_TRIALS];
        double lo, hi;
        memcpy(sorted, out->trials, sizeof(double) * (size_t)out->ntrial);
        out->ns = median_of(sorted, (size_t)out->ntrial);
        lo = hi = sorted[0];
        for (int t = 1; t < out->ntrial; t++) {
            if (sorted[t] < lo)
                lo = sorted[t];
            if (sorted[t] > hi)
                hi = sorted[t];
        }
        out->best_ns = lo; /* latency: the minimum is the peak */
        out->spread_pct = out->ns > 0.0 ? (hi - lo) / out->ns * 100.0 : 0.0;
    }

    /* Same core, same thermal moment as the chase. */
    out->clock_mhz = measure_mhz_best(15.0, 2);

    buf_free(&b);
}

/* ------------------------------------------------------------------ */
/* sizing tokens (--sizes auto)                                        */
/* ------------------------------------------------------------------ */

/*
 * The sizes membench's own per-level latency cells use: the bandwidth buffer
 * for that level, floored at 8 KiB.  Producing the same sizes here is what
 * makes the latency cells in compare.py comparable rather than merely
 * same-shaped.
 */
static int auto_sizes(const Machine *m, Point *pts, int max)
{
    int n = 0;
    struct {
        const char *label;
        size_t bytes;
    } want[4];

    memset(want, 0, sizeof(want));
    if (m->l1d > 0) {
        want[n].label = "l1d";
        want[n].bytes = (size_t)(m->l1d / 2);
        if (want[n].bytes < 8192)
            want[n].bytes = 8192;
        n++;
    }
    if (m->l2 > 0) {
        want[n].label = "l2";
        want[n].bytes = (size_t)(m->l2 / 2);
        if (want[n].bytes < 8192)
            want[n].bytes = 8192;
        n++;
    }
    if (m->l3 > 0) {
        long long l2 = m->l2 > 0 ? m->l2 : m->l3 / 8;
        int cores = m->ncore > 0 ? m->ncore : 1;
        size_t share, floor2 = (size_t)(2 * l2);
        while (cores > 1 && (size_t)((double)m->l3 * 0.75 / cores) < floor2)
            cores--;
        share = (size_t)((double)m->l3 * 0.75 / cores);
        if (share < floor2)
            share = floor2;
        want[n].label = "l3";
        want[n].bytes = share < 8192 ? 8192 : share;
        n++;
    }
    {
        unsigned long long total = ONE_GIB;
        unsigned long long avail = mem_available_bytes();
        int threads = m->nall > 0 ? m->nall : 1;
        if (m->l3 > 0 && (unsigned long long)(16 * (m->l3 + 1)) > total)
            total = (unsigned long long)(16 * (m->l3 + 1));
        if (avail && total > avail / 2)
            total = avail / 2;
        want[n].label = "memory";
        want[n].bytes = (size_t)(total / (unsigned long long)threads);
        if (want[n].bytes < 8192)
            want[n].bytes = 8192;
        n++;
    }

    for (int i = 0; i < n && max > 0; i++) {
        pts->size = want[i].bytes & ~(size_t)(PAGE_SZ - 1);
        snprintf(pts->label, sizeof(pts->label), "%s", want[i].label);
        pts++;
        max--;
    }
    return n;
}

/* ------------------------------------------------------------------ */
/* options                                                             */
/* ------------------------------------------------------------------ */

static void usage(FILE *f)
{
    fputs(
        "usage: mblatency [--sizes LIST] [--trials N] [--duration MS] [--cpu N]\n"
        "                 [--seed N] [--line N] [--no-huge] [--json PATH] [--help]\n"
        "\n"
        "Dependent pointer-chase latency: one pointer per cache line, one closed\n"
        "cycle (Sattolo), pinned to one CPU, ns per dependent load.\n"
        "LIST is a comma-separated list of sizes (4K, 32K, 256K, 1M, 16M, 256M, 1G)\n"
        "or the word `auto`, which expands to the buffer sizes membench's own\n"
        "per-level latency cells use so the two can be compared cell for cell.\n"
        "Cycles/load use a core clock measured here with a dependent add chain,\n"
        "never read from cpufreq.\n",
        f);
}

/* ------------------------------------------------------------------ */
/* main                                                                */
/* ------------------------------------------------------------------ */

int main(int argc, char **argv)
{
    char size_list[1024] = "auto";
    Point req[MAX_SIZES], got[MAX_SIZES];
    int npts = 0;
    int trials = 5, duration_ms = 100, cpu = -1, request_huge = 1;
    uint64_t seed = 0x5EED12349ABCDEFull;
    size_t line_override = 0;
    const char *json_path = NULL;
    Machine m;
    char thp[32];

    for (int i = 1; i < argc; i++) {
        const char *a = argv[i];
        const char *v = (i + 1 < argc) ? argv[i + 1] : NULL;
        if (!strcmp(a, "--help") || !strcmp(a, "-h")) {
            usage(stdout);
            return 0;
        } else if (!strcmp(a, "--sizes")) {
            if (!v) { usage(stderr); return 2; }
            snprintf(size_list, sizeof(size_list), "%s", v);
            i++;
        } else if (!strcmp(a, "--trials")) {
            if (!v) { usage(stderr); return 2; }
            trials = atoi(v);
            if (trials < 1 || trials > MAX_TRIALS)
                die("--trials must be 1..%d", MAX_TRIALS);
            i++;
        } else if (!strcmp(a, "--duration")) {
            if (!v) { usage(stderr); return 2; }
            duration_ms = atoi(v);
            if (duration_ms < 5)
                die("--duration must be at least 5 ms");
            i++;
        } else if (!strcmp(a, "--cpu")) {
            if (!v) { usage(stderr); return 2; }
            cpu = atoi(v);
            if (cpu < 0)
                die("--cpu must be >= 0");
            i++;
        } else if (!strcmp(a, "--seed")) {
            if (!v) { usage(stderr); return 2; }
            seed = strtoull(v, NULL, 0);
            i++;
        } else if (!strcmp(a, "--line")) {
            if (!v) { usage(stderr); return 2; }
            line_override = (size_t)strtoull(v, NULL, 10);
            i++;
        } else if (!strcmp(a, "--no-huge")) {
            request_huge = 0;
        } else if (!strcmp(a, "--json")) {
            if (!v) { usage(stderr); return 2; }
            json_path = v;
            i++;
        } else {
            fprintf(stderr, "mblatency: unknown argument '%s'\n", a);
            usage(stderr);
            return 2;
        }
    }

    if (!optimised_build)
        fprintf(stderr,
                "mblatency: WARNING: compiled without optimisation (__OPTIMIZE__ is not set).\n"
                "mblatency: WARNING: the chase itself survives, but the clock chain and the\n"
                "mblatency: WARNING: window sizing do not: rebuild with -O2/-O3 before quoting.\n");

    detect(&m);
    if (line_override)
        m.line = (long long)line_override;
    thp_setting(thp, sizeof(thp));

    /* Parse the size list.  `auto` may stand alone or be mixed with explicit
     * sizes, because the useful run is "the four level cells plus a sweep".
     * Duplicated sizes are dropped: the labelled level cell wins, so a size
     * never gets measured twice under two names. */
    {
        char *save = NULL;
        Point tmp[MAX_SIZES];
        int ntmp = 0;
        for (char *tok = strtok_r(size_list, ",", &save); tok;
             tok = strtok_r(NULL, ",", &save)) {
            while (*tok == ' ')
                tok++;
            if (!*tok)
                continue;
            if (!strcmp(tok, "auto")) {
                Point auto_pts[MAX_SIZES];
                int n = auto_sizes(&m, auto_pts, MAX_SIZES);
                for (int i = 0; i < n && ntmp < MAX_SIZES; i++)
                    tmp[ntmp++] = auto_pts[i];
                continue;
            }
            long long bytes = parse_size_suffix(tok);
            if (bytes <= 0)
                die("bad size '%s' in --sizes", tok);
            if (ntmp < MAX_SIZES) {
                tmp[ntmp].size = (size_t)bytes;
                tmp[ntmp].label[0] = '\0';
                ntmp++;
            }
        }
        for (int i = 0; i < ntmp && npts < MAX_SIZES; i++) {
            int dup = 0;
            for (int j = 0; j < i; j++)
                if (tmp[j].size == tmp[i].size)
                    dup = 1;
            if (dup)
                continue;
            req[npts++] = tmp[i];
        }
    }
    if (npts == 0)
        die("no sizes to measure");

    if (cpu < 0)
        cpu = m.ncore > 0 ? m.core_cpus[0] : 0;

    {
        cpu_set_t set;
        CPU_ZERO(&set);
        CPU_SET(cpu, &set);
        /* Pin before allocating, then verify: an unpinned chase wanders between
         * cores and measures the scheduler. */
        if (sched_setaffinity(0, sizeof(set), &set) != 0)
            die("sched_setaffinity(cpu %d): %s", cpu, strerror(errno));
        CPU_ZERO(&set);
        if (sched_getaffinity(0, sizeof(set), &set) == 0 &&
            (!CPU_ISSET(cpu, &set) || CPU_COUNT(&set) != 1))
            die("affinity to cpu %d did not stick", cpu);
    }

    double run_clock = measure_mhz_best(20.0, 2);

    printf("mblatency -- dependent pointer-chase latency reference\n");
    printf("build        : %s\n",
           optimised_build ? "optimised (-O)" : "UNOPTIMISED (-O0): NUMBERS ARE MEANINGLESS");
    printf("cpu          : %d (pinned, verified; chase and clock on this core only)\n", cpu);
    printf("caches       : L1d %s, L2 %s, L3 %s, line %lld B\n",
           m.l1d ? human_bytes((unsigned long long)m.l1d) : "unknown",
           m.l2 ? human_bytes((unsigned long long)m.l2) : "unknown",
           m.l3 ? human_bytes((unsigned long long)m.l3) : "unknown", m.line);
    printf("chain        : one pointer per %lld B line, single closed cycle (Sattolo), seed 0x%llx\n",
           m.line, (unsigned long long)seed);
    printf("windows      : %d trials of up to %d ms after 1 untimed pass, median reported\n",
           trials, duration_ms > TRIAL_MS_CAP ? TRIAL_MS_CAP : duration_ms);
    printf("thp setting  : %s%s\n", thp[0] ? thp : "unknown",
           request_huge ? "" : " (2 MiB pages not requested: --no-huge)");
    printf("clock        : %.0f MHz at start, measured with a dependent add chain"
           " (not cpufreq)\n", run_clock);
    {
        const char *bad = clock_sanity(run_clock);
        if (bad)
            printf("WARNING      : %s (%.0f MHz)\n", bad, run_clock);
    }
    printf("\n%10s %9s %10s %9s %10s %8s %6s %s\n",
           "size", "nodes", "ns/load", "cycles", "MHz", "spread", "trial", "huge pages");
    fflush(stdout);

    for (int i = 0; i < npts; i++) {
        measure_point(&req[i], &got[i], request_huge, (size_t)m.line, seed, trials, duration_ms);
        if (got[i].too_small) {
            printf("%10s %9s %10s -- refused: the buffer holds fewer than two %lld B\n"
                   "           nodes, so there is nothing to chase\n",
                   human_bytes((unsigned long long)req[i].size), "-", "-", (long long)m.line);
            continue;
        }
        /* Say which of the three "no huge pages" cases this is: refused, or
         * never asked for.  They are different facts and the reader of the
         * number needs to know which caveat applies. */
        const char *huge_note;
        if (got[i].huge_bytes < 0)
            huge_note = "unknown (no AnonHugePages line in smaps)";
        else if (got[i].huge_bytes > 0)
            huge_note = "granted";
        else if (!request_huge)
            huge_note = "not requested (--no-huge): 4 KiB mapped, so page walks are"
                        " part of this number";
        else
            huge_note = "0 of the requested bytes granted, kernel refused: 4 KiB"
                        " mapped, so page walks are part of this number";
        printf("%10s %9zu %10.3f %9.2f %10.0f %7.1f%% %6d %s\n",
               human_bytes((unsigned long long)got[i].size), got[i].nodes, got[i].ns,
               got[i].clock_mhz > 0.0 ? got[i].ns * got[i].clock_mhz / 1000.0 : 0.0,
               got[i].clock_mhz, got[i].spread_pct, got[i].ntrial, huge_note);
        fflush(stdout);
    }

    /* Monotonicity is a property of the machine, not of this program: say so
     * when the curve goes backwards, because that means something else was
     * running or the sizes did not cross a cache boundary. */
    {
        /* Monotonic in SIZE, not in the order the flags were typed: `--sizes
         * auto,4K,...` interleaves, so walking the list compared 2 MiB with
         * 4 KiB and cried wolf. */
        int inversions = 0;
        size_t prev = 0;
        double prev_ns = 0.0;
        for (size_t want = 0; want < (size_t)npts; want++) {
            int best = -1;
            for (int i = 0; i < npts; i++) {
                if (got[i].too_small || got[i].size <= prev)
                    continue;
                if (best < 0 || got[i].size < got[best].size)
                    best = i;
            }
            if (best < 0)
                break;
            if (prev && got[best].ns < prev_ns * 0.98)
                inversions++;
            prev = got[best].size;
            prev_ns = got[best].ns;
        }
        if (inversions)
            printf("\nNOTE: the curve is not monotonic at %d point(s): something else was"
                   " running,\nor the sizes did not cross a cache boundary.\n", inversions);
    }

    if (json_path) {
        FILE *f = fopen(json_path, "wb");
        if (!f)
            die("--json %s: %s", json_path, strerror(errno));
        fputs("{\n", f);
        fputs("  \"tool\": \"mblatency\",\n", f);
        fputs("  \"tool_version\": \"1\",\n", f);
        fputs("  \"schema_version\": 1,\n", f);
        fputs("  \"units\": \"ns per dependent load\",\n", f);
        fprintf(f, "  \"optimised_build\": %s,\n", optimised_build ? "true" : "false");
        fprintf(f, "  \"cpu\": %d,\n", cpu);
        fprintf(f, "  \"cache_bytes\": {\"l1d\": %lld, \"l2\": %lld, \"l3\": %lld, \"line\": %lld},\n",
                m.l1d, m.l2, m.l3, m.line);
        fprintf(f, "  \"thp_setting\": \"%s\",\n", thp);
        fprintf(f, "  \"pages_requested\": \"%s\",\n", request_huge ? "2m" : "4k");
        fprintf(f, "  \"seed\": %llu,\n", (unsigned long long)seed);
        fprintf(f, "  \"trials\": %d,\n", trials);
        fprintf(f, "  \"duration_ms\": %d,\n", duration_ms);
        fprintf(f, "  \"clock_mhz_at_start\": %.1f,\n", run_clock);
        fputs("  \"clock_method\": \"dependent integer add chain on the measured core,"
              " never cpufreq\",\n", f);
        fputs("  \"points\": [\n", f);
        for (int i = 0, first = 1; i < npts; i++) {
            const Point *p = &got[i];
            if (p->too_small)
                continue;
            if (!first)
                fputs(",\n", f);
            first = 0;
            fputs("    {", f);
            fprintf(f, "\"size_bytes\": %zu, ", p->size);
            fprintf(f, "\"label\": %s%s%s, ", p->label[0] ? "\"" : "",
                    p->label[0] ? p->label : "null", p->label[0] ? "\"" : "");
            fprintf(f, "\"nodes\": %zu, ", p->nodes);
            fprintf(f, "\"ns\": %.6f, ", p->ns);
            fprintf(f, "\"best_ns\": %.6f, ", p->best_ns);
            fprintf(f, "\"cycles\": %.3f, ", p->clock_mhz > 0.0 ? p->ns * p->clock_mhz / 1000.0 : 0.0);
            fprintf(f, "\"spread_pct\": %.3f, ", p->spread_pct);
            fprintf(f, "\"clock_mhz\": %.1f, ", p->clock_mhz);
            fprintf(f, "\"pages_actual\": \"%s\", ", p->huge_bytes > 0 ? "2m" : "4k");
            if (p->huge_bytes < 0)
                fputs("\"huge_pages_bytes\": null, ", f);
            else
                fprintf(f, "\"huge_pages_bytes\": %lld, ", p->huge_bytes);
            fprintf(f, "\"madvise_hugepage_errno\": %d, ", p->madvise_errno);
            fputs("\"trials\": [", f);
            for (int t = 0; t < p->ntrial; t++)
                fprintf(f, "%s%.6f", t ? ", " : "", p->trials[t]);
            fputs("]}", f);
        }
        fputs("\n  ],\n", f);
        /* Level cells for compare.py: the point tagged by --sizes auto. */
        fputs("  \"level_latency\": [\n", f);
        for (int i = 0, first = 1; i < npts; i++) {
            const Point *p = &got[i];
            if (p->too_small || !p->label[0])
                continue;
            if (!first)
                fputs(",\n", f);
            first = 0;
            fprintf(f, "    {\"level\": \"%s\", \"size_bytes\": %zu, \"ns\": %.6f,"
                       " \"cycles\": %.3f, \"clock_mhz\": %.1f}",
                    p->label, p->size, p->ns,
                    p->clock_mhz > 0.0 ? p->ns * p->clock_mhz / 1000.0 : 0.0, p->clock_mhz);
        }
        fputs("\n  ]\n}\n", f);
        if (fclose(f) != 0)
            die("fclose(%s): %s", json_path, strerror(errno));
        printf("\nwrote %s\n", json_path);
    }
    return 0;
}
