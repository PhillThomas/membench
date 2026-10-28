//! Wall-clock timing and the clock measurements that convert ns ↔ cycles.
//!
//! PLAN.md rule 5: never trust `cpufreq` for the frequency during a run. The
//! effective core clock is measured with a **dependent integer add chain**
//! (one `add rax, 1` per cycle, so chain throughput is exactly one per clock)
//! on the same core that is about to be measured, immediately before the run.
//! The TSC rate is calibrated alongside it as a cross-check: on AMD/Virt the
//! TSC ticks at the *nominal* base rate, so a large gap between the two is
//! itself information (boost active, or a VM with a synthesised TSC).

use std::time::{Duration, Instant};

use crate::affinity;
use crate::kernels;
use crate::result::ClockMeasurement;
use crate::Result;

/// A start/stop pair. Kept explicit (rather than measuring a `Duration` inside
/// the timed region) so that bandwidth can use "earliest start → latest end"
/// across threads, as PLAN.md specifies.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub start: Instant,
    pub end: Instant,
}

impl Span {
    pub fn started() -> Span {
        Span {
            start: Instant::now(),
            end: Instant::now(),
        }
    }

    pub fn stop(&mut self) {
        self.end = Instant::now();
    }

    pub fn nanos(&self) -> u128 {
        self.end.saturating_duration_since(self.start).as_nanos()
    }

    pub fn micros(&self) -> u128 {
        self.nanos() / 1_000
    }

    pub fn as_secs_f64(&self) -> f64 {
        self.end.saturating_duration_since(self.start).as_secs_f64()
    }
}

/// Number of iterations to run a calibration pass for.
const CALIBRATION_CHAIN: u64 = 1 << 20;

/// Measure the effective clock of the current core.
///
/// `target` is how long to spend on the real measurement (the calibration pass
/// is short and untimed).
pub fn measure_core_clock(target: Duration) -> Result<ClockMeasurement> {
    // 1. How long does one add take? (untimed calibration pass)
    let cal_start = Instant::now();
    let sunk = unsafe { kernels::add_chain(CALIBRATION_CHAIN) };
    let cal = cal_start.elapsed();
    std::hint::black_box(sunk);
    if cal.is_zero() {
        return Err(crate::Error::Unsupported("clock measurement timer resolution"));
    }
    let ns_per_add = cal.as_nanos() as f64 / CALIBRATION_CHAIN as f64;

    // 2. How many adds fill `target`?
    let want = (target.as_nanos() as f64 / ns_per_add.max(0.001)) as u64;
    let iterations = want.max(1 << 14) & !15; // multiple of 16 for the unrolled chain

    let start = Instant::now();
    let sunk = unsafe { kernels::add_chain(iterations) };
    let elapsed = start.elapsed();
    std::hint::black_box(sunk);
    if elapsed.is_zero() {
        return Err(crate::Error::Unsupported("clock measurement timer resolution"));
    }

    let hz = iterations as f64 / elapsed.as_secs_f64();
    let cpu = affinity::current_cpu();
    Ok(ClockMeasurement {
        hz,
        mhz: (hz / 1e6).round(),
        cpu: cpu.unwrap_or(usize::MAX),
        method: "dependent integer add chain (1 add/cycle), same core, during the run".to_string(),
    })
}

/// TSC rate, as a cross-check on [`measure_core_clock`]. `None` where the TSC
/// is unavailable or unusable.
pub fn measure_tsc_hz(target: Duration) -> Option<f64> {
    if !kernels::tsc_available() {
        return None;
    }
    let start_ticks = unsafe { kernels::rdtsc() };
    std::thread::sleep(target.max(Duration::from_millis(2)));
    let end_ticks = unsafe { kernels::rdtsc() };
    let dt = target.max(Duration::from_millis(2)).as_secs_f64();
    if dt <= 0.0 || end_ticks <= start_ticks {
        return None;
    }
    Some((end_ticks - start_ticks) as f64 / dt)
}

/// ns → cycles using a measured core clock.
pub fn ns_to_cycles(ns: f64, hz: f64) -> f64 {
    ns * hz / 1e9
}

/// cycles → ns.
pub fn cycles_to_ns(cycles: f64, hz: f64) -> f64 {
    cycles * 1e9 / hz
}

/// Human "is this clock plausible" check (PLAN.md: never report a number that
/// cannot be true without saying so).
pub fn clock_sanity_note(clock: &ClockMeasurement) -> Option<String> {
    if clock.hz < 100e6 || clock.hz > 12e9 {
        Some(format!(
            "measured core clock {:.0} MHz looks implausible; cycle figures are unreliable",
            clock.mhz
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_clock_is_in_a_physical_range() {
        let c = measure_core_clock(Duration::from_millis(5)).expect("clock");
        assert!(c.hz > 1e8 && c.hz < 12e9, "{} Hz", c.hz);
        assert!(c.mhz > 100.0);
    }

    #[test]
    fn cycle_conversion_round_trips() {
        let hz = 3.7e9;
        let ns = 12.5;
        assert!((cycles_to_ns(ns_to_cycles(ns, hz), hz) - ns).abs() < 1e-9);
    }
}
