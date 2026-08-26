//! Measurement-driven quality control, and the bottleneck reporting behind it.
//!
//! ## What this is not
//!
//! It is not a table mapping a slider position to a resolution. A stream is
//! *supposed* to run at the full resolution and frame rate the user asked for;
//! degrading it is a response to a problem, not a setting. So this starts at the
//! ceiling and stays there until something measurably cannot keep up, then gives
//! back the smallest amount that fixes it, then takes it back once the pressure
//! clears.
//!
//! [`super::encoder::LatencyPriority`] does not decide *what* the stream runs
//! at. It decides **what gets sacrificed first when something has to give**:
//! towards quality, the frame rate sags and the pixels stay; towards latency,
//! pixels go and the frame rate holds. It also sets how patient the controller
//! is before acting.
//!
//! ## Why measurement rather than configuration
//!
//! Every bottleneck looks identical from the outside — the picture stutters and
//! the bandwidth graph sits idle. A slow encoder, a slow decoder, a slow link
//! and a slow capture are indistinguishable to the person watching, and three of
//! the four are made *worse* by the instinctive fix of lowering quality on the
//! other end. Naming which one is active is most of the value here; acting on it
//! correctly is the rest.
//!
//! ## Ladders, not continuous control
//!
//! Bitrate moves in steps and resolution in the four [`QualityScale`] stops,
//! rather than continuously. A resolution change is disruptive — it rebuilds the
//! encoder and forces a keyframe — so it must not happen twice a second, and
//! discrete steps with a dwell time are the simplest thing that guarantees that.

use std::time::{Duration, Instant};

use super::encoder::LatencyPriority;
use super::screen_capture::QualityScale;

// ── Bottlenecks ──────────────────────────────────────────────

/// A stage that is currently unable to keep up.
///
/// The discriminants are stable and exposed through [`Bottleneck::id`] so the UI
/// can key icons off them without depending on the variant order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bottleneck {
    /// The host cannot grab frames fast enough. Usually a busy GPU or a
    /// compositor stall — nothing about stream settings will help.
    SlowCapture,
    /// Encoding costs more than the frame budget. Fixed by fewer pixels, or by
    /// a hardware encoder if the software one is in use.
    SlowEncode,
    /// Frames are produced faster than the link carries them.
    SlowNetwork,
    /// The guest cannot decode as fast as frames arrive.
    SlowDecode,
    /// The guest decodes in time but cannot draw in time — its own display,
    /// not the stream.
    SlowRender,
    /// The round trip is long enough to be felt on input regardless of frame
    /// rate. Distance or a relayed path; not something quality can fix.
    HighLatency,
}

impl Bottleneck {
    pub const ALL: [Self; 6] = [
        Self::SlowCapture,
        Self::SlowEncode,
        Self::SlowNetwork,
        Self::SlowDecode,
        Self::SlowRender,
        Self::HighLatency,
    ];

    /// Stable identifier for icons and logs. Never reorder these strings.
    pub fn id(self) -> &'static str {
        match self {
            Self::SlowCapture => "slow-capture",
            Self::SlowEncode => "slow-encode",
            Self::SlowNetwork => "slow-network",
            Self::SlowDecode => "slow-decode",
            Self::SlowRender => "slow-render",
            Self::HighLatency => "high-latency",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::SlowCapture => "Slow capture",
            Self::SlowEncode => "Slow encoding",
            Self::SlowNetwork => "Slow network",
            Self::SlowDecode => "Slow decoding",
            Self::SlowRender => "Slow display",
            Self::HighLatency => "High latency",
        }
    }

    /// What the user can actually do about it.
    pub fn advice(self) -> &'static str {
        match self {
            Self::SlowCapture => {
                "The host cannot grab frames fast enough — usually another program \
                 saturating its GPU. Stream settings will not help."
            }
            Self::SlowEncode => {
                "Encoding a frame takes longer than the frame budget. Enable the \
                 hardware encoder on the host, or stream fewer pixels."
            }
            Self::SlowNetwork => {
                "The link cannot carry the frames being produced. Lowering the \
                 bitrate or resolution is what helps here."
            }
            Self::SlowDecode => {
                "This machine cannot decode frames as fast as they arrive. A lower \
                 resolution costs it less work."
            }
            Self::SlowRender => {
                "Frames decode in time but the window cannot draw them. Usually \
                 another program competing for this machine's GPU."
            }
            Self::HighLatency => {
                "The round trip to the host is long — distance, or a relayed \
                 connection rather than a direct one. Quality settings cannot \
                 shorten it."
            }
        }
    }

    /// Whether reducing the stream can actually relieve this.
    ///
    /// The controller only acts on the ones where it can: shrinking the picture
    /// to fight a 200 ms round trip would cost quality and change nothing.
    pub fn is_actionable(self) -> bool {
        matches!(self, Self::SlowEncode | Self::SlowNetwork | Self::SlowDecode)
    }

    fn bit(self) -> u8 {
        match self {
            Self::SlowCapture => 1,
            Self::SlowEncode => 2,
            Self::SlowNetwork => 4,
            Self::SlowDecode => 8,
            Self::SlowRender => 16,
            Self::HighLatency => 32,
        }
    }
}

/// The set of bottlenecks active right now.
///
/// A bitmask rather than a `Vec` so it is `Copy` and can be sent on the wire
/// in one byte — it is included in every status message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BottleneckSet(u8);

impl BottleneckSet {
    pub const NONE: Self = Self(0);

    pub fn insert(&mut self, b: Bottleneck) {
        self.0 |= b.bit();
    }

    pub fn contains(self, b: Bottleneck) -> bool {
        self.0 & b.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Active bottlenecks, in [`Bottleneck::ALL`] order so the UI is stable.
    pub fn iter(self) -> impl Iterator<Item = Bottleneck> {
        Bottleneck::ALL.into_iter().filter(move |b| self.contains(*b))
    }

    /// Whether anything here can be relieved by changing the stream.
    pub fn any_actionable(self) -> bool {
        self.iter().any(Bottleneck::is_actionable)
    }

    /// Raw bits, for the wire. Round-trips through [`Self::from_bits`].
    pub fn bits(self) -> u8 {
        self.0
    }

    /// Rebuild from the wire, discarding bits this build does not know about.
    pub fn from_bits(bits: u8) -> Self {
        let known = Bottleneck::ALL.iter().fold(0, |acc, b| acc | b.bit());
        Self(bits & known)
    }
}

// ── Measurements ─────────────────────────────────────────────

/// What the guest reports back about its own side of the pipeline.
///
/// Defined with the wire types, because that is what it is.
pub use super::protocol::GuestTelemetry;

/// One evaluation window of host-side measurements.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Observation {
    /// Median milliseconds spent grabbing a frame.
    pub capture_ms: f32,
    /// Median milliseconds spent encoding a frame.
    pub encode_ms: f32,
    /// Milliseconds per frame the pipeline sat blocked waiting for the network
    /// to take the previous frame. This is the direct signal that the link, not
    /// the CPU, is the limit.
    pub send_blocked_ms: f32,
    /// Frames per second the pipeline actually produced.
    pub achieved_fps: f32,
    /// Most recent guest report, if one has arrived.
    pub guest: Option<GuestTelemetry>,
}

// ── The plan ─────────────────────────────────────────────────

/// What the stream should run at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPlan {
    pub scale: QualityScale,
    pub fps: u32,
    /// Concrete, never zero — the controller needs a number it can reduce.
    pub bitrate_kbps: u32,
}

impl StreamPlan {
    /// The frame budget this plan implies, in milliseconds.
    pub fn frame_budget_ms(&self) -> f32 {
        1000.0 / self.fps.max(1) as f32
    }
}

/// Bitrate steps, as a percentage of the ceiling.
///
/// Coarse on purpose: a 10% cut is inside the noise of what a link delivers
/// minute to minute, so it would take several windows to have any effect while
/// the picture was already stuttering.
///
/// # Why it goes down to a twentieth
///
/// It used to stop at 25%, and that was the reason a long-distance link never
/// settled. The ladder is a fraction of the *ceiling*, and the ceiling is
/// derived from the display: a 2560x1600 host at 60 fps asks for about 17
/// Mbit/s, so the old floor was still 4.3 Mbit/s. A link that could not carry
/// the top of the ladder usually could not carry a quarter of it either, and the
/// controller ran out of ladder while the picture was still breaking up.
///
/// The bottom of this one is around 850 kbit/s on that same host, which is a
/// rate a distant link can actually hold. It looks soft, and it is supposed to —
/// a soft picture that arrives beats a sharp one that stutters, which is the
/// whole reason this type exists.
const BITRATE_LADDER: [u32; 8] = [100, 70, 50, 35, 25, 15, 10, 5];

/// The lowest bitrate the ladder may ever produce, in kbit/s.
///
/// A percentage of a small ceiling is a smaller number still, and below roughly
/// this the encoder stops producing a picture at all rather than a soft one.
const BITRATE_FLOOR_KBPS: u32 = 400;

/// How long a plan must hold before it may change again.
///
/// A resolution change forces a keyframe and rebuilds the encoder, so reacting
/// to every window would produce a stream that spends its life renegotiating.
const MIN_DWELL: Duration = Duration::from_millis(1500);

/// How soon after a recovery a back-off counts as that recovery having failed.
///
/// Generous, because the point is to catch a link that cannot hold the higher
/// setting at all rather than one that had a bad second.
const PUMP_WINDOW: Duration = Duration::from_secs(20);

/// The most the recovery patience may be multiplied by.
///
/// Bounded so a link that recovers permanently — someone stopped a download —
/// is still eventually offered its quality back rather than being written off
/// for the rest of the session.
const MAX_RECOVER_PENALTY: u32 = 4;

/// Below this many bits per pixel per second, the picture stops being a picture.
///
/// Half of `codec_h264::DEFAULT_BITS_PER_PIXEL`, which is what a stream asks for
/// when nothing is wrong. At half of it a frame is soft; at a tenth — which is
/// where a 517 kbit/s link sending 1920x1200 lands — it is unreadable, and every
/// bit spent on the extra pixels made it worse rather than better.
const STARVED_BITS_PER_PIXEL: f64 = 0.035;

// ── The controller ───────────────────────────────────────────

/// One axis the controller can give ground on. Returns whether it had any left.
type Lever = fn(&mut AdaptiveController) -> bool;

/// Holds the stream at the user's ceiling, and gives ground only under
/// measured, sustained pressure.
pub struct AdaptiveController {
    priority: LatencyPriority,
    /// The host display's own size, which is what a [`QualityScale`] is a
    /// fraction *of*. Needed to answer how many pixels a plan is paying for —
    /// see [`Self::bits_per_pixel`].
    native: (u16, u16),
    /// What the user asked for. The controller never exceeds this.
    ceiling: StreamPlan,
    /// Index into [`BITRATE_LADDER`]; 0 is the ceiling.
    bitrate_step: usize,
    /// Index into [`QualityScale::ALL`]; 0 is the ceiling's own scale.
    scale_step: usize,
    /// Steps below the ceiling frame rate, each halving towards `min_fps`.
    fps_step: usize,
    active: BottleneckSet,
    /// Whether the last change was a recovery. With [`PUMP_WINDOW`], this is how
    /// a recovery that had to be immediately undone is recognised.
    last_change_recovered: bool,
    /// How many recoveries have been undone. Multiplies the patience before the
    /// next one, so a link that cannot hold the higher setting is asked less and
    /// less often whether it can.
    recover_penalty: u32,
    /// Consecutive windows under actionable pressure, and consecutive clean
    /// ones. Both are needed: acting on one bad window chases noise, and
    /// recovering after one good one oscillates.
    strained_windows: u32,
    clean_windows: u32,
    last_change: Instant,
    /// What the round trip costs when nothing is queued in it. See [`RttFloor`].
    rtt_floor: RttFloor,
}

/// The shortest round trip this link has shown lately.
///
/// # Why a floor and not a threshold
///
/// A fixed threshold cannot tell distance from congestion, and the two want
/// opposite responses. A link to another continent sits at 110 ms because that
/// is how far away it is; shrinking the picture will not shorten it, which is
/// why [`Bottleneck::HighLatency`] is not actionable. The same link at 900 ms is
/// not further away — it is the same distance with most of a second of our own
/// frames queued in front of the one being timed, and sending less is exactly
/// what relieves it.
///
/// Measured against its own floor, those are trivially different: 110 against a
/// floor of 110, and 900 against a floor of 110. Measured against 80 ms, they
/// are both simply "high", which is what a controller learned nothing from
/// while it walked the bitrate up from 5 to 23 Mbit/s on a link that could
/// carry about ten.
///
/// # Why two windows
///
/// The floor has to be able to rise. A route that genuinely changes — a direct
/// connection dropping back to a relay — is a new distance, not a queue, and a
/// floor that only ever fell would read it as permanent congestion and hold the
/// stream down for the rest of the session.
///
/// So the minimum is kept over a window that is periodically retired. A lower
/// round trip is adopted at once, because a shorter path is news immediately; a
/// longer one has to survive a whole window before it becomes the floor, by
/// which time a queue would have drained.
#[derive(Debug, Default)]
struct RttFloor {
    /// Minimum over the window currently in force.
    settled: Option<f32>,
    /// Minimum over the window being collected, which will replace it.
    collecting: Option<f32>,
    window_started: Option<Instant>,
}

impl RttFloor {
    /// Fold in one measurement.
    fn observe(&mut self, rtt_ms: f32, now: Instant) {
        // A round trip of zero or worse is not a measurement. It is what an
        // unanswered ping reports.
        if !(rtt_ms.is_finite() && rtt_ms > 0.0) {
            return;
        }

        let started = *self.window_started.get_or_insert(now);
        if now.duration_since(started) >= RTT_FLOOR_WINDOW {
            self.settled = self.collecting.take();
            self.window_started = Some(now);
        }

        self.collecting = Some(self.collecting.map_or(rtt_ms, |c| c.min(rtt_ms)));
        // Downwards immediately: a shorter path is a better answer to the same
        // question, and waiting a window to believe it would read the improvement
        // as congestion relative to a floor that is now wrong.
        self.settled = Some(self.settled.map_or(rtt_ms, |f| f.min(rtt_ms)));
    }

    /// The floor, once there is one. Used by the tests, which are where the
    /// two-window behaviour is pinned down.
    #[cfg(test)]
    fn get(&self) -> Option<f32> {
        self.settled
    }

    /// Whether `rtt_ms` is far enough above the floor to be our own queue.
    ///
    /// Both a ratio and an absolute margin have to be exceeded, and each covers
    /// what the other gets wrong. On a link across a city the floor is a couple
    /// of milliseconds, where a ratio alone would call ordinary jitter
    /// congestion; across an ocean it is over a hundred, where a fixed margin
    /// alone would trip on the normal spread between one sample and the next.
    fn is_queued(&self, rtt_ms: f32) -> bool {
        let Some(floor) = self.settled else { return false };
        rtt_ms > floor * QUEUE_RTT_RATIO && rtt_ms > floor + QUEUE_RTT_MARGIN_MS
    }
}

impl AdaptiveController {
    pub fn new(priority: LatencyPriority, ceiling: StreamPlan, native: (u16, u16)) -> Self {
        Self {
            priority,
            native,
            ceiling,
            bitrate_step: 0,
            scale_step: 0,
            fps_step: 0,
            active: BottleneckSet::NONE,
            last_change_recovered: false,
            recover_penalty: 0,
            strained_windows: 0,
            clean_windows: 0,
            last_change: Instant::now(),
            rtt_floor: RttFloor::default(),
        }
    }

    /// The stream settings in force right now.
    pub fn plan(&self) -> StreamPlan {
        let scale_index = QualityScale::ALL
            .iter()
            .position(|s| *s == self.ceiling.scale)
            .unwrap_or(0);
        let scale = QualityScale::ALL
            [(scale_index + self.scale_step).min(QualityScale::ALL.len() - 1)];

        let mut fps = self.ceiling.fps;
        for _ in 0..self.fps_step {
            fps = (fps * 2 / 3).max(self.priority.min_fps());
        }

        StreamPlan {
            scale,
            fps: fps.max(1),
            bitrate_kbps: (self.ceiling.bitrate_kbps as u64
                * BITRATE_LADDER[self.bitrate_step] as u64
                / 100)
                .max(BITRATE_FLOOR_KBPS.min(self.ceiling.bitrate_kbps) as u64)
                as u32,
        }
    }

    /// Bottlenecks measured in the most recent window.
    pub fn bottlenecks(&self) -> BottleneckSet {
        self.active
    }

    /// True when the stream is running below what the user asked for.
    pub fn is_degraded(&self) -> bool {
        self.bitrate_step > 0 || self.scale_step > 0 || self.fps_step > 0
    }

    /// Change what the user asked for.
    ///
    /// Resets the degradation: the new ceiling is a fresh statement of intent,
    /// and carrying old penalties into it would mean a user who just raised the
    /// quality saw no change.
    pub fn set_ceiling(&mut self, ceiling: StreamPlan) {
        if ceiling != self.ceiling {
            self.ceiling = ceiling;
            self.bitrate_step = 0;
            self.scale_step = 0;
            self.fps_step = 0;
            self.strained_windows = 0;
            self.clean_windows = 0;
        }
    }

    /// Adopt a new position on the quality-versus-latency slider.
    ///
    /// Takes effect from the next decision. Deliberately does *not* reset how
    /// far the stream has already been degraded: the link has not changed, only
    /// the preference about which axis to give up, and starting again from the
    /// ceiling would mean a settings tweak briefly floods a link already known
    /// not to carry it.
    pub fn set_priority(&mut self, priority: LatencyPriority) {
        self.priority = priority;
    }

    /// Feed one window of measurements. Returns the new plan if it changed.
    pub fn observe(&mut self, obs: &Observation) -> Option<StreamPlan> {
        self.observe_at(obs, Instant::now())
    }

    /// As [`Self::observe`], with the clock supplied — which is what makes the
    /// dwell and hysteresis logic testable without sleeping.
    pub fn observe_at(&mut self, obs: &Observation, now: Instant) -> Option<StreamPlan> {
        if let Some(guest) = obs.guest {
            self.rtt_floor.observe(guest.rtt_ms, now);
        }

        let before = self.plan();
        self.active = self.detect(obs, &before);

        if self.active.any_actionable() {
            self.strained_windows += 1;
            self.clean_windows = 0;
        } else {
            self.clean_windows += 1;
            self.strained_windows = 0;
        }

        // A change is only allowed once the current plan has had time to show
        // its effect. Without this the controller reacts to the transient its
        // own last change caused.
        if now.duration_since(self.last_change) < MIN_DWELL {
            return None;
        }

        if self.strained_windows >= self.priority.strain_tolerance() {
            // # Why more than one step
            //
            // The controller used to give up exactly one step per decision, and
            // a decision costs a dwell plus several windows — call it three
            // seconds. A link that can carry a third of what is being sent needs
            // four steps, which is a quarter of a minute of a broken picture
            // while the controller walks down a ladder it can already see the
            // bottom of.
            //
            // So the size of the step follows the size of the deficit. Being
            // slightly over budget still moves one rung at a time.
            for _ in 0..self.severity(obs, &before) {
                self.back_off();
            }
            self.strained_windows = 0;

            // Backing off shortly after taking quality back means the recovery
            // was wrong. Without noticing that, a marginal link spends its whole
            // session climbing up and falling down — which reads as the stream
            // pulsing, and is more distracting than sitting a step low forever.
            if now.duration_since(self.last_change) < PUMP_WINDOW && self.last_change_recovered {
                self.recover_penalty = (self.recover_penalty + 1).min(MAX_RECOVER_PENALTY);
            }
            self.last_change_recovered = false;
        } else if self.clean_windows >= self.patience() && self.is_degraded() {
            self.recover();
            self.clean_windows = 0;
            self.last_change_recovered = true;
        } else {
            return None;
        }

        let after = self.plan();
        if after == before {
            return None;
        }
        self.last_change = now;
        Some(after)
    }

    /// Consecutive clean windows this controller wants before recovering.
    ///
    /// Grows each time a recovery had to be undone — see [`Self::recover_penalty`].
    fn patience(&self) -> u32 {
        self.priority.recovery_patience() * (1 + self.recover_penalty)
    }

    /// How many steps to give up at once, from how far behind the stream is.
    ///
    /// Only the two signals that quantify a shortfall are used. "Encoding is
    /// over budget" says something is wrong but not by how much, so it stays
    /// worth one step; the guest showing a third of the frames, or the send path
    /// blocking for longer than a whole frame, both say plainly how big the gap
    /// is.
    fn severity(&self, obs: &Observation, plan: &StreamPlan) -> u32 {
        let budget = plan.frame_budget_ms();
        let mut steps = 1;

        // Frames that were produced and did not arrive.
        if let Some(guest) = obs.guest
            && obs.achieved_fps > 1.0
        {
            // Against what decoded, not what was drawn: a guest that
            // decoded every frame and painted half of them lost nothing.
            let delivered = guest.decoded_fps / obs.achieved_fps;
            if delivered < 0.35 {
                steps = steps.max(3);
            } else if delivered < 0.6 {
                steps = steps.max(2);
            }
        }

        // Time the pipeline spent unable to hand the link a frame it had ready.
        if obs.send_blocked_ms > budget * 2.0 {
            steps = steps.max(3);
        } else if obs.send_blocked_ms > budget {
            steps = steps.max(2);
        }

        steps
    }

    /// Decide which stages are behind, given the budget the current plan implies.
    fn detect(&self, obs: &Observation, plan: &StreamPlan) -> BottleneckSet {
        let budget = plan.frame_budget_ms();
        let mut set = BottleneckSet::NONE;

        // Each stage is judged against the whole budget rather than a share of
        // it: they overlap across threads, so any single one exceeding the
        // budget caps the frame rate on its own.
        if obs.capture_ms > budget {
            set.insert(Bottleneck::SlowCapture);
        }
        if obs.encode_ms > budget {
            set.insert(Bottleneck::SlowEncode);
        }
        // Blocking on the send path is unambiguous: frames were ready and the
        // link would not take them.
        if obs.send_blocked_ms > budget * 0.5 {
            set.insert(Bottleneck::SlowNetwork);
        }

        if let Some(guest) = obs.guest {
            if guest.decode_ms > budget {
                set.insert(Bottleneck::SlowDecode);
            }
            if guest.render_ms > budget {
                set.insert(Bottleneck::SlowRender);
            }
            // The guest *decoding* materially fewer frames than the host
            // produced means they were lost or late in between. Comparing
            // against what it drew instead reads a 60 Hz monitor as a lossy
            // link — see `GuestTelemetry::decoded_fps` for the session that
            // cost.
            if obs.achieved_fps > 1.0 && guest.decoded_fps < obs.achieved_fps * 0.8 {
                set.insert(Bottleneck::SlowNetwork);
            }
            if guest.rtt_ms > HIGH_LATENCY_MS {
                set.insert(Bottleneck::HighLatency);
            }
            // # The signal the other two miss
            //
            // A link being over-driven does not have to block the sender or
            // lose a frame. QUIC accepts the write and queues it, so
            // `send_blocked_ms` stays near zero — measured at 2 to 5 ms per
            // second on a link carrying half again what it could — and every
            // frame does eventually arrive and decode, so the delivered-frame
            // check is quiet too. Both of the existing network signals look
            // clean while the queue grows.
            //
            // What grows with it is the round trip. Against this link's own
            // floor that is unambiguous, and unlike distance it is something
            // sending less will actually relieve — so it is reported as the
            // link being unable to carry the stream, which is what it is.
            if self.rtt_floor.is_queued(guest.rtt_ms) {
                set.insert(Bottleneck::SlowNetwork);
            }
        }

        set
    }

    /// Bits each pixel of the current plan is actually getting.
    ///
    /// The number that says whether a picture is being kept or merely being
    /// *counted*. See [`STARVED_BITS_PER_PIXEL`].
    fn bits_per_pixel(&self) -> f64 {
        let plan = self.plan();
        let (w, h) = plan.scale.apply(self.native);
        let pixels = w as f64 * h as f64 * plan.fps.max(1) as f64;
        if pixels <= 0.0 {
            return f64::INFINITY;
        }
        plan.bitrate_kbps as f64 * 1000.0 / pixels
    }

    /// Give up one step, on the axis that relieves the measured problem.
    fn back_off(&mut self) {
        let net = self.active.contains(Bottleneck::SlowNetwork);
        let compute = self.active.contains(Bottleneck::SlowEncode)
            || self.active.contains(Bottleneck::SlowDecode);

        // # Pixels and bitrate are not independent axes
        //
        // They were treated as though they were, and that is how a session ends
        // up at 517 kbit/s still trying to send 1920x1200 — 0.03 bits a pixel,
        // which is not a picture, it is a rumour of one. Every complaint about
        // it was about *compression*: text mushy, detail smeared. The controller
        // thought it was protecting quality, because it was protecting the thing
        // it had been told to protect, which was pixel count.
        //
        // So before anything else: if the bits on hand cannot pay for the pixels
        // being sent, send fewer pixels. The same bits over a smaller picture is
        // a sharper picture, which is what everyone actually meant.
        //
        // This overrides the priority's own scale floor, deliberately. That
        // floor says how far the user would *rather* not shrink; it was never
        // meant to hold a resolution whose every pixel is a guess.
        if self.bits_per_pixel() < STARVED_BITS_PER_PIXEL
            && self.lower_scale_to(QualityScale::ALL.len() - 1)
        {
            return;
        }

        // Bitrate first for a slow link: it is the cheapest thing to change,
        // costs no keyframe, and is exactly the axis a bandwidth limit binds on.
        // For a slow encoder or decoder it does almost nothing — the work is per
        // pixel, not per bit — so that case goes to the pixel axes directly.
        if net && !compute && self.bitrate_step + 1 < BITRATE_LADDER.len() {
            self.bitrate_step += 1;
            return;
        }

        // Then whichever of resolution and frame rate the user cares less
        // about. This is the whole job of the priority slider.
        let (first, second): (Lever, Lever) = if self.priority.sheds_pixels_first() {
            (Self::lower_scale, Self::lower_fps)
        } else {
            (Self::lower_fps, Self::lower_scale)
        };

        if first(self) || second(self) {
            return;
        }
        // Both pixel axes are at their floor; the bitrate is all that is left.
        if self.bitrate_step + 1 < BITRATE_LADDER.len() {
            self.bitrate_step += 1;
        }
    }

    fn lower_scale(&mut self) -> bool {
        let floor = QualityScale::ALL
            .iter()
            .position(|s| *s == self.priority.min_scale())
            .unwrap_or(QualityScale::ALL.len() - 1);
        self.lower_scale_to(floor)
    }

    /// Shed one step of resolution, down to `floor` in [`QualityScale::ALL`].
    fn lower_scale_to(&mut self, floor: usize) -> bool {
        let index = QualityScale::ALL
            .iter()
            .position(|s| *s == self.ceiling.scale)
            .unwrap_or(0);
        if index + self.scale_step < floor {
            self.scale_step += 1;
            return true;
        }
        false
    }

    fn lower_fps(&mut self) -> bool {
        if self.plan().fps > self.priority.min_fps() {
            self.fps_step += 1;
            true
        } else {
            false
        }
    }

    /// Take back one step, in the reverse order it was given up.
    ///
    /// Reverse order matters: whatever was sacrificed last was the least
    /// painful thing to lose, so it is the first thing worth reclaiming, and
    /// the most disruptive change (resolution) is reclaimed only once the
    /// cheaper ones have held.
    fn recover(&mut self) {
        if self.priority.sheds_pixels_first() {
            if self.fps_step > 0 {
                self.fps_step -= 1;
            } else if self.scale_step > 0 {
                self.scale_step -= 1;
            } else if self.bitrate_step > 0 {
                self.bitrate_step -= 1;
            }
        } else if self.scale_step > 0 {
            self.scale_step -= 1;
        } else if self.fps_step > 0 {
            self.fps_step -= 1;
        } else if self.bitrate_step > 0 {
            self.bitrate_step -= 1;
        }
    }
}

/// Round trip beyond which input feels detached however good the picture is.
///
/// 80 ms is roughly where a pointer stops feeling attached to the hand; well
/// under it, frame rate dominates the experience instead.
const HIGH_LATENCY_MS: f32 = 80.0;

/// How long a round-trip floor stays authoritative before being re-measured.
///
/// Long enough that a queue cannot outlast it and become the new floor — a
/// stream is backed off within a few seconds of being detected — and short
/// enough that a genuine change of route is picked up while the session is
/// still worth adapting.
const RTT_FLOOR_WINDOW: Duration = Duration::from_secs(30);

/// How far above its floor a round trip has to be before it counts as queued.
///
/// Both apply; see [`RttFloor::is_queued`]. A link at 110 ms therefore has to
/// reach 165 ms, which the reported session did — repeatedly, to 700 and 900 —
/// while sitting comfortably under any fixed threshold that 110 ms itself did
/// not already trip.
const QUEUE_RTT_RATIO: f32 = 1.5;
const QUEUE_RTT_MARGIN_MS: f32 = 50.0;

#[cfg(test)]
mod tests {
    use super::*;

    /// The display these tests pretend to be streaming.
    ///
    /// A real one, because the bits-per-pixel coupling in `back_off` is only
    /// meaningful against a real pixel count.
    const NATIVE: (u16, u16) = (2560, 1600);

    fn ceiling() -> StreamPlan {
        StreamPlan { scale: QualityScale::Native, fps: 60, bitrate_kbps: 20_000 }
    }

    fn healthy() -> Observation {
        Observation {
            capture_ms: 3.0,
            encode_ms: 6.0,
            send_blocked_ms: 0.0,
            achieved_fps: 60.0,
            guest: Some(GuestTelemetry {
                decode_ms: 4.0,
                render_ms: 2.0,
                displayed_fps: 60.0,
                decoded_fps: 60.0,
                rtt_ms: 12.0,
            }),
        }
    }

    /// Feed `obs` until the controller reacts, or give up. Advances a fake clock
    /// past the dwell each time so the test is not gated on wall time.
    fn drive(
        c: &mut AdaptiveController,
        obs: &Observation,
        windows: usize,
    ) -> Vec<StreamPlan> {
        let mut clock = Instant::now();
        let mut changes = Vec::new();
        for _ in 0..windows {
            clock += MIN_DWELL + Duration::from_millis(1);
            if let Some(plan) = c.observe_at(obs, clock) {
                changes.push(plan);
            }
        }
        changes
    }

    #[test]
    fn a_healthy_stream_is_left_completely_alone() {
        // The entire point: quality is not something to be spent by default.
        let mut c = AdaptiveController::new(LatencyPriority::LATENCY, ceiling(), NATIVE);
        let changes = drive(&mut c, &healthy(), 50);

        assert!(changes.is_empty(), "an untroubled stream must never be degraded");
        assert_eq!(c.plan(), ceiling());
        assert!(!c.is_degraded());
        assert!(c.bottlenecks().is_empty());
    }

    #[test]
    fn even_the_latency_end_starts_at_full_resolution() {
        // Regression against the earlier design, where asking for low latency
        // permanently dropped to a third of the pixels whether or not anything
        // was actually struggling.
        for value in 0..=100u8 {
            let c = AdaptiveController::new(LatencyPriority::new(value), ceiling(), NATIVE);
            assert_eq!(
                c.plan().scale,
                QualityScale::Native,
                "priority {value} degraded a stream that had shown no problem"
            );
            assert_eq!(c.plan().fps, 60);
        }
    }

    /// A distant link, sitting at its own floor, is not congested.
    ///
    /// This is the case that must not regress: 110 ms across an ocean is
    /// distance, and the whole reason [`Bottleneck::HighLatency`] is not
    /// actionable is that no amount of shrinking the picture shortens it.
    #[test]
    fn a_link_that_is_merely_far_away_is_left_alone() {
        let distant = |rtt: f32| Observation {
            guest: Some(GuestTelemetry { rtt_ms: rtt, ..healthy().guest.unwrap() }),
            ..healthy()
        };

        let mut c = AdaptiveController::new(LatencyPriority::BALANCED, ceiling(), NATIVE);
        let mut clock = Instant::now();
        let mut changes = Vec::new();
        // The spread a real transatlantic link showed while behaving: 107 to 131.
        for rtt in [110.0f32, 119.0, 107.0, 131.0, 112.0, 125.0, 118.0].repeat(8) {
            clock += MIN_DWELL + Duration::from_millis(1);
            if let Some(plan) = c.observe_at(&distant(rtt), clock) {
                changes.push(plan);
            }
        }

        assert!(changes.is_empty(), "distance was treated as something to fix: {changes:?}");
        assert!(c.bottlenecks().contains(Bottleneck::HighLatency), "it is still high");
        assert!(
            !c.bottlenecks().contains(Bottleneck::SlowNetwork),
            "and being far away is not the link failing to carry the stream"
        );
    }

    /// The failure this was written for.
    ///
    /// Reported: a host on roughly a 10 Mbit/s uplink, 110 ms away, whose
    /// controller walked the bitrate from 5 Mbit/s to its 23 Mbit/s ceiling and
    /// stayed there while the picture fell apart. Every window looked clean,
    /// because the only signal that reflected the queue was the round trip and
    /// the round trip was written off as distance.
    ///
    /// Neither existing network signal catches it. The sender does not block —
    /// QUIC takes the write and queues it, measured at 2 to 5 ms per second —
    /// and every frame eventually arrives and decodes, so the delivered-frame
    /// check is quiet as well.
    #[test]
    fn a_queue_growing_on_a_distant_link_is_recognised_as_the_link() {
        let at = |rtt: f32| Observation {
            // Exactly what the reported host measured: nothing blocking, every
            // frame arriving, and the queue visible only in the round trip.
            send_blocked_ms: 4.0 / 1000.0,
            guest: Some(GuestTelemetry { rtt_ms: rtt, ..healthy().guest.unwrap() }),
            ..healthy()
        };

        let mut c = AdaptiveController::new(LatencyPriority::BALANCED, ceiling(), NATIVE);
        let mut clock = Instant::now();

        // Settle the floor at this link's real distance.
        for _ in 0..10 {
            clock += MIN_DWELL + Duration::from_millis(1);
            c.observe_at(&at(110.0), clock);
        }
        assert_eq!(c.plan(), ceiling(), "nothing wrong yet");

        // Then the queue the ceiling put there: the same session's own spikes.
        let mut changes = Vec::new();
        for rtt in [855.0f32, 721.0, 932.0, 800.0].repeat(4) {
            clock += MIN_DWELL + Duration::from_millis(1);
            if let Some(plan) = c.observe_at(&at(rtt), clock) {
                changes.push(plan);
            }
        }

        assert!(c.bottlenecks().contains(Bottleneck::SlowNetwork), "the queue is the link");
        let first = changes.first().expect("a queue that deep must produce a response");
        assert!(
            first.bitrate_kbps < ceiling().bitrate_kbps,
            "bitrate is the axis a saturated uplink binds on, got {first:?}"
        );
    }

    /// On a fast link the floor is a millisecond or two, where a ratio alone
    /// would call every ordinary sample congestion.
    #[test]
    fn ordinary_jitter_on_a_fast_link_is_not_a_queue() {
        let mut floor = RttFloor::default();
        let start = Instant::now();
        floor.observe(2.0, start);

        // Ten times the floor, and still only 20 ms.
        assert!(!floor.is_queued(20.0), "20 ms on a LAN is not a queue");
        // Half a second on the same link plainly is.
        assert!(floor.is_queued(500.0));
    }

    /// And across an ocean the floor is over a hundred, where a fixed margin
    /// alone would trip on the normal spread between samples.
    #[test]
    fn the_spread_of_a_distant_link_is_not_a_queue() {
        let mut floor = RttFloor::default();
        let start = Instant::now();
        for rtt in [110.0f32, 119.0, 107.0, 131.0] {
            floor.observe(rtt, start);
        }
        assert_eq!(floor.get(), Some(107.0));

        for ordinary in [110.0f32, 119.0, 131.0, 140.0, 155.0] {
            assert!(!floor.is_queued(ordinary), "{ordinary} ms is within this link's spread");
        }
        for queued in [200.0f32, 721.0, 932.0] {
            assert!(floor.is_queued(queued), "{queued} ms is a queue");
        }
    }

    /// A route that genuinely lengthens is a new distance, not a queue.
    ///
    /// A direct connection dropping back to a relay is the case. A floor that
    /// only ever fell would read the new distance as permanent congestion and
    /// hold the stream down for the rest of the session.
    #[test]
    fn a_longer_route_becomes_the_new_floor() {
        let mut floor = RttFloor::default();
        let start = Instant::now();
        floor.observe(20.0, start);
        assert!(floor.is_queued(200.0), "at first this is far above the floor");

        // The relay is now the path, and stays the path.
        let mut clock = start;
        for _ in 0..4 {
            clock += RTT_FLOOR_WINDOW + Duration::from_secs(1);
            floor.observe(200.0, clock);
        }
        assert_eq!(floor.get(), Some(200.0), "the longer path is the distance now");
        assert!(!floor.is_queued(200.0), "and sitting at it is not congestion");
        assert!(floor.is_queued(700.0), "while a queue on top of it still is");
    }

    /// A shorter path is believed at once — it answers the same question better.
    #[test]
    fn a_shorter_route_is_adopted_immediately() {
        let mut floor = RttFloor::default();
        let start = Instant::now();
        floor.observe(200.0, start);
        floor.observe(20.0, start);
        assert_eq!(floor.get(), Some(20.0));
    }

    /// An unanswered ping is not a measurement.
    #[test]
    fn a_round_trip_of_zero_does_not_become_the_floor() {
        let mut floor = RttFloor::default();
        let start = Instant::now();
        floor.observe(110.0, start);
        for nonsense in [0.0f32, -5.0, f32::NAN, f32::INFINITY] {
            floor.observe(nonsense, start);
        }
        assert_eq!(floor.get(), Some(110.0));
        // A floor of zero would make every sample infinitely above it.
        assert!(!floor.is_queued(120.0));
    }

    #[test]
    fn a_slow_link_gives_up_bitrate_before_pixels() {
        let mut c = AdaptiveController::new(LatencyPriority::BALANCED, ceiling(), NATIVE);
        let congested = Observation { send_blocked_ms: 40.0, ..healthy() };

        let changes = drive(&mut c, &congested, 10);
        let first = changes.first().expect("congestion must produce a response");

        assert!(first.bitrate_kbps < ceiling().bitrate_kbps);
        assert_eq!(first.scale, QualityScale::Native, "pixels are not the constrained axis here");
        assert!(c.bottlenecks().contains(Bottleneck::SlowNetwork));
    }

    #[test]
    fn a_slow_encoder_gives_up_pixels_because_bitrate_would_not_help() {
        // Encode cost is per pixel. Lowering the bitrate would leave the encoder
        // doing exactly as much work and the stream just as far behind.
        let mut c = AdaptiveController::new(LatencyPriority::LATENCY, ceiling(), NATIVE);
        let slow = Observation { encode_ms: 40.0, ..healthy() };

        let changes = drive(&mut c, &slow, 10);
        let first = changes.first().expect("a slow encoder must produce a response");

        assert_ne!(first.scale, QualityScale::Native);
        assert_eq!(first.bitrate_kbps, ceiling().bitrate_kbps);
        assert!(c.bottlenecks().contains(Bottleneck::SlowEncode));
    }

    #[test]
    fn the_priority_decides_which_of_pixels_and_frame_rate_is_sacrificed() {
        let slow = Observation { encode_ms: 40.0, ..healthy() };

        let mut latency = AdaptiveController::new(LatencyPriority::LATENCY, ceiling(), NATIVE);
        drive(&mut latency, &slow, 6);
        let mut quality = AdaptiveController::new(LatencyPriority::QUALITY, ceiling(), NATIVE);
        drive(&mut quality, &slow, 6);

        assert!(
            latency.plan().scale != QualityScale::Native,
            "asking for latency should spend pixels"
        );
        assert_eq!(
            quality.plan().scale,
            QualityScale::Native,
            "asking for quality should keep pixels and spend frame rate instead"
        );
        assert!(quality.plan().fps < ceiling().fps);
    }

    #[test]
    fn quality_priority_still_yields_when_it_has_nothing_left_to_give() {
        // Holding native resolution at 5fps forever would be a worse answer than
        // a slightly smaller picture that moves.
        let mut c = AdaptiveController::new(LatencyPriority::QUALITY, ceiling(), NATIVE);
        drive(&mut c, &Observation { encode_ms: 200.0, ..healthy() }, 40);

        assert!(c.plan().fps >= LatencyPriority::QUALITY.min_fps());
        assert_ne!(
            c.plan().scale,
            QualityScale::Native,
            "the frame rate floor must hand over to the resolution axis"
        );
    }

    /// Ground given up is ground taken back — eventually.
    ///
    /// "Eventually" is the point. Back-off takes several steps at once when the
    /// deficit is large; recovery only ever takes one. That asymmetry is
    /// deliberate — falling fast and climbing slowly is what keeps a marginal
    /// link from pulsing — so this drives enough clean windows to climb the
    /// whole way rather than assuming it happens quickly.
    #[test]
    fn pressure_that_clears_gives_the_quality_back() {
        let mut c = AdaptiveController::new(LatencyPriority::BALANCED, ceiling(), NATIVE);
        drive(&mut c, &Observation { send_blocked_ms: 40.0, ..healthy() }, 20);
        assert!(c.is_degraded(), "the test needs something to recover from");

        drive(&mut c, &healthy(), 400);
        assert!(!c.is_degraded(), "a stream that recovered must return to the ceiling");
        assert_eq!(c.plan(), ceiling());
    }

    #[test]
    fn one_bad_window_is_not_enough_to_act_on() {
        // Networks hiccup. Reacting to a single window would make the picture
        // visibly step down every time something else on the machine blinked.
        let mut c = AdaptiveController::new(LatencyPriority::BALANCED, ceiling(), NATIVE);
        let mut clock = Instant::now() + MIN_DWELL * 2;

        assert!(c
            .observe_at(&Observation { send_blocked_ms: 40.0, ..healthy() }, clock)
            .is_none());
        clock += MIN_DWELL * 2;
        assert!(c.observe_at(&healthy(), clock).is_none());
        assert!(!c.is_degraded());
    }

    #[test]
    fn changes_are_spaced_out_even_under_sustained_pressure() {
        // Back to back resolution changes would each force a keyframe and a
        // decoder reset, which looks worse than the problem being fixed.
        let mut c = AdaptiveController::new(LatencyPriority::LATENCY, ceiling(), NATIVE);
        let slow = Observation { encode_ms: 100.0, ..healthy() };
        let start = Instant::now();

        let mut changes = 0;
        // Twenty windows inside a single dwell period.
        for i in 0..20 {
            if c.observe_at(&slow, start + Duration::from_millis(i * 50)).is_some() {
                changes += 1;
            }
        }
        assert!(changes <= 1, "{changes} changes inside one dwell period");
    }

    #[test]
    fn latency_that_cannot_be_fixed_is_reported_but_not_acted_on() {
        // Shrinking the picture does not shorten a round trip. Doing it anyway
        // would cost quality for no benefit at all.
        let mut c = AdaptiveController::new(LatencyPriority::LATENCY, ceiling(), NATIVE);
        let distant = Observation {
            guest: Some(GuestTelemetry { rtt_ms: 220.0, ..healthy().guest.unwrap() }),
            ..healthy()
        };

        let changes = drive(&mut c, &distant, 30);
        assert!(c.bottlenecks().contains(Bottleneck::HighLatency));
        assert!(changes.is_empty(), "a long round trip is not a reason to degrade the stream");
    }

    #[test]
    fn each_stage_is_identified_separately() {
        let mut c = AdaptiveController::new(LatencyPriority::BALANCED, ceiling(), NATIVE);
        let bad = Observation {
            capture_ms: 50.0,
            encode_ms: 50.0,
            send_blocked_ms: 50.0,
            achieved_fps: 10.0,
            guest: Some(GuestTelemetry {
                decode_ms: 50.0,
                render_ms: 50.0,
                displayed_fps: 5.0,
                decoded_fps: 5.0,
                rtt_ms: 300.0,
            }),
        };
        c.observe_at(&bad, Instant::now());

        for b in Bottleneck::ALL {
            assert!(c.bottlenecks().contains(b), "{} was not detected", b.id());
        }
        assert_eq!(c.bottlenecks().iter().count(), Bottleneck::ALL.len());
    }

    #[test]
    fn raising_the_ceiling_clears_accumulated_degradation() {
        // Someone who just turned the quality up must see it, not discover the
        // controller is still holding them at a penalty from five minutes ago.
        let mut c = AdaptiveController::new(LatencyPriority::BALANCED, ceiling(), NATIVE);
        drive(&mut c, &Observation { send_blocked_ms: 40.0, ..healthy() }, 20);
        assert!(c.is_degraded());

        c.set_ceiling(StreamPlan { fps: 120, ..ceiling() });
        assert!(!c.is_degraded());
        assert_eq!(c.plan().fps, 120);
    }

    #[test]
    fn the_bottleneck_set_round_trips_through_its_wire_form() {
        let mut set = BottleneckSet::NONE;
        set.insert(Bottleneck::SlowNetwork);
        set.insert(Bottleneck::HighLatency);
        assert_eq!(BottleneckSet::from_bits(set.bits()), set);

        // A newer peer may set bits this build has never heard of; they are
        // dropped rather than showing up as some unrelated icon.
        assert_eq!(BottleneckSet::from_bits(0xFF).iter().count(), Bottleneck::ALL.len());
    }

    #[test]
    fn every_bottleneck_explains_itself_and_says_whether_it_is_fixable() {
        let mut ids: Vec<_> = Bottleneck::ALL.iter().map(|b| b.id()).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "bottleneck ids must be unique — the UI keys icons off them");

        for b in Bottleneck::ALL {
            assert!(!b.label().is_empty());
            assert!(!b.advice().is_empty());
        }
        assert!(!Bottleneck::HighLatency.is_actionable());
        assert!(!Bottleneck::SlowCapture.is_actionable());
        assert!(Bottleneck::SlowNetwork.is_actionable());
    }

    /// The ladder has to reach a rate a distant link can actually hold.
    ///
    /// This used to assert the opposite — that the bitrate never fell below a
    /// fifth of the ceiling — and that floor was the reason a hundred-millisecond
    /// link never settled. A 20 Mbit/s ceiling has a 4 Mbit/s fifth, which is not
    /// a rate that link was ever going to carry, so the controller reached the
    /// bottom of its ladder with the picture still breaking up.
    #[test]
    fn the_bitrate_floor_is_low_enough_to_be_a_floor() {
        let plan = StreamPlan { bitrate_kbps: 20_000, ..ceiling() };
        let mut c = AdaptiveController::new(LatencyPriority::BALANCED, plan, NATIVE);
        drive(&mut c, &Observation { send_blocked_ms: 100.0, ..healthy() }, 60);

        assert!(
            c.plan().bitrate_kbps <= plan.bitrate_kbps / 10,
            "a link this far over budget must be able to get below a tenth of the ceiling, \
             not {} kbps",
            c.plan().bitrate_kbps
        );
        // But still a picture rather than nothing.
        assert!(c.plan().bitrate_kbps >= BITRATE_FLOOR_KBPS);
    }

    /// A ceiling below the absolute floor is honoured rather than raised.
    ///
    /// Someone who caps the stream at 300 kbit/s means it, and quietly sending
    /// 400 would break the one promise a metered link cares about.
    #[test]
    fn a_ceiling_under_the_floor_is_not_quietly_raised() {
        let plan = StreamPlan { bitrate_kbps: 300, ..ceiling() };
        let mut c = AdaptiveController::new(LatencyPriority::BALANCED, plan, NATIVE);
        drive(&mut c, &Observation { send_blocked_ms: 100.0, ..healthy() }, 60);
        assert!(c.plan().bitrate_kbps <= 300);
    }

    /// Moving the slider mid-session does not undo what the link has taught.
    ///
    /// A settings change is a change of preference, not of circumstance. Reset
    /// the degradation and the stream jumps back to a ceiling already known not
    /// to fit, floods the link, and has to walk all the way back down — a
    /// visible lurch every time someone nudges the slider while comparing
    /// settings, which is exactly when they are nudging it most.
    #[test]
    fn changing_the_slider_does_not_restart_from_the_ceiling() {
        let mut c = AdaptiveController::new(LatencyPriority::BALANCED, ceiling(), NATIVE);
        drive(&mut c, &Observation { send_blocked_ms: 100.0, ..healthy() }, 30);
        assert!(c.is_degraded(), "the test needs the link to have pushed back first");
        let settled = c.plan();

        c.set_priority(LatencyPriority::QUALITY);
        assert!(c.is_degraded(), "the ground already given up must stay given up");
        assert_ne!(c.plan(), ceiling(), "a preference change must not undo the degradation");
        // The plan may still shift a little, and should: the floors move with
        // the slider, so the quality end permits a lower frame rate than the
        // balanced one and the steps already taken now reach further. What must
        // not happen is starting again from the top.
        assert_eq!(c.plan().scale, settled.scale);
        assert_eq!(c.plan().bitrate_kbps, settled.bitrate_kbps);
        assert!(c.plan().fps <= settled.fps);
    }

    /// The ceiling, by contrast, *is* a new statement of intent.
    #[test]
    fn raising_the_ceiling_does_start_again() {
        let mut c = AdaptiveController::new(LatencyPriority::BALANCED, ceiling(), NATIVE);
        drive(&mut c, &Observation { send_blocked_ms: 100.0, ..healthy() }, 30);
        assert!(c.is_degraded());

        c.set_ceiling(StreamPlan { bitrate_kbps: 40_000, ..ceiling() });
        assert!(!c.is_degraded(), "a new ceiling is asked for, so it is tried");
    }

    /// A picture nobody can afford is made smaller, whatever the slider says.
    ///
    /// # The session this comes from
    ///
    /// 517 kbit/s, 8 fps, still sending three quarters of a 2560x1600 display —
    /// 0.03 bits a pixel. The complaint was that the image was heavily
    /// compressed, and it was: the controller had spent every axis it was
    /// willing to spend and was holding a pixel count it could not pay for.
    ///
    /// The quality end of the slider refuses to shrink below `High`. That
    /// preference is honoured right up to the point where honouring it makes the
    /// picture worse than shrinking would, which is what this pins.
    #[test]
    fn a_starved_picture_is_shrunk_even_at_the_quality_end() {
        let mut c = AdaptiveController::new(LatencyPriority::QUALITY, ceiling(), NATIVE);
        assert_eq!(LatencyPriority::QUALITY.min_scale(), QualityScale::High);

        // A link far over budget, for long enough to run out of the other axes.
        drive(&mut c, &Observation { send_blocked_ms: 100.0, ..healthy() }, 80);

        let plan = c.plan();
        assert!(
            plan.scale.factor() < QualityScale::High.factor(),
            "still at {:?} on {} kbps — the scale floor was held past the point of use",
            plan.scale,
            plan.bitrate_kbps
        );

        // And the result is a picture worth sending rather than one worth
        // counting.
        let (w, h) = plan.scale.apply(NATIVE);
        let bpp = plan.bitrate_kbps as f64 * 1000.0
            / (w as f64 * h as f64 * plan.fps.max(1) as f64);
        assert!(
            bpp >= STARVED_BITS_PER_PIXEL,
            "{w}x{h} at {} fps on {} kbps is {bpp:.4} bits a pixel",
            plan.fps,
            plan.bitrate_kbps
        );
    }

    /// And a healthy link is not shrunk by the same rule.
    #[test]
    fn a_stream_that_can_pay_for_its_pixels_keeps_them() {
        let mut c = AdaptiveController::new(LatencyPriority::QUALITY, ceiling(), NATIVE);
        drive(&mut c, &healthy(), 40);
        assert_eq!(c.plan(), ceiling(), "nothing was wrong and nothing should have changed");
    }

    /// A link that keeps failing at the higher setting stops being asked.
    ///
    /// Without this, a marginal link spends the whole session climbing a step,
    /// failing, and dropping back — which the user sees as the picture pulsing,
    /// and is more distracting than sitting one step low.
    #[test]
    fn a_recovery_that_has_to_be_undone_makes_the_next_one_slower() {
        let mut c = AdaptiveController::new(LatencyPriority::BALANCED, ceiling(), NATIVE);
        let strained = Observation { send_blocked_ms: 40.0, ..healthy() };
        let mut clock = Instant::now();

        let mut feed = |c: &mut AdaptiveController, obs: &Observation, windows: usize| {
            for _ in 0..windows {
                clock += MIN_DWELL + Duration::from_millis(1);
                c.observe_at(obs, clock);
            }
        };

        // Degrade, recover a step, then fail again — twice.
        feed(&mut c, &strained, 12);
        let base = c.patience();
        for _ in 0..2 {
            let clean = c.patience() as usize + 1;
            feed(&mut c, &healthy(), clean);
            feed(&mut c, &strained, 4);
        }

        assert!(
            c.patience() > base,
            "patience {} did not grow past {base} after two failed recoveries",
            c.patience()
        );
    }
    /// A guest that draws slower than it decodes is not a slow link.
    ///
    /// The session this comes from: a host climbing to 58 fps on a 30 Mbit/s
    /// link, send path never blocking, guest presenting what a 60 Hz monitor
    /// can present. Judged on what was drawn that reads as a fifth of the
    /// frames lost, and the controller spent ninety seconds collapsing to 17
    /// fps and 933 kbit/s. Nothing was lost.
    #[test]
    fn a_guest_that_cannot_paint_fast_enough_is_not_a_slow_network() {
        let mut obs = healthy();
        obs.achieved_fps = 58.0;
        if let Some(guest) = obs.guest.as_mut() {
            // Every frame arrived and decoded...
            guest.decoded_fps = 58.0;
            // ...and the window painted rather fewer of them.
            guest.displayed_fps = 44.0;
        }

        let mut controller = AdaptiveController::new(LatencyPriority::BALANCED, ceiling(), NATIVE);
        drive(&mut controller, &obs, 6);
        assert!(
            !controller.bottlenecks().contains(Bottleneck::SlowNetwork),
            "frames that decoded were not lost, whatever the repaint rate",
        );
    }

    /// And frames that genuinely never arrive still say so.
    #[test]
    fn frames_that_never_decoded_are_still_a_slow_network() {
        let mut obs = healthy();
        obs.achieved_fps = 58.0;
        if let Some(guest) = obs.guest.as_mut() {
            // Half of what the host sent never turned up at all.
            guest.decoded_fps = 25.0;
            guest.displayed_fps = 25.0;
        }

        let mut controller = AdaptiveController::new(LatencyPriority::BALANCED, ceiling(), NATIVE);
        drive(&mut controller, &obs, 6);
        assert!(controller.bottlenecks().contains(Bottleneck::SlowNetwork));
    }
}
