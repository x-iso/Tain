//! The host that answers for this machine while the application is not
//! running.
//!
//! # Why a process of its own, on the desktop
//!
//! It used to be a thread inside the service, and the service lives in session
//! zero, which has no desktop. So every frame had to come from an agent: raw
//! pixels over a named pipe, at the display's native resolution, in BGRA, sent
//! whether or not anyone wanted them. The controls that make the application's
//! picture cheap all talk to a local capturer, and the service had none — so
//! the quality setting, the guest's window size and the GPU format conversion
//! reached nothing. The low-level hook that lets somebody at the machine take
//! the mouse back sat in a session with no input to see. The pointer was
//! painted into the frames rather than sent on its own.
//!
//! None of that is true of the application, and the one thing the application
//! has that the service did not is a desktop. So the service now starts this on
//! one — the same way, and with the same token, as it starts the agents — and
//! this runs the application's hosting path as it is: capture here, and the
//! secure desktop's agent only while a consent prompt or the lock screen is up.
//! Nothing about the session is a special case for being unattended except the
//! two things that must be: there is nobody to approve anything, and nobody to
//! show a regenerated code to.
//!
//! # How it is stopped
//!
//! Two ways, for two different endings.
//!
//! Asked: it waits on a named event the service created for it. The service
//! sets it when the application starts hosting or the console session changes,
//! and this tells connected guests their session is over and closes the
//! endpoint properly — so the relay forgets it at once, and the application's
//! endpoint, which carries the same identity, is not refused as a duplicate.
//!
//! Not asked, because the service has gone: this process is in a job the
//! service holds, and Windows ends everything in the job when its last handle
//! closes. A host that outlived its service would answer on the same identity
//! as the next application to start, with nothing left to stop it.

use anyhow::Result;

/// Runs this executable as the unattended host.
pub const HOST_FLAG: &str = "--unattended-host";

/// Names the event that asks this host to stop.
pub const STOP_EVENT_ARG: &str = "--stop-event";

/// Read `--stop-event <name>` out of the command line, if it is there.
pub fn stop_event_from_args(args: &[String]) -> Option<String> {
    let at = args.iter().position(|a| a == STOP_EVENT_ARG)?;
    args.get(at + 1).cloned()
}

/// The session configuration an unattended host runs with.
///
/// The application's own settings, translated the same way the application
/// translates them — one call to `to_session_config`, so the frame rate,
/// bitrate, codec, resolution ceiling, audio exclusions, host name and
/// local-input window are the ones the owner chose. `None` means there are no
/// usable settings, and the defaults are a working session rather than none.
///
/// # Two things are not taken from the settings
///
/// `trusted_admins` comes from the credential store instead, because it is the
/// one field here that decides who gets in. The settings file sits beside the
/// service, and reading approvals from it would mean anybody who could write
/// that file could approve themselves; the store needs rights that already
/// imply everything else.
///
/// `auto_regen_guest_passcode` is forced off. Issuing a fresh guest code when
/// the last guest leaves is right for a host with somebody at it — the new code
/// is on screen the moment it exists. Here there is no screen, and a code
/// regenerated here is one the application's stored copy no longer matches,
/// leaving a machine that answers to neither.
pub fn session_config_from(
    saved: Option<crate::gui::settings::Settings>,
    approved: std::collections::BTreeSet<[u8; 32]>,
) -> tain_core::SessionConfig {
    let mut config = saved.map(|settings| settings.to_session_config()).unwrap_or_default();
    config.trusted_admins = approved;
    config.auto_regen_guest_passcode = false;
    config
}

#[cfg(not(target_os = "windows"))]
pub async fn run(_args: &[String]) -> Result<()> {
    anyhow::bail!("The Tain unattended host is Windows-only")
}

#[cfg(target_os = "windows")]
pub use platform::run;

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use anyhow::Context;
    use std::sync::Arc;

    /// How long guests are given to hear that the session is over.
    ///
    /// The same allowance the application gives on its way out, for the same
    /// reason: a kick is queued on each session's writer, and closing the
    /// endpoint before any of them has run would be exactly as silent as not
    /// saying anything.
    const GOODBYE_GRACE: std::time::Duration = std::time::Duration::from_millis(300);

    /// Host until the service says to stop.
    pub async fn run(args: &[String]) -> Result<()> {
        // Before anything is bound. A host that cannot be asked to stop is one
        // the service can only kill, and killing one skips the goodbye that
        // keeps the next endpoint on this identity from being refused.
        let stop_event = stop_event_from_args(args)
            .context("started without an event to stop on — this is only started by the service")?;
        let stopped = wait_for_stop(&stop_event)?;

        let secrets = crate::gui::secrets::HostSecrets::os();
        let identity = secrets
            .identity()
            .context("no host identity has been handed to the service — nothing to host as")?;

        let server = Arc::new(
            tain_core::HostServer::bind_with_identity(Some(identity))
                .await
                .context("the unattended host could not bind")?,
        );

        // Codes are restored rather than reissued. A new one would name the
        // same endpoint and still be a code nobody has been given.
        let endpoint_id = server.endpoint_id_bytes();
        for typ in [tain_core::PasscodeType::Master, tain_core::PasscodeType::Guest] {
            let Some(saved) = secrets.code(typ) else { continue };
            match tain_core::ConnectionCode::parse(&saved) {
                Ok(parsed) if parsed.endpoint_id == endpoint_id => {
                    server.adopt_passcode(typ, parsed.secret).await;
                }
                // A code for a different endpoint is one the application
                // regenerated without telling the service. Refusing it is
                // right: adopting it would answer to a secret whose code names
                // somewhere else.
                Ok(_) => tracing::warn!(
                    "The stored {} code is for a different endpoint — ignoring it",
                    typ.prefix()
                ),
                Err(e) => tracing::warn!("The stored {} code is unreadable: {e}", typ.prefix()),
            }
        }

        let approved = secrets.trusted_admins();
        let approved_count = approved.len();
        let saved = crate::service::settings_path()
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<crate::gui::settings::Settings>(&text).ok());
        if saved.is_none() {
            // No file yet, or one this build cannot read. The line says why the
            // picture may not look the way the settings panel promises.
            tracing::warn!(
                "No usable settings from the application — hosting with defaults, so the frame \
                 rate, bitrate and codec are not this machine's chosen ones"
            );
        }
        server.set_session_config(session_config_from(saved, approved));

        // No approval channel, deliberately. Nobody is here to answer one, and
        // `await_approval` refuses a guest that has no channel — so a master
        // code from an approved device connects, and a guest code is declined
        // rather than hanging until it times out or, worse, being let through.
        match approved_count {
            0 => tracing::warn!(
                "Unattended host listening, but no device is approved for the admin passcode — \
                 every connection will be refused. Connect once from a Tain window on this \
                 machine to approve the device."
            ),
            n => tracing::info!(
                "Unattended host listening — master codes from {n} approved device(s)"
            ),
        }

        let serving = server.clone().run();
        tokio::pin!(serving);
        tokio::select! {
            result = &mut serving => {
                // The endpoint closed without being asked. The service notices
                // the exit and starts another.
                return result.context("the unattended host stopped");
            }
            _ = stopped => tracing::info!("The service asked the unattended host to stop"),
        }

        let live = server.sessions().active_sessions();
        if !live.is_empty() {
            tracing::info!("Stopping — telling {} connected guest(s)", live.len());
            for id in live {
                server.kick_session(id, tain_core::protocol::end_reason::HOST_CLOSED);
            }
            tokio::time::sleep(GOODBYE_GRACE).await;
        }
        server.shutdown().await
    }

    /// Resolves once the service sets the named stop event.
    ///
    /// Opened here, before hosting starts, so a name that does not open is a
    /// failure to start rather than a host that can never be stopped politely.
    /// The wait itself is a blocking call and gets a thread of its own.
    fn wait_for_stop(name: &str) -> Result<tokio::sync::oneshot::Receiver<()>> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        use windows::Win32::System::Threading::{
            OpenEventW, WaitForSingleObject, INFINITE, SYNCHRONIZATION_SYNCHRONIZE,
        };

        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: the name is null-terminated and outlives the call.
        let event = unsafe { OpenEventW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(wide.as_ptr())) }
            .with_context(|| format!("could not open the stop event {name}"))?;
        // A raw pointer is not `Send`; its value is, and it is only ever used
        // by the one thread it is handed to.
        let raw = event.0 as isize;

        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("tain-host-stop".into())
            .spawn(move || {
                let event = HANDLE(raw as *mut _);
                // SAFETY: a handle this thread owns from here, waited on and
                // then closed once.
                unsafe {
                    WaitForSingleObject(event, INFINITE);
                    let _ = CloseHandle(event);
                }
                let _ = tx.send(());
            })
            .context("could not start the thread that waits for the service")?;
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The owner's settings arrive, and the two fields that must not come from
    /// them do not.
    #[test]
    fn an_unattended_session_is_the_owners_but_never_regenerates_or_self_approves() {
        use crate::gui::settings::Settings;

        let chosen = Settings {
            target_fps: 24,
            exclude_audio_processes: vec!["teams.exe".into()],
            // Both of these say something the unattended host must overrule.
            guest_autoreset: true,
            trusted_admin_endpoints: vec!["ab".repeat(32)],
            ..Settings::default()
        };
        let from_store = std::collections::BTreeSet::from([[7u8; 32]]);

        let config = session_config_from(Some(chosen.clone()), from_store.clone());

        assert_eq!(config.encode.fps, chosen.to_session_config().encode.fps);
        assert_eq!(config.exclude_audio_processes, vec!["teams.exe".to_string()]);
        assert!(
            !config.auto_regen_guest_passcode,
            "a code regenerated with no screen to show it on is a code nobody holds"
        );
        assert_eq!(
            config.trusted_admins, from_store,
            "approvals come from the credential store, never the file beside the service"
        );
    }

    /// No settings is a working session with the same two rules, not no session.
    #[test]
    fn without_settings_it_hosts_on_defaults_with_the_same_two_rules() {
        let from_store = std::collections::BTreeSet::from([[7u8; 32]]);
        let config = session_config_from(None, from_store.clone());

        assert_eq!(config.encode.fps, tain_core::SessionConfig::default().encode.fps);
        assert!(!config.auto_regen_guest_passcode);
        assert_eq!(config.trusted_admins, from_store);
    }

    #[test]
    fn the_stop_event_is_read_from_the_command_line() {
        let args: Vec<String> =
            ["tain.exe", HOST_FLAG, STOP_EVENT_ARG, r"Global\TainUnattendedHostStop-1-0"]
                .into_iter()
                .map(String::from)
                .collect();
        assert_eq!(
            stop_event_from_args(&args).as_deref(),
            Some(r"Global\TainUnattendedHostStop-1-0")
        );
        assert_eq!(stop_event_from_args(&args[..3]), None, "a flag with nothing after it");
    }
}
