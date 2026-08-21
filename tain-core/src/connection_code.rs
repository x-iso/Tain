//! Self-contained connection codes.
//!
//! A code carries everything a guest needs to reach a host: the authorization
//! level, the shared secret, and the host's iroh endpoint ID.
//!
//! ```text
//! MASTER-<base58check( [type] + [32-byte secret] + [32-byte endpoint id] )>
//! ```
//!
//! Bundling the address into the code is what lets the UI be a single
//! copy button on the host and a single paste button on the guest — there is no
//! separate "ticket" for the user to shuttle around.
//!
//! Base58**check** is deliberate: these get pasted through chat clients, and a
//! truncated or mangled paste must fail loudly rather than producing a valid-
//! looking code that dials the wrong endpoint.

use anyhow::{Context, Result};

use crate::passcode::PasscodeType;

/// Bytes carried inside a code: 1 type + 32 secret + 32 endpoint id.
const PAYLOAD_LEN: usize = 1 + 32 + 32;

/// A parsed connection code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionCode {
    pub passcode_type: PasscodeType,
    /// The shared secret the host validates against its passcode store.
    pub secret: [u8; 32],
    /// The host's iroh endpoint ID (its public key).
    pub endpoint_id: [u8; 32],
}

impl ConnectionCode {
    pub fn new(passcode_type: PasscodeType, secret: [u8; 32], endpoint_id: [u8; 32]) -> Self {
        Self { passcode_type, secret, endpoint_id }
    }

    /// Render as a shareable string.
    pub fn encode(&self) -> String {
        let mut payload = Vec::with_capacity(PAYLOAD_LEN);
        payload.push(self.passcode_type as u8);
        payload.extend_from_slice(&self.secret);
        payload.extend_from_slice(&self.endpoint_id);

        let body = bs58::encode(&payload).with_check().into_string();
        format!("{}-{}", self.passcode_type.prefix(), body)
    }

    /// Parse a code produced by [`Self::encode`].
    pub fn parse(input: &str) -> Result<Self> {
        let input = input.trim();
        let (prefix, body) = input
            .split_once('-')
            .context("Not a connection code — expected PREFIX-<code>")?;

        let prefix_type = PasscodeType::from_prefix(prefix)
            .with_context(|| format!("Unknown passcode prefix '{prefix}'"))?;

        let payload = bs58::decode(body)
            .with_check(None)
            .into_vec()
            .context("Code is damaged or incomplete — check it was copied in full")?;

        if payload.len() != PAYLOAD_LEN {
            anyhow::bail!(
                "Code has the wrong length ({} bytes, expected {})",
                payload.len(),
                PAYLOAD_LEN
            );
        }

        let passcode_type = match payload[0] {
            b if b == PasscodeType::Master as u8 => PasscodeType::Master,
            b if b == PasscodeType::Guest as u8 => PasscodeType::Guest,
            other => anyhow::bail!("Unknown passcode type byte {other}"),
        };

        // The prefix is a human-readable hint; the byte is authoritative. If they
        // disagree the code was tampered with or hand-edited.
        if passcode_type != prefix_type {
            anyhow::bail!(
                "Code prefix says {} but its contents say {}",
                prefix_type.prefix(),
                passcode_type.prefix()
            );
        }

        let mut secret = [0u8; 32];
        secret.copy_from_slice(&payload[1..33]);
        let mut endpoint_id = [0u8; 32];
        endpoint_id.copy_from_slice(&payload[33..65]);

        Ok(Self { passcode_type, secret, endpoint_id })
    }

    /// Safe-to-log form: the prefix and a short tail, never the secret.
    pub fn redacted(&self) -> String {
        let encoded = self.encode();
        crate::passcode::redact(&encoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(typ: PasscodeType) -> ConnectionCode {
        ConnectionCode::new(typ, [7u8; 32], [9u8; 32])
    }

    #[test]
    fn roundtrips_through_encode_and_parse() {
        for typ in [PasscodeType::Master, PasscodeType::Guest] {
            let code = sample(typ);
            let parsed = ConnectionCode::parse(&code.encode()).unwrap();
            assert_eq!(parsed, code);
        }
    }

    #[test]
    fn encoded_form_carries_a_human_readable_prefix() {
        assert!(sample(PasscodeType::Master).encode().starts_with("MASTER-"));
        assert!(sample(PasscodeType::Guest).encode().starts_with("GUEST-"));
    }

    #[test]
    fn surrounding_whitespace_from_a_paste_is_tolerated() {
        let encoded = sample(PasscodeType::Master).encode();
        let parsed = ConnectionCode::parse(&format!("  {encoded}\n")).unwrap();
        assert_eq!(parsed, sample(PasscodeType::Master));
    }

    #[test]
    fn a_truncated_paste_is_rejected() {
        // The whole point of base58check: a partial copy must not parse.
        let encoded = sample(PasscodeType::Master).encode();
        let truncated = &encoded[..encoded.len() - 6];
        assert!(ConnectionCode::parse(truncated).is_err());
    }

    #[test]
    fn a_single_character_corruption_is_rejected() {
        let encoded = sample(PasscodeType::Guest).encode();
        let mut chars: Vec<char> = encoded.chars().collect();
        // Swap a character in the body for a different valid base58 digit.
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'z' { 'y' } else { 'z' };
        let corrupted: String = chars.into_iter().collect();
        assert!(ConnectionCode::parse(&corrupted).is_err());
    }

    #[test]
    fn junk_input_is_rejected_with_a_useful_message() {
        assert!(ConnectionCode::parse("").is_err());
        assert!(ConnectionCode::parse("no-separator-here").is_err());

        let err = ConnectionCode::parse("hello world").unwrap_err().to_string();
        assert!(err.contains("connection code"), "unhelpful message: {err}");

        let err = ConnectionCode::parse("BANANA-abc").unwrap_err().to_string();
        assert!(err.contains("prefix"), "unhelpful message: {err}");
    }

    #[test]
    fn prefix_and_type_byte_must_agree() {
        // Re-label a GUEST code as MASTER without touching its payload.
        let guest = sample(PasscodeType::Guest).encode();
        let body = guest.split_once('-').unwrap().1;
        let forged = format!("MASTER-{body}");

        let err = ConnectionCode::parse(&forged).unwrap_err().to_string();
        assert!(err.contains("prefix"), "expected a mismatch error, got: {err}");
    }

    #[test]
    fn distinct_endpoints_produce_distinct_codes() {
        let a = ConnectionCode::new(PasscodeType::Master, [1u8; 32], [2u8; 32]);
        let b = ConnectionCode::new(PasscodeType::Master, [1u8; 32], [3u8; 32]);
        assert_ne!(a.encode(), b.encode(), "endpoint must be part of the code");
    }

    #[test]
    fn redacted_form_hides_the_body() {
        let code = sample(PasscodeType::Master);
        let redacted = code.redacted();
        assert!(redacted.starts_with("MASTER-"));
        let body = code.encode().split_once('-').unwrap().1.to_string();
        assert!(!redacted.contains(&body[..body.len() - 4]));
    }
}
