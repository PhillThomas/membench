//! What else the machine was doing while the run happened.
//!
//! This exists because of one specific failure. A cell was observed to move by a
//! factor of three on one machine and the run was cleared of interference on the
//! strength of a load average that the benchmark itself was producing: the cell
//! used eight threads, and eight threads for two and a half seconds every
//! twenty-nine accounts for most of the 1.15 that was logged as "flat". A single
//! core doing memory-heavy work can cost a bandwidth figure far more than its
//! share of a load average suggests, so a load average taken during a run is a
//! poor instrument for the question "was anything else here".
//!
//! PSI (`/proc/pressure/*`) is the kernel's own answer to that question and needs
//! no sampling window of us: `some avg10` is the percentage of the last ten
//! seconds during which at least one task was stalled on that resource. It is
//! coarse — ten seconds is fifty cells — and it is still better than nothing,
//! which is what the exported document used to carry on this subject.
//!
//! The honest limitation, stated here so nobody reads the field as a guarantee:
//! this samples at the start of a run and at the end of it. Nothing here can see
//! a process that arrived between two cells and left again, and on a kernel
//! without PSI compiled in it sees only the load average and who was runnable at
//! the two instants.
//!
//! And the warning is narrower than the field. It fires on stalls, not on how
//! many things are awake: two `yes` processes spinning on a twelve-core machine
//! leave `cpu some` at 0.0 — measured, not reasoned — because nothing is waiting
//! for a core. That is the same shape as the observation that started this
//! investigation, so this module is an instrument and not a verdict. The list of
//! runnable processes exists so a human can make the judgement the thresholds
//! cannot.

use crate::result::{Interference, InterferenceSample, RunningProcess, Warning};
use crate::sysinfo::read_to_string;

/// Most processes to name in a sample. The count of what was running matters
/// more than the list, so the list is capped rather than exhaustive.
const MAX_NAMED: usize = 8;

/// Warn above these. They are ours, not the kernel's, and the warning says so:
/// 5% of wall time with someone runnable, 1% stalled on memory, 20% stalled on
/// IO. The first is low on purpose. A run that shares a core with anything is a
/// run whose bandwidth number is not comparable to an idle one, and the whole
/// point of the document is comparability.
const WARN_CPU_SOME: f64 = 5.0;
const WARN_MEM_SOME: f64 = 1.0;
const WARN_IO_SOME: f64 = 20.0;

/// `some avg10=0.50 avg60=... total=...` out of one PSI file.
fn psi_some_avg10(text: &str) -> Option<f64> {
    let some = text.lines().find(|l| l.starts_with("some "))?;
    let field = some
        .split_whitespace()
        .find(|f| f.starts_with("avg10="))?
        .trim_start_matches("avg10=");
    field.parse().ok()
}

/// State and last-CPU from a `/proc/<pid>/stat` line. The comm field is in
/// parentheses and may itself contain parentheses and spaces, so the split has
/// to start after the last one rather than at the second field.
fn parse_stat(line: &str) -> Option<(char, Option<usize>, String)> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    if close < open {
        return None;
    }
    let comm = line[open + 1..close].to_string();
    let rest = line[close + 1..].trim();
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `state` is the first field left, and `processor` is at index 36 after it.
    // That position was not trusted to a manual count: a process pinned with
    // `taskset -c 5` reads 5 here and `taskset -c 9` reads 9. The field before
    // it is `exit_signal`, which is 17 for an ordinary process, so
    // an off-by-one here does not fail — it reports CPU 17 on a twelve-CPU
    // machine, which is a number a reader would never question. That is the
    // entire argument for testing it.
    let state = fields.first()?.chars().next()?;
    let cpu = fields.get(36).and_then(|f| f.parse().ok());
    Some((state, cpu, comm))
}

/// Read the pressure files, the load average, and who was runnable, right now.
///
/// Process-level, not thread-level: a process listed here has *a* thread
/// runnable, which is all this can tell, and it is enough for the only question
/// the field answers, which is whether the run had the machine to itself.
pub fn sample() -> InterferenceSample {
    let mut s = InterferenceSample {
        at: crate::time::now_rfc3339(),
        ..Default::default()
    };

    // PSI is optional in the kernel build, and its absence is worth recording
    // as its own fact rather than three silent nulls.
    let mut read_psi = 0;
    for (path, present) in [
        ("/proc/pressure/cpu", Some(&mut s.cpu_some_avg10)),
        ("/proc/pressure/memory", Some(&mut s.memory_some_avg10)),
        ("/proc/pressure/io", Some(&mut s.io_some_avg10)),
    ] {
        if let Some(text) = read_to_string(path) {
            read_psi += 1;
            if let Some(slot) = present {
                *slot = psi_some_avg10(&text);
            }
        }
    }

    if let Some(text) = read_to_string("/proc/loadavg") {
        let mut nums = text
            .split_whitespace()
            .take(3)
            .filter_map(|f| f.parse().ok());
        s.loadavg = [
            nums.next().unwrap_or(0.0),
            nums.next().unwrap_or(0.0),
            nums.next().unwrap_or(0.0),
        ];
    }

    let mut named = 0;
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let Some(text) = read_to_string(&format!("/proc/{name}/stat")) else {
                continue;
            };
            let Some((state, cpu, comm)) = parse_stat(text.trim_end()) else {
                continue;
            };
            match state {
                'R' => {
                    s.runnable_total += 1;
                    if named < MAX_NAMED {
                        named += 1;
                        s.running.push(RunningProcess {
                            pid: name.parse().unwrap_or(0),
                            comm,
                            cpu,
                        });
                    }
                }
                // Blocked in the kernel is the other half of "something is
                // happening that we are not measuring".
                'D' => s.uninterruptible += 1,
                _ => {}
            }
        }
    }

    s.source = if read_psi == 3 {
        "psi + loadavg + /proc/<pid>/stat".to_string()
    } else if read_psi > 0 {
        format!("{read_psi} of 3 psi files present; loadavg + /proc/<pid>/stat")
    } else {
        "no /proc/pressure in this kernel; loadavg + /proc/<pid>/stat only".to_string()
    };
    s
}

/// The start and end samples, plus the warning they deserve, if any.
///
/// Both instants are reported even when they agree, because the shape of the
/// disagreement is the thing a reader wants: pressure rising over a run says
/// something different from pressure that was there from the beginning.
pub fn snapshot(start: InterferenceSample) -> Interference {
    let end = sample();
    let warning = pressure_warning(&start, &end);
    Interference {
        start,
        end,
        warning,
    }
}

/// The worst `some avg10` seen at either end of the run for one resource.
///
/// `None` means the kernel did not report it, which is deliberately not the
/// same as zero: a machine without PSI must not read as a machine at rest.
fn worst(start: Option<f64>, end: Option<f64>) -> Option<f64> {
    match (start, end) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

/// Decide whether the two samples make the run non-comparable, and say it in the
/// document's own words. Split out from [`snapshot`] so that the thresholds can
/// be tested without loading a machine on purpose.
fn pressure_warning(
    start: &InterferenceSample,
    end: &InterferenceSample,
) -> Option<crate::result::Warning> {
    let cpu = worst(start.cpu_some_avg10, end.cpu_some_avg10);
    let mem = worst(start.memory_some_avg10, end.memory_some_avg10);
    let io = worst(start.io_some_avg10, end.io_some_avg10);
    let mut what = Vec::new();
    if cpu.unwrap_or(0.0) >= WARN_CPU_SOME {
        what.push(format!("cpu {:.1}%", cpu.unwrap_or(0.0)));
    }
    if mem.unwrap_or(0.0) >= WARN_MEM_SOME {
        what.push(format!("memory {:.1}%", mem.unwrap_or(0.0)));
    }
    if io.unwrap_or(0.0) >= WARN_IO_SOME {
        what.push(format!("io {:.1}%", io.unwrap_or(0.0)));
    }
    if what.is_empty() {
        return None;
    }
    Some(Warning::new(
        "interference",
        format!(
            "over this run the kernel reported pressure on {}",
            what.join(" and ")
        ),
    )
    .with_hint(
        "the thresholds are ours and the numbers are the kernel's; `interference.running` names \
         what was runnable when the run started and ended, and a bandwidth figure taken on a \
         busy machine is not comparable with one taken on an idle one",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cpu_field_is_a_cpu_on_this_machine() {
        // The index was established by hand, twice: a process started under
        // `taskset -c 5` reports 5 as the thirty-seventh field after the closing
        // parenthesis, and `taskset -c 9` reports 9. What this test adds is the
        // guard against someone editing the index and getting a plausible number
        // back — the neighbouring field is `exit_signal`, 17 for an ordinary
        // process, which on a machine with more than seventeen CPUs would look
        // entirely normal.
        //
        // The earlier version of this test pinned the thread and expected the
        // kernel to have caught up by the time `/proc` was read again. It passed
        // in isolation and failed inside a full-suite run, which is the worst
        // kind of flaky: a red light that depends on what ran before it. The
        // hand-verification above is reproducible and cheap, so the check stays
        // here and the timing dependence goes.
        let real = std::fs::read_to_string("/proc/self/stat").expect("readable");
        // Counted over lines, not by matching `"\nprocessor\t:"` throughout the
        // text: that misses the first processor because nothing precedes it with
        // a newline, and a machine that is one CPU smaller than it is rejects a
        // legitimate CPU number.
        let online = std::fs::read_to_string("/proc/cpuinfo")
            .map(|t| {
                t.lines()
                    .filter(|l| l.starts_with("processor\t:"))
                    .count()
                    .max(1)
            })
            .unwrap_or(0);
        let (state, cpu, comm) =
            parse_stat(real.trim_end()).expect("this kernel writes a parseable stat line");
        assert!(!comm.is_empty());
        assert!(
            matches!(state, 'R' | 'S' | 'D' | 'T' | 'Z'),
            "state {state}"
        );
        assert!(
            matches!(cpu, Some(c) if c < online),
            "cpu {cpu:?} is not one of this machine's {online} processors: {real}"
        );
    }

    #[test]
    fn psi_lines_parse_the_way_the_kernel_writes_them() {
        let cpu = "some avg10=4.30 avg60=2.10 avg300=0.70 total=1207649\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n";
        assert_eq!(psi_some_avg10(cpu), Some(4.3));
        // `full` must not be mistaken for `some`.
        let only_full = "full avg10=9.90 avg60=9.90 avg300=9.90 total=1\n";
        assert_eq!(psi_some_avg10(only_full), None);
        assert_eq!(psi_some_avg10("some avg10=abc\n"), None);
    }

    /// Build a stat line the way the kernel writes one, so the position of the
    /// CPU is stated in the test rather than inherited from the code. Thirty-
    /// five fields sit between `state` and `processor`; the last of them is
    /// `exit_signal`, always 17, which is what makes an off-by-one here survive
    /// as a plausible-looking CPU number.
    fn stat_line(comm: &str, state: char, cpu: usize) -> String {
        format!("1 ({}) {state} {}17 {cpu} 0 0 0 0 0", comm, "0 ".repeat(34))
    }

    #[test]
    fn stat_lines_survive_a_nasty_comm() {
        // The comm in the middle contains a closing parenthesis and spaces: a
        // naive split would shift every field after it, which is exactly how a
        // process's exit signal ends up reported as its CPU.
        let line = stat_line("weird (proc) name", 'R', 7);
        let (state, cpu, comm) = parse_stat(&line).expect("parses");
        assert_eq!(state, 'R');
        assert_eq!(comm, "weird (proc) name");
        assert_eq!(
            cpu,
            Some(7),
            "the processor is not the 37th field after the comm: {line}"
        );
        assert!(parse_stat("no parens here").is_none());
    }

    #[test]
    fn a_sample_from_this_machine_reports_what_it_read() {
        let s = sample();
        assert!(!s.source.is_empty());
        assert!(
            s.at.contains('T'),
            "the sample must be timestamped: {}",
            s.at
        );
        // Whatever else the host looks like, the load average is always there.
        assert!(
            s.loadavg.iter().all(|v| v.is_finite()),
            "loadavg {:?}",
            s.loadavg
        );
        assert!(s.running.len() <= MAX_NAMED);
    }

    #[test]
    fn pressure_decides_the_warning() {
        let quiet = InterferenceSample {
            cpu_some_avg10: Some(0.0),
            memory_some_avg10: Some(0.0),
            io_some_avg10: Some(0.0),
            ..Default::default()
        };
        assert!(
            pressure_warning(&quiet, &quiet).is_none(),
            "an idle machine must not be warned about"
        );

        // A neighbour arriving after the start sample is still caught.
        let busy_late = InterferenceSample {
            cpu_some_avg10: Some(31.0),
            ..Default::default()
        };
        let w = pressure_warning(&quiet, &busy_late).expect("cpu pressure warns");
        assert_eq!(w.code, "interference");
        assert!(w.message.contains("cpu 31.0%"), "{}", w.message);

        // Memory pressure warns at a much lower number, because a run that
        // touches fifty gigabytes is the one this tool is usually used for.
        let mem = InterferenceSample {
            memory_some_avg10: Some(1.4),
            ..Default::default()
        };
        assert!(pressure_warning(&quiet, &mem)
            .expect("memory warns")
            .message
            .contains("memory 1.4%"));

        // No PSI is not an idle machine: three nulls must not read as a verdict
        // in either direction.
        let blind = InterferenceSample::default();
        assert!(
            pressure_warning(&blind, &blind).is_none(),
            "absence of data must not warn"
        );
        assert!(blind.cpu_some_avg10.is_none());
    }

    #[test]
    fn worst_ignores_nothing_it_should_not() {
        assert_eq!(worst(Some(1.0), Some(4.0)), Some(4.0));
        assert_eq!(worst(Some(4.0), None), Some(4.0));
        assert_eq!(worst(None, None), None);
    }
}
