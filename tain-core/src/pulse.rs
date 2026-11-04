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

use crate::AudioDevice;

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

// ── Which devices there are ──────────────────────────────────────

/// The start of `pa_sink_info`, which has stayed this way since PulseAudio 0.9:
/// fields are only ever added after these.
#[repr(C)]
struct SinkInfo {
    name: *const c_char,
    index: u32,
    description: *const c_char,
}

/// The start of `pa_server_info`, likewise.
#[repr(C)]
struct ServerInfo {
    user_name: *const c_char,
    host_name: *const c_char,
    server_version: *const c_char,
    server_name: *const c_char,
    sample_spec: SampleSpec,
    default_sink_name: *const c_char,
}

type SinkCallback = unsafe extern "C" fn(*mut c_void, *const SinkInfo, c_int, *mut c_void);
type ServerCallback = unsafe extern "C" fn(*mut c_void, *const ServerInfo, *mut c_void);

const PA_CONTEXT_NOAUTOSPAWN: c_int = 1;
const PA_CONTEXT_READY: c_int = 4;
const PA_CONTEXT_FAILED: c_int = 5;
const PA_CONTEXT_TERMINATED: c_int = 6;
const PA_OPERATION_RUNNING: c_int = 0;

/// libpulse proper, for the one thing the simple API cannot do: ask what is
/// plugged in. Also opened at run time, for the same reason as the rest.
struct Introspect {
    mainloop_new: unsafe extern "C" fn() -> *mut c_void,
    mainloop_get_api: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    mainloop_iterate: unsafe extern "C" fn(*mut c_void, c_int, *mut c_int) -> c_int,
    mainloop_free: unsafe extern "C" fn(*mut c_void),
    context_new: unsafe extern "C" fn(*mut c_void, *const c_char) -> *mut c_void,
    context_connect: unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *const c_void) -> c_int,
    context_get_state: unsafe extern "C" fn(*mut c_void) -> c_int,
    context_disconnect: unsafe extern "C" fn(*mut c_void),
    context_unref: unsafe extern "C" fn(*mut c_void),
    get_server_info: unsafe extern "C" fn(*mut c_void, ServerCallback, *mut c_void) -> *mut c_void,
    get_sink_info_list: unsafe extern "C" fn(*mut c_void, SinkCallback, *mut c_void) -> *mut c_void,
    operation_get_state: unsafe extern "C" fn(*mut c_void) -> c_int,
    operation_unref: unsafe extern "C" fn(*mut c_void),
}

fn introspect() -> Option<&'static Introspect> {
    static LOADED: OnceLock<Option<Introspect>> = OnceLock::new();
    LOADED
        .get_or_init(|| {
            // SAFETY: dlopen with a constant C string; the handle is never closed.
            let handle =
                unsafe { libc::dlopen(c"libpulse.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if handle.is_null() {
                debug!("No libpulse to list sound devices with ({})", dl_error());
                return None;
            }
            let symbol = |name: &CStr| -> Option<usize> {
                // SAFETY: a live handle and a constant C string.
                let address = unsafe { libc::dlsym(handle, name.as_ptr()) };
                (!address.is_null()).then_some(address as usize)
            };
            // SAFETY: each address is the named function, whose prototype the
            // field it goes into copies from <pulse/mainloop.h>,
            // <pulse/context.h> and <pulse/introspect.h>.
            unsafe {
                Some(Introspect {
                    mainloop_new: std::mem::transmute::<usize, _>(symbol(c"pa_mainloop_new")?),
                    mainloop_get_api: std::mem::transmute::<usize, _>(symbol(c"pa_mainloop_get_api")?),
                    mainloop_iterate: std::mem::transmute::<usize, _>(symbol(c"pa_mainloop_iterate")?),
                    mainloop_free: std::mem::transmute::<usize, _>(symbol(c"pa_mainloop_free")?),
                    context_new: std::mem::transmute::<usize, _>(symbol(c"pa_context_new")?),
                    context_connect: std::mem::transmute::<usize, _>(symbol(c"pa_context_connect")?),
                    context_get_state: std::mem::transmute::<usize, _>(symbol(c"pa_context_get_state")?),
                    context_disconnect: std::mem::transmute::<usize, _>(symbol(c"pa_context_disconnect")?),
                    context_unref: std::mem::transmute::<usize, _>(symbol(c"pa_context_unref")?),
                    get_server_info: std::mem::transmute::<usize, _>(symbol(c"pa_context_get_server_info")?),
                    get_sink_info_list: std::mem::transmute::<usize, _>(symbol(c"pa_context_get_sink_info_list")?),
                    operation_get_state: std::mem::transmute::<usize, _>(symbol(c"pa_operation_get_state")?),
                    operation_unref: std::mem::transmute::<usize, _>(symbol(c"pa_operation_unref")?),
                })
            }
        })
        .as_ref()
}

/// What the server's two answers are collected into.
#[derive(Default)]
struct Answers {
    default_sink: Option<String>,
    sinks: Vec<(String, String)>,
}

fn owned(text: *const c_char) -> Option<String> {
    // SAFETY: the callers pass strings the server's answer owns, or null.
    (!text.is_null()).then(|| unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned())
}

unsafe extern "C" fn on_server(_: *mut c_void, info: *const ServerInfo, answers: *mut c_void) {
    if info.is_null() {
        return;
    }
    // SAFETY: `answers` is the `Answers` `ask_server` passed, alive until its
    // loop ends, and `info` is valid for this call.
    unsafe { (*answers.cast::<Answers>()).default_sink = owned((*info).default_sink_name) };
}

unsafe extern "C" fn on_sink(_: *mut c_void, info: *const SinkInfo, end: c_int, answers: *mut c_void) {
    if end != 0 || info.is_null() {
        return;
    }
    // SAFETY: as in `on_server`.
    unsafe {
        let Some(name) = owned((*info).name) else { return };
        let description = owned((*info).description).unwrap_or_else(|| name.clone());
        (*answers.cast::<Answers>()).sinks.push((name, description));
    }
}

/// Ask the sound server what it plays on. `None` if it does not answer in time.
fn ask_server() -> Option<Answers> {
    /// Long enough for a server that is there, and short enough not to be
    /// noticed when it is not: a picker waits on the first answer.
    const PATIENCE: std::time::Duration = std::time::Duration::from_millis(400);

    let api = introspect()?;
    let mut answers = Answers::default();
    let deadline = std::time::Instant::now() + PATIENCE;

    // SAFETY: the documented sequence, with every object checked and freed in
    // reverse order below, on this thread only. `answers` outlives the loop,
    // and nothing runs a callback once the context has been disconnected.
    unsafe {
        let mainloop = (api.mainloop_new)();
        if mainloop.is_null() {
            return None;
        }
        let context = (api.context_new)((api.mainloop_get_api)(mainloop), c"Tain".as_ptr());
        let mut done = false;
        if !context.is_null() {
            let connected =
                (api.context_connect)(context, std::ptr::null(), PA_CONTEXT_NOAUTOSPAWN, std::ptr::null()) >= 0;
            let target: *mut c_void = (&raw mut answers).cast();
            let mut running: [*mut c_void; 2] = [std::ptr::null_mut(); 2];
            let mut asked = false;
            while connected && !done && std::time::Instant::now() < deadline {
                (api.mainloop_iterate)(mainloop, 0, std::ptr::null_mut());
                match (api.context_get_state)(context) {
                    PA_CONTEXT_READY if !asked => {
                        asked = true;
                        running[0] = (api.get_server_info)(context, on_server, target);
                        running[1] = (api.get_sink_info_list)(context, on_sink, target);
                    }
                    PA_CONTEXT_READY => {
                        done = running
                            .iter()
                            .all(|op| op.is_null() || (api.operation_get_state)(*op) != PA_OPERATION_RUNNING);
                    }
                    PA_CONTEXT_FAILED | PA_CONTEXT_TERMINATED => break,
                    _ => {}
                }
                if !done {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            for op in running {
                if !op.is_null() {
                    (api.operation_unref)(op);
                }
            }
            (api.context_disconnect)(context);
            (api.context_unref)(context);
        }
        (api.mainloop_free)(mainloop);
        done.then_some(answers)
    }
}

/// The server's answers as the pickers show them, the default first.
fn listed(answers: Answers) -> Vec<AudioDevice> {
    let Answers { default_sink, sinks } = answers;
    let mut devices: Vec<AudioDevice> = sinks
        .into_iter()
        .map(|(id, name)| AudioDevice { default: default_sink.as_deref() == Some(&id), id, name })
        .collect();
    devices.sort_by_key(|device| !device.default);
    devices
}

/// Every playback device the sound server has, the default first.
///
/// Called from the interface's frames and from the threads playing sound, so it
/// does not wait on the server: only the very first call does, so that a picker
/// is right the first time it opens. After that it answers from what it last
/// heard and asks again in the background once that is [`STALE`] old.
pub fn devices() -> Vec<AudioDevice> {
    const STALE: std::time::Duration = std::time::Duration::from_secs(2);

    struct Heard {
        at: std::time::Instant,
        devices: Vec<AudioDevice>,
        asking: bool,
    }
    static HEARD: std::sync::Mutex<Option<Heard>> = std::sync::Mutex::new(None);

    fn ask() -> Vec<AudioDevice> {
        ask_server().map(listed).unwrap_or_default()
    }

    let mut heard = HEARD.lock().unwrap_or_else(|e| e.into_inner());
    let Some(known) = heard.as_mut() else {
        let devices = ask();
        *heard = Some(Heard { at: std::time::Instant::now(), devices: devices.clone(), asking: false });
        return devices;
    };
    if known.at.elapsed() >= STALE && !known.asking {
        known.asking = true;
        std::thread::spawn(|| {
            let devices = ask();
            if let Some(known) = HEARD.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                *known = Heard { at: std::time::Instant::now(), devices, asking: false };
            }
        });
    }
    known.devices.clone()
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
    /// A playback stream on `device`, or on the default with `None`, keeping
    /// about `target_ms` of sound queued in the server and asking for more
    /// `period_ms` at a time.
    ///
    /// What is queued is latency, so it is kept small; see `audio_output`.
    pub fn playback(
        stream_name: &str,
        device: Option<&str>,
        rate: u32,
        channels: u16,
        target_ms: u32,
        period_ms: u32,
    ) -> Result<Self> {
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
        Self::open(PA_STREAM_PLAYBACK, device, stream_name, rate, channels, &attr)
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
