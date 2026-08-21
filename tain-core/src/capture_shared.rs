//! One desktop duplication, shared by every session.
//!
//! ## Why this exists
//!
//! DXGI allows a process **one** duplication of a given output. A second
//! `DuplicateOutput` returns `E_INVALIDARG` — measured, not assumed; see
//! `capture_dxgi::tests::what_a_second_duplication_does` — and it succeeds again
//! the moment the first one is dropped.
//!
//! Sessions used to each build their own capturer, so the second guest's
//! duplication was refused and `create_capturer` quietly handed it the animated
//! test pattern instead. Reconnecting before the previous session had finished
//! tearing down did the same thing, which is why it looked timing-dependent. In
//! both cases the guest saw a plausible moving picture that was not the host's
//! screen at all.
//!
//! ## The shape of the fix
//!
//! One thread owns the duplication and pumps it. Every session subscribes and is
//! handed each frame as it arrives — a [`ScreenFrame`] clone is a refcount bump,
//! not a copy of sixteen megabytes.
//!
//! Latency is unchanged for the single-session case, which is the one that
//! matters most. The pump blocks inside `AcquireNextFrame` exactly as the
//! session loop used to, and subscribers block on a condition variable that the
//! pump signals the instant a frame lands.
//!
//! ## The sessions are the clock
//!
//! The pump captures only while a subscriber is waiting for a frame, and does no
//! pacing of its own. That is not an optimisation, it is the difference between
//! a smooth stream and a stuttering one: a pump on its own timer is a *second*
//! clock running at nominally the same rate as the session loop's, and two
//! free-running clocks drift through each other. Every time they cross, a
//! session waits nearly an extra period for a frame — a hitch on the beat
//! frequency, with a healthy link and idle hardware. See [`Desktop::pump`].
//!
//! ## What sharing costs
//!
//! Resolution and pixel format become one decision for everybody, because there
//! is one converter producing one set of frames. The first session to attach
//! owns them; later ones are told no, which is a case
//! [`ScreenCaptureBackend::set_scale`] already defines — a controller that
//! cannot shed pixels sheds frame rate and bitrate instead, and those stay
//! per-session. Ownership passes to a remaining session when the owner leaves,
//! so a long-lived second guest is not stuck at whatever the first one chose.

use anyhow::{Context, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use tracing::*;

use super::capture_dxgi::DxgiCapturer;
use super::cursor::CursorUpdate;
use super::screen_capture::{PixelFormat, QualityScale, ScreenCaptureBackend, ScreenFrame};

/// How long a subscriber waits for a frame before reporting that there is none.
///
/// Comfortably above the duplication's own 16 ms acquire timeout, so a wait that
/// ends here means the pump itself is wedged rather than that the desktop simply
/// has not changed. Returning `None` is not an error either way — it is what the
/// session loop expects from a still screen.
const WAIT: std::time::Duration = std::time::Duration::from_millis(25);

/// Cursor updates kept for a subscriber that has not collected them.
///
/// Small. The session loop takes these every frame, so a backlog means that
/// session has stopped reading, and holding thousands of stale pointer positions
/// for it would help nobody.
const CURSOR_BACKLOG: usize = 64;

/// Floor on one pump iteration, so a lost duplication cannot spin a core.
///
/// Never reached while capture is healthy: the acquire blocks for longer than
/// this on its own whenever the desktop is not changing.
const MIN_ITERATION: std::time::Duration = std::time::Duration::from_millis(4);

/// How often the idle pump re-checks whether it has been told to stop.
const IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// The live duplication, if any session has one open.
///
/// `Weak`, so the last session leaving really does release it — a held
/// duplication is not merely idle, it is the thing that makes the *next* one
/// fail.
static DESKTOP: Mutex<Weak<Desktop>> = Mutex::new(Weak::new());

/// Attach to the shared duplication, starting it if this is the first session.
pub fn attach() -> Result<Box<dyn ScreenCaptureBackend>> {
    // Held across both the upgrade and the subscriber count so that attaching
    // cannot race the last subscriber's departure. See [`Subscriber::drop`].
    let mut slot = DESKTOP.lock().unwrap_or_else(|e| e.into_inner());
    let desktop = match slot.upgrade() {
        Some(desktop) => desktop,
        None => {
            let desktop = Desktop::start()?;
            *slot = Arc::downgrade(&desktop);
            desktop
        }
    };
    Ok(Box::new(Subscriber::new(desktop)))
}

/// Frames and pointer updates the pump has published.
struct Published {
    /// The newest frame. Subscribers take it when `generation` moves past the
    /// one they last saw, so a session that falls behind skips to the current
    /// picture rather than working through stale ones.
    frame: Option<ScreenFrame>,
    generation: u64,
    /// Pointer updates per subscriber. Not a single shared queue: these are
    /// discrete events, and a subscriber that missed a *shape* would draw the
    /// wrong pointer until the next time it changed.
    cursors: HashMap<u64, VecDeque<CursorUpdate>>,
    /// Subscribers currently blocked waiting for a frame.
    ///
    /// The pump captures only while this is non-zero, which is what makes the
    /// sessions the clock rather than the pump. See [`Desktop::pump`].
    waiting: u32,
    /// Emitted frame size, mirrored here so `frame_size` never has to wait for
    /// the pump to come out of an acquire.
    size: (u16, u16),
    /// Why the pump stopped, if it did. Reported to every subscriber — a
    /// duplication that cannot be rebuilt is the session's problem, not
    /// something to hide behind an endless run of `None`.
    failed: Option<String>,
}

struct Desktop {
    /// The capturer itself. Held by the pump for the length of one acquire, and
    /// by control calls ([`Subscriber::set_scale`] and friends) for as long as
    /// it takes to reconfigure — which is why those answer honestly instead of
    /// posting a request and hoping.
    capturer: Mutex<DxgiCapturer>,
    published: Mutex<Published>,
    /// Signalled by the pump on every wake-up, frame or not: a pointer move on a
    /// still desktop is also something a subscriber is waiting for.
    woken: Condvar,
    /// Signalled by a subscriber that has started waiting, so the pump wakes up
    /// and captures. The other half of "the consumers are the clock".
    wanted: Condvar,
    stopping: AtomicBool,
    subscribers: AtomicU64,
    /// Next subscriber id. Ids are never reused, so a departing subscriber
    /// cannot delete a newcomer's cursor queue.
    next_id: AtomicU64,
    /// The subscriber whose scale, ceiling and format requests are honoured.
    owner: Mutex<Option<u64>>,
    pump: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Desktop {
    fn start() -> Result<Arc<Self>> {
        // Built on the pump thread, not here, so the D3D device and the
        // duplication are created and used on the same thread for their whole
        // life. Handing a live one across threads works, but only by accident of
        // how it is used, and this removes the question.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<DxgiCapturer>>();
        let (start_tx, start_rx) = std::sync::mpsc::channel::<Arc<Desktop>>();

        let handle = std::thread::Builder::new()
            .name("desktop-capture".into())
            .spawn(move || {
                let capturer = DxgiCapturer::new();
                let failed = capturer.is_err();
                if ready_tx.send(capturer).is_err() || failed {
                    return;
                }
                // Blocks until the caller below has the `Arc` to give us. If it
                // fails, the caller went away and there is nothing to pump.
                if let Ok(desktop) = start_rx.recv() {
                    desktop.pump();
                }
            })
            .context("could not start the desktop capture thread")?;

        let capturer = ready_rx
            .recv()
            .context("the desktop capture thread stopped before reporting")??;
        let size = capturer.frame_size();

        let desktop = Arc::new(Self {
            capturer: Mutex::new(capturer),
            published: Mutex::new(Published {
                frame: None,
                generation: 0,
                cursors: HashMap::new(),
                waiting: 0,
                size,
                failed: None,
            }),
            woken: Condvar::new(),
            wanted: Condvar::new(),
            stopping: AtomicBool::new(false),
            subscribers: AtomicU64::new(0),
            next_id: AtomicU64::new(1),
            owner: Mutex::new(None),
            pump: Mutex::new(None),
        });

        // Stored before the thread is released, so a subscriber that leaves
        // immediately still has something to join.
        *desktop.pump.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);

        // The thread holds an `Arc` of its own, which is why the registry above
        // keeps only a `Weak`: the pump must not be what keeps the duplication
        // alive after the last session has gone.
        if start_tx.send(Arc::clone(&desktop)).is_err() {
            anyhow::bail!("the desktop capture thread stopped before it could start");
        }
        info!("Desktop capture started at {}x{}", size.0, size.1);
        Ok(desktop)
    }

    fn pump(self: Arc<Self>) {
        while !self.stopping.load(Ordering::Acquire) {
            // # The consumers are the clock, not this loop
            //
            // This used to run on a timer of its own, at whatever rate the most
            // demanding session asked for. That was wrong, and it was wrong in a
            // way that is invisible until you look for it: the session loop
            // *also* paces itself, so there were two free-running clocks at
            // nominally the same rate. They drift through each other, and every
            // time they cross, a session's tick lands just before the pump's
            // publish and waits nearly a whole extra period for a frame.
            //
            // The result is a frame interval that breathes in and out on the
            // beat frequency of the two clocks — stutter with a healthy link, a
            // fast encoder and a fast decoder, which is exactly what was
            // reported. Before the pump existed, the session's own tick was the
            // only clock and this could not happen.
            //
            // So the pump captures only while somebody is waiting for a frame.
            // One clock again — the session's — and no over-capturing either,
            // because nothing is produced that nobody asked for.
            let started = std::time::Instant::now();
            {
                let mut published = self.published.lock().unwrap_or_else(|e| e.into_inner());
                while published.waiting == 0 && !self.stopping.load(Ordering::Acquire) {
                    let (guard, _) = self
                        .wanted
                        .wait_timeout(published, IDLE_POLL)
                        .unwrap_or_else(|e| e.into_inner());
                    published = guard;
                }
            }
            if self.stopping.load(Ordering::Acquire) {
                break;
            }

            let (frame, cursor) = {
                let mut capturer = self.capturer.lock().unwrap_or_else(|e| e.into_inner());
                // Blocks here, inside the lock, for up to one acquire timeout.
                // Control calls wait that long and no longer.
                let frame = capturer.capture_next();
                let cursor = capturer.take_cursor();
                let size = capturer.frame_size();
                (frame.map(|f| (f, size)), cursor)
            };

            let mut published = self.published.lock().unwrap_or_else(|e| e.into_inner());
            match frame {
                Ok((frame, size)) => {
                    published.size = size;
                    if let Some(frame) = frame {
                        published.frame = Some(frame);
                        published.generation += 1;
                    }
                }
                Err(e) => {
                    // `DxgiCapturer` already retries a lost duplication itself
                    // and only surfaces what it could not recover from.
                    error!("Desktop capture stopped: {e:#}");
                    published.failed = Some(format!("{e:#}"));
                    self.woken.notify_all();
                    return;
                }
            }
            if let Some(cursor) = cursor {
                for queue in published.cursors.values_mut() {
                    if queue.len() >= CURSOR_BACKLOG {
                        queue.pop_front();
                    }
                    queue.push_back(cursor.clone());
                }
            }
            drop(published);
            self.woken.notify_all();

            // A floor on the iteration, and *only* a floor.
            //
            // A healthy acquire already blocks for up to its own timeout waiting
            // for the desktop to change, so this never fires. It exists for the
            // case where there is nothing to block on: a duplication lost to a
            // UAC prompt or the lock screen returns instantly, and without this
            // the loop would spin a core flat for as long as the prompt was up.
            //
            // Deliberately not a frame-rate pacer. Pacing here is what put a
            // second clock in the pipeline; see the top of this loop.
            if let Some(remaining) = MIN_ITERATION.checked_sub(started.elapsed()) {
                std::thread::sleep(remaining);
            }
        }
    }

    /// Stop the pump and wait for it, so the duplication is really gone.
    ///
    /// Waiting is the point. Returning while the thread still holds the
    /// duplication is exactly the reconnect race this module exists to remove —
    /// the next session would ask for a duplication that is a millisecond away
    /// from being free and be refused.
    fn stop(&self) {
        self.stopping.store(true, Ordering::Release);
        let handle = self.pump.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

/// One session's view of the shared duplication.
struct Subscriber {
    desktop: Arc<Desktop>,
    id: u64,
    /// The newest generation this session has taken.
    seen: u64,
}

impl Subscriber {
    fn new(desktop: Arc<Desktop>) -> Self {
        let id = desktop.next_id.fetch_add(1, Ordering::Relaxed);
        desktop.subscribers.fetch_add(1, Ordering::Relaxed);
        {
            let mut published = desktop.published.lock().unwrap_or_else(|e| e.into_inner());
            published.cursors.insert(id, VecDeque::new());
        }
        {
            let mut owner = desktop.owner.lock().unwrap_or_else(|e| e.into_inner());
            if owner.is_none() {
                *owner = Some(id);
            }
        }
        // Start from the generation before the current one, so a session joining
        // a machine whose screen is not changing gets the last frame straight
        // away rather than staring at black until something moves.
        let seen = {
            let published = desktop.published.lock().unwrap_or_else(|e| e.into_inner());
            published.generation.saturating_sub(1)
        };
        Self { desktop, id, seen }
    }

    /// Whether this session's configuration requests are the ones that apply.
    fn owns_configuration(&self) -> bool {
        *self.desktop.owner.lock().unwrap_or_else(|e| e.into_inner()) == Some(self.id)
    }
}

impl ScreenCaptureBackend for Subscriber {
    fn capture_next(&mut self) -> Result<Option<ScreenFrame>> {
        let mut published = self.desktop.published.lock().unwrap_or_else(|e| e.into_inner());

        // Registering the wait is what asks the pump to capture at all. It has
        // to happen before the wait rather than after, or the pump sleeps
        // through a request nobody told it about.
        if published.generation == self.seen && published.failed.is_none() {
            published.waiting += 1;
            self.desktop.wanted.notify_all();
            while published.generation == self.seen && published.failed.is_none() {
                let (guard, timeout) = self
                    .desktop
                    .woken
                    .wait_timeout(published, WAIT)
                    .unwrap_or_else(|e| e.into_inner());
                published = guard;
                if timeout.timed_out() {
                    break;
                }
            }
            published.waiting -= 1;
        }

        if let Some(reason) = &published.failed {
            anyhow::bail!("{reason}");
        }
        if published.generation == self.seen {
            return Ok(None);
        }
        self.seen = published.generation;
        Ok(published.frame.clone())
    }

    /// The pointer's current state, not the next step of its history.
    ///
    /// # Why this drains rather than pops
    ///
    /// It used to return one queued update per call, and that turned the pointer
    /// into a recording played back at the wrong speed. The pump enqueues an
    /// update every time the duplication reports one, and a pointer moving over
    /// a *static* desktop wakes the acquire without producing a frame — so
    /// several updates pile up between one session tick and the next, while the
    /// session took exactly one of them.
    ///
    /// The result was a pointer trailing further and further behind the real
    /// one, crawling through positions it had left long ago, then jumping when
    /// the queue hit [`CURSOR_BACKLOG`] and the oldest were discarded. Reported,
    /// exactly, as "ridiculously slow and late motions, sometimes quickly
    /// speeding up to catch up".
    ///
    /// A position is **state**, not an event: only the newest one is worth
    /// anything, and the ones behind it describe where the pointer no longer is.
    /// A shape *is* an event — miss one and the wrong pointer is drawn until it
    /// next changes — so the most recent shape in the batch is carried onto the
    /// newest position rather than thrown away with it.
    fn take_cursor(&mut self) -> Option<CursorUpdate> {
        let mut published = self.desktop.published.lock().unwrap_or_else(|e| e.into_inner());
        let queue = published.cursors.get_mut(&self.id)?;

        let mut newest = queue.pop_front()?;
        while let Some(update) = queue.pop_front() {
            let shape = update.shape.or(newest.shape);
            newest = CursorUpdate { position: update.position, shape };
        }
        Some(newest)
    }

    fn frame_size(&self) -> (u16, u16) {
        self.desktop.published.lock().unwrap_or_else(|e| e.into_inner()).size
    }

    fn set_scale(&mut self, scale: QualityScale) -> bool {
        if !self.owns_configuration() {
            return false;
        }
        let mut capturer = self.desktop.capturer.lock().unwrap_or_else(|e| e.into_inner());
        let applied = capturer.set_scale(scale);
        if applied {
            let size = capturer.frame_size();
            drop(capturer);
            self.desktop.published.lock().unwrap_or_else(|e| e.into_inner()).size = size;
        }
        applied
    }

    fn set_size_ceiling(&mut self, ceiling: Option<(u16, u16)>) -> bool {
        if !self.owns_configuration() {
            return false;
        }
        let mut capturer = self.desktop.capturer.lock().unwrap_or_else(|e| e.into_inner());
        let applied = capturer.set_size_ceiling(ceiling);
        if applied {
            let size = capturer.frame_size();
            drop(capturer);
            self.desktop.published.lock().unwrap_or_else(|e| e.into_inner()).size = size;
        }
        applied
    }

    fn request_format(&mut self, format: PixelFormat) -> bool {
        let mut capturer = self.desktop.capturer.lock().unwrap_or_else(|e| e.into_inner());
        // A later session asking for what is already being produced is not a
        // configuration change, so it is answered whoever asks. Only an actual
        // switch is the owner's to make — flipping the format under a running
        // session would feed its encoder bytes it would read as the other thing.
        if capturer.current_format() == format {
            return true;
        }
        if !self.owns_configuration() {
            debug!("Capture is already producing {:?} — this session takes it", capturer.current_format());
            return false;
        }
        capturer.request_format(format)
    }
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        // The registry lock, not the subscriber count, is what makes this safe:
        // `attach` holds it while it upgrades and registers, so a session
        // arriving as this one leaves either finds a live desktop and joins it
        // or finds the slot already cleared and starts a fresh one. It cannot
        // find one that is halfway through shutting down.
        let mut slot = DESKTOP.lock().unwrap_or_else(|e| e.into_inner());

        {
            let mut published = self.desktop.published.lock().unwrap_or_else(|e| e.into_inner());
            published.cursors.remove(&self.id);
        }
        {
            let mut owner = self.desktop.owner.lock().unwrap_or_else(|e| e.into_inner());
            if *owner == Some(self.id) {
                // Hand the configuration to whoever is left, so a session that
                // outlives the first one is not pinned to its choices forever.
                let published = self.desktop.published.lock().unwrap_or_else(|e| e.into_inner());
                *owner = published.cursors.keys().copied().min();
            }
        }

        if self.desktop.subscribers.fetch_sub(1, Ordering::AcqRel) == 1 {
            *slot = Weak::new();
            // Still holding the registry lock, deliberately: the next session
            // must not be able to ask for a duplication until this one is
            // actually released, which is what `stop` waits for.
            self.desktop.stop();
            debug!("Last session left — desktop capture released");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two sessions at once, which is the case that used to hand the second one
    /// an animated test pattern.
    #[test]
    #[ignore = "requires an interactive desktop session"]
    fn two_sessions_both_get_the_real_desktop() {
        let mut first = attach().expect("first session");
        let mut second = attach().expect("second session");

        assert!(first.is_real() && second.is_real());
        assert_eq!(first.frame_size(), second.frame_size());

        // Both must see frames. A static desktop legitimately yields nothing,
        // so keep asking for a couple of seconds.
        let mut seen = [0, 0];
        for _ in 0..100 {
            if first.capture_next().unwrap().is_some() {
                seen[0] += 1;
            }
            if second.capture_next().unwrap().is_some() {
                seen[1] += 1;
            }
            if seen[0] > 2 && seen[1] > 2 {
                break;
            }
        }
        assert!(seen[0] > 2 && seen[1] > 2, "frames seen: {seen:?}");
    }

    /// The reconnect case: a session ending and another starting immediately.
    #[test]
    #[ignore = "requires an interactive desktop session"]
    fn a_session_can_start_the_instant_the_last_one_ends() {
        for round in 0..5 {
            let capturer = attach().unwrap_or_else(|e| panic!("round {round}: {e}"));
            assert!(capturer.is_real(), "round {round} did not get a real capture");
            drop(capturer);
        }
    }

    /// The pointer must arrive where it is, not where it was several moves ago.
    #[test]
    fn queued_pointer_moves_collapse_to_the_newest_position() {
        use crate::cursor::{CursorPosition, CursorShape};

        let Ok(desktop) = Desktop::start() else {
            eprintln!("no desktop duplication — skipping");
            return;
        };
        let mut subscriber = Subscriber::new(desktop);

        let at = |x: i32| CursorUpdate {
            position: CursorPosition {
                x,
                y: 0,
                visible: true,
                desktop_width: 1920,
                desktop_height: 1080,
            },
            shape: None,
        };
        let shape = CursorShape {
            id: 7,
            width: 2,
            height: 2,
            hotspot_x: 0,
            hotspot_y: 0,
            rgba: vec![0; 16],
        };

        {
            let mut published =
                subscriber.desktop.published.lock().unwrap_or_else(|e| e.into_inner());
            let queue = published.cursors.get_mut(&subscriber.id).expect("registered");
            queue.push_back(at(10));
            // The shape arrives partway through the batch, which is the case
            // that must not be lost.
            queue.push_back(CursorUpdate { shape: Some(shape.clone()), ..at(20) });
            queue.push_back(at(30));
        }

        let taken = subscriber.take_cursor().expect("something was queued");
        assert_eq!(taken.position.x, 30, "the pointer is where it is now, not where it was");
        assert_eq!(
            taken.shape.map(|s| s.id),
            Some(7),
            "a shape change in the middle of a batch must survive the collapse"
        );
        assert!(subscriber.take_cursor().is_none(), "the queue must be empty afterwards");
    }

    /// Only one session decides the size, and it is the one that arrived first.
    #[test]
    #[ignore = "requires an interactive desktop session"]
    fn the_second_session_does_not_get_to_resize_the_first() {
        let mut first = attach().expect("first session");
        let mut second = attach().expect("second session");

        assert!(!second.set_scale(QualityScale::Low), "a later session resized the capture");
        assert!(first.set_scale(QualityScale::Low), "the owning session could not resize");
        assert_eq!(first.frame_size(), second.frame_size(), "sessions disagree on the size");

        // Ownership passes on when the owner leaves.
        drop(first);
        assert!(second.set_scale(QualityScale::Native), "ownership did not pass on");
    }
}
