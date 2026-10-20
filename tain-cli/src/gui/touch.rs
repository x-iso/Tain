//! Making Tain usable under a finger.
//!
//! # What egui already does
//!
//! winit reports a touch, and egui-winit turns the first finger into a pointer:
//! it moves there and presses where the finger lands, and lifts and vanishes
//! where it leaves. A tap is therefore a click on whatever was under the finger
//! when it went down, and a press held still for `max_click_duration` is a
//! secondary click — `secondary_clicked` is true for it — so neither needs
//! anything here.
//!
//! Windows sends no mouse messages of its own on top: it only makes them from a
//! touch the window left to the default procedure, and winit consumes the
//! pointer messages. Measured with an injected tap — a window that passes them
//! on gets `WM_TOUCH` and a promoted move, press and release; one that consumes
//! them, as winit does, gets neither.
//!
//! # What it does not
//!
//! It treats the finger as a mouse. Five points of slack around a widget is a
//! mouse pointer's, and a fingertip's contact is several millimetres across;
//! six points of travel before a press stops being a click is less than a
//! fingertip rolls as it lands; every label can be dragged to select its text,
//! which is a scroll as far as a finger is concerned; and nothing shows where
//! the finger is, or that holding it is about to become a right-click.
//!
//! So this watches where input is coming from, and while it is a finger:
//!
//! - widens egui's hit radius and click slop, so a near miss still lands and a
//!   tap that rolls is still a tap;
//! - stops labels taking drags, so dragging anywhere in a list scrolls it;
//! - draws a mark under the finger, with a ring that fills toward the long
//!   press — see [`paint_feedback`].
//!
//! The first two go back to what they were as soon as the mouse is used again.

use std::sync::Mutex;

use egui::{Color32, Context, Event, Id, LayerId, Order, Pos2, Stroke};

/// How far from a widget a finger may land and still hit it, in points.
///
/// egui only uses it when the finger is on nothing that takes input at all, so
/// it widens small targets without stealing from neighbours that were hit
/// squarely.
const TOUCH_RADIUS: f32 = 16.0;

/// How far a finger may travel and still be tapping rather than dragging.
const TOUCH_SLOP: f32 = 12.0;

/// Where there is no system cursor to ask — see [`observe`] — how soon a second
/// mouse movement has to follow the first for the mouse to be believed, in
/// seconds. Timed rather than counted in frames because two windows feed this,
/// and neither sees the other's frames.
const MOUSE_CONFIRM: f64 = 0.5;

struct State {
    touch: bool,
    /// When the last mouse movement arrived, while a finger was in charge.
    last_move: Option<f64>,
    /// Where the system cursor was at the last look, in screen pixels.
    last_cursor: Option<(i32, i32)>,
    /// What the touch settings replaced, put back when the mouse returns.
    saved: Option<Saved>,
}

#[derive(Clone, Copy)]
struct Saved {
    interact_radius: f32,
    selectable_labels: bool,
    max_click_dist: f32,
}

static STATE: Mutex<State> =
    Mutex::new(State { touch: false, last_move: None, last_cursor: None, saved: None });

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether the last input came from a finger (or a pen, which winit reports
/// the same way).
pub fn active() -> bool {
    state().touch
}

/// Note where a frame's input came from, and switch the settings to suit.
/// `time` is the frame's input time, in egui's seconds.
///
/// Best called before egui sees the input — from the main window's raw-input
/// hook — so the first tap after using the mouse already gets the wider radius.
/// An immediate viewport has no such hook and calls it from inside its frame,
/// where the change applies from the next one.
///
/// # A move is not a mouse
///
/// Windows posts mouse moves with no hand on the mouse — whenever a window
/// appears, goes or changes size under a resting pointer — and two of those
/// close together used to be enough to take a finger's settings away. In
/// fullscreen that hid the bar a finger had just brought down. So where it can,
/// this asks whether the system cursor actually moved: a mouse moves it, and a
/// finger, whose pointer messages winit consumes, leaves it where it was.
pub fn observe(ctx: &Context, events: &[Event], time: f64) {
    observe_with(ctx, events, time, platform::cursor());
}

fn observe_with(ctx: &Context, events: &[Event], time: f64, cursor: Option<(i32, i32)>) {
    let touched = events.iter().any(|e| matches!(e, Event::Touch { .. }));
    let clicked = events.iter().any(|e| matches!(e, Event::PointerButton { pressed: true, .. }));
    let moved = events.iter().any(|e| matches!(e, Event::PointerMoved(_)));
    // A wheel is a mouse or a touchpad, never a screen.
    let wheeled = events.iter().any(|e| matches!(e, Event::MouseWheel { .. }));

    let switch = {
        let mut state = state();
        let cursor_moved = match (cursor, state.last_cursor) {
            (Some(now), Some(before)) => Some(now != before),
            (Some(_), None) => Some(false),
            (None, _) => None,
        };
        if cursor.is_some() {
            state.last_cursor = cursor;
        }

        let now = if touched {
            state.last_move = None;
            true
        } else if clicked || wheeled {
            // egui-winit's simulated press always comes with the touch that
            // made it, so a press on its own is a real button.
            false
        } else if moved && state.touch {
            match cursor_moved {
                Some(moved) => !moved,
                None => {
                    let confirmed =
                        state.last_move.is_some_and(|last| time - last < MOUSE_CONFIRM);
                    state.last_move = Some(time);
                    !confirmed
                }
            }
        } else {
            state.touch
        };
        if now == state.touch {
            return;
        }
        state.touch = now;
        tracing::debug!(
            touched,
            clicked,
            wheeled,
            moved,
            ?cursor_moved,
            "Input is now coming from {}",
            if now { "a finger" } else { "the mouse" }
        );
        now
    };
    apply(ctx, switch);
}

fn apply(ctx: &Context, touch: bool) {
    if touch {
        let saved = Saved {
            interact_radius: ctx.style().interaction.interact_radius,
            selectable_labels: ctx.style().interaction.selectable_labels,
            max_click_dist: ctx.options(|o| o.input_options.max_click_dist),
        };
        state().saved.get_or_insert(saved);
        ctx.style_mut(|style| {
            style.interaction.interact_radius = TOUCH_RADIUS;
            style.interaction.selectable_labels = false;
        });
        ctx.options_mut(|o| o.input_options.max_click_dist = TOUCH_SLOP);
    } else if let Some(saved) = state().saved.take() {
        ctx.style_mut(|style| {
            style.interaction.interact_radius = saved.interact_radius;
            style.interaction.selectable_labels = saved.selectable_labels;
        });
        ctx.options_mut(|o| o.input_options.max_click_dist = saved.max_click_dist);
    }
}

/// Mark where the finger is, while it is down, and how far holding it has got
/// toward a right-click.
///
/// # Why there is a mark at all
///
/// A mouse pointer is always on screen, so a control that lights up under it
/// has said "this is what you are about to press" before anything is pressed.
/// A finger hides the control it is on and shows nothing of its own, and a
/// control too small to show a change from under a fingertip leaves no sign
/// that anything was touched — the complaint about the saved-connection card's
/// button, which looked dead whether or not it had been hit.
///
/// The ring is for the hold. A long press is a right-click, and with nothing
/// counting it down there is no telling a hold that is about to open a menu
/// from one that is simply being slow.
///
/// `suppressed` leaves it out, for a finger on the remote screen: that touch is
/// the host's, which draws its own.
pub fn paint_feedback(ctx: &Context, suppressed: bool) {
    if suppressed || !active() {
        return;
    }
    let (pos, down, started, still, now) = ctx.input(|i| {
        (
            i.pointer.latest_pos(),
            i.any_touches() && i.pointer.primary_down(),
            i.pointer.press_start_time(),
            i.pointer.could_any_button_be_click(),
            i.time,
        )
    });
    let (Some(pos), true) = (pos, down) else { return };

    const RADIUS: f32 = 22.0;
    let painter = ctx.layer_painter(LayerId::new(Order::Tooltip, Id::new("touch-feedback")));
    painter.circle_filled(pos, RADIUS, Color32::from_white_alpha(24));
    painter.circle_stroke(pos, RADIUS, Stroke::new(1.0, Color32::from_white_alpha(70)));

    // Only while the finger is still: once it moves it is dragging, and a
    // drag never becomes a right-click.
    let Some(started) = started.filter(|_| still) else { return };
    let hold = ctx.options(|o| o.input_options.max_click_duration);
    let progress = ((now - started) / hold).clamp(0.0, 1.0) as f32;
    // Not for an ordinary tap, which is over before this would show.
    if progress > 0.2 {
        arc(&painter, pos, RADIUS + 3.0, progress, Stroke::new(3.0, super::theme::SELECT_BLUE));
    }
    if progress < 1.0 {
        ctx.request_repaint();
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    /// Where the system cursor is, in screen pixels.
    pub fn cursor() -> Option<(i32, i32)> {
        let mut point = POINT::default();
        // SAFETY: writes one POINT, which is all it is given.
        unsafe { GetCursorPos(&mut point) }.ok()?;
        Some((point.x, point.y))
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    pub fn cursor() -> Option<(i32, i32)> {
        None
    }
}

/// A clockwise arc from twelve o'clock, `fraction` of the way round.
fn arc(painter: &egui::Painter, centre: Pos2, radius: f32, fraction: f32, stroke: Stroke) {
    const STEPS: usize = 48;
    let end = (STEPS as f32 * fraction).ceil() as usize;
    let points: Vec<Pos2> = (0..=end)
        .map(|step| {
            let angle = (step as f32 / STEPS as f32).min(fraction) * std::f32::consts::TAU
                - std::f32::consts::FRAC_PI_2;
            centre + radius * egui::vec2(angle.cos(), angle.sin())
        })
        .collect();
    painter.add(egui::Shape::line(points, stroke));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch_event() -> Event {
        Event::Touch {
            device_id: egui::TouchDeviceId(0),
            id: egui::TouchId(0),
            phase: egui::TouchPhase::Start,
            pos: egui::pos2(10.0, 10.0),
            force: None,
        }
    }

    fn moved_to(x: f32) -> Event {
        Event::PointerMoved(egui::pos2(x, 200.0))
    }

    // One test, because the mode is process-wide and tests run in parallel.
    #[test]
    fn a_finger_widens_the_targets_and_the_mouse_takes_them_back() {
        let ctx = Context::default();
        let radius = ctx.style().interaction.interact_radius;
        let slop = ctx.options(|o| o.input_options.max_click_dist);
        let resting = Some((500, 500));

        observe_with(&ctx, &[touch_event(), moved_to(10.0)], 1.0, resting);
        assert!(active());
        assert_eq!(ctx.style().interaction.interact_radius, TOUCH_RADIUS);
        assert!(!ctx.style().interaction.selectable_labels, "a drag on a label scrolls");
        assert_eq!(ctx.options(|o| o.input_options.max_click_dist), TOUCH_SLOP);

        // # Where there is a system cursor
        //
        // Moves that leave it where it was are Windows' own, however many and
        // however close together — a window appearing under a resting mouse.
        for t in [2.0, 2.1, 2.2] {
            observe_with(&ctx, &[moved_to(300.0)], t, resting);
        }
        assert!(active(), "moves with a resting cursor are not a mouse");
        // The cursor moving is.
        observe_with(&ctx, &[moved_to(310.0)], 2.3, Some((510, 500)));
        assert!(!active());
        assert_eq!(ctx.style().interaction.interact_radius, radius);
        assert!(ctx.style().interaction.selectable_labels);
        assert_eq!(ctx.options(|o| o.input_options.max_click_dist), slop);

        // # Where there is not
        //
        // One stray move is not somebody picking up the mouse, and neither is
        // another long after it; two close together are.
        observe_with(&ctx, &[touch_event()], 3.0, None);
        assert!(active());
        observe_with(&ctx, &[moved_to(300.0)], 4.0, None);
        assert!(active(), "a single move must not undo the finger's settings");
        observe_with(&ctx, &[moved_to(300.0)], 9.0, None);
        assert!(active(), "two stray moves seconds apart are still not a mouse");
        observe_with(&ctx, &[moved_to(301.0)], 9.05, None);
        assert!(!active());

        // A real click, or a wheel, is believed at once.
        observe_with(&ctx, &[touch_event()], 10.0, resting);
        assert!(active());
        observe_with(
            &ctx,
            &[Event::PointerButton {
                pos: egui::pos2(5.0, 5.0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            10.5,
            resting,
        );
        assert!(!active());
        observe_with(&ctx, &[touch_event()], 11.0, resting);
        observe_with(
            &ctx,
            &[Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: egui::vec2(0.0, -1.0),
                modifiers: egui::Modifiers::NONE,
            }],
            11.5,
            resting,
        );
        assert!(!active());
    }
}
