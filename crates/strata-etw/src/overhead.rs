//! CPU overhead guard and write sampling.
//!
//! The guard is fed the consumer thread's CPU time at a fixed interval and
//! expresses it as a percentage of the machine's total CPU capacity (all
//! logical processors, the scale Task Manager uses), which is what the
//! "CPU cap" setting means to a user.
//!
//! **Strategy.** When every sample in the sustain window (default 6 × 5 s =
//! 30 s) is over the cap, the guard switches to sampling: only every Nth
//! `Write` event is decoded and its size is multiplied by N, starting at
//! N = 4 and multiplying by 4 per further sustained breach up to
//! [`OverheadConfig::max_sample_rate`]. Creates, deletes, renames, name and
//! process events are never sampled, because the file and process maps depend
//! on every one of them and they are a small part of the stream. Byte totals
//! stay unbiased estimates; last-writer times can lag by up to N-1 writes.
//!
//! Entering sampling also raises [`OverheadReport::suggest_disable`], so the
//! UI can offer to turn tracking off. The guard steps back down (N / 4) only
//! when the projected cost at the lower rate (current cost × 4) is under half
//! the cap for a whole window, which keeps it from oscillating; the
//! suggestion clears when it is back to full fidelity.

use std::collections::VecDeque;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Guard settings.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OverheadConfig {
    /// CPU cap in percent of total machine capacity (default 2.0).
    pub cap_percent: f32,
    /// Consecutive samples that must agree before changing rate (default 6).
    pub sustain_samples: usize,
    /// Highest sampling rate, 1 in N writes (default 64).
    pub max_sample_rate: u32,
}

impl Default for OverheadConfig {
    fn default() -> Self {
        Self {
            cap_percent: 2.0,
            sustain_samples: 6,
            max_sample_rate: 64,
        }
    }
}

/// The guard's verdict after a sample.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OverheadReport {
    /// Consumer CPU over the last interval, percent of machine capacity.
    pub cpu_percent: f32,
    /// Writes are sampled 1 in `sample_rate` (1 = every write).
    pub sample_rate: u32,
    /// Tracking costs more than the cap; the UI should suggest disabling it.
    pub suggest_disable: bool,
}

/// The guard state machine (pure; the monitor feeds it measurements).
#[derive(Debug, Clone)]
pub struct OverheadGuard {
    cfg: OverheadConfig,
    history: VecDeque<f32>,
    rate: u32,
    suggest: bool,
}

impl OverheadGuard {
    /// A guard at full fidelity.
    #[must_use]
    pub fn new(cfg: OverheadConfig) -> Self {
        Self {
            cfg,
            history: VecDeque::new(),
            rate: 1,
            suggest: false,
        }
    }

    /// Current sampling rate.
    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// Feeds one interval: `cpu` consumed over `wall` on a machine with
    /// `logical_cpus` processors.
    pub fn observe(&mut self, cpu: Duration, wall: Duration, logical_cpus: u32) -> OverheadReport {
        let capacity = wall.as_secs_f64() * f64::from(logical_cpus.max(1));
        let pct = if capacity > 0.0 {
            (cpu.as_secs_f64() / capacity * 100.0) as f32
        } else {
            0.0
        };
        self.history.push_back(pct);
        while self.history.len() > self.cfg.sustain_samples.max(1) {
            self.history.pop_front();
        }
        let full = self.history.len() >= self.cfg.sustain_samples.max(1);
        let cap = self.cfg.cap_percent;
        if full && self.history.iter().all(|&p| p > cap) {
            self.suggest = true;
            if self.rate < self.cfg.max_sample_rate {
                self.rate = (self.rate * 4).min(self.cfg.max_sample_rate.max(1));
                self.history.clear();
            }
        } else if full && self.rate > 1 && self.history.iter().all(|&p| p * 4.0 < cap * 0.5) {
            self.rate = (self.rate / 4).max(1);
            self.history.clear();
            if self.rate == 1 {
                self.suggest = false;
            }
        }
        OverheadReport {
            cpu_percent: pct,
            sample_rate: self.rate,
            suggest_disable: self.suggest,
        }
    }
}

/// Deterministic 1-in-N admission of write events.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sampler {
    rate: u32,
    counter: u32,
}

impl Sampler {
    /// Sets the rate (1 = admit everything).
    pub fn set_rate(&mut self, rate: u32) {
        self.rate = rate.max(1);
    }

    /// Current rate.
    #[must_use]
    pub const fn rate(&self) -> u32 {
        if self.rate == 0 { 1 } else { self.rate }
    }

    /// Whether to process this write; when `true`, scale its size by
    /// [`Sampler::rate`].
    pub fn admit(&mut self) -> bool {
        let r = self.rate();
        if r == 1 {
            return true;
        }
        self.counter = (self.counter + 1) % r;
        self.counter == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC5: Duration = Duration::from_secs(5);

    fn feed(g: &mut OverheadGuard, pct: f64, n: usize) -> OverheadReport {
        // 8 CPUs × 5 s = 40 CPU-seconds of capacity per interval.
        let cpu = Duration::from_secs_f64(40.0 * pct / 100.0);
        let mut r = None;
        for _ in 0..n {
            r = Some(g.observe(cpu, SEC5, 8));
        }
        r.unwrap()
    }

    #[test]
    fn sustained_breach_engages_sampling_and_escalates() {
        let mut g = OverheadGuard::new(OverheadConfig::default());
        let r = feed(&mut g, 1.0, 20);
        assert_eq!((r.sample_rate, r.suggest_disable), (1, false));
        assert!((r.cpu_percent - 1.0).abs() < 1e-3);
        // Five bad samples and one good one: not sustained.
        feed(&mut g, 5.0, 5);
        let r = feed(&mut g, 1.0, 1);
        assert_eq!(r.sample_rate, 1);
        let r = feed(&mut g, 5.0, 6);
        assert_eq!((r.sample_rate, r.suggest_disable), (4, true));
        let r = feed(&mut g, 3.0, 6);
        assert_eq!(r.sample_rate, 16);
        let r = feed(&mut g, 3.0, 12);
        assert_eq!(r.sample_rate, 64);
        let r = feed(&mut g, 3.0, 12);
        assert_eq!((r.sample_rate, r.suggest_disable), (64, true));
    }

    #[test]
    fn steps_down_without_oscillating() {
        let mut g = OverheadGuard::new(OverheadConfig::default());
        feed(&mut g, 5.0, 6);
        assert_eq!(g.sample_rate(), 4);
        // 0.4% at 1/4 projects to 1.6% at full rate: above half the cap, stay.
        let r = feed(&mut g, 0.4, 30);
        assert_eq!(r.sample_rate, 4);
        // 0.2% projects to 0.8%: step down and clear the suggestion.
        let r = feed(&mut g, 0.2, 6);
        assert_eq!((r.sample_rate, r.suggest_disable), (1, false));
    }

    #[test]
    fn sampler_admits_one_in_n() {
        let mut s = Sampler::default();
        assert!((0..10).all(|_| s.admit()));
        s.set_rate(4);
        let admitted = (0..400).filter(|_| s.admit()).count();
        assert_eq!(admitted, 100);
        assert_eq!(s.rate(), 4);
    }
}
