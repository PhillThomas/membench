//! Plain-text output: the grid, the per-cell detail table, the system panel,
//! the sweep, and the comparison. Everything the TUI shows is here first, in a
//! form that is greppable and diffable.
//!
//! Two rules from PLAN.md govern this file: units are printed next to every
//! number (rule 11), and anything that could not be read is printed as
//! `Unknown` rather than guessed.

use std::fmt::Write as _;

use membench_core::result::{CellResult, Column, RunResults, Settings, Sweep, SystemInfo, Warning};
use membench_core::sysinfo::human_bytes;

/// One grid figure, formatted with its unit.
pub fn fmt_value(value: f64, unit: &str) -> String {
    // Cache bandwidth passes 1000 GB/s; AIDA-style output would be unreadable
    // as "3072.4 GB/s" in a four-column grid, so the scale changes and the
    // label says so. SI throughout: 1 TB/s = 1000 GB/s.
    match unit {
        "GB/s" if value >= 1000.0 => format!("{:.2} TB/s", value / 1000.0),
        "GB/s" if value >= 10.0 => format!("{:.1} {unit}", value),
        "GB/s" => format!("{:.2} {unit}", value),
        // Sub-10 ns is L1/L2 territory, where the second decimal is the whole
        // story; above that it is noise from the clock and the page walks.
        "ns" if value >= 10.0 => format!("{:.1} {unit}", value),
        "ns" => format!("{:.2} {unit}", value),
        _ => format!("{value:.2} {unit}"),
    }
}

fn dash() -> String {
    // An em dash, not a zero: a cell that did not run has no number.
    "—".to_string()
}

/// The AIDA-shaped grid. Rows in grid order, one figure per column.
pub fn grid(results: &RunResults) -> String {
    let columns = [Column::Read, Column::Write, Column::Copy, Column::Latency];
    let levels = results.grid_row_levels();
    let mut out = String::new();

    // Two passes: the first measures the widest figure so the columns line up.
    let mut cells: Vec<Vec<String>> = Vec::new();
    for level in &levels {
        let mut row = Vec::new();
        for column in columns {
            match results.grid_cell(*level, column) {
                Some(c) => row.push(fmt_value(c.value, &c.unit)),
                None => row.push(dash().to_string()),
            }
        }
        cells.push(row);
    }

    let row_label_width = levels
        .iter()
        .map(|l| l.grid_label().len())
        .max()
        .unwrap_or(6)
        .max(columns[0].to_string().len());
    let mut widths = [0usize; 4];
    for (i, column) in columns.iter().enumerate() {
        widths[i] = column
            .to_string()
            .len()
            .max(cells.iter().map(|r| r[i].len()).max().unwrap_or(0));
    }

    let mut header = format!("{:>width$}", "", width = row_label_width + 2);
    for (i, column) in columns.iter().enumerate() {
        let _ = write!(
            header,
            "  {:>width$}",
            column.to_string(),
            width = widths[i]
        );
    }
    out.push_str(&header);
    out.push('\n');

    for (level, row) in levels.iter().zip(cells.iter()) {
        let _ = write!(
            out,
            "{label:<label_width$}  ",
            label = level.grid_label(),
            label_width = row_label_width
        );
        for (i, cell) in row.iter().enumerate() {
            let is_latency = column_is_latency(columns[i]);
            let _ = write!(out, "  {:>w$}", cell, w = widths[i]);
            // Only the latency column deserves colour on a slow cell; bandwidth
            // has no absolute meaning without a theoretical peak to compare to.
            let _ = is_latency;
        }
        out.push('\n');
    }

    if results.cells.is_empty() {
        out.push_str("\n(no cells measured)\n");
    }
    out
}

fn column_is_latency(c: Column) -> bool {
    matches!(c, Column::Latency)
}

/// Best/median/stddev/spread per cell, plus everything that explains the number:
/// threads, buffer size, ISA path, page size, notes (PLAN.md: the `d`etail view).
pub fn details(results: &RunResults) -> String {
    let mut out = String::new();
    out.push_str(
        "Cell                    Value       Best      Median    Stddev   Spread  Trials  Threads  Buffer/thread  Pages    ISA\n",
    );
    for level in results.grid_row_levels() {
        for cell in results
            .cells
            .iter()
            .filter(|c| c.level == level && (c.headline || !results.settings.ops.contains(&c.op)))
        {
            out.push_str(&detail_row(cell));
        }
        // Anything else that ran for this level, so nothing is hidden.
        for cell in results
            .cells
            .iter()
            .filter(|c| c.level == level && !c.headline && results.settings.ops.contains(&c.op))
        {
            out.push_str(&detail_row(cell));
        }
    }

    let mut notes = String::new();
    for cell in &results.cells {
        for note in &cell.notes {
            let _ = writeln!(
                notes,
                "  {} {}: {}",
                cell.level.short(),
                cell.op.label(),
                note
            );
        }
    }
    if !notes.is_empty() {
        out.push_str("\nNotes\n");
        out.push_str(&notes);
    }
    out
}

fn detail_row(c: &CellResult) -> String {
    let spread = if c.trials.len() > 1 {
        format!("{:.1}%", c.spread_pct)
    } else {
        "n/a".to_string()
    };
    let pages = if c.pages_actual == c.pages_requested {
        c.pages_actual.label().to_string()
    } else {
        format!("{}→4K", c.pages_requested.label())
    };
    let buffer = human_bytes(c.buffer_bytes_per_thread as u64);
    let buffer = if c.buffers_per_thread > 1 {
        format!("{buffer}x{}", c.buffers_per_thread)
    } else {
        buffer
    };
    let mut name = format!("{} {}", c.level.short(), c.op.label());
    if !c.headline {
        name.push_str("  ");
    }
    format!(
        "{:<24}{:>11}{:>10}{:>10}{:>9}{:>8}{:>8}{:>9}  {:>14}  {:>8}  {:>5}\n",
        name,
        fmt_value(c.value, &c.unit),
        fmt_value(c.best, &c.unit),
        fmt_value(c.median, &c.unit),
        match c.unit.as_str() {
            "GB/s" => format!("{:.1}", c.stddev),
            _ => format!("{:.2}", c.stddev),
        },
        spread,
        c.trials.len(),
        c.threads,
        buffer,
        pages,
        c.isa.label(),
    )
}

/// The lines under the grid that make the numbers interpretable (rule 11: the
/// convention is printed, not assumed).
pub fn conventions(results: &RunResults) -> String {
    let s = &results.settings;
    let mut out = String::new();
    out.push_str("Conventions\n");
    out.push_str("  bandwidth: GB/s = 10^9 bytes/s (SI, not GiB/s); cache figures often exceed 1000 GB/s and are then shown as TB/s\n");
    out.push_str("  latency: ns per dependent load; cycles use the clock measured during this run, not cpufreq\n");
    out.push_str(if s.copy_counts_both {
        "  copy: read + write counted (STREAM convention, --copy-counts-both)\n"
    } else {
        "  copy: source bytes only (AIDA convention; --copy-counts-both counts both)\n"
    });
    out.push_str(
        "  headline figure: best trial; spread is (max-min)/median of the retained trials\n",
    );
    out.push_str(&format!(
        "  memory write headline: {} stores (--write-hint {} to see the other)\n",
        match s.write_headline {
            membench_core::result::WriteHeadline::Nt => "non-temporal",
            membench_core::result::WriteHeadline::Normal => "normal",
        },
        match s.write_headline {
            membench_core::result::WriteHeadline::Nt => "normal",
            membench_core::result::WriteHeadline::Normal => "nt",
        }
    ));
    if let Some(clock) = &results.clock {
        out.push_str(&format!(
            "  clock: {:.0} MHz measured on cpu {} ({})\n",
            clock.mhz, clock.cpu, clock.method
        ));
    } else {
        out.push_str("  clock: Unknown (no cell measured long enough to time a clock)\n");
    }
    // The ISA and page size actually used, which is not always what was asked
    // for: the runner falls back for the scalar path and the kernel can refuse
    // huge pages.
    if let Some(c) = results.cells.first() {
        out.push_str(&format!(
            "  instruction path: {}{}",
            c.isa.label(),
            if s.isa.is_some() {
                " (forced with --isa)"
            } else {
                ""
            }
        ));
        out.push('\n');
        let requested = c.pages_requested;
        let actual = c.pages_actual;
        if requested == actual {
            let _ = writeln!(out, "  pages: {} (verified per buffer)", requested.label());
        } else {
            let huge = c
                .huge_pages_bytes
                .map(human_bytes)
                .unwrap_or_else(|| "none".to_string());
            let _ = writeln!(
                out,
                "  pages: asked for {}, the kernel backed the buffers with 4K pages ({huge} huge-backed)"
            ,
                requested.label());
        }
    }
    out
}

pub fn warnings(warnings: &[Warning]) -> String {
    let mut out = String::new();
    for w in warnings {
        let _ = writeln!(out, "  [{}] {}", w.code, w.message);
        if let Some(hint) = &w.hint {
            let _ = writeln!(out, "         → {hint}");
        }
    }
    out
}

/// The system panel: what `membench info` prints and what the TUI shows below
/// the grid.
pub fn system_panel(sys: &SystemInfo) -> String {
    let mut out = String::new();
    let cpu = sys
        .cpu
        .model
        .clone()
        .unwrap_or_else(|| "Unknown CPU".to_string());
    let _ = writeln!(
        out,
        "  {:<11}{}",
        "CPU",
        match (&sys.cpu.vendor, &sys.cpu.vendor_flavour) {
            (Some(_), Some(fl)) => format!("{cpu}  ({fl})"),
            _ => cpu.clone(),
        }
    );
    let _ = writeln!(
        out,
        "  {:<11}{} logical / {} physical, SMT {}, {} socket(s)",
        "Cores",
        sys.topology.logical_cpus,
        sys.topology.physical_cores,
        if sys.topology.smt_enabled {
            "on"
        } else {
            "off"
        },
        sys.topology.sockets
    );
    if !sys.cpu.features.is_empty() {
        let _ = writeln!(out, "  {:<11}{}", "Features", sys.cpu.features.join(" "));
    }
    let _ = writeln!(out, "  {:<11}{}", "ISA paths", {
        let paths = sys
            .cpu
            .isa_paths
            .iter()
            .map(|i| i.label())
            .collect::<Vec<_>>()
            .join(" > ");
        if paths.is_empty() {
            "Unknown".to_string()
        } else {
            paths
        }
    });

    let mut caches = Vec::new();
    for level in 1..=3u8 {
        if let Some(c) = sys.caches.data_level(level) {
            let size = c
                .size_bytes
                .map(human_bytes)
                .unwrap_or_else(|| "Unknown".to_string());
            // An L3 shared by several domains is reported per domain, because
            // that is what a buffer of that size actually sees.
            let domains = sys.caches.l3_domains.len();
            if c.level == 3 && domains > 1 {
                caches.push(format!("L3d {size} x {domains}"));
            } else {
                caches.push(format!("L{level}d {size}"));
            }
        }
    }
    if let Some(line) = sys.caches.line_bytes {
        caches.push(format!("{line} B line"));
    }
    let _ = writeln!(
        out,
        "  {:<11}{}",
        "Caches",
        if caches.is_empty() {
            "Unknown".to_string()
        } else {
            caches.join("  ")
        }
    );

    let mem = &sys.memory;
    let total = mem
        .total_bytes
        .map(human_bytes)
        .unwrap_or_else(|| "Unknown".to_string());
    let mut memline = total.clone();
    if let Some(kind) = &mem.mem_type {
        memline.push_str(&format!(
            "  {kind}{}",
            mem.speed_mts.map(|s| format!("-{s}")).unwrap_or_default()
        ));
        if let Some(src) = &mem.mem_type_source {
            memline.push_str(&format!(" ({src})"));
        }
    }
    if let Some(n) = mem.populated_modules {
        memline.push_str(&format!("  {n} module(s)"));
    }
    if let Some(c) = mem.channels {
        memline.push_str(&format!("  {c} channel(s)"));
    } else if let Some(note) = &mem.channels_note {
        memline.push_str(&format!("  {note}"));
    }
    let _ = writeln!(out, "  {:<11}{memline}", "Memory");
    // SPD timings are the module's rated values, never its running values, and
    // the panel says so next to the device that was read.
    for t in &mem.spd {
        // Only the timings count as decoded. The generation string is already
        // on the line above, and echoing it here would make a module whose
        // timing fields were blank look like it had been read successfully.
        let mut timings = Vec::new();
        if let Some(cas) = &t.cas_string {
            timings.push(format!("CL{cas}"));
        }
        for (label, value) in [
            ("tAA", t.tAA_ns),
            ("tRCD", t.tRCD_ns),
            ("tRP", t.tRP_ns),
            ("tRAS", t.tRAS_ns),
        ] {
            if let Some(v) = value {
                timings.push(format!("{label} {v} ns"));
            }
        }
        if timings.is_empty() {
            let _ = writeln!(out, "  {:<11}{}", "", t.note);
        } else {
            let _ = writeln!(
                out,
                "  {:<11}{}  (rated, from SPD on {})",
                "",
                timings.join("  "),
                t.device
            );
        }
    }
    if let Some(note) = &mem.access_note {
        let _ = writeln!(out, "  {:<11}{note}", "");
    }
    let _ = writeln!(
        out,
        "  {:<11}{} transparent hugepages, {} node(s)",
        "Kernel",
        mem.transparent_hugepages
            .clone()
            .unwrap_or_else(|| "Unknown".to_string()),
        mem.numa_node_count
    );
    let _ = writeln!(
        out,
        "  {:<11}{} (membench {}{}, mlock limit {})",
        "OS",
        sys.os
            .kernel_release
            .clone()
            .unwrap_or_else(|| "Unknown kernel".to_string()),
        sys.os.membench_version,
        if sys.os.running_as_root {
            ", uid 0"
        } else {
            ""
        },
        sys.os
            .mlock_limit_bytes
            .map(human_bytes)
            .unwrap_or_else(|| "Unknown".to_string())
    );
    if !sys.warnings.is_empty() {
        let _ = write!(out, "  {:<11}", "Warnings");
        let text = warnings(&sys.warnings);
        // Re-indent the warning block under the label.
        out.push_str(&text.replace("\n         → ", "\n             → "));
    }
    out
}

/// One-line summary of what a run is about to do (used by `--dry-run`).
pub fn plan_summary(settings: &Settings, sys: &SystemInfo) -> String {
    let mut out = String::new();
    let plans = match membench_core::sizes::plan(sys, settings) {
        Ok(p) => p,
        Err(e) => return format!("could not size the buffers: {e}\n"),
    };
    if let Some(cpu) = settings.latency_cpu {
        let _ = writeln!(
            out,
            "  latency cells and the sweep are pinned to cpu {cpu} (--cpu); the sibling\
             core is left alone"
        );
    }
    for p in &plans {
        let _ = writeln!(
            out,
            "  {:<8} {:>3} thread(s), {:>10} per thread{}  {}",
            p.level.short(),
            p.threads,
            human_bytes(p.bytes_per_thread as u64),
            if p.buffers_per_thread > 1 {
                format!(" x{}", p.buffers_per_thread)
            } else {
                String::new()
            },
            p.derivation
        );
        if let Some(note) = &p.note {
            let _ = writeln!(out, "           → {note}");
        }
    }
    let ops: Vec<String> = settings
        .levels
        .iter()
        .map(|l| {
            format!(
                "{}: {}",
                l.short(),
                settings
                    .ops_for(*l)
                    .iter()
                    .map(|o| o.label())
                    .collect::<Vec<_>>()
                    .join("+")
            )
        })
        .collect();
    let _ = writeln!(out, "  cells: {}", ops.join(", "));
    let _ = writeln!(
        out,
        "  {} trials of ~{} ms each{}",
        settings.trials,
        settings.duration_ms,
        if settings.with_sweep {
            ", plus a latency sweep"
        } else {
            ""
        }
    );
    out
}

/// Latency sweep as a table (the TUI draws the same points as a chart).
pub fn sweep_table(sweep: &Sweep) -> String {
    let mut out = String::new();
    out.push_str("  Block      Nodes      ns      Cycles   Spread  Pages\n");
    for p in &sweep.points {
        out.push_str(&sweep_row(p));
    }
    for n in &sweep.notes {
        let _ = writeln!(out, "  note: {n}");
    }
    out
}

pub fn sweep_row(p: &membench_core::result::SweepPoint) -> String {
    format!(
        "  {:<10}{:>7}{:>9}{:>9}{:>8}  {}\n",
        human_bytes(p.size_bytes as u64),
        p.nodes,
        format!("{:.2}", p.ns),
        p.cycles
            .map(|c| format!("{c:.1}"))
            .unwrap_or_else(|| "—".to_string()),
        format!("{:.1}%", p.spread_pct),
        p.pages_actual.label(),
    )
}

/// Sweep as CSV, for `membench sweep` and for plotting elsewhere.
pub fn sweep_csv(sweep: &Sweep) -> String {
    let mut out = String::from(
        "size_bytes,nodes,ns,cycles,trials,best_ns,spread_pct,pages_actual,huge_pages_bytes\n",
    );
    for p in &sweep.points {
        let _ = writeln!(
            out,
            "{},{},{:.4},{},{},{:.4},{:.2},{},{}",
            p.size_bytes,
            p.nodes,
            p.ns,
            p.cycles.map(|c| format!("{c:.2}")).unwrap_or_default(),
            p.trials,
            p.best_ns,
            p.spread_pct,
            p.pages_actual,
            p.huge_pages_bytes
                .map(|h| h.to_string())
                .unwrap_or_default(),
        );
    }
    out
}

/// Side-by-side comparison with the percentage change (`membench compare`).
pub fn compare_table(rows: &[crate::store::Diff]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "  {:<16}{:>16}{:>16}{:>10}",
        "Cell", "A", "B", "Change"
    );
    for d in rows {
        let a = d.a.map(|v| fmt_value(v, &d.unit)).unwrap_or_else(dash);
        let b = d.b.map(|v| fmt_value(v, &d.unit)).unwrap_or_else(dash);
        let change = d
            .change_pct()
            .map(|p| format!("{p:+.1}%"))
            .unwrap_or_else(|| "—".to_string());
        let _ = writeln!(
            out,
            "  {:<16}{:>16}{:>16}{:>10}",
            format!("{} {}", d.level.short(), d.column),
            a,
            b,
            change
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use membench_core::result::{Level, Op, RunResults, Settings, SystemInfo};

    fn cell(level: Level, op: Op, value: f64) -> CellResult {
        let mut c = CellResult::new(level, op, value, op.unit().to_string());
        c.headline = true;
        c.trials = vec![value, value * 0.98];
        c.threads = 2;
        c.buffer_bytes_per_thread = 1 << 20;
        c.isa = membench_core::result::Isa::Avx2;
        c
    }

    #[test]
    fn bandwidth_switches_to_tb_per_second_above_a_thousand() {
        assert_eq!(fmt_value(78.43, "GB/s"), "78.4 GB/s");
        assert_eq!(fmt_value(3100.0, "GB/s"), "3.10 TB/s");
        assert_eq!(fmt_value(1000.0, "GB/s"), "1.00 TB/s");
        assert_eq!(fmt_value(4.2, "GB/s"), "4.20 GB/s");
        assert_eq!(fmt_value(72.14, "ns"), "72.1 ns");
        assert_eq!(fmt_value(126.0, "ns"), "126.0 ns");
        assert_eq!(fmt_value(0.83, "ns"), "0.83 ns");
    }

    #[test]
    fn a_missing_cell_is_a_dash_not_a_zero() {
        let mut r = RunResults::new(SystemInfo::default(), Settings::default());
        // 48.0, not 50.0: "50.0 GB/s" contains "0.0 GB/s" and the assertion
        // below would pass for the wrong reason.
        r.cells.push(cell(Level::Memory, Op::Read, 48.0));
        let text = grid(&r);
        assert!(text.contains("48.0 GB/s"), "{text}");
        assert!(text.contains('—'), "{text}");
        assert!(!text.contains("0.0 GB/s"), "{text}");
    }

    #[test]
    fn grid_lists_the_levels_that_measured_something() {
        let mut r = RunResults::new(SystemInfo::default(), Settings::default());
        r.cells.push(cell(Level::L1, Op::Read, 3072.0));
        r.cells.push(cell(Level::L1, Op::Latency, 0.82));
        let text = grid(&r);
        assert!(text.contains("L1 Cache"), "{text}");
        assert!(!text.contains("Memory"), "{text}");
        assert!(text.contains("3.07 TB/s"), "{text}");
    }

    #[test]
    fn conventions_state_the_copy_rule_either_way() {
        let mut r = RunResults::new(SystemInfo::default(), Settings::default());
        r.cells.push(cell(Level::Memory, Op::Read, 50.0));
        assert!(conventions(&r).contains("source bytes only"));
        r.settings.copy_counts_both = true;
        assert!(conventions(&r).contains("STREAM convention"));
    }

    #[test]
    fn system_panel_never_leaves_a_blank_label() {
        let text = system_panel(&SystemInfo::default());
        for label in ["CPU", "Cores", "Caches", "Memory", "OS"] {
            assert!(text.contains(label), "{label} missing from:\n{text}");
        }
        // Unknown is spelled out rather than left empty (rule 11).
        assert!(text.contains("Unknown"), "{text}");
    }

    #[test]
    fn sweep_csv_has_a_header_and_one_line_per_point() {
        let sweep = Sweep {
            points: vec![membench_core::result::SweepPoint {
                size_bytes: 4096,
                nodes: 64,
                ns: 4.2,
                cycles: Some(19.0),
                trials: 3,
                best_ns: 4.1,
                spread_pct: 1.5,
                pages_actual: membench_core::result::PageKind::TwoM,
                huge_pages_bytes: Some(1 << 20),
            }],
            notes: vec![],
            duration_ms: 60,
            trials: 3,
        };
        let csv = sweep_csv(&sweep);
        assert!(csv.starts_with("size_bytes,nodes,ns,cycles"));
        // one header + one line per point
        assert_eq!(csv.lines().count(), 2);
        assert!(csv.contains("4096,64,4.2000,19.00,3,4.1000,1.50"));
        assert!(sweep_table(&sweep).contains("4K"));
    }
}
