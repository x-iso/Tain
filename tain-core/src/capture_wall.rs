//! Several displays composed into one picture.
//!
//! # What this is for
//!
//! A guest that wants to see the whole desk at once. Each display is captured
//! on its own — one duplication per output, exactly as [`super::capture_shared`]
//! already provides — and the results are laid out here the way the host has
//! them arranged, so what arrives is a picture of the desk rather than an
//! arbitrary row of screens.
//!
//! # One stream, not one per display
//!
//! The obvious alternative is a stream per display, composed by the guest. It
//! would give better quality, and it was not chosen:
//!
//! - **Mixed HDR needs one answer.** A composed surface has a single transfer
//!   function, so an HDR monitor beside an SDR one has to be tone-mapped — which
//!   the capture path already does per display, and which a guest compositing
//!   two decoded streams of different transfer functions would have to redo.
//! - **Encoders are limited and few.** Hardware encode sessions are a scarce
//!   resource — NVIDIA's consumer drivers have historically capped concurrent
//!   NVENC sessions at a handful — and three displays would want three.
//! - **The scale is already agreed.** The guest's window is a ceiling the host
//!   is told about, so a wall being shrunk to fit is shrunk once, before the
//!   encoder, rather than encoded at full size and thrown away on arrival.
//!
//! What that costs is the composition itself, which is a copy per display per
//! frame, and the loss of the direct-NV12 capture path — see [`Wall::compose`].
//!
//! # A thread per display, because they block
//!
//! A subscriber's `capture_next` waits for its display to change, up to about
//! twenty-five milliseconds. Polling three of them in turn is therefore up to
//! seventy-five milliseconds a frame — thirteen frames a second, on hardware
//! doing nothing. So each display gets a thread that pumps it into a slot, and
//! composition takes whatever is current.
//!
//! That also gets the semantics right. Displays change independently, and a
//! wall where one screen is playing video and two are still should run at the
//! rate of the one that is moving.

use anyhow::{Context, Result};
use bytes::Bytes;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use tracing::*;

use super::protocol::DisplayInfo;
use super::screen_capture::{
    fit_within, PixelFormat, QualityScale, ScreenCaptureBackend, ScreenFrame,
};

/// How long to wait for any display to produce something before giving up on
/// this tick. Comfortably longer than one panel's own wait.
const WAIT: std::time::Duration = std::time::Duration::from_millis(40);

/// One display's capture, its latest frame, and where it belongs.
struct Panel {
    /// Behind a mutex because the pump thread captures through it and the
    /// composing side has to reconfigure it — see [`Wall::resize_panels`],
    /// which is the difference between each screen being *scaled* into its
    /// place on the wall and being *cropped* to it.
    ///
    /// Contended for the length of one capture, which is the same arrangement
    /// [`super::capture_shared`] uses for the same reason.
    capturer: Mutex<Box<dyn ScreenCaptureBackend>>,
    /// Offset within the wall, in wall pixels. Never negative: the layout is
    /// shifted so the top-left of the bounding box is the origin.
    x: u32,
    y: u32,
    /// The display's own size, for working out its share of the wall.
    source: (u16, u16),
    /// The latest frame this display produced, if it has produced one.
    frame: Mutex<Option<ScreenFrame>>,
}

/// Displays composed into one picture, laid out as the host has them.
pub struct Wall {
    panels: Vec<Arc<Panel>>,
    pumps: Vec<std::thread::JoinHandle<()>>,
    stopping: Arc<AtomicBool>,
    /// Signalled whenever any display publishes a frame.
    fresh: Arc<(Mutex<bool>, Condvar)>,
    /// The whole desk, in virtual-desktop pixels.
    bounds: (u32, u32),
    /// What this emits, after the scale and any ceiling.
    out: (u16, u16),
    scale: QualityScale,
    ceiling: Option<(u16, u16)>,
    /// Reused between frames; this is several megabytes.
    buffer: Vec<u8>,
}

impl Wall {
    /// Compose these displays, capturing each one.
    ///
    /// Fails only if no display could be captured at all. One display out of
    /// three failing leaves a hole in the picture, which is better than no
    /// picture — and the log says which.
    pub fn new(displays: &[DisplayInfo]) -> Result<Self> {
        anyhow::ensure!(!displays.is_empty(), "a wall of no displays is not a picture");

        // The bounding box of the arrangement. Positions can be negative — a
        // monitor left of the primary one — so the origin moves rather than
        // being assumed.
        let left = displays.iter().map(|d| d.x).min().unwrap_or(0);
        let top = displays.iter().map(|d| d.y).min().unwrap_or(0);
        let right = displays.iter().map(|d| d.x + i32::from(d.width)).max().unwrap_or(0);
        let bottom = displays.iter().map(|d| d.y + i32::from(d.height)).max().unwrap_or(0);
        let bounds = ((right - left).max(1) as u32, (bottom - top).max(1) as u32);

        let stopping = Arc::new(AtomicBool::new(false));
        let fresh = Arc::new((Mutex::new(false), Condvar::new()));
        let (mut panels, mut pumps) = (Vec::new(), Vec::new());

        // Named `screen`, not `display`: `use tracing::*` brings a `display`
        // function into scope and the macros below resolve the identifier to it.
        for screen in displays {
            let capturer = match super::capture_shared::attach(Some(screen.name.clone())) {
                Ok(capturer) => capturer,
                Err(e) => {
                    // A hole rather than a failure: the rest of the desk is
                    // still worth showing, and the guest can see which part is
                    // missing.
                    warn!("Leaving {} out of the wall: {e:#}", screen.name);
                    continue;
                }
            };

            let panel = Arc::new(Panel {
                capturer: Mutex::new(capturer),
                x: (screen.x - left).max(0) as u32,
                y: (screen.y - top).max(0) as u32,
                source: (screen.width, screen.height),
                frame: Mutex::new(None),
            });
            panels.push(Arc::clone(&panel));
            pumps.push(spawn_pump(
                screen.name.clone(),
                panel,
                Arc::clone(&stopping),
                Arc::clone(&fresh),
            )?);
        }

        anyhow::ensure!(!panels.is_empty(), "none of the host's displays could be captured");

        let out = QualityScale::Native.apply((
            bounds.0.min(u16::MAX as u32) as u16,
            bounds.1.min(u16::MAX as u32) as u16,
        ));
        info!(
            "Composing {} display(s) into a {}x{} wall",
            panels.len(),
            out.0,
            out.1
        );

        let wall = Self {
            panels,
            pumps,
            stopping,
            fresh,
            bounds,
            out,
            scale: QualityScale::Native,
            ceiling: None,
            buffer: Vec::new(),
        };
        // Before the first frame, not on the first resize: a display whose slot
        // is smaller than itself has to be told so, and at native scale a wall
        // of two differently sized screens already has one.
        wall.resize_panels();
        Ok(wall)
    }

    /// The size this should emit, from the wall's own size and what has been
    /// asked of it.
    fn target(&self) -> (u16, u16) {
        let native = (
            self.bounds.0.min(u16::MAX as u32) as u16,
            self.bounds.1.min(u16::MAX as u32) as u16,
        );
        let scaled = self.scale.apply(native);
        match self.ceiling {
            Some(ceiling) => {
                let (w, h) = fit_within(scaled, ceiling);
                // Even, as every codec here needs.
                ((w.max(2)) & !1, (h.max(2)) & !1)
            }
            None => scaled,
        }
    }

    /// Ask each display for the size its place on the wall actually needs.
    ///
    /// # Not an optimisation
    ///
    /// Composition copies rectangles; it does not resample. A 2560-wide display
    /// dropped into a 1280-wide slot without this would have its right half
    /// clipped away rather than shrunk — the wall would show the top-left corner
    /// of every screen, which looks like a layout bug and is really a missing
    /// scale.
    ///
    /// The capture path scales on the GPU, so asking each display for its slot
    /// size gets it done for free and leaves this a memcpy. Aspect is preserved
    /// on both sides — a slot is the display's own share of the bounding box —
    /// so `fit_within` inside the capturer lands on the same shape.
    fn resize_panels(&self) {
        for panel in &self.panels {
            let slot = self.slot(panel);
            let mut capturer = panel.capturer.lock().unwrap_or_else(|e| e.into_inner());
            capturer.set_size_ceiling(Some(slot));
        }
    }

    /// The size and position this display occupies on the wall.
    fn slot(&self, panel: &Panel) -> (u16, u16) {
        let placed = place(panel.source, (panel.x, panel.y), self.bounds, self.out);
        (placed.width, placed.height)
    }

    /// Lay the panels into the buffer.
    ///
    /// # Why BGRA only
    ///
    /// Composition is a copy of rectangles, and NV12 is planar with chroma at
    /// half resolution in both axes — every offset and every size would have to
    /// be even, and a panel landing on an odd row would shift the colour of the
    /// whole screen below it. The direct-NV12 capture path is given up here in
    /// exchange for one obvious loop. A wall is already paying a copy per
    /// display; this is not where the cost is.
    fn compose(&mut self) -> ScreenFrame {
        let started = std::time::Instant::now();
        let (out_w, out_h) = self.out;
        let stride = usize::from(out_w) * 4;
        let needed = stride * usize::from(out_h);
        if self.buffer.len() != needed {
            self.buffer = vec![0; needed];
        } else {
            // Cleared so a display that has dropped out leaves black rather
            // than the last thing it showed, frozen, for as long as the session
            // lasts.
            self.buffer.fill(0);
        }

        for panel in &self.panels {
            let frame = panel.frame.lock().unwrap_or_else(|e| e.into_inner());
            let Some(frame) = frame.as_ref() else { continue };
            if frame.format != PixelFormat::Bgra {
                continue;
            }

            // Where this panel starts on the wall, scaled from its position on
            // the desk. Computed per frame because the emitted size changes
            // when the guest resizes its window.
            let placed = place(panel.source, (panel.x, panel.y), self.bounds, self.out);
            let (x, y) = (usize::from(placed.x), usize::from(placed.y));
            if x >= usize::from(out_w) || y >= usize::from(out_h) {
                continue;
            }

            let src_stride = usize::from(frame.width) * 4;
            let columns = usize::from(frame.width).min(usize::from(out_w) - x);
            let rows = usize::from(frame.height).min(usize::from(out_h) - y);
            for row in 0..rows {
                let from = row * src_stride;
                let to = (y + row) * stride + x * 4;
                let (Some(src), Some(dst)) = (
                    frame.data.get(from..from + columns * 4),
                    self.buffer.get_mut(to..to + columns * 4),
                ) else {
                    break;
                };
                dst.copy_from_slice(src);
            }
        }

        ScreenFrame::new(
            out_w,
            out_h,
            Bytes::copy_from_slice(&self.buffer),
            started.elapsed(),
        )
    }
}

/// Where one display sits on the wall, in wall pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Placed {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

/// Scale a display's place on the desk into its place on the wall.
///
/// Free and taking everything explicitly so the arithmetic can be tested
/// against arrangements this machine does not have — which is most of them.
///
/// Sizes are forced even because every codec here needs even dimensions, and a
/// panel is a picture in its own right before it is part of a bigger one.
fn place(source: (u16, u16), at: (u32, u32), bounds: (u32, u32), out: (u16, u16)) -> Placed {
    let scale = |value: u64, span: u32, out: u16| {
        (value * u64::from(out) / u64::from(span.max(1))) as u16
    };
    let even = |v: u16| (v & !1).max(2);
    Placed {
        x: scale(u64::from(at.0), bounds.0, out.0),
        y: scale(u64::from(at.1), bounds.1, out.1),
        width: even(scale(u64::from(source.0), bounds.0, out.0)),
        height: even(scale(u64::from(source.1), bounds.1, out.1)),
    }
}

/// Pump one display into its slot until told to stop.
fn spawn_pump(
    name: String,
    panel: Arc<Panel>,
    stopping: Arc<AtomicBool>,
    fresh: Arc<(Mutex<bool>, Condvar)>,
) -> Result<std::thread::JoinHandle<()>> {
    let thread_name = format!("tain-wall-{name}");
    std::thread::Builder::new()
        .name(thread_name.clone())
        .spawn(move || {
            // BGRA, for the reason given on `Wall::compose`.
            panel
                .capturer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .request_format(PixelFormat::Bgra);

            while !stopping.load(Ordering::Relaxed) {
                // Locked for one capture, then released, so the composing side
                // can reconfigure between frames rather than waiting for the
                // session to end.
                let got = {
                    let mut capturer =
                        panel.capturer.lock().unwrap_or_else(|e| e.into_inner());
                    capturer.capture_next()
                };
                match got {
                    Ok(Some(frame)) => {
                        *panel.frame.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
                        let (lock, cv) = &*fresh;
                        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
                        cv.notify_all();
                    }
                    // Nothing changed on this display, which is the ordinary
                    // case for a wall: most screens are still most of the time.
                    Ok(None) => {}
                    Err(e) => {
                        warn!("{name} stopped contributing to the wall: {e:#}");
                        return;
                    }
                }
            }
        })
        .with_context(|| format!("could not start {thread_name}"))
}

impl ScreenCaptureBackend for Wall {
    fn capture_next(&mut self) -> Result<Option<ScreenFrame>> {
        let target = self.target();
        if target != self.out {
            self.out = target;
            self.resize_panels();
        }

        // Wait for *any* display to have something new. A wall of still screens
        // legitimately produces nothing, which is what `None` is for.
        let (lock, cv) = &*Arc::clone(&self.fresh);
        let mut ready = lock.lock().unwrap_or_else(|e| e.into_inner());
        if !*ready {
            let (again, _) =
                cv.wait_timeout(ready, WAIT).unwrap_or_else(|e| e.into_inner());
            ready = again;
        }
        if !*ready {
            return Ok(None);
        }
        *ready = false;
        drop(ready);

        Ok(Some(self.compose()))
    }

    fn frame_size(&self) -> (u16, u16) {
        self.out
    }

    fn set_scale(&mut self, scale: QualityScale) -> bool {
        self.scale = scale;
        true
    }

    fn set_size_ceiling(&mut self, ceiling: Option<(u16, u16)>) -> bool {
        self.ceiling = ceiling;
        true
    }

    fn request_format(&mut self, format: PixelFormat) -> bool {
        // See `compose`: a wall is composed in BGRA and nothing else.
        format == PixelFormat::Bgra
    }

    fn is_real(&self) -> bool {
        true
    }
}

impl Drop for Wall {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Relaxed);
        for pump in self.pumps.drain(..) {
            // Each pump wakes within one capture timeout, so this is a short
            // wait. Joining rather than detaching matters: the duplications
            // these hold have to be released before another session can take
            // them, which is the same reason `capture_shared` waits.
            let _ = pump.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(id: u32, x: i32, y: i32, w: u16, h: u16) -> DisplayInfo {
        DisplayInfo {
            id,
            name: format!(r"\\.\DISPLAY{}", id + 1),
            width: w,
            height: h,
            x,
            y,
            primary: x == 0 && y == 0,
            hdr: false,
            virtual_display: false,
        }
    }

    /// The bounding box covers every display, wherever they are.
    ///
    /// Worth its own test because the positions can be negative and the obvious
    /// implementation — take the widths and add them up — is right only for
    /// monitors in a row to the right of the primary one.
    #[test]
    fn the_wall_is_the_bounding_box_of_the_arrangement() {
        let displays = [screen(0, 0, 0, 2560, 1600), screen(1, -1920, -100, 1920, 1080)];

        let left = displays.iter().map(|d| d.x).min().unwrap();
        let top = displays.iter().map(|d| d.y).min().unwrap();
        let right = displays.iter().map(|d| d.x + i32::from(d.width)).max().unwrap();
        let bottom = displays.iter().map(|d| d.y + i32::from(d.height)).max().unwrap();

        assert_eq!((left, top), (-1920, -100), "the origin is not the primary's");
        assert_eq!((right - left, bottom - top), (4480, 1700));
    }

    /// A wall of nothing is refused rather than producing an empty picture.
    #[test]
    fn a_wall_needs_at_least_one_display() {
        assert!(Wall::new(&[]).is_err());
    }

    /// Two screens side by side fill the wall between them.
    ///
    /// The property that matters is that the panels tile: no gap that would
    /// show as a black stripe down the middle, and no overlap that would put
    /// one screen's right-hand edge over the other's left.
    #[test]
    fn two_screens_side_by_side_tile_the_wall() {
        let bounds = (3840u32, 1080u32);
        let out = (1920u16, 540u16);

        let left = place((1920, 1080), (0, 0), bounds, out);
        let right = place((1920, 1080), (1920, 0), bounds, out);

        assert_eq!(left, Placed { x: 0, y: 0, width: 960, height: 540 });
        assert_eq!(right, Placed { x: 960, y: 0, width: 960, height: 540 });
        assert_eq!(
            left.x + left.width,
            right.x,
            "a gap or an overlap between the two halves of the wall"
        );
        assert_eq!(right.x + right.width, out.0, "the wall is not filled to its right edge");
    }

    /// Screens of different sizes keep their share, not an equal one.
    ///
    /// A 2560 beside a 1920 is not two halves. Splitting the wall evenly would
    /// stretch one and squash the other, which is exactly what the arrangement
    /// is supposed to preserve.
    #[test]
    fn a_mixed_pair_keeps_each_screens_share() {
        let bounds = (4480u32, 1600u32);
        let out = (2240u16, 800u16);

        let main = place((2560, 1600), (0, 0), bounds, out);
        let side = place((1920, 1080), (2560, 0), bounds, out);

        assert_eq!(main.width, 1280, "the larger screen did not keep its share");
        assert_eq!(side.width, 960, "the smaller screen did not keep its share");
        assert_eq!(main.x + main.width, side.x, "the two do not meet");
        // Aspect is preserved: the side screen is 1920x1080 in a 4480x1600 box.
        assert_eq!(side.height, 540);
    }

    /// A screen above the primary one lands in the top half.
    ///
    /// Positions are shifted to a non-negative origin before they get here, so
    /// what this checks is that the vertical axis is scaled at all — an early
    /// version scaled x by the width and y by the width too.
    #[test]
    fn a_stacked_screen_lands_above_and_not_beside() {
        let bounds = (2560u32, 2680u32);
        let out = (1280u16, 1340u16);

        // The upper screen occupies the first 1080 rows of the desk.
        let upper = place((1920, 1080), (0, 0), bounds, out);
        // The primary sits below it.
        let lower = place((2560, 1600), (0, 1080), bounds, out);

        assert_eq!(upper.y, 0);
        assert_eq!(lower.y, 540, "the lower screen is not below the upper one");
        assert_eq!(upper.height, 540);
        assert_eq!(lower.x, 0, "a stacked screen must not be shifted sideways");
    }

    /// Every panel is even-sized, whatever the arrangement divides into.
    ///
    /// Odd dimensions break chroma subsampling in every codec here, and a wall
    /// is composed from sizes that are the result of two divisions.
    #[test]
    fn panels_are_always_even_sized() {
        for out_w in [640u16, 1002, 1366, 1920, 2559] {
            for source in [(1365u16, 767u16), (1920, 1080), (2560, 1600), (3, 3)] {
                let placed = place(source, (0, 0), (4097, 2161), (out_w, 700));
                assert_eq!(placed.width % 2, 0, "{source:?} into {out_w} gave an odd width");
                assert_eq!(placed.height % 2, 0, "{source:?} into {out_w} gave an odd height");
                assert!(placed.width >= 2 && placed.height >= 2, "a panel shrank to nothing");
            }
        }
    }
}
