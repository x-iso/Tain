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

use egui::{Context, RichText, ViewportId};

use tain_core::protocol::{
    InputEvent, KeyEvent, MouseButtonEvent, MouseEvent, ScrollEvent, WHEEL_DELTA,
};

use super::immersive::Immersive;
use super::keymap;
use super::session::GuestSession;
use super::settings::{ScalingMode, Settings};
use super::theme::{self, *};

/// egui reports scroll in points; the wire uses Windows wheel-delta units.
/// One notch is roughly 50 points on a typical mouse.
const POINTS_PER_DETENT: f32 = 50.0;

/// Leading blank in a tab label, holding the space the status dot is painted in.
const DOT_GAP: &str = "   ";

/// What the session window is asking the app to do.
#[derive(Default)]
pub struct SessionWindowActions {
    /// Tab ids to disconnect and close.
    pub close: Vec<u64>,
    /// The window itself was closed; every session should end.
    pub close_all: bool,
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
    active_tab_id: Option<u64>,
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
    }

    if let Some(action) = ctx.input(|i| bindings.triggered(i)) {
        match action {
            ShortcutAction::Disconnect => {
                if let Some(id) = active_tab_id {
                    actions.close.push(id);
                }
            }
            ShortcutAction::ToggleFullscreen => window.toggle_fullscreen(),
            ShortcutAction::ToggleToolbar => window.show_toolbar = !window.show_toolbar,
            ShortcutAction::ToggleImmersive => immersive.set_active(!immersive.is_active()),
        }
        if let Some(key) = bindings.get(action).egui_key() {
            consumed.push(key);
        }
    }

    // Escape always leaves fullscreen, regardless of bindings — it is the
    // universal escape hatch. It deliberately never disconnects: a reflexive
    // press should not destroy a session.
    if window.fullscreen && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
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

        let title = match sessions.len() {
            1 => "Tain — Session".to_string(),
            n => format!("Tain — {n} Sessions"),
        };

        parent.show_viewport_immediate(
            ViewportId::from_hash_of("tain-sessions"),
            egui::ViewportBuilder::default()
                .with_title(title)
                .with_inner_size([1280.0, 760.0])
                .with_min_inner_size([480.0, 320.0]),
            |ctx, _class| {
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
                }

                let active_tab_id = sessions.get(self.active_tab).map(|s| s.tab_id);
                let reserved =
                    handle_shortcuts(ctx, self, settings, immersive, active_tab_id, &mut actions);

                self.render_contents(ctx, sessions, immersive, &reserved, &mut actions);

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
    ) {
        // The tab strip is hidden in fullscreen with a single session — the
        // point of fullscreen is to see the remote screen and nothing else.
        let show_tabs = sessions.len() > 1 || !self.fullscreen;
        if show_tabs {
            egui::TopBottomPanel::top("session-tabs")
                .exact_height(32.0)
                .frame(
                    egui::Frame::none()
                        .fill(HEADER_BG)
                        .inner_margin(egui::Margin::symmetric(8.0, 4.0)),
                )
                .show(ctx, |ui| self.render_tabs(ui, sessions, actions));
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

                        let rect = theme::scaled_rect(session.scaling, viewport, src, session.pan);
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
                        if session.sends_keyboard() {
                            forward_keyboard(session, ui, immersive, reserved);
                        }
                        if session.sends_mouse() && !over_toolbar {
                            forward_pointer(session, ui, &resp, rect);
                        }

                        // Panning is driven by the pointer, not by frames, so a
                        // still screen would otherwise scroll one step and stop.
                        if panning {
                            ctx.request_repaint();
                        }
                    }
                    None => render_waiting(ui, session),
                }

                self.render_toolbar(ui, viewport, sessions, immersive, actions);
            });
    }

    fn render_tabs(
        &mut self,
        ui: &mut egui::Ui,
        sessions: &[GuestSession],
        actions: &mut SessionWindowActions,
    ) {
        ui.horizontal(|ui| {
            for (index, session) in sessions.iter().enumerate() {
                let selected = index == self.active_tab;
                let dot = match session.status() {
                    s if s.is_active() => LIVE_GREEN,
                    _ => OFFLINE_RED,
                };

                // Spaces, not a bullet character. The dot the user sees is
                // painted below — it has to be, because it carries the colour
                // that says live or offline. The text only reserves room for
                // it, and a `●` doing that job rendered as an empty box in
                // egui's bundled fonts, sitting behind the real dot.
                let response = ui.selectable_label(
                    selected,
                    RichText::new(format!("{DOT_GAP}{}", session.label)).color(if selected {
                        TEXT
                    } else {
                        TEXT_DIM
                    }),
                );
                ui.painter().circle_filled(
                    response.rect.left_center() + egui::vec2(9.0, 0.0),
                    3.5,
                    dot,
                );
                if response.clicked() {
                    self.active_tab = index;
                }

                if ui
                    .small_button("×")
                    .on_hover_text("Close this session")
                    .clicked()
                {
                    actions.close.push(session.tab_id);
                }
                ui.add_space(6.0);
            }
        });
    }

    fn render_toolbar(
        &mut self,
        ui: &mut egui::Ui,
        viewport: egui::Rect,
        sessions: &mut [GuestSession],
        immersive: &mut Immersive,
        actions: &mut SessionWindowActions,
    ) {
        // How close to the top edge counts as reaching for the toolbar.
        const REVEAL_BAND: f32 = 90.0;

        let near_top = ui
            .ctx()
            .pointer_latest_pos()
            .is_some_and(|p| p.y < viewport.top() + REVEAL_BAND);

        // Collapsed, or auto-hidden in fullscreen: leave a small handle that
        // appears when the pointer comes near, so the bar is discoverable
        // rather than only reachable by remembering the shortcut.
        if !self.show_toolbar || (self.fullscreen && !near_top) {
            self.toolbar_rect = None;
            if near_top {
                self.render_reveal_handle(ui, viewport);
            }
            return;
        }

        let Some(session) = sessions.get_mut(self.active_tab) else { return };
        // The toolbar is where the input toggles live, so it is also where they
        // first learn what the host permitted.
        session.seed_input_toggles();
        let tab_id = session.tab_id;

        const ROW: f32 = 38.0;
        let rows = if self.show_stats { 2.0 } else { 1.0 };
        let bar = egui::Rect::from_min_size(
            viewport.left_top(),
            egui::vec2(viewport.width(), ROW * rows),
        );
        self.toolbar_rect = Some(bar);
        paint_toolbar_backdrop(ui.painter(), bar);

        // Drawn after the backdrop and before the controls, so the row's own
        // layout does not have to leave a gap for something centred.
        self.render_collapse_handle(ui, bar);

        let first_row = egui::Rect::from_min_size(bar.left_top(), egui::vec2(bar.width(), ROW));
        let mut content = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(first_row.shrink2(egui::vec2(12.0, 6.0)))
                .layout(egui::Layout::left_to_right(egui::Align::Center)),
        );

        let (w, h) = session.remote_size();
        content.label(RichText::new(format!("{w}×{h}")).size(12.0).color(TEXT));

        content.add_space(12.0);
        render_input_toggles(&mut content, session);

        content.add_space(12.0);
        // The stats toggle sits with the other toggles rather than at the far
        // end: it controls this bar, not the session.
        let stats_on = self.show_stats;
        if theme::icon_toggle(
            &mut content,
            16.0,
            if stats_on { theme::IconState::On } else { theme::IconState::Off },
            egui::include_image!("../../assets/fps-metrics.svg"),
            if stats_on { "Hide connection stats" } else { "Show connection stats" },
        )
        .clicked()
        {
            self.show_stats = !stats_on;
        }

        let mut toggle_fullscreen = false;
        content.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Icons rather than words, to match the toggles on the left half of
            // the bar. Disconnect keeps a distinct glyph because it is the one
            // control here that ends something.
            if theme::icon_toggle(
                ui,
                16.0,
                theme::IconState::Off,
                egui::include_image!("../../assets/cancel.svg"),
                "Disconnect from this host",
            )
            .clicked()
            {
                actions.close.push(tab_id);
            }

            ui.add_space(6.0);
            // Two drawings rather than one toggled state: the icon shows what
            // the click will *do*, which for a mode switch is clearer than
            // showing which mode is current.
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
                if full { "Leave fullscreen" } else { "Go fullscreen" },
            )
            .clicked()
            {
                toggle_fullscreen = true;
            }

            ui.add_space(6.0);
            let on = immersive.is_active();
            if theme::icon_toggle(
                ui,
                16.0,
                if on { theme::IconState::On } else { theme::IconState::Off },
                egui::include_image!("../../assets/immersive_mode.svg"),
                if on {
                    "Immersive — every key goes to the host and the pointer stays here"
                } else {
                    "Immersive mode: send every key, Win and Alt included, and keep the pointer in this window"
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

            ui.add_space(10.0);
            // Reversed: in a right-to-left layout the first item added lands
            // rightmost, so iterating forwards would render 1:1 before Fit.
            for mode in ScalingMode::ALL.into_iter().rev() {
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

            if session.loopback {
                ui.add_space(10.0);
                ui.label(RichText::new("⚠ loopback — input disabled").size(12.0).color(AMBER))
                    .on_hover_text(
                        "This session points at our own host endpoint. Forwarding input \
                         would drive this machine's own pointer in a feedback loop.",
                    );
            }
        });

        if self.show_stats {
            let second = egui::Rect::from_min_size(
                bar.left_top() + egui::vec2(0.0, ROW),
                egui::vec2(bar.width(), ROW),
            );
            let mut stats = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(second.shrink2(egui::vec2(12.0, 6.0)))
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            render_stats_row(&mut stats, session);
        }

        if toggle_fullscreen {
            self.toggle_fullscreen();
        }
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
        resp.on_hover_text("Hide the toolbar — move the pointer to the top edge to bring it back");
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
        resp.on_hover_text("Show the toolbar");
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
fn render_input_toggles(ui: &mut egui::Ui, session: &mut GuestSession) {
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
            _ if session.loopback => "Pointer disabled — this session is a loopback",
            theme::IconState::Unavailable => "The host has not allowed pointer control",
            theme::IconState::On => "Pointer goes to the host — click to keep it local",
            theme::IconState::Off => "Pointer stays on this machine — click to send it",
        },
    );
    if resp.clicked() {
        session.want_mouse = !session.want_mouse;
    }

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
            _ if session.loopback => "Keyboard disabled — this session is a loopback",
            theme::IconState::Unavailable => "The host has not allowed keyboard control",
            theme::IconState::On => "Typing goes to the host — click to keep it local",
            theme::IconState::Off => "Typing stays on this machine — click to send it",
        },
    );
    if resp.clicked() {
        session.want_keyboard = !session.want_keyboard;
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
            "Showing the host's pointer — click to hide it"
        } else {
            "Host's pointer hidden — click to show it"
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
/// The toolbar sits over the video, and every control on it is a permanent hole
/// in the picture. A slider is much wider than an icon, and volume is set once
/// and then left — so it earns its space only while someone is reaching for it.
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
            "Playing the host's sound — click to mute"
        } else {
            "Host sound muted — click to play it"
        },
    );
    if icon.clicked() {
        session.want_sound = !on;
        session.apply_sound_preference();
    }

    let mut hot = icon.rect;
    if icon.hovered() || near {
        ui.add_space(4.0);
        ui.spacing_mut().slider_width = 64.0;
        let slider = ui.add(
            egui::Slider::new(&mut session.volume, 0.0..=1.0)
                .show_value(false)
                .custom_formatter(|_, _| String::new()),
        );
        if slider.changed() {
            // Moving the slider up is an unambiguous request to hear it, so it
            // undoes the mute rather than leaving the user dragging a control
            // that does nothing.
            if session.volume > 0.0 {
                session.want_sound = true;
            }
            session.apply_sound_preference();
        }
        hot = hot.union(slider.rect);
        slider.on_hover_text(format!("Host volume {}%", (session.volume * 100.0).round()));
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
            "{}\n\n\
             lat — capture on the host through to display here, the whole \
             pipeline.\n\
             net — round trip on the link alone.\n\n\
             A large gap between them means the delay is in encoding or \
             decoding rather than the network.",
            session.health_detail()
        ));

    // A relayed path gets its own warning rather than a line in the stats,
    // because no quality setting can compensate for it.
    match session.relay_warning() {
        Some(advice) => {
            ui.add_space(10.0);
            ui.label(RichText::new("⚠ relayed").size(12.0).color(AMBER))
                .on_hover_text(advice);
        }
        None => {
            if session.is_direct() == Some(true) {
                ui.add_space(10.0);
                ui.label(RichText::new("direct").size(12.0).color(TEXT_DIM))
                    .on_hover_text("Connected peer to peer.");
            }
        }
    }

    if let Some(ms) = session.audio_buffered_ms() {
        ui.add_space(10.0);
        // The target as well as the depth. The buffer grows itself when the link
        // keeps breaking it, so a session with unexpected audio delay should be
        // able to say whether that is a stall or a cushion it had to build.
        let text = match session.audio_target_ms() {
            Some(target) => format!("audio {ms}/{target} ms"),
            None => format!("audio {ms} ms"),
        };
        ui.label(RichText::new(text).size(12.0).color(TEXT_DIM)).on_hover_text(
            "Audio buffered ahead of the speakers, and the cushion it is aiming for. \
             The target grows when the link drops audio and shrinks again when it stops.",
        );
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

fn render_waiting(ui: &mut egui::Ui, session: &GuestSession) {
    ui.vertical_centered(|ui| {
        ui.add_space(80.0);
        if session.is_active() {
            ui.add(egui::Spinner::new().size(28.0));
            ui.add_space(10.0);
        }
        ui.label(RichText::new(session.status().label()).size(13.0).color(TEXT_DIM));
    });
}

/// Translate egui pointer state into protocol input events.
///
/// Only sends on *change* — egui reports state every frame, and streaming a
/// redundant event at the repaint rate would swamp the input channel.
fn forward_pointer(
    session: &mut GuestSession,
    ui: &egui::Ui,
    resp: &egui::Response,
    rect: egui::Rect,
) {
    // Don't bother sending what the host would only drop.
    if !session.grants().mouse {
        return;
    }

    if let Some(pos) = resp.hover_pos() {
        // Wire coordinates are normalized to the whole screen, so scaling mode
        // and window size drop out of the calculation entirely.
        let event = MouseEvent::from_fraction(
            (pos.x - rect.left()) / rect.width(),
            (pos.y - rect.top()) / rect.height(),
        );
        if session.last_pointer() != Some((event.x, event.y)) {
            session.set_last_pointer((event.x, event.y));
            session.send_input(InputEvent::MouseMove(event));
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
            let to_wheel = |points: f32| (points / POINTS_PER_DETENT * WHEEL_DELTA as f32) as i16;
            session.send_input(InputEvent::Scroll(ScrollEvent {
                dx: to_wheel(delta.x),
                dy: to_wheel(delta.y),
            }));
        }
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
}
