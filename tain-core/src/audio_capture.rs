//! System audio capture via WASAPI loopback.
//!
//! Captures what the machine is playing, on its own thread and its own clock,
//! from the default playback device or the one chosen in settings — and keeps
//! up when either changes or the device goes away. See [`super::audio_device`].
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
//! Always [`STREAM_FORMAT`], whatever the device runs at. The audio engine
//! converts, because it is asked to: see [`open_endpoint_loopback`].
//!
//! It used to be the endpoint's own mix format, read and passed on. That is
//! usually 48 kHz stereo and never safe to assume — plenty of machines run
//! 44.1 kHz, and Opus does not take 44.1 kHz at all, so on those machines the
//! encoder refused to start and every session was silent. And a format that
//! belongs to the device cannot survive changing device: the encoder and the
//! guest's decoder were both built for the first one.

#![cfg(target_os = "windows")]

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::*;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
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

/// What every capture delivers, whatever the device underneath runs at.
///
/// Fixed so that a session can move from one device to another without the
/// stream changing under the encoder and the guest's decoder. 48 kHz because it
/// is Opus's own rate and what almost every device mixes at anyway; stereo
/// because Opus takes no more, and the engine's downmix of a surround device is
/// a better one than taking the front pair.
pub const STREAM_FORMAT: AudioFormat = AudioFormat { sample_rate: 48_000, channels: 2 };

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
    /// The device being captured, while there is one. Always `None` for a
    /// capture that excludes applications, which taps them rather than a device.
    device: Arc<std::sync::Mutex<Option<crate::AudioDevice>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LoopbackCapture {
    /// Start capturing a playback device: the one chosen with
    /// [`super::audio_device::set_capture_device`], or the default.
    ///
    /// Follows the choice, and the default, for as long as it runs. With no
    /// device at all it still starts, and sends silence until one appears —
    /// a machine whose headphones are unplugged at the moment a guest connects
    /// is not a machine without sound for the whole session.
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
        let device = Arc::new(std::sync::Mutex::new(None));
        // The thread takes ownership; this copy is only for the log line below.
        let describe = exclude.clone();
        let thread = std::thread::Builder::new()
            .name("audio-loopback".into())
            .spawn({
                let stop = Arc::clone(&stop);
                let device = Arc::clone(&device);
                move || capture_thread(exclude, ready_tx, chunk_tx, device, stop)
            })
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
        Ok((Self { format, device, stop, thread: Some(thread) }, chunk_rx))
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// The device being captured right now, if any.
    pub fn device(&self) -> Option<crate::AudioDevice> {
        self.device.lock().unwrap_or_else(|e| e.into_inner()).clone()
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

/// One capture, heard by every session that wants the same sound.
///
/// # Why sessions share
///
/// Each session used to open a capture of its own. On a real host, with
/// applications excluded, the second session's capture opened none of the
/// applications it was meant to mix while the first session's was running —
/// so the guest who connected second heard silence for the whole session,
/// three times in a row, and nothing here could say why: Windows only says
/// no. Sharing does not need to know why. The second session hears the capture
/// the first already has working, and the same sound is captured once rather
/// than once per guest.
///
/// # What makes two captures the same
///
/// What they leave out — the process ids a session resolved its exclusions
/// to. Sessions that resolved the same ones share; one that resolved others
/// (an excluded application restarted in between) gets a capture of its own.
/// The device needs no key: every capture follows the one setting.
///
/// The capture stops when the last session listening to it lets go.
pub struct SharedCapture {
    feed: Arc<Feed>,
    chunks: std::sync::mpsc::Receiver<AudioChunk>,
}

/// A capture and everyone listening to it.
struct Feed {
    excluding: Vec<u32>,
    capture: LoopbackCapture,
    listeners: Arc<std::sync::Mutex<Vec<std::sync::mpsc::SyncSender<AudioChunk>>>>,
}

/// Every capture that is running, by what it excludes.
static FEEDS: std::sync::Mutex<Vec<std::sync::Weak<Feed>>> = std::sync::Mutex::new(Vec::new());

impl SharedCapture {
    /// Listen to the capture that leaves out `excluding`, starting it if no
    /// session has.
    ///
    /// Takes what [`LoopbackCapture::start_excluding_many`] takes, and fails
    /// when that does — but only for the first session to ask.
    pub fn join(excluding: &[u32]) -> Result<Self> {
        // Held while a capture starts, so two sessions arriving together get
        // one capture between them rather than one each.
        let mut feeds = FEEDS.lock().unwrap_or_else(|e| e.into_inner());
        feeds.retain(|feed| feed.strong_count() > 0);
        let running = feeds
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .find(|feed| feed.excluding == excluding);
        let feed = match running {
            Some(feed) => {
                info!(
                    "Sharing the audio capture already running for {} other session(s)",
                    Arc::strong_count(&feed) - 1
                );
                feed
            }
            None => {
                let feed = Arc::new(Feed::start(excluding)?);
                feeds.push(Arc::downgrade(&feed));
                feed
            }
        };
        // As deep as a capture's own queue, for the same reason: a listener
        // that falls behind must not become unbounded memory.
        let (tx, chunks) = std::sync::mpsc::sync_channel(200);
        feed.listeners.lock().unwrap_or_else(|e| e.into_inner()).push(tx);
        Ok(Self { feed, chunks })
    }

    pub fn format(&self) -> AudioFormat {
        self.feed.capture.format()
    }

    /// The device being captured right now, if any. See
    /// [`LoopbackCapture::device`].
    pub fn device(&self) -> Option<crate::AudioDevice> {
        self.feed.capture.device()
    }

    /// The captured blocks, in order. Closed when the capture ends for good.
    pub fn chunks(&self) -> &std::sync::mpsc::Receiver<AudioChunk> {
        &self.chunks
    }
}

impl Feed {
    fn start(excluding: &[u32]) -> Result<Self> {
        let (capture, source) = LoopbackCapture::start_excluding_many(excluding)?;
        let listeners: Arc<std::sync::Mutex<Vec<std::sync::mpsc::SyncSender<AudioChunk>>>> =
            Arc::default();
        std::thread::Builder::new()
            .name("audio-share".into())
            .spawn({
                let listeners = Arc::clone(&listeners);
                // Ends with the capture: the last session to let go drops the
                // `Feed`, which stops the capture and closes `source`.
                move || {
                    while let Ok(chunk) = source.recv() {
                        let mut listeners = listeners.lock().unwrap_or_else(|e| e.into_inner());
                        listeners.retain(|listener| match listener.try_send(chunk.clone()) {
                            // A listener that has fallen behind loses this
                            // block, as it would have from a capture of its own.
                            Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => true,
                            // Its session has ended.
                            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
                        });
                    }
                }
            })
            .context("could not start the audio sharing thread")?;
        Ok(Self { excluding: excluding.to_vec(), capture, listeners })
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
///
/// Which is exactly why it makes sure COM is there first. Called from a
/// session's async worker with nothing in the process having started COM yet,
/// it returned an empty list, the session excluded nothing, and the first guest
/// after launch heard the apps it was meant not to. See
/// [`super::audio_device::ensure_mta`].
pub fn audio_sessions() -> Vec<AudioSession> {
    super::audio_device::ensure_mta();
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

/// What to hand [`LoopbackCapture::start_excluding_many`] to leave these
/// processes out of the stream.
///
/// # One application, one exclusion
///
/// An application is often several processes, and the ones with an audio
/// session are usually not the one that started it: Discord's sound comes from
/// two children of its main process. Each used to count as an exclusion of its
/// own, and more than one cannot be done natively — so excluding Discord alone
/// took the path that mixes every *other* application, which leaves Windows'
/// own sounds out and has more to go wrong.
///
/// The native exclusion takes a process *tree*. So when every process here
/// descends from one root of the same name, that root is excluded instead, once,
/// by the audio engine. Anything else comes back as it went in.
pub fn exclusion_targets(pids: &[u32]) -> Vec<u32> {
    if pids.len() < 2 {
        return pids.to_vec();
    }
    let roots = tree_roots(pids, &process_table());
    if roots.len() == 1 { roots } else { pids.to_vec() }
}

/// For each of `pids`, the topmost process it descends from through processes
/// of its own name, each root once.
///
/// # Only through its own name
///
/// A parent id is the number the process was given when it started, and that
/// process may have exited and the number gone to something else. Discord's
/// main process names a parent that no longer exists — its launcher, gone once
/// it had started it. Following a parent id into a different executable could
/// exclude whatever inherited the number, and everything that one ever started.
fn tree_roots(pids: &[u32], table: &std::collections::HashMap<u32, (u32, String)>) -> Vec<u32> {
    let mut roots: Vec<u32> = Vec::new();
    for &pid in pids {
        let mut root = pid;
        if let Some((_, name)) = table.get(&pid) {
            // Bounded, against two reused ids that name each other as parent.
            for _ in 0..64 {
                let Some(&(parent, _)) = table.get(&root) else { break };
                match table.get(&parent) {
                    Some((_, parent_name))
                        if parent != root && parent_name.eq_ignore_ascii_case(name) =>
                    {
                        root = parent
                    }
                    _ => break,
                }
            }
        }
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

/// Every running process's parent and executable name, from one snapshot.
///
/// Empty if the snapshot cannot be taken, which leaves every process its own
/// root — exactly how exclusion worked before roots were looked for.
fn process_table() -> std::collections::HashMap<u32, (u32, String)> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let mut table = std::collections::HashMap::new();
    // SAFETY: the snapshot is closed on the one path that opens it, and every
    // entry is filled in by the API with `dwSize` set as it requires.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return table;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            let name = &entry.szExeFile;
            let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            table.insert(
                entry.th32ProcessID,
                (entry.th32ParentProcessID, String::from_utf16_lossy(&name[..end])),
            );
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    table
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
    const RATE: u32 = STREAM_FORMAT.sample_rate;
    const CHANNELS: u16 = STREAM_FORMAT.channels;
    let format = STREAM_FORMAT;

    // Started before the first scan, so the audio thread never enumerates.
    let watcher = SessionWatcher::start(excluded.to_vec());
    let mut streams: Vec<MixedStream> = Vec::new();
    rescan(&watcher.playing(), &mut streams, RATE, CHANNELS);
    // # Nothing to mix yet is not a failure
    //
    // It used to be: with every application that had a session excluded, or
    // none open, the capture refused to start and the session had no sound for
    // its whole length — while a guest who had connected a moment earlier, or
    // later, heard everything. The loop below already sends silence while no
    // stream has anything, and the rescan adds applications as they start.
    if streams.is_empty() {
        info!("Nothing to mix yet — sending silence until an application plays");
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
    device: Arc<std::sync::Mutex<Option<crate::AudioDevice>>>,
    stop: Arc<AtomicBool>,
) {
    // First, so the CoUninitialize at the end of this thread can never be the
    // one that takes the process's apartment away from a session still using
    // it. See `audio_device::ensure_mta`.
    super::audio_device::ensure_mta();
    // SAFETY: paired with CoUninitialize at the end of this function, and every
    // COM object created here is confined to this thread.
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com.is_err() {
        let _ = ready.send(Err(anyhow::anyhow!("CoInitializeEx failed: {com:?}")));
        return;
    }

    // Each of these owns its clients and reports its own format. Mixing several
    // included applications is a different loop entirely, because it holds N
    // clients rather than one.
    match &exclude {
        Excluding::AllBut(pids) => run_mixed(pids, &ready, &chunks, &stop),
        Excluding::One(pid) => run_excluding_one(*pid, &ready, &chunks, &stop),
        Excluding::Nothing => run_following(&ready, &chunks, &device, &stop),
    }

    // SAFETY: balances the CoInitializeEx above.
    unsafe { CoUninitialize() };
}

/// Everything but one process tree, left out by the audio engine itself.
///
/// Not tied to a playback device — the engine taps the applications' own
/// streams — so there is nothing here to follow when the default changes.
fn run_excluding_one(
    pid: u32,
    ready: &std::sync::mpsc::Sender<Result<AudioFormat>>,
    chunks: &std::sync::mpsc::SyncSender<AudioChunk>,
    stop: &AtomicBool,
) {
    let opened =
        open_process_loopback(pid, false, STREAM_FORMAT.sample_rate, STREAM_FORMAT.channels);
    let (client, capture, format, event) = match opened {
        Ok(opened) => opened,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // Nobody is waiting for this any more if the send fails, and the stream is
    // closed below either way.
    if ready.send(Ok(format)).is_ok() {
        let mut position = 0u64;
        run(&capture, format.channels as usize, event, &mut position, chunks, stop, &|| false);
    }
    // SAFETY: the event was created for this client and is not used again.
    unsafe {
        let _ = client.Stop();
        let _ = CloseHandle(event);
    }
}

/// Capture whichever playback device is right, for as long as the capture runs.
///
/// The one chosen in settings while it is plugged in, the default otherwise —
/// and when the default changes, the device goes away, or the choice is
/// changed, the stream moves. See [`super::audio_device`].
///
/// # One stream, however many devices
///
/// Every move opens a new client, but the guest hears a single stream: the
/// frame position carries on from where the last device left off, and time
/// spent with no device at all is filled with silence. A jump in the position
/// would be concealed as lost packets on the far side, and one that went back
/// to zero would be dropped there as late, for the rest of the session.
fn run_following(
    ready: &std::sync::mpsc::Sender<Result<AudioFormat>>,
    chunks: &std::sync::mpsc::SyncSender<AudioChunk>,
    device: &std::sync::Mutex<Option<crate::AudioDevice>>,
    stop: &AtomicBool,
) {
    use super::audio_device::{resolve, should_move, DeviceWatch, CAPTURE};

    let in_use = |about: Option<crate::AudioDevice>| {
        *device.lock().unwrap_or_else(|e| e.into_inner()) = about;
    };
    let enumerator = match super::audio_device::enumerator() {
        Ok(enumerator) => enumerator,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // Without notifications a lost device is still noticed — the stream fails —
    // but a new default would not be followed until then.
    let watch = DeviceWatch::register(&enumerator)
        .map_err(|e| warn!("Audio capture will not follow device changes: {e:#}"))
        .ok();
    let changed = || watch.as_ref().is_some_and(DeviceWatch::take);

    // Ready after the first attempt to open a device, whichever way it goes:
    // the format does not depend on one, but `LoopbackCapture::device` should
    // be right by the time `start` returns. Answering before the attempt had
    // the session tell its guest "no device" for the few hundred milliseconds
    // the first open takes.
    let mut ready = Some(ready);
    let mut announce = || ready.take().is_none_or(|ready| ready.send(Ok(STREAM_FORMAT)).is_ok());

    let mut position = 0u64;
    let mut said_missing = false;
    while !stop.load(Ordering::Relaxed) {
        let seen = CAPTURE.generation();
        let chosen = CAPTURE.chosen();
        let opened = resolve(&enumerator, chosen.as_deref()).and_then(|(device, about, how)| {
            open_endpoint_loopback(&device, &about.name).map(|opened| (opened, about, how))
        });

        let ((client, capture, channels, event), about, how) = match opened {
            Ok(opened) => opened,
            Err(e) => {
                if !said_missing {
                    info!("No system audio to capture ({e:#}) — sending silence until there is");
                    said_missing = true;
                }
                in_use(None);
                if !announce() {
                    return;
                }
                // Anything happening to any device is worth another look, and
                // so is time passing, in case a notification went astray.
                let retry = || changed() || CAPTURE.generation() != seen;
                fill_silence(&mut position, chunks, stop, RETRY_EVERY, &retry);
                continue;
            }
        };

        info!("Capturing system audio from {} — {}", about.name, how.describe());
        said_missing = false;
        in_use(Some(about.clone()));
        let generation = std::cell::Cell::new(seen);
        let moved = || should_move(&enumerator, changed(), &CAPTURE, &generation, &about.id);
        let opened_at = std::time::Instant::now();
        // Nobody is waiting for this any more if the announcement fails, and
        // the stream is closed below either way.
        let ended = if announce() {
            run(&capture, channels as usize, event, &mut position, chunks, stop, &moved)
        } else {
            Ended::Stopped
        };
        // SAFETY: the event was created for this client and is not used again.
        unsafe {
            let _ = client.Stop();
            let _ = CloseHandle(event);
        }

        match ended {
            Ended::Stopped => break,
            Ended::Moved => {}
            Ended::Lost => {
                info!("Stopped capturing {} — looking for another device", about.name);
                // A device that fails as soon as it opens would otherwise be
                // reopened as fast as the loop can turn.
                if opened_at.elapsed() < std::time::Duration::from_secs(1) {
                    fill_silence(&mut position, chunks, stop, LOST_PAUSE, &|| false);
                }
            }
        }
    }
}

/// How long to send silence before looking for a device again, when there was
/// none and nothing has said one arrived.
const RETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// How long to wait before reopening a device that failed straight away.
const LOST_PAUSE: std::time::Duration = std::time::Duration::from_millis(500);

/// Send silence in real time, until `until` says otherwise or `limit` passes.
///
/// What keeps the guest's timeline moving while there is nothing to capture.
/// Paced by the clock rather than by counting sleeps, so a late wake-up sends
/// more rather than letting the stream fall behind real time.
fn fill_silence(
    position: &mut u64,
    chunks: &std::sync::mpsc::SyncSender<AudioChunk>,
    stop: &AtomicBool,
    limit: std::time::Duration,
    until: &dyn Fn() -> bool,
) {
    let channels = STREAM_FORMAT.channels as usize;
    let started = std::time::Instant::now();
    let mut sent = 0u64;
    while !stop.load(Ordering::Relaxed) && started.elapsed() < limit && !until() {
        std::thread::sleep(std::time::Duration::from_millis(WAIT_TIMEOUT_MS as u64));
        let due = STREAM_FORMAT.frames(started.elapsed()) as u64;
        let frames = due.saturating_sub(sent) as usize;
        if frames == 0 {
            continue;
        }
        let chunk = AudioChunk { samples: vec![0.0; frames * channels], position: *position, silent: true };
        *position += frames as u64;
        sent += frames as u64;
        let _ = chunks.try_send(chunk);
    }
}

/// Open a playback device for loopback capture, delivering [`STREAM_FORMAT`].
///
/// Returns how many channels the client delivers, which is two unless the
/// fallback below was needed.
///
/// # The engine converts, because it is asked to
///
/// `AUTOCONVERTPCM` puts the audio engine's own resampler and channel mixer
/// between the device and this client, so whatever the device runs at, what
/// comes out is 48 kHz stereo. That is what lets a session change device
/// without the stream changing format, and what lets a 44.1 kHz device be heard
/// at all.
///
/// # And when it will not
///
/// Should the engine refuse, the device's own mix format is used instead,
/// provided it already runs at the stream's rate; its channels are folded to
/// stereo on the way out by [`to_stereo`]. A device at any other rate cannot be
/// streamed without a resampler of our own, and is reported rather than sent at
/// the wrong speed.
fn open_endpoint_loopback(
    device: &IMMDevice,
    name: &str,
) -> Result<(IAudioClient, IAudioCaptureClient, u16, HANDLE)> {
    // SAFETY: standard WASAPI activation. Every pointer the API hands back is
    // checked, the mix format is freed before returning, and the event is
    // closed on every path that does not hand it back.
    unsafe {
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .context("could not activate an audio client on the playback device")?;

        let wanted = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: STREAM_FORMAT.channels,
            nSamplesPerSec: STREAM_FORMAT.sample_rate,
            nAvgBytesPerSec: STREAM_FORMAT.sample_rate * STREAM_FORMAT.channels as u32 * 4,
            nBlockAlign: STREAM_FORMAT.channels * 4,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let converted = client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            // Loopback captures the render stream; the event makes the wait
            // in `run` cheap instead of a poll.
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            BUFFER_DURATION,
            0,
            &wanted,
            None,
        );

        let (client, channels) = match converted {
            Ok(()) => (client, STREAM_FORMAT.channels),
            Err(e) => {
                debug!("The audio engine would not convert {name} for capture ({e}) — trying its own format");
                // A client whose Initialize failed is not tried again; a fresh
                // one is the documented way to have another go.
                let client: IAudioClient = device
                    .Activate(CLSCTX_ALL, None)
                    .context("could not activate an audio client on the playback device")?;
                let mix = client.GetMixFormat().context("could not read the device's mix format")?;
                anyhow::ensure!(!mix.is_null(), "GetMixFormat returned nothing");
                let native = read_format(mix);
                let initialized = match &native {
                    Ok(native) if native.sample_rate == STREAM_FORMAT.sample_rate => client
                        .Initialize(
                            AUDCLNT_SHAREMODE_SHARED,
                            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                            BUFFER_DURATION,
                            0,
                            mix,
                            None,
                        )
                        .context("the audio engine refused a loopback client"),
                    Ok(native) => Err(anyhow::anyhow!(
                        "{name} runs at {} Hz, and the audio engine would not convert it",
                        native.sample_rate
                    )),
                    // Reported by `native?` below.
                    Err(_) => Ok(()),
                };
                // Owned by us the moment GetMixFormat returned, and not
                // referenced after Initialize has copied what it needs.
                CoTaskMemFree(Some(mix as *const _));
                let native = native?;
                initialized?;
                (client, native.channels)
            }
        };

        let event = CreateEventW(None, false, false, None)
            .context("could not create the audio capture event")?;
        let started = (|| -> Result<IAudioCaptureClient> {
            client
                .SetEventHandle(event)
                .context("could not attach the event to the audio client")?;
            let capture: IAudioCaptureClient = client
                .GetService()
                .context("could not get a capture client for the loopback stream")?;
            client.Start().context("could not start the loopback stream")?;
            Ok(capture)
        })();
        match started {
            Ok(capture) => Ok((client, capture, channels, event)),
            Err(e) => {
                let _ = CloseHandle(event);
                Err(e)
            }
        }
    }
}

/// Interleaved samples with `from` channels, as stereo.
///
/// Only needed when the engine would not convert — see
/// [`open_endpoint_loopback`]. Mono is doubled; anything wider keeps its front
/// pair, which is where a desktop puts what matters.
fn to_stereo(samples: &[f32], from: usize) -> Vec<f32> {
    match from {
        2 => samples.to_vec(),
        1 => samples.iter().flat_map(|&s| [s, s]).collect(),
        // Zero is refused when the format is read, so this is never `chunks_exact(0)`.
        _ => samples.chunks_exact(from.max(2)).flat_map(|frame| [frame[0], frame[1]]).collect(),
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

/// Why [`run`] returned.
enum Ended {
    /// The capture is being shut down.
    Stopped,
    /// A different device should be captured now.
    Moved,
    /// The device stopped working: unplugged, disabled, or its format changed
    /// under the stream.
    Lost,
}

/// Move a running client's audio into `chunks` until told otherwise.
///
/// `channels` is what the client delivers, and `position` is the stream's,
/// carried in and out so that a stream that moves device keeps its timeline.
/// Whatever comes in, what goes out is [`STREAM_FORMAT`].
fn run(
    capture: &IAudioCaptureClient,
    channels: usize,
    event: HANDLE,
    position: &mut u64,
    chunks: &std::sync::mpsc::SyncSender<AudioChunk>,
    stop: &AtomicBool,
    moved: &dyn Fn() -> bool,
) -> Ended {
    let format = STREAM_FORMAT;
    let out_channels = format.channels as usize;
    let mut dropped = 0u32;
    let mut reported = std::time::Instant::now();

    while !stop.load(Ordering::Relaxed) {
        // Once a period: two atomic loads unless something has happened.
        if moved() {
            return Ended::Moved;
        }

        // SAFETY: waiting on an event this thread owns.
        let signalled = unsafe { WaitForSingleObject(event, WAIT_TIMEOUT_MS) } == WAIT_OBJECT_0;

        // A wait that runs out is not proof that nothing is playing. The event
        // can be late — more so with several clients on one stream, as two
        // guests have — while a packet sits ready behind it.
        // SAFETY: a plain query on a client this thread owns.
        let waiting = if signalled { None } else { Some(unsafe { capture.GetNextPacketSize() }) };
        match waiting {
            // Also what a device that has gone away looks like: no events.
            // Asking is what tells the two apart.
            Some(Err(e)) => {
                info!("Audio capture lost its device: {e}");
                return Ended::Lost;
            }
            // # Sound was there all along
            //
            // Read it, rather than inventing silence in front of it. Both used
            // to go into the stream: forty milliseconds that never happened,
            // then the sound they stood in for. The stream ran ahead of the
            // clock, and the guest's buffer, which only ever sees too much
            // arriving, shed the excess — audible as sound that skipped and
            // crackled, on whichever guest's capture the late events landed.
            Some(Ok(frames)) if frames > 0 => {}
            Some(Ok(_)) => {
                // Nothing is playing, and on some Windows versions that means
                // no buffers at all rather than silent ones. Manufacture the
                // silence so the stream stays gapless; see the module header.
                let frames =
                    format.frames(std::time::Duration::from_millis(WAIT_TIMEOUT_MS as u64));
                let chunk = AudioChunk {
                    samples: vec![0.0; frames * out_channels],
                    position: *position,
                    silent: true,
                };
                *position += frames as u64;
                if chunks.try_send(chunk).is_err() {
                    dropped += 1;
                }
                continue;
            }
            None => {}
        }

        loop {
            // SAFETY: the standard capture loop. Every successful GetBuffer is
            // matched by exactly one ReleaseBuffer on both paths below.
            let available = match unsafe { capture.GetNextPacketSize() } {
                Ok(frames) if frames > 0 => frames,
                Ok(_) => break,
                Err(e) => {
                    // Not a warning any more: the caller finds another device.
                    info!("Audio capture lost its device: {e}");
                    return Ended::Lost;
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
                info!("Audio capture lost its device: {e}");
                return Ended::Lost;
            }
            let _ = available;

            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                // The engine dropped audio before it reached us. Worth a line,
                // because it points at the machine being overloaded rather than
                // at anything this code can fix.
                debug!("Audio capture reported a discontinuity");
            }

            let samples = if silent || data.is_null() {
                // A silent buffer's memory is documented as not worth reading.
                vec![0.0; frames as usize * out_channels]
            } else {
                // SAFETY: the engine guarantees `frames * channels` floats at
                // `data`, valid until ReleaseBuffer below.
                let raw = unsafe {
                    std::slice::from_raw_parts(data as *const f32, frames as usize * channels)
                };
                to_stereo(raw, channels)
            };

            // SAFETY: balances the GetBuffer above, exactly once.
            if let Err(e) = unsafe { capture.ReleaseBuffer(frames) } {
                info!("Audio capture could not release a buffer: {e}");
                return Ended::Lost;
            }

            let chunk = AudioChunk { samples, position: *position, silent };
            *position += frames as u64;
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
    Ended::Stopped
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

    /// Sessions leaving out the same processes hear one capture; a session
    /// leaving out others gets its own; and a capture nobody hears stops.
    ///
    /// The ids are ones no process has, so these keys are this test's alone
    /// even with other tests' sessions sharing captures of their own.
    #[test]
    fn sessions_that_exclude_the_same_processes_share_one_capture() {
        let same = [u32::MAX - 7, u32::MAX - 8];
        let first = SharedCapture::join(&same).expect("the first session");
        let second = SharedCapture::join(&same).expect("the second session");
        assert!(Arc::ptr_eq(&first.feed, &second.feed), "the second session started a capture of its own");

        let other = SharedCapture::join(&[u32::MAX - 9, u32::MAX - 10]).expect("another key");
        assert!(!Arc::ptr_eq(&first.feed, &other.feed), "different exclusions shared a capture");

        // Both hear it, from the same timeline: the second joined a moment
        // after the first, so their blocks are moments apart, not from two
        // counts that each started at zero.
        let a = first.chunks().recv_timeout(std::time::Duration::from_secs(2)).expect("first hears");
        let b = second.chunks().recv_timeout(std::time::Duration::from_secs(2)).expect("second hears");
        assert!(
            a.position.abs_diff(b.position) < 48_000,
            "the two sessions are on different timelines: {} and {}",
            a.position,
            b.position
        );

        let feed = Arc::downgrade(&first.feed);
        drop(first);
        assert!(feed.upgrade().is_some(), "the capture stopped while a session still listened");
        drop(second);
        assert!(feed.upgrade().is_none(), "nobody listens, and the capture is still running");
        drop(other);
    }

    /// A process table as `process_table` would build it, from
    /// `(pid, parent, name)`.
    fn table(rows: &[(u32, u32, &str)]) -> std::collections::HashMap<u32, (u32, String)> {
        rows.iter().map(|&(pid, parent, name)| (pid, (parent, name.to_string()))).collect()
    }

    /// Discord as it actually runs: sound from two children of a main process
    /// whose own parent has exited.
    #[test]
    fn an_applications_processes_share_the_root_they_started_from() {
        let discord = table(&[
            (31932, 31916, "Discord.exe"),
            (31844, 31932, "Discord.exe"),
            (34760, 31932, "Discord.exe"),
            // A grandchild, for depth.
            (40000, 34760, "discord.EXE"),
        ]);
        assert_eq!(tree_roots(&[31844, 34760], &discord), vec![31932]);
        assert_eq!(tree_roots(&[40000, 31844], &discord), vec![31932], "case and depth do not matter");
        assert_eq!(tree_roots(&[31932], &discord), vec![31932], "a root is its own");
    }

    /// The walk stops at a parent of another name, which may be an unrelated
    /// process that inherited a dead parent's id.
    #[test]
    fn a_root_is_never_a_different_application() {
        let rows = table(&[
            (10, 1, "explorer.exe"),
            (20, 10, "Update.exe"),
            (30, 20, "Discord.exe"),
            (31, 30, "Discord.exe"),
        ]);
        assert_eq!(tree_roots(&[31], &rows), vec![30], "not Update.exe, and never explorer.exe");
    }

    #[test]
    fn two_instances_of_an_application_are_two_trees() {
        let rows = table(&[
            (100, 1, "app.exe"),
            (101, 100, "app.exe"),
            (200, 1, "app.exe"),
            (201, 200, "app.exe"),
        ]);
        assert_eq!(tree_roots(&[101, 201], &rows), vec![100, 200]);
    }

    /// Ids reused into each other's parents, or a process missing from the
    /// snapshot because it exited in between: neither may hang or lose a pid.
    #[test]
    fn a_strange_table_still_gives_an_answer() {
        let looped = table(&[(10, 11, "a.exe"), (11, 10, "a.exe")]);
        assert_eq!(tree_roots(&[10], &looped).len(), 1);
        let missing = table(&[]);
        assert_eq!(tree_roots(&[5, 6], &missing), vec![5, 6], "unknown processes are their own roots");
    }

    /// On this machine: an application with several processes making sound is
    /// left out by one native exclusion of its tree, and that opens.
    #[test]
    #[ignore = "touches the real audio engine"]
    fn a_multi_process_application_is_excluded_natively() {
        let sessions = audio_sessions();
        let mut tried = 0;
        let mut names: Vec<&str> = sessions.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        for name in names {
            let pids: Vec<u32> =
                sessions.iter().filter(|s| s.name == name).map(|s| s.pid).collect();
            if pids.len() < 2 {
                continue;
            }
            tried += 1;
            let targets = exclusion_targets(&pids);
            println!("{name}: {pids:?} -> {targets:?}");
            if let [root] = targets[..] {
                let (_capture, chunks) =
                    LoopbackCapture::start_excluding_many(&[root]).expect("native exclusion");
                assert!(chunks.recv_timeout(std::time::Duration::from_millis(500)).is_ok());
            }
        }
        if tried == 0 {
            eprintln!("no application here has several processes making sound — skipping");
        }
    }

    #[test]
    fn whatever_the_device_has_comes_out_as_stereo() {
        assert_eq!(to_stereo(&[0.1, 0.2, 0.3, 0.4], 2), vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(to_stereo(&[0.1, 0.2], 1), vec![0.1, 0.1, 0.2, 0.2], "mono is doubled");
        // 5.1: front left, front right, centre, LFE, back left, back right.
        let surround = [0.1, 0.2, 0.9, 0.9, 0.9, 0.9, 0.3, 0.4, 0.9, 0.9, 0.9, 0.9];
        assert_eq!(to_stereo(&surround, 6), vec![0.1, 0.2, 0.3, 0.4], "the front pair is kept");
    }

    /// The engine converts a loopback stream, which is the whole of how a device
    /// at 44.1 kHz — or anything but 48 — is heard, and how a session keeps one
    /// format across devices.
    #[test]
    #[ignore = "touches the real default playback device"]
    fn the_engine_converts_a_loopback_stream_to_what_it_is_asked_for() {
        // SAFETY: COM has to be up; this is a test thread.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let enumerator = crate::audio_device::enumerator().expect("enumerator");
        let Ok((device, about, _)) = crate::audio_device::resolve(&enumerator, None) else {
            eprintln!("no playback device — skipping");
            return;
        };

        let (client, _capture, channels, event) =
            open_endpoint_loopback(&device, &about.name).expect("loopback on the default device");
        // SAFETY: the client and event were opened just above.
        unsafe {
            let _ = client.Stop();
            let _ = CloseHandle(event);
        }
        assert_eq!(channels, 2, "the engine refused to convert {} and the fallback was used", about.name);

        // And at a rate no device here is likely to be running at, which is
        // the case that actually matters.
        // SAFETY: an ordinary activation and initialise on a fresh client.
        let odd = unsafe {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).expect("activate");
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
                nChannels: 1,
                nSamplesPerSec: 44_100,
                nAvgBytesPerSec: 44_100 * 4,
                nBlockAlign: 4,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                BUFFER_DURATION,
                0,
                &format,
                None,
            )
        };
        assert!(odd.is_ok(), "{} would not convert a loopback stream to 44.1 kHz mono: {odd:?}", about.name);
    }

    /// Choosing another device mid-capture moves the capture, and the timeline
    /// runs straight on — a guest must hear one stream, not two.
    ///
    /// Needs two playback devices, and leaves the system default alone: the
    /// move is made by choosing, which is Tain's own setting.
    #[test]
    #[ignore = "touches the real playback devices"]
    fn choosing_another_device_moves_the_capture_without_a_gap() {
        // The capture thread says which device it opened, and why not.
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        // SAFETY: COM has to be up for the enumeration; this is a test thread.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let devices = crate::audio_device::playback_devices();
        let Some(other) = devices.iter().find(|d| !d.default).cloned() else {
            eprintln!("only one playback device — skipping");
            return;
        };
        crate::audio_device::set_capture_device(None);
        let (capture, chunks) = LoopbackCapture::start().expect("capture");
        assert_eq!(capture.format(), STREAM_FORMAT);

        // Frames received in each phase. Opening the first device takes a few
        // hundred milliseconds, so only what follows a move is held to a rate.
        let mut next_position = 0u64;
        let mut drain = |for_ms: u64| {
            let from = next_position;
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(for_ms);
            while std::time::Instant::now() < deadline {
                let Ok(chunk) = chunks.recv_timeout(std::time::Duration::from_millis(200)) else {
                    continue;
                };
                assert_eq!(chunk.position, next_position, "gap in the capture position");
                assert_eq!(chunk.samples.len() % 2, 0, "not stereo");
                next_position += (chunk.samples.len() / 2) as u64;
            }
            next_position - from
        };

        drain(800);
        let before = capture.device().expect("capturing the default");
        assert!(before.default, "started on {} rather than the default", before.name);

        crate::audio_device::set_capture_device(Some(other.id.clone()));
        let after_move = drain(600);
        let moved = capture.device();
        // Put back before asserting, so a failure does not leave the choice set.
        crate::audio_device::set_capture_device(None);
        let after_return = drain(600);
        let back = capture.device();

        assert_eq!(moved.as_ref().map(|d| &d.id), Some(&other.id), "did not move to {}", other.name);
        assert_eq!(back.map(|d| d.id), Some(before.id), "did not come back to the default");
        // A move costs a reopen, not the stream: most of each phase still arrives.
        let half = STREAM_FORMAT.frames(std::time::Duration::from_millis(300)) as u64;
        assert!(after_move >= half, "only {after_move} frames in the 600 ms after moving");
        assert!(after_return >= half, "only {after_return} frames in the 600 ms after coming back");
        println!("moved {} -> {} -> {}, {next_position} frames", before.name, other.name, before.name);
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

    /// What two captures running at once actually hear, while this process
    /// plays a quiet tone.
    ///
    /// Opening is not hearing: a capture that is fed nothing still produces a
    /// stream, of silence it makes up itself. So this counts the frames that
    /// carry sound, for each kind of capture a session can use, with two of
    /// them open at once as two guests would have.
    #[cfg(db_ffmpeg)]
    #[test]
    #[ignore = "plays a quiet tone on the real default device"]
    fn two_captures_at_once_both_hear_what_is_playing() {
        use crate::audio_codec::{OpusEncoder, DEFAULT_BITRATE, SAMPLE_RATE};
        use crate::audio_output::AudioOutput;
        use crate::encoder::LatencyPriority;

        // A process to exclude that is neither this one nor anything this one
        // descends from — excluding an ancestor's tree would exclude the tone.
        let table = process_table();
        let mut ancestors = vec![std::process::id()];
        while let Some(&(parent, _)) = table.get(ancestors.last().unwrap()) {
            if parent == 0 || ancestors.contains(&parent) {
                break;
            }
            ancestors.push(parent);
        }
        let stranger = *table
            .iter()
            .find(|(pid, (_, name))| {
                name.eq_ignore_ascii_case("svchost.exe") && !ancestors.contains(pid)
            })
            .expect("a process to exclude")
            .0;

        // The tone, fed in real time for as long as the captures listen.
        let stop = Arc::new(AtomicBool::new(false));
        let player = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                let mut encoder = OpusEncoder::new(SAMPLE_RATE, 2, DEFAULT_BITRATE).unwrap();
                let output = AudioOutput::start(SAMPLE_RATE, 2, LatencyPriority::BALANCED).unwrap();
                let mut frame = 0u64;
                let started = std::time::Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    // Paced by the clock with a little lead, not by counting
                    // sleeps: a sleep that overruns would leave gaps in the
                    // tone, and those would be counted against the captures.
                    let due = (started.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as u64 + 2400;
                    while frame < due {
                        let pcm: Vec<f32> = (0..480)
                            .flat_map(|i| {
                                let t = (frame as usize + i) as f32 / SAMPLE_RATE as f32;
                                let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.03;
                                [v, v]
                            })
                            .collect();
                        let blob = encoder.encode(&pcm).unwrap().to_vec();
                        if !blob.is_empty() {
                            output.push(&blob, frame, 480).unwrap();
                        }
                        frame += 480;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(300));

        // Share of each capture's frames, over two seconds, that carry sound —
        // and how many frames it produced per second of wall clock, and how
        // many blocks it made up as silence while the tone never stopped.
        let listen = |pair: [(LoopbackCapture, std::sync::mpsc::Receiver<AudioChunk>); 2]| {
            // Whatever queued while opening is not what is being measured.
            for (_, chunks) in &pair {
                while chunks.try_recv().is_ok() {}
            }
            let started = std::time::Instant::now();
            let deadline = started + std::time::Duration::from_secs(2);
            let mut heard = [(0u64, 0u64, 0u32); 2];
            while std::time::Instant::now() < deadline {
                for (i, (_, chunks)) in pair.iter().enumerate() {
                    while let Ok(chunk) = chunks.try_recv() {
                        if chunk.silent {
                            heard[i].2 += 1;
                        }
                        for frame in chunk.samples.chunks_exact(2) {
                            heard[i].1 += 1;
                            if frame[0].abs() > 1e-3 {
                                heard[i].0 += 1;
                            }
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let seconds = started.elapsed().as_secs_f64();
            heard.map(|(loud, all, made_up)| {
                (loud as f64 / all.max(1) as f64, all as f64 / seconds, made_up)
            })
        };

        let device = listen([LoopbackCapture::start().unwrap(), LoopbackCapture::start().unwrap()]);
        let excluding = listen([
            LoopbackCapture::start_excluding(stranger).unwrap(),
            LoopbackCapture::start_excluding(stranger).unwrap(),
        ]);
        let including = {
            let me = std::process::id();
            let open = || {
                let (client, capture, format, event) =
                    open_process_loopback(me, true, 48_000, 2).expect("include this process");
                (client, capture, format, event)
            };
            // Driven by hand, as `run_mixed` drives them.
            let streams = [open(), open()];
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            let mut heard = [(0u64, 0u64); 2];
            while std::time::Instant::now() < deadline {
                for (i, (_, capture, _, _)) in streams.iter().enumerate() {
                    let mut pending = std::collections::VecDeque::new();
                    drain_into(capture, 2, &mut pending);
                    for pair in pending.make_contiguous().chunks_exact(2) {
                        heard[i].1 += 1;
                        if pair[0].abs() > 1e-3 {
                            heard[i].0 += 1;
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            for (client, _, _, event) in streams {
                // SAFETY: each was opened just above and is closed once.
                unsafe {
                    let _ = client.Stop();
                    let _ = CloseHandle(event);
                }
            }
            heard.map(|(loud, all)| loud as f64 / all.max(1) as f64)
        };

        stop.store(true, Ordering::Relaxed);
        player.join().unwrap();

        println!("(share of frames with sound, frames per second, blocks made up as silence)");
        println!("device loopback, two at once:   {device:.2?}");
        println!("excluding {stranger}, two at once: {excluding:.2?}");
        println!("including this process, two:    {including:.2?}");
        for (what, results) in [("device", device), ("excluding", excluding)] {
            for (i, (share, rate, made_up)) in results.iter().enumerate() {
                assert!(*share > 0.9, "{what} capture {i} heard sound in only {share:.2} of its frames");
                // Silence made up while sound was waiting to be read is time
                // added to the stream that never happened: it runs faster
                // than the clock, and the guest has to throw the excess away.
                assert!(
                    (*rate - 48_000.0).abs() < 48_000.0 * 0.05,
                    "{what} capture {i} produced {rate:.0} frames a second"
                );
                assert_eq!(*made_up, 0, "{what} capture {i} made up silence while sound was playing");
            }
        }
        for (i, share) in including.iter().enumerate() {
            assert!(*share > 0.9, "including capture {i} heard sound in only {share:.2} of its frames");
        }
    }

    /// How fast each kind of capture delivers against the clock, alone and in
    /// pairs, while this process plays a steady tone. Prints, asserts nothing.
    ///
    /// Written while chasing distorted sound with two guests connected. On the
    /// machine it was written on, every kind ran within about half a percent
    /// of 48 000 a second, alone or paired — so a capture falling behind was
    /// not the cause; silence made up in front of waiting sound was.
    #[cfg(db_ffmpeg)]
    #[test]
    #[ignore = "diagnostic — plays a quiet tone on the real default device"]
    fn how_fast_each_kind_of_capture_delivers() {
        use crate::audio_codec::{OpusEncoder, DEFAULT_BITRATE, SAMPLE_RATE};
        use crate::audio_output::AudioOutput;
        use crate::encoder::LatencyPriority;
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();

        let table = process_table();
        let mut ancestors = vec![std::process::id()];
        while let Some(&(parent, _)) = table.get(ancestors.last().unwrap()) {
            if parent == 0 || ancestors.contains(&parent) {
                break;
            }
            ancestors.push(parent);
        }
        let stranger = *table
            .iter()
            .find(|(pid, (_, name))| name.eq_ignore_ascii_case("svchost.exe") && !ancestors.contains(pid))
            .unwrap()
            .0;

        let stop = Arc::new(AtomicBool::new(false));
        let player = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                let mut encoder = OpusEncoder::new(SAMPLE_RATE, 2, DEFAULT_BITRATE).unwrap();
                let output = AudioOutput::start(SAMPLE_RATE, 2, LatencyPriority::BALANCED).unwrap();
                let mut frame = 0u64;
                let started = std::time::Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    let due = (started.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as u64 + 2400;
                    while frame < due {
                        let pcm: Vec<f32> = (0..480)
                            .flat_map(|i| {
                                let t = (frame as usize + i) as f32 / SAMPLE_RATE as f32;
                                let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.03;
                                [v, v]
                            })
                            .collect();
                        let blob = encoder.encode(&pcm).unwrap().to_vec();
                        if !blob.is_empty() {
                            output.push(&blob, frame, 480).unwrap();
                        }
                        frame += 480;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(300));

        // Frames a second for each of several captures listened to at once,
        // over four seconds, all counted the same way.
        let rates = |captures: Vec<(LoopbackCapture, std::sync::mpsc::Receiver<AudioChunk>)>| {
            for (_, chunks) in &captures {
                while chunks.try_recv().is_ok() {}
            }
            let started = std::time::Instant::now();
            let mut frames = vec![0u64; captures.len()];
            while started.elapsed() < std::time::Duration::from_secs(4) {
                for (i, (_, chunks)) in captures.iter().enumerate() {
                    while let Ok(chunk) = chunks.try_recv() {
                        frames[i] += (chunk.samples.len() / 2) as u64;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let seconds = started.elapsed().as_secs_f64();
            frames.iter().map(|f| (*f as f64 / seconds).round()).collect::<Vec<_>>()
        };
        let device = || LoopbackCapture::start().unwrap();
        let excluding = || LoopbackCapture::start_excluding(stranger).unwrap();
        println!("frames a second:");
        println!("  one device capture      {:?}", rates(vec![device()]));
        println!("  two device captures     {:?}", rates(vec![device(), device()]));
        println!("  one excluding capture   {:?}", rates(vec![excluding()]));
        println!("  two excluding captures  {:?}", rates(vec![excluding(), excluding()]));
        stop.store(true, Ordering::Relaxed);
        player.join().unwrap();
    }

    /// Whether a second include-mode capture of *another* application opens
    /// while a first is still running — two guests mixing the same apps.
    #[test]
    #[ignore = "diagnostic — run explicitly, with some other application playing"]
    fn a_second_capture_of_another_application() {
        let me = std::process::id();
        let sessions = audio_sessions();
        let mut tried = 0;
        for session in sessions.iter().filter(|s| s.pid != me) {
            tried += 1;
            let first = open_process_loopback(session.pid, true, 48_000, 2);
            let second = open_process_loopback(session.pid, true, 48_000, 2);
            println!(
                "{} ({}): first {}, second {}",
                session.name,
                session.pid,
                if first.is_ok() { "opened".to_string() } else { format!("{:#}", first.as_ref().err().unwrap()) },
                if second.is_ok() { "opened".to_string() } else { format!("{:#}", second.as_ref().err().unwrap()) },
            );
            for (client, _, _, event) in [first, second].into_iter().flatten() {
                // SAFETY: opened just above, closed once.
                unsafe {
                    let _ = client.Stop();
                    let _ = CloseHandle(event);
                }
            }
        }
        if tried == 0 {
            eprintln!("no other application has an audio session — skipping");
        }
    }

    /// How many include-mode captures one process can hold open at once:
    /// every application with a session, several times over, all kept open.
    #[test]
    #[ignore = "diagnostic — run explicitly"]
    fn how_many_application_captures_one_process_can_hold() {
        let me = std::process::id();
        let pids: Vec<u32> =
            audio_sessions().iter().map(|s| s.pid).filter(|pid| *pid != me).collect();
        let mut open = Vec::new();
        'rounds: for round in 1..=4 {
            for &pid in &pids {
                match open_process_loopback(pid, true, 48_000, 2) {
                    Ok(opened) => open.push(opened),
                    Err(e) => {
                        println!("round {round}: capture {} refused ({pid}): {e:#}", open.len() + 1);
                        break 'rounds;
                    }
                }
            }
            println!("round {round}: {} open", open.len());
        }
        for (client, _, _, event) in open {
            // SAFETY: opened above, closed once.
            unsafe {
                let _ = client.Stop();
                let _ = CloseHandle(event);
            }
        }
    }

    /// Two sessions can each leave the same application out at once.
    ///
    /// Every guest gets a capture of its own, and the second must not be
    /// refused because the first already holds one for the same process.
    #[test]
    #[ignore = "touches the real audio engine"]
    fn two_captures_can_exclude_the_same_process_at_once() {
        let pid = std::process::id();
        let (first, first_chunks) = LoopbackCapture::start_excluding(pid).expect("the first");
        let (second, second_chunks) =
            LoopbackCapture::start_excluding(pid).expect("the second, beside the first");
        for chunks in [&first_chunks, &second_chunks] {
            let got = chunks.recv_timeout(std::time::Duration::from_millis(500));
            assert!(got.is_ok(), "a capture produced nothing");
        }
        drop((first, second));
    }

    /// A mix of the remaining applications starts even with nothing to mix.
    ///
    /// It used to refuse, and the session it belonged to had no sound for its
    /// whole length. Every application with a session right now is excluded,
    /// so there is nothing left to mix — plus two ids no process has, so there
    /// are always several and this is the mix rather than the single exclusion.
    #[test]
    #[ignore = "touches the real audio engine"]
    fn a_mix_starts_with_nothing_to_mix_and_stays_gapless() {
        let mut everything: Vec<u32> = audio_sessions().iter().map(|s| s.pid).collect();
        everything.extend([u32::MAX - 1, u32::MAX - 2]);
        let (capture, chunks) = LoopbackCapture::start_excluding_many(&everything)
            .expect("a mix with nothing in it must still start");
        assert_eq!(capture.format(), STREAM_FORMAT);

        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
        let mut next_position = 0u64;
        let mut frames = 0u64;
        while std::time::Instant::now() < deadline {
            let Ok(chunk) = chunks.recv_timeout(std::time::Duration::from_millis(200)) else {
                continue;
            };
            assert_eq!(chunk.position, next_position, "gap in the mix position");
            next_position += (chunk.samples.len() / 2) as u64;
            frames += (chunk.samples.len() / 2) as u64;
        }
        assert!(frames > 0, "no audio at all from the mix");
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
