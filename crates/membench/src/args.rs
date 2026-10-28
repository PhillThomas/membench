//! Command-line surface (PLAN.md "CLI").
//!
//! The shape follows the plan: `membench` with no subcommand opens the TUI, and
//! every measurement command shares one set of flags so a number can always be
//! reproduced with the same words.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use membench_core::result::{Isa, Level, Op, PageKind, Settings, WriteHeadline};

#[derive(Parser, Debug)]
#[command(
    name = "membench",
    version,
    about = "Cache and memory benchmark for Linux, in the shape of AIDA64's Cache & Memory Benchmark"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Open the terminal UI (the default when no subcommand is given).
    Tui(Common),
    /// Print the system panel and exit.
    Info,
    /// Run the benchmark and print the grid.
    Run(RunArgs),
    /// Latency sweep, CSV on stdout.
    Sweep(SweepArgs),
    /// Compare two exported runs side by side with the percentage change.
    Compare {
        /// First run (the baseline).
        a: PathBuf,
        /// Second run.
        b: PathBuf,
        /// Also write the comparison as JSON.
        #[arg(long, value_name = "PATH")]
        json: Option<PathBuf>,
    },
    /// Check the kernels, ISA dispatch and detection on this machine without
    /// timing anything. Works in a debug build on purpose.
    SelfTest,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[command(flatten)]
    pub common: Common,
    /// Write the whole result document here.
    #[arg(long, value_name = "PATH")]
    pub json: Option<PathBuf>,
    /// Print the plain table instead of opening the TUI.
    #[arg(long)]
    pub no_tui: bool,
    /// Also print the per-cell statistics table.
    #[arg(long, short = 'v')]
    pub detail: bool,
}

#[derive(Args, Debug)]
pub struct SweepArgs {
    #[command(flatten)]
    pub common: Common,
    /// Write the sweep (and the system info) as a JSON document too.
    #[arg(long, value_name = "PATH")]
    pub json: Option<PathBuf>,
}

/// Flags shared by everything that measures.
#[derive(Args, Debug, Clone)]
pub struct Common {
    /// Worker threads. Default: one per physical core for the cache levels, all
    /// logical CPUs for Memory.
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,
    /// Force one instruction-set path instead of the best one available.
    #[arg(long, value_name = "ISA")]
    pub isa: Option<String>,
    /// Page size to request for the buffers.
    #[arg(long, value_name = "PAGES")]
    pub pages: Option<String>,
    /// Timed trials per cell.
    #[arg(long, value_name = "N")]
    pub trials: Option<usize>,
    /// Target measurement time per trial, in milliseconds.
    #[arg(long, short = 'd', alias = "duration", value_name = "MS")]
    pub duration_ms: Option<u64>,
    /// Which operations to run, comma separated: read,write,writent,copy,copynt,latency.
    #[arg(long, short = 'o', value_name = "OPS")]
    pub only: Option<String>,
    /// Which rows to run: l1,l2,l3,mem.
    #[arg(long, short = 'l', value_name = "LEVELS")]
    pub level: Option<String>,
    /// Count both the read and the write of a copy (STREAM convention).
    #[arg(long)]
    pub copy_counts_both: bool,
    /// Which write the grid shows for Memory: nt (default) or normal.
    #[arg(long, value_name = "WHICH")]
    pub write_hint: Option<String>,
    /// Seed for the pointer-chase shuffle (default 0x5eed, so runs are repeatable).
    #[arg(long, value_name = "N")]
    pub seed: Option<u64>,
    /// Show what would run and what it would be measured on, then exit.
    #[arg(long)]
    pub dry_run: bool,
}

impl Common {
    /// Turn the parsed flags into library settings. Errors are strings because
    /// clap has already decided these are the user's own words.
    pub fn settings(&self) -> Result<Settings, String> {
        let mut s = Settings::default();
        if let Some(n) = self.threads {
            if n == 0 {
                return Err("--threads must be at least 1".to_string());
            }
            s.threads = Some(n);
        }
        if let Some(raw) = &self.isa {
            s.isa = Some(raw.parse::<Isa>().map_err(|e| format!("--isa: {e}"))?);
        }
        if let Some(raw) = &self.pages {
            s.pages = raw
                .parse::<PageKind>()
                .map_err(|e| format!("--pages: {e}"))?;
        }
        if let Some(n) = self.trials {
            if n == 0 {
                return Err("--trials must be at least 1".to_string());
            }
            s.trials = n;
        }
        if let Some(ms) = self.duration_ms {
            if ms == 0 {
                return Err("--duration-ms must be at least 1".to_string());
            }
            s.duration_ms = ms;
        }
        if let Some(raw) = &self.only {
            s.ops = Op::parse_list(raw)?;
        }
        if let Some(raw) = &self.level {
            s.levels = parse_levels(raw)?;
        }
        s.copy_counts_both = self.copy_counts_both;
        if let Some(raw) = &self.write_hint {
            s.write_headline = raw
                .parse::<WriteHeadline>()
                .map_err(|e| format!("--write-hint: {e}"))?;
        }
        if let Some(n) = self.seed {
            s.seed = n;
        }
        Ok(s)
    }
}

fn parse_levels(raw: &str) -> Result<Vec<Level>, String> {
    let mut out = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let level: Level = part
            .parse()
            .map_err(|e: <Level as std::str::FromStr>::Err| format!("--level: {e}"))?;
        if !out.contains(&level) {
            out.push(level);
        }
    }
    // Keep the grid order whatever the user typed.
    out.sort_by_key(|l| Level::ALL.iter().position(|x| x == l).unwrap_or(9));
    if out.is_empty() {
        return Err("--level needs at least one of l1, l2, l3, mem".to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_map_onto_settings() {
        let c = Common {
            threads: Some(4),
            isa: Some("avx2".into()),
            pages: Some("4k".into()),
            trials: Some(3),
            duration_ms: Some(50),
            only: Some("read,latency".into()),
            level: Some("mem,l3".into()),
            copy_counts_both: true,
            write_hint: Some("normal".into()),
            seed: Some(7),
            dry_run: false,
        };
        let s = c.settings().expect("valid");
        assert_eq!(s.threads, Some(4));
        assert_eq!(s.isa, Some(Isa::Avx2));
        assert_eq!(s.pages, PageKind::FourK);
        assert_eq!(s.trials, 3);
        assert_eq!(s.duration_ms, 50);
        assert_eq!(s.ops, vec![Op::Read, Op::Latency]);
        // Levels come back in grid order, not the order typed.
        assert_eq!(s.levels, vec![Level::L3, Level::Memory]);
        assert!(s.copy_counts_both);
        assert_eq!(s.write_headline, WriteHeadline::Normal);
        assert_eq!(s.seed, 7);
    }

    #[test]
    fn nonsense_is_refused_with_a_useful_message() {
        let c = Common {
            isa: Some("mmx".into()),
            ..default_common()
        };
        let e = c.settings().unwrap_err();
        assert!(e.contains("--isa"), "{e}");

        let c = Common {
            level: Some("gpu".into()),
            ..default_common()
        };
        assert!(c.settings().unwrap_err().contains("--level"));

        let c = Common {
            threads: Some(0),
            ..default_common()
        };
        assert!(c.settings().unwrap_err().contains("at least 1"));
    }

    fn default_common() -> Common {
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

    #[test]
    fn cli_parses_the_documented_invocations() {
        // Every line in the plan's CLI table has to parse.
        for line in [
            "membench",
            "membench run --no-tui",
            "membench run --only read,latency --level mem,l3",
            "membench run --json out.json",
            "membench sweep",
            "membench info",
            "membench compare a.json b.json",
            "membench run --threads 4 --isa avx2 --pages 2m --trials 9 --duration-ms 250",
        ] {
            let parsed = Cli::try_parse_from(line.split_whitespace());
            assert!(parsed.is_ok(), "{line}: {}", parsed.unwrap_err());
        }
    }
}
