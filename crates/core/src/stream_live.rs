//! Stream go-live send contract: the pure claim → send → settle decision
//! table for Twitch `stream.online` announcements.
//!
//! For one validated [`StreamOnline`](crate::stream_funnel::StreamOnline):
//! check the broadcaster allowlist, render the post, claim the go-live row
//! ([`ClaimOutcome`](crate::stream_live_store::ClaimOutcome)), and send it to
//! the announcements channel. The sender MUST send with
//! `allowed_mentions: { parse: [] }`; that is the `LiveSender` contract in
//! `stream_live_store`, and the bot's executor-backed implementation posts
//! through the shared executor shape that pins it. Only a provably-unsent
//! send failure releases the claim (the
//! store deletes it only for the token-holding owner:
//! [`release_live`](crate::stream_live_store::release_live) before the
//! send starts,
//! [`release_started_live`](crate::stream_live_store::release_started_live)
//! after it starts) and
//! answers 5xx so Twitch retries; claim and stamp failures answer
//! 5xx WITHOUT releasing, because a stamped-failed send may already have
//! reached Discord. A retry of a started or delivered claim is acknowledged
//! with 2xx and skipped, while a retry of a fresh never-started claim is
//! answered 5xx (`InFlight`), which asks Twitch to retry instead of
//! treating the go-live as delivered. `claim_live` classifies a
//! conflicting row itself, so that answer needs no follow-up call.
//! Never-started claims expire on a bounded lease
//! (`STALE_CLAIM_LEASE_SECS`, 30 s) via atomic stale takeover; started rows
//! never expire and are never reclaimed, while delivered rows expire past
//! the dedup horizon on the retention cadence (see
//! [`crate::stream_live_store::expire_delivered_live`]). Twitch documents
//! no retry schedule, so recovery depends on a retry arriving after the
//! lease; there is no sweeper in this slice.
//!
//! [`disposition`] is the table the receiver executes; every row is pinned
//! below. The async glue (claim → start → post → stamp-or-release, with
//! stale takeover of never-started claims inside
//! [`crate::stream_live_store::claim_live`]) is
//! [`crate::stream_live_store::deliver_go_live`], which the receiver calls
//! for a verified notification when delivery dependencies are injected. It
//! calls `mark_live_send_started` before the first Discord POST so an
//! ambiguous send (posted but unstamped) stays fenced as `Duplicate`
//! forever.

/// What one pipeline stage decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Broadcaster is not in the allowlist: acknowledge, never post.
    AllowlistDenied,
    /// Invite URL failed validation: server misconfiguration.
    InvalidInvite,
    /// The claim insert hit a database error.
    ClaimFailed,
    /// The go-live row already exists (retry, replay, or restart).
    Duplicate,
    /// A never-started claim exists but its lease is still fresh: answer
    /// 5xx, asking Twitch to retry after the lease can expire or the owner
    /// settles the row.
    InFlight,
    /// This delivery owns the post but the Discord send provably never
    /// reached Discord (never a timeout, an ambiguous result, or a stamp
    /// failure after a possibly-successful send).
    SendFailed,
    /// Sent and stamped with the Discord message receipt.
    Sent,
    /// Sent, but stamping the receipt failed; the row stays claimed.
    StampFailed,
}

/// What the receiver must do for a [`Stage`]: the HTTP status to answer
/// Twitch with, and whether the claim row must be released first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disposition {
    /// 200 acknowledges (posted, duplicate, or not ours); 500 asks Twitch
    /// to retry.
    pub status: u16,
    /// Release the unposted claim so the retry can claim again. Only set
    /// when the send provably never reached Discord (`SendFailed`); the
    /// store honours it only for the claim-token owner, via `release_live`
    /// for a never-started row or `release_started_live` for a started
    /// one. An ambiguous or stamp-failed send never sets this, so its row
    /// stays fenced.
    pub release: bool,
}

/// The full claim → send → settle decision table.
#[must_use]
pub fn disposition(stage: Stage) -> Disposition {
    match stage {
        Stage::AllowlistDenied | Stage::Duplicate | Stage::Sent => Disposition {
            status: 200,
            release: false,
        },
        Stage::InvalidInvite | Stage::ClaimFailed | Stage::StampFailed | Stage::InFlight => {
            Disposition {
                status: 500,
                release: false,
            }
        }
        Stage::SendFailed => Disposition {
            status: 500,
            release: true,
        },
    }
}

/// What announcing one go-live did, for receiver logs and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoLiveOutcome {
    Posted { message_id: String },
    Duplicate,
    NotAllowed,
    Failed { reason: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition_table_pins_every_stage() {
        for (stage, status, release) in [
            (Stage::AllowlistDenied, 200, false),
            (Stage::Duplicate, 200, false),
            (Stage::Sent, 200, false),
            (Stage::InvalidInvite, 500, false),
            (Stage::ClaimFailed, 500, false),
            (Stage::StampFailed, 500, false),
            (Stage::InFlight, 500, false),
            (Stage::SendFailed, 500, true),
        ] {
            assert_eq!(
                disposition(stage),
                Disposition { status, release },
                "{stage:?} must map to status {status}, release {release}"
            );
        }
    }

    #[test]
    fn only_send_failure_releases_the_claim() {
        for stage in [
            Stage::AllowlistDenied,
            Stage::InvalidInvite,
            Stage::ClaimFailed,
            Stage::Duplicate,
            Stage::InFlight,
            Stage::Sent,
            Stage::StampFailed,
        ] {
            assert!(!disposition(stage).release, "{stage:?} must never release");
        }
        assert!(disposition(Stage::SendFailed).release);
    }

    #[test]
    fn failures_answer_5xx_so_twitch_retries() {
        for stage in [
            Stage::InvalidInvite,
            Stage::ClaimFailed,
            Stage::SendFailed,
            Stage::StampFailed,
            Stage::InFlight,
        ] {
            assert_eq!(
                disposition(stage).status,
                500,
                "{stage:?} must ask for retry"
            );
        }
    }

    #[test]
    fn posted_duplicate_and_denied_acknowledge_with_2xx() {
        for stage in [Stage::Sent, Stage::Duplicate, Stage::AllowlistDenied] {
            assert_eq!(disposition(stage).status, 200, "{stage:?} must not retry");
        }
    }
}
