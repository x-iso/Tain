//! Which playback device sound is captured from and played on.
//!
//! ## Following the default
//!
//! Both ends used to open the default device once, when sound started, and
//! keep it for the life of the session. Plugging in headphones, or Windows
//! moving the default to a monitor that had just woken up, left the stream on
//! the device the session started with — and a device that went away entirely
//! stopped the stream with a single warning in the log and nothing after it.
//! From the guest's chair every one of those looked the same: no sound, and no
//! way to tell which machine had it wrong.
//!
//! So capture and playback now watch for the default changing and for their own
//! device disappearing, and reopen on whatever is right at that moment. The
//! notifications come from [`DeviceWatch`]; the reopening is each stream's own.
//!
//! ## A choice, and what happens when it is not there
//!
//! Each end can also be told to use one device in particular — see
//! [`set_capture_device`] and [`set_playback_device`]. A chosen device that is
//! unplugged is not a reason to go silent, so [`resolve`] falls back to the
//! default and goes back to the chosen one when it returns. Remembering the
//! choice rather than the device in use is what makes that work.

#![cfg(target_os = "windows")]

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::*;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Media::Audio::{
    eConsole, eRender, EDataFlow, ERole, IMMDevice, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoIncrementMTAUsage, CoTaskMemFree, CLSCTX_ALL, STGM_READ,
};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

use crate::AudioDevice;

/// `PKEY_Device_FriendlyName`, spelled out rather than pulled in.
///
/// windows-rs files it under `Win32_Devices_FunctionDiscovery`, and a whole
/// feature module for one key is a poor trade against a build that is already
/// slow — the same reasoning as the format tags in `audio_capture`.
const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

/// A device the user picked, or none for "whatever the default is".
///
/// Process-wide, like [`super::local_input::set_window`] and for the same
/// reason: this machine has one set of speakers and one answer to which of them
/// to use. Every stream reads it, so a change reaches sessions already running.
pub(crate) struct Preference {
    chosen: Mutex<Option<String>>,
    /// Bumped on every change, so a stream can notice one with an atomic load
    /// instead of taking the lock on every audio period.
    generation: AtomicU64,
}

impl Preference {
    const fn new() -> Self {
        Self { chosen: Mutex::new(None), generation: AtomicU64::new(0) }
    }

    fn set(&self, id: Option<String>) {
        let mut chosen = self.chosen.lock().unwrap_or_else(|e| e.into_inner());
        if *chosen != id {
            *chosen = id;
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn chosen(&self) -> Option<String> {
        self.chosen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }
}

/// The device the host captures from.
pub(crate) static CAPTURE: Preference = Preference::new();
/// The device the guest plays the host's sound on.
pub(crate) static PLAYBACK: Preference = Preference::new();

/// Capture from this device, by id, or from the default with `None`.
///
/// Applies to sessions already running as well as later ones. Ignored while
/// applications are excluded from the stream: that captures applications one by
/// one, not a device — see `audio_capture::LoopbackCapture::start_excluding_many`.
pub fn set_capture_device(id: Option<String>) {
    CAPTURE.set(id);
}

/// Play the host's sound on this device, by id, or on the default with `None`.
///
/// Applies to sessions already playing as well as later ones.
pub fn set_playback_device(id: Option<String>) {
    PLAYBACK.set(id);
}

/// Make sure this process has a multithreaded COM apartment, for good.
///
/// # Why the callers cannot assume one
///
/// The lists here are asked for from the UI thread, which has an apartment of
/// its own, and from whichever async worker a session runs on, which has none.
/// A thread with none can still use COM — through the process's multithreaded
/// apartment — but only once something has created that apartment. Until then
/// every call fails, and these functions turn failure into an empty list: the
/// first session after launch found no applications to exclude and sent the
/// sound of every one of them. Later sessions worked only because the first
/// one's media threads had created the apartment by then.
///
/// # Why not initialise the calling thread and undo it after
///
/// That was the first fix, and it crashed the test suite on every parallel run.
/// When the thread doing it held the process's only claim on the apartment,
/// undoing it tore the apartment down under every thread using COM without
/// having initialised it — Windows.Graphics.Capture's, Media Foundation's —
/// which is an access violation in somebody else's code, some time later.
/// `CoIncrementMTAUsage` exists for exactly this: it keeps the apartment up
/// without touching the calling thread. The claim is never given back.
pub(crate) fn ensure_mta() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: no preconditions. The cookie is dropped without
        // `CoDecrementMTAUsage` on purpose: the apartment is for the life of
        // the process.
        if let Err(e) = unsafe { CoIncrementMTAUsage() } {
            warn!("Could not keep COM's multithreaded apartment up: {e}");
        }
    });
}

/// Every playback device that is plugged in and enabled, the default first.
///
/// What the device pickers offer. Empty rather than failing — the caller has no
/// better answer to a COM error than the user does.
pub fn playback_devices() -> Vec<AudioDevice> {
    ensure_mta();
    // SAFETY: ordinary endpoint enumeration; every pointer is checked and
    // nothing outlives this function.
    unsafe {
        let Ok(enumerator) = enumerator() else { return Vec::new() };
        let default_id = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .ok()
            .and_then(|device| device_id(&device).ok());
        let Ok(collection) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
            return Vec::new();
        };
        let count = collection.GetCount().unwrap_or(0);

        let mut found = Vec::new();
        for i in 0..count {
            let Ok(device) = collection.Item(i) else { continue };
            let Ok(mut about) = describe(&device) else { continue };
            about.default = default_id.as_deref() == Some(about.id.as_str());
            found.push(about);
        }
        // Default first, then as Windows' Sound settings would list them.
        found.sort_by(|a, b| b.default.cmp(&a.default).then_with(|| a.name.cmp(&b.name)));
        found
    }
}

pub(crate) fn enumerator() -> Result<IMMDeviceEnumerator> {
    // SAFETY: creating the enumerator has no preconditions beyond COM being up.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .context("could not create the audio device enumerator")
}

/// The endpoint to open: the chosen one while it is plugged in, otherwise the
/// default.
///
/// The returned description says which of those it was, so the log can say
/// *why* a device is in use — the question a silent session always comes down
/// to.
pub(crate) fn resolve(
    enumerator: &IMMDeviceEnumerator,
    chosen: Option<&str>,
) -> Result<(IMMDevice, AudioDevice, Choice)> {
    if let Some(id) = chosen
        && let Some(device) = active(enumerator, id)
    {
        let about = describe(&device)?;
        return Ok((device, about, Choice::Chosen));
    }
    // SAFETY: an ordinary enumerator call; the result is checked.
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
        .context("there is no playback device")?;
    let mut about = describe(&device)?;
    about.default = true;
    let how = if chosen.is_some() { Choice::ChosenMissing } else { Choice::Default };
    Ok((device, about, how))
}

/// Just the id [`resolve`] would pick, for deciding whether anything changed.
///
/// `None` when there is nothing to pick, which the streams treat as "keep what
/// you have": a device that has really gone announces itself by failing.
pub(crate) fn resolve_id(enumerator: &IMMDeviceEnumerator, chosen: Option<&str>) -> Option<String> {
    if let Some(id) = chosen
        && active(enumerator, id).is_some()
    {
        return Some(id.to_string());
    }
    // SAFETY: an ordinary enumerator call; the result is checked.
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.ok()?;
    device_id(&device).ok()
}

/// Whether a stream on `current` should move to another device.
///
/// `notified` is whether a [`DeviceWatch`] has fired. Cheap when nothing has
/// happened — an atomic load, and the caller's swap — so a stream can ask every
/// period. Only when something has does it ask Windows which device is right,
/// and most of the time the answer is the one already open.
///
/// `seen` is the preference generation the stream has acted on, updated here so
/// a change that turned out not to matter is not looked at again.
pub(crate) fn should_move(
    enumerator: &IMMDeviceEnumerator,
    notified: bool,
    preference: &Preference,
    seen: &std::cell::Cell<u64>,
    current: &str,
) -> bool {
    let generation = preference.generation();
    if !notified && generation == seen.get() {
        return false;
    }
    seen.set(generation);
    resolve_id(enumerator, preference.chosen().as_deref()).is_some_and(|id| id != current)
}

/// How [`resolve`] arrived at a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Choice {
    /// Nothing was chosen; this is the default.
    Default,
    /// This is the device that was chosen.
    Chosen,
    /// One was chosen and is not plugged in, so this is the default instead.
    ChosenMissing,
}

impl Choice {
    /// The end of a log line about the device in use.
    pub(crate) fn describe(self) -> &'static str {
        match self {
            Choice::Default => "the system default",
            Choice::Chosen => "chosen in settings",
            Choice::ChosenMissing => "the system default, because the chosen device is not connected",
        }
    }
}

/// The device with this id, if it is plugged in and enabled.
fn active(enumerator: &IMMDeviceEnumerator, id: &str) -> Option<IMMDevice> {
    // SAFETY: ordinary enumerator and device calls; results are checked.
    unsafe {
        let device = enumerator.GetDevice(&HSTRING::from(id)).ok()?;
        (device.GetState().ok()? == DEVICE_STATE_ACTIVE).then_some(device)
    }
}

fn device_id(device: &IMMDevice) -> Result<String> {
    // SAFETY: GetId allocates the string with CoTaskMemAlloc, and it is freed
    // once copied, on both paths.
    unsafe {
        let raw = device.GetId().context("could not read the device id")?;
        let id = raw.to_string();
        CoTaskMemFree(Some(raw.0 as *const _));
        Ok(id?)
    }
}

/// Id and name, with `default` left false for the caller to fill in.
fn describe(device: &IMMDevice) -> Result<AudioDevice> {
    let id = device_id(device)?;
    // A device with no readable name is still a device. Its id is unreadable
    // to a person, but an empty label in a picker is worse.
    // SAFETY: reading one property from the device's own store.
    let name = unsafe {
        device
            .OpenPropertyStore(STGM_READ)
            .and_then(|store| store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME))
            .map(|value| value.to_string())
            .unwrap_or_default()
    };
    let name = if name.is_empty() { id.clone() } else { name };
    Ok(AudioDevice { id, name, default: false })
}

/// Raised when anything happens that could change which device is right.
///
/// # Why a flag and not a callback that reopens
///
/// Windows calls these from a thread of its own, and documents that they must
/// not block — and reopening a stream is exactly the kind of call that does.
/// So all this does is say that something happened. The stream's own thread
/// notices on its next period and decides with [`resolve_id`] whether it
/// matters, because most of what arrives here does not: a microphone being
/// plugged in, the communications default moving, a device nobody is using
/// changing state.
pub(crate) struct DeviceWatch {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
    changed: Arc<AtomicBool>,
}

impl DeviceWatch {
    /// Register for notifications. Unregistered when dropped.
    pub(crate) fn register(enumerator: &IMMDeviceEnumerator) -> Result<Self> {
        let changed = Arc::new(AtomicBool::new(false));
        let client: IMMNotificationClient = Notifier { changed: Arc::clone(&changed) }.into();
        // SAFETY: the client stays alive in `self` until it is unregistered in
        // `drop`, on this same enumerator.
        unsafe { enumerator.RegisterEndpointNotificationCallback(&client) }
            .context("could not watch for audio device changes")?;
        Ok(Self { enumerator: enumerator.clone(), client, changed })
    }

    /// Whether anything has happened since the last call.
    pub(crate) fn take(&self) -> bool {
        self.changed.swap(false, Ordering::Relaxed)
    }
}

impl Drop for DeviceWatch {
    fn drop(&mut self) {
        // SAFETY: balances the registration in `register`.
        if let Err(e) = unsafe { self.enumerator.UnregisterEndpointNotificationCallback(&self.client) } {
            debug!("Could not stop watching audio devices: {e}");
        }
    }
}

#[windows::core::implement(IMMNotificationClient)]
struct Notifier {
    changed: Arc<AtomicBool>,
}

impl IMMNotificationClient_Impl for Notifier_Impl {
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> windows::core::Result<()> {
        self.changed.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn OnDeviceAdded(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        self.changed.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> windows::core::Result<()> {
        self.changed.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        _id: &PCWSTR,
    ) -> windows::core::Result<()> {
        // Only the default that `resolve` reads. Moving the communications
        // default, or the default microphone, changes nothing here.
        if flow == eRender && role == eConsole {
            self.changed.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> windows::core::Result<()> {
        // Names, icons and the like. A format change is announced by the
        // stream failing, which every stream already handles.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preference_only_counts_as_changed_when_it_changes() {
        let preference = Preference::new();
        let start = preference.generation();
        preference.set(None);
        assert_eq!(preference.generation(), start, "setting the same value is not a change");

        preference.set(Some("{0.0.0.00000000}.{a}".into()));
        assert_eq!(preference.generation(), start + 1);
        assert_eq!(preference.chosen().as_deref(), Some("{0.0.0.00000000}.{a}"));

        preference.set(Some("{0.0.0.00000000}.{a}".into()));
        assert_eq!(preference.generation(), start + 1);

        preference.set(None);
        assert_eq!(preference.generation(), start + 2);
        assert_eq!(preference.chosen(), None);
    }

    /// The lists work from a thread nobody started COM on.
    ///
    /// The host looks up which apps to exclude on whichever async worker the
    /// session happens to run on. Before the first session's media threads
    /// existed, nothing in the process had started COM, every lookup failed
    /// quietly, and the first guest after launch heard the apps it was meant
    /// not to. Run alone, in a fresh process, to see it fail without
    /// `ensure_mta`: in a full run some other test has usually started COM
    /// already.
    #[test]
    fn the_lists_work_on_a_thread_without_com() {
        let bare = std::thread::spawn(|| {
            (playback_devices().len(), crate::audio_capture::audio_sessions().len())
        })
        .join()
        .unwrap();

        let prepared = std::thread::spawn(|| {
            // SAFETY: balanced below; this thread does nothing else.
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_MULTITHREADED,
                );
            }
            let devices = playback_devices().len();
            // SAFETY: balances the CoInitializeEx above.
            unsafe { windows::Win32::System::Com::CoUninitialize() };
            devices
        })
        .join()
        .unwrap();

        assert_eq!(bare.0, prepared, "devices listed without COM: {} of {prepared}", bare.0);
    }

    /// What the pickers will offer on this machine.
    #[test]
    #[ignore = "diagnostic — run explicitly"]
    fn list_the_playback_devices_on_this_machine() {
        // SAFETY: COM has to be up for the enumeration; this is a test thread.
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            );
        }
        let devices = playback_devices();
        println!("{} playback devices:", devices.len());
        for device in &devices {
            println!("  {} {}  {}", if device.default { "*" } else { " " }, device.name, device.id);
        }
        assert!(devices.iter().filter(|d| d.default).count() <= 1, "one default at most");
        if let Some(first) = devices.first() {
            assert!(first.default, "the default is listed first");
        }

        // A chosen device that is not there falls back rather than failing.
        let enumerator = enumerator().expect("enumerator");
        if let Ok((_, about, how)) = resolve(&enumerator, Some("not a device")) {
            assert_eq!(how, Choice::ChosenMissing);
            assert!(about.default);
        }
    }
}
