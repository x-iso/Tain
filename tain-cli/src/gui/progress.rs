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
    /// Whether the connection stopped here rather than moving on.
    pub failed: bool,
}

/// What each stage produced, for the caption under its node.
///
/// # Why the graph shows results and not only steps
///
/// A row of stage names says how far a connection got and nothing about what
/// it found on the way, which leaves the most interesting question — *which*
/// route located this host — answerable only by hovering something else. Every
/// stage that has an answer worth six characters now carries it under its own
/// node.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outcomes {
    /// Which lookup found the host, if one did.
    pub found_via: Option<Branch>,
    /// Whether the connection is being carried by a relay.
    ///
    /// Only ever true while a session is failing or about to be: a relayed
    /// session is refused. It is here because it is what separates the two
    /// ways of arriving without a lookup — see [`Located`].
    pub on_relay: bool,
    /// Where the direct path ended up, and how far away that is.
    pub peer: Option<String>,
    pub rtt_ms: Option<f32>,
    /// What the picture is actually running at.
    pub fps: Option<f32>,
}

/// How the host was reached, including the ways that involved no lookup.
///
/// # Why "known" was not good enough
///
/// The first version of this said "known" whenever no address lookup had
/// answered, which folded together three situations that want different
/// responses — and made the common case look like the interesting one.
///
/// A connection code carries a relay, so most dials never need a lookup at all
/// and "known" was mostly saying "the code worked". It was also what a session
/// showed after arriving on a *stale* remembered address, and after arriving on
/// the code's relay with nothing else having contributed — which is the exact
/// state that then fails at `Direct`, and the one place where knowing which of
/// the three happened tells you what to do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Located {
    /// An address lookup answered. See [`Branch`].
    Looked(Branch),
    /// An address this host answered on before, tried first and reachable.
    Remembered,
    /// The relay named in the connection code, and nothing else.
    Code,
}

impl Located {
    pub fn label(self) -> &'static str {
        match self {
            Self::Looked(branch) => branch.label(),
            Self::Remembered => "saved",
            Self::Code => "code",
        }
    }

    pub fn describe(self) -> String {
        match self {
            Self::Looked(branch) => branch.describe().to_string(),
            Self::Remembered => {
                "Reached on an address this host answered on before, so nothing had to be \
                 looked up. Addresses are raced, so a remembered one that has gone stale \
                 simply loses rather than holding anything back."
                    .to_string()
            }
            Self::Code => {
                "Reached through the relay carried in the connection code. No address lookup \
                 contributed anything, so the relay is the only route there was — which is \
                 why a direct path has to be punched from it, and why there is nothing else \
                 to fall back on if that fails."
                    .to_string()
            }
        }
    }
}

impl Outcomes {
    /// How the host was reached, once `Find` is behind us.
    ///
    /// Derived rather than reported, because the three cases are distinguished
    /// by what actually happened: a lookup noting an answer, or — failing that
    /// — whether the connection landed on a relay, which only the code could
    /// have supplied. A direct path with no lookup behind it can only have come
    /// from a remembered address, since nothing else offers one.
    pub fn located(&self) -> Located {
        match self.found_via {
            Some(branch) => Located::Looked(branch),
            None if self.on_relay => Located::Code,
            None => Located::Remembered,
        }
    }
}

impl Stage {
    /// The short result to put under this node, once the stage is behind us.
    ///
    /// Only called for completed stages, which is what lets `Find` report
    /// "known" rather than having to distinguish "nothing looked yet" from
    /// "nothing needed to look".
    pub fn outcome(self, outcomes: &Outcomes) -> Option<String> {
        match self {
            Self::Find => Some(outcomes.located().label().to_string()),
            Self::Direct => outcomes.rtt_ms.map(|ms| format!("{ms:.0} ms")),
            Self::Video => outcomes.fps.map(|fps| format!("{fps:.0} fps")),
            // Starting, authenticating and opening a session either happen or
            // do not. A caption saying "done" under a node already drawn as
            // done is noise.
            Self::Start | Self::Auth | Self::Session => None,
        }
    }

    /// The short reason to put under this node when it is where things stopped.
    ///
    /// A red dot says a connection failed here; this says what it was left
    /// holding when it did, which for the common failure — arriving on a relay
    /// and never getting off it — is the whole diagnosis.
    pub fn stopped_on(self, outcomes: &Outcomes) -> Option<String> {
        match self {
            Self::Direct if outcomes.on_relay => Some("relayed".to_string()),
            Self::Find => Some("not found".to_string()),
            Self::Auth => Some("refused".to_string()),
            _ => None,
        }
    }
}

/// Where the graph should be drawn, for a session that may have failed.
///
/// `furthest` is the highest stage this session ever reached — see
/// `SharedState::set_status`, which is where it is kept. A failure needs it
/// because `GuestStatus::Failed` carries a message and nothing else, and a
/// graph that vanishes at the moment it becomes most useful is the wrong
/// answer: the step it stopped on is exactly what someone wants to see.
pub fn resting(status: &GuestStatus, furthest: Option<usize>) -> Option<Progress> {
    match status {
        GuestStatus::Failed(_) => {
            Some(Progress { at: furthest.unwrap_or(0), done: false, failed: true })
        }
        live => progress(live),
    }
}

/// Where this status sits on the path, if it is on it at all.
///
/// `None` for a session that has not started or has failed — see [`resting`],
/// which is what the window calls, and which can place a failure.
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
    Some(Progress { at, done: at == Stage::ALL.len() - 1, failed: false })
}

/// The whole path, written out, with the branch that found the host.
///
/// Shown on hover once a session is running, where the row of stages has been
/// replaced by the single word that matters — see the session window. Reading
/// it should answer "how did this connection come about" without needing the
/// log.
pub fn summary(progress: Option<Progress>, outcomes: &Outcomes) -> String {
    let Some(progress) = progress else {
        return "Not connected.".to_string();
    };

    let mut lines = Vec::with_capacity(Stage::ALL.len());
    for (index, stage) in Stage::ALL.iter().enumerate() {
        let complete = index < progress.at || progress.done;
        let mark = if complete {
            '✓'
        } else if index == progress.at {
            // The step it stopped on, versus the step it is working on.
            if progress.failed {
                '✗'
            } else {
                '·'
            }
        } else {
            ' '
        };
        let stopped = index == progress.at && progress.failed;
        let detail = match stage {
            Stage::Find if complete => {
                let located = outcomes.located();
                format!("{} — {}", located.label(), located.describe())
            }
            Stage::Direct if complete => match outcomes.peer.as_deref() {
                Some(peer) => format!("peer to peer with {peer}"),
                None => stage.describe().to_string(),
            },
            // What it was left holding when it stopped, which is more use than
            // repeating what the step was supposed to do.
            other if stopped => match other.stopped_on(outcomes) {
                Some(reason) => format!("{reason} — {}", other.describe()),
                None => other.describe().to_string(),
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
        let outcomes = Outcomes {
            found_via: Some(Branch::Dht),
            peer: Some("192.168.0.10:61369".into()),
            ..Outcomes::default()
        };
        let text = summary(done, &outcomes);

        assert!(text.contains("DHT"), "the branch is the point of this: {text}");
        assert!(text.contains("192.168.0.10:61369"), "the peer address is missing: {text}");
        for stage in Stage::ALL {
            assert!(text.contains(stage.label()), "{} is missing from the path", stage.label());
        }
        assert_eq!(text.matches('✓').count(), Stage::ALL.len(), "a finished path is all ticks");
    }

    /// The three ways of arriving are told apart, not lumped together.
    ///
    /// # Why this matters more than it looks
    ///
    /// All three used to read "known", which made the common case — a code that
    /// carried a relay, needing no lookup at all — indistinguishable from the
    /// one that then fails at `Direct`. Someone looking at a failed connection
    /// could not tell "nothing looked because nothing needed to" from "nothing
    /// looked, so the relay was the only route there ever was".
    #[test]
    fn how_the_host_was_reached_is_never_just_known() {
        let looked = Outcomes { found_via: Some(Branch::Lan), ..Outcomes::default() };
        assert_eq!(looked.located(), Located::Looked(Branch::Lan));
        assert_eq!(looked.located().label(), "LAN");

        // No lookup answered and the connection is on a relay: the only thing
        // that could have supplied it is the relay in the code.
        let by_code = Outcomes { found_via: None, on_relay: true, ..Outcomes::default() };
        assert_eq!(by_code.located(), Located::Code);

        // No lookup answered and the path is direct: nothing but a remembered
        // address offers a direct address, so that is where it came from.
        let remembered = Outcomes { found_via: None, on_relay: false, ..Outcomes::default() };
        assert_eq!(remembered.located(), Located::Remembered);

        // A lookup answering always wins, relay or not: it is what supplied
        // the address, whatever the connection then settled on.
        let both = Outcomes { found_via: Some(Branch::Dht), on_relay: true, ..Outcomes::default() };
        assert_eq!(both.located(), Located::Looked(Branch::Dht));
    }

    /// A failure says what it was left holding, not what it meant to do.
    #[test]
    fn a_stopped_stage_reports_what_stopped_it() {
        let relayed = Outcomes { on_relay: true, ..Outcomes::default() };
        assert_eq!(Stage::Direct.stopped_on(&relayed).as_deref(), Some("relayed"));

        // Reached Direct and stopped there, on a relay, with the code the only
        // thing that had located the host: the whole of the failure, in a row.
        let at = resting(&GuestStatus::Failed("no direct path".into()), Some(2)).expect("placed");
        let text = summary(Some(at), &relayed);
        assert!(text.contains("relayed"), "the failure does not say what it stopped on: {text}");
        assert!(text.contains("code"), "the failure does not say how it got there: {text}");
    }

    /// A failure stays on the graph, at the step it failed on.
    ///
    /// The whole reason `furthest` is carried: `GuestStatus::Failed` has only a
    /// message, so without it the graph either vanishes — which is what it used
    /// to do, exactly when it was most worth seeing — or claims the connection
    /// never started.
    #[test]
    fn a_failure_is_shown_where_it_happened() {
        let failed = GuestStatus::Failed("no direct path".into());

        // Reached `Direct` and stopped there.
        let at = resting(&failed, Some(2)).expect("a failure has a place on the path");
        assert_eq!(at.at, 2, "the failure moved off the step it happened on");
        assert!(at.failed);
        assert!(!at.done, "a failure never counts as finished");

        let text = summary(Some(at), &Outcomes::default());
        assert!(text.contains('✗'), "the failed step is not marked: {text}");
        assert_eq!(text.matches('✓').count(), 2, "the two steps that did work should stand");
        // The step it failed on has not produced a result, so it must not
        // claim one — this is what a summary written from `complete` avoids.
        assert!(!text.contains("peer to peer"), "a failed step claimed a result: {text}");
    }

    /// Only a completed stage gets to report an outcome.
    #[test]
    fn an_unfinished_stage_reports_nothing() {
        let outcomes = Outcomes {
            found_via: Some(Branch::Lan),
            rtt_ms: Some(3.0),
            fps: Some(58.0),
            ..Outcomes::default()
        };
        assert_eq!(Stage::Find.outcome(&outcomes).as_deref(), Some("LAN"));
        assert_eq!(Stage::Direct.outcome(&outcomes).as_deref(), Some("3 ms"));
        assert_eq!(Stage::Video.outcome(&outcomes).as_deref(), Some("58 fps"));
        // Nothing to say, rather than something empty to draw.
        assert_eq!(Stage::Start.outcome(&outcomes), None);
        assert_eq!(Stage::Auth.outcome(&outcomes), None);
    }
}
