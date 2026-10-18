//! A time limit for tests that call into a graphics driver.
//!
//! # Why a test needs a watchdog at all
//!
//! The hardware encoder tests block, rarely and unpredictably, somewhere inside
//! Media Foundation. Measured on an AMD card over several hundred runs of the
//! `tain-core` suite: roughly one run in ten stops with every test reported
//! except one of [`super::codec_mf`]'s, at no CPU, and stays there. It is not a
//! deadlock this repository can fix — the encode loop itself is guarded by
//! `ENCODER_STALL_TIMEOUT` and polls rather than blocks, so what is left is the
//! driver's own `ActivateObject` and `ShutdownObject`, which are opaque
//! blocking calls into somebody else's code.
//!
//! What *can* be fixed is what a stall costs. Without this, `cargo test` waits
//! for it forever: the run never ends, the harness holds every result it has
//! collected, and the only evidence is a process sitting at 0% CPU. Twice that
//! has meant a test binary left running for an hour, still holding the build
//! lock, with nothing on screen saying why.
//!
//! # Why it aborts rather than fails
//!
//! A blocked thread inside a driver cannot be unwound — there is no way to make
//! `ShutdownObject` return, and nothing to poison. Aborting ends the run with a
//! named cause in a bounded time, which is the whole difference between "this
//! is a known driver stall in `a_frame_may_change_format_mid_stream`" and "the
//! tests hang sometimes". The other tests in flight are lost, and that is a far
//! smaller price than the run never finishing.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Long enough that a busy machine never trips it, short enough to notice.
///
/// The tests it guards take well under a second each; a GPU already encoding
/// for something else can make that several. Sixty seconds is not a judgement
/// about how slow the work can be, it is the point past which the work is not
/// happening at all.
const LIMIT: Duration = Duration::from_secs(60);

/// Abort the process, naming `what`, if this guard has not been dropped in time.
///
/// Hold it for as long as the driver calls last:
///
/// ```ignore
/// let _watchdog = watchdog::limit("a_frame_may_change_format_mid_stream");
/// ```
#[must_use = "the limit ends the moment the guard is dropped"]
pub fn limit(what: &'static str) -> Guard {
    after(what, LIMIT)
}

/// [`limit`] with the deadline spelled out, so the mechanism can be shown
/// working without a test that takes a minute to do it.
#[must_use = "the limit ends the moment the guard is dropped"]
pub fn after(what: &'static str, deadline: Duration) -> Guard {
    let done = Arc::new((Mutex::new(false), Condvar::new()));
    let waiting = Arc::clone(&done);
    let stage: Arc<Mutex<&'static str>> = Arc::new(Mutex::new("before anything was marked"));
    let reached = Arc::clone(&stage);

    std::thread::Builder::new()
        .name(format!("watchdog {what}"))
        .spawn(move || {
            let (finished, wake) = &*waiting;
            let started = std::time::Instant::now();
            let mut finished = finished.lock().unwrap_or_else(|e| e.into_inner());
            // A condvar can wake for no reason, so the deadline is checked
            // rather than assumed from one return.
            while !*finished {
                let left = deadline.saturating_sub(started.elapsed());
                if left.is_zero() {
                    // Written and flushed before the abort, not printed by it.
                    // `abort` does not unwind and does not run at-exit
                    // handlers, so anything still sitting in a buffer when it
                    // is called is simply lost — which is how the first version
                    // of this ended a run with no explanation at all.
                    use std::io::Write;
                    // Read without blocking on it: the stuck thread is not
                    // holding this lock — it marks a stage and lets go — but a
                    // watchdog that could itself hang would be worthless.
                    let last = reached
                        .try_lock()
                        .map(|stage| *stage)
                        .unwrap_or("could not be read");
                    let mut err = std::io::stderr().lock();
                    let _ = write!(
                        err,
                        "\nwatchdog: {what} has been stuck for {deadline:?} and is not coming \
                         back.\n\
                         It got as far as: {last}\n\
                         This is the Media Foundation stall described in \
                         tain-core/src/watchdog.rs — a blocking call into the graphics \
                         driver that never returns. Aborting so the run ends with a cause \
                         rather than waiting for it forever.\n\n"
                    );
                    let _ = err.flush();
                    drop(err);
                    // Straight out, without unwinding: the stuck thread is
                    // inside a driver and will not be unwound by anything.
                    std::process::abort();
                }
                let (guard, _) = wake
                    .wait_timeout(finished, left)
                    .unwrap_or_else(|e| e.into_inner());
                finished = guard;
            }
        })
        .expect("a watchdog thread");

    Guard { done, stage }
}

/// Ends the limit when it drops.
pub struct Guard {
    done: Arc<(Mutex<bool>, Condvar)>,
    stage: Arc<Mutex<&'static str>>,
}

impl Guard {
    /// Say what is about to be attempted, for the watchdog to quote if it fires.
    ///
    /// # Why a breadcrumb rather than a stack trace
    ///
    /// The thread this bounds is blocked inside a graphics driver, and there is
    /// no way to ask it where from: it is not unwinding, the frames are in a
    /// module with no symbols, and every Windows API for reading another
    /// thread's stack needs a debugger attached to a process that is, by then,
    /// already wedged. A line marked before each call costs nothing and answers
    /// the only question that matters — which call it was.
    pub fn at(&self, stage: &'static str) {
        *self.stage.lock().unwrap_or_else(|e| e.into_inner()) = stage;
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let (finished, wake) = &*self.done;
        *finished.lock().unwrap_or_else(|e| e.into_inner()) = true;
        wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Work that finishes is left alone.
    ///
    /// There is no way to test the abort from inside the process it aborts, so
    /// what is testable is the other half: that a guard taken and released many
    /// times over is silent, and that reaching the end of this test at all means
    /// none of them fired.
    #[test]
    fn work_that_finishes_is_left_alone() {
        for _ in 0..50 {
            drop(limit("a test that returns promptly"));
        }
    }

    /// The other half, which can only be run on its own.
    ///
    /// A watchdog that fires ends the process, so this cannot sit in the suite
    /// — but the firing is the whole point of the module and a mechanism nobody
    /// has ever seen work is not one to rely on. Run it deliberately:
    ///
    /// ```text
    /// cargo test -p tain-core --lib watchdog -- --ignored --nocapture
    /// ```
    ///
    /// and expect the process to abort within a couple of seconds, having said
    /// which name it was given.
    #[test]
    #[ignore = "aborts the process on purpose — that is the thing being shown"]
    fn a_watchdog_that_fires_says_so_before_it_goes() {
        let _limit = after("a test that deliberately overruns its watchdog", Duration::from_secs(1));
        std::thread::sleep(Duration::from_secs(30));
        unreachable!("the watchdog should have ended this process");
    }

    /// Dropping the guard has to wake the waiter, or every guarded test leaves
    /// a thread asleep for a minute behind it — and the encoder tests take one
    /// each.
    #[test]
    fn dropping_the_guard_releases_its_thread() {
        let guard = limit("a test measuring its own watchdog");
        let done = Arc::clone(&guard.done);
        drop(guard);

        assert!(*done.0.lock().unwrap(), "the watchdog was never told to stand down");

        // The thread holds a clone until it returns, and that happens after the
        // notify rather than during it. Waited for rather than assumed, because
        // asserting the count immediately is a race that passes on a quiet
        // machine and fails on a busy one.
        let freed = std::time::Instant::now();
        while Arc::strong_count(&done) > 1 {
            assert!(
                freed.elapsed() < Duration::from_secs(5),
                "the watchdog thread is still holding on"
            );
            std::thread::yield_now();
        }
    }
}
