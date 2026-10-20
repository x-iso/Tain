//! A separate window holding one tab per live session.
//!
//! Sessions used to render inside the main window, which caused two problems
//! that a second window removes rather than works around:
//!
//! - Fullscreen still showed the app header, which is useless once a session is
//!   up. A dedicated window has no header to hide.
//! - Disconnecting while fullscreen left the *main* window stuck fullscreen with
//!   nothing to click. Fullscreen now belongs to a window that simply closes.
//!
//! It is an **immediate** viewport rather than a deferred one, because the
//! render callback needs `&mut` access to the sessions it draws; a deferred
//! viewport's callback must be `Send + Sync + 'static`.
//!
//! # One row
//!
//! Windowed, the tabs and the tools share the row Windows would have spent on
//! the window's title, with minimize, maximize and close at the end of it; see
//! `title_bar` for how that row is taken from Windows. In fullscreen there is
//! no header at all, and the same row comes down over the picture when the
//! pointer reaches the top edge.

use egui::{Context, RichText, ViewportId};

use tain_core::i18n;
use tain_core::t;
use tain_core::protocol::{
    InputEvent, KeyEvent, MouseButtonEvent, MouseEvent, ScrollEvent, TouchEvent, TouchPhase,
    WHEEL_DELTA,
};

use super::immersive::Immersive;
use super::keymap;
use super::session::GuestSession;
use super::settings::{ScalingMode, Settings};
use super::theme::{self, *};
use super::title_bar::{self, TitleBar};

/// egui reports scroll in points; the wire uses Windows wheel-delta units.
/// One notch is roughly 50 points on a typical mouse.
const POINTS_PER_DETENT: f32 = 50.0;

/// Turn egui's scroll delta into the protocol's.
///
/// # The horizontal axis is the other way round
///
/// The two conventions agree vertically and disagree horizontally, which is
/// why this was wrong in only one axis and went unnoticed until somebody used
/// a trackpad. winit, which is where egui's deltas come from, documents them as
///
/// > Positive values indicate that the content that is being scrolled should
/// > move right and down (revealing more content left and up).
///
/// So positive `y` means the content moves down, revealing what was above it —
/// scrolling *up*, which is what `MOUSEEVENTF_WHEEL` calls positive too. But
/// positive `x` means the content moves right, revealing what was to its
/// *left* — scrolling left, where `MOUSEEVENTF_HWHEEL` calls rightward
/// positive.
///
/// The protocol's own contract is "positive is right", so the conversion
/// happens here rather than in the injector: the wire keeps one meaning and
/// only the side that speaks egui has to know egui's.
fn scroll_to_wheel(delta: egui::Vec2) -> ScrollEvent {
    let to_wheel = |points: f32| (points / POINTS_PER_DETENT * WHEEL_DELTA as f32) as i16;
    ScrollEvent { dx: -to_wheel(delta.x), dy: to_wheel(delta.y) }
}

/// Height of the header, and of the bar that stands in for it in fullscreen.
const BAR_H: f32 = 36.0;

/// What the session window is asking the app to do.
#[derive(Default)]
pub struct SessionWindowActions {
    /// Tab ids to disconnect and close.
    pub close: Vec<u64>,
    /// The window itself was closed; every session should end.
    pub close_all: bool,
    /// Hosts renamed from their tab, as endpoint hex and the new name. The
    /// name belongs to the remembered host, not the tab, so the card under
    /// Previous connections takes it too.
    pub rename: Vec<(String, String)>,
}

/// A session's name being edited where it stands.
struct Rename {
    tab_id: u64,
    text: String,
    /// Until the field has been drawn once, which is when it takes the focus
    /// and selects the old name, so typing replaces it.
    fresh: bool,
}

/// Handle session shortcuts against this window's own input.
///
/// Returns the keys consumed, which must not be forwarded to the host.
/// Shortcuts live here rather than in the main window because they only make
/// sense while a session has focus — and an immediate viewport has its own
/// `Context`, so this is where its key presses actually arrive.
fn handle_shortcuts(
    ctx: &Context,
    window: &mut SessionWindow,
    settings: &Settings,
    immersive: &mut Immersive,
    mut active: Option<&mut GuestSession>,
    actions: &mut SessionWindowActions,
) -> Vec<egui::Key> {
    use super::shortcuts::ShortcutAction;

    let bindings = &settings.shortcuts;
    let mut consumed = Vec::new();

    // # Handed to the hook, every frame
    //
    // While immersive mode is on the hook swallows every key, so egui never
    // sees that Ctrl or Shift is held and `bindings.triggered` below can never
    // match. The hook has to recognise the way out itself, and it can only do
    // that if it has been told the current binding — which the user can change
    // in settings while a session is running.
    let immersive_binding = bindings.get(ShortcutAction::ToggleImmersive);
    if let Some(vk) = immersive_binding.egui_key().and_then(keymap::vk_for) {
        let mut mods = 0;
        if immersive_binding.ctrl {
            mods |= super::immersive::MOD_CTRL;
        }
        if immersive_binding.shift {
            mods |= super::immersive::MOD_SHIFT;
        }
        if immersive_binding.alt {
            mods |= super::immersive::MOD_ALT;
        }
        immersive.set_escape_shortcut(vk, mods);
    }

    // The hook reports this rather than egui, for the reason above.
    if immersive.escape_requested() {
        immersive.set_active(false);
        // And the pointer with it. The hook's shortcut is the one way out that
        // is guaranteed to work while every other key is being swallowed, so
        // it has to free *everything* immersive mode took — otherwise it ends
        // the mode and leaves the cursor still hidden and still pinned to the
        // middle of the picture, which looks exactly like the escape failing.
        if let Some(session) = active.as_mut() {
            session.pointer_captured = false;
        }
    }

    if let Some(action) = ctx.input(|i| bindings.triggered(i)) {
        match action {
            ShortcutAction::Disconnect => {
                if let Some(session) = active.as_ref() {
                    actions.close.push(session.tab_id);
                }
            }
            ShortcutAction::ToggleFullscreen => window.toggle_fullscreen(),
            ShortcutAction::ToggleToolbar => window.show_toolbar = !window.show_toolbar,
            ShortcutAction::ToggleImmersive => immersive.set_active(!immersive.is_active()),
            ShortcutAction::ToggleCapture => {
                if let Some(session) = active.as_mut() {
                    session.pointer_captured = !session.pointer_captured;
                }
            }
        }
        if let Some(key) = bindings.get(action).egui_key() {
            consumed.push(key);
        }
    }

    // Escape always leaves fullscreen, regardless of bindings — it is the
    // universal escape hatch. It deliberately never disconnects: a reflexive
    // press should not destroy a session. While a tab is being renamed it
    // cancels the rename instead, and only that.
    if window.fullscreen
        && window.renaming.is_none()
        && ctx.input(|i| i.key_pressed(egui::Key::Escape))
    {
        window.request_fullscreen(false);
        consumed.push(egui::Key::Escape);
    }

    consumed
}

/// Persistent state for the session window.
pub struct SessionWindow {
    pub active_tab: usize,
    pub fullscreen: bool,
    pub show_toolbar: bool,
    /// Whether the stats occupy a second toolbar row.
    ///
    /// Off by default: the numbers matter when something is wrong, and the
    /// toggles matter always. Row one is for what you reach for.
    pub show_stats: bool,
    /// Set while the fullscreen command is in flight, so the toolbar label does
    /// not flicker before the OS catches up.
    pending_fullscreen: Option<bool>,
    /// Where the toolbar was last drawn, in screen points.
    ///
    /// Kept so pointer input can be withheld from it — the toolbar sits on top
    /// of the video, and without this a click on a button was also delivered to
    /// whatever was under it on the host.
    toolbar_rect: Option<egui::Rect>,
    /// The window's own caption; see `title_bar`.
    title_bar: TitleBar,
    /// How wide the tools were last frame.
    ///
    /// They are laid out from the right edge, and egui lays a row out left to
    /// right in one pass, so where the row starts depends on how long it turned
    /// out to be. `None` until measured: that frame is drawn invisibly rather
    /// than in the wrong place.
    tools_width: Option<f32>,
    renaming: Option<Rename>,
    /// Whether the rename field was drawn this frame. One that was not — its
    /// tab closed, or the fullscreen bar went away under it — has lost its
    /// focus without anyone pressing anything, and is dropped.
    rename_drawn: bool,
}

impl Default for SessionWindow {
    fn default() -> Self {
        Self {
            active_tab: 0,
            fullscreen: false,
            show_toolbar: true,
            show_stats: false,
            pending_fullscreen: None,
            toolbar_rect: None,
            title_bar: TitleBar::default(),
            tools_width: None,
            renaming: None,
            rename_drawn: false,
        }
    }
}

impl SessionWindow {
    /// Reset window chrome when the last session goes away.
    ///
    /// Leaving `fullscreen` set would make the next session open fullscreen for
    /// no reason the user asked for.
    pub fn reset(&mut self) {
        self.active_tab = 0;
        self.fullscreen = false;
        self.pending_fullscreen = None;
        self.show_toolbar = true;
        // The window goes with the last session, and the next one is a new
        // window whose caption has to be taken over again.
        self.title_bar = TitleBar::default();
        self.tools_width = None;
        self.renaming = None;
    }

    pub fn request_fullscreen(&mut self, on: bool) {
        self.pending_fullscreen = Some(on);
    }

    pub fn toggle_fullscreen(&mut self) {
        self.request_fullscreen(!self.fullscreen);
    }

    /// Draw the window and everything in it.
    pub fn show(
        &mut self,
        parent: &Context,
        sessions: &mut [GuestSession],
        settings: &Settings,
        immersive: &mut Immersive,
    ) -> SessionWindowActions {
        let mut actions = SessionWindowActions::default();
        if sessions.is_empty() {
            return actions;
        }
        self.active_tab = self.active_tab.min(sessions.len() - 1);

        // A session is open, so the keys egui cannot express are worth taking
        // from the hook. Narrow and non-destructive — see `Immersive`. Switched
        // off with the last session, in `after_sessions_changed`.
        immersive.set_forwarding(true);

        let title =
            tain_core::i18n::plural("session.window_title", sessions.len() as u64, &[]);

        let builder = self.title_bar.builder(
            egui::ViewportBuilder::default()
                .with_title(title.clone())
                .with_inner_size([1280.0, 760.0])
                .with_min_inner_size([480.0, 320.0]),
        );

        parent.show_viewport_immediate(
            ViewportId::from_hash_of("tain-sessions"),
            builder,
            |ctx, _class| {
                let window_buttons = self.title_bar.update(ctx, &title);

                if let Some(on) = self.pending_fullscreen.take() {
                    self.fullscreen = on;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
                }

                // Closing the window ends every session in it.
                if ctx.input(|i| i.viewport().close_requested()) {
                    actions.close_all = true;
                }

                // The hook reads this rather than asking Windows itself.
                immersive.publish_forward_gate();
                immersive.log_foreground();

                // If something else came to the front while immersive mode was
                // on, end it and let go of everything the host thinks is held.
                if immersive.end_if_backgrounded()
                    && let Some(session) = sessions.get_mut(self.active_tab)
                {
                    session.release_held_keys();
                    for event in session.modifiers_mut().release_all() {
                        session.send_input(InputEvent::Key(event));
                    }
                    // The pointer too: something else is in front, and a
                    // window that is no longer being used must not still be
                    // holding this machine's cursor in the middle of itself.
                    session.pointer_captured = false;
                }

                // Read before the call: `self` goes in mutably, so the index
                // cannot be fetched from it while the slice is being indexed.
                let active_tab = self.active_tab;
                let reserved = handle_shortcuts(
                    ctx,
                    self,
                    settings,
                    immersive,
                    sessions.get_mut(active_tab),
                    &mut actions,
                );

                self.render_contents(
                    ctx,
                    sessions,
                    immersive,
                    &reserved,
                    &mut actions,
                    window_buttons,
                );

                // Keep the pointer confined to *this* window while immersive.
                if immersive.is_active() {
                    // No focus test here any more. Every attempt to suspend
                    // capture when this app was not in front failed, because
                    // the keys being captured are the ones that change which
                    // app is in front — see the note on `Immersive`.
                    //
                    // Staying ahead of any other application that hooks the
                    // same keys is the hook thread's own job, on its own timer —
                    // it must not depend on how often this window repaints.
                    let rect =
                        ctx.input(|i| i.viewport().inner_rect.or(i.viewport().outer_rect));
                    if let Some(rect) = rect {
                        immersive.confine_cursor(rect, ctx.pixels_per_point());
                    }
                }
            },
        );

        actions
    }

    fn render_contents(
        &mut self,
        ctx: &Context,
        sessions: &mut [GuestSession],
        immersive: &mut Immersive,
        reserved: &[egui::Key],
        actions: &mut SessionWindowActions,
        window_buttons: bool,
    ) {
        // Decided before anything is drawn, so the frame that ends a rename —
        // Enter, which is also a key press — does not send that Enter on to
        // the host as well.
        let typing = self.renaming.is_some();
        self.rename_drawn = false;

        if self.fullscreen {
            // No header, so nothing for the frame to treat as one. The window
            // has no caption style in fullscreen either, so this is belt and
            // braces.
            self.title_bar.publish(ctx, title_bar::Layout::default());
        } else {
            egui::TopBottomPanel::top("session-header")
                .exact_height(BAR_H)
                .show_separator_line(false)
                .frame(egui::Frame::none().fill(HEADER_BG))
                .show(ctx, |ui| {
                    let row = ui.max_rect();
                    let mut layout =
                        self.render_bar(ui, row, sessions, immersive, actions, window_buttons);
                    // While a menu is open, a click on empty header is how it
                    // gets dismissed, so the header is all egui's until then.
                    if !ctx.memory(|m| m.any_popup_open()) {
                        layout.caption = Some(row);
                    }
                    self.title_bar.publish(ctx, layout);
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                let Some(session) = sessions.get_mut(self.active_tab) else { return };
                session.upload_pending_frame(ctx);

                let viewport = ui.max_rect();
                // In physical pixels, which is what the host would have to
                // encode — points would under-report on a scaled display.
                session.report_viewport(viewport.size(), ctx.pixels_per_point());
                // Bind the id and size up front: `auto_pan` needs `&mut session`
                // while the texture handle borrows it immutably.
                // Two ways a frame reaches the screen. A texture egui owns,
                // uploaded from pixels read back off the GPU; or the decoder's
                // own Direct3D surface, drawn where it already is. The second
                // one has no egui texture id, so the size travels beside it.
                #[cfg(target_os = "windows")]
                let framed = session
                    .gpu_frame()
                    .map(|(_, src)| (None, src))
                    .or_else(|| session.texture().map(|t| (Some(t.id()), t.size_vec2())));
                #[cfg(not(target_os = "windows"))]
                let framed = session.texture().map(|t| (Some(t.id()), t.size_vec2()));

                match framed {
                    Some((texture_id, src)) => {
                        // Pan before drawing, so the frame reflects this cursor
                        // position rather than lagging one frame behind it.
                        let pointer = ctx.pointer_latest_pos();
                        let dt = ctx.input(|i| i.stable_dt).min(0.1);
                        session.auto_pan(viewport, src, pointer, dt);
                        let panning = session.scaling == ScalingMode::Actual
                            && theme::pan_limit(viewport.size(), src) != egui::Vec2::ZERO;

                        let rect =
                            theme::scaled_rect(session.scaling, viewport, src, session.pan, ctx.pixels_per_point());
                        let painter = ui.painter_at(viewport);
                        match texture_id {
                            Some(texture_id) => {
                                painter.image(
                                    texture_id,
                                    rect,
                                    egui::Rect::from_min_max(
                                        egui::pos2(0.0, 0.0),
                                        egui::pos2(1.0, 1.0),
                                    ),
                                    egui::Color32::WHITE,
                                );
                            }
                            // Drawn by our own shader, because the frame is a
                            // Direct3D surface rather than anything egui knows
                            // about. Everything after this — the pointer, the
                            // toolbar — is ordinary egui on top of it.
                            #[cfg(target_os = "windows")]
                            None => {
                                if let Some(callback) = session.gpu_paint(rect) {
                                    painter.add(callback);
                                }
                            }
                            #[cfg(not(target_os = "windows"))]
                            None => {}
                        }

                        // After the video, before the input hit-rect: the
                        // pointer belongs on top of the picture and must not
                        // intercept anything.
                        if session.show_remote_cursor {
                            draw_remote_cursor(ui, session, rect, src);
                        }

                        let resp =
                            ui.allocate_rect(rect.intersect(viewport), egui::Sense::click_and_drag());
                        // # The toolbar is not part of the remote screen
                        //
                        // Without this, clicking Disconnect or a toggle also
                        // sent that click to the host — the toolbar is drawn
                        // over the video, and the video's hit rect covered it.
                        // Pressing a button here would press whatever happened
                        // to be underneath it on the other machine.
                        let over_toolbar = ui
                            .ctx()
                            .pointer_latest_pos()
                            .is_some_and(|p| self.toolbar_rect.is_some_and(|r| r.contains(p)));
                        // # Keyboard first, deliberately
                        //
                        // A click carries no modifier state on the wire: the
                        // host reproduces Ctrl+click by having Ctrl actually
                        // held when the click is injected. So if the modifier
                        // and the click land in the same frame, sending the
                        // pointer first delivers a bare click and then presses
                        // Ctrl afterwards — which is every modified click, on
                        // the frame it happens.
                        // Not while a name is being typed: those keys are for
                        // the rename field, and the host would get them all.
                        if session.sends_keyboard() && !typing {
                            forward_keyboard(session, ui, immersive, reserved);
                        }
                        if session.sends_mouse() && !over_toolbar {
                            forward_pointer(session, ui, &resp, rect);
                        } else {
                            // Whatever stopped the forwarding — the host
                            // withdrawing control, a finger sliding onto the
                            // toolbar — the contacts it already sent are still
                            // pressed against the host's screen, and this is
                            // the last chance to lift them.
                            release_touches(session);
                        }

                        // Panning is driven by the pointer, not by frames, so a
                        // still screen would otherwise scroll one step and stop.
                        if panning {
                            ctx.request_repaint();
                        }
                    }
                    None => render_waiting(ui, session),
                }

                // Over the picture, whether or not there is one. A window that
                // was minimised mid-session leaves its last frame on screen,
                // and a frozen picture with nothing over it is indistinguishable
                // from a dead link — which is the whole reason the host bothers
                // to say what happened.
                render_shared_window_notice(ui, session, viewport);

                self.render_overlay(ui, viewport, sessions, immersive, actions);
            });

        if !self.rename_drawn {
            self.renaming = None;
        }
    }

    /// The row of tabs and tools: the window's header, or in fullscreen the
    /// bar that stands in for it.
    ///
    /// Returns where everything clickable in it is, which the frame has to
    /// leave alone when it decides what drags the window. The caption itself is
    /// the caller's to fill in: only the header has one.
    fn render_bar(
        &mut self,
        ui: &mut egui::Ui,
        row: egui::Rect,
        sessions: &mut [GuestSession],
        immersive: &mut Immersive,
        actions: &mut SessionWindowActions,
        window_buttons: bool,
    ) -> title_bar::Layout {
        const PAD: f32 = 8.0;
        /// Always left between the tabs and the tools, so there is somewhere
        /// to take hold of the window however many tabs there are.
        const GRIP: f32 = 32.0;

        let mut layout = title_bar::Layout::default();
        let buttons = if window_buttons { 3.0 * title_bar::BUTTON_W } else { 0.0 };
        let tools_right = row.right() - buttons - PAD;
        let tools_left = (tools_right - self.tools_width.unwrap_or(0.0)).max(row.left() + PAD);

        if let Some(session) = sessions.get_mut(self.active_tab) {
            let mut tools = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt("session-tools")
                    .max_rect(egui::Rect::from_x_y_ranges(tools_left..=tools_right, row.y_range()))
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            // A window too narrow for them clips the tools, rather than letting
            // them run on under the window buttons.
            let clip = tools.clip_rect();
            tools.set_clip_rect(egui::Rect::from_min_max(
                clip.min,
                egui::pos2(clip.max.x.min(tools_right), clip.max.y),
            ));
            if self.tools_width.is_none() {
                tools.set_invisible();
            }
            // Only the fullscreen bar is over video; the header is gray.
            theme::chips_over(
                &mut tools,
                if self.fullscreen { theme::Backdrop::Video } else { theme::Backdrop::Header },
            );

            self.render_tools(&mut tools, session, immersive, actions);

            let used = tools.min_rect();
            if self.tools_width.is_none_or(|w| (w - used.width()).abs() > 0.5) {
                self.tools_width = Some(used.width());
                ui.ctx().request_repaint();
            }
            layout.controls.push(used);
        }

        let space = egui::Rect::from_x_y_ranges(
            row.left() + PAD..=(tools_left - GRIP).max(row.left() + PAD),
            row.y_range(),
        );
        // Immersive mode hands every key to the host before egui sees it, so a
        // rename field opened then could never be typed into.
        let can_rename = !immersive.is_active();
        if sessions.len() > 1 {
            self.render_tabs(ui, space, sessions, actions, can_rename, &mut layout.controls);
        } else if let Some(session) = sessions.first() {
            let title = self.render_title(ui, space, session, actions, can_rename);
            layout.controls.push(title);
        }

        // Last, so that they paint over anything that overflowed toward them.
        if window_buttons {
            self.render_window_buttons(ui, row, &mut layout);
        }
        layout
    }

    /// The sessions, one tab each, in `space`. Double-clicking one renames it.
    ///
    /// Only drawn for two or more: one tab is a control with nothing to switch
    /// to, and its name is shown anyway — see `render_title`.
    fn render_tabs(
        &mut self,
        ui: &mut egui::Ui,
        space: egui::Rect,
        sessions: &[GuestSession],
        actions: &mut SessionWindowActions,
        can_rename: bool,
        controls: &mut Vec<egui::Rect>,
    ) {
        const TAB_H: f32 = 26.0;
        const GAP: f32 = 4.0;
        const MAX_W: f32 = 220.0;
        /// Dot, its margins, and the close button with its own: everything in
        /// a tab but the name.
        const CHROME_W: f32 = 48.0;

        // Shared out equally once they no longer fit, the way browsers do it:
        // a tab that kept its width would be the only one left readable, at
        // the expense of the ones that then cannot be found.
        let count = sessions.len() as f32;
        let share = ((space.width() - GAP * (count - 1.0)) / count).max(0.0);
        let painter = ui.painter_at(space);
        let mut x = space.left();

        for (index, session) in sessions.iter().enumerate() {
            let selected = index == self.active_tab;
            let renaming = self.renaming.as_ref().is_some_and(|r| r.tab_id == session.tab_id);
            let text = RichText::new(&session.label).color(if selected { TEXT } else { TEXT_DIM });
            let natural = egui::WidgetText::from(text.clone())
                .into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Body)
                .size()
                .x
                + CHROME_W;
            // A name being typed gets all the room a tab may have, so the field
            // is not as cramped as the name it is replacing.
            let width = if renaming { MAX_W } else { natural.min(MAX_W) }.min(share);

            let rect = egui::Rect::from_min_size(
                egui::pos2(x, space.center().y - TAB_H / 2.0),
                egui::vec2(width, TAB_H),
            );
            x += width + GAP;
            let close_rect = egui::Rect::from_center_size(
                egui::pos2(rect.right() - 13.0, rect.center().y),
                egui::vec2(18.0, 18.0),
            );

            // The tab first and its close button second: where two widgets
            // overlap, egui gives the click to the one added last.
            let tab = ui.interact(
                rect.intersect(space),
                ui.id().with(("session-tab", session.tab_id)),
                egui::Sense::click(),
            );
            let close = ui.interact(
                close_rect.intersect(space),
                ui.id().with(("session-tab-close", session.tab_id)),
                egui::Sense::click(),
            );

            let fill = if selected {
                CARD_BG_HOVER
            } else if tab.contains_pointer() {
                CARD_INNER_BG
            } else {
                egui::Color32::TRANSPARENT
            };
            painter.rect_filled(rect, 6.0, fill);
            painter.circle_filled(
                egui::pos2(rect.left() + 12.0, rect.center().y),
                3.5,
                status_colour(session),
            );

            let text_left = rect.left() + 22.0;
            let text_rect = egui::Rect::from_min_max(
                egui::pos2(text_left, rect.top() + 2.0),
                egui::pos2((close_rect.left() - 4.0).max(text_left), rect.bottom() - 2.0),
            );
            if can_rename && tab.double_clicked() {
                self.start_rename(session);
            }
            let mut elided = false;
            if self.renaming.as_ref().is_some_and(|r| r.tab_id == session.tab_id) {
                self.render_rename(ui, text_rect.intersect(space), session, actions);
            } else {
                let galley = egui::WidgetText::from(text).into_galley(
                    ui,
                    Some(egui::TextWrapMode::Truncate),
                    text_rect.width(),
                    egui::TextStyle::Body,
                );
                elided = galley.elided;
                painter.galley(
                    egui::pos2(text_left, rect.center().y - galley.size().y / 2.0),
                    galley,
                    TEXT,
                );
            }

            if close.hovered() {
                painter.circle_filled(close_rect.center(), 9.0, egui::Color32::from_white_alpha(24));
            }
            let ink = if close.hovered() { TEXT } else { TEXT_DIM };
            cross(&painter, close_rect.center(), 3.5, egui::Stroke::new(1.2, ink));

            let close_clicked = close.clicked();
            close.on_hover_text(t!("session.close_tab"));
            let tab_clicked = tab.clicked();
            if elided {
                tab.on_hover_text(&session.label);
            }
            if close_clicked {
                actions.close.push(session.tab_id);
            } else if tab_clicked {
                self.active_tab = index;
            }
            controls.push(rect.intersect(space));
        }
    }

    /// The session's name where the tabs would be, when there is only one.
    ///
    /// Dragging it moves the window, as dragging a title does, and
    /// double-clicking renames the session as it would a tab. Returns where it
    /// is, which the frame leaves to egui rather than treating as caption.
    fn render_title(
        &mut self,
        ui: &mut egui::Ui,
        space: egui::Rect,
        session: &GuestSession,
        actions: &mut SessionWindowActions,
        can_rename: bool,
    ) -> egui::Rect {
        let middle = space.center().y;
        let painter = ui.painter_at(space);
        painter.circle_filled(egui::pos2(space.left() + 6.0, middle), 3.5, status_colour(session));
        let text_left = space.left() + 16.0;
        let field = egui::Rect::from_x_y_ranges(
            text_left..=space.right().max(text_left),
            (middle - 11.0)..=(middle + 11.0),
        );

        if self.renaming.as_ref().is_some_and(|r| r.tab_id == session.tab_id) {
            self.render_rename(ui, field, session, actions);
            return field;
        }

        let galley = egui::WidgetText::from(RichText::new(&session.label).color(TEXT)).into_galley(
            ui,
            Some(egui::TextWrapMode::Truncate),
            (space.right() - text_left).max(0.0),
            egui::TextStyle::Body,
        );
        let rect = egui::Rect::from_min_max(
            egui::pos2(space.left(), middle - 12.0),
            egui::pos2((text_left + galley.size().x + 4.0).min(space.right()), middle + 12.0),
        );

        // # A control, so that it can be double-clicked
        //
        // As part of the caption it would drag the window for free, but
        // Windows would also take the double-click, to maximize. So egui takes
        // it, and hands a drag back to Windows' own move loop, which snaps the
        // same way a caption drag does.
        let resp = ui.interact(
            rect,
            ui.id().with(("session-title", session.tab_id)),
            egui::Sense::click_and_drag(),
        );
        if resp.drag_started() && !self.fullscreen {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }

        // # The field is drawn this frame, not the next
        //
        // A rename whose field was not drawn in a frame is dropped at the end of
        // it — see `rename_drawn` — and the frame that saw the double-click is
        // one of those frames. Deciding after the name had been painted, as this
        // first did, opened the rename and closed it again before anything
        // could be seen.
        if can_rename && resp.double_clicked() {
            self.start_rename(session);
            self.render_rename(ui, field, session, actions);
            return field;
        }

        let elided = galley.elided;
        painter.galley(egui::pos2(text_left, middle - galley.size().y / 2.0), galley, TEXT);
        if elided {
            resp.on_hover_text(&session.label);
        }
        rect
    }

    fn start_rename(&mut self, session: &GuestSession) {
        self.renaming =
            Some(Rename { tab_id: session.tab_id, text: session.label.clone(), fresh: true });
    }

    /// The field that stands in for a session's name while it is renamed.
    ///
    /// Enter or clicking away keeps the new name; Escape, or leaving it empty,
    /// keeps the old one.
    fn render_rename(
        &mut self,
        ui: &mut egui::Ui,
        rect: egui::Rect,
        session: &GuestSession,
        actions: &mut SessionWindowActions,
    ) {
        self.rename_drawn = true;
        let Some(rename) = self.renaming.as_mut() else { return };

        let id = ui.id().with(("session-rename", session.tab_id));
        let mut field = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );
        let mut output = egui::TextEdit::singleline(&mut rename.text)
            .id(id)
            .text_color(TEXT)
            .margin(egui::vec2(4.0, 2.0))
            .desired_width(rect.width())
            .show(&mut field);

        let mut finished = false;
        if rename.fresh {
            rename.fresh = false;
            output.response.request_focus();
            let all = egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(rename.text.chars().count()),
            );
            output.state.cursor.set_char_range(Some(all));
            output.state.store(ui.ctx(), id);
        } else if output.response.lost_focus() {
            finished = true;
            let cancelled = ui.input(|i| i.key_pressed(egui::Key::Escape));
            let name = rename.text.trim();
            if !cancelled && !name.is_empty() && name != session.label {
                actions.rename.push((session.endpoint_hex.clone(), name.to_string()));
            }
        }
        if finished {
            self.renaming = None;
        }
    }

    /// What is drawn over the picture: in fullscreen, the bar that stands in
    /// for the header; windowed, only the stats row, once asked for.
    fn render_overlay(
        &mut self,
        ui: &mut egui::Ui,
        viewport: egui::Rect,
        sessions: &mut [GuestSession],
        immersive: &mut Immersive,
        actions: &mut SessionWindowActions,
    ) {
        // How close to the top edge counts as reaching for the toolbar.
        const REVEAL_BAND: f32 = 90.0;

        if !self.fullscreen {
            // The header holds the bar, so the picture is left alone apart
            // from the numbers.
            self.toolbar_rect = None;
            if self.show_stats
                && let Some(session) = sessions.get(self.active_tab)
            {
                let row = egui::Rect::from_min_size(
                    viewport.left_top(),
                    egui::vec2(viewport.width(), BAR_H),
                );
                paint_toolbar_backdrop(ui.painter(), row);
                render_stats_in(ui, row, session);
                self.toolbar_rect = Some(row);
            }
            return;
        }

        // A name being typed into the bar keeps it down: the pointer has no
        // reason to stay up there while the keyboard is busy.
        let near_top = self.renaming.is_some()
            || ui
                .ctx()
                .pointer_latest_pos()
                .is_some_and(|p| p.y < viewport.top() + REVEAL_BAND);

        // Collapsed, or not being reached for: leave a small handle that
        // appears when the pointer comes near, so the bar is discoverable
        // rather than only reachable by remembering the shortcut.
        if !self.show_toolbar || !near_top {
            self.toolbar_rect = None;
            if near_top {
                self.render_reveal_handle(ui, viewport);
            }
            return;
        }

        let rows = if self.show_stats { 2.0 } else { 1.0 };
        let bar = egui::Rect::from_min_size(
            viewport.left_top(),
            egui::vec2(viewport.width(), BAR_H * rows),
        );
        self.toolbar_rect = Some(bar);
        // # The header's row, on the fading backdrop
        //
        // The same tabs and tools as windowed, but not the header's flat fill:
        // here they sit on live video, and a solid slab across it would be the
        // header coming back.
        paint_toolbar_backdrop(ui.painter(), bar);

        // Drawn after the backdrop and before the controls, so the row's own
        // layout does not have to leave a gap for something centred.
        self.render_collapse_handle(ui, bar);

        let row = egui::Rect::from_min_size(bar.left_top(), egui::vec2(bar.width(), BAR_H));
        self.render_bar(ui, row, sessions, immersive, actions, false);

        if self.show_stats
            && let Some(session) = sessions.get(self.active_tab)
        {
            render_stats_in(ui, egui::Rect::from_min_size(row.left_bottom(), row.size()), session);
        }
    }

    /// The controls for the session on screen, left to right.
    fn render_tools(
        &mut self,
        ui: &mut egui::Ui,
        session: &mut GuestSession,
        immersive: &mut Immersive,
        actions: &mut SessionWindowActions,
    ) {
        // The toolbar is where the input toggles live, so it is also where they
        // first learn what the host permitted.
        session.seed_input_toggles();
        let tab_id = session.tab_id;

        let (w, h) = session.remote_size();
        ui.label(RichText::new(format!("{w}×{h}")).size(12.0).color(TEXT));

        // # Next to the size, not in the stats
        //
        // How the host was found belongs with what the session *is* rather than
        // with what it is doing per second: it is decided once and never
        // changes, while everything in the stats row moves. Sitting among
        // numbers that update every second also made it read as one of them.
        //
        // A relayed session is refused before it starts, so there is no "direct"
        // to report — the branch that found the host is the whole of what this
        // has to say, and the full path is on hover.
        match session.relay_warning() {
            Some(advice) => {
                ui.add_space(10.0);
                ui.label(RichText::new("⚠ relayed").size(12.0).color(AMBER))
                    .on_hover_text(advice);
            }
            None => {
                if let Some(branch) = session.found_via() {
                    ui.add_space(10.0);
                    ui.label(RichText::new(branch.label()).size(12.0).color(LIVE_GREEN))
                        .on_hover_text(session.path_summary());
                }
            }
        }

        render_display_picker(ui, session);

        ui.add_space(12.0);
        render_input_toggles(ui, session, &mut self.show_stats);

        if session.loopback {
            ui.add_space(12.0);
            ui.label(RichText::new(i18n::text("session.loopback")).size(12.0).color(AMBER))
                .on_hover_text(i18n::text("session.loopback_hint"));
        }

        ui.add_space(12.0);
        for mode in ScalingMode::ALL {
            let selected = session.scaling == mode;
            if theme::icon_toggle(
                ui,
                16.0,
                if selected { theme::IconState::On } else { theme::IconState::Off },
                match mode {
                    ScalingMode::Fit => egui::include_image!("../../assets/fit.svg"),
                    ScalingMode::Actual => egui::include_image!("../../assets/1to1.svg"),
                },
                mode.description(),
            )
            .clicked()
            {
                session.set_scaling(mode);
            }
            ui.add_space(4.0);
        }

        ui.add_space(6.0);
        let on = immersive.is_active();
        if theme::icon_toggle(
            ui,
            16.0,
            if on { theme::IconState::On } else { theme::IconState::Off },
            egui::include_image!("../../assets/immersive_mode.svg"),
            if on {
                t!("session.immersive_on")
            } else {
                t!("session.immersive_off")
            },
        )
        .clicked()
        {
            immersive.set_active(!on);
            if on {
                // Leaving immersive mode: anything the hook was holding has
                // to be let go on the host, or it stays down there forever.
                session.release_held_keys();
            }
        }

        ui.add_space(6.0);
        // Two drawings rather than one toggled state: the icon shows what the
        // click will *do*, which for a mode switch is clearer than showing
        // which mode is current.
        let full = self.fullscreen;
        if theme::icon_toggle(
            ui,
            16.0,
            theme::IconState::Off,
            if full {
                egui::include_image!("../../assets/windowed.svg")
            } else {
                egui::include_image!("../../assets/fullscreen.svg")
            },
            if full { t!("session.leave_fullscreen") } else { t!("session.go_fullscreen") },
        )
        .clicked()
        {
            self.toggle_fullscreen();
        }

        // # Disconnect only in fullscreen
        //
        // Windowed, the window's close button ends a lone session and a tab's
        // close button ends one of several, so a disconnect button would only
        // repeat one of them. Fullscreen has neither. It keeps a distinct
        // glyph because it is the one control here that ends something.
        if full {
            ui.add_space(6.0);
            if theme::icon_toggle(
                ui,
                16.0,
                theme::IconState::Off,
                egui::include_image!("../../assets/cancel.svg"),
                t!("session.disconnect"),
            )
            .clicked()
            {
                actions.close.push(tab_id);
            }
        }
    }

    /// Minimize, maximize and close, drawn the way Windows draws its own.
    ///
    /// Painted rather than set in a font: Windows' glyphs are in Segoe Fluent
    /// Icons, which egui does not have, and a few strokes on the pixel grid are
    /// crisper than any substitute at this size.
    fn render_window_buttons(
        &self,
        ui: &mut egui::Ui,
        row: egui::Rect,
        layout: &mut title_bar::Layout,
    ) {
        /// Windows 11's close-button red.
        const CLOSE_RED: egui::Color32 = egui::Color32::from_rgb(0xC4, 0x2B, 0x1C);
        /// Half the glyphs' width.
        const HALF: f32 = 5.0;

        let w = title_bar::BUTTON_W;
        let button = |n: f32| {
            egui::Rect::from_min_size(
                egui::pos2(row.right() - (3.0 - n) * w, row.top()),
                egui::vec2(w, row.height()),
            )
        };
        let (minimize, maximize, close) = (button(0.0), button(1.0), button(2.0));

        let ctx = ui.ctx().clone();
        let painter = ui.painter();
        // Dimmed while another window is in front, as Windows' own are.
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        let stroke = hairline(&ctx, if focused { TEXT } else { TEXT_DIM });

        let resp = ui.interact(minimize, ui.id().with("window-minimize"), egui::Sense::click());
        painter.rect_filled(minimize, 0.0, button_fill(resp.hovered(), resp.is_pointer_button_down_on()));
        let c = crisp(painter, minimize.center());
        painter.line_segment([c - egui::vec2(HALF, 0.0), c + egui::vec2(HALF, 0.0)], stroke);
        if resp.clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        layout.controls.push(minimize);

        // Windows hit-tests this one itself, for snap layouts, so egui never
        // sees the pointer over it: the frame says what state it is in, and
        // the frame clicks it.
        let (hovered, pressed) = self.title_bar.maximize_state();
        painter.rect_filled(maximize, 0.0, button_fill(hovered, pressed));
        let c = crisp(painter, maximize.center());
        if ctx.input(|i| i.viewport().maximized).unwrap_or(false) {
            // Restore: a square with another behind it, up and to the right.
            let front = egui::Rect::from_min_max(
                crisp(painter, c + egui::vec2(-HALF, -HALF + 2.0)),
                crisp(painter, c + egui::vec2(HALF - 2.0, HALF)),
            );
            painter.rect_stroke(front, 1.0, stroke);
            let back = front.translate(egui::vec2(2.0, -2.0));
            painter.line_segment([back.left_top(), back.right_top()], stroke);
            painter.line_segment([back.right_top(), back.right_bottom()], stroke);
        } else {
            let square = egui::Rect::from_min_max(
                crisp(painter, c - egui::vec2(HALF, HALF)),
                crisp(painter, c + egui::vec2(HALF, HALF)),
            );
            painter.rect_stroke(square, 1.0, stroke);
        }
        layout.maximize = Some(maximize);

        let resp = ui.interact(close, ui.id().with("window-close"), egui::Sense::click());
        let lit = resp.hovered() || resp.is_pointer_button_down_on();
        if lit {
            painter.rect_filled(close, 0.0, CLOSE_RED);
        }
        let ink = if lit { hairline(&ctx, egui::Color32::WHITE) } else { stroke };
        cross(painter, close.center(), HALF, ink);
        if resp.clicked() {
            // The same as closing it any other way: every session ends.
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        layout.controls.push(close);
    }
}

/// Draw the host's pointer over the video.
///
/// # Mapping
///
/// The position arrives in captured-frame pixels, and the video is drawn into
/// `rect` at whatever scale the mode and window size produce. Both the position
/// and the hotspot have to be scaled by the same factor, or the pointer drifts
/// away from where it actually is — most visibly at the far corners, and worst
/// in `Fit` on a window that does not match the host's aspect ratio.
fn draw_remote_cursor(
    ui: &mut egui::Ui,
    session: &mut GuestSession,
    rect: egui::Rect,
    src: egui::Vec2,
) {
    let _ = src;
    let Some((position, texture)) = session.cursor_texture(ui.ctx()) else { return };
    if !position.visible {
        // Hidden on the host — full-screen video and games do this, and a
        // remote pointer left over the top would be a ghost the user cannot
        // get rid of.
        return;
    }

    // # Scaled against the desktop, not the frame
    //
    // The position is in the host's native resolution, which is not the size of
    // the video whenever the stream has been scaled down. Using the frame size
    // here put the pointer further from where it really was the further right
    // and further down it went, and drew it too large — one wrong divisor
    // producing both symptoms.
    let desktop = egui::vec2(
        position.desktop_width.max(1) as f32,
        position.desktop_height.max(1) as f32,
    );
    let scale = egui::vec2(rect.width() / desktop.x, rect.height() / desktop.y);

    let size = texture.size_vec2() * scale;
    // Sub-pixel cursors are worse than none: below about a pixel this is noise
    // rather than a pointer.
    if size.x < 1.0 || size.y < 1.0 {
        return;
    }

    let hotspot = session.cursor_hotspot() * scale;
    let top_left = rect.min + egui::vec2(position.x as f32, position.y as f32) * scale - hotspot;
    let target = egui::Rect::from_min_size(top_left, size);
    // Clipped to the video, so a pointer at the edge does not paint over the
    // toolbar or the letterbox.
    if !rect.intersects(target) {
        return;
    }
    ui.painter_at(rect).image(
        texture.id(),
        target,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
}

/// A backdrop that fades out downwards instead of ending on a hard edge.
///
/// A flat panel over live video reads as a slab bolted on top of it; the picture
/// stops dead at a horizontal line. Fading the alpha out towards the bottom lets
/// the video come through where the bar has nothing to protect, and keeps the
/// contrast where the text and icons actually sit.
///
/// Drawn as a mesh because egui's painter has no gradient — four vertices, two
/// triangles, colour interpolated between them by the renderer for free.
fn paint_toolbar_backdrop(painter: &egui::Painter, rect: egui::Rect) {
    // # Why the bar is solid and only the edge below it fades
    //
    // It used to be one gradient across the whole bar: alpha 215 at the top
    // down to 40 at the bottom. With one row that was fine, because the row sat
    // in the opaque half. With the stats row open the numbers landed in the
    // faint half and became unreadable over a light desktop — which is the one
    // moment anybody is reading them.
    //
    // So the bar itself is a flat, near-opaque block whatever height it is, and
    // the gradient is a short strip *below* it whose only job is to stop the
    // edge cutting a hard line across the picture.
    const FADE: f32 = 16.0;
    let solid = egui::Color32::from_black_alpha(235);
    painter.rect_filled(rect, 0.0, solid);

    let fade = egui::Rect::from_min_max(
        rect.left_bottom(),
        rect.right_bottom() + egui::vec2(0.0, FADE),
    );
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(fade.left_top(), solid);
    mesh.colored_vertex(fade.right_top(), solid);
    mesh.colored_vertex(fade.left_bottom(), egui::Color32::TRANSPARENT);
    mesh.colored_vertex(fade.right_bottom(), egui::Color32::TRANSPARENT);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 2, 3);
    painter.add(mesh);
}

/// The stats row in `row`, inset the way the bar's own row is.
fn render_stats_in(ui: &mut egui::Ui, row: egui::Rect, session: &GuestSession) {
    let mut stats = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(row.shrink2(egui::vec2(12.0, 6.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    render_stats_row(&mut stats, session);
}

/// Live or not, as the dot beside a session's name says it.
fn status_colour(session: &GuestSession) -> egui::Color32 {
    if session.status().is_active() { LIVE_GREEN } else { OFFLINE_RED }
}

/// An × centred on `at`, `half` points from the middle to each corner.
fn cross(painter: &egui::Painter, at: egui::Pos2, half: f32, stroke: egui::Stroke) {
    painter.line_segment([at + egui::vec2(-half, -half), at + egui::vec2(half, half)], stroke);
    painter.line_segment([at + egui::vec2(-half, half), at + egui::vec2(half, -half)], stroke);
}

/// A window button's background: none until the pointer is on it.
fn button_fill(hovered: bool, pressed: bool) -> egui::Color32 {
    if pressed {
        egui::Color32::from_white_alpha(30)
    } else if hovered {
        egui::Color32::from_white_alpha(16)
    } else {
        egui::Color32::TRANSPARENT
    }
}

/// A line a whole number of pixels wide: one at 100%, two at 200%.
///
/// A width in points lands between pixels on any other scale, and a glyph
/// this small turns to grey smear when it does.
fn hairline(ctx: &Context, colour: egui::Color32) -> egui::Stroke {
    let ppp = ctx.pixels_per_point();
    egui::Stroke::new(ppp.round().max(1.0) / ppp, colour)
}

/// `at`, moved to the middle of the pixel it falls in, so a hairline through
/// it covers that pixel rather than half of each neighbour.
fn crisp(painter: &egui::Painter, at: egui::Pos2) -> egui::Pos2 {
    egui::pos2(painter.round_to_pixel_center(at.x), painter.round_to_pixel_center(at.y))
}

/// The tab that collapses the toolbar, and the one that brings it back.
///
/// # Why they are the same shape in the same place
///
/// Collapsing used to be an icon in the row — and the icon it used was the
/// *stats* glyph, a second copy of it, sitting next to the real stats toggle.
/// Two identical pictures side by side doing unrelated things.
///
/// The reveal tab was already the right idea: a small centred handle with a
/// chevron pointing the way the bar will move. So collapsing is the same handle
/// in the same position with the chevron reversed, and the pair reads as one
/// control that flips rather than two that have to be learned.
impl SessionWindow {
    fn render_collapse_handle(&mut self, ui: &mut egui::Ui, bar: egui::Rect) {
        let handle = egui::Rect::from_min_size(
            egui::pos2(bar.center().x - HANDLE.x / 2.0, bar.bottom() - HANDLE.y),
            HANDLE,
        );
        let resp = ui.allocate_rect(handle, egui::Sense::click());

        let alpha = if resp.hovered() { 230 } else { 120 };
        ui.painter().rect_filled(
            handle,
            egui::Rounding { nw: 4.0, ne: 4.0, sw: 0.0, se: 0.0 },
            egui::Color32::from_black_alpha(alpha),
        );
        chevron(ui, handle.center(), true);

        if resp.clicked() {
            self.show_toolbar = false;
        }
        resp.on_hover_text(t!("session.hide_toolbar"));
    }
}

/// The handle both tabs are drawn at, so the pair cannot drift apart.
const HANDLE: egui::Vec2 = egui::vec2(54.0, 14.0);

/// A chevron pointing up or down, centred on `at`.
fn chevron(ui: &egui::Ui, at: egui::Pos2, up: bool) {
    let dy = if up { -3.0 } else { 3.0 };
    let stroke = egui::Stroke::new(1.6, egui::Color32::from_white_alpha(210));
    ui.painter().line_segment([at + egui::vec2(-6.0, -dy), at + egui::vec2(0.0, dy)], stroke);
    ui.painter().line_segment([at + egui::vec2(6.0, -dy), at + egui::vec2(0.0, dy)], stroke);
}

/// A small tab at the top edge that brings the toolbar back.
///
/// Only drawn while the pointer is near the top, so it costs nothing visually
/// the rest of the time — and it means a collapsed toolbar is discoverable by
/// moving the mouse rather than only by remembering the shortcut.
impl SessionWindow {
    fn render_reveal_handle(&mut self, ui: &mut egui::Ui, viewport: egui::Rect) {
        let handle = egui::Rect::from_min_size(
            egui::pos2(viewport.center().x - HANDLE.x / 2.0, viewport.top()),
            HANDLE,
        );
        // Claimed as interactive so the click lands here rather than on the
        // remote screen underneath.
        let resp = ui.allocate_rect(handle, egui::Sense::click());
        self.toolbar_rect = Some(handle);

        let alpha = if resp.hovered() { 230 } else { 150 };
        ui.painter().rect_filled(
            handle,
            egui::Rounding { nw: 0.0, ne: 0.0, sw: 4.0, se: 4.0 },
            egui::Color32::from_black_alpha(alpha),
        );
        // Pointing down: the bar comes from above.
        chevron(ui, handle.center(), false);

        if resp.clicked() {
            self.show_toolbar = true;
        }
        resp.on_hover_text(t!("session.show_toolbar"));
    }
}

/// Mouse, keyboard and sound, as three toggles.
///
/// # Why these are here and not in settings
///
/// They were one global checkbox called "Send keystrokes to the host", which
/// read as though it were the permission — and the permission is the host's,
/// granted per session and living somewhere else entirely. As per-session
/// toggles beside the picture they are what they actually are: this end's own
/// switches, narrowing what the host already allowed.
///
/// A toggle the host has not permitted is shown disabled rather than hidden, so
/// the reason input is not working is visible instead of absent.
fn render_input_toggles(ui: &mut egui::Ui, session: &mut GuestSession, show_stats: &mut bool) {
    let grants = session.grants();

    // # Which art goes with which state
    //
    // The `*_disabled` assets are faint on purpose, which is right for "the
    // host said no" and wrong for "I turned this off" — the second is a live
    // control the user is expected to find and click again. So an off-but-
    // permitted toggle keeps the normal art and is dimmed by alpha instead.
    let mouse_state = input_state(session.loopback, grants.mouse, session.want_mouse);
    let resp = theme::icon_toggle(
        ui,
        16.0,
        mouse_state,
        if mouse_state == theme::IconState::Unavailable {
            egui::include_image!("../../assets/mouse_disabled.svg")
        } else {
            egui::include_image!("../../assets/mouse_allowed.svg")
        },
        match mouse_state {
            _ if session.loopback => t!("session.pointer_loopback"),
            theme::IconState::Unavailable => t!("session.pointer_unavailable"),
            theme::IconState::On => t!("session.pointer_on"),
            theme::IconState::Off => t!("session.pointer_off"),
        },
    );
    if resp.clicked() {
        session.want_mouse = !session.want_mouse;
    }
    // # The sensitivity lives behind the control it belongs to
    //
    // It applies to one thing — movement sent while the host has captured its
    // pointer — and that thing is what this button governs. A slider on the
    // toolbar beside it would be a permanent control for a mode most sessions
    // never enter; in the settings only, it could not be adjusted against the
    // game it is being adjusted *for*.
    //
    // So: right-click the pointer button. See `Settings::pointer_sensitivity`
    // for why this needs to be adjustable at all.
    let sensitivity_id = ui.make_persistent_id(("pointer-sensitivity", session.tab_id));
    if resp.secondary_clicked() {
        ui.memory_mut(|m| m.toggle_popup(sensitivity_id));
    }
    render_sensitivity_popup(ui, session, &resp, sensitivity_id);

    ui.add_space(4.0);
    let key_state = input_state(session.loopback, grants.keyboard, session.want_keyboard);
    let resp = theme::icon_toggle(
        ui,
        16.0,
        key_state,
        if key_state == theme::IconState::Unavailable {
            egui::include_image!("../../assets/keyboard_disabled.svg")
        } else {
            egui::include_image!("../../assets/keyboard_allowed.svg")
        },
        match key_state {
            _ if session.loopback => t!("session.keyboard_loopback"),
            theme::IconState::Unavailable => t!("session.keyboard_unavailable"),
            theme::IconState::On => t!("session.keyboard_on"),
            theme::IconState::Off => t!("session.keyboard_off"),
        },
    );
    if resp.clicked() {
        session.want_keyboard = !session.want_keyboard;
    }

    ui.add_space(4.0);
    // # Why the stats toggle sits here, between the inputs and the view
    //
    // It used to be last, past the sound slider, on the reasoning that it
    // controls the toolbar rather than the session. But the bar reads left to
    // right as *what I send* — pointer, keyboard — then *what I see* — the
    // host's pointer, its sound. The numbers are the first of the second group:
    // they are about this connection, not about anything being sent to it.
    let stats_on = *show_stats;
    if theme::icon_toggle(
        ui,
        16.0,
        if stats_on { theme::IconState::On } else { theme::IconState::Off },
        egui::include_image!("../../assets/fps-metrics.svg"),
        if stats_on { t!("session.hide_stats") } else { t!("session.show_stats") },
    )
    .clicked()
    {
        *show_stats = !stats_on;
    }

    ui.add_space(4.0);
    // The host's pointer. Separate from the input toggles above: those decide
    // whether *our* pointer reaches the host, this decides whether we can see
    // *theirs*, and the two are independent.
    let on = session.show_remote_cursor;
    if theme::icon_toggle(
        ui,
        16.0,
        if on { theme::IconState::On } else { theme::IconState::Off },
        egui::include_image!("../../assets/cursor_show.svg"),
        if on {
            t!("session.cursor_shown")
        } else {
            t!("session.cursor_hidden")
        },
    )
    .clicked()
    {
        session.show_remote_cursor = !on;
    }

    ui.add_space(4.0);
    volume_control(ui, session);
    // Applied unconditionally: the stream may have opened after the last click,
    // and it must not come up unmuted for someone who muted it.
    session.apply_sound_preference();
}

/// The mute toggle, and a volume slider that appears when the pointer is near.
///
/// # Why it appears on hover rather than living there
///
/// A slider is much wider than an icon, and volume is set once and then left —
/// so it earns its space only while someone is reaching for it.
///
/// # Why below the icon rather than beside it
///
/// The tools are laid out from the right edge. A slider opening inside the row
/// would widen it and push every control left of it sideways — the speaker
/// included, out from under the pointer, which would close the slider again the
/// frame after it opened. In a layer of its own it takes no room in the row.
///
/// The hover region is the icon *and the slider together*, remembered from the
/// last frame. Keying it off the icon alone would make the slider vanish the
/// moment the pointer moved onto it, which is the classic way to build a control
/// nobody can use.
fn volume_control(ui: &mut egui::Ui, session: &mut GuestSession) {
    let hot_id = ui.id().with(("volume-hover", session.tab_id));
    let previous: Option<egui::Rect> = ui.memory(|m| m.data.get_temp(hot_id));
    let near = match (previous, ui.ctx().pointer_latest_pos()) {
        (Some(rect), Some(pointer)) => rect.contains(pointer),
        _ => false,
    };

    // Sound has distinct art for both states, so the state is legible from the
    // glyph itself and only the chip has to say which is current.
    let on = session.want_sound;
    let icon = theme::icon_toggle(
        ui,
        16.0,
        if on { theme::IconState::On } else { theme::IconState::Off },
        if on {
            egui::include_image!("../../assets/sound_on.svg")
        } else {
            egui::include_image!("../../assets/sound_off.svg")
        },
        if on {
            t!("session.sound_on")
        } else {
            t!("session.sound_off")
        },
    );
    if icon.clicked() {
        session.want_sound = !on;
        session.apply_sound_preference();
    }

    let mut hot = icon.rect;
    if icon.hovered() || near {
        // Over the picture, which is fine: being a layer above it is also what
        // keeps the pointer on the slider from reaching the host.
        let area = egui::Area::new(hot_id.with("slider"))
            .order(egui::Order::Foreground)
            .pivot(egui::Align2::CENTER_TOP)
            .fixed_pos(icon.rect.center_bottom() + egui::vec2(0.0, 4.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::none()
                    .fill(egui::Color32::from_black_alpha(235))
                    .rounding(4.0)
                    .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                    .show(ui, |ui| {
                        ui.spacing_mut().slider_width = 64.0;
                        let slider = ui.add(
                            egui::Slider::new(&mut session.volume, 0.0..=1.0)
                                .show_value(false)
                                .custom_formatter(|_, _| String::new()),
                        );
                        if slider.changed() {
                            // Moving the slider up is an unambiguous request to
                            // hear it, so it undoes the mute rather than leaving
                            // the user dragging a control that does nothing.
                            if session.volume > 0.0 {
                                session.want_sound = true;
                            }
                            session.apply_sound_preference();
                        }
                        slider.on_hover_text(t!(
                            "session.volume",
                            percent = (session.volume * 100.0).round()
                        ));
                    });
            });
        hot = hot.union(area.response.rect);
    }
    // A little larger than the controls, so crossing the gap between them does
    // not count as leaving.
    ui.memory_mut(|m| m.data.insert_temp(hot_id, hot.expand(8.0)));
}

/// Fold a grant and a local preference into what the icon should say.
fn input_state(loopback: bool, granted: bool, wanted: bool) -> theme::IconState {
    if loopback || !granted {
        // Not the user's to choose: the host refused, or forwarding would drive
        // this machine's own pointer in a feedback loop.
        theme::IconState::Unavailable
    } else if wanted {
        theme::IconState::On
    } else {
        theme::IconState::Off
    }
}

/// Which of the host's displays to watch.
///
/// # Why it is not shown for one display
///
/// A menu with a single entry is a control that cannot do anything, and most
/// hosts have one monitor. It appears when there is a choice to make, which is
/// also the only time anyone would look for it.
///
/// The label is the display's position in the host's own arrangement rather
/// than its device name: `\\.\DISPLAY2` says nothing about *which* monitor that
/// is, while "left" and "right" match what the person at the other end sees.
fn render_display_picker(ui: &mut egui::Ui, session: &GuestSession) {
    // # A window session is one the host granted windows to, not one that
    // happens to be showing a window
    //
    // Keyed off the list rather than off `shared_window`, which names the single
    // window being watched and is `None` while the composite is on screen. Read
    // the other way round, switching to "all windows" would make the switcher
    // vanish and the *screen* picker appear in its place — offering monitors
    // this session cannot see.
    let offered = session.shared_windows();
    if offered.len() > 1 {
        render_window_picker(ui, session, session.shared_window().as_ref(), &offered);
        return;
    }

    // Every shared window has been closed. Nothing to switch between and no
    // composite to switch to — the buttons go with the windows they stood for,
    // rather than lingering as controls that do nothing. What happened is said
    // over the picture instead; see `render_shared_window_notice`.
    if session.windows_all_closed() {
        return;
    }

    if let Some(window) = session.shared_window() {
        // One window, so there is nothing to offer. Saying which window is
        // worth the space the picker would have taken; offering a choice that
        // does not exist is not.
        let named = if window.app.is_empty() {
            i18n::text("session.window_unnamed").to_string()
        } else {
            window.app.clone()
        };
        ui.add_space(12.0);
        ui.label(
            RichText::new(i18n::format("session.watching_window", &[("app", &named)]))
                .size(12.0)
                .color(TEXT_DIM),
        )
        .on_hover_text(format!(
            "{}\n\n{}",
            window.title,
            i18n::text("session.watching_window_hint")
        ));
        return;
    }

    let displays = session.displays();
    if displays.len() < 2 {
        return;
    }
    let watching = session.watching();

    use tain_core::protocol::DisplayChoice;

    ui.add_space(12.0);

    // Every screen at once, first: it is the answer to "what is on that
    // machine", and the individual screens are the refinement.
    let all = watching == Some(DisplayChoice::All);
    if theme::icon_toggle(
        ui,
        16.0,
        if all { theme::IconState::On } else { theme::IconState::Off },
        egui::include_image!("../../assets/overview.svg"),
        i18n::text("session.all_screens_hint"),
    )
    .clicked()
        && !all
    {
        session.watch_display(DisplayChoice::All);
    }
    ui.add_space(2.0);

    for (index, display) in displays.iter().enumerate() {
        let choice = DisplayChoice::One(display.id);
        let selected = watching == Some(choice);
        let mut hover = describe_display(display, &displays);
        if display.hdr {
            hover.push_str(" · HDR");
        }

        // Numbered by position in the list, from one, rather than by the
        // wire id. The id starts at zero and is an enumeration index; what
        // belongs on a button is what a person would count.
        let clicked = theme::numbered_icon_toggle(
            ui,
            16.0,
            if selected { theme::IconState::On } else { theme::IconState::Off },
            if display.virtual_display {
                egui::include_image!("../../assets/virtualdisplay.svg")
            } else {
                egui::include_image!("../../assets/monitor.svg")
            },
            index as u32 + 1,
            &hover,
        )
        .clicked();

        if clicked && !selected {
            session.watch_display(choice);
        }
        ui.add_space(2.0);
    }
}

/// The windows a host granted, as buttons to switch between.
///
/// # Why there is no "all of them" here
///
/// The screen picker opens with one, because a wall of monitors is a real
/// picture of a real desk: they sit beside each other, the host knows where, and
/// composing them reproduces that arrangement. Windows have no such
/// arrangement — they overlap, in a stack whose order changes as somebody works
/// — so a composition of them would be a layout this end invented. One at a
/// time is the only honest answer.
fn render_window_picker(
    ui: &mut egui::Ui,
    session: &GuestSession,
    showing: Option<&tain_core::protocol::SharedWindow>,
    offered: &[tain_core::protocol::SharedWindow],
) {
    use tain_core::protocol::DisplayChoice;

    ui.add_space(12.0);

    // Every shared window at once, first — the same place the screen picker
    // puts the whole desk, and for the same reason: it is the answer to "what
    // did they share with me", and the individual windows are the refinement.
    let all = session.watching() == Some(DisplayChoice::All);
    if theme::icon_toggle(
        ui,
        16.0,
        if all { theme::IconState::On } else { theme::IconState::Off },
        egui::include_image!("../../assets/multi-window.svg"),
        &i18n::format("session.all_windows_hint", &[("n", &offered.len().to_string())]),
    )
    .clicked()
        && !all
    {
        session.watch_display(DisplayChoice::All);
    }
    ui.add_space(2.0);

    for window in offered {
        // Measured against the window actually on screen rather than against
        // the last request: a host that refused a switch, or moved on its own
        // because a window closed, says so through this — and the button that
        // lights up has to be the one being watched. Nothing lights up while
        // the composite is showing, which is correct: none of them is *the*
        // window then.
        let selected = !all && showing.is_some_and(|shown| shown.id == window.id);

        // Named the way the host's own picker named it: the program, then the
        // caption that separates that program's windows from each other. The
        // caption is live for the window being watched, where the host keeps
        // re-reading it, and as it was granted for the rest.
        let caption = match showing {
            Some(shown) if selected && !shown.title.is_empty() => shown.title.clone(),
            _ => window.title.clone(),
        };
        let hover = if caption.is_empty() {
            window.app.clone()
        } else {
            format!("{}\n{caption}", window.app)
        };

        let clicked = theme::lettered_icon_toggle(
            ui,
            16.0,
            if selected { theme::IconState::On } else { theme::IconState::Off },
            // # The plain frame, not the window art
            //
            // The letter goes in the middle of the icon, and `app_window.svg`
            // has a title bar drawn across exactly that part — so the two
            // collided. Shrinking the letter to fit between them makes it
            // unreadable at 16 points, which is the size this row is.
            //
            // `empty_button.svg` is the same rounded frame with nothing inside
            // it, which is what a mark drawn *by* the caller wants: the art
            // says "a window" and leaves the middle alone.
            egui::include_image!("../../assets/empty_button.svg"),
            &window.app,
            &hover,
        )
        .clicked();

        if clicked && !selected {
            session.watch_display(DisplayChoice::Window(window.id));
        }
        ui.add_space(2.0);
    }
}

/// A display named the way the person at the host would recognise it.
///
/// Position first, because that is what identifies a monitor to a human — the
/// device name is an index into something nobody can see. Size second, because
/// two screens in the same relative position is not a thing, but two the same
/// size very much is.
fn describe_display(
    display: &tain_core::protocol::DisplayInfo,
    all: &[tain_core::protocol::DisplayInfo],
) -> String {
    // A virtual screen is named for what it is, not where it is. Nobody at the
    // host is looking at it and it has no place on a desk, so "Right" would
    // describe a position that means nothing to either end.
    let size = [
        ("w", display.width.to_string()),
        ("h", display.height.to_string()),
    ];
    let with_size = |key: &str| {
        tain_core::i18n::format(key, &[("w", &size[0].1), ("h", &size[1].1)])
    };

    if display.virtual_display {
        return with_size("session.screen_virtual");
    }
    if display.primary {
        return with_size("session.screen_main");
    }

    // Relative to the primary, which is the one at the origin.
    let where_ = match all.iter().find(|d| d.primary) {
        Some(main) if display.x + i32::from(display.width) <= main.x => "session.screen_left",
        Some(main) if display.x >= main.x + i32::from(main.width) => "session.screen_right",
        Some(main) if display.y + i32::from(display.height) <= main.y => "session.screen_above",
        Some(main) if display.y >= main.y + i32::from(main.height) => "session.screen_below",
        // Overlapping or stacked oddly: fall back to something that at least
        // distinguishes them.
        _ => {
            return tain_core::i18n::format(
                "session.screen_numbered",
                &[
                    ("n", &(display.id + 1).to_string()),
                    ("w", &size[0].1),
                    ("h", &size[1].1),
                ],
            )
        }
    };
    with_size(where_)
}

/// The numbers, on their own row when asked for.
fn render_stats_row(ui: &mut egui::Ui, session: &GuestSession) {
    let grants = session.grants();
    ui.label(
        RichText::new(super::app::describe_grants(grants.mouse, grants.keyboard))
            .size(12.0)
            .color(TEXT_DIM),
    );

    // The live numbers, not a tooltip. A session that is unusable should say
    // why on its face — the first real two-machine test of this app was
    // diagnosed from log files because the window showed nothing at all.
    ui.add_space(10.0);
    ui.label(RichText::new(session.live_stats()).size(12.0).color(TEXT_DIM))
        .on_hover_text(format!(
            "{}\n\n{}",
            session.health_detail(),
            i18n::text("session.stats_hint")
        ));

    if let Some(ms) = session.audio_buffered_ms() {
        ui.add_space(10.0);
        // The target as well as the depth. The buffer grows itself when the link
        // keeps breaking it, so a session with unexpected audio delay should be
        // able to say whether that is a stall or a cushion it had to build.
        let text = match session.audio_target_ms() {
            Some(target) => i18n::format(
                "session.audio_buffer",
                &[("ms", &ms.to_string()), ("target", &target.to_string())],
            ),
            None => i18n::format("session.audio_buffer_plain", &[("ms", &ms.to_string())]),
        };
        ui.label(RichText::new(text).size(12.0).color(TEXT_DIM))
            .on_hover_text(i18n::text("session.audio_buffer_hint"));
    }

    render_bottlenecks(ui, session.bottlenecks());
}

/// Draw one chip per stage that is currently struggling.
///
/// Named rather than merged into a single "poor connection" warning, because
/// the four causes need four different responses and three of them are not the
/// network — telling someone their link is bad when their laptop cannot decode
/// sends them to reset a router for nothing.
///
/// Text for now; [`Bottleneck::id`] is the stable key an icon can hang off
/// later without touching this logic.
fn render_bottlenecks(ui: &mut egui::Ui, active: tain_core::BottleneckSet) {
    if active.is_empty() {
        return;
    }
    ui.add_space(10.0);
    for bottleneck in active.iter() {
        // Unfixable ones are dimmer: they are information, not a call to act.
        let colour = if bottleneck.is_actionable() { AMBER } else { TEXT_DIM };
        ui.label(RichText::new(format!("⚠ {}", bottleneck.label())).size(12.0).color(colour))
            .on_hover_text(bottleneck.advice());
        ui.add_space(6.0);
    }
}

/// Say why the picture has stopped, when the reason is not a fault.
///
/// # Why this is drawn over the frame rather than instead of it
///
/// A window that is minimised mid-session leaves its last frame on screen, and
/// that frame is worth keeping: it is what was there a moment ago, it says
/// which window this is, and it comes straight back when the window does.
/// Replacing it with a blank panel would throw away context to make room for a
/// sentence. So the frame stays, dimmed, and the sentence sits on top of it.
///
/// Nothing is drawn at all for an ordinary screen session — a screen cannot be
/// minimised, and this is the only thing that separates "no picture because
/// somebody put the window away" from "no picture because something is wrong".
fn render_shared_window_notice(
    ui: &mut egui::Ui,
    session: &GuestSession,
    viewport: egui::Rect,
) {
    use tain_core::protocol::SharedWindowState;

    // # A view that has been asked for and has not arrived
    //
    // Checked before everything below, because it is the more recent fact: a
    // guest switching away from a minimised window should be told its new view
    // is coming, not told again about the one it just left.
    if session.switching() {
        render_switching_notice(ui, viewport);
        return;
    }

    // # Nothing left at all, which is not any one window's story
    //
    // Checked first and without a `shared_window`, because there is no longer a
    // window to be in a state: a composite watches none of them individually,
    // and every one of them has gone. Without this the guest sits looking at
    // the last frame it received with nothing saying why it stopped — which is
    // the same failure the per-window notice exists to prevent, one level up.
    let (key, detail, named) = if let Some(away) = session.windows_unavailable() {
        match away {
            super::session::WindowsAway::Closed => (
                "session.windows_all_closed",
                "session.windows_all_closed_wait",
                String::new(),
            ),
            // Still granted, still listed, none of them on screen. Not the same
            // sentence as closed: these are one gesture away from coming back,
            // and the host is the one who can make it.
            super::session::WindowsAway::Minimised => (
                "session.windows_all_away",
                "session.windows_all_away_wait",
                String::new(),
            ),
        }
    } else {
        let Some(window) = session.shared_window() else { return };
        // Whether there is anywhere else to go. A window that has closed is the
        // end of a one-window session and nothing can be done about it from
        // this side — but a session granted several has the others, and the
        // sentence should say so rather than leave somebody staring at a dead
        // panel.
        let elsewhere = session.shared_windows().len() > 1;
        let (key, detail) = match window.state {
            SharedWindowState::Showing => return,
            SharedWindowState::Minimised if elsewhere => {
                ("session.window_minimised", "session.window_waiting_others")
            }
            SharedWindowState::Minimised => {
                ("session.window_minimised", "session.window_waiting")
            }
            SharedWindowState::Closed if elsewhere => {
                ("session.window_closed", "session.window_gone_others")
            }
            SharedWindowState::Closed => ("session.window_closed", "session.window_gone"),
        };

        // Named by the program where it has one. A window that has closed may
        // have answered neither, and "the shared window" is still a true
        // sentence.
        let named = if window.app.is_empty() {
            i18n::text("session.window_unnamed").to_string()
        } else {
            window.app.clone()
        };
        (key, detail, named)
    };

    let painter = ui.painter_at(viewport);
    // Dimmed rather than hidden: enough to say the picture is not live, not so
    // much that what it was showing is lost.
    painter.rect_filled(viewport, 0.0, egui::Color32::from_black_alpha(150));

    let heading = i18n::format(key, &[("app", &named)]);
    let waiting = i18n::text(detail);

    // Painted rather than laid out with widgets: it is two centred lines over a
    // picture, and a widget layout here would claim input the video area needs.
    let centre = viewport.center();
    let heading_id = painter.text(
        centre - egui::vec2(0.0, 12.0),
        egui::Align2::CENTER_CENTER,
        &heading,
        egui::FontId::proportional(17.0),
        TEXT,
    );
    painter.text(
        egui::pos2(centre.x, heading_id.max.y + 10.0),
        egui::Align2::CENTER_TOP,
        waiting,
        egui::FontId::proportional(13.0),
        TEXT_DIM,
    );
}

/// "The picture you asked for is on its way", over the one you have.
///
/// # Why the old picture stays underneath
///
/// It is dimmed rather than replaced, for the same reason a minimised window's
/// notice leaves its last frame up: what was there a moment ago is context, and
/// a black panel with a sentence on it throws that away to say something the
/// sentence already says. Dimming is what separates "this is live" from "this
/// is what was live".
///
/// The ring is what separates *this* from the notices around it. Those describe
/// a state that will last until somebody at the host does something; this one
/// is work in progress, and a moving thing is the only honest way to say so.
fn render_switching_notice(ui: &mut egui::Ui, viewport: egui::Rect) {
    let painter = ui.painter_at(viewport);
    painter.rect_filled(viewport, 0.0, egui::Color32::from_black_alpha(150));

    let centre = viewport.center();
    // The ring draws around the rectangle it is given rather than inside it, so
    // this is the space at its middle, not its extent.
    let ring = egui::Rect::from_center_size(
        centre - egui::vec2(0.0, 22.0),
        egui::Vec2::splat(18.0),
    );
    theme::spinner_ring(ui, ring, TEXT);

    painter.text(
        centre + egui::vec2(0.0, 14.0),
        egui::Align2::CENTER_CENTER,
        i18n::text("session.switching"),
        egui::FontId::proportional(15.0),
        TEXT,
    );
}

fn render_waiting(ui: &mut egui::Ui, session: &GuestSession) {
    // A session waiting on a minimised window is not waiting on the connection,
    // and a spinner over "waiting for the first frame" would say it was. The
    // notice drawn on top of this says the true thing.
    if !session.source_is_live() {
        return;
    }
    ui.vertical_centered(|ui| {
        ui.add_space(80.0);
        if session.is_active() {
            ui.add(egui::Spinner::new().size(28.0));
            ui.add_space(10.0);
        }
        ui.label(RichText::new(session.status().label()).size(13.0).color(TEXT_DIM));

        // The line above says what is happening; this says how much of it is
        // left and what each step found, which is the rest of what someone
        // watching a spinner wants to know. See `super::progress`.
        //
        // Drawn for a failed session too, frozen on the step that failed — that
        // is the moment it is worth the most, and it used to be the moment it
        // disappeared.
        if let Some(progress) = session.progress() {
            ui.add_space(22.0);
            render_progress_graph(ui, progress, &session.outcomes());
        }
    });
}

/// The stages of connecting, as a row that fills up.
///
/// Drawn rather than built from widgets because it is one picture: a line of
/// nodes with the track between them filled in behind the current one. Widgets
/// would give six separate items that happen to sit next to each other, and
/// getting the connecting track to line up between them is more work than
/// painting the whole thing.
fn render_progress_graph(
    ui: &mut egui::Ui,
    progress: super::progress::Progress,
    outcomes: &super::progress::Outcomes,
) {
    use super::progress::Stage;

    const NODE: f32 = 7.0;
    const LABEL_DROP: f32 = 16.0;
    /// Where the result of a stage sits, under its name.
    const OUTCOME_DROP: f32 = 30.0;
    /// Room for the widest label plus a little air. Below this the labels
    /// collide, so the row shrinks no further and takes the space it needs.
    const MIN_GAP: f32 = 52.0;
    const IDEAL_GAP: f32 = 74.0;

    let count = Stage::ALL.len();
    let spans = (count - 1) as f32;
    // Fits a narrow session window rather than running off the edge of it, and
    // stops shrinking once the labels would touch.
    let room = (ui.available_width() - NODE * 2.0 - 8.0) / spans;
    let gap = room.clamp(MIN_GAP, IDEAL_GAP);

    let width = gap * spans + NODE * 2.0;
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, NODE * 2.0 + OUTCOME_DROP + 12.0),
        egui::Sense::hover(),
    );
    let painter = ui.painter();

    let y = rect.top() + NODE;
    let centre = |index: usize| egui::pos2(rect.left() + NODE + gap * index as f32, y);

    for (index, stage) in Stage::ALL.iter().enumerate() {
        // Behind the current stage is done; the current one is lit, or marked
        // in red if this is where it stopped; ahead is still only an outline.
        let done = index < progress.at || progress.done;
        let here = index == progress.at && !progress.done;
        let stopped = here && progress.failed;
        let accent = if progress.failed { OFFLINE_RED } else { LIVE_GREEN };

        if index + 1 < count {
            // The track to the next node, filled only as far as we have got.
            let filled = index < progress.at || progress.done;
            painter.line_segment(
                [egui::pos2(centre(index).x + NODE, y), egui::pos2(centre(index + 1).x - NODE, y)],
                egui::Stroke::new(2.0, if filled { LIVE_GREEN } else { SEPARATOR }),
            );
        }

        let (fill, stroke) = if stopped {
            // Solid, because a failure is a state the connection is *in*, not
            // one it is working through.
            (OFFLINE_RED, OFFLINE_RED)
        } else if done {
            (LIVE_GREEN, LIVE_GREEN)
        } else if here {
            (WINDOW_BG, accent)
        } else {
            (WINDOW_BG, SEPARATOR)
        };
        painter.circle(centre(index), NODE, fill, egui::Stroke::new(2.0, stroke));

        painter.text(
            egui::pos2(centre(index).x, y + NODE + LABEL_DROP),
            egui::Align2::CENTER_CENTER,
            stage.label(),
            egui::FontId::proportional(11.0),
            if done || here { TEXT } else { TEXT_DIM },
        );

        // What the stage produced, under its name — only once it is behind us,
        // which is what lets `Find` report how the host was reached rather than
        // having to tell "nothing looked yet" from "nothing needed to".
        //
        // A stage that stopped says what it was left holding instead, which for
        // the common failure — arriving on a relay and never getting off it —
        // is the whole diagnosis, and the thing the graph was silent about.
        let caption = if stopped {
            stage.stopped_on(outcomes).map(|reason| (reason, OFFLINE_RED))
        } else if done {
            stage.outcome(outcomes).map(|result| (result, LIVE_GREEN))
        } else {
            None
        };
        if let Some((text, colour)) = caption {
            painter.text(
                egui::pos2(centre(index).x, y + NODE + OUTCOME_DROP),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(10.0),
                colour,
            );
        }
    }

    // The same path in words, for anyone who wants to know what a stage means.
    response.on_hover_text(
        Stage::ALL
            .iter()
            .map(|stage| format!("{:<8} {}", stage.label(), stage.describe()))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

/// Translate egui pointer state into protocol input events.
///
/// Only sends on *change* — egui reports state every frame, and streaming a
/// redundant event at the repaint rate would swamp the input channel.
///
/// # Every movement, not one a frame
///
/// This used to read `hover_pos()`, which is where the pointer finished the
/// frame. Everything it passed through on the way was discarded, so a hand
/// moving smoothly arrived as one jump per repaint — and on a link drawing at
/// eight frames a second, that is one jump every 120ms.
///
/// For putting a cursor somewhere that is harmless: the position it lands on is
/// the same either way. It is not harmless for anything reading *deltas*, which
/// is what dragging a slider or a numeric field does. Pointer acceleration is
/// superlinear, so one large delta travels considerably further than the same
/// distance delivered in small ones — reported as the cursor moving far too
/// fast while adjusting a value, and only while adjusting one.
///
/// So each movement egui saw is forwarded. That costs more messages, which used
/// to mean queueing behind a keyframe; since video moved to its own stream it
/// costs almost nothing.
fn forward_pointer(
    session: &mut GuestSession,
    ui: &egui::Ui,
    resp: &egui::Response,
    rect: egui::Rect,
) {
    // Don't bother sending what the host would only drop — but do let go of
    // any contacts first, so the slot table does not carry fingers that will
    // never be lifted into the next gesture.
    if !session.grants().mouse {
        release_touches(session);
        return;
    }

    // # Touch instead of the pointer, not as well as it
    //
    // egui synthesises pointer events from a single contact so that ordinary
    // widgets work under a finger. Forwarding both would send every tap twice:
    // once as a touch and once as a click.
    //
    // Touch is the one to keep. It is what actually happened, it carries the
    // other fingers, and Windows promotes it to a click by itself for any
    // window that does not handle touch — so nothing is lost by sending it,
    // and pinch and two-finger gestures are gained.
    //
    // `any_touches` alone is not the question. egui drops a contact while
    // processing the very event that ends it, so on the frame a finger lifts it
    // already reports none — and taking the mouse path there would cancel the
    // gesture instead of finishing it, turning every tap into an abort. The
    // events still sitting in the queue are the other half of the answer.
    let touching = ui.input(|i| {
        i.any_touches() || i.events.iter().any(|e| matches!(e, egui::Event::Touch { .. }))
    });
    if touching {
        forward_touch(session, ui, rect);
        return;
    }
    release_touches(session);

    // # A captured pointer is driven by movement, not by position
    //
    // Instead of the block below and not as well as it: the two are
    // alternatives, and sending both would have the host's own re-centring
    // fight the positions. See `GuestSession::wants_relative_pointer`.
    //
    // The buttons and the wheel after it are *not* an alternative — a game
    // being looked around in is still being shot in — so this replaces the
    // movement and falls through to the rest.
    let relative = session.wants_relative_pointer();
    if relative {
        forward_relative_pointer(session, ui, rect);
    }

    if !relative && resp.hovered() {
        // Collected before sending: `ui.input` holds a lock, and sending takes
        // the session mutably.
        let moved: Vec<egui::Pos2> = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::PointerMoved(pos) => Some(*pos),
                    _ => None,
                })
                .collect()
        });

        // Wire coordinates are normalized to the whole screen, so scaling mode
        // and window size drop out of the calculation entirely.
        let to_event = |pos: egui::Pos2| {
            MouseEvent::from_fraction(
                (pos.x - rect.left()) / rect.width(),
                (pos.y - rect.top()) / rect.height(),
            )
        };

        for pos in moved {
            // A movement that ended outside the video is not one to send: the
            // host would put its cursor at the edge for a pointer that is over
            // the toolbar.
            if !rect.contains(pos) {
                continue;
            }
            let event = to_event(pos);
            if session.last_pointer() != Some((event.x, event.y)) {
                session.set_last_pointer((event.x, event.y));
                session.send_input(InputEvent::MouseMove(event));
            }
        }

        // Where it finished, if that is somewhere the events did not already
        // leave it. Covers the frame where the window itself moved under a
        // still pointer, which produces no movement event and a new position.
        if let Some(pos) = resp.hover_pos() {
            let event = to_event(pos);
            if session.last_pointer() != Some((event.x, event.y)) {
                session.set_last_pointer((event.x, event.y));
                session.send_input(InputEvent::MouseMove(event));
            }
        }
    }

    let buttons = [
        egui::PointerButton::Primary,
        egui::PointerButton::Secondary,
        egui::PointerButton::Middle,
    ];
    for (index, button) in buttons.iter().enumerate() {
        // Gating on `hovered` also releases the button if the pointer leaves the
        // view while held, rather than stranding it down on the host.
        let down = resp.hovered() && ui.input(|i| i.pointer.button_down(*button));
        if down != session.button_state(index) {
            session.set_button_state(index, down);
            session.send_input(InputEvent::MouseButton(MouseButtonEvent {
                button: index as u8,
                pressed: down,
            }));
        }
    }

    if resp.hovered() {
        let delta = ui.input(|i| i.raw_scroll_delta);
        if delta != egui::Vec2::ZERO {
            session.send_input(InputEvent::Scroll(scroll_to_wheel(delta)));
        }
    }
}

/// Taking the pointer, and how far it goes once taken.
///
/// Shown as a popup under the pointer button rather than as permanent
/// controls: they govern one mode, most sessions never enter that mode, and a
/// toolbar is a scarce thing. Live, and per session — a value that suited the
/// last game is not evidence about this one.
fn render_sensitivity_popup(
    ui: &mut egui::Ui,
    session: &mut GuestSession,
    anchor: &egui::Response,
    id: egui::Id,
) {
    egui::popup_below_widget(
        ui,
        id,
        anchor,
        egui::PopupCloseBehavior::CloseOnClickOutside,
        |ui| {
            ui.set_min_width(190.0);

            // # The way in that does not wait to be noticed
            //
            // Capture engages on its own when the host is seen to hide its
            // cursor, which is the right default and covers most games — but
            // only where the host is capturing in a way that *has* a pointer
            // to report. See `GuestSession::pointer_captured`. This is the
            // switch for every other case, and it sits above the sensitivity
            // because it is the thing the sensitivity applies to.
            let mut captured = session.pointer_captured;
            if ui
                .checkbox(&mut captured, i18n::text("session.capture_pointer"))
                .on_hover_text(i18n::text("session.capture_pointer_hint"))
                .changed()
            {
                session.pointer_captured = captured;
            }
            ui.add_space(4.0);

            ui.label(
                RichText::new(i18n::text("session.pointer_sensitivity")).size(12.0).color(TEXT),
            );
            let range = super::settings::POINTER_SENSITIVITY_RANGE;
            ui.add(
                egui::Slider::new(&mut session.pointer_sensitivity, range)
                    .logarithmic(true)
                    .fixed_decimals(2),
            );
            ui.label(
                RichText::new(i18n::text("session.pointer_sensitivity_hint"))
                    .size(11.0)
                    .color(TEXT_DIM),
            );
            // A way back that does not need the number remembering. The value
            // this restores is the one in Settings, not a constant, so somebody
            // who set a default gets their default.
            if ui.button(i18n::text("session.pointer_sensitivity_reset")).clicked() {
                session.pointer_sensitivity = session.default_pointer_sensitivity();
            }
        },
    );
}

/// Send the pointer as movement, for a host that has captured its own.
///
/// # Why the cursor is put back in the middle every frame
///
/// Movement is only movement while there is somewhere to move *from*. A pointer
/// left where the user pushed it stops producing anything the moment it reaches
/// the edge of the window — which, in a game, is the moment you try to keep
/// turning. Warping it back to the middle after reading each delta is what
/// makes the space endless, and it is what every application that captures a
/// pointer does.
///
/// # Why the position is read from the system and not from egui
///
/// The warp is itself a movement, and egui would report it as one — so the
/// delta it hands over would include the correction, doubling every input and
/// cancelling it on alternate frames. Reading the position, subtracting the
/// centre and putting it back is one exchange with no bookkeeping to get wrong.
fn forward_relative_pointer(session: &mut GuestSession, ui: &egui::Ui, rect: egui::Rect) {
    // Only while this window is the one being typed into. A session left open
    // behind something else must not be dragging the pointer of the machine it
    // is running on.
    if !ui.ctx().input(|i| i.focused) {
        return;
    }

    // Hidden, because there are two pointers and only one of them is real: the
    // host is drawing whatever the game draws, and a local arrow sitting in the
    // middle of it is the ghost `draw_remote_cursor` already refuses to be.
    ui.ctx().set_cursor_icon(egui::CursorIcon::None);

    // The middle of the picture, in the screen's own coordinates — which is
    // where the pointer is held.
    let centre = rect.center();
    let scale = ui.ctx().pixels_per_point();
    let outer = ui.ctx().input(|i| i.viewport().inner_rect);
    let Some(outer) = outer else { return };
    let hold = (
        ((outer.min.x + centre.x) * scale).round() as i32,
        ((outer.min.y + centre.y) * scale).round() as i32,
    );

    // # Movement is sent as it was made, times what the user asked for
    //
    // It was scaled by the ratio between the picture and the host's screen, on
    // the reasoning that a delta should feel the same as moving across that
    // screen. That is right for a *position* and wrong here: a program that
    // captured the pointer is reading movement, not a place on a screen, and
    // what it does with the number is its own sensitivity setting. Multiplying
    // by a window-size ratio put a second, invisible sensitivity in front of
    // it — which is how a small window turned ordinary movements into extreme
    // rotations.
    //
    // So one is one, and anything else is a slider the user can see. See
    // `Settings::pointer_sensitivity`.
    let sensitivity = session.pointer_sensitivity();

    if let Some((dx, dy)) = pointer_delta_from(hold) {
        let dx = (dx as f32 * sensitivity).round() as i32;
        let dy = (dy as f32 * sensitivity).round() as i32;
        if (dx, dy) != (0, 0) {
            session.send_input(InputEvent::MouseMotion(tain_core::protocol::MouseMotion {
                dx: dx.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                dy: dy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            }));
        }
    }

    // Nothing else wakes the interface while the pointer is the only thing
    // moving, and a frame not drawn is a delta not read.
    ui.ctx().request_repaint();
}

/// How far the pointer has moved from `hold`, putting it back there.
///
/// `None` where this cannot be done — every platform but Windows, and a failed
/// call — which leaves the pointer alone rather than half-driving it.
#[cfg(target_os = "windows")]
fn pointer_delta_from(hold: (i32, i32)) -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

    let mut at = POINT::default();
    // SAFETY: both take a pointer to a `POINT` this frame owns, and neither
    // allocates or retains anything.
    unsafe {
        GetCursorPos(&mut at).ok()?;
        let delta = (at.x - hold.0, at.y - hold.1);
        if delta != (0, 0) {
            SetCursorPos(hold.0, hold.1).ok()?;
        }
        Some(delta)
    }
}

#[cfg(not(target_os = "windows"))]
fn pointer_delta_from(_hold: (i32, i32)) -> Option<(i32, i32)> {
    None
}

/// Forward every contact egui saw this frame.
///
/// # Why each finger travels on its own
///
/// The host has to keep the set of live contacts regardless, because the
/// Windows call that injects them wants all of them on every update. Given
/// that, one finger per message is the smaller protocol, it fits the fixed
/// record the agent pipe carries to the secure desktop, and it passes the same
/// per-event permission check as a click. See `protocol::TouchEvent`.
fn forward_touch(session: &mut GuestSession, ui: &egui::Ui, rect: egui::Rect) {
    // Collected before sending: `ui.input` holds a lock and sending takes the
    // session mutably.
    let touches: Vec<(u64, egui::TouchPhase, egui::Pos2, Option<f32>)> = ui.input(|i| {
        i.events
            .iter()
            .filter_map(|event| match event {
                egui::Event::Touch { id, phase, pos, force, .. } => {
                    Some((id.0, *phase, *pos, *force))
                }
                _ => None,
            })
            .collect()
    });

    for (id, phase, pos, force) in touches {
        let (x, y) = normalized_in(pos, rect);
        match phase {
            egui::TouchPhase::Start => {
                // A touch that began off the video belongs to the toolbar or
                // the window edge, and claims no slot.
                if !rect.contains(pos) {
                    continue;
                }
                let Some(slot) = session.touch_slot(id) else { continue };
                session.send_input(InputEvent::Touch(TouchEvent {
                    id: slot,
                    phase: TouchPhase::Start,
                    x,
                    y,
                    pressure: to_pressure(force),
                }));
            }
            // Not gated on the rect: a finger that drifts past the edge of the
            // video is still part of the gesture that started inside it, and
            // dropping it would strand the contact on the host.
            egui::TouchPhase::Move => {
                let Some(slot) = session.touch_slot_if_live(id) else { continue };
                session.send_input(InputEvent::Touch(TouchEvent {
                    id: slot,
                    phase: TouchPhase::Move,
                    x,
                    y,
                    pressure: to_pressure(force),
                }));
            }
            egui::TouchPhase::End | egui::TouchPhase::Cancel => {
                let Some(slot) = session.release_touch_slot(id) else { continue };
                session.send_input(InputEvent::Touch(TouchEvent {
                    id: slot,
                    phase: if matches!(phase, egui::TouchPhase::End) {
                        TouchPhase::End
                    } else {
                        TouchPhase::Cancel
                    },
                    x,
                    y,
                    pressure: 0,
                }));
            }
        }
    }
}

/// Lift every contact the host still believes is down.
///
/// Cheap when there are none, which is almost always: it is a scan of ten
/// `Option`s that usually finds nothing and sends nothing.
fn release_touches(session: &mut GuestSession) {
    for slot in session.take_touch_slots() {
        session.send_input(InputEvent::Touch(TouchEvent {
            id: slot,
            phase: TouchPhase::Cancel,
            // Where it was is not known here and does not matter: the host
            // lifts the contact from wherever it last put it.
            x: 0,
            y: 0,
            pressure: 0,
        }));
    }
}

/// A point in the video, as the fraction of the screen the wire carries.
fn normalized_in(pos: egui::Pos2, rect: egui::Rect) -> (u16, u16) {
    let event = MouseEvent::from_fraction(
        (pos.x - rect.left()) / rect.width(),
        (pos.y - rect.top()) / rect.height(),
    );
    (event.x, event.y)
}

/// egui's `0.0..=1.0` force as the wire's `1..=255`.
///
/// `0` means the digitizer did not report any, which most do not.
fn to_pressure(force: Option<f32>) -> u8 {
    match force {
        Some(force) if force > 0.0 => (force.clamp(0.0, 1.0) * 255.0).round().max(1.0) as u8,
        _ => 0,
    }
}

/// Translate egui key events into protocol key events.
fn forward_keyboard(
    session: &mut GuestSession,
    ui: &egui::Ui,
    immersive: &mut Immersive,
    reserved: &[egui::Key],
) {
    if !session.grants().keyboard {
        return;
    }

    // # One source at a time
    //
    // Armed, the hook has swallowed everything, so egui's view of the keyboard
    // is empty and stale — its modifier flags in particular describe a key
    // press it never saw. Reading both would send each key twice on the way in
    // and never release it on the way out.
    //
    // The hook is the better source anyway: real virtual-key codes, left and
    // right modifiers apart, and the keys egui has no name for at all. It is
    // only unavailable because the window is not focused, and then nothing
    // should be going to the host regardless.
    if immersive.is_armed() {
        for event in immersive.drain_captured_keys() {
            session.note_held_key(event.keycode, event.pressed);
            session.send_input(InputEvent::Key(event));
        }
        return;
    }

    // # Modifiers and Tab come from the hook when it is available
    //
    // egui reports Ctrl, Alt and Shift only as a state flag, never as key
    // events, and it consumes Tab for its own focus navigation — so on the egui
    // path alone a session could not send a bare modifier or a Tab at all.
    // `ModifierTracker` reconstructs the modifiers by diffing that flag, which
    // works but cannot tell left from right and cannot see Tab.
    //
    // When the hook is forwarding, those keys arrive from it instead, as real
    // virtual-key codes. They are then skipped here, so nothing is sent twice.
    // # Which source supplies the modifiers
    //
    // Immersive mode swallows everything, so egui is blind and the hook is the
    // only source there is.
    //
    // Outside it, egui's modifier *state* is reliable and the hook is not — the
    // hook can be uninstalled by Windows without warning, which is the whole
    // history of this file. Taking modifiers from the hook there replaced a
    // working path with a broken one, and modified clicks stopped arriving at
    // all. So the tracker keeps that job, and the hook contributes only Tab,
    // which egui consumes for focus navigation and cannot pass on.
    // Anything held when the focus left has no key-up coming, so let it go on
    // the host before sending anything else.
    if immersive.keys_went_elsewhere() {
        session.release_held_keys();
        for event in session.modifiers_mut().release_all() {
            session.send_input(InputEvent::Key(event));
        }
    }

    let immersed = immersive.is_armed();
    let (modifiers, events) = ui.input(|i| (i.modifiers, i.events.clone()));
    if immersed {
        for event in immersive.drain_captured_keys() {
            session.note_held_key(event.keycode, event.pressed);
            session.send_input(InputEvent::Key(event));
        }
    } else {
        // Modifiers first: the host needs the modifier down before the key —
        // or the click — that it modifies.
        //
        // Read from the OS rather than from egui. egui's modifier flag is not
        // dependable inside this immediate viewport, and it cannot tell left
        // from right in any case.
        let vks = super::immersive::MODIFIER_VKS;
        let down = std::array::from_fn(|i| super::immersive::key_is_down(vks[i]));
        for event in session.modifiers_mut().diff_actual(down, vks) {
            session.send_input(InputEvent::Key(event));
        }
        for event in immersive.drain_captured_keys() {
            // Tab only. Everything else here would duplicate what the tracker
            // and the egui event loop already sent.
            if event.keycode != 0x09 {
                continue;
            }
            session.note_held_key(event.keycode, event.pressed);
            session.send_input(InputEvent::Key(event));
        }
    }

    for event in events {
        let egui::Event::Key { key, pressed, repeat, .. } = event else { continue };
        // Auto-repeat is regenerated by the host's own OS from the held key,
        // and a key claimed by a shortcut must never reach the host.
        if repeat || reserved.contains(&key) {
            continue;
        }
        // Already sent, with a better code than egui could give.
        if key == egui::Key::Tab {
            continue;
        }
        let Some(keycode) = keymap::vk_for(key) else { continue };
        session.send_input(InputEvent::Key(KeyEvent {
            keycode,
            pressed,
            modifiers: keymap::modifier_bits(modifiers),
            // egui gives no scancode. Keys that reach here are the ones the
            // hook is not forwarding, so a virtual key is all there is.
            scancode: 0,
            extended: false,
        }));
    }
}

#[cfg(test)]
mod tests {
    /// The two axes disagree, and only one of them was wrong.
    ///
    /// winit documents a positive delta as "the content should move right and
    /// down". Down means the view goes up, which is what Windows calls a
    /// positive wheel — so vertical passes straight through. Right means the
    /// view goes *left*, where Windows calls rightward positive, so horizontal
    /// has to flip. Getting this wrong is invisible on a mouse with no tilt
    /// wheel, which is why it shipped.
    #[test]
    fn horizontal_scroll_is_flipped_and_vertical_is_not() {
        // Scrolling up: egui positive y, and the wire agrees.
        let up = scroll_to_wheel(egui::vec2(0.0, POINTS_PER_DETENT));
        assert_eq!(up.dy, WHEEL_DELTA, "one notch up should be one wheel delta");
        assert_eq!(up.dx, 0);

        // Scrolling down keeps its sign too.
        let down = scroll_to_wheel(egui::vec2(0.0, -POINTS_PER_DETENT));
        assert_eq!(down.dy, -WHEEL_DELTA);

        // egui's positive x reveals content to the left, which the wire calls
        // leftward — negative.
        let left = scroll_to_wheel(egui::vec2(POINTS_PER_DETENT, 0.0));
        assert_eq!(left.dx, -WHEEL_DELTA, "egui positive x is a leftward scroll");
        assert_eq!(left.dy, 0);

        let right = scroll_to_wheel(egui::vec2(-POINTS_PER_DETENT, 0.0));
        assert_eq!(right.dx, WHEEL_DELTA);
    }

    /// A diagonal trackpad flick carries both axes at once.
    #[test]
    fn both_axes_travel_together() {
        let both = scroll_to_wheel(egui::vec2(POINTS_PER_DETENT, POINTS_PER_DETENT));
        assert_eq!(both.dx, -WHEEL_DELTA);
        assert_eq!(both.dy, WHEEL_DELTA);
    }

    use super::*;

    /// The three states must be distinguishable, and only two are clickable.
    ///
    /// # The bug this is here for
    ///
    /// "Off" and "not permitted" were both drawn with the `*_disabled` art,
    /// which carries `opacity="0.3"`, and then tinted grey on top. The result
    /// was invisible against the toolbar, and it conflated the user's own
    /// choice with the host's refusal.
    #[test]
    fn a_refused_grant_and_a_local_refusal_are_different_states() {
        use super::theme::IconState;

        // Host refused: not the user's to choose.
        assert_eq!(input_state(false, false, true), IconState::Unavailable);
        assert_eq!(input_state(false, false, false), IconState::Unavailable);

        // A loopback session forwards nothing, whatever the host said.
        assert_eq!(input_state(true, true, true), IconState::Unavailable);

        // Permitted: now the local toggle decides, and both states are live.
        assert_eq!(input_state(false, true, true), IconState::On);
        assert_eq!(input_state(false, true, false), IconState::Off);
    }

    /// Clicking the toolbar must not also click the remote screen.
    ///
    /// # The bug this is here for
    ///
    /// The toolbar is drawn on top of the video, and the video's hit rectangle
    /// covered the whole viewport including the toolbar. Pressing Disconnect,
    /// or any toggle, also delivered that press to whatever happened to be
    /// underneath it on the host.
    #[test]
    fn the_toolbar_area_is_excluded_from_what_is_sent_to_the_host() {
        let mut window = SessionWindow::default();
        let bar = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 38.0));
        window.toolbar_rect = Some(bar);

        // A press on the bar belongs to the bar.
        assert!(bar.contains(egui::pos2(400.0, 20.0)));
        // One below it belongs to the remote screen.
        assert!(!bar.contains(egui::pos2(400.0, 60.0)));

        // Collapsed, nothing is withheld — the whole picture is live again.
        window.toolbar_rect = None;
        assert!(window.toolbar_rect.is_none());
    }

    #[test]
    fn the_stats_row_is_off_until_asked_for() {
        // Row one is toggles, which are always wanted; the numbers matter when
        // something is wrong. A window that opens two rows tall by default
        // spends the space the toggles were moved there to get.
        let window = SessionWindow::default();
        assert!(!window.show_stats);
        assert!(window.show_toolbar);
    }

    #[test]
    fn resetting_clears_fullscreen_so_the_next_session_opens_windowed() {
        // Regression: disconnecting while fullscreen used to leave the window
        // stuck with nothing to click.
        let mut window = SessionWindow {
            fullscreen: true,
            active_tab: 3,
            show_toolbar: false,
            ..SessionWindow::default()
        };

        window.reset();

        assert!(!window.fullscreen);
        assert_eq!(window.active_tab, 0);
        assert!(window.show_toolbar);
    }

    #[test]
    fn toggling_fullscreen_queues_the_opposite_state() {
        let mut window = SessionWindow::default();
        assert!(!window.fullscreen);

        window.toggle_fullscreen();
        assert_eq!(window.pending_fullscreen, Some(true));

        // Applying happens inside the viewport callback.
        window.fullscreen = true;
        window.pending_fullscreen = None;
        window.toggle_fullscreen();
        assert_eq!(window.pending_fullscreen, Some(false));
    }

    #[test]
    fn an_empty_session_list_produces_no_actions() {
        let mut window = SessionWindow::default();
        let actions = SessionWindowActions::default();
        assert!(actions.close.is_empty());
        assert!(!actions.close_all);
        window.reset();
    }

    fn screen(id: u32, x: i32, y: i32, w: u16, h: u16) -> tain_core::protocol::DisplayInfo {
        tain_core::protocol::DisplayInfo {
            id,
            name: format!(r"\.\DISPLAY{}", id + 1),
            monitor: None,
            width: w,
            height: h,
            x,
            y,
            primary: x == 0 && y == 0,
            hdr: false,
            virtual_display: false,
        }
    }

    /// A screen is named by where it is, not by what the driver calls it.
    ///
    /// `\.\DISPLAY2` identifies nothing to the person choosing: it is an index
    /// into a list they cannot see, and it is not even stable when a monitor is
    /// plugged in. Where a screen sits is what someone at the host recognises.
    #[test]
    fn screens_are_named_by_where_they_are() {
        let all = vec![
            screen(0, 0, 0, 2560, 1600),
            screen(1, -1920, 0, 1920, 1080),
            screen(2, 2560, 0, 1920, 1080),
        ];

        assert_eq!(describe_display(&all[0], &all), "Main 2560×1600");
        assert_eq!(describe_display(&all[1], &all), "Left 1920×1080");
        assert_eq!(describe_display(&all[2], &all), "Right 1920×1080");
    }

    /// Stacked screens are above and below, not left and right.
    #[test]
    fn a_screen_stacked_over_the_main_one_says_so() {
        let all = vec![screen(0, 0, 0, 2560, 1600), screen(1, 0, -1080, 1920, 1080)];
        assert_eq!(describe_display(&all[1], &all), "Above 1920×1080");

        let all = vec![screen(0, 0, 0, 2560, 1600), screen(1, 0, 1600, 1920, 1080)];
        assert_eq!(describe_display(&all[1], &all), "Below 1920×1080");
    }

    /// An arrangement that is none of those still names every screen distinctly.
    ///
    /// Monitors can overlap in Windows' layout, and two screens that both came
    /// back "Screen" would make the menu useless in exactly the case where it
    /// is hardest to guess.
    #[test]
    fn an_odd_arrangement_still_tells_the_screens_apart() {
        // Diagonally offset: neither wholly beside nor wholly above.
        let all = vec![
            screen(0, 0, 0, 2560, 1600),
            screen(1, 1000, 800, 1920, 1080),
            screen(2, 1200, 900, 1920, 1080),
        ];
        let named: Vec<String> = all.iter().map(|d| describe_display(d, &all)).collect();
        let unique: std::collections::HashSet<&String> = named.iter().collect();
        assert_eq!(unique.len(), named.len(), "two screens share a name: {named:?}");
    }

    /// The main screen is the main screen whatever else is around it.
    #[test]
    fn the_primary_is_always_called_main() {
        let alone = vec![screen(0, 0, 0, 1920, 1080)];
        assert!(describe_display(&alone[0], &alone).starts_with("Main"));
    }

    /// A virtual screen is named for what it is, not where it is.
    ///
    /// Nobody at the host is looking at it and it occupies no place on a desk,
    /// so "Left" or "Right" would describe a position that means nothing to
    /// either end — and a virtual display's coordinates are wherever Windows
    /// happened to put it.
    #[test]
    fn a_virtual_screen_is_named_for_being_virtual() {
        let mut virt = screen(1, 2560, 0, 1920, 1080);
        virt.virtual_display = true;
        let all = vec![screen(0, 0, 0, 2560, 1600), virt.clone()];

        assert_eq!(describe_display(&virt, &all), "Virtual 1920×1080");
        // The real one is still described by where it is.
        assert_eq!(describe_display(&all[0], &all), "Main 2560×1600");
    }

    /// Being virtual wins over being the primary.
    ///
    /// A virtual display can be made primary — that is the point of it for game
    /// streaming — and calling it "Main" would hide the thing worth knowing.
    #[test]
    fn a_virtual_primary_still_says_virtual() {
        let mut virt = screen(0, 0, 0, 1920, 1080);
        virt.virtual_display = true;
        assert!(virt.primary);
        let all = vec![virt.clone()];
        assert!(describe_display(&virt, &all).starts_with("Virtual"));
    }
}
