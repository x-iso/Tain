//! Sound on Linux, through the PulseAudio simple API, opened at run time.
//!
//! ## Why PulseAudio's API on a PipeWire desktop
//!
//! Every Linux desktop of the last decade answers it: PulseAudio itself, or
//! PipeWire through `pipewire-pulse`, which is what Fedora, Bazzite, Ubuntu
//! 22.10 and later, Arch and SteamOS run. One backend covers them all, where
//! PipeWire's own API would leave out the PulseAudio machines and ALSA would
//! leave out the mixing.
//!
//! The *simple* API in particular: a blocking read and a blocking write on a
//! stream, which is exactly the shape of a capture thread and a render thread.
//! The blocking write is what paces playback at the device's rate, the way the
//! WASAPI event does on Windows.
//!
//! ## Why opened at run time
//!
//! The same rule as every desktop library this binary uses: a missing linked
//! library stops the program before `main`. A machine with no sound server
//! still runs Tain, without sound, and says why.

#![cfg(target_os = "linux")]

use anyhow::{Context, Result};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::OnceLock;
use tracing::*;

/// `pa_sample_spec`.
#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

/// `pa_buffer_attr`. `u32::MAX` means "the server's default".
#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

const PA_SAMPLE_FLOAT32LE: c_int = 5;
const PA_STREAM_PLAYBACK: c_int = 1;
const PA_STREAM_RECORD: c_int = 2;
const DEFAULT: u32 = u32::MAX;

type NewFn = unsafe extern "C" fn(
    server: *const c_char,
    name: *const c_char,
    direction: c_int,
    device: *const c_char,
    stream_name: *const c_char,
    spec: *const SampleSpec,
    channel_map: *const c_void,
    attr: *const BufferAttr,
    error: *mut c_int,
) -> *mut c_void;
type FreeFn = unsafe extern "C" fn(s: *mut c_void);
type WriteFn = unsafe extern "C" fn(s: *mut c_void, data: *const c_void, bytes: usize, error: *mut c_int) -> c_int;
type ReadFn = unsafe extern "C" fn(s: *mut c_void, data: *mut c_void, bytes: usize, error: *mut c_int) -> c_int;
type StrerrorFn = unsafe extern "C" fn(error: c_int) -> *const c_char;

struct Api {
    new: NewFn,
    free: FreeFn,
    write: WriteFn,
    read: ReadFn,
    strerror: StrerrorFn,
}

static API: OnceLock<Option<Api>> = OnceLock::new();

fn api() -> Result<&'static Api> {
    API.get_or_init(|| match load() {
        Ok(api) => Some(api),
        Err(e) => {
            info!("No PulseAudio client library ({e:#}) — no sound");
            None
        }
    })
    .as_ref()
    .context("libpulse-simple.so.0 is not installed, so there is no sound")
}

fn load() -> Result<Api> {
    // SAFETY: dlopen with a constant C string; the handle is never closed.
    let handle =
        unsafe { libc::dlopen(c"libpulse-simple.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    anyhow::ensure!(!handle.is_null(), "{}", dl_error());
    let symbol = |name: &CStr| -> Result<usize> {
        // SAFETY: a live handle and a constant C string. pa_strerror lives in
        // libpulse, which libpulse-simple depends on, and dlsym on a handle
        // searches its dependencies too.
        let address = unsafe { libc::dlsym(handle, name.as_ptr()) };
        anyhow::ensure!(!address.is_null(), "{} is missing", name.to_string_lossy());
        Ok(address as usize)
    };
    // SAFETY: each address is the named function, whose prototype the type
    // beside it copies from <pulse/simple.h> and <pulse/error.h>.
    unsafe {
        Ok(Api {
            new: std::mem::transmute::<usize, NewFn>(symbol(c"pa_simple_new")?),
            free: std::mem::transmute::<usize, FreeFn>(symbol(c"pa_simple_free")?),
            write: std::mem::transmute::<usize, WriteFn>(symbol(c"pa_simple_write")?),
            read: std::mem::transmute::<usize, ReadFn>(symbol(c"pa_simple_read")?),
            strerror: std::mem::transmute::<usize, StrerrorFn>(symbol(c"pa_strerror")?),
        })
    }
}

fn dl_error() -> String {
    // SAFETY: dlerror returns a thread-local string or null.
    let message = unsafe { libc::dlerror() };
    if message.is_null() {
        return "unknown error".to_string();
    }
    unsafe { CStr::from_ptr(message) }.to_string_lossy().into_owned()
}

fn describe(api: &Api, error: c_int) -> String {
    // SAFETY: pa_strerror returns a static string for any code.
    let text = unsafe { (api.strerror)(error) };
    if text.is_null() {
        return format!("error {error}");
    }
    unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned()
}

/// Whether there is a PulseAudio client library to talk to a sound server with.
pub fn available() -> bool {
    api().is_ok()
}

/// Bytes in `ms` milliseconds of interleaved float32.
fn bytes_for(ms: u32, rate: u32, channels: u16) -> u32 {
    (rate as u64 * ms as u64 / 1000) as u32 * channels as u32 * 4
}

/// One stream to or from the sound server: interleaved float32, blocking.
pub struct Stream {
    handle: *mut c_void,
    api: &'static Api,
}

// SAFETY: a pa_simple object may be used from any one thread at a time, and
// every method here takes `&mut self`.
unsafe impl Send for Stream {}

impl Stream {
    /// A playback stream on the default device, keeping about `target_ms` of
    /// sound queued in the server and asking for more `period_ms` at a time.
    ///
    /// What is queued is latency, so it is kept small; see `audio_output`.
    pub fn playback(stream_name: &str, rate: u32, channels: u16, target_ms: u32, period_ms: u32) -> Result<Self> {
        let attr = BufferAttr {
            maxlength: DEFAULT,
            tlength: bytes_for(target_ms, rate, channels),
            // Start playing as soon as one period is there, rather than waiting
            // for the whole target: the jitter buffer upstream already decided
            // when there was enough to start.
            prebuf: bytes_for(period_ms, rate, channels),
            minreq: bytes_for(period_ms, rate, channels),
            fragsize: DEFAULT,
        };
        Self::open(PA_STREAM_PLAYBACK, None, stream_name, rate, channels, &attr)
    }

    /// A record stream from `device`, delivered `fragment_ms` at a time.
    ///
    /// `@DEFAULT_MONITOR@` is what the default output is playing, and follows
    /// the default when it moves — on PulseAudio and on PipeWire alike.
    pub fn record(stream_name: &str, device: &str, rate: u32, channels: u16, fragment_ms: u32) -> Result<Self> {
        let attr = BufferAttr {
            maxlength: DEFAULT,
            tlength: DEFAULT,
            prebuf: DEFAULT,
            minreq: DEFAULT,
            // Without this the server batches capture into large fragments,
            // and every block arrives late by however much it held back.
            fragsize: bytes_for(fragment_ms, rate, channels),
        };
        Self::open(PA_STREAM_RECORD, Some(device), stream_name, rate, channels, &attr)
    }

    fn open(
        direction: c_int,
        device: Option<&str>,
        stream_name: &str,
        rate: u32,
        channels: u16,
        attr: &BufferAttr,
    ) -> Result<Self> {
        let api = api()?;
        let spec = SampleSpec { format: PA_SAMPLE_FLOAT32LE, rate, channels: channels as u8 };
        let device = device.map(CString::new).transpose().context("a NUL in the device name")?;
        let stream_name = CString::new(stream_name).context("a NUL in the stream name")?;
        let mut error: c_int = 0;
        // SAFETY: every pointer is valid for the duration of the call, the
        // structures are laid out as <pulse/sample.h> and <pulse/def.h> declare
        // them, and the result is checked.
        let handle = unsafe {
            (api.new)(
                std::ptr::null(),
                c"Tain".as_ptr(),
                direction,
                device.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
                stream_name.as_ptr(),
                &spec,
                std::ptr::null(),
                attr,
                &mut error,
            )
        };
        anyhow::ensure!(
            !handle.is_null(),
            "the sound server refused the stream: {}",
            describe(api, error)
        );
        Ok(Self { handle, api })
    }

    /// Queue samples for playback, blocking until the server has room.
    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        let mut error: c_int = 0;
        // SAFETY: a live stream and a slice valid for its length in bytes.
        let rc = unsafe {
            (self.api.write)(self.handle, samples.as_ptr().cast(), std::mem::size_of_val(samples), &mut error)
        };
        anyhow::ensure!(rc >= 0, "{}", describe(self.api, error));
        Ok(())
    }

    /// Fill `samples` with captured sound, blocking until there is enough.
    pub fn read(&mut self, samples: &mut [f32]) -> Result<()> {
        let mut error: c_int = 0;
        // SAFETY: a live stream and a slice valid for its length in bytes.
        let rc = unsafe {
            (self.api.read)(self.handle, samples.as_mut_ptr().cast(), std::mem::size_of_val(samples), &mut error)
        };
        anyhow::ensure!(rc >= 0, "{}", describe(self.api, error));
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: a live stream, freed exactly once.
        unsafe { (self.api.free)(self.handle) };
    }
}
