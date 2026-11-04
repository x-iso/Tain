//! One window, captured through KWin rather than the portal. See [`crate::kwin`]
//! for why, and for when this is available at all.
//!
//! KWin streams the window itself, not the part of the screen it covers: one
//! behind another, or partly off the edge of the desk, comes out whole, as it
//! does through Windows.Graphics.Capture. The stream is the window's frame,
//! decorations included, which is the same rectangle `kwin::Window::geometry`
//! gives — so a click mapped through that rectangle lands where it was aimed.

#![cfg(all(target_os = "linux", db_pipewire))]

use anyhow::{Context, Result};
use tracing::*;

use crate::capture_portal::PwStream;
use crate::kwin::{self, WindowStream};
use crate::screen_capture::{ScreenCaptureBackend, ScreenFrame};
use crate::window_capture::Thumbnail;

/// A window's stream, as frames.
pub struct KwinWindowCapturer {
    /// First, so it closes before KWin is told the stream is finished with.
    frames: PwStream,
    _stream: WindowStream,
}

impl KwinWindowCapturer {
    pub fn for_window(id: u64, fps: u32) -> Result<Self> {
        let kwin = kwin::get().context("KWin is not sharing windows with Tain")?;
        let stream = kwin.stream_window(id)?;
        let frames = PwStream::open(None, stream.node, fps, "shared window")?;
        let (width, height) = frames.size();
        info!(
            "Capturing {} at {width}x{height} through KWin's PipeWire node {}",
            kwin.window(id).map(|w| format!("\"{}\"", w.title)).unwrap_or_else(|| "a window".into()),
            stream.node
        );
        Ok(Self { frames, _stream: stream })
    }
}

impl ScreenCaptureBackend for KwinWindowCapturer {
    fn capture_next(&mut self) -> Result<Option<ScreenFrame>> {
        self.frames.next()
    }

    fn frame_size(&self) -> (u16, u16) {
        self.frames.size()
    }
}

/// A picture of a window for the picker, no more than `max_edge` on its longer
/// side.
///
/// A stream started and stopped for one frame. KWin draws a window into a new
/// stream at once, so this waits for that and no longer.
pub fn thumbnail(id: u64, max_edge: u16) -> Option<Thumbnail> {
    /// How long to wait for the first frame.
    const PATIENCE: std::time::Duration = std::time::Duration::from_millis(1500);

    let mut capturer = match KwinWindowCapturer::for_window(id, 30) {
        Ok(capturer) => capturer,
        Err(e) => {
            debug!("No picture of window {id}: {e:#}");
            return None;
        }
    };
    let deadline = std::time::Instant::now() + PATIENCE;
    while std::time::Instant::now() < deadline {
        match capturer.capture_next() {
            Ok(Some(frame)) => return Some(shrink(&frame, max_edge)),
            Ok(None) => {}
            Err(e) => {
                debug!("No picture of window {id}: {e:#}");
                return None;
            }
        }
    }
    None
}

/// A BGRA frame as straight RGBA, averaged down to fit `max_edge`.
fn shrink(frame: &ScreenFrame, max_edge: u16) -> Thumbnail {
    let (w, h) = (usize::from(frame.width.max(1)), usize::from(frame.height.max(1)));
    let scale = (f64::from(max_edge.max(1)) / w.max(h) as f64).min(1.0);
    let (tw, th) = (((w as f64 * scale).round() as usize).max(1), ((h as f64 * scale).round() as usize).max(1));
    let mut rgba = Vec::with_capacity(tw * th * 4);
    for ty in 0..th {
        let (y0, y1) = (ty * h / th, ((ty + 1) * h / th).max(ty * h / th + 1));
        for tx in 0..tw {
            let (x0, x1) = (tx * w / tw, ((tx + 1) * w / tw).max(tx * w / tw + 1));
            let mut sum = [0u32; 3];
            for y in y0..y1.min(h) {
                for x in x0..x1.min(w) {
                    let at = (y * w + x) * 4;
                    for (channel, total) in sum.iter_mut().enumerate() {
                        *total += u32::from(frame.data[at + channel]);
                    }
                }
            }
            let count = ((y1.min(h) - y0) * (x1.min(w) - x0)).max(1) as u32;
            // BGRA in, RGBA out, and opaque: a window is.
            rgba.extend_from_slice(&[
                (sum[2] / count) as u8,
                (sum[1] / count) as u8,
                (sum[0] / count) as u8,
                255,
            ]);
        }
    }
    Thumbnail { width: tw as u16, height: th as u16, rgba }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Saves a frame where a person can look at it, as a PNG.
    fn save(frame: &ScreenFrame, path: &std::path::Path) {
        let rgba: Vec<u8> =
            frame.data.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], 255]).collect();
        image::RgbaImage::from_raw(u32::from(frame.width), u32::from(frame.height), rgba)
            .expect("a whole frame")
            .save(path)
            .expect("saved");
    }

    fn first_frame(capturer: &mut dyn ScreenCaptureBackend) -> ScreenFrame {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Some(frame) = capturer.capture_next().expect("frames") {
                return frame;
            }
        }
        panic!("no frame in five seconds");
    }

    /// Not a test so much as a hand: click in the Tain window of process
    /// `TAIN_TEST_PID` and photograph it, following `TAIN_TEST_STEPS`, which is
    /// `click X Y`, `wait MS` and `shot NAME` separated by `;`, in that window's
    /// own pixels. For watching the interface work in a KWin with no person in
    /// front of it.
    #[test]
    #[ignore = "needs a running KWin, PipeWire, and a Tain window to drive"]
    fn drive_a_tain_window() {
        use crate::input_inject::{InputInjector, MouseButtonInput, MousePosition, Shown};

        let out = std::path::PathBuf::from(std::env::var("TAIN_TEST_OUT").expect("TAIN_TEST_OUT"));
        let pid: u32 = std::env::var("TAIN_TEST_PID").expect("TAIN_TEST_PID").parse().unwrap();
        // The main window unless another is named: a session's is "Tain — Session".
        let title = std::env::var("TAIN_TEST_TITLE").unwrap_or_else(|_| "Tain".to_string());
        let kwin = kwin::get().expect("KWin lets this process list and capture windows");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let id = loop {
            if let Some((id, _)) = kwin.windows().into_iter().find(|(_, w)| w.pid == pid && w.title == title) {
                break id;
            }
            assert!(std::time::Instant::now() < deadline, "no Tain window for process {pid}");
            std::thread::sleep(std::time::Duration::from_millis(200));
        };
        let injector = crate::input_portal::PortalInjector::new();
        injector.show_windows(&[id]);
        for step in std::env::var("TAIN_TEST_STEPS").unwrap_or_default().split(';') {
            let words: Vec<&str> = step.split_whitespace().collect();
            match words.as_slice() {
                ["click", x, y] => {
                    let at = crate::window_capture::on_screen(id).expect("where it is");
                    injector.set_shown(Shown { x: at.x, y: at.y, width: at.width, height: at.height });
                    let fraction = |value: &str, size: u16| {
                        (value.parse::<f64>().unwrap() / f64::from(size) * 65535.0).round() as u16
                    };
                    injector
                        .move_mouse(MousePosition { x: fraction(x, at.width), y: fraction(y, at.height) })
                        .unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    injector.mouse_button(MouseButtonInput { button: 0, pressed: true }).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(80));
                    injector.mouse_button(MouseButtonInput { button: 0, pressed: false }).unwrap();
                }
                ["wait", ms] => std::thread::sleep(std::time::Duration::from_millis(ms.parse().unwrap())),
                ["shot", name] => {
                    let mut stream = KwinWindowCapturer::for_window(id, 30).expect("the window's stream");
                    save(&first_frame(&mut stream), &out.join(format!("{name}.png")));
                }
                [] => {}
                other => panic!("not a step: {other:?}"),
            }
        }
    }

    /// Everything here, against a real KWin with Tain's windows open in it:
    /// the window list, a thumbnail, a window's stream, a composite of every
    /// Tain window, and a click put into one through fake input.
    ///
    /// Run inside the KWin session, with PipeWire and WirePlumber running, and
    /// `TAIN_TEST_OUT` naming a folder for the pictures. `TAIN_TEST_CLICK`, as
    /// `x,y` in the first window's own pixels, is where to click; the frame
    /// after the click is saved beside the one before.
    #[test]
    #[ignore = "needs a running KWin, PipeWire, and a Tain window to share"]
    fn against_a_running_kwin() {
        use crate::input_inject::{InputInjector, MouseButtonInput, MousePosition, Shown};

        let out = std::path::PathBuf::from(std::env::var("TAIN_TEST_OUT").expect("TAIN_TEST_OUT"));
        let kwin = kwin::get().expect("KWin lets this process list and capture windows");
        let windows = crate::window_capture::shareable();
        println!("offered: {:?}", windows.iter().map(|w| (&w.app, &w.title, w.width, w.height)).collect::<Vec<_>>());
        let tains: Vec<_> = windows.iter().filter(|w| w.app == "tain").collect();
        assert!(!tains.is_empty(), "a Tain window to share");
        // One that is not in front where there is a choice, so that the click
        // below has to bring it forward to land in it.
        let first = tains
            .iter()
            .copied()
            .find(|w| kwin.window(w.id).is_some_and(|window| !window.active()))
            .unwrap_or(tains[0]);
        println!("first: {:?} at {:?}", first.title, crate::window_capture::on_screen(first.id));

        let thumb = crate::window_capture::thumbnail(first.id, 256).expect("a thumbnail");
        assert!(thumb.width <= 256 && thumb.height <= 256 && thumb.width > 0);
        image::RgbaImage::from_raw(u32::from(thumb.width), u32::from(thumb.height), thumb.rgba)
            .unwrap()
            .save(out.join("kwin-thumbnail.png"))
            .unwrap();

        let mut one = KwinWindowCapturer::for_window(first.id, 30).expect("the window's stream");
        let before = first_frame(&mut one);
        println!("stream {}x{}", before.width, before.height);
        save(&before, &out.join("kwin-window.png"));

        if tains.len() > 1 {
            let mut wall = crate::capture_wall::Wall::of_windows(
                &tains.iter().map(|w| (*w).clone()).collect::<Vec<_>>(),
            )
            .expect("a composite of the Tain windows");
            let tiles = wall.input_tiles().expect("tiles to map clicks through");
            println!("composite {:?}, tiles {tiles:?}", wall.frame_size());
            // The newest after a second, by which every panel has had time to
            // deliver: the first is composed as soon as any one of them has.
            let mut frame = first_frame(&mut wall);
            let settle = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while std::time::Instant::now() < settle {
                if let Some(newer) = wall.capture_next().expect("frames") {
                    frame = newer;
                }
            }
            save(&frame, &out.join("kwin-composite.png"));
        }

        if let Ok(click) = std::env::var("TAIN_TEST_CLICK") {
            let (x, y) = click.split_once(',').expect("x,y");
            let (x, y): (f64, f64) = (x.parse().unwrap(), y.parse().unwrap());
            let at = crate::window_capture::on_screen(first.id).expect("where it is");
            let injector = crate::input_portal::PortalInjector::new();
            injector.show_windows(&[first.id]);
            injector.set_shown(Shown { x: at.x, y: at.y, width: at.width, height: at.height });
            let fraction = |value: f64, size: u16| (value / f64::from(size) * 65535.0).round() as u16;
            injector
                .move_mouse(MousePosition { x: fraction(x, before.width), y: fraction(y, before.height) })
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            injector.mouse_button(MouseButtonInput { button: 0, pressed: true }).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(80));
            injector.mouse_button(MouseButtonInput { button: 0, pressed: false }).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(1500));
            // A fresh stream, so the frame is certainly from after the click.
            drop(one);
            let mut again = KwinWindowCapturer::for_window(first.id, 30).expect("the stream again");
            save(&first_frame(&mut again), &out.join("kwin-after-click.png"));
        }
        let _ = kwin;
    }

    #[test]
    fn a_thumbnail_keeps_the_shape_and_turns_bgra_into_rgba() {
        // 4x2, every pixel blue in BGRA order.
        let data: Vec<u8> = std::iter::repeat([255u8, 0, 0, 255]).take(8).flatten().collect();
        let frame = ScreenFrame::new(4, 2, bytes::Bytes::from(data), std::time::Duration::ZERO);
        let small = shrink(&frame, 2);
        assert_eq!((small.width, small.height), (2, 1));
        assert_eq!(&small.rgba[..4], &[0, 0, 255, 255], "blue, now in RGBA order");
        let same = shrink(&frame, 100);
        assert_eq!((same.width, same.height), (4, 2), "never enlarged");
    }
}
