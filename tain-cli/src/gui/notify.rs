//! Telling the person at the keyboard that somebody has connected.
//!
//! # Why the application cannot just show it itself
//!
//! A host is useful precisely when nobody is looking at it. The window is
//! behind something, or minimised, or on another desktop, and a card appearing
//! in a list nobody can see is not a notification — it is a record, read
//! afterwards. That is fine for the ones that need approval, because the guest
//! is stuck until somebody answers and will say so. It is not fine for an
//! administrator's connection, which needs no approval at all: it simply
//! begins, and the first the owner of the machine knows about it is whatever
//! they happen to notice.
//!
//! # Why a tray icon rather than a modern toast
//!
//! A `ToastNotification` needs an Application User Model ID that Windows has
//! been told about, which in practice means a Start Menu shortcut placed by an
//! installer. Tain runs from wherever it was put. Without that registration the
//! toast is not refused — it is accepted and silently never shown, which is the
//! worst behaviour available.
//!
//! `Shell_NotifyIconW` needs nothing registered and has worked on every Windows
//! since it mattered. On Windows 10 and 11 the balloon it asks for is drawn as
//! an ordinary notification and lands in the action centre with the rest.
//!
//! The icon is attached to a message-only window of this module's own, rather
//! than to the application window. It costs one window that never draws, and it
//! means notifications do not depend on eframe's window existing yet, surviving
//! a settings change, or being reachable from here at all.

/// Announce that a guest has arrived.
///
/// `needs_approval` decides the wording, not whether anything is shown: a
/// connection that let itself in is the one most worth mentioning.
pub fn incoming_connection(name: &str, needs_approval: bool) {
    #[cfg(target_os = "windows")]
    platform::show(name, needs_approval);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (name, needs_approval);
    }
}

/// Take the icon down. Called when the application is closing.
pub fn shut_down() {
    #[cfg(target_os = "windows")]
    platform::remove();
}

/// Whether somebody clicked the notification since this was last asked.
///
/// # Why this is asked rather than acted on where the click arrives
///
/// Raising a window means Win32's foreground rules, which exist to stop
/// background processes stealing focus and which a window procedure is a poor
/// place to argue with. eframe already knows how to ask for focus in a way the
/// window system accepts — see `ViewportCommand::Focus` — so the click is
/// recorded here and the frame that notices it does the asking.
pub fn was_clicked() -> bool {
    #[cfg(target_os = "windows")]
    {
        platform::was_clicked()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// Give this module a way to wake the interface when a notification is clicked.
///
/// Cheap to call every frame; only the first one is kept.
pub fn watch(ctx: &egui::Context) {
    #[cfg(target_os = "windows")]
    platform::attach(ctx);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = ctx;
    }
}

/// What a notification should say.
///
/// Split out so the wording can be tested without a desktop to draw on — the
/// Win32 half below has nothing to check that a test could reach.
pub fn wording(name: &str, needs_approval: bool) -> (String, String) {
    let title = if needs_approval {
        tain_core::i18n::text("notify.wants_to_connect").to_string()
    } else {
        tain_core::i18n::text("notify.has_connected").to_string()
    };
    let body = if needs_approval {
        tain_core::i18n::format("notify.waiting_body", &[("name", name)])
    } else {
        // Said plainly. An administrator connection needs no permission and
        // gets none asked for, so the only protection the owner of the machine
        // has is knowing it happened.
        tain_core::i18n::format("notify.admin_body", &[("name", name)])
    };
    (title, body)
}

#[cfg(target_os = "windows")]
mod platform {
    use std::sync::Mutex;

    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD,
        NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, LoadIconW, RegisterClassW, HWND_MESSAGE, IDI_APPLICATION,
        WINDOW_EX_STYLE, WINDOW_STYLE, WM_LBUTTONUP, WNDCLASSW,
    };

    /// The window the icon hangs off, once one has been made.
    ///
    /// Behind a mutex rather than a `OnceLock` because it can be taken down and
    /// must not be handed out afterwards. `usize` because `HWND` is a pointer
    /// and this is only ever touched from the one thread that made it.
    static ICON_WINDOW: Mutex<Option<usize>> = Mutex::new(None);

    /// Anything but zero, and the same every time: the icon is identified by
    /// the pair of window and id, and a second id would be a second icon.
    const ICON_ID: u32 = 1;

    /// The message the shell sends this window when the icon or its balloon is
    /// interacted with. Anything in the `WM_APP` range is the application's to
    /// choose.
    const CALLBACK: u32 = 0x0400 + 1;

    /// `NIN_BALLOONUSERCLICK`, which the crate does not name. Sent when the
    /// notification itself is clicked, as opposed to being dismissed with its
    /// close button — which is `NIN_BALLOONUSERCLICK`'s neighbour and
    /// deliberately not acted on.
    const NIN_BALLOONUSERCLICK: u32 = 0x0405;

    /// Set when somebody clicked the notification, cleared when it is read.
    static CLICKED: AtomicBool = AtomicBool::new(false);

    /// How to wake the interface, once it has said.
    ///
    /// # Why a click has to wake anything
    ///
    /// The flag above is read on a frame, and a frame only happens when
    /// something asks for one. The case this whole module exists for is a
    /// window nobody is looking at — behind something, or minimised — which is
    /// exactly the case where nothing is asking. Without this, the click would
    /// be recorded faithfully and acted on whenever the application next
    /// happened to draw, which is not an answer.
    static WAKE: Mutex<Option<egui::Context>> = Mutex::new(None);

    /// Remember how to wake the interface. Cheap to call repeatedly.
    pub fn attach(ctx: &egui::Context) {
        let mut held = WAKE.lock().unwrap_or_else(|e| e.into_inner());
        if held.is_none() {
            *held = Some(ctx.clone());
        }
    }

    /// Whether the notification has been clicked since this was last asked.
    ///
    /// Takes the answer rather than merely reading it: acting on a click twice
    /// would fight the user for the foreground.
    pub fn was_clicked() -> bool {
        CLICKED.swap(false, Ordering::Relaxed)
    }

    pub fn show(name: &str, needs_approval: bool) {
        let Some(window) = window() else { return };
        let (title, body) = super::wording(name, needs_approval);

        let mut data = base(window);
        data.uFlags = NIF_ICON | NIF_TIP | NIF_INFO | NIF_MESSAGE;
        data.dwInfoFlags = NIIF_INFO;
        write_utf16(&mut data.szInfoTitle, &title);
        write_utf16(&mut data.szInfo, &body);

        // SAFETY: the window is live and the structure is fully initialised,
        // including the size field the shell dispatches on.
        let shown = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool();
        if !shown {
            // Adding is the right recovery: the icon may never have been
            // registered, or explorer may have restarted and forgotten it.
            // SAFETY: as above.
            unsafe {
                let _ = Shell_NotifyIconW(NIM_ADD, &data);
            }
        }
    }

    pub fn remove() {
        let Some(window) = ICON_WINDOW.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let data = base(HWND(window as *mut _));
        // SAFETY: removing an icon this process added, by the id it added it
        // under. Harmless if it is already gone.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }

    /// The common half of every call. The shell reads `cbSize` to decide which
    /// version of the structure it has been handed, so it is never optional.
    fn base(window: HWND) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            uID: ICON_ID,
            // Carried on every call, including the delete, so the shell always
            // knows where to send what it has to say about this icon.
            uCallbackMessage: CALLBACK,
            ..Default::default()
        };
        // SAFETY: a system icon, which needs no module and is never freed.
        if let Ok(icon) = unsafe { LoadIconW(None, IDI_APPLICATION) } {
            data.hIcon = icon;
        }
        write_utf16(&mut data.szTip, "Tain");
        data
    }

    /// The message-only window, made once.
    ///
    /// `HWND_MESSAGE` gives a window that never draws, is not enumerated and
    /// costs nothing — it exists only to be an address the shell can associate
    /// an icon with.
    fn window() -> Option<HWND> {
        let mut held = ICON_WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = *held {
            return Some(HWND(existing as *mut _));
        }

        // SAFETY: registering a class and creating a window, both with fully
        // initialised arguments. A duplicate class registration fails
        // harmlessly and the creation below still succeeds.
        let window = unsafe {
            let instance = GetModuleHandleW(None).ok()?;
            let class = w!("TainNotifyIcon");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                PCWSTR::null(),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                instance,
                None,
            )
            .ok()?
        };

        *held = Some(window.0 as usize);
        Some(window)
    }

    /// Nothing is handled: the icon sends its messages here and they are all
    /// declined except the one that means somebody wants the window.
    ///
    /// # What changed and why
    ///
    /// This used to decline everything, on the reasoning that a notification is
    /// not a control surface and the window it would raise is already in the
    /// taskbar. That reasoning ignores what a notification is *for*: it appears
    /// because something needs attention, and the obvious gesture — clicking
    /// the thing that just appeared — did nothing at all, which reads as the
    /// application being broken rather than as a deliberate restraint.
    ///
    /// The click is only *recorded* here. Raising a window from a window
    /// procedure means Win32 foreground rules, and eframe already knows how to
    /// ask for focus properly; see [`super::was_clicked`].
    unsafe extern "system" fn procedure(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == CALLBACK {
            // The shell packs the event into the low word of `lparam`. A click
            // on the balloon and a click on the tray icon are the same wish.
            let event = (lparam.0 as u32) & 0xFFFF;
            if event == NIN_BALLOONUSERCLICK || event == WM_LBUTTONUP {
                CLICKED.store(true, Ordering::Relaxed);
                if let Some(ctx) = WAKE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    ctx.request_repaint();
                }
            }
            return LRESULT(0);
        }
        // SAFETY: forwarding a message the system sent, unmodified.
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    /// Copy `text` into a fixed-size UTF-16 field, truncated and terminated.
    ///
    /// The shell reads these as null-terminated, so the last slot is always left
    /// empty — a name that fills the field exactly would otherwise run into
    /// whatever follows it in the structure.
    fn write_utf16(field: &mut [u16], text: &str) {
        let room = field.len().saturating_sub(1);
        let mut written = 0;
        for unit in text.encode_utf16().take(room) {
            field[written] = unit;
            written += 1;
        }
        field[written..].fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::wording;

    /// The two cases have to read differently, and the quiet one has to be the
    /// louder message.
    ///
    /// A guest waiting for approval announces itself anyway: the session cannot
    /// start until somebody answers, so a missed notification costs a delay. An
    /// administrator's connection has already begun, and nobody is going to be
    /// asked anything — if that notification is missed, the fact that it
    /// happened is missed with it.
    #[test]
    fn an_admin_connection_says_that_nobody_was_asked() {
        let (waiting_title, waiting_body) = wording("DESKPC", true);
        let (admin_title, admin_body) = wording("DESKPC", false);

        assert_ne!(waiting_title, admin_title, "both cases announce themselves the same way");
        assert!(waiting_body.contains("DESKPC") && admin_body.contains("DESKPC"));

        assert!(
            waiting_body.contains("waiting"),
            "{waiting_body:?} does not say that something is expected of the reader"
        );
        assert!(
            admin_body.contains("not waiting") || admin_body.contains("admin"),
            "{admin_body:?} does not say this connection let itself in, which is the whole \
             reason it is worth a notification"
        );
    }

    /// A name from another machine goes into a fixed-size field.
    #[test]
    fn a_very_long_name_does_not_run_away_with_the_message() {
        let long = "Ω".repeat(500);
        let (_, body) = wording(&long, true);
        assert!(body.contains(&long), "the wording itself does not truncate — the field does");
    }
}
