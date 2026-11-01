//! Screen capture on Linux: the screen cast portal's PipeWire stream.
//!
//! [`crate::portal`] gets the desktop's permission and the stream's node;
//! `linux/db_pipewire.c` receives its frames; this turns them into the
//! [`ScreenFrame`]s the host's pipeline already takes from DXGI on Windows.
//!
//! ## What the person at the host sees
//!
//! The first time, the desktop's own dialog, asking which screen or window to
//! share and whether to allow control. It is remembered after that (see
//! `portal`), so a later session starts without it. While anything is shared
//! the desktop shows that it is, in its own way — GNOME's indicator, KDE's
//! notification.
//!
//! ## Frames arrive when the screen changes
//!
//! A compositor sends a frame when something on screen moved, not on a clock,
//! so an idle desktop produces nothing and [`ScreenCaptureBackend::capture_next`]
//! says so with `None` — exactly what Desktop Duplication does on Windows.

#![cfg(all(target_os = "linux", db_pipewire))]

use anyhow::{Context, Result};
use bytes::Bytes;
use std::os::fd::IntoRawFd;
use std::sync::Arc;
use tracing::*;

use crate::portal::{self, RemoteSession};
use crate::screen_capture::{CaptureConfig, ScreenCaptureBackend, ScreenFrame};

#[repr(C)]
struct DbPwStream {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn db_pw_available(err: *mut u8, err_len: i32) -> i32;
    fn db_pw_stream_open(fd: i32, node: u32, err: *mut u8, err_len: i32) -> *mut DbPwStream;
    fn db_pw_stream_wait_size(
        s: *mut DbPwStream,
        timeout_ms: i32,
        width: *mut i32,
        height: *mut i32,
        err: *mut u8,
        err_len: i32,
    ) -> i32;
    fn db_pw_stream_frame(
        s: *mut DbPwStream,
        timeout_ms: i32,
        dst: *mut u8,
        dst_cap: usize,
        width: *mut i32,
        height: *mut i32,
        err: *mut u8,
        err_len: i32,
    ) -> i32;
    fn db_pw_stream_close(s: *mut DbPwStream);
}

fn message(err: &[u8]) -> String {
    let end = err.iter().position(|&b| b == 0).unwrap_or(err.len());
    String::from_utf8_lossy(&err[..end]).into_owned()
}

/// How long to wait for the stream to say what size it is.
const SIZE_TIMEOUT_MS: i32 = 5000;

/// Whether this machine can capture through the portal at all: PipeWire is
/// installed. Asked before the portal, so a machine without it does not show
/// a dialog for a stream nothing could read.
pub fn available() -> Result<()> {
    let mut err = [0u8; 256];
    // SAFETY: the buffer is valid for its stated length.
    let ok = unsafe { db_pw_available(err.as_mut_ptr(), err.len() as i32) };
    anyhow::ensure!(ok == 1, "{}", message(&err));
    Ok(())
}

/// One shared screen, as frames.
pub struct PortalCapturer {
    /// Kept for as long as this captures: dropping the last one ends the
    /// portal session, and with it the desktop's sharing indicator.
    session: Arc<RemoteSession>,
    stream: *mut DbPwStream,
    size: (u16, u16),
    /// The screen's size in the desktop's logical coordinates, which on a
    /// scaled display is smaller than the frames. Input is mapped through it.
    logical: Option<(i32, i32)>,
    wait_ms: i32,
}

// SAFETY: the stream handle is owned solely here and used from one thread at a
// time; the C side locks around everything its own thread shares.
unsafe impl Send for PortalCapturer {}
unsafe impl Sync for PortalCapturer {}

impl PortalCapturer {
    /// Share a screen. Shows the desktop's dialog unless a remembered choice
    /// holds, and blocks until it is answered.
    pub fn new(config: &CaptureConfig) -> Result<Self> {
        available()?;
        let session = portal::session()?;
        let info = session.streams().first().cloned().context("the desktop shared no screen")?;
        let fd = session.pipewire_fd()?;

        let mut err = [0u8; 256];
        // SAFETY: the descriptor is handed over (PipeWire closes it), the error
        // buffer is valid for its stated length, and the handle is checked.
        let stream = unsafe {
            db_pw_stream_open(fd.into_raw_fd(), info.node_id, err.as_mut_ptr(), err.len() as i32)
        };
        anyhow::ensure!(!stream.is_null(), "{}", message(&err));

        let (mut width, mut height) = (0, 0);
        // SAFETY: a live stream and valid out-pointers.
        let rc = unsafe {
            db_pw_stream_wait_size(stream, SIZE_TIMEOUT_MS, &mut width, &mut height, err.as_mut_ptr(), err.len() as i32)
        };
        if rc != 1 {
            // SAFETY: a live stream, closed exactly once.
            unsafe { db_pw_stream_close(stream) };
            anyhow::bail!(
                "the shared screen never said its size ({})",
                if rc < 0 { message(&err) } else { "timed out".to_string() }
            );
        }
        let size = (width.clamp(1, u16::MAX as i32) as u16, height.clamp(1, u16::MAX as i32) as u16);
        info!(
            "Capturing the shared screen at {}x{} (logical {:?}) through PipeWire node {}",
            size.0, size.1, info.size, info.node_id
        );
        Ok(Self {
            session,
            stream,
            size,
            logical: info.size,
            // A frame period, so the capture loop's own pacing is not undone
            // by waiting here longer than it would.
            wait_ms: (1000 / config.fps.max(1)).clamp(1, 100) as i32,
        })
    }

    /// The screen's size in logical desktop coordinates, when the desktop said.
    pub fn logical_size(&self) -> Option<(i32, i32)> {
        self.logical
    }

    /// The portal session this capture belongs to.
    pub fn session(&self) -> &Arc<RemoteSession> {
        &self.session
    }
}

impl ScreenCaptureBackend for PortalCapturer {
    fn capture_next(&mut self) -> Result<Option<ScreenFrame>> {
        let mut err = [0u8; 256];
        let (mut width, mut height) = (0, 0);
        // First only wait: with no room offered, a fresh frame reports its
        // size and stays put. The wait is not the frame's cost.
        // SAFETY: a live stream and valid out-pointers; a zero-length
        // destination is never written.
        let rc = unsafe {
            db_pw_stream_frame(
                self.stream,
                self.wait_ms,
                std::ptr::null_mut(),
                0,
                &mut width,
                &mut height,
                err.as_mut_ptr(),
                err.len() as i32,
            )
        };
        match rc {
            0 => return Ok(None),
            -1 => anyhow::bail!("the shared screen went away: {}", message(&err)),
            _ => {}
        }

        let started = std::time::Instant::now();
        let len = width as usize * height as usize * 4;
        let mut frame = vec![0u8; len];
        // SAFETY: a live stream, a destination valid for `len` bytes, and
        // valid out-pointers. A zero timeout: the frame is already there.
        let rc = unsafe {
            db_pw_stream_frame(
                self.stream,
                0,
                frame.as_mut_ptr(),
                frame.len(),
                &mut width,
                &mut height,
                err.as_mut_ptr(),
                err.len() as i32,
            )
        };
        match rc {
            1 => {}
            // A newer frame of another size arrived in between; the next call
            // takes it.
            -2 | 0 => return Ok(None),
            _ => anyhow::bail!("the shared screen went away: {}", message(&err)),
        }
        frame.truncate(width as usize * height as usize * 4);
        let (w, h) = (width as u16, height as u16);
        if (w, h) != self.size {
            info!("The shared screen is now {w}x{h}");
            self.size = (w, h);
        }
        Ok(Some(ScreenFrame::new(w, h, Bytes::from(frame), started.elapsed())))
    }

    fn frame_size(&self) -> (u16, u16) {
        self.size
    }
}

impl Drop for PortalCapturer {
    fn drop(&mut self) {
        // SAFETY: a live stream, closed exactly once, before the session it
        // reads from is let go.
        unsafe { db_pw_stream_close(self.stream) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PipeWire half without a portal: connect to a video node on the
    /// local PipeWire daemon, as a capture does to the portal's, and read
    /// frames from it.
    ///
    /// Needs a PipeWire daemon with a session manager and a video source.
    /// Publish one with
    ///
    /// ```text
    /// gst-launch-1.0 videotestsrc is-live=true pattern=smpte \
    ///     ! video/x-raw,format=BGRx,width=640,height=360,framerate=30/1 \
    ///     ! pipewiresink mode=provide stream-properties="props,media.class=Video/Source"
    /// ```
    ///
    /// and run with `TAIN_TEST_PIPEWIRE_NODE=<its id>`.
    #[test]
    #[ignore = "needs a PipeWire daemon and a video source node"]
    fn frames_arrive_from_a_pipewire_video_node() {
        let node: u32 = std::env::var("TAIN_TEST_PIPEWIRE_NODE")
            .expect("set TAIN_TEST_PIPEWIRE_NODE to a Video/Source node id")
            .parse()
            .expect("a node id");
        available().expect("libpipewire-0.3 must load");
        let runtime = std::env::var("PIPEWIRE_RUNTIME_DIR")
            .or_else(|_| std::env::var("XDG_RUNTIME_DIR"))
            .expect("XDG_RUNTIME_DIR");
        let socket = std::os::unix::net::UnixStream::connect(format!("{runtime}/pipewire-0"))
            .expect("the PipeWire socket");

        let mut err = [0u8; 256];
        let stream = unsafe {
            db_pw_stream_open(socket.into_raw_fd(), node, err.as_mut_ptr(), err.len() as i32)
        };
        assert!(!stream.is_null(), "{}", message(&err));
        let (mut w, mut h) = (0, 0);
        let rc = unsafe {
            db_pw_stream_wait_size(stream, 5000, &mut w, &mut h, err.as_mut_ptr(), err.len() as i32)
        };
        assert_eq!(rc, 1, "no size negotiated: {}", message(&err));
        assert_eq!((w, h), (640, 360), "negotiated an unexpected size");

        let mut frames = 0;
        let mut last = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline && frames < 10 {
            let mut frame = vec![0u8; 640 * 360 * 4];
            let rc = unsafe {
                db_pw_stream_frame(stream, 200, frame.as_mut_ptr(), frame.len(), &mut w, &mut h, err.as_mut_ptr(), err.len() as i32)
            };
            assert!(rc >= 0, "the stream ended: {}", message(&err));
            if rc == 1 {
                frames += 1;
                last = frame;
            }
        }
        unsafe { db_pw_stream_close(stream) };
        assert!(frames >= 5, "only {frames} frames in three seconds");

        // SMPTE bars: the first is 75% white, and alpha is opaque because BGRx
        // came in. A transposed or misread frame fails one of these.
        let px = &last[(100 * 640 + 20) * 4..(100 * 640 + 20) * 4 + 4];
        assert!(px[0] > 150 && px[1] > 150 && px[2] > 150, "top-left bar is not white: {px:?}");
        assert_eq!(px[3], 255, "BGRx must come out opaque");
        let px = &last[(100 * 640 + 640 * 3 / 7 + 20) * 4..][..4];
        // The fourth bar is green: G high, R and B low.
        assert!(px[1] > 150 && px[0] < 80 && px[2] < 80, "fourth bar is not green: {px:?}");
    }
}
