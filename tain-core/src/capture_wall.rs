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

/// How long a pump waits before asking a source that had nothing again.
///
/// Short enough to be invisible against a sixteen-millisecond frame budget, and
/// long enough that asking is not a spin — see the pump's own note. A source
/// that blocks on its own pays this only on top of a wait it already did.
const IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(2);

/// The widest or tallest a composed wall may be.
///
/// # Why the capture layer knows about encoders
///
/// It should not have to, and the alternative is worse. A wall is the one
/// capture source whose size is not a display's size: two 1920-wide screens
/// beside a 2560-wide one is a 6400-pixel picture, and hardware H.264 encoders
/// commonly stop at 4096 in either axis. Asking for more does not degrade, it
/// fails to create the encoder — which ends the session, with the error naming
/// the encoder rather than the arrangement that caused it.
///
/// Measured on the machine that found this: two screens side by side make a
/// 4480-wide wall, and selecting it killed the session.
///
/// So the wall shrinks itself to fit. That costs sharpness in a mode whose
/// whole purpose is seeing everything at once, which is the right thing to
/// give up — and it is capped rather than refused because a wall that is
/// slightly soft is worth vastly more than one that does not appear.
///
/// H.264's limit rather than the current codec's: the codec can change
/// mid-session, and a wall that only fits while H.265 is selected would fail
/// the moment somebody switched back.
const MAX_WALL: u16 = 4096;

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
    /// Where this panel sits and how big its source is, in wall pixels.
    ///
    /// # Why this can change without the wall being rebuilt
    ///
    /// A display's size is a fact for the life of the wall — a mode change
    /// rearranges the desk and rebuilds it. A *window's* is not: somebody drags
    /// its edge and it is a different shape, while the capture behind it
    /// carries on perfectly well, because the compositor follows the window and
    /// does not care what size it is.
    ///
    /// Left fixed, the panel keeps the slot it was given at construction and
    /// the frame arriving for it is squashed into the old shape. Switching away
    /// and back corrected it, which is what a stale layout looks like from the
    /// outside. See [`Wall::relayout`].
    ///
    /// Never negative: the layout is shifted so the top-left of the bounding
    /// box is the origin.
    at: Mutex<Placement>,
    /// What the display is called, for a log line that has to name which one.
    name: String,
    /// The latest frame this display produced, if it has produced one.
    frame: Mutex<Option<ScreenFrame>>,
    /// That frame, resampled to the slot it has to fill, and which slot that
    /// was.
    ///
    /// Only ever populated for a panel that is not producing its slot's size —
    /// see [`Wall::compose`], which explains why that happens and why doing the
    /// resample once instead of once a frame is the difference between a
    /// composite that runs and one that does not.
    ///
    /// Cleared by the pump when a newer frame arrives, which is what keeps it
    /// honest: it is a property of the frame beside it, not of the panel.
    fitted: Mutex<Option<Fitted>>,
}

/// A panel's frame resampled to a slot, and which slot that was.
type Fitted = ((u16, u16), Vec<u8>);

/// Where a panel sits on the wall and how big the thing behind it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Placement {
    x: u32,
    y: u32,
    /// The source's own size, for working out its share of the wall.
    source: (u16, u16),
}

impl Panel {
    fn placement(&self) -> Placement {
        *self.at.lock().unwrap_or_else(|e| e.into_inner())
    }
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
    /// Multiples the emitted size must land on, from the encoder.
    granularity: (u16, u16),
    /// Reused between frames; this is several megabytes.
    buffer: Vec<u8>,
    /// Displays already complained about for not taking their slot size.
    ///
    /// Once each: it happens on every frame for as long as the other session
    /// lasts, and a log line per frame is how a diagnosis gets buried.
    resized: std::collections::BTreeSet<String>,
    /// What this wall is made of, and what would make it out of date.
    composed: Composed,
}

/// What a wall is composed from, and what "out of date" means for it.
///
/// The composition itself does not care — it copies rectangles into a buffer —
/// but the two sources go stale for entirely different reasons, and neither can
/// fix itself in place: the panels are attached to particular displays or
/// particular windows. So each says when it should be rebuilt, and the session
/// rebuilds it. See [`Wall::stale`].
enum Composed {
    /// Displays, laid out where the host has them.
    ///
    /// A wall is a picture of where the screens *are*, and that is a fact about
    /// the machine at the moment it was composed. Someone dragging a monitor to
    /// the other side of the primary in Display Settings, or plugging one in,
    /// changes it — and this wall would go on drawing the old layout, because
    /// its panels, their offsets and its bounding box were all decided at
    /// construction.
    ///
    /// Worse than looking wrong: the pointer maps guest coordinates through the
    /// desk's bounding box, so a stale layout puts the pointer somewhere the
    /// user did not click.
    Desk {
        /// Name, position and size of every display, as it was.
        layout: Vec<(String, i32, i32, u16, u16)>,
    },
    /// Windows, tiled.
    ///
    /// Moving a window changes nothing here — the grid is not where they are —
    /// which is the point: a guest watching a composite should not have the
    /// picture rearrange itself every time somebody drags something. What does
    /// change it is a window going away or coming back.
    Windows {
        /// Every window the session asked for, including any that could not be
        /// captured at the moment this was built.
        wanted: Vec<u64>,
        /// The ones that actually got a panel, in grid order.
        present: Vec<u64>,
    },
}

/// The arrangement, as a value that can be compared.
///
/// Name, position and size of every display, in the order they enumerate. Not
/// a hash: the comparison happens a few times a second at most, and a value
/// that can be printed is worth more here than one that is small.
pub fn layout_now() -> Vec<(String, i32, i32, u16, u16)> {
    super::screen_capture::displays()
        .into_iter()
        .map(|screen| (screen.name, screen.x, screen.y, screen.width, screen.height))
        .collect()
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
                name: screen.name.clone(),
                capturer: Mutex::new(capturer),
                at: Mutex::new(Placement {
                    x: (screen.x - left).max(0) as u32,
                    y: (screen.y - top).max(0) as u32,
                    source: (screen.width, screen.height),
                }),
                frame: Mutex::new(None),
                fitted: Mutex::new(None),
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
            granularity: (2, 2),
            buffer: Vec::new(),
            resized: std::collections::BTreeSet::new(),
            composed: Composed::Desk { layout: layout_now() },
        };
        // Before the first frame, not on the first resize: a display whose slot
        // is smaller than itself has to be told so, and at native scale a wall
        // of two differently sized screens already has one.
        wall.resize_panels();
        Ok(wall)
    }

    /// Compose these windows, tiled, capturing each one.
    ///
    /// # Why a grid and not where the windows are
    ///
    /// Screens are composed where the host has them because that is a fact:
    /// they sit beside each other on a desk, and reproducing the arrangement
    /// reproduces the desk. Windows are not like that. They overlap, in a stack
    /// whose order changes as somebody works — so laying them out by position
    /// would hide whichever is underneath, and two maximised windows would
    /// compose into a picture of one window. Seeing all of them is the entire
    /// reason for asking for this view.
    ///
    /// The cost is that the pointer can no longer map through one rectangle,
    /// which is what [`Self::input_tiles`] is for.
    ///
    /// # Why the cells are the size of the largest window
    ///
    /// So nothing is enlarged. A small window in a big cell sits in the middle
    /// of it with space around; the alternative — a cell per window sized to
    /// that window — makes a grid with ragged rows, and the alternative to
    /// *that* is upscaling a 400-pixel utility window to fill a cell beside a
    /// 4K editor, which spends the picture's resolution on the thing that
    /// needed it least.
    ///
    /// Fails only if not one window could be captured. One out of three failing
    /// leaves a gap, which is better than no picture — and the log says which.
    pub fn of_windows(windows: &[super::protocol::WindowInfo]) -> Result<Self> {
        anyhow::ensure!(!windows.is_empty(), "a composite of no windows is not a picture");

        let sizes: Vec<(u16, u16)> =
            windows.iter().map(|w| (w.width.max(1), w.height.max(1))).collect();
        let (bounds, places) = grid(&sizes);

        let stopping = Arc::new(AtomicBool::new(false));
        let fresh = Arc::new((Mutex::new(false), Condvar::new()));
        let (mut panels, mut pumps, mut present) = (Vec::new(), Vec::new(), Vec::new());

        // # Built at the same time, because each one is slow and none of them
        // wait for each other
        //
        // Measured on the machine that reported this: about 200 ms per window —
        // roughly a third of it creating a graphics device and the rest setting
        // up the frame pool and capture session — done one after another. Eight
        // windows took **3.7 seconds**, which is what "switching back to the
        // composite takes a while" was.
        //
        // Nothing about them is shared, so the only reason it was serial is
        // that it was written as a loop. Done at once it is the slowest single
        // window plus the cost of starting some threads.
        let built: Vec<_> = std::thread::scope(|scope| {
            let starting: Vec<_> = windows
                .iter()
                .map(|window| {
                    scope.spawn(move || {
                        (window, super::capture_wgc::WgcCapturer::for_window(window.id))
                    })
                })
                .collect();
            // In the order they were asked for, not the order they finished:
            // the grid is positional, and a composite whose cells shuffled
            // according to which window's driver answered first would be a
            // different picture every time it was opened.
            starting.into_iter().filter_map(|one| one.join().ok()).collect()
        });

        for (index, (window, capturer)) in built.into_iter().enumerate() {
            let capturer = match capturer {
                Ok(capturer) => capturer,
                Err(e) => {
                    // A gap rather than a failure. A window that is minimised
                    // cannot be captured and is entitled to come back, which is
                    // what `stale` watches for.
                    warn!("Leaving {} out of the composite: {e:#}", window.title);
                    continue;
                }
            };

            // By position in the request, not in the built list: a window that
            // refused leaves its cell empty rather than shuffling every window
            // after it one place along. The gap lasts until `stale` notices and
            // the composite is rebuilt without it, which is the next tick.
            let panel = Arc::new(Panel {
                name: window.title.clone(),
                capturer: Mutex::new(Box::new(capturer)),
                at: Mutex::new(places[index]),
                frame: Mutex::new(None),
                fitted: Mutex::new(None),
            });
            panels.push(Arc::clone(&panel));
            present.push(window.id);
            pumps.push(spawn_pump(
                format!("window-{}", window.id),
                panel,
                Arc::clone(&stopping),
                Arc::clone(&fresh),
            )?);
        }

        anyhow::ensure!(!panels.is_empty(), "none of the shared windows could be captured");

        let out = QualityScale::Native.apply((
            bounds.0.min(u16::MAX as u32) as u16,
            bounds.1.min(u16::MAX as u32) as u16,
        ));
        info!(
            "Composing {} of {} window(s) into a {}x{} grid",
            panels.len(),
            windows.len(),
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
            granularity: (2, 2),
            buffer: Vec::new(),
            resized: std::collections::BTreeSet::new(),
            composed: Composed::Windows {
                wanted: windows.iter().map(|w| w.id).collect(),
                present,
            },
        };
        wall.resize_panels();
        Ok(wall)
    }

    /// Re-place the windows for the sizes they are now.
    ///
    /// # Why this is not a rebuild
    ///
    /// A window being resized changes nothing about the capture behind it — the
    /// compositor follows the window and does not care what shape it is — so
    /// tearing down N capture sessions and building N more would be a second of
    /// black picture to fix some arithmetic. What is stale is only the
    /// arithmetic: the panel's slot, the cell size derived from the largest
    /// window, and the bounding box derived from that.
    ///
    /// The symptom without it is specific and was reported as such: an
    /// odd-shaped window keeps the slot it had when the composite was built, so
    /// a frame of its new shape is squashed into the old one. Switching to that
    /// window and back rebuilt the wall and corrected it, which is exactly what
    /// a layout that only refreshes on rebuild looks like from outside.
    ///
    /// Returns whether anything moved, so the caller only pays for the resize
    /// when there was one.
    fn relayout(&mut self) -> bool {
        let Composed::Windows { present, .. } = &self.composed else {
            return false;
        };

        // What each window is now. A window that has gone missing keeps the
        // size it had: `stale` is what deals with those, and guessing here
        // would shrink the grid for a moment before rebuilding it anyway.
        let sizes: Vec<(u16, u16)> = self
            .panels
            .iter()
            .zip(present)
            .map(|(panel, id)| {
                super::window_capture::on_screen(*id)
                    .map(|at| (at.width.max(1), at.height.max(1)))
                    .unwrap_or(panel.placement().source)
            })
            .collect();

        // The same arithmetic the composite was built with, not a second copy
        // of it: two implementations of a layout would eventually disagree, and
        // the symptom would be the picture jumping the first time anybody
        // resized a window.
        let (bounds, places) = grid(&sizes);

        let mut moved = self.bounds != bounds;
        for (panel, want) in self.panels.iter().zip(places) {
            let mut at = panel.at.lock().unwrap_or_else(|e| e.into_inner());
            if *at != want {
                *at = want;
                moved = true;
            }
        }

        if !moved {
            return false;
        }
        self.bounds = bounds;
        debug!("A shared window changed shape — the composite is now {bounds:?}");
        true
    }

    /// Where each panel sits in the composed picture, and where it came from on
    /// this machine.
    ///
    /// `None` for a wall of screens: those already tile the desktop, so the
    /// pointer maps through one rectangle and needs nothing per panel.
    fn tiles(&self) -> Option<Vec<super::input_inject::Tile>> {
        let Composed::Windows { present, .. } = &self.composed else {
            return None;
        };

        let mut tiles = Vec::new();
        for (panel, id) in self.panels.iter().zip(present) {
            // Asked now rather than remembered: the grid does not move when a
            // window does, but where the click has to *land* certainly does.
            let Some(at) = super::window_capture::on_screen(*id) else {
                continue;
            };
            let placed = place_panel(panel, self.bounds, self.out);
            tiles.push(super::input_inject::Tile {
                picture: (placed.x, placed.y, placed.width, placed.height),
                desktop: at,
            });
        }
        (!tiles.is_empty()).then_some(tiles)
    }

    /// The size this should emit, from the wall's own size and what has been
    /// asked of it.
    ///
    /// [`MAX_WALL`] is applied last and applies whatever else was asked, 1:1
    /// included: a size the encoder will refuse is not a size, and lifting the
    /// guest's ceiling cannot lift a hardware one.
    fn target(&self) -> (u16, u16) {
        wall_target(self.bounds, self.scale, self.ceiling, self.granularity)
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
        let placed = place_panel(panel, self.bounds, self.out);
        (placed.width, placed.height)
    }

    /// Whether the desk has been rearranged since this wall was built.
    ///
    /// Cheap enough to ask per frame — it is an enumeration of display devices,
    /// which is what `capture_next` is already waiting tens of milliseconds for
    /// — but asked on a timer anyway, because "cheap" and "sixty times a
    /// second" are a bad combination when the answer changes twice a year.
    pub fn stale(&self) -> bool {
        match &self.composed {
            Composed::Desk { layout } => layout_now() != *layout,
            // # Which windows *could* be shown, against which are
            //
            // Not "has anything changed about them": moving or resizing a
            // window is not a reason to tear a composite down, and doing it
            // would rebuild the whole thing on every frame of a drag.
            //
            // Comparing against `wanted` rather than only `present` is what
            // lets a window come *back*. A minimised one cannot be captured, so
            // it is not a panel — and without asking after it, restoring it
            // would leave a gap in the picture for the rest of the session.
            //
            // Self-limiting in the other direction too: a window that closed
            // drops out of the answer, the wall is rebuilt without it once, and
            // the two agree again. Comparing against the original request alone
            // would make it permanently stale and rebuild every frame.
            Composed::Windows { wanted, present } => {
                let showing: Vec<u64> = wanted
                    .iter()
                    .copied()
                    .filter(|id| super::window_capture::state_of(*id).is_live())
                    .collect();
                showing != *present
            }
        }
    }

    /// Say once that a display is not producing the size it was asked for.
    fn warn_once_about_resize(&mut self, name: &str, got: (u16, u16), slot: (u16, u16)) {
        if self.resized.insert(name.to_string()) {
            warn!(
                "{name} is producing {}x{} for a {}x{} slot — scaling it to fit. Its \
                 capture is probably owned by another session.",
                got.0, got.1, slot.0, slot.1
            );
        }
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

        let mut mismatched = Vec::new();
        for panel in &self.panels {
            let frame = panel.frame.lock().unwrap_or_else(|e| e.into_inner());
            let Some(frame) = frame.as_ref() else { continue };
            if frame.format != PixelFormat::Bgra {
                continue;
            }

            // Where this panel starts on the wall, scaled from its position on
            // the desk. Computed per frame because the emitted size changes
            // when the guest resizes its window.
            let placed = place_panel(panel, self.bounds, self.out);
            let (x, y) = (usize::from(placed.x), usize::from(placed.y));
            if x >= usize::from(out_w) || y >= usize::from(out_h) {
                continue;
            }

            // # A panel that is not the size of its slot has to be made so
            //
            // This copied at 1:1 and clipped, which is right exactly when every
            // panel emits the size it was asked for — and `resize_panels` asks.
            // It is not always answered. A display's capture is *shared*, and
            // only one subscriber owns its configuration: with another session
            // already watching that screen directly, the wall's request to
            // resize it is refused, and the panel keeps producing whatever the
            // other session wanted.
            //
            // The result was a wall where one screen occupied the wrong area of
            // the picture — drawn small with black beside it, or clipped — and
            // that is not merely ugly. The pointer maps guest coordinates
            // through the wall to the desk on the assumption that the wall is a
            // *uniform* scale of the desktop's bounding box. One panel at a
            // different scale breaks that assumption for itself and for
            // everything to its right, which is the mismatched mouse position
            // that was reported.
            //
            // So the slot is authoritative and the frame is fitted to it. The
            // equal case is still a memcpy, which is what it is in the ordinary
            // run where every panel did as it was asked.
            //
            // # And it is fitted once, not once a frame
            //
            // A panel that is the wrong size is usually the wrong size for a
            // while: it is showing a still window whose last frame was captured
            // before the slot was worked out, and nothing new will arrive until
            // that window changes. Resampling it on every composition meant
            // paying for a picture that had not moved — measured at 3.2 seconds
            // per composed frame across seven windows, which is a composite
            // running at a third of a frame per second while the GPUs sit idle.
            //
            // So the fitted copy is kept beside the frame it came from, and the
            // pump throws it away when it stores a new one.
            let slot = (placed.width, placed.height);
            let fitted = panel.fitted.lock().unwrap_or_else(|e| e.into_inner());
            let mut fitted = fitted;
            let (pixels, src_w, src_h) = if (frame.width, frame.height) == slot {
                (&frame.data[..], frame.width, frame.height)
            } else {
                if fitted.as_ref().map(|(at, _)| *at) != Some(slot) {
                    // Noted rather than logged here: `self.panels` is borrowed
                    // by this loop, and saying so needs `&mut self`.
                    mismatched.push((panel.name.clone(), (frame.width, frame.height), slot));
                    *fitted = Some((
                        slot,
                        super::screen_capture::box_downscale(frame, slot.0, slot.1),
                    ));
                }
                let held = fitted.as_ref().map(|(_, pixels)| &pixels[..]).unwrap_or(&[]);
                (held, slot.0, slot.1)
            };

            let src_stride = usize::from(src_w) * 4;
            let columns = usize::from(src_w).min(usize::from(out_w) - x);
            let rows = usize::from(src_h).min(usize::from(out_h) - y);
            for row in 0..rows {
                let from = row * src_stride;
                let to = (y + row) * stride + x * 4;
                let (Some(src), Some(dst)) = (
                    pixels.get(from..from + columns * 4),
                    self.buffer.get_mut(to..to + columns * 4),
                ) else {
                    break;
                };
                dst.copy_from_slice(src);
            }
        }

        for (name, got, slot) in mismatched {
            self.warn_once_about_resize(&name, got, slot);
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
/// Lay these window sizes out in a grid: the bounding box, and where each sits.
///
/// # Why a grid and not where the windows are
///
/// Screens are composed where the host has them because that is a fact — they
/// sit beside each other on a desk. Windows overlap, in a stack whose order
/// changes as somebody works, so laying them out by position would hide
/// whichever is underneath and two maximised windows would compose into a
/// picture of one.
///
/// Free and taking only sizes, so the arrangement can be tested for counts and
/// shapes this machine does not happen to have open — and so the composite and
/// the relayout that follows a resize cannot drift apart.
fn grid(sizes: &[(u16, u16)]) -> ((u32, u32), Vec<Placement>) {
    // Square-ish, favouring width, because screens are wider than they are tall
    // and so is nearly every window on them.
    let columns = (sizes.len() as f64).sqrt().ceil().max(1.0) as u32;
    // The largest window, so nothing is enlarged: a small window sits in the
    // middle of its cell with space around it, where sizing each cell to its
    // own window makes ragged rows and upscaling a 400-pixel utility window
    // beside a 4K editor spends the picture on whatever needed it least.
    let cell = (
        sizes.iter().map(|s| s.0).max().unwrap_or(1).max(1),
        sizes.iter().map(|s| s.1).max().unwrap_or(1).max(1),
    );
    let rows = sizes.len().div_ceil(columns.max(1) as usize) as u32;

    let inset = |span: u16, of: u16| u32::from(of.saturating_sub(span)) / 2;
    let places = sizes
        .iter()
        .enumerate()
        .map(|(index, size)| {
            let (column, row) = (index as u32 % columns, index as u32 / columns);
            Placement {
                x: column * u32::from(cell.0) + inset(size.0, cell.0),
                y: row * u32::from(cell.1) + inset(size.1, cell.1),
                source: (size.0.max(1), size.1.max(1)),
            }
        })
        .collect();

    ((columns * u32::from(cell.0), rows * u32::from(cell.1)), places)
}

/// [`place`] for a panel, reading the placement it currently holds.
fn place_panel(panel: &Panel, bounds: (u32, u32), out: (u16, u16)) -> Placed {
    let at = panel.placement();
    place(at.source, (at.x, at.y), bounds, out)
}

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
                        // What was fitted belonged to the frame just replaced.
                        *panel.fitted.lock().unwrap_or_else(|e| e.into_inner()) = None;
                        let (lock, cv) = &*fresh;
                        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
                        cv.notify_all();
                    }
                    // # Nothing changed, which is the ordinary case
                    //
                    // Most sources are still most of the time, and this loop
                    // was written for one that *blocks* while that is true — a
                    // display subscriber waits about twenty-five milliseconds
                    // for its screen to change, so an empty answer already cost
                    // a wait and going straight round again was free.
                    //
                    // A window does not work that way. `TryGetNextFrame` is in
                    // its name: the compositor answers "not yet" immediately,
                    // so this became a spin, at whatever rate the CPU allows,
                    // holding and releasing the panel's lock the whole time —
                    // one core per shared window, plus the contention with the
                    // side trying to compose.
                    //
                    // Measured as a composite running at six to ten frames a
                    // second on a machine whose GPUs were idle, and reported as
                    // "slow capture" with nothing on either end struggling.
                    // Both were true: nothing was struggling, and the capture
                    // thread could not get scheduled.
                    Ok(None) => std::thread::sleep(IDLE_POLL),
                    Err(e) => {
                        warn!("{name} stopped contributing to the wall: {e:#}");
                        return;
                    }
                }
            }
        })
        .with_context(|| format!("could not start {thread_name}"))
}

/// What a wall of `bounds` emits, given everything asked of it.
///
/// Free rather than a method so the test can reach it. It used to be inline in
/// [`Wall::target`] with the test carrying its own copy of the arithmetic —
/// which is a test that agrees with a comment rather than with the code, and it
/// went on agreeing after the encoder's size granularity was added to one of
/// them and not the other.
fn wall_target(
    bounds: (u32, u32),
    scale: QualityScale,
    ceiling: Option<(u16, u16)>,
    granularity: (u16, u16),
) -> (u16, u16) {
    let native = (bounds.0.min(u16::MAX as u32) as u16, bounds.1.min(u16::MAX as u32) as u16);
    // `MAX_WALL` before the guest's ceiling and whatever else was asked, 1:1
    // included: lifting the guest's ceiling cannot lift a hardware one.
    let scaled = fit_within(scale.apply(native), (MAX_WALL, MAX_WALL));
    let fitted = match ceiling {
        Some(ceiling) => {
            let (w, h) = fit_within(scaled, ceiling);
            // Even, as every codec here needs.
            ((w.max(2)) & !1, (h.max(2)) & !1)
        }
        None => scaled,
    };
    // Last, and for the same reason `MAX_WALL` is applied at all: a size the
    // encoder will refuse is not a size.
    super::screen_capture::snap_to(fitted, granularity)
}

impl ScreenCaptureBackend for Wall {
    fn capture_next(&mut self) -> Result<Option<ScreenFrame>> {
        // A window that has been resized changes the grid it sits in — see
        // `relayout`. Asked before the target, because the target is derived
        // from the bounding box this may have just moved.
        let moved = self.relayout();

        let target = self.target();
        if moved || target != self.out {
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

    /// What the *next* frame will be, not what the last one was.
    ///
    /// `out` only moves when a frame is composed, so this used to answer with
    /// the size the wall was built at until one had been. The session log said
    /// "now streaming every screen (4800x1620)" — a size no encoder was ever
    /// offered, since neither the wall's own limit nor the guest's ceiling nor
    /// the encoder's grid had been applied yet — and reading that as the size
    /// being sent cost an evening.
    fn frame_size(&self) -> (u16, u16) {
        self.target()
    }

    fn set_scale(&mut self, scale: QualityScale) -> bool {
        self.scale = scale;
        true
    }

    fn set_size_ceiling(&mut self, ceiling: Option<(u16, u16)>) -> bool {
        self.ceiling = ceiling;
        true
    }

    fn set_size_granularity(&mut self, granularity: (u16, u16)) -> bool {
        self.granularity = granularity;
        true
    }

    fn layout_changed(&self) -> bool {
        self.stale()
    }

    fn input_tiles(&self) -> Option<Vec<super::input_inject::Tile>> {
        self.tiles()
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

    /// Windows are tiled without overlapping and without being enlarged.
    ///
    /// # The two things that would ruin this view
    ///
    /// Overlap, because the entire reason for asking for it is seeing every
    /// window at once — a composite where one covers another is a worse picture
    /// of the same thing you get by sharing one window. And enlargement,
    /// because a 400-pixel utility window blown up to fill a cell beside a 4K
    /// editor spends the picture's resolution on the thing that needed it
    /// least.
    #[test]
    fn a_composite_tiles_its_windows_without_overlap_or_enlargement() {
        // Deliberately mismatched: a wide one, a tall one, a small one, and a
        // count that does not fill its last row.
        let windows = [(1920u16, 1080u16), (800, 1400), (400, 300), (2560, 1440), (640, 480)];
        let (bounds, places) = grid(&windows);

        // Three across and two down, of cells the size of the largest window.
        assert_eq!(
            bounds,
            (2560 * 3, 1440 * 2),
            "five windows want a three-by-two grid of the largest window's size"
        );
        let out = (bounds.0 as u16, bounds.1 as u16);

        // Every window inside the picture, and no two of them touching.
        let placed: Vec<Placed> = places
            .iter()
            .map(|at| place(at.source, (at.x, at.y), bounds, out))
            .collect();
        for (index, one) in placed.iter().enumerate() {
            assert!(
                u32::from(one.x) + u32::from(one.width) <= bounds.0
                    && u32::from(one.y) + u32::from(one.height) <= bounds.1,
                "window {index} at {one:?} runs off the picture"
            );
            for (other_index, other) in placed.iter().enumerate().skip(index + 1) {
                let apart = u32::from(one.x) + u32::from(one.width) <= u32::from(other.x)
                    || u32::from(other.x) + u32::from(other.width) <= u32::from(one.x)
                    || u32::from(one.y) + u32::from(one.height) <= u32::from(other.y)
                    || u32::from(other.y) + u32::from(other.height) <= u32::from(one.y);
                assert!(apart, "windows {index} and {other_index} overlap: {one:?} and {other:?}");
            }
        }

        // And each keeps its own size, since the picture is the grid at native
        // scale. Within a pixel, because panels are forced to even dimensions.
        for (window, one) in windows.iter().zip(&placed) {
            assert!(
                one.width.abs_diff(window.0) <= 1 && one.height.abs_diff(window.1) <= 1,
                "a {}x{} window was drawn {}x{}",
                window.0,
                window.1,
                one.width,
                one.height
            );
        }
    }

    /// What a live composite costs to run, per frame and in CPU.
    ///
    /// Reported rather than asserted. It exists because a composite was
    /// measured at six to ten frames a second with both machines' GPUs idle,
    /// and the numbers that answer "why" are the composition cost and how much
    /// of a core the pumps burn while nothing is changing.
    #[test]
    #[ignore = "builds a real capture of every window on this machine — run explicitly"]
    fn report_what_a_live_composite_costs() {
        let windows = crate::window_capture::shareable();
        if windows.len() < 2 {
            println!("need at least two windows open to compose");
            return;
        }
        let Ok(mut wall) = Wall::of_windows(&windows) else {
            println!("could not compose them");
            return;
        };
        // What a guest's window would ask for, so the panels are shrunk the way
        // they are in a real session.
        wall.set_size_ceiling(Some((1600, 900)));

        // The first composition pays for whatever the panels captured before
        // they were told their size, so it is not the steady state and is not
        // what a session spends its life doing.
        let warmup = std::time::Instant::now();
        while warmup.elapsed() < std::time::Duration::from_secs(2) {
            let _ = wall.capture_next();
        }

        let started = std::time::Instant::now();
        let (mut frames, mut cost) = (0u32, std::time::Duration::ZERO);
        while started.elapsed() < std::time::Duration::from_secs(5) {
            if let Ok(Some(frame)) = wall.capture_next() {
                frames += 1;
                cost += frame.cost;
            }
        }
        let seconds = started.elapsed().as_secs_f64();
        println!(
            "{} window(s) at {}x{}: {:.1} composed/s, {:.1} ms each",
            windows.len(),
            wall.frame_size().0,
            wall.frame_size().1,
            f64::from(frames) / seconds,
            cost.as_secs_f64() * 1000.0 / f64::from(frames.max(1)),
        );
    }

    /// How long a composite takes to build, on this machine, right now.
    ///
    /// Reported rather than asserted: it depends on what is open and on the
    /// graphics driver. It exists because "switching back to the composite
    /// takes a while" is a complaint with no number in it, and the number is
    /// what says whether the cost is the capture sessions or something else.
    #[test]
    #[ignore = "builds a real capture of every window on this machine — run explicitly"]
    fn report_how_long_a_composite_takes_to_build() {
        let windows = crate::window_capture::shareable();
        if windows.is_empty() {
            println!("nothing open to compose");
            return;
        }

        // One at a time first, so the per-window cost is visible against the
        // total rather than inferred from it.
        for window in &windows {
            let started = std::time::Instant::now();
            let one = crate::capture_wgc::WgcCapturer::for_window(window.id);
            println!(
                "  {:>7.1} ms  {} — {}",
                started.elapsed().as_secs_f64() * 1000.0,
                window.app,
                if one.is_ok() { "ok" } else { "refused" }
            );
        }

        let started = std::time::Instant::now();
        let wall = Wall::of_windows(&windows);
        println!(
            "{:.1} ms to compose {} window(s) — {}",
            started.elapsed().as_secs_f64() * 1000.0,
            windows.len(),
            if wall.is_ok() { "ok" } else { "failed" }
        );
    }

    /// One window is still one window, not a grid of one.
    #[test]
    fn a_composite_of_one_window_is_that_window() {
        let (bounds, places) = grid(&[(1280, 720)]);
        assert_eq!(bounds, (1280, 720), "one window is the whole picture");
        assert_eq!(
            places,
            vec![Placement { x: 0, y: 0, source: (1280, 720) }],
            "with one cell there is nothing to inset it by"
        );
    }

    /// A window resized while the composite is showing re-places everything.
    ///
    /// # The bug this is for
    ///
    /// A panel kept the slot it was given when the composite was built, so a
    /// window whose shape changed had its frames squashed into the old one.
    /// Switching to that window and back corrected it, which was the report:
    /// "doesn't respect aspect ratio when it's changed live". The layout has to
    /// follow the windows without the capture being rebuilt.
    #[test]
    fn resizing_a_window_re_places_the_grid_around_it() {
        // Two windows, the second much smaller and centred in its cell.
        let before = grid(&[(1000, 1000), (500, 500)]);
        assert_eq!(before.0, (2000, 1000), "cells are the larger window");
        assert_eq!(before.1[1], Placement { x: 1250, y: 250, source: (500, 500) });

        // Now the small one is dragged wider than the big one. The cell grows,
        // the bounding box grows with it, and *both* windows move — which is
        // why this cannot be a per-panel fix-up.
        let after = grid(&[(1000, 1000), (3000, 500)]);
        assert_eq!(after.0, (6000, 1000), "the cell follows the largest window");
        assert_eq!(
            after.1[0],
            Placement { x: 1000, y: 0, source: (1000, 1000) },
            "the window that did not change still moved, because its cell did"
        );
        assert_eq!(
            after.1[1],
            Placement { x: 3000, y: 250, source: (3000, 500) },
            "the resized window keeps its own shape rather than the old cell's"
        );
    }

    fn screen(id: u32, x: i32, y: i32, w: u16, h: u16) -> DisplayInfo {
        DisplayInfo {
            id,
            name: format!(r"\\.\DISPLAY{}", id + 1),
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

    /// A wall never asks the encoder for more than it can take.
    ///
    /// # The session this killed
    ///
    /// Two screens side by side — 2560 and 1920 — compose to a 4480-pixel-wide
    /// wall. Hardware H.264 encoders commonly stop at 4096 in either axis, and
    /// asking for more does not degrade: the encoder fails to build and the
    /// session ends, with the error naming the encoder rather than the
    /// arrangement that caused it.
    ///
    /// Checked through `target` rather than by inspecting the constant, because
    /// the cap has to survive both a scale and a ceiling being applied — and
    /// 1:1, which lifts the ceiling entirely and is exactly when an oversized
    /// wall would otherwise get through.
    #[test]
    fn a_wall_is_never_wider_than_an_encoder_will_take() {
        // The real arithmetic; a real Wall needs displays to capture.
        let target = |bounds: (u32, u32), scale: QualityScale, ceiling: Option<(u16, u16)>| {
            wall_target(bounds, scale, ceiling, (2, 2))
        };

        // Two side by side, at 1:1 — no ceiling, which is the case that used to
        // reach the encoder unshrunk.
        let (w, h) = target((4480, 1600), QualityScale::Native, None);
        assert!(w <= MAX_WALL && h <= MAX_WALL, "a 4480-wide wall came back {w}x{h}");
        // Still the right shape: shrinking must not stretch the desk.
        let ratio = f64::from(w) / f64::from(h);
        assert!((ratio - 4480.0 / 1600.0).abs() < 0.02, "the wall changed shape: {w}x{h}");

        // Three 4K screens, which is well past it in both directions.
        let (w, h) = target((11520, 2160), QualityScale::Native, None);
        assert!(w <= MAX_WALL && h <= MAX_WALL, "a 11520-wide wall came back {w}x{h}");

        // Stacked, so the tall axis is the one over the limit.
        let (w, h) = target((2560, 6400), QualityScale::Native, None);
        assert!(w <= MAX_WALL && h <= MAX_WALL, "a 6400-tall wall came back {w}x{h}");

        // A wall that already fits is left alone.
        let (w, h) = target((3840, 1080), QualityScale::Native, None);
        assert_eq!((w, h), (3840, 1080), "a wall within the limit was shrunk anyway");
    }

    /// Every panel fills its slot exactly, whatever size it arrived at.
    ///
    /// # The bug this is for
    ///
    /// Composition copied each panel at 1:1 and clipped, which is right only
    /// while every panel emits the size it was asked for. A display's capture
    /// is shared, and only one subscriber owns its configuration — so with
    /// another session already watching that screen directly, the wall's resize
    /// request is refused and the panel keeps producing the other session's
    /// size.
    ///
    /// The picture then had one screen drawn small with black beside it. That
    /// is not merely ugly: the pointer maps guest coordinates through the wall
    /// on the assumption that it is a *uniform* scale of the desk, so one panel
    /// at a different scale moves the pointer for itself and for everything to
    /// its right.
    ///
    /// Checked as geometry rather than by composing: `place` is what decides
    /// where a panel goes, and the property is that the slots tile the wall in
    /// the same proportions the displays tile the desk.
    #[test]
    fn every_panel_keeps_its_share_of_the_wall() {
        // A 2560x1600 beside a 1920x1080, the arrangement that found this.
        let desk = (4480u32, 1600u32);
        let out = (2240u16, 800u16);
        let screens = [((2560u16, 1600u16), (0u32, 0u32)), ((1920, 1080), (2560, 0))];

        for (source, at) in screens {
            let placed = place(source, at, desk, out);

            // Where it lands, as a fraction of the wall, must be where the
            // display sits as a fraction of the desk — within the rounding that
            // even-sized panels cost.
            let want_x = f64::from(at.0) / f64::from(desk.0) * f64::from(out.0);
            let want_w = f64::from(source.0) / f64::from(desk.0) * f64::from(out.0);
            let want_h = f64::from(source.1) / f64::from(desk.1) * f64::from(out.1);

            assert!(
                (f64::from(placed.x) - want_x).abs() <= 2.0,
                "{source:?} at {at:?} landed at x={} not {want_x:.0}",
                placed.x
            );
            assert!(
                (f64::from(placed.width) - want_w).abs() <= 2.0,
                "{source:?} got width {} not {want_w:.0}",
                placed.width
            );
            assert!(
                (f64::from(placed.height) - want_h).abs() <= 2.0,
                "{source:?} got height {} not {want_h:.0}",
                placed.height
            );
        }
    }

    /// The encoder's grid reaches the wall too.
    ///
    /// It did not, and the wall was the one place a session could still be
    /// handed a size its AV1 encoder refuses — the arrangement in the report:
    /// a 2560x1600 beside a 3840x2160, composed, capped to 4096 wide, and
    /// landing on a height of 1382, which stalls the transform two seconds in.
    #[test]
    fn a_wall_lands_on_the_grid_the_encoder_asked_for() {
        let desk = (6400u32, 2160u32);
        for scale in QualityScale::ALL {
            for ceiling in [None, Some((1536u16, 874u16)), Some((2560, 1440))] {
                let (w, h) = wall_target(desk, scale, ceiling, (64, 16));
                assert!(w.is_multiple_of(64), "{scale:?}/{ceiling:?} gave {w}x{h}");
                assert!(h.is_multiple_of(16), "{scale:?}/{ceiling:?} gave {w}x{h}");
                assert!(w <= MAX_WALL && h <= MAX_WALL, "{scale:?}/{ceiling:?} gave {w}x{h}");
                if let Some((cw, ch)) = ceiling {
                    assert!(w <= cw && h <= ch, "{scale:?}/{ceiling:?} gave {w}x{h}");
                }
            }
        }

        // And the ordinary granularity still changes nothing.
        assert_eq!(
            wall_target(desk, QualityScale::Native, None, (2, 2)),
            (4096, 1382),
            "asking for nothing must leave the wall as it was"
        );
    }
}
