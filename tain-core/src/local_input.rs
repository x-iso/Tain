//! Noticing that somebody is sitting at this machine.
//!
//! # What this is for
//!
//! A host being streamed is still a computer somebody may be using. When they
//! reach for the mouse, the guest's pointer should not be fighting them for it
//! — two hands on one cursor is not a shared session, it is neither person
//! being able to do anything.
//!
//! So remote input gives way for a moment whenever local input happens. The
//! moment is short, because the point is to yield the machine rather than to
//! take it away: a guest driving a host that nobody is touching notices
//! nothing at all.
//!
//! # Why a low-level hook and not `GetLastInputInfo`
//!
//! `GetLastInputInfo` is one call and answers exactly the question this module
//! asks — when did input last happen — and it cannot be used, for one reason:
//! it counts *injected* input too. Everything the guest sends arrives through
//! `SendInput`, so the last input time would follow the guest's own events, the
//! host would look permanently busy, and remote input would block itself
//! forever the moment anybody used it.
//!
//! A `WH_MOUSE_LL` / `WH_KEYBOARD_LL` hook sees a flag the API does not expose:
//! `LLMHF_INJECTED` and `LLKHF_INJECTED`, set on anything a program produced.
//! Ignoring those leaves exactly the events a person made with their hands,
//! which is the question.
//!
//! # What it costs
//!
//! One thread, which sleeps in `GetMessageW` and wakes on input; two hook
//! callbacks that read one flag and store one number. Low-level hooks run on
//! *every* event in the system, so the callbacks do nothing else — a hook that
//! takes too long is silently removed by Windows, and a removed hook is a
//! feature that stops working with no error anywhere.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// When the process started, so instants can live in an atomic as a number.
static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Milliseconds since [`EPOCH`] at the last local input. Zero means none seen.
static LAST: AtomicU64 = AtomicU64::new(0);

/// How long remote input gives way for, in milliseconds.
///
/// # Why this lives here and not in a session's configuration
///
/// It is a property of the machine rather than of a connection: there is one
/// keyboard, one person who may be sitting at it, and one answer to how long
/// they should have it to themselves. Two guests cannot each have their own
/// version of that.
///
/// Keeping it here also means changing it takes effect at once, on every
/// session already running, without the value having to be carried down through
/// each of them.
static WINDOW_MS: AtomicU64 = AtomicU64::new(DEFAULT_WINDOW_MS);

/// Two seconds. Long enough that a sentence typed at the host is not
/// interrupted between words, short enough that a guest whose host got up and
/// left is not locked out for any noticeable time.
pub const DEFAULT_WINDOW_MS: u64 = 2000;

/// How long remote input currently gives way for.
pub fn window() -> Duration {
    Duration::from_millis(WINDOW_MS.load(Ordering::Relaxed))
}

/// Change it. Takes effect on the next event, in every session.
pub fn set_window(window: Duration) {
    WINDOW_MS.store(window.as_millis().min(u64::MAX as u128) as u64, Ordering::Relaxed);
}

fn epoch() -> Instant {
    *EPOCH.get_or_init(Instant::now)
}

/// Record that somebody just used this machine.
///
/// Called from the hooks, and from the tests of what acts on it — which is why
/// it is reachable inside the crate rather than private to the platform half.
pub(crate) fn seen() {
    let millis = epoch().elapsed().as_millis().min(u64::MAX as u128) as u64;
    // Saturated away from zero, which is the "nothing seen" value: an event in
    // the first millisecond of the process is still an event.
    LAST.store(millis.max(1), Ordering::Relaxed);
}

/// How long since the person at this machine last touched it.
///
/// `None` when nothing has been seen — a machine nobody is at, or one where
/// the watcher could not start. Both mean the same thing to a caller: there is
/// nobody to give way to.
pub fn quiet_for() -> Option<Duration> {
    let millis = LAST.load(Ordering::Relaxed);
    if millis == 0 {
        return None;
    }
    Some(epoch().elapsed().saturating_sub(Duration::from_millis(millis)))
}

/// Whether somebody has used this machine within `window`.
///
/// Pure apart from the clock, and the whole of the decision — see
/// [`crate::input_inject::deferring`], which is what acts on it.
pub fn active_within(window: Duration) -> bool {
    // A zero window turns the feature off, and must not be defeated by a clock
    // that has not ticked since the last event.
    !window.is_zero() && quiet_for().is_some_and(|quiet| quiet < window)
}

/// Start watching. Safe to call repeatedly; only the first one does anything.
pub fn watch() {
    let _ = epoch();
    #[cfg(target_os = "windows")]
    windows_impl::start();
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::seen;
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, SetWindowsHookExW, HC_ACTION, KBDLLHOOKSTRUCT,
        LLKHF_INJECTED, MSG, MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL,
    };

    /// `LLMHF_INJECTED`, which the crate does not name. Set on any mouse event
    /// a program produced rather than a person — which is every event this
    /// application injects on a guest's behalf.
    const LLMHF_INJECTED: u32 = 0x0000_0001;

    static STARTED: AtomicBool = AtomicBool::new(false);

    pub fn start() {
        if STARTED.swap(true, Ordering::SeqCst) {
            return;
        }
        // # Its own thread, and a message loop on it
        //
        // A low-level hook is called on the thread that installed it, and only
        // while that thread is pumping messages. Installed on a thread that
        // then goes and does something else, the hook is never called — and
        // installed on a thread that is *slow* to pump, Windows removes it
        // without saying so. A thread whose only job is `GetMessageW` cannot be
        // either.
        let started = std::thread::Builder::new()
            .name("tain-local-input".into())
            .spawn(|| {
                // SAFETY: both hooks are installed for this thread only, with
                // callbacks that outlive it, and are released when the process
                // ends. `None` for the module is what a thread-local hook with
                // a Rust function pointer requires.
                let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0) };
                let keyboard =
                    unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0) };

                if mouse.is_err() && keyboard.is_err() {
                    tracing::warn!(
                        "Could not watch for local input — remote input will not give way \
                         to somebody using this machine"
                    );
                    return;
                }

                let mut message = MSG::default();
                // SAFETY: blocks this thread until a message arrives, which is
                // exactly what keeps the hooks alive and being called.
                while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {}
            });

        if let Err(e) = started {
            STARTED.store(false, Ordering::SeqCst);
            tracing::warn!("Could not start the local input watcher: {e}");
        }
    }

    /// # Safety
    /// A Windows hook callback: called by the system with a live `lparam`.
    unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            // SAFETY: for `HC_ACTION` the system guarantees `lparam` points at
            // a `MSLLHOOKSTRUCT` for the duration of this call.
            let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            if info.flags & LLMHF_INJECTED == 0 {
                seen();
            }
        }
        // SAFETY: passing the event on unchanged. Not doing this would swallow
        // every mouse event on the machine.
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }

    /// # Safety
    /// A Windows hook callback: called by the system with a live `lparam`.
    unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            // SAFETY: as above, for `KBDLLHOOKSTRUCT`.
            let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            if info.flags.0 & LLKHF_INJECTED.0 == 0 {
                seen();
            }
        }
        // SAFETY: as above.
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }
}

/// Take exclusive use of this module's state, for a test.
///
/// # Why anything here needs a lock
///
/// The last-input time and the window are process-wide, deliberately: there is
/// one keyboard and one person who may be at it. Which makes every test that
/// touches them a test that touches *the same* them, and Rust runs tests in
/// parallel — one setting a wide window while another was measuring with the
/// feature off is exactly the interference this exists to prevent, and it
/// showed up as a test that failed only sometimes.
///
/// Resets to a known state on the way in rather than trusting whoever ran last.
#[cfg(test)]
pub(crate) fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    LAST.store(0, Ordering::Relaxed);
    set_window(Duration::ZERO);
    guard
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine nobody has touched has nobody to give way to.
    #[test]
    fn nothing_seen_is_not_somebody_using_the_machine() {
        let _held = exclusive();
        assert!(
            !active_within(Duration::from_secs(3600)),
            "a machine with no observed input must not be treated as busy"
        );
    }

    /// Zero turns it off, whatever the clock says.
    ///
    /// The setting offers zero as "never give way", and a caller that reached
    /// for `quiet_for` directly would get "0ms since the last event is less
    /// than 0ms" wrong in either direction depending on the comparison.
    #[test]
    fn a_zero_window_never_suppresses_anything() {
        let _held = exclusive();
        seen();
        assert!(active_within(Duration::from_secs(3600)), "the event was seen");
        assert!(!active_within(Duration::ZERO), "and zero still means off");
    }

    /// An event a moment ago counts; one long past does not.
    #[test]
    fn the_window_is_measured_from_the_last_event() {
        let _held = exclusive();
        seen();
        std::thread::sleep(Duration::from_millis(20));

        assert!(
            !active_within(Duration::from_millis(2)),
            "an event older than the window must not still be suppressing"
        );
        assert!(
            active_within(Duration::from_secs(3600)),
            "and a window wide enough to contain it must"
        );
    }

    /// The window survives a round trip through the setting.
    #[test]
    fn the_window_is_what_was_asked_for() {
        let _held = exclusive();
        set_window(Duration::from_millis(2500));
        assert_eq!(window(), Duration::from_millis(2500));
    }
}
