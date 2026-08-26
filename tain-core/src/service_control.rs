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
    /// Take over hosting when nobody is signed in, using this identity.
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
    },
    /// Stop hosting when nobody is signed in, and forget what was adopted.
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
    ApplicationHosting(bool),
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_survives_the_round_trip() {
        let messages = [
            ControlMessage::AdoptIdentity {
                secret_key: [7u8; 32],
                codes: vec!["MASTER-abc".to_string(), "GUEST-def".to_string()],
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
}
