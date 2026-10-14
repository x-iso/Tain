//! What the application tells the service, over a pipe of its own.
//!
//! # Why the service needs to be told anything
//!
//! Unattended access means the machine answers when nobody is signed in, and
//! only the service is running then. But the host's identity — its iroh secret
//! key, and the connection codes people have already been given — lives in the
//! signed-in user's Windows Credential Manager, and Local System cannot read
//! another account's store. So the service starts life unable to be the same
//! host the user has been handing codes out for.
//!
//! The application hands it over while somebody is signed in, and the service
//! keeps a copy in its own credential store. The same code then reaches the
//! machine whether or not anyone is at it, which is the whole point: a code that
//! only worked while you were already logged in would be useless for the case
//! it exists to cover.
//!
//! # What that means for the secret
//!
//! Local System ends up holding the host key. That is worth stating plainly
//! rather than burying: anything running as SYSTEM on this machine can already
//! read every user's credential store, so this grants no access that was not
//! there — but it does put the key in a second place, and a machine with
//! unattended access turned off should not pay that. So nothing is sent until
//! it is switched on.
//!
//! # Why a second pipe rather than the agents'
//!
//! The agent pipes carry a desktop's frames and are opened per desktop, per
//! connection, by whoever is hosting. This is a different lifetime and a
//! different audience: one long-lived channel to the service itself, used a
//! handful of times a session. Sharing them would mean the frame path had to
//! stay up to configure something, which is exactly backwards.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// The pipe the service listens on.
pub const CONTROL_PIPE: &str = r"\\.\pipe\tain-service-control";

/// Refuses a length that is not a control message before allocating for it.
///
/// Control messages are tiny — the largest carries a key and a few codes. A
/// generous ceiling that is still nowhere near "believe whatever arrives".
pub const MAX_CONTROL_MESSAGE: usize = 64 * 1024;

/// What the application asks of the service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlMessage {
    /// Host on this machine's behalf while the application is not, as this
    /// identity and with these settings.
    ///
    /// Sent when unattended access is on, and re-sent whenever the identity or
    /// the codes change — a regenerated passcode that the service did not hear
    /// about would leave it answering to a code nobody has any more.
    AdoptIdentity {
        /// The host's iroh secret key.
        secret_key: [u8; 32],
        /// Connection codes, whole, exactly as they are handed out.
        ///
        /// Whole rather than bare secrets so the service parses them through
        /// base58check on the way in, as everything else does — a damaged
        /// credential fails to parse instead of being adopted as thirty-two
        /// bytes of nonsense that looks like a working passcode.
        codes: Vec<String>,
        /// The application's settings, verbatim, as the JSON it saves.
        ///
        /// # Why the whole thing and not the fields it needs
        ///
        /// Because naming the fields is how this went wrong four times. The
        /// service hosts on the same identity and runs the same session
        /// pipeline, so it needs the same configuration — and it was given the
        /// identity, then the codes, then the approved devices, each as a
        /// separate errand, each added only after somebody noticed it missing.
        /// What was still missing after all three: the frame rate, the bitrate,
        /// the codec, the resolution ceiling, the host name, the audio
        /// exclusions and the local-input window, because an unattended session
        /// ran on `SessionConfig::default()` and quietly ignored every one of
        /// them.
        ///
        /// A blob has none of that failure mode. The service builds its
        /// configuration the same way the application does — one call to
        /// `to_session_config` — so a setting added later reaches it without
        /// anybody remembering to forward it.
        ///
        /// # Why JSON rather than the config itself
        ///
        /// `SessionConfig` is not serialisable and should not become so: it
        /// holds resolved things like a window handle to capture, which mean
        /// nothing in another process. The settings are already a serde type,
        /// already `#[serde(default)]` — so a service that predates a field
        /// reads the file without it rather than failing to parse — and the
        /// translation to a session configuration already exists on the far
        /// side of it.
        settings: String,
    },
    /// Stop hosting on this machine's behalf, and forget what was adopted.
    ///
    /// Sent when the setting is turned off. Forgetting is the point: leaving
    /// the key in Local System's store after somebody has said they do not want
    /// unattended access would keep the cost of a feature they declined.
    ForgetIdentity,
    /// Whether the application is currently hosting.
    ///
    /// Two hosts on one identity would be worse than none: guests would reach
    /// whichever answered first, and which one that was would change between
    /// attempts. So the service only binds while this says nobody else is.
    ///
    /// # Why `true` is a lease and not a latch
    ///
    /// This was once sent exactly once, when the application began hosting, and
    /// the service treated it as true from then on. That made unattended access
    /// fail in the one case it exists for: close the application and the
    /// machine went unreachable, because the service was still yielding to a
    /// host that no longer existed. Nothing sent `false`, and nothing could —
    /// [`send`] hangs up after each message, so there is no broken connection
    /// for the service to notice either.
    ///
    /// So `true` expires. The application re-sends it every
    /// [`HOSTING_HEARTBEAT`] and the service forgets the claim after
    /// [`HOSTING_LEASE`] of silence. `false` on the way out is the courtesy
    /// that makes handover immediate rather than a lease long — it is not the
    /// mechanism, because a process that is killed, panics or loses power sends
    /// nothing, and "the machine I am not sitting at" is the entire premise of
    /// the feature.
    ApplicationHosting(bool),
}

/// How often the application repeats its hosting claim.
///
/// Short enough that [`HOSTING_LEASE`] can be short, long enough that this is a
/// named pipe opened twelve times a minute rather than continuously.
pub const HOSTING_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the service honours a hosting claim it has stopped hearing.
///
/// Three heartbeats. One missed beat is a busy machine; three is a host that is
/// gone. The cost of being wrong is asymmetric and this errs the right way: too
/// short and both ends bind briefly, which the next beat resolves; too long and
/// the machine stays unreachable for exactly as long as the excess.
pub const HOSTING_LEASE: std::time::Duration = std::time::Duration::from_secs(15);

/// Frame one message for the pipe: four bytes of length, then the body.
///
/// The same shape as the network protocol's framing, deliberately. There is
/// nothing to gain from a second convention, and somebody debugging this at
/// three in the morning should not have to remember which one they are looking
/// at.
pub fn encode(message: &ControlMessage) -> Result<Vec<u8>> {
    let body = bincode::serialize(message)?;
    anyhow::ensure!(
        body.len() <= MAX_CONTROL_MESSAGE,
        "control message is {} bytes, over the {MAX_CONTROL_MESSAGE} limit",
        body.len()
    );
    let mut framed = Vec::with_capacity(4 + body.len());
    framed.extend_from_slice(&(body.len() as u32).to_be_bytes());
    framed.extend_from_slice(&body);
    Ok(framed)
}

/// Read a body back. The caller has already taken the length off the wire.
pub fn decode(body: &[u8]) -> Result<ControlMessage> {
    Ok(bincode::deserialize(body)?)
}

/// Whether a length prefix could be a control message.
pub fn length_is_plausible(len: usize) -> bool {
    len > 0 && len <= MAX_CONTROL_MESSAGE
}

#[cfg(target_os = "windows")]
pub use platform::{send, serve};

/// Ask the service to do something. No-op where there is no service.
#[cfg(not(target_os = "windows"))]
pub fn send(_message: &ControlMessage) -> Result<()> {
    anyhow::bail!("the Tain service is Windows-only")
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use anyhow::{bail, Context};
    use std::sync::atomic::{AtomicBool, Ordering};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    };
    use windows::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    use crate::pipe_security::{console_user_sid, wide, OwnedHandle, PipeSecurity};

    /// Listen for the application until `running` goes false.
    ///
    /// One client at a time, which is all there is: the application on this
    /// machine. `handle` is called for each message and may be called from this
    /// thread only, so it is free to touch whatever the service owns.
    ///
    /// A message that cannot be understood ends that connection rather than the
    /// server. The application reconnects, and a service that stopped listening
    /// because of one bad frame would need a reboot to recover.
    pub fn serve(running: &AtomicBool, mut handle: impl FnMut(ControlMessage)) -> Result<()> {
        let name = wide(CONTROL_PIPE);
        let sid = console_user_sid()?;

        while running.load(Ordering::Relaxed) {
            let security = PipeSecurity::for_console_user(&sid)?;

            // SAFETY: the name and attributes outlive the call.
            let pipe = unsafe {
                CreateNamedPipeW(
                    PCWSTR(name.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    1,
                    // Control messages are tiny; this is generous.
                    64 * 1024,
                    64 * 1024,
                    0,
                    Some(security.attributes()),
                )
            };
            if pipe.is_invalid() {
                bail!(
                    "could not create the service control pipe: {}",
                    windows::core::Error::from_win32()
                );
            }
            let pipe = OwnedHandle(pipe);

            // SAFETY: a pipe this function owns and has not connected yet.
            let connected = unsafe { ConnectNamedPipe(pipe.0, None) };
            if let Err(e) = &connected
                && e.code() != ERROR_PIPE_CONNECTED.to_hresult()
            {
                tracing::debug!("Service control pipe: waiting: {e}");
                continue;
            }

            tracing::debug!("Service control pipe: the application connected");
            while running.load(Ordering::Relaxed) {
                match read_message(pipe.0) {
                    Ok(Some(message)) => handle(message),
                    // A clean disconnect. The application closes the pipe after
                    // saying what it came to say.
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("Service control pipe: {e:#}");
                        break;
                    }
                }
            }
            // SAFETY: connected immediately above.
            unsafe {
                let _ = DisconnectNamedPipe(pipe.0);
            }
        }
        Ok(())
    }

    /// One message, or `None` when the far end has gone.
    fn read_message(pipe: HANDLE) -> Result<Option<ControlMessage>> {
        let mut header = [0u8; 4];
        if !read_exact(pipe, &mut header)? {
            return Ok(None);
        }
        let len = u32::from_be_bytes(header) as usize;

        // Checked before allocating. The length arrives from another process,
        // and `vec![0; n]` on an unchecked one is how a stray write becomes an
        // out-of-memory abort in a service.
        if !length_is_plausible(len) {
            bail!("the application sent a {len}-byte message, which is not one");
        }

        let mut body = vec![0u8; len];
        if !read_exact(pipe, &mut body)? {
            return Ok(None);
        }
        // # Why this one error gets a sentence
        //
        // A body that arrives whole and will not decode is, in practice,
        // always the same thing: the application and the service are different
        // builds, and the message changed shape between them. The raw error
        // says something about an invalid tag, which sends people to look at
        // the pipe. The consequence is invisible and specific — the service
        // keeps whatever it stored last and silently stops learning about new
        // codes, new settings and newly approved devices — so it is worth
        // naming rather than leaving to be worked out from a machine that
        // refuses connections for no stated reason.
        decode(&body).map(Some).context(
            "the application sent a control message this service cannot read — they are \
             almost certainly different builds of Tain. Reinstall the service from the \
             application you are running, then restart the application.",
        )
    }

    /// Fill `into`, or report that the far end closed first.
    fn read_exact(pipe: HANDLE, into: &mut [u8]) -> Result<bool> {
        let mut filled = 0usize;
        while filled < into.len() {
            let mut read = 0u32;
            // SAFETY: a live pipe handle and a slice that outlives the call.
            let got = unsafe { ReadFile(pipe, Some(&mut into[filled..]), Some(&mut read), None) };
            if got.is_err() || read == 0 {
                return Ok(false);
            }
            filled += read as usize;
        }
        Ok(true)
    }

    /// Say one thing to the service and hang up.
    ///
    /// Connect-per-message rather than a held connection: these are sent a
    /// handful of times a session, and a channel kept open for hours is a
    /// channel that has to be noticed when it breaks. Failing is normal — the
    /// service may not be installed — so the caller decides whether to care.
    pub fn send(message: &ControlMessage) -> Result<()> {
        let framed = encode(message)?;
        let name = wide(CONTROL_PIPE);

        // SAFETY: the name outlives the call.
        let pipe = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                windows::Win32::Foundation::GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }
        .context("the Tain service is not listening")?;
        let pipe = OwnedHandle(pipe);

        let mut sent = 0usize;
        while sent < framed.len() {
            let mut wrote = 0u32;
            // SAFETY: a live pipe handle and a slice that outlives the call.
            unsafe { WriteFile(pipe.0, Some(&framed[sent..]), Some(&mut wrote), None) }
                .context("the service stopped reading")?;
            if wrote == 0 {
                bail!("the service stopped reading");
            }
            sent += wrote as usize;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_survives_the_round_trip() {
        let messages = [
            ControlMessage::AdoptIdentity {
                secret_key: [7u8; 32],
                codes: vec!["MASTER-abc".to_string(), "GUEST-def".to_string()],
                settings: r#"{"target_fps":30,"exclude_audio_processes":["teams.exe"]}"#
                    .to_string(),
            },
            // Both routinely empty — no codes issued yet, no settings file
            // written yet — and each has to survive the trip as empty rather
            // than as an absence the far side could read as "unchanged".
            ControlMessage::AdoptIdentity {
                secret_key: [7u8; 32],
                codes: Vec::new(),
                settings: String::new(),
            },
            ControlMessage::ForgetIdentity,
            ControlMessage::ApplicationHosting(true),
            ControlMessage::ApplicationHosting(false),
        ];

        for message in messages {
            let framed = encode(&message).expect("our own message");
            let len = u32::from_be_bytes(framed[0..4].try_into().unwrap()) as usize;
            assert_eq!(len, framed.len() - 4, "the prefix must describe the body");
            assert_eq!(decode(&framed[4..]).expect("our own body"), message);
        }
    }

    /// A length arriving from another process is not a number to allocate on.
    #[test]
    fn an_absurd_length_is_refused() {
        assert!(!length_is_plausible(0));
        assert!(!length_is_plausible(MAX_CONTROL_MESSAGE + 1));
        assert!(!length_is_plausible(usize::MAX));
        assert!(length_is_plausible(1));
        assert!(length_is_plausible(MAX_CONTROL_MESSAGE));
    }

    /// Nonsense in the body must be an error, not a partial adoption.
    #[test]
    fn a_damaged_body_is_refused_rather_than_guessed_at() {
        assert!(decode(&[0xFF; 16]).is_err());
    }

    /// The lease has to outlast a missed beat, or the two ends fight.
    ///
    /// Both sides of this matter and they pull opposite ways. Too short and a
    /// busy machine drops one heartbeat, the service binds alongside a host
    /// that never stopped, and a guest reaches whichever answered first — the
    /// exact failure the claim exists to prevent. Too long and the machine
    /// stays unreachable after the application closes for however much the
    /// excess is, which is the failure the lease exists to prevent.
    ///
    /// Room for two missed beats is the judgement; this test is here so that
    /// changing either constant alone cannot quietly undo it.
    #[test]
    fn the_hosting_lease_survives_a_missed_heartbeat() {
        assert!(
            HOSTING_HEARTBEAT < HOSTING_LEASE,
            "a lease no longer than the beat that renews it expires between beats",
        );
        assert!(
            HOSTING_HEARTBEAT * 3 <= HOSTING_LEASE,
            "the lease must survive two missed beats: {HOSTING_HEARTBEAT:?} against \
             {HOSTING_LEASE:?}",
        );
    }
}
