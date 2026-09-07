//! Palette, metrics, and small drawing helpers shared by the GUI.
//!
//! Colours are lifted from the SVGs in `assets/` so the hand-drawn chrome and
//! the imported icons agree (e.g. the live dot here is the same `#34C759` as
//! `in_connect_live.svg`).

use egui::{Color32, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2};

// ── Palette ──────────────────────────────────────────────────

pub const WINDOW_BG: Color32 = Color32::from_rgb(0x1C, 0x1C, 0x1E);
pub const HEADER_BG: Color32 = Color32::from_rgb(0x2C, 0x2C, 0x2E);
pub const CARD_BG: Color32 = Color32::from_rgb(0x2C, 0x2C, 0x2E);
/// The same card with the pointer over it.
///
/// Deliberately a small step. The card is a control — clicking the person on it
/// admits or removes a guest — and a control that does not react at all reads
/// as a label, which is what an incoming card looked like while somebody was
/// waiting to be let in.
pub const CARD_BG_HOVER: Color32 = Color32::from_rgb(0x3A, 0x3A, 0x3D);
pub const CARD_INNER_BG: Color32 = Color32::from_rgb(0x33, 0x33, 0x36);
pub const CARD_STROKE: Color32 = Color32::from_rgb(0x8E, 0x8E, 0x93);
pub const TEXT: Color32 = Color32::from_rgb(0xF2, 0xF2, 0xF7);
pub const TEXT_DIM: Color32 = Color32::from_rgb(0xA9, 0xA9, 0xA9);
pub const SEPARATOR: Color32 = Color32::from_rgb(0x48, 0x48, 0x4A);

/// Admin passcode button — the one action that hands over full control.
pub const ADMIN_FILL: Color32 = Color32::from_rgb(0x7B, 0x3A, 0x1F);
pub const ADMIN_STROKE: Color32 = Color32::from_rgb(0xE0, 0x7B, 0x39);
pub const GUEST_FILL: Color32 = Color32::from_rgb(0x2C, 0x2C, 0x2E);
pub const GUEST_STROKE: Color32 = Color32::from_rgb(0xD1, 0xD1, 0xD6);

/// A choice that is currently made — the screens being shared, and nothing
/// else so far.
///
/// Blue rather than the green used for "this is working": one is a decision the
/// user is making right now and can still change, the other is a report about
/// the world. A tick that looks like a status light invites reading a proposal
/// as a fact.
pub const SELECT_BLUE: Color32 = Color32::from_rgb(0x0A, 0x84, 0xFF);

pub const LIVE_GREEN: Color32 = Color32::from_rgb(0x34, 0xC7, 0x59);
pub const OFFLINE_RED: Color32 = Color32::from_rgb(0xCB, 0x2F, 0x33);
/// Something is wrong but recoverable — a relayed path, an expired code, a
/// shortcut that will swallow typing. Distinct from [`OFFLINE_RED`], which is
/// for things that are simply not working.
pub const AMBER: Color32 = Color32::from_rgb(0xE0, 0x9B, 0x2E);

// ── Metrics ──────────────────────────────────────────────────

/// Header is a fixed band. It must never grow into the middle section.
///
/// Two 30pt passcode rows plus their gap and the panel's vertical margins.
pub const HEADER_HEIGHT: f32 = 86.0;
/// Height of the vertical rules dividing the header's sections.
pub const HEADER_RULE_H: f32 = 62.0;
pub const BUTTON_HEIGHT: f32 = 30.0;
pub const BUTTON_MIN_W: f32 = 132.0;
pub const BUTTON_MAX_W: f32 = 250.0;

/// Width of everything in the header that is *not* a passcode button:
/// the two section labels, reset icons, autoreset checkboxes, both vertical
/// rules, and the settings section.
///
/// The header must fit whatever width it is given — at 175% display scaling a
/// 1200pt window is only ~690 logical points wide, and fixed button widths
/// pushed the Connect block off the right edge.
pub const HEADER_CHROME_W: f32 = 390.0;

/// The connect arrow's box.
///
/// Smaller than it was: at 34 it overhung the paste button it sits beside and,
/// on a narrow window, ran into the settings gear — the two blocks are laid out
/// independently and nothing reserved the gap between them.
pub const CONNECT_ICON: f32 = 26.0;

// The connect arrow sits inside a button-height row. Checked here rather than
// in a test: it is two constants, so a mismatch should fail the build rather
// than wait for someone to run the suite.
const _: () = assert!(
    CONNECT_ICON <= BUTTON_HEIGHT,
    "the connect icon would overhang the row it sits in"
);

/// Width the settings section needs on the right, including its rule and gaps.
///
/// Subtracted before the passcode buttons are sized. The settings block is
/// added *last* and drawn right-to-left, so without reserving it here the
/// Connect block is measured against space that is not really free and grows
/// into it.
pub const SETTINGS_BLOCK_W: f32 = 60.0;
pub const CARD_SIZE: Vec2 = Vec2::new(300.0, 208.0);

/// Smallest the main window may be dragged to, in points.
///
/// # Why this is a constant rather than a literal at the call site
///
/// It is set twice: once when the window is built, and again whenever the
/// display scale changes. The second is not optional — winit converts this to
/// physical pixels when it is given, and never reconverts it. A window given a
/// minimum on a 175% display and then dragged to a 125% one keeps the *pixel*
/// figure, so its minimum silently grows by 40% in the units that matter.
///
/// Measured on the machine that reported it: 814x637 points on the first
/// display, 1134x878 on the second — a minimum taller than the 1080-pixel
/// screen it was now on, which is why the window could not be dragged smaller
/// there and could only be maximised.
///
/// Two places with the same number written out would drift, and the drift would
/// silently change the minimum rather than fail.
pub const MIN_WINDOW_SIZE: [f32; 2] = [800.0, 600.0];

/// An incoming-connection card is sized by what is in it, so only its height
/// and its smallest useful width are constants.
///
/// There used to be a third: the width of everything that was *not* the name,
/// counted out by hand so the card could be sized in advance. It was wrong
/// twice — the count left out the spacing egui puts between widgets, and a long
/// name ran under the permission icons. A frame that measures its own contents
/// cannot get that wrong, and does not need to be told.
pub const PILL_HEIGHT: f32 = 44.0;

/// How wide an incoming connection card is. All of them, always.
///
/// # Why one width rather than one per name
///
/// It used to be a minimum, with each card growing to fit its guest's name.
/// Two things ended that.
///
/// The approval popup is drawn directly above its card and takes exactly that
/// card's width — which is what keeps two popups from overlapping, since the
/// cards themselves already lay out without colliding. So a card has to be wide
/// enough for the question it may carry: two checkboxes, a row of screen icons,
/// and two answers side by side.
///
/// And the marks on the card — mouse, keyboard, status — sit on its right edge.
/// Right-aligning inside a card that is only as wide as its contents is the
/// same as not aligning at all, and a strip of cards whose marks are at
/// different distances from the left is a strip that has to be read one card at
/// a time.
///
/// The cost is empty space beside a short name, which is what the design asks
/// for: a row of identical cards, each with its name at one end and its state
/// at the other.
pub const PILL_W: f32 = 268.0;
pub const CORNER: f32 = 5.0;

/// Width to give each passcode button so the header fits `available`.
///
/// Both the Host and Connect blocks hold one, so the leftover splits two ways.
pub fn passcode_button_width(available: f32) -> f32 {
    ((available - HEADER_CHROME_W) * 0.5).clamp(BUTTON_MIN_W, BUTTON_MAX_W)
}


// ── Drawing helpers ──────────────────────────────────────────

/// Move a rectangle onto whole physical pixels.
///
/// The one cause of soft icons that code can actually fix. A texture rendered
/// at exactly the right resolution is still resampled if its rectangle lands
/// between physical pixels, which a layout in points does routinely once the
/// display scaling is fractional.
///
/// Both edges are snapped rather than the origin and the size, so the result
/// cannot come out a fraction of a pixel wider than it started.
pub fn snap(ui: &Ui, rect: egui::Rect) -> egui::Rect {
    let p = ui.painter();
    egui::Rect::from_min_max(
        egui::pos2(p.round_to_pixel(rect.min.x), p.round_to_pixel(rect.min.y)),
        egui::pos2(p.round_to_pixel(rect.max.x), p.round_to_pixel(rect.max.y)),
    )
}

/// Draw an SVG at an explicitly stated pixel size.
///
/// # What this does and does not fix
///
/// It was written believing egui rasterised SVGs at their intrinsic size and
/// resampled. **That is not what happens.** `Image` defaults to
/// `ImageFit::Fraction(1.0)`, and `paint_at` passes the destination rectangle,
/// so egui already asks the loader for `rect.size() * pixels_per_point` — the
/// true pixel count. The size passed here computes the same number.
///
/// What it is still worth: the size becomes visible at the call site instead of
/// depending on which `Image` method is used, and texture options are set in one
/// place rather than defaulted six times.
///
/// The blur these icons actually suffer from has two other causes, and only one
/// of them is fixable here:
///
/// - **Sub-pixel placement**, which [`snap`] handles. A texture rendered at
///   exactly the right resolution is still resampled if its rectangle lands
///   between physical pixels, which a layout in points does routinely at
///   fractional display scaling.
/// - **A 24px drawing rendered at 16px.** The assets declare `viewBox="0 0 24
///   24"` and are drawn at 16 points. resvg renders that correctly, but strokes
///   authored for 24 pixels come out around two thirds of a pixel wide and no
///   renderer can make that crisp. The remedies are to draw the icons larger or
///   to author versions whose strokes land on whole pixels at 16 — neither of
///   which is a code change.
pub fn sharp_image(src: egui::ImageSource<'_>, size: Vec2) -> egui::Image<'_> {
    egui::Image::new(src)
        .fit_to_exact_size(size)
        // Rendered at native resolution, so filtering has nothing to correct.
        // Linear stays for the case where a fractional scale factor leaves a
        // half-pixel offset — nearest would alias the edges there.
        .texture_options(egui::TextureOptions::LINEAR)
}


/// A rotating arc, for work that is in flight and has no measurable progress.
///
/// Used while a pasted code is being checked against its host. That check
/// already existed and already worked; nothing on screen said it was happening,
/// so a code that took a second to verify looked like a button that did nothing.
///
/// Returns nothing and paints nothing else — the caller draws whatever sits
/// inside the ring.
pub fn spinner_ring(ui: &Ui, rect: egui::Rect, colour: Color32) {
    // Time-driven rather than frame-driven, so the speed is the same on a
    // 60 Hz panel and a 165 Hz one.
    let t = ui.input(|i| i.time) as f32;
    let centre = rect.center();
    let radius = rect.width().max(rect.height()) * 0.5 + 3.0;

    // The track it runs on, so the ring reads as a ring rather than a comet.
    ui.painter().circle_stroke(
        centre,
        radius,
        egui::Stroke::new(1.5, colour.gamma_multiply(0.25)),
    );

    // A 90 degree arc, two turns a second.
    const SWEEP: f32 = std::f32::consts::FRAC_PI_2;
    const SEGMENTS: usize = 12;
    let start = t * std::f32::consts::TAU * 2.0 % std::f32::consts::TAU;
    let points: Vec<egui::Pos2> = (0..=SEGMENTS)
        .map(|i| {
            let angle = start + SWEEP * (i as f32 / SEGMENTS as f32);
            centre + egui::vec2(angle.cos(), angle.sin()) * radius
        })
        .collect();
    ui.painter().add(egui::Shape::line(points, egui::Stroke::new(2.0, colour)));

    // Nothing else would wake the UI to advance it.
    ui.ctx().request_repaint();
}


/// Draw a non-interactive icon.
pub fn icon(ui: &mut Ui, size: f32, src: egui::ImageSource<'_>) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    if ui.is_rect_visible(rect) {
        let rect = snap(ui, rect);
        sharp_image(src, rect.size()).paint_at(ui, rect);
    }
}

/// A clickable icon carrying one of three states.
///
/// # Why three and not two
///
/// "Off" and "not available" look the same to a naive toggle and mean opposite
/// things: one is the user's choice and the other is the host's refusal. They
/// need different art and different behaviour — a refusal is not clickable, and
/// showing it as merely off invites clicking at something that will never
/// respond.
///
/// # Why nothing is tinted grey
///
/// The `*_disabled` assets already carry `opacity="0.3"`. Tinting those again —
/// which the first version of this did, with `Color32::from_gray(110)` —
/// multiplies 0.3 by 0.43 and produces an icon that is, in the user's words,
/// basically invisible. Dimming here is alpha only, via `from_white_alpha`, so
/// it never darkens art that is already faint, and it is applied only to art
/// that is drawn at full strength in its other state.
pub fn icon_toggle(
    ui: &mut Ui,
    size: f32,
    state: IconState,
    src: egui::ImageSource<'_>,
    hover: &str,
) -> Response {
    let padding = Vec2::splat(5.0);
    let sense = match state {
        // Nothing to click: the host decides this one.
        IconState::Unavailable => Sense::hover(),
        _ => Sense::click(),
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size) + padding * 2.0, sense);

    if ui.is_rect_visible(rect) {
        match state {
            IconState::On => {
                ui.painter().rect_filled(rect, 4.0, Color32::from_white_alpha(30));
            }
            IconState::Off if resp.hovered() => {
                ui.painter().rect_filled(rect, 4.0, Color32::from_white_alpha(16));
            }
            _ => {}
        }
        let inner = snap(ui, rect.shrink2(padding));
        let img = sharp_image(src, inner.size());
        match state {
            // Full strength, lit chip behind it.
            IconState::On => img.paint_at(ui, inner),
            // Legible but plainly not lit. Hovering brings it up to full so the
            // control confirms it is live before you commit to clicking.
            IconState::Off if resp.hovered() => img.paint_at(ui, inner),
            IconState::Off => img.tint(Color32::from_white_alpha(150)).paint_at(ui, inner),
            // Already faint by design; touching it would erase it.
            IconState::Unavailable => img.paint_at(ui, inner),
        }
    }
    resp.on_hover_text(hover)
}

/// An [`icon_toggle`] with a number drawn inside it.
///
/// # Why the number is painted rather than drawn into the art
///
/// A screen's number is not known until the host says how many it has, so there
/// is no fixed set of icons to draw — and rewriting the SVG at runtime to
/// substitute a digit would mean re-rasterising a texture per display per
/// theme change, to place a glyph that the UI font can already draw crisply at
/// any size.
///
/// So the monitor is art and the digit is text, centred on the screen area of
/// [`monitor.svg`] rather than on the icon: the shape has a stand below it, so
/// the middle of the picture is not the middle of the screen. `SCREEN_CENTRE`
/// is that offset as a fraction of the icon, taken from the artwork's own
/// viewBox.
pub fn numbered_icon_toggle(
    ui: &mut Ui,
    size: f32,
    state: IconState,
    src: egui::ImageSource<'_>,
    number: u32,
    hover: &str,
) -> Response {
    /// Where the middle of the screen sits within the monitor art, as a
    /// fraction of its 24x23 viewBox. The panel spans y 3..17, so its centre is
    /// well above the icon's own.
    const SCREEN_CENTRE: (f32, f32) = (12.0 / 24.0, 10.0 / 23.0);

    let resp = icon_toggle(ui, size, state, src, hover);
    if ui.is_rect_visible(resp.rect) {
        let at = resp.rect.min
            + egui::vec2(
                resp.rect.width() * SCREEN_CENTRE.0,
                resp.rect.height() * SCREEN_CENTRE.1,
            );
        // Matching the art's own weight: the strokes are white at full
        // strength when lit and dimmed when not, and a digit that ignored that
        // would sit oddly bright on an unlit icon.
        let colour = match state {
            IconState::On => Color32::WHITE,
            _ => Color32::from_white_alpha(150),
        };
        ui.painter().text(
            at,
            egui::Align2::CENTER_CENTER,
            number.to_string(),
            egui::FontId::proportional((size * 0.5).max(8.0)),
            colour,
        );
    }
    resp
}

/// What an [`icon_toggle`] is currently saying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    /// On, and the user can turn it off.
    On,
    /// Off by the user's choice, and they can turn it back on.
    Off,
    /// Not the user's to choose — drawn, not clickable.
    Unavailable,
}

/// Draw a clickable icon that brightens on hover.
pub fn icon_button(ui: &mut Ui, size: f32, src: egui::ImageSource<'_>, hover: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    if ui.is_rect_visible(rect) {
        let rect = snap(ui, rect);
        let img = sharp_image(src, rect.size());
        // Only dim when idle — tinting a coloured icon at full strength would
        // wash out its own palette.
        if resp.hovered() {
            img.paint_at(ui, rect);
        } else {
            img.tint(Color32::from_gray(215)).paint_at(ui, rect);
        }
    }
    resp.on_hover_text(hover)
}

/// A wide labelled button with a trailing icon, as used for the passcode actions.
pub fn labelled_button(
    ui: &mut Ui,
    size: Vec2,
    label: &str,
    src: egui::ImageSource<'_>,
    fill: Color32,
    stroke: Color32,
    enabled: bool,
) -> Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);

    if ui.is_rect_visible(rect) {
        let fill = if enabled && resp.hovered() { fill.gamma_multiply(1.35) } else { fill };
        let (fill, stroke, text) = if enabled {
            (fill, stroke, TEXT)
        } else {
            (fill.gamma_multiply(0.6), stroke.gamma_multiply(0.5), TEXT_DIM)
        };

        let painter = ui.painter();
        painter.rect(rect, CORNER, fill, Stroke::new(1.0, stroke));
        painter.text(
            rect.left_center() + Vec2::new(12.0, 0.0),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(13.0),
            text,
        );

        let icon_rect = Rect::from_center_size(
            rect.right_center() - Vec2::new(18.0, 0.0),
            Vec2::splat(18.0),
        );
        let icon_rect = snap(ui, icon_rect);
        sharp_image(src, icon_rect.size()).paint_at(ui, icon_rect);
    }

    resp
}

/// One screen, as a button that shows whether it is being shared.
///
/// # Why icons and not a column of checkboxes
///
/// The list they replace named every display in full — "2. DELL U2720Q —
/// 3840×2160 (main)" — one per row, with a separate "All screens" tick above.
/// That is a paragraph to read before a decision that is really about *shape*:
/// how many screens there are, and which of them somebody is about to see. A
/// row of monitors answers that at a glance and fits a dialog that appears
/// while a person is waiting.
///
/// The detail is not lost, it is moved: the caller hangs it on the hover text,
/// where it is available to anyone who wants it and in the way of nobody who
/// does not.
///
/// `number` is `None` for the "all screens" button, which stands for whatever
/// is plugged in rather than for any particular screen.
pub fn display_button(
    ui: &mut Ui,
    size: f32,
    src: egui::ImageSource<'_>,
    number: Option<usize>,
    caption: Option<&str>,
    selected: bool,
) -> Response {
    // # Every cell is the same size, captioned or not
    //
    // The caption belongs to one of them — "All" — and giving that button a
    // taller cell than the rest made the row lay out around it: a horizontal
    // layout centres each item against the tallest, so the numbered screens
    // hung below the one with a label under it.
    //
    // Reserving the caption's height on every cell costs a few points of empty
    // space under the screens that have no caption, and means a row of them is
    // a row, and several rows are a grid.
    let button = Vec2::splat(size + 12.0);
    let caption_height = ui.text_style_height(&egui::TextStyle::Small) + 2.0;
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(button.x, button.y + caption_height), Sense::click());
    let face = Rect::from_min_size(rect.min, button);

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if selected {
            painter.rect(face, CORNER, SELECT_BLUE, Stroke::NONE);
        } else if resp.hovered() {
            painter.rect(face, CORNER, CARD_BG_HOVER, Stroke::NONE);
        }

        let icon_rect = snap(ui, Rect::from_center_size(face.center(), Vec2::splat(size)));
        sharp_image(src, icon_rect.size()).paint_at(ui, icon_rect);

        // Inside the screen part of the glyph, which is its upper portion —
        // centred on the whole icon would sit the digit over the stand.
        if let Some(number) = number {
            painter.text(
                icon_rect.center() - Vec2::new(0.0, icon_rect.height() * 0.08),
                egui::Align2::CENTER_CENTER,
                number.to_string(),
                egui::FontId::proportional(size * 0.42),
                TEXT,
            );
        }

        if let Some(caption) = caption {
            painter.text(
                egui::pos2(face.left(), face.bottom() + 1.0),
                egui::Align2::LEFT_TOP,
                caption,
                egui::TextStyle::Small.resolve(ui.style()),
                if selected { TEXT } else { TEXT_DIM },
            );
        }
    }

    resp
}

/// What a [`slide_to_reveal`] cover did this frame.
pub struct SlideOutcome {
    /// Where the knob is now, 0 at rest and 1 fully across. Feed it back next
    /// frame — the widget itself keeps nothing.
    pub progress: f32,
    /// The knob reached the far end. What was covered should be uncovered.
    pub completed: bool,
    /// Somebody touched it. Not the same as [`Self::completed`]: this is the
    /// first contact, and it is what a one-time explanation hangs on.
    pub touched: bool,
}

/// A cover that has to be dragged aside, over a button that should not be
/// pressed by accident.
///
/// # Why a slider and not a confirmation dialog
///
/// The thing being guarded is the admin passcode's *copy* button, and what it
/// guards against is the copy happening at all — a code that reaches the
/// clipboard can leave in the next paste, into whatever window happens to have
/// focus, and nothing afterwards can call it back. A dialog does not help with
/// that: it appears after the click, and the answer to a dialog you did not
/// mean to summon is to dismiss it, which people do without reading.
///
/// A drag cannot happen by accident. It takes a deliberate gesture in a
/// particular direction, it cannot be produced by a stray click or a keyboard
/// repeat, and it leaves the button covered until somebody means it.
///
/// # Nothing is remembered here
///
/// `progress` comes in and goes out, so the caller owns the state. That is what
/// lets the cover close on a timer, on a copy, and on a passcode reset without
/// this widget knowing any of those things exist.
pub fn slide_to_reveal(ui: &mut Ui, size: Vec2, label: &str, progress: f32) -> SlideOutcome {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::drag());

    let knob_width = (size.y - 6.0).max(18.0);
    // The knob's centre travels between these, so the distance it can cover is
    // short of the full width by its own size.
    let travel = (rect.width() - knob_width - 6.0).max(1.0);

    let mut progress = progress.clamp(0.0, 1.0);
    let touched = resp.drag_started() || resp.dragged();
    if let Some(pointer) = resp.interact_pointer_pos() {
        let from_left = pointer.x - rect.left() - 3.0 - knob_width / 2.0;
        progress = (from_left / travel).clamp(0.0, 1.0);
    }
    // Released short of the end: back to the start rather than left part way,
    // so the next attempt is the same gesture as the first and there is no
    // half-open state to reason about.
    let completed = progress >= 0.995;
    if resp.drag_stopped() && !completed {
        progress = 0.0;
    }

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect(rect, CORNER, CARD_INNER_BG, Stroke::new(1.0, SEPARATOR));

        // The label fades out as the knob crosses it: the instruction stops
        // being useful exactly as it stops being needed.
        let text_alpha = (1.0 - progress * 1.6).clamp(0.0, 1.0);
        painter.text(
            rect.center() + Vec2::new(knob_width / 2.0, 0.0),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(12.0),
            TEXT_DIM.gamma_multiply(text_alpha),
        );

        let knob_x = rect.left() + 3.0 + knob_width / 2.0 + travel * progress;
        let knob = Rect::from_center_size(
            egui::pos2(knob_x, rect.center().y),
            Vec2::new(knob_width, size.y - 6.0),
        );
        let lit = resp.hovered() || resp.dragged();
        painter.rect(
            knob,
            CORNER,
            if lit { ADMIN_FILL.gamma_multiply(1.35) } else { ADMIN_FILL },
            Stroke::new(1.0, ADMIN_STROKE),
        );
        // Two chevrons, pointing the way the knob has to go. Drawn rather than
        // an asset: it is four lines of geometry and the alternative is a file.
        for step in 0..2 {
            let x = knob.center().x - 3.0 + step as f32 * 5.0;
            let arm = 3.5;
            let colour = TEXT.gamma_multiply(1.0 - progress);
            painter.line_segment(
                [egui::pos2(x, knob.center().y - arm), egui::pos2(x + arm, knob.center().y)],
                Stroke::new(1.5, colour),
            );
            painter.line_segment(
                [egui::pos2(x + arm, knob.center().y), egui::pos2(x, knob.center().y + arm)],
                Stroke::new(1.5, colour),
            );
        }
    }

    SlideOutcome { progress, completed, touched }
}

/// How far the pan offset can travel on each axis before the far edge of the
/// remote screen is flush with the far edge of the window.
///
/// Zero on an axis where the remote screen already fits — there is nothing
/// hidden to pan to, so the image stays centred.
pub fn pan_limit(avail: Vec2, src: Vec2) -> Vec2 {
    Vec2::new((src.x - avail.x).max(0.0), (src.y - avail.y).max(0.0))
}

/// Place a remote frame of size `src` inside `avail` according to `mode`.
///
/// `Fit` is always centred and ignores `pan`. `Actual` may produce a rect larger
/// than `avail` — the caller clips, which is the correct behaviour for a 1:1
/// view of a bigger screen — and `pan` slides which part of it is visible, in
/// source pixels from the top-left. Values outside the valid range are clamped
/// here rather than trusted, so a stale offset from a previous frame size can
/// never scroll the image off into nothing.
pub fn scaled_rect(
    mode: super::settings::ScalingMode,
    avail: Rect,
    src: Vec2,
    pan: Vec2,
    pixels_per_point: f32,
) -> Rect {
    use super::settings::ScalingMode;

    if src.x <= 0.0 || src.y <= 0.0 {
        return Rect::from_center_size(avail.center(), Vec2::ZERO);
    }

    // # One host pixel per guest pixel, which is not one per point
    //
    // `src` is the host's framebuffer in *pixels*; `avail` is egui's rectangle
    // in *points*. Drawing `src` points wide asks for `src * pixels_per_point`
    // physical pixels, so on a display at 175% a 1920-wide desktop was painted
    // 3360 pixels across — magnified by the guest's scaling, in the one mode
    // whose entire purpose is not to scale.
    //
    // Nothing about the host's own DPI enters this. The host captures pixels
    // and the guest draws pixels; what sits between them is the guest's own
    // point-to-pixel ratio, and that is the whole of the conversion.
    let src = if pixels_per_point > 0.0 { src / pixels_per_point } else { src };

    match mode {
        ScalingMode::Fit => {
            let scale = (avail.width() / src.x).min(avail.height() / src.y);
            Rect::from_center_size(avail.center(), src * scale)
        }
        ScalingMode::Actual => {
            let limit = pan_limit(avail.size(), src);
            let centred = Rect::from_center_size(avail.center(), src);
            // On an axis with room to spare the image is centred; on one that
            // overflows it starts at the window edge and slides by `pan`.
            let x = if limit.x > 0.0 {
                avail.left() - pan.x.clamp(0.0, limit.x)
            } else {
                centred.left()
            };
            let y = if limit.y > 0.0 {
                avail.top() - pan.y.clamp(0.0, limit.y)
            } else {
                centred.top()
            };
            Rect::from_min_size(egui::pos2(x, y), src)
        }
    }
}

/// A vertical rule dividing header sections.
///
/// Allocates its own slot rather than painting at the cursor, so it works the
/// same in left-to-right and right-to-left layouts.
pub fn header_separator(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(1.0, HEADER_RULE_H), Sense::hover());
    ui.painter().vline(
        rect.center().x,
        rect.top()..=rect.bottom(),
        Stroke::new(1.0, SEPARATOR),
    );
}

/// A right-pointing play triangle, drawn rather than imported so it can be
/// recoloured per state (the asset is a fixed red).
pub fn play_triangle(ui: &Ui, rect: Rect, color: Color32) {
    let h = rect.height();
    let w = rect.width();
    let c = rect.center();
    let points = vec![
        Pos2::new(c.x - w * 0.35, c.y - h * 0.45),
        Pos2::new(c.x + w * 0.45, c.y),
        Pos2::new(c.x - w * 0.35, c.y + h * 0.45),
    ];
    ui.painter().add(egui::Shape::convex_polygon(points, color, Stroke::NONE));
}

/// Stroke a rounded rect with a dashed outline, matching `incoming_card_pending.svg`.
/// A dashed outline, optionally rotating.
///
/// `phase` slides the dashes along the outline. Zero is a still border; feeding
/// it the clock makes the dashes travel, which is the difference between a
/// border that is merely styled and one that says *this is waiting for you*.
/// A guest sitting unanswered is the one thing on this screen that wants
/// noticing, and a static dotted rectangle was not managing it.
pub fn dashed_rect(ui: &Ui, rect: Rect, color: Color32, phase: f32) {
    /// Dash and gap, in points. Equal, so the outline reads as a rhythm rather
    /// than a dotted line with holes in it.
    const DASH: f32 = 3.0;
    const GAP: f32 = 3.0;

    let stroke = Stroke::new(1.0, color);
    let r = rect.shrink(0.5);
    let radius = CORNER.min(r.width() * 0.5).min(r.height() * 0.5).max(0.0);

    // # One closed path, not four sides
    //
    // Drawn as four separate segments the dashes restart at every corner, so
    // they shuffle in place instead of travelling, and the corners are square
    // while the card behind them is rounded. A single path around the rounded
    // outline fixes both at once: the pattern is continuous, and the corners
    // follow the same radius as the card.
    let mut path = Vec::with_capacity(4 * (ARC_STEPS + 1));
    let corners = [
        // (centre of the arc, angle it starts at) going clockwise from top-left.
        (Pos2::new(r.left() + radius, r.top() + radius), std::f32::consts::PI),
        (Pos2::new(r.right() - radius, r.top() + radius), 1.5 * std::f32::consts::PI),
        (Pos2::new(r.right() - radius, r.bottom() - radius), 0.0),
        (Pos2::new(r.left() + radius, r.bottom() - radius), 0.5 * std::f32::consts::PI),
    ];
    for (centre, start) in corners {
        for step in 0..=ARC_STEPS {
            let angle = start + (step as f32 / ARC_STEPS as f32) * 0.5 * std::f32::consts::PI;
            path.push(Pos2::new(
                centre.x + radius * angle.cos(),
                centre.y + radius * angle.sin(),
            ));
        }
    }
    // Closed, so the pattern meets itself rather than leaving a gap at the top.
    if let Some(first) = path.first().copied() {
        path.push(first);
    }

    // # The offset must be positive
    //
    // `dashes_from_line` seeds its walk with `position_on_segment = offset` and
    // interpolates `start + vector * (position / segment_length)` without ever
    // clamping. A negative offset therefore extrapolates *backwards* past the
    // beginning of the path, and every dash laid down on the way there is drawn
    // — a long line trailing off in the direction the outline came from.
    //
    // That is the stray diagonal reported against a pending card, and because
    // the phase is driven by the clock it grew with uptime: two minutes in, the
    // walk began some fourteen hundred points behind the corner and spent
    // thousands of iterations per frame reaching it.
    //
    // The pattern repeats every dash-and-gap, so folding the phase into one
    // period costs nothing and leaves the outline identical. Done here rather
    // than at the call site because no caller should have to know this.
    let phase = phase.rem_euclid(DASH + GAP);

    ui.painter().extend(egui::Shape::dashed_line_with_offset(
        &path,
        stroke,
        &[DASH],
        &[GAP],
        phase,
    ));
}

/// Points per quarter-turn of a corner. Four is smooth at this radius and keeps
/// the path short enough that the dash pattern stays even along it.
const ARC_STEPS: usize = 4;

/// How fast the dashes travel, in points per second.
///
/// Slow enough to read as waiting rather than loading — this is not progress,
/// nothing is happening, and a fast crawl would suggest otherwise.
pub const DASH_SPEED: f32 = 12.0;

#[cfg(test)]
mod icon_tests {
    use super::*;

    /// Draw one icon at `zoom` and report the size of the texture it used.
    ///
    /// The size comes from the texture manager rather than from anything the
    /// drawing code says, because what is being checked is exactly whether the
    /// two agree.
    fn icon_texture_size(ctx: &egui::Context, zoom: f32) -> [usize; 2] {
        ctx.set_zoom_factor(zoom);

        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 300.0),
            )),
            ..Default::default()
        };

        let mut id = None;
        // More than one pass: a zoom set now applies from the next pass, and
        // the first draw is what starts the load.
        for _ in 0..4 {
            let output = ctx.run(input.clone(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    icon(ui, 16.0, egui::include_image!("../../assets/settings.svg"));
                });
            });
            for clipped in ctx.tessellate(output.shapes, output.pixels_per_point) {
                if let egui::epaint::Primitive::Mesh(mesh) = clipped.primitive {
                    // Managed(0) is the font atlas, which every frame carries.
                    if !matches!(mesh.texture_id, egui::TextureId::Managed(0)) {
                        id = Some(mesh.texture_id);
                    }
                }
            }
        }

        let id = id.expect("the icon should have been drawn from a texture");
        ctx.tex_manager().read().meta(id).expect("the texture should still exist").size
    }

    /// An icon must be rasterised again when the interface scale changes.
    ///
    /// # What this is guarding
    ///
    /// Everything about this looks handled. The assets are SVG, egui asks its
    /// image loaders for `size * pixels_per_point`, and the SVG loader caches
    /// on `(uri, size hint)` — so a new scale should mean a new rasterisation.
    ///
    /// Between them sits `DefaultTextureLoader`, whose cache key is
    /// `(uri, texture options)` with the size hint left out. Once an icon has
    /// been uploaded at one resolution, every later draw gets that texture back
    /// whatever resolution it asked for, and the SVG loader is never reached.
    /// The result is an icon stretched from its original raster: soft, and
    /// softer the further the scale has moved.
    ///
    /// Nothing in this application's own code looks wrong when that happens,
    /// which is why it is worth a test rather than a comment. The fix is a
    /// single call in `app::GuestApp::note_scale_change`, and exactly the kind
    /// of line that reads as redundant to whoever finds it next.
    #[test]
    fn an_icon_is_rasterised_again_when_the_scale_changes() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);

        let small = icon_texture_size(&ctx, 1.0);

        // What the application does on noticing the scale moved.
        ctx.forget_all_images();
        let large = icon_texture_size(&ctx, 2.0);

        assert!(
            large[0] > small[0] && large[1] > small[1],
            "a 16-point icon was {small:?} pixels at 100% and {large:?} at 200%; \
             it should have been rasterised again, not stretched"
        );
    }

    /// And the same drawing at the same scale must not be re-rendered.
    ///
    /// The cache is what stops every frame rasterising every icon, so a fix
    /// that forgot them unconditionally would trade a soft icon for a
    /// permanent cost.
    #[test]
    fn an_unchanged_scale_reuses_what_was_already_rasterised() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);

        let first = icon_texture_size(&ctx, 1.0);
        let again = icon_texture_size(&ctx, 1.0);
        assert_eq!(first, again);
    }
}

#[cfg(test)]
mod tests {
    /// Icons must be asked for at physical pixels, not points.
    ///
    /// # The bug this is here for
    ///
    /// `egui::Image` defaults to `ImageFit::Original`, for which egui asks the
    /// SVG loader for `SizeHint::Scale(1.0)` — rasterise at the file's own
    /// intrinsic size, ignoring the destination and the display scaling. The
    /// assets are 24x24 and are drawn at 16 points, so every icon was being
    /// resampled: down to 16 pixels at 100% scaling, and up to a fractional 28
    /// at 175%.
    ///
    /// This pins the arithmetic `sharp_image` relies on — that an exact fit
    /// produces a hint in real pixels.
    #[test]
    fn an_exact_fit_asks_for_pixels_rather_than_points() {
        use egui::load::SizeHint;

        for (points, ppp, expected) in
            [(16.0f32, 1.0f32, 16u32), (16.0, 1.75, 28), (16.0, 1.5, 24), (22.0, 2.0, 44)]
        {
            let size = egui::ImageSize {
                max_size: Vec2::INFINITY,
                fit: egui::ImageFit::Exact(Vec2::splat(points)),
                maintain_aspect_ratio: true,
            };
            match size.hint(Vec2::splat(points), ppp) {
                SizeHint::Size(w, h) => {
                    assert_eq!((w, h), (expected, expected), "{points}pt at {ppp}x");
                }
                _ => panic!("an exact fit must give a pixel size, not a scale"),
            }
        }
    }

    /// The default fit already asks in pixels, which is worth pinning.
    ///
    /// This started life asserting the opposite — that the default ignored
    /// display scaling — which is what `sharp_image` was written to correct.
    /// The test failed, the assumption was wrong, and the real causes of the
    /// blur are recorded on `sharp_image` instead. Kept so that if egui ever
    /// does change this, it is noticed rather than assumed.
    #[test]
    fn the_default_fit_also_asks_in_physical_pixels() {
        use egui::load::SizeHint;

        let size = egui::ImageSize::default();
        match size.hint(Vec2::splat(16.0), 1.75) {
            SizeHint::Size(w, h) => assert_eq!((w, h), (28, 28)),
            _ => panic!("the default fit no longer resolves to a pixel size"),
        }
    }

    /// The header must fit, including the settings gear on the right.
    ///
    /// # The bug this is here for
    ///
    /// The passcode buttons were sized against the *full* available width, but
    /// the settings section is added last and drawn right-to-left. So the
    /// Connect block was measured against space the gear was about to occupy,
    /// and on a narrow window the connect arrow was drawn over it.
    #[test]
    fn the_header_leaves_room_for_the_settings_section() {
        // A 1200pt window at 175% scaling is about this wide, which is where
        // the overlap was first seen.
        for available in [560.0f32, 690.0, 900.0, 1400.0, 1920.0] {
            let usable = (available - SETTINGS_BLOCK_W).max(0.0);
            let button = passcode_button_width(usable);

            // Two buttons plus everything that is not a button plus the gear.
            let needed = button * 2.0 + HEADER_CHROME_W + SETTINGS_BLOCK_W;
            assert!(
                needed <= available.max(BUTTON_MIN_W * 2.0 + HEADER_CHROME_W + SETTINGS_BLOCK_W),
                "at {available}pt the header wants {needed}pt"
            );
            assert!(button >= BUTTON_MIN_W, "buttons collapsed to {button}pt");
        }
    }


    use super::*;

    #[test]
    fn header_fits_the_width_it_is_given() {
        // Two buttons plus chrome must never exceed the available width, or the
        // Connect block gets clipped off the right edge.
        for available in [640.0_f32, 810.0, 1000.0, 1200.0, 1920.0] {
            let w = passcode_button_width(available);
            assert!(
                w * 2.0 + HEADER_CHROME_W <= available.max(BUTTON_MIN_W * 2.0 + HEADER_CHROME_W),
                "header overflows at {available}pt: buttons {w}pt each"
            );
        }
    }

    #[test]
    fn button_width_stays_within_bounds() {
        assert_eq!(passcode_button_width(200.0), BUTTON_MIN_W, "clamps up when cramped");
        assert_eq!(passcode_button_width(4000.0), BUTTON_MAX_W, "clamps down when huge");
    }

    /// Every non-ASCII character in the interface must have a glyph.
    ///
    /// egui bundles a small font set, and a character outside it renders as an
    /// empty box — which is what the close buttons did with `✕` (U+2715) until
    /// they were changed to `×` (U+00D7). Nothing about that fails to compile
    /// and nothing logs; it is only visible in a screenshot. So the check lives
    /// here, and picking a symbol means adding it to this list.
    #[test]
    fn every_symbol_the_ui_draws_has_a_glyph() {
        let symbols = [
            ('×', "close and forget buttons"),
            ('—', "em dash, throughout"),
            ('…', "truncation and in-progress labels"),
            ('·', "separator in the session title"),
            ('≈', "approximate bandwidth"),
            ('⚠', "warnings and bottleneck pills"),
        ];

        let ctx = egui::Context::default();
        // Fonts are built lazily on the first frame; without one there is
        // nothing to ask about coverage.
        let _ = ctx.run(Default::default(), |_| {});

        // Resolved up front: `Context::fonts` holds a lock, and reaching back
        // into the context from inside its closure deadlocks the test.
        let style = ctx.style();
        let fonts: Vec<egui::FontId> = [egui::TextStyle::Body, egui::TextStyle::Button]
            .into_iter()
            .map(|s| s.resolve(&style))
            .collect();

        let mut missing = Vec::new();
        ctx.fonts(|f| {
            for (ch, used_for) in symbols {
                for font in &fonts {
                    if !f.has_glyph(font, ch) {
                        missing.push(format!("U+{:04X} {ch:?} ({used_for})", ch as u32));
                    }
                }
            }
        });
        assert!(missing.is_empty(), "these render as empty boxes: {missing:?}");
    }

    mod scaling {
        use super::*;
        use crate::gui::settings::ScalingMode;

        fn avail() -> Rect {
            Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(800.0, 600.0))
        }

        /// Most cases don't care about panning.
        fn scaled_rect(mode: ScalingMode, avail: Rect, src: Vec2) -> Rect {
            // One point per pixel, so the existing expectations still describe
            // what they always did.
            super::super::scaled_rect(mode, avail, src, Vec2::ZERO, 1.0)
        }

        #[test]
        fn fit_preserves_aspect_and_letterboxes() {
            // 16:9 source into a 4:3 area — width-limited, bars top and bottom.
            let r = scaled_rect(ScalingMode::Fit, avail(), Vec2::new(1600.0, 900.0));
            assert!((r.width() - 800.0).abs() < 0.01);
            assert!((r.height() - 450.0).abs() < 0.01);
            assert!(r.height() < avail().height(), "expected letterboxing");
            assert_eq!(r.center(), avail().center());
        }

        #[test]
        fn fit_is_height_limited_for_tall_sources() {
            let r = scaled_rect(ScalingMode::Fit, avail(), Vec2::new(600.0, 1200.0));
            assert!((r.height() - 600.0).abs() < 0.01);
            assert!(r.width() < avail().width());
        }

        #[test]
        fn actual_keeps_source_pixels_and_may_overflow() {
            let big = Vec2::new(1920.0, 1080.0);
            let r = scaled_rect(ScalingMode::Actual, avail(), big);
            assert_eq!(r.size(), big);
            // Larger than the viewport: the caller clips rather than shrinking.
            assert!(r.width() > avail().width());
            // Unpanned, the top-left of the remote screen is what you see.
            assert_eq!(r.min, avail().min);
        }

        #[test]
        fn panning_slides_the_visible_region_of_a_bigger_screen() {
            let big = Vec2::new(1920.0, 1080.0);
            let panned = super::super::scaled_rect(
                ScalingMode::Actual,
                avail(),
                big,
                Vec2::new(200.0, 100.0),
                1.0,
            );
            // Panning right by 200 moves the image 200 to the left.
            assert_eq!(panned.left(), avail().left() - 200.0);
            assert_eq!(panned.top(), avail().top() - 100.0);
            assert_eq!(panned.size(), big, "panning must not resize anything");
        }

        #[test]
        fn panning_cannot_push_the_screen_past_its_own_edge() {
            // A stale offset from a larger frame must not scroll into nothing.
            let big = Vec2::new(1920.0, 1080.0);
            let far = super::super::scaled_rect(
                ScalingMode::Actual,
                avail(),
                big,
                Vec2::splat(99_999.0),
                1.0,
            );
            assert_eq!(far.right(), avail().right(), "the far edge should stop flush");
            assert_eq!(far.bottom(), avail().bottom());

            let negative = super::super::scaled_rect(
                ScalingMode::Actual,
                avail(),
                big,
                Vec2::splat(-500.0),
                1.0,
            );
            assert_eq!(negative.min, avail().min);
        }

        #[test]
        fn panning_does_nothing_on_an_axis_with_room_to_spare() {
            // Wide but short: horizontally there is somewhere to go, vertically
            // the image is centred and must stay put.
            let src = Vec2::new(1600.0, 400.0);
            let r = super::super::scaled_rect(
                ScalingMode::Actual,
                avail(),
                src,
                Vec2::new(300.0, 300.0),
                1.0,
            );
            assert_eq!(r.left(), avail().left() - 300.0);
            assert_eq!(r.center().y, avail().center().y);
        }

        /// 1:1 means one host pixel per guest pixel, not one per point.
        ///
        /// A display at 175% has 1.75 physical pixels to a point. Drawing the
        /// host's 1920-pixel desktop as 1920 *points* asks for 3360 pixels —
        /// magnifying it, in the one mode whose whole purpose is not to.
        #[test]
        fn one_to_one_is_pixels_not_points() {
            let src = Vec2::new(1920.0, 1080.0);
            // A window big enough that neither axis has to pan.
            let room = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(4000.0, 3000.0));

            let unscaled = super::super::scaled_rect(
                ScalingMode::Actual,
                room,
                src,
                Vec2::ZERO,
                1.0,
            );
            assert_eq!(unscaled.size(), src, "at 100% a point is a pixel");

            let scaled = super::super::scaled_rect(
                ScalingMode::Actual,
                room,
                src,
                Vec2::ZERO,
                1.75,
            );
            // Compared with a tolerance: these are divisions, and the last
            // bits of a float are not what the assertion is about.
            let close = |a: Vec2, b: Vec2| (a - b).abs().max_elem() < 0.01;
            assert!(
                close(scaled.size(), src / 1.75),
                "at 175% the same pixels occupy fewer points: {:?}",
                scaled.size(),
            );

            // Which is the point: both cover the same number of physical
            // pixels once egui multiplies back up.
            assert!(close(scaled.size() * 1.75, unscaled.size()));
        }

        #[test]
        fn fit_ignores_panning_entirely() {
            let src = Vec2::new(1600.0, 900.0);
            let still = scaled_rect(ScalingMode::Fit, avail(), src);
            let panned =
                super::super::scaled_rect(ScalingMode::Fit, avail(), src, Vec2::splat(400.0), 1.0);
            assert_eq!(still, panned, "there is nothing hidden to pan to when it all fits");
        }

        #[test]
        fn the_pan_limit_is_whatever_does_not_fit() {
            let limit = super::super::pan_limit(Vec2::new(800.0, 600.0), Vec2::new(1920.0, 1080.0));
            assert_eq!(limit, Vec2::new(1120.0, 480.0));

            // Nothing hidden means nowhere to pan.
            let none = super::super::pan_limit(Vec2::new(800.0, 600.0), Vec2::new(320.0, 240.0));
            assert_eq!(none, Vec2::ZERO);
        }

        #[test]
        fn actual_centres_a_smaller_source_without_upscaling() {
            let r = scaled_rect(ScalingMode::Actual, avail(), Vec2::new(320.0, 180.0));
            assert_eq!(r.size(), Vec2::new(320.0, 180.0));
            assert_eq!(r.center(), avail().center());
        }

        #[test]
        fn degenerate_source_does_not_produce_nan() {
            for mode in ScalingMode::ALL {
                let r = scaled_rect(mode, avail(), Vec2::ZERO);
                assert!(r.width().is_finite() && r.height().is_finite());
            }
        }
    }
}
