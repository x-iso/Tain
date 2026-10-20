//! Self-contained connection codes.
//!
//! A code carries everything a guest needs to reach a host: the authorization
//! level, the shared secret, the host's iroh endpoint ID, and — since v2 — the
//! relay it can be reached through.
//!
//! ```text
//! MASTER-<base58check( [type] + [32-byte secret] + [32-byte endpoint id] )>
//! MASTER-<base58check( [type|0x80] + [secret] + [endpoint id] + [relay url] )>
//! ```
//!
//! ## Why the relay URL is in the code
//!
//! An endpoint ID on its own is a public key, not an address. Reaching the host
//! from one means asking iroh's lookup service — a DNS query to
//! `dns.iroh.link` — to translate it. On a network where that query does not
//! return, the connection does not fail: it retries with a growing backoff and
//! hangs indefinitely, which is what "it just never connects" looks like from
//! the outside.
//!
//! With the relay URL travelling in the code there is nothing to look up. The
//! guest dials the relay directly, and hole punching proceeds from there as
//! usual. That removes the whole discovery step from the critical path, which
//! matters most on exactly the networks where it is least likely to work.
//!
//! Bundling the address into the code is what lets the UI be a single
//! copy button on the host and a single paste button on the guest — there is no
//! separate "ticket" for the user to shuttle around.
//!
//! Base58**check** is deliberate: these get pasted through chat clients, and a
//! truncated or mangled paste must fail loudly rather than producing a valid-
//! looking code that dials the wrong endpoint.
//!
//! ## The same code as a link
//!
//! ```text
//! tain://connect/MASTER-<base58check>
//! ```
//!
//! Nothing is added: a link is the code with a scheme in front, so whatever
//! the code carries — the relay included — the link carries too, and a link
//! pasted into the paste button works exactly as the code would. Windows hands
//! a clicked link to the program registered for the scheme, which is what lets
//! one click on another machine open Tain and connect.
//!
//! The code sits in the path and not where a host name would go, because
//! browsers lowercase the host part of a URL and base58 is case-sensitive: a
//! code there would arrive mangled and, thanks to the checksum, refused.

use anyhow::{Context, Result};

use crate::passcode::PasscodeType;

/// Bytes in a v1 code: 1 type + 32 secret + 32 endpoint id.
const PAYLOAD_LEN: usize = 1 + 32 + 32;

/// Set on the type byte when a relay URL follows the endpoint id.
///
/// A flag rather than a version field: the type byte only ever holds small
/// values, so the high bit was free, and a v1 code stays parseable by exactly
/// the same code path. Codes people saved before this existed keep working —
/// they simply have no relay to offer.
const RELAY_FLAG: u8 = 0x80;

/// The URL scheme Tain registers for itself.
pub const LINK_SCHEME: &str = "tain";

/// The part of a link before the code.
const LINK_PATH: &str = "connect/";

/// The code inside a `tain:` link, or `None` if `input` is not a link at all.
///
/// Lenient about what launchers and chat clients do to a URL on its way: the
/// scheme in any case, the slashes doubled or not, and a trailing slash, query
/// or fragment, which some browsers add. Strict about everything else, because
/// what comes back is handed to [`ConnectionCode::parse`] and its checksum.
pub fn code_in_link(input: &str) -> Option<&str> {
    let input = input.trim();
    let scheme = input.get(..LINK_SCHEME.len() + 1)?;
    if !scheme.eq_ignore_ascii_case(&format!("{LINK_SCHEME}:")) {
        return None;
    }
    let rest = &input[scheme.len()..];
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let rest = match rest.get(..LINK_PATH.len()) {
        Some(path) if path.eq_ignore_ascii_case(LINK_PATH) => &rest[LINK_PATH.len()..],
        _ => rest,
    };
    // Base58 has neither, so nothing of the code is cut off with them.
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    Some(rest.trim_end_matches('/'))
}

/// A parsed connection code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionCode {
    pub passcode_type: PasscodeType,
    /// The shared secret the host validates against its passcode store.
    pub secret: [u8; 32],
    /// The host's iroh endpoint ID (its public key).
    pub endpoint_id: [u8; 32],
    /// The relay the host was reachable through when the code was made.
    ///
    /// `None` for codes issued before this was carried, and for a host with no
    /// home relay. Advisory: the guest still hole-punches, and a stale relay
    /// only costs the lookup it was meant to save.
    pub relay_url: Option<String>,
}

impl ConnectionCode {
    pub fn new(passcode_type: PasscodeType, secret: [u8; 32], endpoint_id: [u8; 32]) -> Self {
        Self { passcode_type, secret, endpoint_id, relay_url: None }
    }

    /// The same, carrying the relay the host can be reached through.
    pub fn with_relay(
        passcode_type: PasscodeType,
        secret: [u8; 32],
        endpoint_id: [u8; 32],
        relay_url: Option<String>,
    ) -> Self {
        Self { passcode_type, secret, endpoint_id, relay_url }
    }

    /// Render as a shareable string.
    pub fn encode(&self) -> String {
        let mut payload = Vec::with_capacity(PAYLOAD_LEN);
        let relay = self.relay_url.as_deref().filter(|url| !url.is_empty());
        payload.push(self.passcode_type as u8 | if relay.is_some() { RELAY_FLAG } else { 0 });
        payload.extend_from_slice(&self.secret);
        payload.extend_from_slice(&self.endpoint_id);
        if let Some(url) = relay {
            payload.extend_from_slice(url.as_bytes());
        }

        let body = bs58::encode(&payload).with_check().into_string();
        format!("{}-{}", self.passcode_type.prefix(), body)
    }

    /// The same code addressed to `relay`, or `None` when that changes nothing
    /// worth changing.
    ///
    /// Only the address moves: the secret, and so whoever already holds the
    /// code, is untouched. And no relay is never an improvement on one — a host
    /// that has lost its relay for a moment is usually about to get it back,
    /// and a code naming the last one beats a code naming none.
    pub fn readdressed(&self, relay: Option<&str>) -> Option<Self> {
        let relay = relay.filter(|url| !url.is_empty())?;
        if self.relay_url.as_deref() == Some(relay) {
            return None;
        }
        Some(Self { relay_url: Some(relay.to_string()), ..self.clone() })
    }

    /// Render as a link that opens Tain and connects. See the module notes.
    pub fn link(&self) -> String {
        format!("{LINK_SCHEME}://{LINK_PATH}{}", self.encode())
    }

    /// Parse a code produced by [`Self::encode`], or a link from [`Self::link`].
    pub fn parse(input: &str) -> Result<Self> {
        let input = code_in_link(input).unwrap_or(input).trim();
        let (prefix, body) = input
            .split_once('-')
            .context("Not a connection code — expected PREFIX-<code>")?;

        let prefix_type = PasscodeType::from_prefix(prefix)
            .with_context(|| format!("Unknown passcode prefix '{prefix}'"))?;

        let payload = bs58::decode(body)
            .with_check(None)
            .into_vec()
            .context("Code is damaged or incomplete — check it was copied in full")?;

        if payload.len() < PAYLOAD_LEN {
            anyhow::bail!(
                "Code has the wrong length ({} bytes, expected at least {})",
                payload.len(),
                PAYLOAD_LEN
            );
        }

        let carries_relay = payload[0] & RELAY_FLAG != 0;
        if !carries_relay && payload.len() != PAYLOAD_LEN {
            anyhow::bail!(
                "Code has the wrong length ({} bytes, expected {})",
                payload.len(),
                PAYLOAD_LEN
            );
        }

        let passcode_type = match payload[0] & !RELAY_FLAG {
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

        // Rejected rather than lossily repaired: a relay URL that survived a
        // mangled paste as valid UTF-8 but wrong text would send the guest
        // somewhere else entirely, and the checksum exists to make that
        // impossible rather than merely unlikely.
        let relay_url = if carries_relay {
            Some(
                String::from_utf8(payload[65..].to_vec())
                    .context("Code carries a relay address that is not valid text")?,
            )
        } else {
            None
        };

        Ok(Self { passcode_type, secret, endpoint_id, relay_url })
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

    /// A relay in the code survives the round trip.
    #[test]
    fn a_relay_address_round_trips() {
        let code = ConnectionCode::with_relay(
            PasscodeType::Master,
            [7u8; 32],
            [9u8; 32],
            Some("https://euc1-1.relay.n0.iroh.link./".to_string()),
        );
        let parsed = ConnectionCode::parse(&code.encode()).expect("round trip");
        assert_eq!(parsed, code);
        assert_eq!(parsed.relay_url.as_deref(), Some("https://euc1-1.relay.n0.iroh.link./"));
    }

    /// Codes saved before relays were carried must keep working.
    ///
    /// People have these written down and pasted into chats. A format change
    /// that quietly invalidated them would look like the application breaking.
    #[test]
    fn a_code_without_a_relay_still_parses() {
        let code = ConnectionCode::new(PasscodeType::Guest, [3u8; 32], [4u8; 32]);
        let encoded = code.encode();
        let parsed = ConnectionCode::parse(&encoded).expect("round trip");

        assert_eq!(parsed.relay_url, None);
        assert_eq!(parsed.endpoint_id, [4u8; 32]);
        // The flag must not be set, or an old build would reject it.
        let payload = bs58::decode(encoded.split_once('-').unwrap().1)
            .with_check(None)
            .into_vec()
            .unwrap();
        assert_eq!(payload.len(), PAYLOAD_LEN);
        assert_eq!(payload[0] & RELAY_FLAG, 0);
    }

    /// An empty relay is the same as no relay, not a code with a blank one.
    #[test]
    fn an_empty_relay_is_omitted() {
        let code = ConnectionCode::with_relay(
            PasscodeType::Master,
            [1u8; 32],
            [2u8; 32],
            Some(String::new()),
        );
        assert_eq!(ConnectionCode::parse(&code.encode()).unwrap().relay_url, None);
    }

    /// The type byte still decides, with the flag masked off.
    #[test]
    fn the_flag_does_not_disturb_the_passcode_type() {
        for typ in [PasscodeType::Master, PasscodeType::Guest] {
            let code = ConnectionCode::with_relay(
                typ,
                [5u8; 32],
                [6u8; 32],
                Some("https://relay.example/".to_string()),
            );
            assert_eq!(ConnectionCode::parse(&code.encode()).unwrap().passcode_type, typ);
        }
    }

    /// A link is the code, relay and all, behind the scheme.
    #[test]
    fn a_link_round_trips_with_its_relay() {
        let code = ConnectionCode::with_relay(
            PasscodeType::Guest,
            [7u8; 32],
            [9u8; 32],
            Some("https://relay.example.org./".to_string()),
        );
        let link = code.link();
        assert!(link.starts_with("tain://connect/GUEST-"), "unexpected link: {link}");
        assert_eq!(ConnectionCode::parse(&link).expect("round trip"), code);
    }

    /// What launchers and browsers do to a URL must not cost the code.
    #[test]
    fn a_link_survives_what_launchers_do_to_it() {
        let code = sample(PasscodeType::Master);
        let encoded = code.encode();
        for link in [
            format!("tain://connect/{encoded}"),
            format!("TAIN://connect/{encoded}"),
            format!("tain://Connect/{encoded}/"),
            format!("tain:connect/{encoded}"),
            format!("tain://{encoded}"),
            format!("tain://connect/{encoded}?from=chat"),
            format!("  tain://connect/{encoded}#top\n"),
        ] {
            assert_eq!(ConnectionCode::parse(&link).ok(), Some(code.clone()), "{link}");
        }
    }

    /// Only a link is unwrapped; anything else reaches the parser as it was.
    #[test]
    fn only_a_link_is_unwrapped() {
        assert_eq!(code_in_link("MASTER-abc"), None);
        assert_eq!(code_in_link("https://connect/MASTER-abc"), None);
        assert_eq!(code_in_link("tai"), None);
        assert_eq!(code_in_link("tain://connect/MASTER-abc/"), Some("MASTER-abc"));
        // A link with no code in it is still a link, and fails as a code.
        assert_eq!(code_in_link("tain://connect/"), Some(""));
        assert!(ConnectionCode::parse("tain://connect/").is_err());
    }

    /// A host that moves relay keeps its secret; only the address changes.
    #[test]
    fn a_code_follows_its_host_to_a_new_relay() {
        let old = ConnectionCode::with_relay(
            PasscodeType::Master,
            [7u8; 32],
            [9u8; 32],
            Some("https://euc1-1.relay.n0.iroh.link./".to_string()),
        );
        let moved = old.readdressed(Some("https://relay.example.org/")).expect("a new relay");
        assert_eq!(moved.relay_url.as_deref(), Some("https://relay.example.org/"));
        assert_eq!((moved.secret, moved.endpoint_id), (old.secret, old.endpoint_id));
        assert_eq!(moved.passcode_type, old.passcode_type);

        // A code issued before any relay answered gets one when it does.
        let bare = sample(PasscodeType::Guest);
        assert!(bare.readdressed(Some("https://relay.example.org/")).is_some());
    }

    /// Nothing to do when the relay is the same, and never a step backwards to
    /// no relay at all.
    #[test]
    fn a_code_keeps_its_relay_rather_than_losing_it() {
        let code = ConnectionCode::with_relay(
            PasscodeType::Guest,
            [7u8; 32],
            [9u8; 32],
            Some("https://relay.example.org/".to_string()),
        );
        assert_eq!(code.readdressed(Some("https://relay.example.org/")), None);
        assert_eq!(code.readdressed(None), None);
        assert_eq!(code.readdressed(Some("")), None);
    }

    /// A browser that lowercases the whole thing produces a code that must be
    /// refused, not one that dials somewhere else.
    #[test]
    fn a_lowercased_link_is_refused() {
        let link = sample(PasscodeType::Guest).link().to_lowercase();
        assert!(ConnectionCode::parse(&link).is_err());
    }

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
