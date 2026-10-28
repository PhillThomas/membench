//! Trial statistics: best, median, mean, stddev and MAD-based outlier
//! rejection.
//!
//! Convention (also in the JSON): **`best` is the raw extremum** — that is the
//! AIDA-style peak the grid shows. `median`, `mean`, `stddev` and `spread_pct`
//! are computed *after* outlier rejection so the detail view is not skewed by
//! one noisy trial. Rejection never changes `best`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Direction {
    /// Bandwidth: the maximum is the peak.
    HigherIsBetter,
    /// Latency: the minimum is the peak.
    LowerIsBetter,
}

impl Direction {
    pub fn best_of(self, a: f64, b: f64) -> f64 {
        match self {
            Direction::HigherIsBetter if a > b => a,
            Direction::LowerIsBetter if a < b => a,
            _ => b,
        }
    }

    pub fn best(self, samples: &[f64]) -> f64 {
        samples
            .iter()
            .copied()
            .reduce(|a, b| self.best_of(a, b))
            .unwrap_or(f64::NAN)
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Stats {
    /// Every trial, in run order.
    pub samples: Vec<f64>,
    /// Samples retained by the outlier filter.
    pub retained: usize,
    pub rejected: usize,
    pub best: f64,
    pub median: f64,
    pub mean: f64,
    /// Sample standard deviation of the retained set.
    pub stddev: f64,
    /// `(max - min) / median` of the retained set, as a percentage.
    pub spread_pct: f64,
    /// Median absolute deviation of the retained set.
    pub mad: f64,
}

impl Stats {
    /// Rejection threshold: `|x - median| > 3 * max(1.4826 * MAD, 0.5% of median)`.
    ///
    /// The 1.4826 factor makes MAD a consistent estimator of the standard
    /// deviation for normal data; the 0.5% floor keeps a set of near-identical
    /// trials from rejecting a legitimate 1% difference (MAD is often exactly
    /// zero for 7 tight bandwidth trials).
    pub const REJECT_SIGMAS: f64 = 3.0;
    pub const RELATIVE_FLOOR: f64 = 0.005;

    pub fn new(samples: &[f64], direction: Direction) -> Stats {
        debug_assert!(!samples.is_empty(), "stats need at least one sample");
        let best = direction.best(samples);

        let med = median(samples);
        let mad = {
            let devs: Vec<f64> = samples.iter().map(|x| (x - med).abs()).collect();
            median(&devs)
        };
        let scale = (1.4826 * mad).max(Self::RELATIVE_FLOOR * med.abs());
        let cutoff = Self::REJECT_SIGMAS * scale;
        let retained: Vec<f64> = samples
            .iter()
            .copied()
            .filter(|x| (x - med).abs() <= cutoff)
            .collect();
        // A pathological input (all samples identical, MAD 0, floor 0) must
        // never reject everything.
        let retained = if retained.is_empty() {
            samples.to_vec()
        } else {
            retained
        };

        let n = retained.len() as f64;
        let mean = retained.iter().sum::<f64>() / n;
        let variance = if retained.len() > 1 {
            retained.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / (n - 1.0)
        } else {
            0.0
        };
        let rmin = retained.iter().copied().fold(f64::INFINITY, f64::min);
        let rmax = retained.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let spread_pct = if med.abs() > f64::EPSILON {
            (rmax - rmin) / med.abs() * 100.0
        } else {
            0.0
        };

        Stats {
            samples: samples.to_vec(),
            retained: retained.len(),
            rejected: samples.len() - retained.len(),
            best,
            median: median(&retained),
            mean,
            stddev: variance.sqrt(),
            spread_pct,
            mad,
        }
    }
}

/// Median of a copy of `xs`. Even-length input averages the two middle values.
pub fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(xs: &[f64], d: Direction) -> Stats {
        Stats::new(xs, d)
    }

    #[test]
    fn best_is_the_extremum_in_both_directions() {
        assert_eq!(s(&[10.0, 12.0, 9.0], Direction::HigherIsBetter).best, 12.0);
        assert_eq!(s(&[10.0, 12.0, 9.0], Direction::LowerIsBetter).best, 9.0);
    }

    #[test]
    fn median_odd_and_even() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    #[test]
    fn rejects_a_solitary_spike_from_the_summary() {
        // PLAN.md validation: known inputs. 100 is 50x off the median.
        let st = s(&[10.0, 10.4, 9.8, 10.2, 100.0], Direction::HigherIsBetter);
        assert_eq!(st.rejected, 1);
        assert_eq!(st.best, 100.0, "best must stay the raw extremum");
        // Median of the retained set [10.0, 10.4, 9.8, 10.2].
        assert!((st.median - 10.1).abs() < 1e-9, "median {}", st.median);
        assert!(st.stddev < 1.0, "stddev {}", st.stddev);
    }

    #[test]
    fn keeps_tight_trials() {
        let st = s(
            &[70.0, 70.1, 69.9, 70.05, 70.2, 69.95, 70.15],
            Direction::HigherIsBetter,
        );
        assert_eq!(st.rejected, 0);
        assert!(st.spread_pct < 3.0, "spread {}", st.spread_pct);
    }

    #[test]
    fn identical_samples_reject_nothing() {
        let st = s(&[5.0; 7], Direction::HigherIsBetter);
        assert_eq!(st.rejected, 0);
        assert_eq!(st.stddev, 0.0);
        assert_eq!(st.spread_pct, 0.0);
    }

    #[test]
    fn spread_percent_matches_definition() {
        let st = s(&[100.0, 102.0, 98.0], Direction::HigherIsBetter);
        // median 100, min 98, max 102 -> 4%
        assert!((st.spread_pct - 4.0).abs() < 1e-9, "{}", st.spread_pct);
    }
}
