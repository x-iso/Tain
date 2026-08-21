//! Where a frame of UI time actually goes.
//!
//! Turned on with `TAIN_DEBUG=frametime`. Reports once a second so the
//! logging itself is not what you end up measuring.
//!
//! This exists because "the UI feels slow" is not a diagnosis, and every guess
//! about which part is slow has a plausible story behind it. A phase breakdown
//! ends the argument in one run.

use std::time::{Duration, Instant};
use tracing::*;

/// How often the breakdown is printed.
const REPORT_EVERY: Duration = Duration::from_secs(1);

/// Per-phase timings for one frame, and the running summary.
#[derive(Default)]
pub struct FrameTimer {
    enabled: bool,
    phases: Vec<(&'static str, Duration)>,
    /// Every frame's phase list since the last report, summed per phase.
    totals: Vec<(&'static str, Duration, Duration)>,
    frames: u32,
    /// Wall time between the start of consecutive frames — what the user sees.
    last_start: Option<Instant>,
    interval: Duration,
    worst_interval: Duration,
    reported_at: Option<Instant>,
}

impl FrameTimer {
    /// Reads `TAIN_DEBUG` itself, in **every** build.
    ///
    /// The other debug flags are compiled out of release, and rightly so — they
    /// change what the app does. This one only measures, and release is the
    /// build whose numbers anyone cares about: a debug build's per-pixel work is
    /// slow for reasons that tell you nothing about the shipped one.
    pub fn from_env() -> Self {
        let enabled = std::env::var("TAIN_DEBUG")
            .is_ok_and(|v| v.split(',').any(|f| f.trim().eq_ignore_ascii_case("frametime")));
        if enabled {
            warn!("TAIN_DEBUG=frametime — reporting UI frame cost every second");
        }
        Self { enabled, ..Default::default() }
    }

    /// Begin a frame. Returns the instant to hand to [`Self::phase`].
    pub fn begin(&mut self) -> Option<Instant> {
        if !self.enabled {
            return None;
        }
        let now = Instant::now();
        if let Some(last) = self.last_start {
            let gap = now - last;
            self.interval += gap;
            self.worst_interval = self.worst_interval.max(gap);
        }
        self.last_start = Some(now);
        Some(now)
    }

    /// Record how long `name` took, given the instant it started.
    pub fn phase(&mut self, name: &'static str, started: Option<Instant>) -> Option<Instant> {
        let started = started?;
        let now = Instant::now();
        self.phases.push((name, now - started));
        Some(now)
    }

    /// Fold this frame into the running totals, printing once a second.
    pub fn end(&mut self) {
        if !self.enabled {
            return;
        }
        for (name, took) in self.phases.drain(..) {
            match self.totals.iter_mut().find(|(n, _, _)| *n == name) {
                Some((_, total, worst)) => {
                    *total += took;
                    *worst = (*worst).max(took);
                }
                None => self.totals.push((name, took, took)),
            }
        }
        self.frames += 1;

        let now = Instant::now();
        let due = self.reported_at.is_none_or(|at| now - at >= REPORT_EVERY);
        if !due || self.frames == 0 {
            return;
        }
        self.reported_at = Some(now);

        let n = self.frames as f64;
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let mut line = format!(
            "UI: {:.1} fps ({:.1} ms/frame, worst gap {:.1} ms) —",
            n / (self.interval.as_secs_f64().max(0.001)),
            ms(self.interval) / n,
            ms(self.worst_interval),
        );
        // Loudest first: the point is to find the one that matters.
        self.totals.sort_by_key(|(_, total, _)| std::cmp::Reverse(*total));
        for (name, total, worst) in &self.totals {
            line.push_str(&format!(
                " {name} {:.2}/{:.2}",
                ms(*total) / n,
                ms(*worst)
            ));
        }
        line.push_str("  (mean/worst ms)");
        info!("{line}");

        self.totals.clear();
        self.frames = 0;
        self.interval = Duration::ZERO;
        self.worst_interval = Duration::ZERO;
    }
}
