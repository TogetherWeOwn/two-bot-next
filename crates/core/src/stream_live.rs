//! Stream go-live send contract: the pure claim → send → settle decision
//! table for Twitch `stream.online` announcements.
//!
//! For one validated [`StreamOnline`](crate::stream_funnel::StreamOnline):
//! check the broadcaster allowlist, render the post, claim the go-live row
//! ([`ClaimOutcome`](crate::stream_live_store::ClaimOutcome)), and send it to
//! the announcements channel. The receiver MUST send with
//! `allowed_mentions: { parse: [] }`; this repository does not yet contain
//! that sender, so the no-ping guarantee is a receiver contract, not verified
//! behavior. On a provable send failure release the claim and answer 5xx so
//! Twitch retries; a retry of any existing claim is acknowledged with 2xx and
//! skipped. Claims do not expire, so the receiver must add stale-claim
//! recovery before wiring this core path into the runtime.
//!
//! [`disposition`] is the table the receiver executes; every row is pinned
//! below. The async glue (claim → post → stamp-or-release) belongs with the
//! receiver, which is not implemented in this slice.

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
    /// This delivery owns the post but the Discord send failed.
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
    /// when the send provably never reached Discord.
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
        Stage::InvalidInvite | Stage::ClaimFailed | Stage::StampFailed => Disposition {
            status: 500,
            release: false,
        },
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
