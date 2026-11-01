//! The desktop's permission to show the screen and drive the input, on Linux.
//!
//! ## Why a portal
//!
//! A Wayland compositor gives no application the screen or the input of
//! another. The way in that GNOME, KDE and the others agree on is
//! `xdg-desktop-portal`: an application asks over D-Bus, the desktop asks the
//! person at the machine what to share, and on yes hands over a PipeWire
//! stream of the screen and accepts input events for it.
//!
//! One **RemoteDesktop** session with screen casting attached, rather than a
//! ScreenCast session and a RemoteDesktop one: a single dialog then grants
//! both, and the pointer coordinates are in the same stream's space as the
//! picture. Where the desktop has no RemoteDesktop portal — wlroots
//! compositors, today — a plain ScreenCast session still shares the screen,
//! and input is reported as unavailable.
//!
//! ## Asked once, remembered
//!
//! The session asks to persist (`persist_mode` 2), and the desktop hands back
//! a restore token, single-use, which is kept in the state directory and
//! passed to the next session so the dialog is not shown again. GNOME and KDE
//! both honour it; an older portal that refuses the option is asked again
//! without it.
//!
//! The token is not a connection secret — it lets this application skip a
//! dialog on this machine, nothing more — so it lives in a file readable only
//! by the user rather than in the credential store.
//!
//! ## One session for the whole process
//!
//! Every capturer and the input injector share it ([`session`]). It is closed
//! when the last of them lets go, which is when the desktop stops showing that
//! the screen is being shared.

#![cfg(target_os = "linux")]

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;
use tracing::*;
use zbus::blocking::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const DESTINATION: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const REMOTE_DESKTOP: &str = "org.freedesktop.portal.RemoteDesktop";
const SCREEN_CAST: &str = "org.freedesktop.portal.ScreenCast";

/// The application id the portal files permissions under. Has to match the
/// installed `.desktop` file's name, which is `tain-cli`'s `gui::APP_ID`.
const APP_ID: &str = "tain";

/// How long a person at the host has to answer the sharing dialog.
const DIALOG_TIMEOUT: Duration = Duration::from_secs(120);
/// How long anything that is not a dialog may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// RemoteDesktop device types.
pub const DEVICE_KEYBOARD: u32 = 1;
pub const DEVICE_POINTER: u32 = 2;
pub const DEVICE_TOUCHSCREEN: u32 = 4;

/// ScreenCast source types and cursor modes.
const SOURCE_MONITOR: u32 = 1;
const SOURCE_WINDOW: u32 = 2;
const CURSOR_EMBEDDED: u32 = 2;

/// One screen the session shares.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    /// The PipeWire node to connect to.
    pub node_id: u32,
    /// Its size in the desktop's logical coordinates, which are what absolute
    /// pointer positions are given in. Not the size of the frames, which on a
    /// scaled display is larger.
    pub size: Option<(i32, i32)>,
}

/// A running portal session, closed when the last holder drops it.
pub struct RemoteSession {
    connection: Connection,
    handle: OwnedObjectPath,
    streams: Vec<StreamInfo>,
    /// Which input devices the desktop granted; zero for a ScreenCast-only
    /// session.
    devices: u32,
    pipewire: std::os::fd::OwnedFd,
}

static SHARED: Mutex<Weak<RemoteSession>> = Mutex::new(Weak::new());

/// The process's portal session: the running one, or a new one, which shows
/// the desktop's sharing dialog unless a remembered choice still holds.
///
/// Blocks while that dialog is up, for up to two minutes.
pub fn session() -> Result<Arc<RemoteSession>> {
    // Held across the setup, so two sessions starting together get one dialog.
    let mut shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(running) = shared.upgrade() {
        return Ok(running);
    }
    let session = Arc::new(RemoteSession::start()?);
    *shared = Arc::downgrade(&session);
    Ok(session)
}

/// The running session, if a capturer has started one. The input injector
/// asks this rather than [`session`], so input alone never puts a dialog up.
pub fn current() -> Option<Arc<RemoteSession>> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner()).upgrade()
}

/// The session bus connection, registered with the portal under [`APP_ID`].
///
/// Registration has to come before any portal call on the connection, and
/// can happen once, so the connection is made once and kept.
fn connection() -> Result<Connection> {
    static CONNECTION: OnceLock<std::result::Result<Connection, String>> = OnceLock::new();
    CONNECTION
        .get_or_init(|| {
            let connection = Connection::session().map_err(|e| format!("no session bus: {e}"))?;
            // xdg-desktop-portal 1.19 and later. Without it the portal files an
            // unsandboxed application under an empty id, which still works,
            // and older portals have no such interface — so failure is fine.
            let registry = proxy(&connection, PATH, "org.freedesktop.host.portal.Registry");
            if let Ok(registry) = registry {
                let options: HashMap<&str, Value> = HashMap::new();
                if let Err(e) = registry.call::<_, _, ()>("Register", &(APP_ID, options)) {
                    debug!("Not registered with the portal as {APP_ID} ({e})");
                }
            }
            Ok(connection)
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

fn proxy<'a>(connection: &Connection, path: &'a str, interface: &'a str) -> Result<zbus::blocking::Proxy<'a>> {
    Ok(zbus::blocking::proxy::Builder::new(connection)
        .destination(DESTINATION)?
        .path(path)?
        .interface(interface)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()?)
}

/// A token for one request, unique within this process.
fn token() -> String {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("tain_{}_{n}", std::process::id())
}

/// The object path the portal will create for a request with this token.
///
/// Known in advance on purpose: the response has to be subscribed to *before*
/// the call, or a fast answer arrives before anyone is listening.
fn request_path(connection: &Connection, token: &str) -> Result<String> {
    let sender = connection.unique_name().context("the bus gave this connection no name")?;
    let sender = sender.as_str().trim_start_matches(':').replace('.', "_");
    Ok(format!("{PATH}/request/{sender}/{token}"))
}

type Results = HashMap<String, OwnedValue>;

/// The options dictionary every portal method ends with, `a{sv}`.
type Options = HashMap<&'static str, Value<'static>>;

/// What a request is made with: the session it belongs to, if any, and the
/// parent window for a method that shows a dialog. Each shape is its own
/// D-Bus signature, so each is spelled out rather than built as a list of
/// variants, which would arrive as `v` where the portal expects `o` or `s`.
enum Arguments<'a> {
    /// `(a{sv})`: CreateSession.
    None,
    /// `(o a{sv})`: SelectDevices, SelectSources.
    Session(&'a OwnedObjectPath),
    /// `(o s a{sv})`: Start, with an empty parent window.
    SessionAndParent(&'a OwnedObjectPath),
}

/// Call a portal method that answers through a Request object, and wait for
/// the answer.
fn request(
    connection: &Connection,
    interface: &str,
    method: &str,
    arguments: Arguments,
    mut options: Options,
    timeout: Duration,
) -> Result<Results> {
    let token = token();
    let path = request_path(connection, &token)?;
    let request = proxy(connection, &path, "org.freedesktop.portal.Request")?;
    let mut responses = request.receive_signal("Response")?;
    options.insert("handle_token", Value::from(token));

    let target = proxy(connection, PATH, interface)?;
    let called = match arguments {
        Arguments::None => target.call_method(method, &(options,)),
        Arguments::Session(session) => target.call_method(method, &(session, options)),
        Arguments::SessionAndParent(session) => target.call_method(method, &(session, "", options)),
    };
    called.with_context(|| format!("{interface}.{method}"))?;

    // The iterator blocks with no timeout of its own, so it waits on a thread
    // and this waits on the thread.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(responses.next());
    });
    let message = match rx.recv_timeout(timeout) {
        Ok(Some(message)) => message,
        Ok(None) => bail!("the portal went away during {method}"),
        Err(_) => {
            // Withdraw it, so a dialog nobody answered does not linger.
            let _ = request.call_method("Close", &());
            bail!("nobody answered the screen sharing dialog in time");
        }
    };
    let (code, results): (u32, Results) =
        message.body().deserialize().context("the portal's answer was malformed")?;
    match code {
        0 => Ok(results),
        1 => bail!("the person at this computer declined to share the screen"),
        _ => bail!("the desktop refused to share the screen ({method})"),
    }
}

/// Where the restore token is kept between sessions.
fn token_file() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/state")))?;
    Some(base.join("Tain").join("screen-sharing-token"))
}

fn load_token() -> Option<String> {
    let token = std::fs::read_to_string(token_file()?).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

fn save_token(token: Option<&str>) {
    let Some(path) = token_file() else { return };
    let Some(token) = token else {
        let _ = std::fs::remove_file(&path);
        return;
    };
    use std::os::unix::fs::OpenOptionsExt;
    let written = path
        .parent()
        .map(std::fs::create_dir_all)
        .transpose()
        .and_then(|_| {
            use std::io::Write;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)?
                .write_all(token.as_bytes())
        });
    if let Err(e) = written {
        debug!("Could not keep the screen sharing token ({e}) — the dialog will ask again");
    }
}

impl RemoteSession {
    fn start() -> Result<Self> {
        let connection = connection()?;
        let remote_desktop = proxy(&connection, PATH, REMOTE_DESKTOP)?;
        let version = remote_desktop.get_property::<u32>("version").ok();
        let available_devices = remote_desktop.get_property::<u32>("AvailableDeviceTypes").ok();
        match (version, available_devices) {
            (Some(version), Some(devices)) => {
                info!("Asking the desktop to share the screen and input (RemoteDesktop portal v{version})");
                let restore = load_token();
                match Self::start_remote_desktop(&connection, version, devices, restore.clone()) {
                    // A remembered choice the desktop no longer honours, or an
                    // older portal that rejects persistence: ask afresh.
                    Err(e) if restore.is_some() || version >= 2 => {
                        debug!("Remote desktop with persistence failed ({e:#}) — asking without it");
                        save_token(None);
                        Self::start_remote_desktop(&connection, 1, devices, None)
                    }
                    other => other,
                }
            }
            _ => {
                info!("No RemoteDesktop portal — sharing the screen without input");
                Self::start_screen_cast(&connection)
            }
        }
    }

    fn start_remote_desktop(
        connection: &Connection,
        version: u32,
        available_devices: u32,
        restore: Option<String>,
    ) -> Result<Self> {
        let mut options = HashMap::new();
        options.insert("session_handle_token", Value::from(token()));
        let created = request(connection, REMOTE_DESKTOP, "CreateSession", Arguments::None, options, CALL_TIMEOUT)?;
        let handle = session_handle(&created)?;

        let wanted = (DEVICE_KEYBOARD | DEVICE_POINTER | DEVICE_TOUCHSCREEN) & available_devices;
        let mut options = HashMap::new();
        options.insert("types", Value::from(wanted));
        if version >= 2 {
            options.insert("persist_mode", Value::from(2u32));
            if let Some(token) = restore {
                options.insert("restore_token", Value::from(token));
            }
        }
        request(connection, REMOTE_DESKTOP, "SelectDevices", Arguments::Session(&handle), options, CALL_TIMEOUT)?;

        // Persistence is not allowed on the ScreenCast half of a remote
        // desktop session; the RemoteDesktop half carries it for both.
        Self::select_sources(connection, &handle, false)?;

        let started = request(
            connection,
            REMOTE_DESKTOP,
            "Start",
            Arguments::SessionAndParent(&handle),
            HashMap::new(),
            DIALOG_TIMEOUT,
        )?;
        let devices = started.get("devices").and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
        Self::finish(connection, handle, &started, devices)
    }

    fn start_screen_cast(connection: &Connection) -> Result<Self> {
        let mut options = HashMap::new();
        options.insert("session_handle_token", Value::from(token()));
        let created = request(connection, SCREEN_CAST, "CreateSession", Arguments::None, options, CALL_TIMEOUT)?;
        let handle = session_handle(&created)?;
        Self::select_sources(connection, &handle, true)?;
        let started = request(
            connection,
            SCREEN_CAST,
            "Start",
            Arguments::SessionAndParent(&handle),
            HashMap::new(),
            DIALOG_TIMEOUT,
        )?;
        Self::finish(connection, handle, &started, 0)
    }

    fn select_sources(connection: &Connection, handle: &OwnedObjectPath, persist: bool) -> Result<()> {
        let screen_cast = proxy(connection, PATH, SCREEN_CAST)?;
        let source_types = screen_cast.get_property::<u32>("AvailableSourceTypes").unwrap_or(SOURCE_MONITOR);
        let cursor_modes = screen_cast.get_property::<u32>("AvailableCursorModes").unwrap_or(0);
        let mut options = HashMap::new();
        // A screen or a window: the desktop's own picker offers both, and the
        // stream looks the same to everything here either way.
        options.insert("types", Value::from((SOURCE_MONITOR | SOURCE_WINDOW) & source_types));
        options.insert("multiple", Value::from(false));
        // Drawn into the picture by the compositor, so it moves with the
        // stream. Sending it separately, as Windows does, is a later step.
        if cursor_modes & CURSOR_EMBEDDED != 0 {
            options.insert("cursor_mode", Value::from(CURSOR_EMBEDDED));
        }
        if persist {
            options.insert("persist_mode", Value::from(2u32));
            if let Some(token) = load_token() {
                options.insert("restore_token", Value::from(token));
            }
        }
        request(connection, SCREEN_CAST, "SelectSources", Arguments::Session(handle), options, CALL_TIMEOUT)?;
        Ok(())
    }

    fn finish(connection: &Connection, handle: OwnedObjectPath, started: &Results, devices: u32) -> Result<Self> {
        save_token(started.get("restore_token").and_then(|v| <&str>::try_from(v).ok()));
        let streams = parse_streams(started)?;
        anyhow::ensure!(!streams.is_empty(), "the desktop shared no screen");

        let screen_cast = proxy(connection, PATH, SCREEN_CAST)?;
        let options: HashMap<&str, Value> = HashMap::new();
        let fd: zbus::zvariant::OwnedFd = screen_cast
            .call("OpenPipeWireRemote", &(&handle, options))
            .context("could not open the screen's PipeWire stream")?;
        info!(
            "Sharing {} screen(s){}",
            streams.len(),
            if devices == 0 { ", without input".to_string() } else { format!(", input devices {devices:#x}") }
        );
        Ok(Self { connection: connection.clone(), handle, streams, devices, pipewire: fd.into() })
    }

    /// The screens being shared. One, today: the picker is asked for one.
    pub fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    /// Which input devices the desktop granted.
    pub fn devices(&self) -> u32 {
        self.devices
    }

    /// A descriptor for the PipeWire remote, for one stream to connect with.
    /// PipeWire takes ownership of what it is given, so each caller gets its own.
    pub fn pipewire_fd(&self) -> Result<std::os::fd::OwnedFd> {
        self.pipewire.try_clone().context("could not duplicate the PipeWire descriptor")
    }

    /// Send one of the RemoteDesktop portal's `Notify*` methods.
    ///
    /// Sent without waiting for a reply: these are a stream of input events,
    /// and a round trip per pointer movement would spend the latency budget on
    /// acknowledgements nobody acts on. D-Bus keeps them in order.
    pub fn notify<B>(&self, method: &'static str, arguments: &B) -> Result<()>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let message = zbus::Message::method_call(PATH, method)?
            .destination(DESTINATION)?
            .interface(REMOTE_DESKTOP)?
            .with_flags(zbus::message::Flags::NoReplyExpected)?
            .build(arguments)?;
        self.connection.send(&message).with_context(|| format!("RemoteDesktop.{method}"))
    }

    /// The session's object path, for the `Notify*` arguments.
    pub fn handle(&self) -> &OwnedObjectPath {
        &self.handle
    }
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        if let Ok(session) = proxy(&self.connection, self.handle.as_str(), "org.freedesktop.portal.Session") {
            let _ = session.call_method("Close", &());
        }
        info!("Stopped sharing the screen");
    }
}

fn session_handle(results: &Results) -> Result<OwnedObjectPath> {
    let handle = results
        .get("session_handle")
        .context("the portal created a session without a handle")?;
    // A string in the specification; some implementations send an object path.
    if let Ok(text) = <&str>::try_from(handle) {
        return OwnedObjectPath::try_from(text.to_string()).context("the session handle is not a path");
    }
    OwnedObjectPath::try_from(handle.try_clone()?).context("the session handle is not a path")
}

fn parse_streams(results: &Results) -> Result<Vec<StreamInfo>> {
    let Some(value) = results.get("streams") else { return Ok(Vec::new()) };
    let streams: Vec<(u32, HashMap<String, OwnedValue>)> =
        value.try_clone()?.try_into().context("the portal's stream list was malformed")?;
    Ok(streams
        .into_iter()
        .map(|(node_id, properties)| StreamInfo {
            node_id,
            size: properties
                .get("size")
                .and_then(|v| <(i32, i32)>::try_from(v.try_clone().ok()?).ok()),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_and_legal_in_an_object_path() {
        let a = token();
        let b = token();
        assert_ne!(a, b);
        for t in [&a, &b] {
            assert!(t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'), "{t}");
        }
    }

    #[test]
    fn a_stream_list_is_read_with_its_logical_size() {
        let mut properties: HashMap<String, OwnedValue> = HashMap::new();
        properties.insert("size".into(), OwnedValue::try_from(Value::from((1280i32, 720i32))).unwrap());
        let streams = vec![(42u32, properties)];
        let mut results: Results = HashMap::new();
        results.insert("streams".into(), OwnedValue::try_from(Value::from(streams)).unwrap());
        let parsed = parse_streams(&results).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].node_id, 42);
        assert_eq!(parsed[0].size, Some((1280, 720)));
    }

    #[test]
    fn a_session_handle_is_accepted_as_a_string() {
        let mut results: Results = HashMap::new();
        results.insert(
            "session_handle".into(),
            OwnedValue::try_from(Value::from("/org/freedesktop/portal/desktop/session/1_2/t")).unwrap(),
        );
        assert_eq!(session_handle(&results).unwrap().as_str(), "/org/freedesktop/portal/desktop/session/1_2/t");
    }
}
