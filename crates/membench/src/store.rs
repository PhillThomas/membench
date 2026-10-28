//! JSON documents on disk: `--json`, the TUI's `e`xport, and `compare`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use membench_core::result::{Column, Level, RunResults};

/// Where exported runs live: `$XDG_DATA_HOME/membench`, else
/// `~/.local/share/membench` (PLAN.md: "`e` writes JSON to
/// `~/.local/share/membench/` with a timestamp").
pub fn default_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("membench");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".local/share/membench")
}

/// Write the document and return the path used.
pub fn save(results: &RunResults, path: Option<&Path>) -> Result<PathBuf> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None => {
            let dir = default_dir();
            fs::create_dir_all(&dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
            dir.join(format!("{}.json", membench_core::time::now_file_stamp()))
        }
    };
    let text = results
        .to_json_pretty()
        .context("could not serialise the result document")?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
    }
    fs::write(&path, text).with_context(|| format!("could not write {}", path.display()))?;
    Ok(path)
}

pub fn load(path: &Path) -> Result<RunResults> {
    let text =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let doc: RunResults = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a membench result document", path.display()))?;
    if doc.schema_version != membench_core::SCHEMA_VERSION {
        bail!(
            "{} was written by schema version {}, this build reads version {}",
            path.display(),
            doc.schema_version,
            membench_core::SCHEMA_VERSION
        );
    }
    Ok(doc)
}

/// One row of the comparison table.
#[derive(Debug, Clone, PartialEq)]
pub struct Diff {
    pub level: Level,
    pub column: Column,
    pub a: Option<f64>,
    pub b: Option<f64>,
    pub unit: String,
}

impl Diff {
    /// Percentage change of `b` against `a`; `None` when there is nothing to
    /// divide by, which the table prints as a dash rather than as zero.
    pub fn change_pct(&self) -> Option<f64> {
        match (self.a, self.b) {
            (Some(a), Some(b)) if a.abs() > f64::EPSILON => Some((b - a) / a * 100.0),
            _ => None,
        }
    }
}

/// Grid cells present in either document, in grid order.
pub fn diff(a: &RunResults, b: &RunResults) -> Vec<Diff> {
    let mut out = Vec::new();
    let mut levels: Vec<Level> = Level::ALL.into();
    levels.retain(|l| {
        a.grid_row_levels().contains(l)
            || b.grid_row_levels().contains(l)
            || a.cells.iter().any(|c| c.level == *l)
            || b.cells.iter().any(|c| c.level == *l)
    });
    for level in levels {
        for column in [Column::Read, Column::Write, Column::Copy, Column::Latency] {
            let ca = a.grid_cell(level, column);
            let cb = b.grid_cell(level, column);
            if ca.is_none() && cb.is_none() {
                continue;
            }
            let unit = ca
                .or(cb)
                .map(|c| c.unit.clone())
                .unwrap_or_else(|| String::from("?"));
            out.push(Diff {
                level,
                column,
                a: ca.map(|c| c.value),
                b: cb.map(|c| c.value),
                unit,
            });
        }
    }
    out
}

/// Settings that change the meaning of a number. Compared before the numbers
/// themselves, because a comparison between two differently-configured runs is
/// the kind of misleading figure this project exists to avoid.
pub fn configuration_diff(a: &RunResults, b: &RunResults) -> Vec<String> {
    let mut out = Vec::new();
    let sa = &a.settings;
    let sb = &b.settings;
    if sa.threads != sb.threads {
        out.push(format!(
            "threads: {} vs {}",
            sa.threads
                .map_or_else(|| "auto".to_string(), |n| n.to_string()),
            sb.threads
                .map_or_else(|| "auto".to_string(), |n| n.to_string())
        ));
    }
    if sa.isa != sb.isa {
        out.push(format!(
            "isa: {} vs {}",
            sa.isa.map_or("auto".to_string(), |i| i.to_string()),
            sb.isa.map_or("auto".to_string(), |i| i.to_string())
        ));
    }
    if sa.pages != sb.pages {
        out.push(format!("pages: {} vs {}", sa.pages, sb.pages));
    }
    if sa.latency_cpu != sb.latency_cpu {
        out.push(format!(
            "latency cpu: {} vs {}",
            sa.latency_cpu.map_or("auto".to_string(), |c| c.to_string()),
            sb.latency_cpu.map_or("auto".to_string(), |c| c.to_string())
        ));
    }
    if sa.duration_ms != sb.duration_ms {
        out.push(format!(
            "duration per trial: {} ms vs {} ms",
            sa.duration_ms, sb.duration_ms
        ));
    }
    if sa.trials != sb.trials {
        out.push(format!("trials: {} vs {}", sa.trials, sb.trials));
    }
    if sa.copy_counts_both != sb.copy_counts_both {
        out.push("copy byte convention differs".to_string());
    }
    if a.system.cpu.model != b.system.cpu.model {
        out.push(format!(
            "cpu: {} vs {}",
            a.system
                .cpu
                .model
                .clone()
                .unwrap_or_else(|| "Unknown".to_string()),
            b.system
                .cpu
                .model
                .clone()
                .unwrap_or_else(|| "Unknown".to_string())
        ));
    }
    if a.system.memory.speed_mts != b.system.memory.speed_mts {
        out.push(format!(
            "memory speed: {} vs {}",
            a.system
                .memory
                .speed_mts
                .map_or_else(|| "Unknown".to_string(), |s| format!("{s} MT/s")),
            b.system
                .memory
                .speed_mts
                .map_or_else(|| "Unknown".to_string(), |s| format!("{s} MT/s"))
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use membench_core::result::{CellResult, Settings, SystemInfo};

    fn doc(cells: &[(Level, Column, f64)]) -> RunResults {
        let mut d = RunResults::new(SystemInfo::default(), Settings::default());
        for (level, column, value) in cells {
            let op = match column {
                Column::Latency => membench_core::result::Op::Latency,
                Column::Read => membench_core::result::Op::Read,
                // WriteNT is the memory headline, so use it for the Write column.
                Column::Write => membench_core::result::Op::WriteNt,
                Column::Copy => membench_core::result::Op::CopyNt,
            };
            let mut c = CellResult::new(*level, op, *value, op.unit().to_string());
            c.headline = true;
            d.cells.push(c);
        }
        d
    }

    #[test]
    fn json_round_trips_through_a_string() {
        // PLAN.md M7: "round-trip test serialises and parses a full result".
        let original = doc(&[(Level::Memory, Column::Read, 41.5)]);
        let text = original.to_json_pretty().expect("serialise");
        let back: RunResults = serde_json::from_str(&text).expect("parse");
        assert_eq!(original, back);
        assert_eq!(back.schema_version, membench_core::SCHEMA_VERSION);
    }

    #[test]
    fn differences_are_computed_against_the_first_document() {
        let a = doc(&[(Level::Memory, Column::Read, 50.0)]);
        let b = doc(&[(Level::Memory, Column::Read, 55.0)]);
        let d = diff(&a, &b);
        assert_eq!(d.len(), 1);
        assert!(
            (d[0].change_pct().unwrap() - 10.0).abs() < 0.001,
            "{:?}",
            d[0]
        );
    }

    #[test]
    fn a_cell_present_in_only_one_run_is_not_zero() {
        let a = doc(&[(Level::Memory, Column::Read, 50.0)]);
        let b = RunResults::new(a.system.clone(), Settings::default());
        let d = diff(&a, &b);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].a, Some(50.0));
        assert_eq!(d[0].b, None);
        assert!(d[0].change_pct().is_none());
    }

    #[test]
    fn mismatched_settings_are_reported() {
        let a = doc(&[]);
        let s = Settings {
            threads: Some(4),
            pages: PageKind::FourK,
            ..Default::default()
        };
        let b = RunResults::new(a.system.clone(), s);
        let d = configuration_diff(&a, &b);
        assert!(d.iter().any(|x| x.contains("threads")), "{d:?}");
        assert!(d.iter().any(|x| x.contains("pages")), "{d:?}");
    }

    use membench_core::result::PageKind;
}
