//! How far a connection has got, drawn as the path it has taken.
//!
//! # Why a graph and not a sentence
//!
//! Connecting is five or six separate things that each take real time and can
//! each fail on their own, and the status line can only ever show the one
//! happening now. That is enough to answer "what is it doing" and no help at
//! all with "how much of this is left", which is the question someone watching
//! a spinner is actually asking.
//!
//! So the same steps are drawn as a row that fills up. Everything behind the
//! current one is done, the current one is lit, and what is left is visibly
//! what is left.
//!
//! The stages are deliberately coarser than [`DialStep`]. That enum exists to
//! say precisely which thing failed, which wants detail; this exists to be
//! read at a glance while waiting, which wants the opposite. The exact step
//! stays in the line of text underneath.

use tain_core::discovery::Branch;
use tain_core::network::DialStep;

use super::network_bridge::{GuestStatus, Waiting};

/// One step of the path from "Connect" to a moving picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Opening this machine's own endpoint.
    Start,
    /// Working out where the host is. The branch that answers this is the one
    /// reported afterwards — see [`Branch`].
    Find,
    /// Hole punching, until the connection is carried peer to peer.
    Direct,
    /// The passcode, and the host's decision about it.
    Auth,
    /// The host has accepted and is opening the session.
    Session,
    /// Frames are arriving and being decoded.
    Video,
}

impl Stage {
    pub const ALL: [Stage; 6] =
        [Self::Start, Self::Find, Self::Direct, Self::Auth, Self::Session, Self::Video];

    /// One word, because six of these sit in a row.
    pub fn label(self) -> &'static str {
        match self {
            Self::Start => "Start",
            Self::Find => "Find",
            Self::Direct => "Direct",
            Self::Auth => "Auth",
            Self::Session => "Session",
            Self::Video => "Video",
        }
    }

    /// What this step is waiting on, for the tooltip.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Start => "Opening this machine's own connection",
            Self::Find => "Working out where the host is",
            Self::Direct => "Opening a direct path, so no relay carries the video",
            Self::Auth => "Sending the passcode, and waiting for the host to accept it",
            Self::Session => "The host is starting its capture and encoder",
            Self::Video => "Frames are arriving and decoding",
        }
    }
}

/// How far along a connection is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Index into [`Stage::ALL`] of the stage in progress, or the last one
    /// reached once everything is done.
    pub at: usize,
    /// Whether the last stage has been reached and the picture is moving.
    pub done: bool,
}

/// Where this status sits on the path, if it is on it at all.
///
/// `None` for a session that has not started or has failed. A failure keeps its
/// own message, which names the step that did not finish — the graph would only
/// repeat it less precisely, and `GuestStatus::Failed` does not carry the step
/// anyway.
pub fn progress(status: &GuestStatus) -> Option<Progress> {
    let at = match status {
        GuestStatus::Connecting(DialStep::Starting) => 0,
        // The three dialling steps differ in *how* the host is being looked
        // for, which the text underneath says. All of them are this stage.
        GuestStatus::Connecting(
            DialStep::ReachingRelay | DialStep::ViaRelay | DialStep::Direct,
        ) => 1,
        GuestStatus::Connecting(DialStep::Punching) => 2,
        // Waiting on the host's decision is still the passcode being dealt
        // with, from the point of view of someone watching progress.
        GuestStatus::Connecting(DialStep::Authenticating) | GuestStatus::AwaitingApproval => 3,
        // `Decoding` means frames are arriving and none are decoding, which is
        // a fault rather than progress — so it does not advance to `Video`.
        GuestStatus::Authenticated(
            Waiting::Session | Waiting::FirstFrame | Waiting::Decoding,
        ) => 4,
        GuestStatus::Streaming => 5,
        GuestStatus::Disconnected | GuestStatus::Failed(_) => return None,
    };
    Some(Progress { at, done: at == Stage::ALL.len() - 1 })
}

/// The whole path, written out, with the branch that found the host.
///
/// Shown on hover once a session is running, where the row of stages has been
/// replaced by the single word that matters — see the session window. Reading
/// it should answer "how did this connection come about" without needing the
/// log.
pub fn summary(progress: Option<Progress>, branch: Option<Branch>, remote: Option<&str>) -> String {
    let Some(progress) = progress else {
        return "Not connected.".to_string();
    };

    let mut lines = Vec::with_capacity(Stage::ALL.len() + 2);
    for (index, stage) in Stage::ALL.iter().enumerate() {
        let mark = if index < progress.at || progress.done {
            '✓'
        } else if index == progress.at {
            '·'
        } else {
            ' '
        };
        let detail = match stage {
            Stage::Find => match branch {
                Some(branch) => format!("{} — {}", branch.label(), branch.describe()),
                // Nothing had to look: a relay in the code, or an address
                // remembered from a connection that already worked.
                None => "already knew where to look".to_string(),
            },
            Stage::Direct => match remote {
                Some(remote) => format!("peer to peer with {remote}"),
                None => stage.describe().to_string(),
            },
            other => other.describe().to_string(),
        };
        lines.push(format!("{mark} {:<8} {detail}", stage.label()));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every status a live session can be in has a place on the path.
    ///
    /// Written as a sweep rather than one case at a time because the failure
    /// that matters is a *new* status having no stage — which a test naming the
    /// existing ones one by one would not notice.
    #[test]
    fn every_active_status_sits_somewhere_on_the_path() {
        let statuses = [
            GuestStatus::Connecting(DialStep::Starting),
            GuestStatus::Connecting(DialStep::ReachingRelay),
            GuestStatus::Connecting(DialStep::ViaRelay),
            GuestStatus::Connecting(DialStep::Direct),
            GuestStatus::Connecting(DialStep::Punching),
            GuestStatus::Connecting(DialStep::Authenticating),
            GuestStatus::AwaitingApproval,
            GuestStatus::Authenticated(Waiting::Session),
            GuestStatus::Authenticated(Waiting::FirstFrame),
            GuestStatus::Authenticated(Waiting::Decoding),
            GuestStatus::Streaming,
        ];
        for status in statuses {
            let progress = progress(&status).unwrap_or_else(|| {
                panic!("{status:?} is a live status with nowhere to sit on the path")
            });
            assert!(progress.at < Stage::ALL.len(), "{status:?} is off the end");
        }
    }

    /// The path only ever moves forwards.
    ///
    /// The order above is the order a connection happens in, so a status later
    /// in that list must never sit earlier on the path. This is what catches a
    /// stage being inserted in the wrong place, which a per-case test cannot.
    #[test]
    fn the_path_never_goes_backwards() {
        let in_order = [
            GuestStatus::Connecting(DialStep::Starting),
            GuestStatus::Connecting(DialStep::ViaRelay),
            GuestStatus::Connecting(DialStep::Punching),
            GuestStatus::Connecting(DialStep::Authenticating),
            GuestStatus::Authenticated(Waiting::Session),
            GuestStatus::Streaming,
        ];
        let mut previous = 0;
        for status in in_order {
            let at = progress(&status).expect("live status").at;
            assert!(at >= previous, "{status:?} sits at {at}, behind the step before it");
            previous = at;
        }
    }

    /// Only the last stage counts as finished.
    #[test]
    fn only_a_moving_picture_finishes_the_path() {
        assert!(progress(&GuestStatus::Streaming).unwrap().done);
        for status in [
            GuestStatus::Connecting(DialStep::Punching),
            GuestStatus::AwaitingApproval,
            // Arriving but not decoding is a fault, and must not read as
            // having got there.
            GuestStatus::Authenticated(Waiting::Decoding),
        ] {
            assert!(!progress(&status).unwrap().done, "{status:?} must not read as finished");
        }
    }

    /// A finished session's summary says how the host was found.
    #[test]
    fn the_summary_names_the_branch_that_found_the_host() {
        let done = progress(&GuestStatus::Streaming);
        let text = summary(done, Some(Branch::Dht), Some("192.168.0.10:61369"));

        assert!(text.contains("DHT"), "the branch is the point of this: {text}");
        assert!(text.contains("192.168.0.10:61369"), "the peer address is missing: {text}");
        for stage in Stage::ALL {
            assert!(text.contains(stage.label()), "{} is missing from the path", stage.label());
        }
        assert_eq!(text.matches('✓').count(), Stage::ALL.len(), "a finished path is all ticks");
    }

    /// Nothing to look up is a real answer, not a missing one.
    #[test]
    fn a_host_that_needed_no_lookup_says_so() {
        let text = summary(progress(&GuestStatus::Streaming), None, None);
        assert!(text.contains("already knew"), "{text}");
    }
}
