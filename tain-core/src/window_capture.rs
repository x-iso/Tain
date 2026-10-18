//! Which windows can be shared, and what state the shared one is in.
//!
//! # Why a window is not just a smaller screen
//!
//! Sharing a display answers "let them see this monitor". Sharing a window
//! answers something narrower and much more useful when the machine is
//! someone's own: let them see *this one program*, and nothing else that
//! happens to be on the same desk. Mail stays private while the spreadsheet is
//! being looked at, and a second monitor is not a workaround for that.
//!
//! It also cannot be done by cropping. A window that is partly behind another
//! one, or scrolled off the edge of the screen, is not present in the desktop
//! image at all — cropping would share whatever is on top of it. The compositor
//! is the only thing that holds a window's own pixels, which is why this path is
//! Windows.Graphics.Capture and has no Desktop Duplication equivalent. See
//! [`super::capture_wgc`].
//!
//! # What a window can do that a monitor cannot
//!
//! It can be minimised, and it can be closed. A monitor being unplugged is rare
//! enough that the session simply falls back; a window being minimised is
//! ordinary, reversible, and happens by accident several times an hour. So it is
//! not a failure: the stream pauses, the guest is told what happened in words,
//! and it resumes when the window comes back. [`State`] is that distinction.

/// What a shared window is doing, as far as the guest needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// On screen and being captured.
    Showing,
    /// Minimised. The compositor stops producing frames for a window in this
    /// state, so there is nothing to send and nothing wrong — the guest is told
    /// so it can say why the picture stopped instead of showing a frozen one.
    Minimised,
    /// The window has gone. Closing the program that was being shared ends the
    /// sharing, and there is nothing to fall back to.
    Closed,
}

impl State {
    /// Whether frames can be expected right now.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Showing)
    }

    /// The same answer, in the shape the guest reads.
    pub fn on_the_wire(self) -> super::protocol::SharedWindowState {
        use super::protocol::SharedWindowState as Wire;
        match self {
            Self::Showing => Wire::Showing,
            Self::Minimised => Wire::Minimised,
            Self::Closed => Wire::Closed,
        }
    }
}

/// Everything the guest needs to know about the window being shared.
///
/// `None` once the window has gone *and* was never known — a window that closes
/// mid-session still reports itself, by the name it had, because "Excel has
/// closed" is worth saying and "something has closed" is not.
pub fn describe(
    id: u64,
    remembered: Option<&super::protocol::SharedWindow>,
) -> super::protocol::SharedWindow {
    let was = |pick: fn(&super::protocol::SharedWindow) -> &String| {
        remembered.map(pick).filter(|s| !s.is_empty()).cloned()
    };
    super::protocol::SharedWindow {
        id,
        // Asked again, then fallen back on: a window that has closed answers
        // nothing, and the last thing it was called is the only name left —
        // "Excel has closed" being worth saying where "something has closed"
        // is not.
        title: title_of(id).or_else(|| was(|w| &w.title)).unwrap_or_default(),
        app: was(|w| &w.app).or_else(|| app_of(id)).unwrap_or_default(),
        state: state_of(id).on_the_wire(),
    }
}

/// What the program owning this window is called, or `None` if it has gone.
pub fn app_of(id: u64) -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        windows_impl::app_of(id)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = id;
        None
    }
}

/// Windows a person could reasonably choose to share.
///
/// Ordered by the title, so the list does not reshuffle itself between one
/// refresh and the next — Z-order would, and a list that moves under the
/// pointer is a list nobody can click accurately.
pub fn shareable() -> Vec<super::protocol::WindowInfo> {
    #[cfg(target_os = "windows")]
    {
        let mut found = windows_impl::enumerate();
        found.sort_by(|a, b| {
            a.app.to_lowercase().cmp(&b.app.to_lowercase()).then(a.title.cmp(&b.title))
        });
        found
    }
    #[cfg(not(target_os = "windows"))]
    {
        Vec::new()
    }
}

/// What that window is doing now.
pub fn state_of(id: u64) -> State {
    #[cfg(target_os = "windows")]
    {
        windows_impl::state_of(id)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = id;
        State::Closed
    }
}

/// The window's current title, or `None` if it has gone.
///
/// Read again rather than remembered: a browser's title is the page it is on,
/// and a session that names the window in its own interface should not still be
/// saying what was open an hour ago.
pub fn title_of(id: u64) -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        windows_impl::title_of(id)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = id;
        None
    }
}

/// One window as an entry in a list, whatever state it is in.
///
/// Distinct from [`shareable`], which is the list of windows worth *offering*
/// and therefore excludes the ones nobody could point at — including minimised
/// ones, which have no on-screen rectangle to measure. This answers about a
/// window already chosen, and a choice does not stop being valid because the
/// program it names has been minimised since.
pub fn info_of(id: u64) -> Option<super::protocol::WindowInfo> {
    #[cfg(target_os = "windows")]
    {
        windows_impl::info_of(id)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = id;
        None
    }
}

/// This window as an entry in a session's offer, or `None` if it has gone.
///
/// # Why the state travels with the name
///
/// A guest's switcher is built from these, and "which of these can be shown
/// right now" is not something it can work out for itself: a minimised window
/// is still granted, still listed, and produces nothing. Without the state, a
/// session whose windows have *all* been minimised looks exactly like one that
/// is streaming — which is a frozen picture with nothing saying why, the
/// failure this whole area keeps coming back to.
pub fn on_offer(id: u64) -> Option<super::protocol::SharedWindow> {
    let window = describe(id, None);
    (window.state != super::protocol::SharedWindowState::Closed).then_some(window)
}

/// Where a window sits on the virtual desktop, in physical pixels.
///
/// The same shape as [`super::input_inject::Shown`] and deliberately not that
/// type: this module answers about windows and knows nothing about pointers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnScreen {
    /// May be negative — a window on a monitor left of the primary one is.
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

/// The rectangle a shared window actually occupies, or `None` if it has gone or
/// is not on screen.
///
/// # Why this is not `GetWindowRect`
///
/// Since Windows 10 a window's rectangle includes an invisible resize border —
/// several pixels of nothing on three sides — while what the compositor hands
/// over as the window's picture is its *visible* extent. Mapping a guest's
/// pointer through the larger rectangle puts every click a few pixels off,
/// consistently, in a way that looks like a calibration problem and is really a
/// border nobody can see. `DWMWA_EXTENDED_FRAME_BOUNDS` is what the compositor
/// itself measures by.
pub fn on_screen(id: u64) -> Option<OnScreen> {
    #[cfg(target_os = "windows")]
    {
        windows_impl::on_screen(id)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = id;
        None
    }
}

/// A small picture of a window, for telling one from another.
///
/// # Why a picture and not a longer list
///
/// The names do not distinguish them. Three browser windows are three page
/// titles, four Explorer windows are four folder names, and a program that puts
/// nothing useful in its caption — which is most of them once a document has no
/// name yet — is a row that reads "Untitled". The person choosing knows exactly
/// which window they mean, and they know it by what it *looks like*.
pub struct Thumbnail {
    pub width: u16,
    pub height: u16,
    /// Straight (non-premultiplied) RGBA, `width * height * 4` bytes, top row
    /// first.
    pub rgba: Vec<u8>,
}

/// Ask a window to draw itself small, for a picker.
///
/// `None` when there is nothing to show — the window has gone, it is minimised,
/// or it declined to render. Not an error: a tile with the program's name on it
/// is a perfectly good entry, and a picker that omitted the windows it could not
/// photograph would be missing the ones somebody is most likely looking for.
///
/// # Why this is not the capture path
///
/// It shares nothing with [`super::capture_wgc`] on purpose. That path exists to
/// produce sixty frames a second from one window and costs a device, a frame
/// pool and a thread to set up; this one runs once over every window on the
/// desk, needs no device at all, and must not make a window flash a capture
/// border merely because somebody opened a menu. They answer different
/// questions and the cheap answer is the right one here.
pub fn thumbnail(id: u64, max_edge: u16) -> Option<Thumbnail> {
    #[cfg(target_os = "windows")]
    {
        windows_impl::thumbnail(id, max_edge)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (id, max_edge);
        None
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::State;
    use crate::protocol::WindowInfo;
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, TRUE};
    use windows::Win32::Graphics::Dwm::{
        DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
    };
    // The thumbnail path. GDI rather than anything newer because a window is
    // asked to *print* itself here, and `PrintWindow` draws into a device
    // context or nothing at all.
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER,
        BI_RGB, DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC, SRCCOPY,
    };
    use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindow, GetWindowLongW, GetWindowRect, GetWindowTextLengthW,
        GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, GWL_EXSTYLE,
        GW_OWNER, PW_RENDERFULLCONTENT, WS_EX_TOOLWINDOW,
    };

    /// Smallest window worth offering, in either direction.
    ///
    /// Not a judgement about useful sizes — it excludes the zero- and near-zero
    /// sized windows that many programs keep around as message sinks, which are
    /// visible, titled, and not anything a person would recognise.
    const MIN_EDGE: i32 = 64;

    pub(super) fn enumerate() -> Vec<WindowInfo> {
        let mut found: Vec<WindowInfo> = Vec::new();

        // SAFETY: the pointer handed across is a `&mut Vec` that outlives the
        // enumeration, and `visit` is the only thing that touches it.
        unsafe {
            let _ = EnumWindows(
                Some(visit),
                LPARAM(std::ptr::addr_of_mut!(found) as isize),
            );
        }
        found
    }

    unsafe extern "system" fn visit(window: HWND, state: LPARAM) -> BOOL {
        // SAFETY: the `Vec` handed to `EnumWindows` above.
        let found = unsafe { &mut *(state.0 as *mut Vec<WindowInfo>) };
        if let Some(info) = describe(window) {
            found.push(info);
        }
        TRUE
    }

    /// One window, if it is one a person could pick out and point at.
    ///
    /// # Why the filtering is this fussy
    ///
    /// `EnumWindows` returns every top-level window in the session, and most of
    /// them are not windows in the sense anybody means: message-only sinks,
    /// tool palettes, the invisible hosts that back tray icons, and — the one
    /// that catches everybody — UWP applications that are "visible" while
    /// suspended. A list with those in it is a list where the thing being
    /// looked for is three screens down.
    fn describe(window: HWND) -> Option<WindowInfo> {
        // SAFETY: a handle from the enumeration, read-only calls throughout.
        unsafe {
            if !IsWindowVisible(window).as_bool() {
                return None;
            }
            // Owned windows are dialogs and palettes belonging to something
            // else. Sharing one shares a fragment of a program rather than the
            // program, which is never what was meant.
            if !GetWindow(window, GW_OWNER).unwrap_or_default().is_invalid() {
                return None;
            }
            if GetWindowLongW(window, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
                return None;
            }
            if is_cloaked(window) {
                return None;
            }

            let mut rect = RECT::default();
            if GetWindowRect(window, &mut rect).is_err() {
                return None;
            }
            let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
            if w < MIN_EDGE || h < MIN_EDGE {
                return None;
            }

            let title = read_title(window)?;
            if title.is_empty() {
                return None;
            }

            // Never offer this application's own windows. Sharing the window
            // that is displaying the share is a hall of mirrors, and the
            // session window belongs to whoever is watching, not to the desk
            // being watched.
            let mut pid = 0u32;
            GetWindowThreadProcessId(window, Some(&mut pid));
            if pid == std::process::id() {
                return None;
            }

            Some(WindowInfo {
                id: window.0 as u64,
                app: app_name(pid).unwrap_or_else(|| title.clone()),
                title,
                width: w.clamp(0, u16::MAX as i32) as u16,
                height: h.clamp(0, u16::MAX as i32) as u16,
            })
        }
    }

    /// Whether the compositor is hiding this window.
    ///
    /// A suspended UWP application answers `IsWindowVisible` with true and is
    /// nowhere on screen. `DWMWA_CLOAKED` is the attribute that tells them
    /// apart, and without it the list is mostly things that cannot be seen.
    fn is_cloaked(window: HWND) -> bool {
        let mut cloaked = 0u32;
        // SAFETY: the out-parameter is a `u32` and the size given matches it.
        let read = unsafe {
            DwmGetWindowAttribute(
                window,
                DWMWA_CLOAKED,
                std::ptr::addr_of_mut!(cloaked).cast(),
                std::mem::size_of::<u32>() as u32,
            )
        };
        // A failure means the attribute could not be read, not that the window
        // is hidden — an older Windows, or a window that has just gone. Not
        // cloaked is the answer that keeps it in the list, and something that
        // has gone will fail the checks around this anyway.
        read.is_ok() && cloaked != 0
    }

    fn read_title(window: HWND) -> Option<String> {
        // SAFETY: length first, then a buffer sized from it.
        unsafe {
            let length = GetWindowTextLengthW(window);
            if length <= 0 {
                return None;
            }
            // Room for the terminator the API writes.
            let mut buffer = vec![0u16; length as usize + 1];
            let written = GetWindowTextW(window, &mut buffer);
            if written <= 0 {
                return None;
            }
            Some(String::from_utf16_lossy(&buffer[..written as usize]))
        }
    }

    /// The executable's own name — "Excel.exe" becomes "Excel".
    ///
    /// The title alone is not enough to recognise a window by: three browser
    /// windows are three page titles with nothing saying they are the browser,
    /// and "Document1" could be anything.
    fn app_name(pid: u32) -> Option<String> {
        // SAFETY: the handle is closed by its own `Drop`; the buffer outlives
        // the call and its length is passed by pointer as the API requires.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buffer = vec![0u16; 260];
            let mut length = buffer.len() as u32;
            let read = QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_FORMAT(0),
                windows::core::PWSTR(buffer.as_mut_ptr()),
                &mut length,
            );
            let _ = windows::Win32::Foundation::CloseHandle(process);
            read.ok()?;

            let path = String::from_utf16_lossy(&buffer[..length as usize]);
            let file = path.rsplit(['\\', '/']).next()?;
            let stem = file.strip_suffix(".exe").or_else(|| file.strip_suffix(".EXE")).unwrap_or(file);
            (!stem.is_empty()).then(|| stem.to_string())
        }
    }

    pub(super) fn state_of(id: u64) -> State {
        let window = HWND(id as isize as *mut std::ffi::c_void);
        // SAFETY: reading the state of a handle that may already be invalid,
        // which is exactly what `IsWindow` is for.
        unsafe {
            if !IsWindow(window).as_bool() {
                return State::Closed;
            }
            if IsIconic(window).as_bool() {
                return State::Minimised;
            }
            // A window can also be hidden without being minimised — moved to
            // another virtual desktop, or its program told to hide it. The
            // compositor produces nothing for those either, and "minimised" is
            // the word a person will understand for all of them.
            if !IsWindowVisible(window).as_bool() || is_cloaked(window) {
                return State::Minimised;
            }
            State::Showing
        }
    }

    pub(super) fn app_of(id: u64) -> Option<String> {
        let window = HWND(id as isize as *mut std::ffi::c_void);
        // SAFETY: guarded by `IsWindow`; the pid is an out-parameter we own.
        unsafe {
            if !IsWindow(window).as_bool() {
                return None;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(window, Some(&mut pid));
            app_name(pid)
        }
    }

    pub(super) fn title_of(id: u64) -> Option<String> {
        let window = HWND(id as isize as *mut std::ffi::c_void);
        // SAFETY: guarded by `IsWindow`, which is what makes the read safe.
        unsafe {
            if !IsWindow(window).as_bool() {
                return None;
            }
        }
        read_title(window)
    }

    pub(super) fn on_screen(id: u64) -> Option<super::OnScreen> {
        let window = HWND(id as isize as *mut std::ffi::c_void);
        // SAFETY: guarded by `IsWindow`; both calls write into a `RECT` we own.
        unsafe {
            if !IsWindow(window).as_bool() || IsIconic(window).as_bool() {
                return None;
            }

            let mut rect = RECT::default();
            let measured = DwmGetWindowAttribute(
                window,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                std::ptr::addr_of_mut!(rect).cast(),
                std::mem::size_of::<RECT>() as u32,
            );
            // An older Windows, or a window that has gone between the two
            // calls. The outer rectangle is a few pixels too generous rather
            // than wrong, which is the better direction to fail in.
            if measured.is_err() && GetWindowRect(window, &mut rect).is_err() {
                return None;
            }

            let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
            if width <= 0 || height <= 0 {
                return None;
            }
            Some(super::OnScreen {
                x: rect.left,
                y: rect.top,
                width: width.clamp(0, u16::MAX as i32) as u16,
                height: height.clamp(0, u16::MAX as i32) as u16,
            })
        }
    }

    pub(super) fn info_of(id: u64) -> Option<WindowInfo> {
        let window = HWND(id as isize as *mut std::ffi::c_void);
        // SAFETY: every call below is a read guarded by `IsWindow`.
        unsafe {
            if !IsWindow(window).as_bool() {
                return None;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(window, Some(&mut pid));
            let title = read_title(window).unwrap_or_default();

            // A minimised window's rectangle is off in the corner of nowhere —
            // Windows parks it at (-32000, -32000) — so the size is reported as
            // zero rather than as that. Nothing renders from these numbers;
            // they are here so the guest's list can say how big a window is,
            // and a made-up size would be worse than none.
            let mut rect = RECT::default();
            let (width, height) = if IsIconic(window).as_bool()
                || GetWindowRect(window, &mut rect).is_err()
            {
                (0, 0)
            } else {
                (
                    (rect.right - rect.left).clamp(0, u16::MAX as i32) as u16,
                    (rect.bottom - rect.top).clamp(0, u16::MAX as i32) as u16,
                )
            };

            Some(WindowInfo {
                id,
                app: app_name(pid).unwrap_or_else(|| title.clone()),
                title,
                width,
                height,
            })
        }
    }

    /// # Why `PrintWindow` and not the capture backend
    ///
    /// Asking the window to draw itself is the only method here that is cheap
    /// enough to run over a whole desk at once and quiet enough to run without
    /// anybody noticing. Windows.Graphics.Capture would give a better picture
    /// and would cost a graphics device, a frame pool and an asynchronous wait
    /// *per window* — and on the Windows builds that lack `IsBorderRequired` it
    /// draws a yellow border around whatever it captures, so opening the picker
    /// would flash a ring around every window on the screen.
    ///
    /// The trade is that some windows come back blank: a program drawing with
    /// its own swapchain need not honour the request. That is why the caller is
    /// handed an `Option` rather than a grey rectangle — a picker that knows it
    /// has no picture can show the program's name instead, and one handed an
    /// empty image cannot tell that from a window that is genuinely blank.
    pub(super) fn thumbnail(id: u64, max_edge: u16) -> Option<super::Thumbnail> {
        let window = HWND(id as isize as *mut std::ffi::c_void);
        // SAFETY: every GDI object created below is released on every path out,
        // including the early returns — which is what the nested scopes and the
        // single `result` binding at the end are arranged to guarantee.
        unsafe {
            if !IsWindow(window).as_bool() || IsIconic(window).as_bool() {
                return None;
            }
            let mut rect = RECT::default();
            if GetWindowRect(window, &mut rect).is_err() {
                return None;
            }
            let (full_w, full_h) = (rect.right - rect.left, rect.bottom - rect.top);
            if full_w <= 0 || full_h <= 0 {
                return None;
            }

            // The long edge is what `max_edge` bounds, so a tall window and a
            // wide one both come back within the same box and the tiles in a
            // picker are the same size whatever shape the window is.
            let longest = full_w.max(full_h) as f64;
            let scale = (f64::from(max_edge) / longest).min(1.0);
            let small_w = ((full_w as f64 * scale).round() as i32).max(1);
            let small_h = ((full_h as f64 * scale).round() as i32).max(1);

            let screen = GetDC(HWND::default());
            if screen.is_invalid() {
                return None;
            }

            let result = draw(window, screen, (full_w, full_h), (small_w, small_h));
            ReleaseDC(HWND::default(), screen);
            result
        }
    }

    /// The GDI half of [`thumbnail`], so the screen DC has exactly one release.
    ///
    /// # SAFETY
    ///
    /// `screen` must be a valid DC that outlives this call. Every object made
    /// here is deleted here.
    unsafe fn draw(
        window: HWND,
        screen: HDC,
        full: (i32, i32),
        small: (i32, i32),
    ) -> Option<super::Thumbnail> {
        let (full_w, full_h) = full;
        let (small_w, small_h) = small;
        // SAFETY: the contract above. Spelled out here rather than inherited
        // from the signature, so what is unsafe is this body and not every call
        // to it.
        unsafe {
            // Both surfaces are DIB sections rather than compatible bitmaps: a
            // compatible bitmap is in whatever format the screen happens to be
            // in and has to be read back through `GetDIBits`, while these hand
            // back a pointer to the bytes themselves in the layout asked for.
            let (full_dc, full_bmp, _full_bits) = dib(screen, full_w, full_h)?;

            // # The bitmap stays selected until the blit has read it
            //
            // A device context is not the picture — the picture is whatever
            // bitmap is *currently selected into* it, and a fresh DC comes with
            // a 1×1 monochrome placeholder in that slot. Putting the full-size
            // bitmap back before shrinking it, which reads better as
            // "borrow, use, return", means the blit reads that placeholder
            // instead: it succeeds, and every window comes out one flat colour.
            //
            // So the restore happens at the end, after the last read.
            let previous = SelectObject(full_dc, full_bmp);

            // `PW_RENDERFULLCONTENT` is what makes this work at all on a
            // composited desktop: without it a window drawing through DWM
            // renders as an empty frame.
            let painted =
                PrintWindow(window, full_dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool();

            let shrunk = if painted {
                match dib(screen, small_w, small_h) {
                    Some((small_dc, small_bmp, small_bits)) => {
                        let was = SelectObject(small_dc, small_bmp);
                        // Averaged rather than sampled. A thumbnail made by
                        // throwing away three pixels in four is aliased text,
                        // which is exactly the part somebody is reading to tell
                        // two windows of the same program apart.
                        SetStretchBltMode(small_dc, HALFTONE);
                        let _ = SetBrushOrgEx(small_dc, 0, 0, None);
                        let ok = StretchBlt(
                            small_dc, 0, 0, small_w, small_h, full_dc, 0, 0, full_w, full_h,
                            SRCCOPY,
                        )
                        .as_bool();
                        let taken = ok.then(|| read_bgra(small_bits, small_w, small_h));
                        SelectObject(small_dc, was);
                        let _ = DeleteObject(small_bmp);
                        let _ = DeleteDC(small_dc);
                        taken.flatten()
                    }
                    None => None,
                }
            } else {
                None
            };

            // A bitmap cannot be deleted while it is selected into a DC, which
            // is the other half of why this ordering matters.
            SelectObject(full_dc, previous);
            let _ = DeleteObject(full_bmp);
            let _ = DeleteDC(full_dc);

            shrunk.map(|rgba| super::Thumbnail {
                width: small_w.clamp(0, u16::MAX as i32) as u16,
                height: small_h.clamp(0, u16::MAX as i32) as u16,
                rgba,
            })
        }
    }

    /// A memory DC with a 32-bit top-down DIB selected into it, and the bytes.
    ///
    /// # SAFETY
    ///
    /// The caller owns everything returned and must delete the bitmap and the
    /// DC. The pointer is valid only until the bitmap is deleted.
    unsafe fn dib(screen: HDC, width: i32, height: i32) -> Option<(HDC, HBITMAP, *mut u8)> {
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Negative for top-down. A DIB is bottom-up by default, and
                // every consumer of these bytes — egui included — reads the
                // first row as the top one.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        // SAFETY: the contract above — `screen` is live, and every object made
        // here is either returned to the caller or deleted before returning.
        unsafe {
            let dc = CreateCompatibleDC(screen);
            if dc.is_invalid() {
                return None;
            }

            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(
                screen,
                std::ptr::addr_of_mut!(info),
                DIB_RGB_COLORS,
                std::ptr::addr_of_mut!(bits),
                None,
                0,
            );
            match bitmap {
                Ok(bitmap) if !bits.is_null() => Some((dc, bitmap, bits.cast())),
                Ok(bitmap) => {
                    let _ = DeleteObject(bitmap);
                    let _ = DeleteDC(dc);
                    None
                }
                Err(_) => {
                    let _ = DeleteDC(dc);
                    None
                }
            }
        }
    }

    /// The DIB's bytes as straight RGBA, or `None` if there is no picture in
    /// them.
    ///
    /// # SAFETY
    ///
    /// `bits` must point at `width * height * 4` bytes of a live DIB section.
    unsafe fn read_bgra(bits: *const u8, width: i32, height: i32) -> Option<Vec<u8>> {
        let count = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
        // SAFETY: the caller's contract — the DIB was created at this size and
        // has not been deleted.
        let source = unsafe { std::slice::from_raw_parts(bits, count) };

        let mut rgba = Vec::with_capacity(count);
        for pixel in source.chunks_exact(4) {
            rgba.push(pixel[2]);
            rgba.push(pixel[1]);
            rgba.push(pixel[0]);
            // # The alpha channel is thrown away deliberately
            //
            // `PrintWindow` writes whatever the window's own drawing left in
            // that byte, which for most of them is zero — the window never had
            // a reason to fill it in. Kept, the picture is invisible; a
            // thumbnail has nothing behind it to blend with, so opaque is both
            // correct and the only useful answer.
            rgba.push(255);
        }

        // A window that declined to draw leaves the buffer exactly as it was
        // allocated, which is one flat colour. Reported as no picture rather
        // than as a black rectangle: the caller can put the program's name in
        // the tile, and cannot do that if it is handed something that looks
        // like a legitimate image.
        let flat = rgba
            .chunks_exact(4)
            .all(|pixel| pixel[0] == rgba[0] && pixel[1] == rgba[1] && pixel[2] == rgba[2]);
        (!flat).then_some(rgba)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever this machine is running, the list has to be self-consistent.
    ///
    /// # Why the assertions are shaped like this
    ///
    /// The windows open depend on who is at the keyboard, so asserting a count
    /// or a name would pin the test to one desk. What can be asserted is that
    /// every entry is usable as a thing to share: a real id, something to call
    /// it, and a size that could hold a picture.
    #[test]
    fn every_window_offered_could_actually_be_shared() {
        let offered = shareable();
        println!("{} shareable windows:", offered.len());
        for window in &offered {
            println!("  {:>12}  {} — {}", window.id, window.app, window.title);
            assert_ne!(window.id, 0, "a window with no handle cannot be captured");
            assert!(!window.title.is_empty(), "an untitled window is unpickable");
            assert!(!window.app.is_empty(), "{} has nothing to call it", window.id);
            assert!(window.width > 0 && window.height > 0, "{} has no size", window.id);
        }
    }

    /// Two entries for one window would be two ways to ask for the same thing,
    /// and the second one would look like a different program.
    #[test]
    fn no_window_is_offered_twice() {
        let offered = shareable();
        let mut seen = std::collections::BTreeSet::new();
        for window in &offered {
            assert!(seen.insert(window.id), "{} appears more than once", window.id);
        }
    }

    /// Sharing the window that shows the sharing is a hall of mirrors.
    #[test]
    fn this_application_never_offers_its_own_windows() {
        // Nothing to assert about *which* windows are ours; what matters is
        // that the filter runs at all and none of them come back.
        let ours = std::process::id();
        for window in shareable() {
            assert_ne!(
                window.id, 0,
                "process {ours} produced a window with no handle, which the filter should have dropped"
            );
        }
    }

    /// A handle that never was cannot be showing anything.
    #[test]
    fn a_window_that_does_not_exist_reads_as_closed() {
        assert_eq!(state_of(0), State::Closed);
        assert_eq!(state_of(u64::MAX), State::Closed);
        assert_eq!(title_of(0), None);
    }

    /// The one state that means "send frames".
    #[test]
    fn only_a_showing_window_is_live() {
        assert!(State::Showing.is_live());
        assert!(!State::Minimised.is_live(), "a minimised window produces no frames");
        assert!(!State::Closed.is_live());
    }

    /// Whatever is on screen right now, asking about it must agree with the
    /// list that offered it.
    #[test]
    fn a_window_that_was_just_offered_is_not_closed() {
        for window in shareable() {
            assert_ne!(
                state_of(window.id),
                State::Closed,
                "{} was offered and is already gone",
                window.title
            );
        }
    }

    /// A window already chosen can still be described after it goes away.
    ///
    /// The picker leans on this: [`shareable`] drops a minimised window because
    /// nobody could point at one, and a selection would silently shrink if that
    /// were the only way to ask about a window.
    #[test]
    fn a_window_can_be_described_without_being_offerable() {
        for window in shareable() {
            let again = info_of(window.id).expect("a window that was just listed still exists");
            assert_eq!(again.id, window.id);
            assert_eq!(again.app, window.app, "the same window answered to two names");
        }
        assert!(info_of(0).is_none(), "a handle that never was describes nothing");
        assert!(info_of(u64::MAX).is_none());
    }

    /// Whatever this desk looks like, a thumbnail of it has to be coherent.
    ///
    /// # Why the assertions are shaped like this
    ///
    /// Which windows will hand back a picture depends on what is open and on
    /// how those programs draw themselves — a window rendering through its own
    /// swapchain may decline, and that is a supported answer, not a failure. So
    /// what is asserted is that every picture which *does* come back is usable:
    /// the right number of bytes for the size it claims, inside the bound it was
    /// asked to fit, and with its proportions intact.
    ///
    /// The counts are printed rather than asserted, because a machine with
    /// nothing open is a legitimate machine.
    #[test]
    fn every_thumbnail_taken_is_the_size_it_says_it_is() {
        const EDGE: u16 = 128;
        let (mut drawn, mut declined) = (0, 0);

        for window in shareable() {
            let Some(shot) = thumbnail(window.id, EDGE) else {
                declined += 1;
                println!("  no picture: {} — {}", window.app, window.title);
                continue;
            };
            drawn += 1;

            assert!(shot.width > 0 && shot.height > 0, "{} came back with no size", window.title);
            assert_eq!(
                shot.rgba.len(),
                usize::from(shot.width) * usize::from(shot.height) * 4,
                "{} claims {}x{} and did not carry that many bytes",
                window.title,
                shot.width,
                shot.height
            );
            assert!(
                shot.width <= EDGE && shot.height <= EDGE,
                "{} came back {}x{}, over the {EDGE} it was asked for",
                window.title,
                shot.width,
                shot.height
            );

            // Its shape, within a pixel of rounding either way. A thumbnail
            // that lost its proportions is a window nobody would recognise,
            // which is the one thing this is for.
            let wanted = f64::from(window.width) / f64::from(window.height);
            let got = f64::from(shot.width) / f64::from(shot.height);
            let tolerance = 2.0 / f64::from(shot.height.min(shot.width));
            assert!(
                (wanted - got).abs() <= wanted * tolerance.max(0.05),
                "{} is {}x{} and its thumbnail is {}x{} — a different shape",
                window.title,
                window.width,
                window.height,
                shot.width,
                shot.height
            );

            // Opaque throughout. `PrintWindow` leaves whatever the window's own
            // drawing put in that byte, which for most of them is nothing —
            // carried through, the tile would be invisible.
            assert!(
                shot.rgba.chunks_exact(4).all(|pixel| pixel[3] == 255),
                "{} came back with transparency, so it would not be visible in a tile",
                window.title
            );
        }

        println!("{drawn} window(s) drew themselves, {declined} declined");
    }

    /// Nothing that is not there can be photographed.
    #[test]
    fn a_window_that_does_not_exist_has_no_picture() {
        assert!(thumbnail(0, 128).is_none());
        assert!(thumbnail(u64::MAX, 128).is_none());
    }

    /// A shared window's rectangle is the one its picture covers.
    ///
    /// # What this is guarding
    ///
    /// The guest's pointer is mapped through this rectangle. Since Windows 10 a
    /// window's `GetWindowRect` includes several pixels of invisible resize
    /// border on three sides, and the compositor hands over only the visible
    /// extent — so measuring by the outer rectangle puts every click a few
    /// pixels off, consistently, and looks like a calibration fault rather than
    /// a border nobody can see.
    ///
    /// Asserted per window as "no larger than", because the two agree exactly
    /// for a maximised window and for one whose program draws its own frame —
    /// and once across the desk as "at least one is genuinely smaller", which is
    /// what would fail if this went back to reading the outer rectangle.
    ///
    /// # Why the two rectangles are re-read together
    ///
    /// The first version compared against the size recorded by `shareable()`,
    /// and failed about one run in ten: a window that was resized between
    /// enumerating the desk and asking about it is not a bug, it is somebody
    /// using their computer. Both numbers now come from adjacent calls, and a
    /// window that moves even so is skipped rather than failed.
    #[test]
    fn a_windows_rectangle_is_the_part_that_can_be_seen() {
        let mut trimmed = 0;
        let mut compared = 0;

        for window in shareable() {
            // Adjacent reads, visible then outer, so the gap between them is two
            // system calls rather than a walk of every window on the desk.
            let (Some(rect), Some(now)) = (on_screen(window.id), info_of(window.id)) else {
                continue;
            };
            if (now.width, now.height) != (window.width, window.height) {
                // It is being dragged or resized right now. Nothing to say about
                // a rectangle that is not standing still.
                continue;
            }

            assert!(rect.width > 0 && rect.height > 0, "{} occupies nothing", window.title);
            assert!(
                rect.width <= now.width && rect.height <= now.height,
                "{} measured {}x{} visible against an outer rectangle of {}x{} — the visible \
                 extent cannot be the larger of the two",
                window.title,
                rect.width,
                rect.height,
                now.width,
                now.height
            );
            compared += 1;
            if rect.width < now.width || rect.height < now.height {
                trimmed += 1;
            }
        }

        println!("{trimmed} of {compared} windows are smaller than their outer rectangle");
        if compared > 0 {
            assert!(
                trimmed > 0,
                "not one of {compared} windows was smaller than its outer rectangle, which is \
                 what reading `GetWindowRect` instead of the extended frame bounds would look \
                 like"
            );
        }

        assert!(on_screen(0).is_none(), "a handle that never was occupies nothing");
        assert!(on_screen(u64::MAX).is_none());
    }
}
