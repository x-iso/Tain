//! The session window's own title bar.
//!
//! # Why the caption is taken over rather than switched off
//!
//! The session window puts its tabs and tools in the row Windows would spend on
//! a title. The obvious way to get that row, `with_decorations(false)`, costs
//! far more than the title: winit then makes the *whole* window client area, so
//! the resize borders that sit outside a normal window go, the drop shadow goes
//! with them, and Windows 11 never offers snap layouts over a maximize button it
//! cannot find. Resize grips would have to move inside the window, onto the
//! edges of the remote screen — exactly where the host's taskbar, Start button
//! and scrollbars are.
//!
//! So the window keeps its native frame and only the caption is removed, the
//! way Windows Terminal and Chrome do it. A subclass answers the two questions
//! Windows asks about the frame:
//!
//! - `WM_NCCALCSIZE`, *where is the client area*: wherever the default frame
//!   leaves it, except that it reaches the top edge. The caption is gone; the
//!   borders on the other three sides stay outside the window where they were.
//! - `WM_NCHITTEST`, *what is under the pointer*: the header's empty space is
//!   the caption, so dragging, double-click to maximize, the system menu and
//!   Aero Snap are all Windows' own; and our maximize button is a maximize
//!   button, so snap layouts appear over it.
//!
//! The header itself is drawn by egui, which says each frame where its empty
//! space and its maximize button are — see [`Layout`].

use egui::{Context, Pos2, Rect, ViewportBuilder, ViewportCommand, ViewportId};

/// Width of the minimize, maximize and close buttons: Windows' own.
pub const BUTTON_W: f32 = 46.0;

/// Frames to spend looking for the window before settling for the native
/// title bar.
///
/// eframe creates an immediate viewport's window before it runs the callback,
/// so the first frame should always find it. The others are insurance, and
/// cost a frame each only when something has already gone wrong.
const LOOK_LIMIT: u8 = 3;

/// Where the header's parts are, in points from the client area's top-left.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    /// The row Windows should treat as the title. `None` when there is no
    /// header, which is fullscreen.
    pub caption: Option<Rect>,
    /// Everything in the caption that has to stay clickable: the tabs, the
    /// tools, the minimize and close buttons. The rest of it drags the window.
    pub controls: Vec<Rect>,
    /// The maximize button, which Windows has to know about by name for snap
    /// layouts to appear over it.
    pub maximize: Option<Rect>,
}

/// What a point in the client area is, as far as the frame is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// Ours: egui handles it.
    Client,
    /// Empty header: Windows drags, maximizes and snaps.
    Caption,
    Maximize,
}

impl Layout {
    pub fn hit(&self, point: Pos2) -> Hit {
        if self.maximize.is_some_and(|r| r.contains(point)) {
            return Hit::Maximize;
        }
        match self.caption {
            Some(caption)
                if caption.contains(point) && !self.controls.iter().any(|r| r.contains(point)) =>
            {
                Hit::Caption
            }
            _ => Hit::Client,
        }
    }
}

/// Where the session window is in taking over its own caption.
pub struct TitleBar {
    stage: Stage,
    /// Whether the window is being kept above the taskbar — see
    /// [`TitleBar::keep_above_taskbar`].
    above: bool,
    /// When to next look for a taskbar in front, in egui's seconds.
    recheck_at: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Hidden, looking for the window. Hidden so that nobody sees the native
    /// caption for a frame before it goes.
    Looking(u8),
    /// Caption taken. One more hidden frame, so the first one shown is laid out
    /// at the size the window has without it.
    Settling,
    /// Shown, with this title bar.
    Custom,
    /// Shown, with Windows' title bar, because the window could not be found.
    /// The header then has no window buttons: Windows is drawing them.
    Native,
}

impl Default for TitleBar {
    fn default() -> Self {
        let stage = if cfg!(target_os = "windows") { Stage::Looking(0) } else { Stage::Native };
        Self { stage, above: false, recheck_at: 0.0 }
    }
}

impl TitleBar {
    /// The window starts hidden while its caption is being taken over.
    pub fn builder(&self, builder: ViewportBuilder) -> ViewportBuilder {
        match self.stage {
            Stage::Looking(_) => builder.with_visible(false),
            _ => builder,
        }
    }

    /// Take the caption over if that has not happened yet, from inside the
    /// session window's own frame.
    ///
    /// Returns whether this title bar is in charge, which is whether the header
    /// should draw minimize, maximize and close.
    pub fn update(&mut self, ctx: &Context, title: &str) -> bool {
        // The subclass lets go when its window is destroyed. If that happened
        // without the session window closing — egui recreating the window — the
        // new one has to be found and taken over again.
        if self.stage == Stage::Custom && !platform::attached() {
            self.stage = Stage::Looking(0);
            self.above = false;
        }

        match self.stage {
            Stage::Looking(tried) => {
                if platform::attach(ctx, title) {
                    self.stage = Stage::Settling;
                } else if tried + 1 >= LOOK_LIMIT {
                    tracing::warn!(
                        "No window titled {title:?} on the interface thread; \
                         the session window keeps Windows' title bar"
                    );
                    self.stage = Stage::Native;
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                } else {
                    self.stage = Stage::Looking(tried + 1);
                }
                // Immediate viewports draw as part of the main window's frame,
                // so that is the one to wake for the next step.
                ctx.request_repaint_of(ViewportId::ROOT);
            }
            Stage::Settling => {
                self.stage = Stage::Custom;
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            }
            Stage::Custom | Stage::Native => {}
        }
        matches!(self.stage, Stage::Settling | Stage::Custom)
    }

    /// Tell the frame where the header's parts are this frame.
    pub fn publish(&self, ctx: &Context, layout: Layout) {
        if matches!(self.stage, Stage::Settling | Stage::Custom) {
            platform::publish(layout, ctx.pixels_per_point());
        }
    }

    /// Whether the maximize button is hovered and pressed.
    ///
    /// Asked of the frame rather than of egui: the button is Windows' to hit
    /// test, so the pointer over it is in the non-client area, and egui is told
    /// it has left the window.
    pub fn maximize_state(&self) -> (bool, bool) {
        platform::maximize_state()
    }

    /// Whether the window is the one in front, asked of Windows — or `None`
    /// with Windows' own title bar, where this has no handle on the window.
    pub fn in_front(&self) -> Option<bool> {
        if !matches!(self.stage, Stage::Settling | Stage::Custom) {
            return None;
        }
        platform::in_front()
    }

    /// Keep the window above the taskbar while it is fullscreen and in front.
    ///
    /// # Why it has to be asked for
    ///
    /// Windows is meant to step the taskbar aside for a window that covers its
    /// monitor and has the focus, and it does recognise this one — the shell
    /// reports a full-screen application while it is up — but the taskbar here
    /// stayed on top of it regardless, and did so before the window drew its
    /// own title bar too. Measured with `WindowFromPoint` over the taskbar,
    /// which a loopback session's picture of the screen cannot fool.
    ///
    /// Topmost is what puts it above. Only while it has the focus, though: a
    /// topmost window covering the screen is one nothing can be switched to,
    /// so on losing the focus it lets go and steps behind whatever took it.
    ///
    /// # And it has to be kept there
    ///
    /// The taskbar is topmost too, and it raises itself again while the window
    /// is turning fullscreen — after this has run, so the first raise lost. So
    /// a few times a second this looks along the window's edges, where a
    /// taskbar would be, and goes back above one it finds in front. Only a
    /// taskbar: other topmost windows, the touch keyboard among them, are left
    /// where they put themselves. `time` is egui's, in seconds.
    pub fn keep_above_taskbar(&mut self, above: bool, time: f64) {
        /// How often to look, in seconds.
        const RECHECK: f64 = 0.25;

        if !matches!(self.stage, Stage::Settling | Stage::Custom) {
            return;
        }
        if above != self.above {
            self.above = above;
            self.recheck_at = time + RECHECK;
            platform::keep_above_taskbar(above);
        } else if above && time >= self.recheck_at {
            self.recheck_at = time + RECHECK;
            platform::stay_above_taskbar();
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use std::sync::{Mutex, MutexGuard};

    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, POINT, RECT, TRUE, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONULL,
    };
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        TrackMouseEvent, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT,
    };
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumThreadWindows, GetAncestor, GetClassNameW, GetForegroundWindow, GetWindowInfo,
        GetWindowLongW, GetWindowRect, GetWindowTextW, IsZoomed, PostMessageW, SetWindowPos,
        WindowFromPoint, GA_ROOT, GWL_EXSTYLE, GWL_STYLE, HTCAPTION, HTCLIENT,
        HTMAXBUTTON, HTTOP, HTTOPLEFT, HTTOPRIGHT, HWND_NOTOPMOST, HWND_TOPMOST,
        NCCALCSIZE_PARAMS, SC_MAXIMIZE, SC_RESTORE, SM_CXPADDEDBORDER, SM_CYSIZEFRAME,
        SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WINDOWINFO,
        WINDOWPOS, WM_NCCALCSIZE, WM_NCDESTROY, WM_NCHITTEST, WM_NCLBUTTONDBLCLK,
        WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_SYSCOMMAND,
        WM_WINDOWPOSCHANGING, WS_CAPTION, WS_EX_TOPMOST,
    };

    use super::{Hit, Layout};

    /// What the subclass and the frame share.
    ///
    /// One, not one per window: there is one session window. `usize` because
    /// `HWND` is a pointer, and both sides run on the interface thread anyway.
    struct Shared {
        window: usize,
        layout: Layout,
        pixels_per_point: f32,
        hovered: bool,
        pressed: bool,
        /// To redraw the maximize button when the pointer reaches or leaves
        /// it, which egui has no way to notice for itself.
        wake: egui::Context,
    }

    static SHARED: Mutex<Option<Shared>> = Mutex::new(None);

    /// Any constant: a subclass is identified by its procedure and this.
    const SUBCLASS_ID: usize = 1;

    fn shared() -> MutexGuard<'static, Option<Shared>> {
        SHARED.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn attached() -> bool {
        shared().is_some()
    }

    pub fn attach(ctx: &egui::Context, title: &str) -> bool {
        let Some(window) = find(title) else { return false };

        // Before the subclass, so it never runs without something to read.
        *shared() = Some(Shared {
            window: window.0 as usize,
            layout: Layout::default(),
            pixels_per_point: ctx.pixels_per_point(),
            hovered: false,
            pressed: false,
            wake: ctx.clone(),
        });

        // SAFETY: a window this thread created, and a procedure that lives as
        // long as the program. It removes itself when the window goes.
        let subclassed = unsafe { SetWindowSubclass(window, Some(procedure), SUBCLASS_ID, 0) };
        if !subclassed.as_bool() {
            *shared() = None;
            return false;
        }

        // The answer to "where is the client area" just changed, and Windows
        // only asks again when told to. Without this the caption stays until
        // the window is next resized.
        //
        // SAFETY: a live window; only the frame is recalculated.
        unsafe {
            let _ = SetWindowPos(
                window,
                HWND::default(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        true
    }

    pub fn publish(layout: Layout, pixels_per_point: f32) {
        if let Some(state) = shared().as_mut() {
            state.layout = layout;
            state.pixels_per_point = pixels_per_point;
        }
    }

    pub fn maximize_state() -> (bool, bool) {
        shared().as_ref().map_or((false, false), |s| (s.hovered, s.pressed))
    }

    pub fn keep_above_taskbar(above: bool) {
        let Some(window) = shared().as_ref().map(|s| HWND(s.window as *mut _)) else { return };
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
        // SAFETY: z-order changes to a live window of this thread, which is
        // neither moved, sized nor activated by them.
        unsafe {
            if above {
                let _ = SetWindowPos(window, HWND_TOPMOST, 0, 0, 0, 0, flags);
                return;
            }
            let _ = SetWindowPos(window, HWND_NOTOPMOST, 0, 0, 0, 0, flags);
            // That leaves it on top of every ordinary window, the one that just
            // took the focus included — which would then be behind a picture
            // covering the whole screen. So it goes under that one. Not under a
            // topmost one, such as the taskbar itself: placed after a topmost
            // window, it would become topmost again.
            let front = GetForegroundWindow();
            let front_is_topmost =
                GetWindowLongW(front, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0;
            if !front.is_invalid() && front != window && !front_is_topmost {
                let _ = SetWindowPos(window, front, 0, 0, 0, 0, flags);
            }
        }
    }

    pub fn in_front() -> Option<bool> {
        let window = shared().as_ref().map(|s| HWND(s.window as *mut _))?;
        // SAFETY: a read-only query.
        Some(unsafe { GetForegroundWindow() } == window)
    }

    pub fn stay_above_taskbar() {
        let Some(window) = shared().as_ref().map(|s| HWND(s.window as *mut _)) else { return };
        if taskbar_in_front(window) {
            // SAFETY: as in `keep_above_taskbar`.
            unsafe {
                let _ = SetWindowPos(
                    window,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }

    /// Hang a fullscreen window one pixel past its monitor.
    ///
    /// # Why it must not fit exactly
    ///
    /// A topmost window that covers its monitor exactly, with nothing over it,
    /// is one Windows stops compositing and shows directly. In that mode the
    /// picture flickered between the latest frame and an older one, and frames
    /// came out three or four refreshes apart: about 32 a second on a 120 Hz
    /// screen, where the same session windowed managed 118. The main window,
    /// hidden behind, presents every frame too, and its present waited on a
    /// compositor that had gone idle.
    ///
    /// One pixel over is enough for Windows to keep compositing, and is off the
    /// screen. It goes over an edge with no monitor beyond it, so that it does
    /// not show as a line along the next screen.
    fn overhang(window: HWND, pos: &mut WINDOWPOS) {
        if pos.flags.0 & SWP_NOSIZE.0 != 0 {
            return;
        }
        let (x, y) = if pos.flags.0 & SWP_NOMOVE.0 != 0 {
            let mut current = RECT::default();
            // SAFETY: a live window and a RECT to write.
            if unsafe { GetWindowRect(window, &mut current) }.is_err() {
                return;
            }
            (current.left, current.top)
        } else {
            (pos.x, pos.y)
        };
        let rect = RECT { left: x, top: y, right: x + pos.cx, bottom: y + pos.cy };

        // SAFETY: read-only queries about the monitors.
        let fits_a_monitor = unsafe {
            let monitor = MonitorFromRect(&rect, MONITOR_DEFAULTTONULL);
            let mut info =
                MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            !monitor.is_invalid()
                && GetMonitorInfoW(monitor, &mut info).as_bool()
                && info.rcMonitor == rect
        };
        if !fits_a_monitor {
            return;
        }

        // SAFETY: as above.
        let nothing_at =
            |x, y| unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONULL) }.is_invalid();
        let (mid_x, mid_y) = ((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2);
        if nothing_at(mid_x, rect.bottom) {
            pos.cy += 1;
        } else if nothing_at(rect.right, mid_y) {
            pos.cx += 1;
        } else if nothing_at(mid_x, rect.top - 1) {
            pos.x = x;
            pos.y = y - 1;
            pos.cy += 1;
            pos.flags.0 &= !SWP_NOMOVE.0;
        } else if nothing_at(rect.left - 1, mid_y) {
            pos.x = x - 1;
            pos.y = y;
            pos.cx += 1;
            pos.flags.0 &= !SWP_NOMOVE.0;
        } else {
            // Below, whatever is there: a monitor in the middle of a grid of
            // them has nowhere better.
            pos.cy += 1;
        }
    }

    /// Whether a taskbar is in front of the window along any of its edges.
    fn taskbar_in_front(window: HWND) -> bool {
        let mut rect = RECT::default();
        // SAFETY: a live window and a RECT to write.
        if unsafe { GetWindowRect(window, &mut rect) }.is_err() {
            return false;
        }
        let (x, y) = ((rect.left + rect.right) / 2, (rect.top + rect.bottom) / 2);
        let edges = [
            (x, rect.top + 2),
            (x, rect.bottom - 3),
            (rect.left + 2, y),
            (rect.right - 3, y),
        ];
        edges.into_iter().any(|(x, y)| {
            // SAFETY: read-only queries about whatever is at a screen point.
            let hit = unsafe { GetAncestor(WindowFromPoint(POINT { x, y }), GA_ROOT) };
            hit != window && is_taskbar(hit)
        })
    }

    /// The taskbar, or one of its copies on another monitor.
    fn is_taskbar(window: HWND) -> bool {
        let mut class = [0u16; 32];
        // SAFETY: reading a window's class name into a buffer it is told the
        // length of.
        let len = unsafe { GetClassNameW(window, &mut class) }.max(0) as usize;
        let class = String::from_utf16_lossy(&class[..len]);
        class == "Shell_TrayWnd" || class == "Shell_SecondaryTrayWnd"
    }

    /// The session window, found by its title among this thread's windows.
    ///
    /// egui hands out the native handle of the main window only, and the title
    /// is the one thing about this window that the code creating it controls.
    /// It is unique: the main window is "Tain", this one "Tain — Session".
    fn find(title: &str) -> Option<HWND> {
        struct Search {
            title: Vec<u16>,
            found: Option<HWND>,
        }

        unsafe extern "system" fn visit(window: HWND, state: LPARAM) -> BOOL {
            // SAFETY: the `Search` handed to `EnumThreadWindows` below.
            let search = unsafe { &mut *(state.0 as *mut Search) };
            let mut text = [0u16; 256];
            // SAFETY: a window of this thread, read into a buffer it is told
            // the length of.
            let len = unsafe { GetWindowTextW(window, &mut text) }.max(0) as usize;
            if text[..len] == search.title[..] {
                search.found = Some(window);
                return BOOL(0);
            }
            TRUE
        }

        let mut search = Search { title: title.encode_utf16().collect(), found: None };
        // SAFETY: the pointer handed across is a `&mut Search` that outlives
        // the enumeration, and `visit` is the only thing that touches it.
        unsafe {
            let _ = EnumThreadWindows(
                GetCurrentThreadId(),
                Some(visit),
                LPARAM(std::ptr::addr_of_mut!(search) as isize),
            );
        }
        search.found
    }

    unsafe extern "system" fn procedure(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        // SAFETY: passing a message on to the next procedure in the chain,
        // unchanged.
        let default = || unsafe { DefSubclassProc(window, message, wparam, lparam) };

        match message {
            // `wparam` false asks only for a rectangle to be converted, which
            // the default frame answers correctly. Without a caption style the
            // window is fullscreen, and has no frame to adjust.
            WM_NCCALCSIZE if wparam.0 != 0 && has_caption(window) => {
                // With `wparam` true, `lparam` is this structure. Kept as a raw
                // pointer: the default procedure writes through it in between.
                let params = lparam.0 as *mut NCCALCSIZE_PARAMS;
                // SAFETY: as above, and valid for the length of the message.
                let top = unsafe { (*params).rgrc[0].top };
                let result = default();
                if result.0 != 0 {
                    return result;
                }
                // # Back to the top edge, except when maximized
                //
                // A maximized window is larger than the screen by its border on
                // every side, so that the border is off screen. The default frame
                // takes that back on the left, right and bottom; the top has to
                // be done here, or the header starts above the monitor.
                let inset = if is_zoomed(window) { border(window) } else { 0 };
                // SAFETY: as above.
                unsafe { (*params).rgrc[0].top = top + inset };
                LRESULT(0)
            }
            // Without a caption style the window is fullscreen. After the
            // default procedure, because winit's snaps a fullscreen window to
            // its monitor and would undo this.
            WM_WINDOWPOSCHANGING if !has_caption(window) => {
                let result = default();
                // SAFETY: this message's WINDOWPOS, valid until it returns.
                let pos = unsafe { &mut *(lparam.0 as *mut WINDOWPOS) };
                overhang(window, pos);
                result
            }
            WM_NCHITTEST => {
                let hit = default();
                if hit.0 as u32 != HTCLIENT || !has_caption(window) {
                    return hit;
                }
                LRESULT(hit_test(window, lparam) as isize)
            }
            WM_NCMOUSEMOVE => {
                set_hovered(window, wparam.0 as u32 == HTMAXBUTTON);
                default()
            }
            WM_NCMOUSELEAVE => {
                set_hovered(window, false);
                default()
            }
            // Ours, not the default procedure's: it would run its own modal
            // loop for the button and draw a Windows 7 maximize button over the
            // header while it did.
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wparam.0 as u32 == HTMAXBUTTON => {
                set_pressed(true);
                LRESULT(0)
            }
            WM_NCLBUTTONUP if wparam.0 as u32 == HTMAXBUTTON => {
                if set_pressed(false) {
                    // The system command rather than `ShowWindow`, so this is
                    // exactly what the native button would have done.
                    let command = if is_zoomed(window) { SC_RESTORE } else { SC_MAXIMIZE };
                    // SAFETY: posting to a live window.
                    unsafe {
                        let _ = PostMessageW(
                            window,
                            WM_SYSCOMMAND,
                            WPARAM(command as usize),
                            LPARAM(0),
                        );
                    }
                }
                LRESULT(0)
            }
            WM_NCDESTROY => {
                // SAFETY: removing the subclass this module installed.
                unsafe {
                    let _ = RemoveWindowSubclass(window, Some(procedure), SUBCLASS_ID);
                }
                let mut held = shared();
                if held.as_ref().is_some_and(|s| s.window == window.0 as usize) {
                    *held = None;
                }
                drop(held);
                default()
            }
            _ => default(),
        }
    }

    /// A point the default frame called client area, in the terms of
    /// [`Layout`].
    fn hit_test(window: HWND, lparam: LPARAM) -> u32 {
        // Screen coordinates, signed: a monitor left of or above the primary
        // has negative ones.
        let x = (lparam.0 & 0xFFFF) as i16 as i32;
        let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;

        let mut info =
            WINDOWINFO { cbSize: std::mem::size_of::<WINDOWINFO>() as u32, ..Default::default() };
        // SAFETY: a live window and a structure sized for the call.
        if unsafe { GetWindowInfo(window, &mut info) }.is_err() {
            return HTCLIENT;
        }
        let (x, y) = (x - info.rcClient.left, y - info.rcClient.top);

        // # The top border is inside the window
        //
        // The other three are still the frame's, outside the window. The top
        // one went with the caption, so a strip of the header stands in for it
        // — the same height, so it is where a hand expects it.
        if !is_zoomed(window) {
            let border = border(window);
            if y < border {
                let width = info.rcClient.right - info.rcClient.left;
                return if x < border {
                    HTTOPLEFT
                } else if x >= width - border {
                    HTTOPRIGHT
                } else {
                    HTTOP
                };
            }
        }

        let held = shared();
        let Some(state) = held.as_ref() else { return HTCLIENT };
        let ppp = state.pixels_per_point;
        match state.layout.hit(egui::pos2(x as f32 / ppp, y as f32 / ppp)) {
            Hit::Client => HTCLIENT,
            Hit::Caption => HTCAPTION,
            Hit::Maximize => HTMAXBUTTON,
        }
    }

    fn set_hovered(window: HWND, hovered: bool) {
        let wake = {
            let mut held = shared();
            let Some(state) = held.as_mut() else { return };
            if state.hovered == hovered {
                return;
            }
            state.hovered = hovered;
            if !hovered {
                state.pressed = false;
            }
            state.wake.clone()
        };

        if hovered {
            // Windows only says the pointer has left the non-client area when
            // asked to, and leaving straight off the edge of the window would
            // otherwise leave the button lit.
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE | TME_NONCLIENT,
                hwndTrack: window,
                dwHoverTime: 0,
            };
            // SAFETY: a live window and a structure sized for the call.
            unsafe {
                let _ = TrackMouseEvent(&mut track);
            }
        }
        // Outside the lock. Mouse messages are posted, so this runs from the
        // message loop and never inside an egui frame.
        wake.request_repaint_of(egui::ViewportId::ROOT);
    }

    /// Set the pressed state, returning what it was.
    fn set_pressed(pressed: bool) -> bool {
        let (was, wake) = {
            let mut held = shared();
            let Some(state) = held.as_mut() else { return false };
            (std::mem::replace(&mut state.pressed, pressed), state.wake.clone())
        };
        wake.request_repaint_of(egui::ViewportId::ROOT);
        was
    }

    fn has_caption(window: HWND) -> bool {
        // SAFETY: reading a live window's style.
        let style = unsafe { GetWindowLongW(window, GWL_STYLE) } as u32;
        style & WS_CAPTION.0 == WS_CAPTION.0
    }

    fn is_zoomed(window: HWND) -> bool {
        // SAFETY: reading a live window's state.
        unsafe { IsZoomed(window) }.as_bool()
    }

    /// How thick the resize border is, in this window's pixels.
    fn border(window: HWND) -> i32 {
        // SAFETY: read-only queries.
        unsafe {
            let dpi = GetDpiForWindow(window);
            GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi)
                + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
        }
    }
}

/// Nothing to take over: elsewhere the window keeps its own decorations.
#[cfg(not(target_os = "windows"))]
mod platform {
    use super::Layout;

    pub fn attached() -> bool {
        false
    }

    pub fn attach(_ctx: &egui::Context, _title: &str) -> bool {
        false
    }

    pub fn publish(_layout: Layout, _pixels_per_point: f32) {}

    pub fn maximize_state() -> (bool, bool) {
        (false, false)
    }

    pub fn keep_above_taskbar(_above: bool) {}

    pub fn stay_above_taskbar() {}

    pub fn in_front() -> Option<bool> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Layout {
        Layout {
            caption: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(800.0, 36.0))),
            controls: vec![
                // A tab, and the tools.
                Rect::from_min_size(egui::pos2(8.0, 5.0), egui::vec2(160.0, 26.0)),
                Rect::from_min_size(egui::pos2(400.0, 5.0), egui::vec2(250.0, 26.0)),
            ],
            maximize: Some(Rect::from_min_size(egui::pos2(708.0, 0.0), egui::vec2(46.0, 36.0))),
        }
    }

    #[test]
    fn empty_header_drags_the_window() {
        assert_eq!(header().hit(egui::pos2(300.0, 18.0)), Hit::Caption);
        // Above and below a control is still empty header.
        assert_eq!(header().hit(egui::pos2(50.0, 2.0)), Hit::Caption);
    }

    #[test]
    fn controls_in_the_header_stay_clickable() {
        assert_eq!(header().hit(egui::pos2(50.0, 18.0)), Hit::Client);
        assert_eq!(header().hit(egui::pos2(500.0, 18.0)), Hit::Client);
    }

    #[test]
    fn the_maximize_button_is_named_as_one() {
        assert_eq!(header().hit(egui::pos2(730.0, 18.0)), Hit::Maximize);
    }

    #[test]
    fn below_the_header_is_the_picture() {
        assert_eq!(header().hit(egui::pos2(300.0, 200.0)), Hit::Client);
    }

    #[test]
    fn fullscreen_has_no_caption_at_all() {
        let layout = Layout::default();
        assert_eq!(layout.hit(egui::pos2(300.0, 18.0)), Hit::Client);
    }
}
