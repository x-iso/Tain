//! System audio capture via WASAPI loopback.
//!
//! Captures what the machine is playing, from the default render endpoint, on
//! its own thread and its own clock.
//!
//! ## The clock is not the video clock
//!
//! Audio is driven by the audio engine's period, not by the capture cadence of
//! the video pipeline. Sampling it on the video thread would tie it to a rate
//! that changes whenever the adaptive controller moves, and drift between the
//! two is exactly what makes lip sync fail. The two streams carry timestamps
//! and are reconciled at the far end; see the transport work.
//!
//! ## Silence is data
//!
//! With nothing playing, a loopback client does not simply block — depending on
//! the Windows version it either delivers buffers flagged silent or delivers
//! nothing at all. Both have to produce a continuous sample stream, or the
//! guest's render buffer underruns and every resumption of sound starts with a
//! click. The capture loop waits with a timeout and manufactures silence when
//! the timeout wins, so the output is gapless regardless of which behaviour
//! this machine has.
//!
//! ## Excluding an application
//!
//! Windows can leave a process tree out of the mix for us, exactly, inside the
//! audio engine — see [`LoopbackCapture::start_excluding`]. That is what
//! `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE` is for, and it is the
//! only correct way to do this.
//!
//! It is worth saying what the alternative would have been, because it sounds
//! reasonable and is not: capturing the app separately and subtracting it from
//! the mix by phase cancellation. The app's contribution to the device mix is
//! not its own samples — per-session volume is applied, then the mix, then the
//! device's own effects (equalisation, loudness levelling, spatial audio), then
//! possibly a resample. Two capture clients also run on separate clocks and
//! drift. The result is a ghost of the excluded app plus comb filtering, not
//! silence.
//!
//! ## Format
//!
//! The endpoint's mix format is whatever the user's device is running at, which
//! is usually 48 kHz float32 stereo and is never safe to assume — plenty of
//! machines run 44.1 kHz, and studio interfaces run 96. It is read and reported,
//! and conversion is the encoder's problem rather than something done blindly
//! here.

#![cfg(target_os = "windows")]

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::*;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Interface;
use windows::Win32::Media::Audio::{
    IAudioSessionControl2, IAudioSessionManager2,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
};

/// What the endpoint is running at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioFormat {
    /// Frames in `duration` at this rate, rounded down.
    pub fn frames(&self, duration: std::time::Duration) -> usize {
        (duration.as_secs_f64() * self.sample_rate as f64) as usize
    }
}

/// One block of captured audio, interleaved float32.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub samples: Vec<f32>,
    /// Frames captured before this block, counting silence.
    ///
    /// The presentation clock. Taken from the running total rather than from
    /// arrival time, so a scheduling hiccup on the capture thread does not
    /// become a timestamp error the far end has to correct for.
    pub position: u64,
    /// Whether the engine reported this block as silent, or it was manufactured.
    pub silent: bool,
}

/// A running loopback capture, stopped when dropped.
pub struct LoopbackCapture {
    format: AudioFormat,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LoopbackCapture {
    /// Start capturing the default render endpoint.
    ///
    /// Blocks only long enough for the thread to open the device and report the
    /// format, so a failure to start is returned here rather than surfacing
    /// later as silence nobody can explain.
    pub fn start() -> Result<(Self, std::sync::mpsc::Receiver<AudioChunk>)> {
        Self::open(Excluding::Nothing)
    }

    /// Capture everything except one process and its children.
    ///
    /// The exclusion is done by the audio engine at mix time, so the result is
    /// the system mix with that tree already absent — exactly, with no residue
    /// and no drift. See the module header for why the obvious alternative
    /// (subtracting the app's own stream) does not work.
    ///
    /// Only one process tree can be excluded per client, which is a limit of
    /// the API rather than a choice. See [`Self::start_excluding_many`].
    pub fn start_excluding(pid: u32) -> Result<(Self, std::sync::mpsc::Receiver<AudioChunk>)> {
        Self::open(Excluding::One(pid))
    }

    /// Capture everything except several process trees.
    ///
    /// # Why this needs a different mechanism
    ///
    /// `ActivateAudioInterfaceAsync` takes exactly one target process id, and
    /// two exclusions cannot be composed from two exclude-mode streams — each
    /// one already contains the other's target.
    ///
    /// So beyond one app the mix is built the other way round: every audio
    /// session that is *not* excluded is captured in include mode and the
    /// results are summed. That is still the engine's own per-process audio
    /// rather than anything subtracted, so it stays exact.
    ///
    /// The cost is that system sounds have no process tree to include and so
    /// are absent from the mix. One exclusion therefore still uses the native
    /// path, which does not have that problem — this is only for the case that
    /// cannot.
    ///
    /// It used to cost more than that. The set of applications was worked out
    /// once, when the session started, so anything launched afterwards was never
    /// heard: "only apps that were already open stream their audio", exactly as
    /// reported. [`run_mixed`] now rescans, and the set is no longer a snapshot.
    pub fn start_excluding_many(
        pids: &[u32],
    ) -> Result<(Self, std::sync::mpsc::Receiver<AudioChunk>)> {
        match pids {
            [] => Self::open(Excluding::Nothing),
            [only] => Self::open(Excluding::One(*only)),
            many => Self::open(Excluding::AllBut(many.to_vec())),
        }
    }

    fn open(exclude: Excluding) -> Result<(Self, std::sync::mpsc::Receiver<AudioChunk>)> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<AudioFormat>>();
        // Bounded, because an encoder that falls behind must not be allowed to
        // turn into unbounded memory. Two seconds of 10 ms blocks is generous
        // for a stage that should never be more than one block behind.
        let (chunk_tx, chunk_rx) = std::sync::mpsc::sync_channel::<AudioChunk>(200);

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        // The thread takes ownership; this copy is only for the log line below.
        let describe = exclude.clone();
        let thread = std::thread::Builder::new()
            .name("audio-loopback".into())
            .spawn(move || capture_thread(exclude, ready_tx, chunk_tx, thread_stop))
            .context("could not start the audio capture thread")?;

        let format = ready_rx
            .recv()
            .context("the audio capture thread stopped before reporting a format")??;

        match &describe {
            Excluding::One(pid) => info!(
                "Capturing system audio at {} Hz, {} channels, excluding process {pid}",
                format.sample_rate, format.channels
            ),
            Excluding::AllBut(pids) => info!(
                "Capturing system audio at {} Hz, {} channels, mixing every application except {} of them",
                format.sample_rate,
                format.channels,
                pids.len()
            ),
            Excluding::Nothing => info!(
                "Capturing system audio at {} Hz, {} channels",
                format.sample_rate, format.channels
            ),
        }
        Ok((Self { format, stop, thread: Some(thread) }, chunk_rx))
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }
}

impl Drop for LoopbackCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // The loop wakes at least every `WAIT_TIMEOUT_MS`, so this is a
            // bounded wait rather than a hope.
            let _ = thread.join();
        }
    }
}

/// What a capture should leave out.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Excluding {
    /// The whole system mix.
    Nothing,
    /// Everything but this process tree, done by the audio engine itself.
    One(u32),
    /// Everything but these process trees, built by summing the applications
    /// that are left. The only way to exclude more than one.
    ///
    /// Holds what to *exclude*, not the applications to capture: the set to
    /// capture changes as applications come and go, and it is [`run_mixed`] that
    /// keeps up with that.
    AllBut(Vec<u32>),
}

/// An application currently playing audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioSession {
    pub pid: u32,
    /// Executable name, without the path. Empty for the system sounds session.
    pub name: String,
}

/// Applications with an audio session on the default playback device.
///
/// What an "exclude this app" picker offers. Deliberately only what is *making
/// sound now* rather than every running process: a list of two hundred
/// processes is not a choice anyone can make, and an app that is silent is not
/// one the user is trying to keep out of the stream.
///
/// Returns an empty list rather than failing — a picker with nothing in it is a
/// fine outcome, and the caller has no better answer to a COM error than the
/// user does.
pub fn audio_sessions() -> Vec<AudioSession> {
    // SAFETY: ordinary WASAPI session enumeration; every pointer is checked and
    // nothing outlives this function.
    unsafe {
        let Ok(enumerator) = ({
            let r: windows::core::Result<IMMDeviceEnumerator> =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL);
            r
        }) else {
            return Vec::new();
        };
        let Ok(device) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else {
            return Vec::new();
        };
        let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
            return Vec::new();
        };
        let Ok(sessions) = manager.GetSessionEnumerator() else {
            return Vec::new();
        };
        let count = sessions.GetCount().unwrap_or(0);

        let mut found: Vec<AudioSession> = Vec::new();
        for i in 0..count {
            let Ok(control) = sessions.GetSession(i) else { continue };
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else { continue };
            let Ok(pid) = control2.GetProcessId() else { continue };
            // Pid 0 is the system sounds session, which nobody wants to exclude
            // and which cannot be targeted by a process tree anyway.
            if pid == 0 {
                continue;
            }
            // One entry per process, however many streams it has open.
            if found.iter().any(|s| s.pid == pid) {
                continue;
            }
            found.push(AudioSession { pid, name: process_name(pid) });
        }
        found.sort_by_key(|s| s.name.to_lowercase());
        found
    }
}

/// Executable name for a process id, or an empty string.
fn process_name(pid: u32) -> String {
    use windows::Win32::Foundation::CloseHandle as Close;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: the handle is closed on every path, and the buffer is passed with
    // its own length.
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buffer = [0u16; 260];
        let mut len = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = Close(handle);
        if !ok {
            return String::new();
        }
        String::from_utf16_lossy(&buffer[..len as usize])
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or_default()
            .to_string()
    }
}

/// A `PROPVARIANT` holding a blob, which is how activation parameters travel.
///
/// # Why the layout is spelled out
///
/// `windows_core::PROPVARIANT` keeps its representation private and offers no
/// constructor for `VT_BLOB`, so there is no way to build one through the safe
/// API. The layout below is the Win32 ABI, which has been fixed for decades —
/// and the assertion under it fails the build rather than corrupting a call if
/// that ever stops being true.
#[repr(C)]
struct PropVariantBlob {
    vt: u16,
    reserved: [u16; 3],
    cb_size: u32,
    _pad: u32,
    blob: *mut u8,
}

/// `VT_BLOB`, from the VARENUM enumeration.
const VT_BLOB: u16 = 65;

// A layout that disagrees with the real type would be read as a different
// variant entirely, which is a wrong answer rather than a crash.
const _: () = assert!(
    std::mem::size_of::<PropVariantBlob>() == std::mem::size_of::<windows_core::PROPVARIANT>(),
    "PROPVARIANT layout has changed; see PropVariantBlob"
);

/// Signals a std event when the asynchronous activation finishes.
///
/// `ActivateAudioInterfaceAsync` reports completion on a callback rather than
/// returning, so getting a client out of it means implementing a COM interface.
/// The handler carries no result: the operation object holds it, and the caller
/// reads it once this has fired.
#[windows::core::implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    done: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        _operation: Option<&IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let (lock, cv) = &*self.done;
        *lock.lock().unwrap() = true;
        cv.notify_all();
        Ok(())
    }
}

/// Open a loopback client that leaves one process tree out of the mix.
///
/// The exclusion happens inside the audio engine, so what comes back is the
/// system mix with that tree already absent — sample accurate, with no residue.
fn open_process_loopback(
    target_pid: u32,
    include: bool,
    sample_rate: u32,
    channels: u16,
) -> Result<(IAudioClient, IAudioCaptureClient, AudioFormat, HANDLE)> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: target_pid,
                ProcessLoopbackMode: if include {
                    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
                } else {
                    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
                },
            },
        },
    };
    let mut variant = PropVariantBlob {
        vt: VT_BLOB,
        reserved: [0; 3],
        cb_size: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        _pad: 0,
        blob: std::ptr::addr_of!(params) as *mut u8,
    };

    let done = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let handler: IActivateAudioInterfaceCompletionHandler =
        ActivationHandler { done: std::sync::Arc::clone(&done) }.into();

    // SAFETY: `params` and `variant` outlive the call — the activation reads
    // them synchronously, before it returns the operation object. The pointer
    // cast is checked by the size assertion above.
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(std::ptr::addr_of_mut!(variant) as *const windows_core::PROPVARIANT),
            &handler,
        )
    }
    .context("could not start per-process audio activation")?;

    // Wait for the callback. Bounded, because a hang here would take the
    // session's audio thread with it.
    {
        let (lock, cv) = &*done;
        let mut finished = lock.lock().unwrap();
        while !*finished {
            let (guard, timeout) = cv
                .wait_timeout(finished, std::time::Duration::from_secs(3))
                .unwrap();
            finished = guard;
            if timeout.timed_out() && !*finished {
                anyhow::bail!("per-process audio activation did not complete");
            }
        }
    }

    let mut activate_result = windows::core::HRESULT(0);
    let mut unknown: Option<windows::core::IUnknown> = None;
    // SAFETY: the activation completed, so the operation holds a result.
    unsafe { operation.GetActivateResult(&mut activate_result, &mut unknown) }
        .context("could not read the activation result")?;
    activate_result.ok().context("the audio engine refused a per-process client")?;
    let client: IAudioClient = unknown
        .context("activation returned no client")?
        .cast()
        .context("activation did not return an IAudioClient")?;

    // # The format is stated, not queried
    //
    // A process loopback client has no endpoint behind it and no mix format to
    // ask for — `GetMixFormat` fails. The engine converts into whatever is
    // asked for here, and 32-bit float is what the rest of this pipeline
    // already speaks.
    let mut format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: channels,
        nSamplesPerSec: sample_rate,
        nAvgBytesPerSec: sample_rate * channels as u32 * 4,
        nBlockAlign: channels * 4,
        wBitsPerSample: 32,
        cbSize: 0,
    };

    // SAFETY: the format outlives Initialize, which copies what it needs.
    unsafe {
        let event = CreateEventW(None, false, false, None)
            .context("could not create the audio capture event")?;
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                BUFFER_DURATION,
                0,
                std::ptr::addr_of_mut!(format),
                None,
            )
            .context("the audio engine refused a per-process loopback client")?;
        client
            .SetEventHandle(event)
            .context("could not attach the event to the per-process client")?;
        let capture: IAudioCaptureClient = client
            .GetService()
            .context("could not get a capture client for the per-process stream")?;
        client.Start().context("could not start the per-process stream")?;
        Ok((client, capture, AudioFormat { sample_rate, channels }, event))
    }
}

/// Capture several applications separately and sum them.
///
/// The only way to keep more than one application out of the stream: the
/// exclusion API takes a single process id, and two exclude-mode streams cannot
/// be composed because each already contains the other's target.
///
/// Every stream here is the audio engine's own per-process output, so summing
/// them is addition of signals that were never mixed together — not a
/// subtraction that has to cancel.
///
/// # Keeping up with applications
///
/// The set of applications is rescanned, because it is not fixed: opening a
/// browser after the session started and hearing nothing from it was the
/// reported symptom of treating it as though it were.
///
/// # Why the scan is not done here
///
/// It was, once a second, on this thread — and this thread has to hand the
/// encoder a continuous stream of samples. Enumerating audio sessions is a COM
/// round trip that also opens a handle to every process it finds to read its
/// name, and doing that between two blocks of audio produced a steady stutter on
/// a one-second cadence. Reported as exactly that.
///
/// So a helper thread does the enumerating and publishes a list of process ids.
/// Reading that list is a lock and a clone of a handful of integers, and the
/// expensive part — opening or closing a capture stream — only happens on the
/// rare occasion the set has actually changed.
fn run_mixed(
    excluded: &[u32],
    ready: &std::sync::mpsc::Sender<Result<AudioFormat>>,
    chunks: &std::sync::mpsc::SyncSender<AudioChunk>,
    stop: &AtomicBool,
) {
    const RATE: u32 = 48_000;
    const CHANNELS: u16 = 2;
    let format = AudioFormat { sample_rate: RATE, channels: CHANNELS };

    // Started before the first scan, so the audio thread never enumerates.
    let watcher = SessionWatcher::start(excluded.to_vec());
    let mut streams: Vec<MixedStream> = Vec::new();
    rescan(&watcher.playing(), &mut streams, RATE, CHANNELS);
    if streams.is_empty() {
        let _ = ready.send(Err(anyhow::anyhow!("no application could be captured")));
        return;
    }
    if ready.send(Ok(format)).is_err() {
        return;
    }

    let channels = CHANNELS as usize;
    let mut position = 0u64;
    let mut mixed: Vec<f32> = Vec::new();
    let mut last_scan = std::time::Instant::now();

    while !stop.load(Ordering::Relaxed) {
        match streams.first() {
            // Wait on the first stream's event; they all run on the same engine
            // period, so one waking means the others have data too.
            // SAFETY: waiting on an event this thread owns.
            Some(first) => unsafe {
                WaitForSingleObject(first.event, WAIT_TIMEOUT_MS);
            },
            // Every application that was playing has closed. Keep the timeline
            // running rather than spinning; the rescan below will find the next
            // one that starts.
            None => std::thread::sleep(std::time::Duration::from_millis(WAIT_TIMEOUT_MS as u64)),
        }

        if last_scan.elapsed() >= RESCAN {
            // Cheap: the list was built on another thread. This only acts when
            // it differs from the streams already open.
            rescan(&watcher.playing(), &mut streams, RATE, CHANNELS);
            last_scan = std::time::Instant::now();
        }

        for stream in streams.iter_mut() {
            drain_into(&stream.capture, channels, &mut stream.pending);
            // Bounded, so one stuck stream cannot grow without limit and cannot
            // come back so far behind that its audio arrives as an echo.
            let cap = format.frames(MAX_BACKLOG) * channels;
            if stream.pending.len() > cap {
                let excess = stream.pending.len() - cap;
                stream.pending.drain(..excess);
            }
        }

        // What every stream *that has anything* can contribute.
        //
        // Deliberately not the minimum across all of them. A stream with an
        // empty queue is an application that is silent, or one that has exited
        // and will never deliver again — and taking the minimum over those
        // pinned it at zero, so a single departing application turned the whole
        // mix into permanent silence. A stream with nothing to add simply does
        // not add anything; it cannot fall out of step by being skipped,
        // because it has no samples to be skipped over.
        let ready_samples =
            streams.iter().map(|s| s.pending.len()).filter(|len| *len > 0).min().unwrap_or(0);
        let whole = ready_samples - ready_samples % channels;
        if whole == 0 {
            // Nothing at all yet. Emit silence so the timeline stays continuous
            // rather than stalling the encoder - see the module header.
            let frames = format.frames(std::time::Duration::from_millis(WAIT_TIMEOUT_MS as u64));
            let chunk =
                AudioChunk { samples: vec![0.0; frames * channels], position, silent: true };
            position += frames as u64;
            let _ = chunks.try_send(chunk);
            continue;
        }

        mixed.clear();
        mixed.resize(whole, 0.0);
        for stream in streams.iter_mut() {
            for slot in mixed.iter_mut() {
                if let Some(sample) = stream.pending.pop_front() {
                    *slot += sample;
                }
            }
        }
        // Summing independent streams can exceed full scale. Clamping is what
        // the engine's own mixer does, and wrapping would be an audible crack.
        for sample in mixed.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }

        let silent = mixed.iter().all(|s| *s == 0.0);
        let chunk = AudioChunk { samples: mixed.clone(), position, silent };
        position += (whole / channels) as u64;
        let _ = chunks.try_send(chunk);
    }
}

/// One application's contribution to the mix.
struct MixedStream {
    pid: u32,
    /// Held because dropping it stops the stream; nothing else reads it.
    _client: IAudioClient,
    capture: IAudioCaptureClient,
    event: HANDLE,
    /// Samples captured but not yet mixed. The streams do not deliver in
    /// lockstep, so each needs its own.
    pending: std::collections::VecDeque<f32>,
}

impl Drop for MixedStream {
    fn drop(&mut self) {
        // SAFETY: each was started in `rescan` and is stopped exactly once, when
        // the stream leaves the mix — which is also what makes an application
        // closing, or the whole capture ending, release its client.
        unsafe {
            let _ = self._client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

/// How often the mix acts on the watcher's latest list.
///
/// Cheap now that the list arrives ready-made, so this is only about how quickly
/// a newly launched application is heard.
const RESCAN: std::time::Duration = std::time::Duration::from_secs(1);

/// Enumerates audio sessions on its own thread and publishes what it finds.
///
/// Exists so the audio thread never makes a COM call. See [`run_mixed`].
struct SessionWatcher {
    playing: Arc<std::sync::Mutex<Vec<u32>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SessionWatcher {
    fn start(excluded: Vec<u32>) -> Self {
        let playing = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = std::thread::Builder::new()
            .name("audio-sessions".into())
            .spawn({
                let playing = Arc::clone(&playing);
                let stop = Arc::clone(&stop);
                move || {
                    // SAFETY: paired with CoUninitialize below; every COM object
                    // this thread creates stays on it.
                    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                    if com.is_err() {
                        warn!("Audio session watcher could not start COM — the mix \
                               will not pick up new applications");
                        return;
                    }
                    while !stop.load(Ordering::Relaxed) {
                        let found: Vec<u32> = audio_sessions()
                            .into_iter()
                            .map(|session| session.pid)
                            .filter(|pid| !excluded.contains(pid))
                            .collect();
                        // An empty result is a COM failure as often as it is an
                        // idle machine, and acting on it would close every
                        // stream. The consumer decides; this only reports.
                        *playing.lock().unwrap() = found;
                        std::thread::sleep(RESCAN);
                    }
                    // SAFETY: balances the CoInitializeEx above.
                    unsafe { CoUninitialize() };
                }
            })
            .ok();

        // The first list is worth waiting a moment for: without it the mix opens
        // with nothing and the first second of a session is silent.
        let watcher = Self { playing, stop, thread };
        for _ in 0..20 {
            if !watcher.playing().is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        watcher
    }

    fn playing(&self) -> Vec<u32> {
        self.playing.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for SessionWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// How far behind a single stream is allowed to fall before its backlog is
/// trimmed.
const MAX_BACKLOG: std::time::Duration = std::time::Duration::from_millis(200);

/// Bring the set of captured applications up to date.
///
/// Adds anything newly making sound that is not excluded, and drops streams
/// whose application has gone. Failing to open one is not an error worth
/// stopping for: it has usually just exited between the enumeration and here.
fn rescan(playing: &[u32], streams: &mut Vec<MixedStream>, rate: u32, channels: u16) {
    // An empty enumeration is not evidence that everything closed — a COM
    // failure here returns the same thing, and acting on it would stop every
    // stream and restart them a second later as an audible gap.
    if !playing.is_empty() {
        // Dropping the stream stops the client — see `MixedStream::drop`.
        streams.retain(|stream| playing.contains(&stream.pid));
    }

    for &pid in playing {
        if streams.iter().any(|stream| stream.pid == pid) {
            continue;
        }
        match open_process_loopback(pid, true, rate, channels) {
            Ok((client, capture, _, event)) => {
                debug!("Adding process {pid} to the audio mix");
                streams.push(MixedStream {
                    pid,
                    _client: client,
                    capture,
                    event,
                    pending: std::collections::VecDeque::new(),
                });
            }
            Err(e) => debug!("Skipping process {pid} in the audio mix: {e:#}"),
        }
    }
}

/// Move everything a capture client has ready into `out`.
fn drain_into(
    capture: &IAudioCaptureClient,
    channels: usize,
    out: &mut std::collections::VecDeque<f32>,
) {
    loop {
        // SAFETY: the standard capture loop; every GetBuffer is released.
        let available = match unsafe { capture.GetNextPacketSize() } {
            Ok(frames) if frames > 0 => frames,
            _ => return,
        };
        let _ = available;

        let mut data = std::ptr::null_mut();
        let mut frames = 0u32;
        let mut flags = 0u32;
        // SAFETY: out-params are valid; `data` is read only on success.
        if unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }.is_err() {
            return;
        }
        let count = frames as usize * channels;
        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
            out.extend(std::iter::repeat_n(0.0, count));
        } else {
            // SAFETY: the engine guarantees `count` floats at `data`, valid
            // until ReleaseBuffer below.
            let samples = unsafe { std::slice::from_raw_parts(data as *const f32, count) };
            out.extend(samples.iter().copied());
        }
        // SAFETY: balances the GetBuffer above, exactly once.
        if unsafe { capture.ReleaseBuffer(frames) }.is_err() {
            return;
        }
    }
}

/// How long to wait for the engine before manufacturing silence.
///
/// Comfortably longer than any sane device period, so a normally-running stream
/// is always event-driven and this only fires when nothing is playing.
const WAIT_TIMEOUT_MS: u32 = 40;

/// Capture buffer the engine is asked for, in 100-nanosecond units.
///
/// 20 ms. Larger than the block size that gets encoded, because this is only the
/// headroom before the engine overwrites unread data, and a thread that is
/// briefly descheduled should lose nothing.
const BUFFER_DURATION: i64 = 20 * 10_000;

fn capture_thread(
    exclude: Excluding,
    ready: std::sync::mpsc::Sender<Result<AudioFormat>>,
    chunks: std::sync::mpsc::SyncSender<AudioChunk>,
    stop: Arc<AtomicBool>,
) {
    // SAFETY: paired with CoUninitialize at the end of this function, and every
    // COM object created here is confined to this thread.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com.is_err() {
        let _ = ready.send(Err(anyhow::anyhow!("CoInitializeEx failed: {com:?}")));
        return;
    }

    // Per-process capture has a fixed format because there is no endpoint to
    // ask; 48 kHz stereo is what Opus wants anyway.
    // Mixing several included applications is a different loop entirely; it
    // owns N clients rather than one and never reaches the code below.
    if let Excluding::AllBut(pids) = &exclude {
        run_mixed(pids, &ready, &chunks, &stop);
        // SAFETY: balances the CoInitializeEx above.
        unsafe { CoUninitialize() };
        return;
    }

    let opened = match exclude {
        Excluding::One(pid) => open_process_loopback(pid, false, 48_000, 2),
        _ => open_loopback(),
    };
    match opened {
        Ok((client, capture, format, event)) => {
            if ready.send(Ok(format)).is_err() {
                // Nobody is waiting for this any more.
                // SAFETY: balances the CoInitializeEx above.
                unsafe { CoUninitialize() };
                return;
            }
            run(&client, &capture, format, event, &chunks, &stop);
            // SAFETY: the event was created here and is not used again.
            unsafe {
                let _ = client.Stop();
                let _ = CloseHandle(event);
            }
        }
        Err(e) => {
            let _ = ready.send(Err(e));
        }
    }

    // SAFETY: balances the CoInitializeEx above.
    unsafe { CoUninitialize() };
}

/// Open the default render endpoint for loopback capture.
fn open_loopback() -> Result<(IAudioClient, IAudioCaptureClient, AudioFormat, HANDLE)> {
    // SAFETY: standard WASAPI activation. Every pointer the API hands back is
    // checked, and the mix format is freed before returning.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("could not create the audio device enumerator")?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("no default playback device to capture from")?;
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .context("could not activate an audio client on the playback device")?;

        let mix = client.GetMixFormat().context("could not read the endpoint mix format")?;
        anyhow::ensure!(!mix.is_null(), "GetMixFormat returned nothing");
        let format = read_format(mix);

        // The event has to exist before Initialize, which stores it.
        let event = CreateEventW(None, false, false, None)
            .context("could not create the audio capture event")?;

        let result = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            // Loopback captures the render stream; the event makes the wait
            // below cheap instead of a poll.
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            BUFFER_DURATION,
            0,
            mix,
            None,
        );
        // Owned by us the moment GetMixFormat returned, and not referenced
        // after Initialize has copied what it needs.
        CoTaskMemFree(Some(mix as *const _));
        result.context("the audio engine refused a loopback client")?;

        client
            .SetEventHandle(event)
            .context("could not attach the event to the audio client")?;
        let capture: IAudioCaptureClient = client
            .GetService()
            .context("could not get a capture client for the loopback stream")?;
        client.Start().context("could not start the loopback stream")?;

        let format = format?;
        Ok((client, capture, format, event))
    }
}

/// Read the parts of a `WAVEFORMATEX` this cares about.
///
/// # Safety
/// `mix` must be a valid pointer from `GetMixFormat`.
unsafe fn read_format(mix: *const WAVEFORMATEX) -> Result<AudioFormat> {
    // SAFETY: the caller guarantees the pointer.
    let header = unsafe { *mix };

    // Copied out field by field: `WAVEFORMATEX` is `#[repr(packed)]`, and even
    // taking a reference to a field of one — which `format!` does implicitly —
    // is undefined behaviour rather than merely slow.
    let tag = header.wFormatTag;
    let bits = header.wBitsPerSample;
    let channels = header.nChannels;
    let sample_rate = header.nSamplesPerSec;

    // Shared mode always hands back float32, but as WAVE_FORMAT_EXTENSIBLE
    // rather than as the plain tag — checking only the tag reads every normal
    // endpoint as "not float" and rejects it.
    let is_float = if tag == WAVE_FORMAT_EXTENSIBLE {
        // SAFETY: an EXTENSIBLE header is the first field of the larger struct,
        // and the caller guarantees the pointer came from GetMixFormat, which
        // allocates the larger one whenever it sets this tag.
        let extended = unsafe { *(mix as *const WAVEFORMATEXTENSIBLE) };
        let subformat = extended.SubFormat;
        subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    } else {
        tag == WAVE_FORMAT_IEEE_FLOAT
    };
    anyhow::ensure!(
        is_float && bits == 32,
        "endpoint mix format is {bits} bit, tag {tag:#06x} — expected 32-bit float"
    );
    anyhow::ensure!(channels > 0, "endpoint reports no channels");

    Ok(AudioFormat { sample_rate, channels })
}

/// Format tags, spelled out rather than pulled in.
///
/// windows-rs files these under `Win32_Media_KernelStreaming` and
/// `Win32_Media_Multimedia`, and two whole feature modules for two integers is a
/// poor trade against a build that is already slow.
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;

/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`, which windows-rs does not export.
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

fn run(
    client: &IAudioClient,
    capture: &IAudioCaptureClient,
    format: AudioFormat,
    event: HANDLE,
    chunks: &std::sync::mpsc::SyncSender<AudioChunk>,
    stop: &AtomicBool,
) {
    let channels = format.channels as usize;
    let mut position = 0u64;
    let mut dropped = 0u32;
    let mut reported = std::time::Instant::now();

    while !stop.load(Ordering::Relaxed) {
        // SAFETY: waiting on an event this thread owns.
        let signalled = unsafe { WaitForSingleObject(event, WAIT_TIMEOUT_MS) } == WAIT_OBJECT_0;

        if !signalled {
            // Nothing is playing, and on some Windows versions that means no
            // buffers at all rather than silent ones. Manufacture the silence
            // so the stream stays gapless; see the module header.
            let frames = format.frames(std::time::Duration::from_millis(WAIT_TIMEOUT_MS as u64));
            let chunk = AudioChunk {
                samples: vec![0.0; frames * channels],
                position,
                silent: true,
            };
            position += frames as u64;
            if chunks.try_send(chunk).is_err() {
                dropped += 1;
            }
            continue;
        }

        loop {
            // SAFETY: the standard capture loop. Every successful GetBuffer is
            // matched by exactly one ReleaseBuffer on both paths below.
            let available = match unsafe { capture.GetNextPacketSize() } {
                Ok(frames) if frames > 0 => frames,
                Ok(_) => break,
                Err(e) => {
                    warn!("Audio capture stopped: {e}");
                    return;
                }
            };

            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            // SAFETY: out-params are valid; `data` is only read when the call
            // succeeds and `frames` says how much of it there is.
            let got = unsafe {
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)
            };
            if let Err(e) = got {
                warn!("Audio capture stopped: {e}");
                return;
            }
            let _ = available;

            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                // The engine dropped audio before it reached us. Worth a line,
                // because it points at the machine being overloaded rather than
                // at anything this code can fix.
                debug!("Audio capture reported a discontinuity");
            }

            let count = frames as usize * channels;
            let samples = if silent || data.is_null() {
                // A silent buffer's memory is documented as not worth reading.
                vec![0.0; count]
            } else {
                // SAFETY: the engine guarantees `frames * channels` floats at
                // `data`, valid until ReleaseBuffer below.
                unsafe { std::slice::from_raw_parts(data as *const f32, count) }.to_vec()
            };

            // SAFETY: balances the GetBuffer above, exactly once.
            if let Err(e) = unsafe { capture.ReleaseBuffer(frames) } {
                warn!("Audio capture could not release a buffer: {e}");
                return;
            }

            let chunk = AudioChunk { samples, position, silent };
            position += frames as u64;
            if chunks.try_send(chunk).is_err() {
                dropped += 1;
            }
        }

        if dropped > 0 && reported.elapsed() >= std::time::Duration::from_secs(5) {
            warn!("Audio: dropped {dropped} blocks in 5s — the encoder is not keeping up");
            dropped = 0;
            reported = std::time::Instant::now();
        }
    }
    let _ = client;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_counts_follow_the_sample_rate() {
        let format = AudioFormat { sample_rate: 48_000, channels: 2 };
        assert_eq!(format.frames(std::time::Duration::from_millis(10)), 480);
        assert_eq!(format.frames(std::time::Duration::from_millis(1000)), 48_000);

        let odd = AudioFormat { sample_rate: 44_100, channels: 2 };
        assert_eq!(odd.frames(std::time::Duration::from_millis(10)), 441);
    }

    /// What the exclusion picker will actually offer.
    #[test]
    #[ignore = "diagnostic — run explicitly"]
    fn list_what_is_playing_audio_right_now() {
        // SAFETY: COM has to be up for the enumeration; this is a test thread.
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            );
        }
        let sessions = audio_sessions();
        println!("{} applications with an audio session:", sessions.len());
        for session in &sessions {
            println!("  {:>8}  {}", session.pid, session.name);
        }
        // Every entry must be usable as an exclusion target.
        for session in &sessions {
            assert!(session.pid != 0, "pid 0 cannot be excluded and must be filtered");
        }
    }

    /// The Win32 ABI this hand-declares must still match the real type.
    #[test]
    fn the_propvariant_layout_matches_the_one_windows_uses() {
        // The const assertion in the module already fails the build on a
        // mismatch; this states the same fact where a reader will look for it.
        assert_eq!(
            std::mem::size_of::<PropVariantBlob>(),
            std::mem::size_of::<windows_core::PROPVARIANT>()
        );
        assert_eq!(std::mem::align_of::<PropVariantBlob>(), 8, "x64 PROPVARIANT alignment");
    }

    /// Per-process capture must open, and must exclude what it was told to.
    ///
    /// Run with something playing audio to make it meaningful. The point is
    /// that a client comes back at all — the exclusion itself is the audio
    /// engine's work, not ours, and cannot be verified from inside this process
    /// without a second application to be excluded.
    #[test]
    #[ignore = "touches the real audio engine"]
    fn a_per_process_capture_opens_and_produces_a_gapless_stream() {
        // Excluding this very process: it produces no audio, so the stream
        // should look the same as an ordinary loopback. What is being checked
        // is that the activation path works end to end.
        let pid = std::process::id();
        let Ok((capture, chunks)) = LoopbackCapture::start_excluding(pid) else {
            eprintln!("per-process capture unavailable on this machine — skipping");
            return;
        };
        let format = capture.format();
        assert_eq!(format.sample_rate, 48_000);
        assert_eq!(format.channels, 2);

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
        let mut next_position = 0u64;
        let mut blocks = 0u32;
        let mut frames = 0u64;
        while std::time::Instant::now() < deadline {
            let Ok(chunk) = chunks.recv_timeout(std::time::Duration::from_millis(200)) else {
                break;
            };
            assert_eq!(chunk.position, next_position, "gap in the capture position");
            let count = chunk.samples.len() / format.channels as usize;
            next_position += count as u64;
            frames += count as u64;
            blocks += 1;
        }
        assert!(blocks > 0, "no audio blocks from the per-process client");
        println!("{blocks} blocks, {frames} frames excluding pid {pid}");
    }

    /// Captures real system audio. Passes whether or not anything is playing —
    /// the point is that the stream is continuous either way.
    #[test]
    #[ignore = "touches the real default playback device"]
    fn the_stream_is_gapless_whether_or_not_anything_is_playing() {
        let Ok((capture, chunks)) = LoopbackCapture::start() else {
            eprintln!("no playback device — skipping");
            return;
        };
        let format = capture.format();
        assert!(format.sample_rate >= 8_000, "implausible rate {}", format.sample_rate);
        assert!(format.channels >= 1);

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
        let mut frames = 0u64;
        let mut blocks = 0u32;
        let mut audible = 0u32;
        let mut next_position = 0u64;

        while std::time::Instant::now() < deadline {
            let Ok(chunk) = chunks.recv_timeout(std::time::Duration::from_millis(200)) else {
                break;
            };
            // The position must account for every frame, with no holes: a gap
            // here is a click at the far end.
            assert_eq!(chunk.position, next_position, "gap in the capture position");
            let count = chunk.samples.len() / format.channels as usize;
            next_position += count as u64;
            frames += count as u64;
            blocks += 1;
            if chunk.samples.iter().any(|s| s.abs() > 0.0001) {
                audible += 1;
            }
        }

        assert!(blocks > 0, "no audio blocks at all in 600 ms");
        // Roughly half a second of frames, allowing for start-up.
        let expected = format.sample_rate as u64 / 4;
        assert!(
            frames >= expected,
            "only {frames} frames in 600 ms, expected at least {expected}"
        );
        println!(
            "{blocks} blocks, {frames} frames at {} Hz x{}, {audible} with sound",
            format.sample_rate, format.channels
        );
    }
}
