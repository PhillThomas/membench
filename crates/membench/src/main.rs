//! `membench` — a Cache & Memory Benchmark work-alike for Linux terminals.
//!
//! The binary is deliberately thin: parsing, one thread to measure, and the
//! rendering. All timing lives in `membench-core`, which the UI never reaches
//! into (PLAN.md: "The UI never touches timing code").

// Numeric casts are kept in their explicit form even when they are a no-op on
// x86-64: `libc::c_long`, `size_t` and the `suseconds` fields change width
// between 32-bit ARM, 64-bit x86 and musl, and the kernels crate is meant to
// build on all of them. Removing the cast to satisfy the lint would move the
// width question from the source into the target.
#![allow(clippy::unnecessary_cast)]

mod args;
mod render;
mod store;
mod tui;

use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread;

use anyhow::{bail, Context, Result};
use clap::Parser;
use membench_core::result::{Level, Op, RunResults, Settings, SystemInfo};
use membench_core::runner::{Event, Runner};
use membench_core::sysinfo;

use args::{Cli, Command, Common, RunArgs, SweepArgs};

/// Set by the SIGINT handler so Ctrl-C stops between trials instead of leaving
/// a pinned, mlocked thread behind.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_sig: libc::c_int) {
    // SAFETY: an atomic store is signal-safe; nothing else happens in here.
    INTERRUPTED.store(true, Ordering::SeqCst);
}

fn install_interrupt_flag() {
    // Rust ignores SIGPIPE, so `membench run | head` dies with a panic on the
    // first write after `head` goes away. `cat` does not do that, and neither
    // should a command whose whole output is a pipe candidate.
    //
    // SAFETY: resetting one signal disposition to the default before any
    // threads exist; nothing else reads SIGPIPE in this process.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    // SAFETY: replacing the SIGINT handler with a function that only stores to
    // an atomic is safe in a process like this one. The default action (die
    // this instant) is what we deliberately give up so that buffers unmap and
    // the worker thread stops pinning a core.
    // Bound through a function pointer first: casting the item directly is what
    // the compiler complains about.
    let handler: extern "C" fn(libc::c_int) = on_sigint;
    unsafe {
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    install_interrupt_flag();
    match dispatch(cli.command) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("membench: {e:#}");
            if is_debug_build_error(&e) {
                eprintln!(
                    "  → build an optimised binary: `cargo build --release`, then run \
                     `target/release/membench …`. An unoptimised build compiles the \
                     kernels into function calls and measures nothing (PLAN.md rule 2)."
                );
            }
            ExitCode::from(1)
        }
    }
}

/// The one error the user can fix without reading a manual.
fn is_debug_build_error(e: &anyhow::Error) -> bool {
    e.root_cause()
        .downcast_ref::<membench_core::Error>()
        .is_some_and(|err| matches!(err, membench_core::Error::DebugBuild { .. }))
}

fn dispatch(command: Option<Command>) -> Result<ExitCode> {
    match command.unwrap_or_else(|| Command::Tui(Common::none())) {
        Command::Tui(common) => {
            let settings = common.settings().map_err(bad_flag)?;
            tui::run(&settings).map(|_| ExitCode::SUCCESS)
        }
        Command::Info => {
            let sys = sysinfo::gather();
            println!("membench {}", membench_core::time::version());
            print!("{}", render::system_panel(&sys));
            Ok(ExitCode::SUCCESS)
        }
        Command::Run(RunArgs {
            common,
            json,
            no_tui,
            detail,
        }) => {
            let settings = common.settings().map_err(bad_flag)?;
            if common.dry_run {
                return dry_run(&settings, &sysinfo::gather());
            }
            let results = if !no_tui && std::io::stdout().is_terminal() {
                // The TUI starts the run straight away and hands back whatever
                // it measured; quitting early prints nothing rather than a
                // half-filled grid.
                match tui::run_and_measure(&settings)? {
                    Some(results) => results,
                    None => return Ok(ExitCode::SUCCESS),
                }
            } else {
                measure_headless(settings.clone())?
            };
            print!("{}", render::grid(&results));
            println!();
            if detail {
                print!("{}", render::details(&results));
            }
            print!("{}", render::conventions(&results));
            if !results.warnings.is_empty() {
                println!("Warnings");
                print!("{}", render::warnings(&results.warnings));
            }
            if let Some(path) = json {
                let written = store::save(&results, Some(&path))?;
                println!("wrote {}", written.display());
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Sweep(SweepArgs { common, json }) => {
            let mut settings = common.settings().map_err(bad_flag)?;
            // The sweep is the point: no grid cells, no bandwidth kernels.
            settings.levels = vec![];
            settings.ops = vec![Op::Latency];
            settings.with_sweep = true;
            if common.dry_run {
                return dry_run(&settings, &sysinfo::gather());
            }
            let results = measure_headless(settings)?;
            match &results.sweep {
                Some(sweep) => {
                    // stdout is the machine-readable CSV, so a human at a
                    // terminal also gets the aligned table, on stderr.
                    if std::io::stderr().is_terminal() {
                        eprint!("{}", render::sweep_table(sweep));
                    }
                    print!("{}", render::sweep_csv(sweep));
                    if let Some(path) = json {
                        let written = store::save(&results, Some(&path))?;
                        eprintln!("wrote {}", written.display());
                    }
                    Ok(ExitCode::SUCCESS)
                }
                None => bail!("the sweep produced no points"),
            }
        }
        Command::Compare { a, b, json } => compare(&a, &b, json.as_deref()),
        Command::SelfTest => self_test(),
    }
}

fn bad_flag(e: String) -> anyhow::Error {
    anyhow::anyhow!("{e} — see --help")
}

fn dry_run(settings: &Settings, sys: &SystemInfo) -> Result<ExitCode> {
    println!(
        "membench {} — nothing measured yet",
        membench_core::time::version()
    );
    println!(
        "  {} · {} · {}",
        sys.cpu
            .model
            .clone()
            .unwrap_or_else(|| "Unknown CPU".to_string()),
        sys.cpu.architecture,
        if sys.os.running_as_root {
            "uid 0"
        } else {
            "unprivileged"
        }
    );
    print!("{}", render::plan_summary(settings, sys));
    let runner = Runner::prepare(settings.clone())?;
    let warnings = runner.preflight_warnings();
    if warnings.is_empty() {
        println!("  warnings: none");
    } else {
        println!("  warnings:");
        print!("{}", render::warnings(&warnings));
    }
    Ok(ExitCode::SUCCESS)
}

/// Run everything, printing one line per event. The measuring happens on a
/// worker thread and this thread blocks on the channel, so nothing in the UI
/// runs while a kernel is timed except the line printed once a cell is done
/// (PLAN.md rule 9).
fn measure_headless(settings: Settings) -> Result<RunResults> {
    let runner = Runner::prepare(settings.clone()).context("could not prepare the run")?;
    if !settings.levels.is_empty() {
        eprintln!(
            "membench {} on {} ({})",
            membench_core::time::version(),
            runner
                .sys
                .cpu
                .model
                .clone()
                .unwrap_or_else(|| "Unknown CPU".into()),
            runner.sys.os.kernel_release.clone().unwrap_or_default()
        );
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Event>();
    let worker_cancel = Arc::clone(&cancel);
    let handle = thread::Builder::new()
        .name("membench-worker".to_string())
        .spawn(move || runner.run(&worker_cancel, Some(tx)))
        .context("could not start the measurement thread")?;

    let mut finished: Option<RunResults> = None;
    let mut failure: Option<String> = None;
    while let Ok(event) = rx.recv() {
        match event {
            Event::Finished(results) => {
                finished = Some(*results);
                break;
            }
            Event::Failed(message) => {
                failure = Some(message);
                cancel.store(true, Ordering::SeqCst);
                break;
            }
            other => {
                if INTERRUPTED.load(Ordering::SeqCst) {
                    eprintln!("interrupted: stopping after the current trial");
                    cancel.store(true, Ordering::SeqCst);
                }
                print_progress(&other);
            }
        }
    }
    drop(rx);

    let joined = handle.join();
    if let Some(message) = failure {
        bail!("measurement failed: {message}");
    }
    match joined {
        Err(_) => bail!("the measurement thread panicked; nothing was measured"),
        Ok(Err(e)) => return Err(e).context("measurement failed"),
        Ok(Ok(results)) => {
            if finished.is_none() {
                finished = Some(results);
            }
        }
    }
    finished.context("the run ended without a result document")
}

fn print_progress(event: &Event) {
    match event {
        Event::CellStart {
            level,
            op,
            threads,
            bytes_per_thread,
        } => eprint!(
            "  {} {:<10} ({} thread{}, {}) …",
            level.short(),
            op.label(),
            threads,
            if *threads == 1 { "" } else { "s" },
            sysinfo::human_bytes(*bytes_per_thread as u64),
        ),
        Event::CellDone(cell) => {
            eprint!(
                "\r  {:<7} {:<10} {} ({} trials, spread {:.1}%)\n",
                cell.level.short(),
                cell.op.label(),
                render::fmt_value(cell.value, &cell.unit),
                cell.trials.len(),
                cell.spread_pct,
            );
        }
        Event::SweepStart { size_bytes } => {
            eprint!("  sweep {:>10} …", sysinfo::human_bytes(*size_bytes as u64));
        }
        Event::SweepPoint(point) => {
            eprint!("\r{}", render::sweep_row(point).trim_start());
        }
        Event::Trial { .. } => {}
        Event::Finished(_) | Event::Failed(_) => unreachable!("handled by the caller"),
    }
}

fn compare(a: &Path, b: &Path, json: Option<&Path>) -> Result<ExitCode> {
    let left = store::load(a)?;
    let right = store::load(b)?;
    println!("A  {}", describe(&left));
    println!("B  {}", describe(&right));
    let config = store::configuration_diff(&left, &right);
    if !config.is_empty() {
        println!(
            "\nThe two runs were not configured the same way, so what follows is not a \
             like-for-like comparison:"
        );
        for line in &config {
            println!("  - {line}");
        }
    }
    let rows = store::diff(&left, &right);
    if rows.is_empty() {
        println!("\nNeither document has a grid cell.");
        return Ok(ExitCode::SUCCESS);
    }
    println!();
    print!("{}", render::compare_table(&rows));

    // The plan's stability gate: a spread over 3% inside one run means the
    // numbers are not comparable, whatever the delta between runs says.
    let mut unstable = Vec::new();
    for cell in right.cells.iter().filter(|c| c.trials.len() > 1) {
        if cell.spread_pct > 3.0 {
            unstable.push(format!(
                "{} {} spread {:.1}%",
                cell.level.short(),
                cell.op.label(),
                cell.spread_pct
            ));
        }
    }
    if !unstable.is_empty() {
        println!("\nRun B was not stable enough to compare cell by cell:");
        for u in unstable {
            println!("  - {u}");
        }
    }
    if let Some(path) = json {
        let doc = serde_json::json!({
            "a": left.timestamp,
            "b": right.timestamp,
            "configuration_diff": config.clone(),
            "cells": rows.iter().map(|d| serde_json::json!({
                "level": d.level.to_string(),
                "column": d.column.to_string(),
                "a": d.a,
                "b": d.b,
                "change_pct": d.change_pct(),
            })).collect::<Vec<_>>(),
        });
        std::fs::write(path, serde_json::to_string_pretty(&doc)?)
            .with_context(|| format!("could not write {}", path.display()))?;
        println!("\nwrote {}", path.display());
    }
    Ok(ExitCode::SUCCESS)
}

fn describe(results: &RunResults) -> String {
    format!(
        "{} · {} · isa {} · {} threads · {}",
        results.timestamp,
        results
            .system
            .cpu
            .model
            .clone()
            .unwrap_or_else(|| "Unknown CPU".to_string()),
        results
            .settings
            .isa
            .map(|i| i.label().to_string())
            .unwrap_or_else(|| "auto".to_string()),
        results
            .settings
            .threads
            .map(|n| n.to_string())
            .unwrap_or_else(|| "auto".to_string()),
        results.generator
    )
}

/// `membench self-test`: the checks that need no timing, so they also run in a
/// debug build (rule 2 forbids *timing* in debug, not verifying).
fn self_test() -> Result<ExitCode> {
    use membench_core::kernels::{self, Kernels};
    use membench_core::result::{Isa, PageKind};

    let mut failures = 0usize;
    let mut check = |name: &str, ok: bool, detail: String| {
        println!(
            "  [{}] {name}{}",
            if ok { " ok " } else { "FAIL" },
            if detail.is_empty() {
                String::new()
            } else {
                format!(" — {detail}")
            }
        );
        if !ok {
            failures += 1;
        }
    };

    println!("membench {} self-test", membench_core::time::version());
    let sys = sysinfo::gather();
    check(
        "cpu detection",
        sys.cpu.model.is_some() && sys.topology.logical_cpus > 0,
        format!(
            "{} · {} logical / {} physical",
            sys.cpu.model.clone().unwrap_or_else(|| "Unknown".into()),
            sys.topology.logical_cpus,
            sys.topology.physical_cores
        ),
    );
    check(
        "cache detection",
        sys.caches.data_cache_bytes(Level::L1).is_some()
            && sys.caches.data_cache_bytes(Level::L2).is_some(),
        format!(
            "L1d {} · L2 {} · L3 {} · line {}",
            sys.caches
                .data_cache_bytes(Level::L1)
                .map(sysinfo::human_bytes)
                .unwrap_or_else(|| "Unknown".into()),
            sys.caches
                .data_cache_bytes(Level::L2)
                .map(sysinfo::human_bytes)
                .unwrap_or_else(|| "Unknown".into()),
            sys.caches
                .data_cache_bytes(Level::L3)
                .map(sysinfo::human_bytes)
                .unwrap_or_else(|| "Unknown".into()),
            sys.caches
                .line_bytes
                .map(|b| b.to_string())
                .unwrap_or_else(|| "Unknown".into()),
        ),
    );

    let available = kernels::available();
    check(
        "isa paths available",
        !available.is_empty(),
        available
            .iter()
            .map(|i| i.label())
            .collect::<Vec<_>>()
            .join(" > "),
    );

    // Every ISA path the CPU claims must run, and every path it does not claim
    // must be refused rather than silently substituted (rule 11).
    let mut all_match = true;
    let mut detail = String::new();
    for len in [4096usize, 12 * 1024, 64 * 1024 + 8] {
        let mut buf = match membench_core::alloc::Buffer::new(len, Default::default()) {
            Ok(b) => b,
            Err(e) => {
                all_match = false;
                detail = format!("could not map a {len} byte buffer: {e}");
                break;
            }
        };
        buf.fill_words(0x9e37_79b9_7f4a_7c15 ^ len as u64);
        let expect = buf.checksum_words();
        for isa in &available {
            let k = match Kernels::for_isa(*isa) {
                Ok(k) => k,
                Err(e) => {
                    all_match = false;
                    detail = format!("{isa} advertised but does not load: {e}");
                    continue;
                }
            };
            // SAFETY: live buffer, readable for its whole length, one pass.
            let got = unsafe { k.read(buf.ptr(), buf.len(), 1) };
            if got != expect {
                all_match = false;
                detail = format!("{isa} read gave {got:#018x}, expected {expect:#018x} at {len} B");
            }
        }
    }
    check("kernel checksums agree", all_match, detail);

    if available.contains(&Isa::Avx512) {
        check(
            "forced-but-missing isa is an error",
            true,
            "every advertised path exists here, so nothing to refuse".to_string(),
        );
    } else {
        match Kernels::resolve(Some(Isa::Avx512)) {
            Err(e) => check(
                "forced-but-missing isa is an error",
                true,
                format!("AVX-512 refused: {e}"),
            ),
            Ok(_) => check(
                "forced-but-missing isa is an error",
                false,
                "AVX-512 was accepted but is not in the available list".to_string(),
            ),
        }
    }

    let settings = Settings::default();
    match membench_core::sizes::plan(&sys, &settings) {
        Ok(plans) => check(
            "buffer sizing",
            plans.iter().all(|p| p.bytes_per_thread >= 1024),
            plans
                .iter()
                .map(|p| {
                    format!(
                        "{} {}",
                        p.level.short(),
                        sysinfo::human_bytes(p.bytes_per_thread as u64)
                    )
                })
                .collect::<Vec<_>>()
                .join(" "),
        ),
        Err(e) => check("buffer sizing", false, e.to_string()),
    }

    match membench_core::alloc::Buffer::new(
        8 * 1024 * 1024,
        membench_core::alloc::AllocOpts {
            pages: PageKind::TwoM,
            ..Default::default()
        },
    ) {
        Ok(b) => {
            let s = b.page_status();
            check(
                "huge page verification",
                s.actual == PageKind::TwoM || s.note.is_some(),
                format!(
                    "asked {}, got {}{}",
                    s.requested.label(),
                    s.actual.label(),
                    s.note.map(|n| format!(", {n}")).unwrap_or_default()
                ),
            );
        }
        Err(e) => check("huge page verification", false, e.to_string()),
    }

    check(
        "pinning",
        membench_core::affinity::current_cpu().is_some(),
        format!(
            "this thread is on cpu {:?}",
            membench_core::affinity::current_cpu()
        ),
    );

    let refuses = membench_core::ensure_release_build("self-test").is_err();
    check(
        "debug-build timing guard",
        refuses == cfg!(debug_assertions),
        if cfg!(debug_assertions) {
            "debug build: timing refused, as it should be".to_string()
        } else {
            "optimised build: timing allowed".to_string()
        },
    );

    if failures == 0 {
        println!("all checks passed");
        Ok(ExitCode::SUCCESS)
    } else {
        println!("{failures} check(s) failed");
        Ok(ExitCode::from(1))
    }
}

impl Common {
    /// Every flag left at its default, for the bare `membench` command.
    fn none() -> Common {
        Common {
            threads: None,
            isa: None,
            pages: None,
            trials: None,
            duration_ms: None,
            only: None,
            level: None,
            copy_counts_both: false,
            write_hint: None,
            seed: None,
            dry_run: false,
        }
    }
}

/// Progress plumbing shared with the TUI.
#[allow(dead_code)]
pub(crate) type EventSender = Sender<Event>;
