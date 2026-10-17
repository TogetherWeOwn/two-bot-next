//! Go-live delivery dependencies for the private EventSub receiver.
//!
//! The receiver stays default-dark on `TWO_EVENTSUB`; delivery is a second,
//! independent gate (`TWO_EVENTSUB_DELIVERY`, parsed by
//! `two_bot_core::eventsub_delivery_config`). While delivery is dark, boot
//! injects no [`DeliveryDeps`] and a verified notification is acknowledged
//! without a send exactly as before. When delivery is enabled, boot builds
//! one `DeliveryDeps` from the bot pool ledger and an admission-backed REST
//! executor ([`DeliveryDeps::production`]) and the receiver runs
//! `deliver_go_live` (durable claim → start → send → settle) for every
//! verified `stream.online`. Nothing here reads the environment.

use std::sync::Arc;

use two_bot_core::stream_live::GoLiveOutcome;
use two_bot_core::stream_live_store::{
    LiveDeliveryConfig, LiveLedger, LiveSendError, LiveSender, SendFuture,
};
use two_bot_discord::ActionExecutor;

/// Injected delivery dependencies: the durable ledger, the one-shot
/// sender, and the static allowlist/invite/channel inputs. Held behind
/// trait objects so receiver tests inject in-memory fakes and no test
/// touches a database or the Discord wire. The production shape is the
/// bot's Postgres pool as `ledger` (it implements `LiveLedger`) and an
/// admission-backed REST executor as `sender` (below); boot constructs
/// that via [`DeliveryDeps::production`] when delivery is enabled.
pub struct DeliveryDeps {
    pub ledger: Arc<dyn LiveLedger>,
    pub sender: Arc<dyn LiveSender>,
    pub config: LiveDeliveryConfig,
}

impl DeliveryDeps {
    /// Production dependencies: the bot Postgres pool as the durable
    /// ledger and the executor (built by boot with the pool-backed send
    /// admission) as the sender, with the validated delivery inputs
    /// mapped to the store's static config.
    #[must_use]
    pub fn production(
        pool: sqlx::PgPool,
        executor: ActionExecutor,
        config: &two_bot_core::eventsub_delivery_config::EventsubDeliveryConfig,
    ) -> Self {
        Self {
            ledger: Arc::new(pool),
            sender: Arc::new(ExecutorSender { executor }),
            config: LiveDeliveryConfig {
                broadcasters: config.broadcasters().to_vec(),
                invite_url: config.invite_url().to_owned(),
                channel_id: config.channel_id().to_owned(),
            },
        }
    }
}

/// The shared executor behind a local newtype is the production
/// `LiveSender` (the orphan rule keeps the impl off `ActionExecutor`
/// itself: neither it nor `LiveSender` lives in this crate).
/// `post_message` builds the exact no-ping body
/// (`allowed_mentions: { parse: [] }`, pinned in
/// `crates/discord/src/executor.rs`) and makes a single attempt.
/// Only an error that proves no effect (explicit rejection, 429, or a
/// local guard refusal) may release the claim; timeouts and transport
/// failures are ambiguous and stay fenced.
pub struct ExecutorSender {
    executor: ActionExecutor,
}

impl LiveSender for ExecutorSender {
    fn send_live<'a>(&'a self, channel_id: &'a str, content: &'a str) -> SendFuture<'a> {
        Box::pin(async move {
            match self.executor.post_message(channel_id, content, None).await {
                Ok(message_id) => Ok(message_id),
                Err(error) if error.proves_no_effect() => Err(LiveSendError::ProvenNoSend),
                Err(_) => Err(LiveSendError::Ambiguous),
            }
        })
    }
}

/// Bounded `eventsub_post` outcome labels for one delivery, as
/// `(attempted, succeeded, failed)` increments. Only deliveries that
/// reached the sender count as attempted; a stamp failure after a landed
/// post counts as failed because the delivery did not settle. Labels are
/// fixed literals at the call site, never formatted from delivery data.
pub fn post_metric_counts(outcome: &GoLiveOutcome) -> (bool, bool, bool) {
    match outcome {
        GoLiveOutcome::Posted { .. } => (true, true, false),
        GoLiveOutcome::Failed { reason } => {
            let attempted = matches!(*reason, "send_failed" | "ambiguous_send" | "stamp_failed");
            (attempted, false, attempted)
        }
        GoLiveOutcome::Duplicate | GoLiveOutcome::NotAllowed => (false, false, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_metric_counts_only_sender_reaching_outcomes() {
        assert_eq!(
            post_metric_counts(&GoLiveOutcome::Posted {
                message_id: "1".to_owned()
            }),
            (true, true, false)
        );
        for reason in ["send_failed", "ambiguous_send", "stamp_failed"] {
            assert_eq!(
                post_metric_counts(&GoLiveOutcome::Failed { reason }),
                (true, false, true),
                "{reason}"
            );
        }
        for outcome in [
            GoLiveOutcome::Duplicate,
            GoLiveOutcome::NotAllowed,
            GoLiveOutcome::Failed {
                reason: "claim_failed",
            },
            GoLiveOutcome::Failed {
                reason: "invalid_invite",
            },
            GoLiveOutcome::Failed {
                reason: "in_flight",
            },
        ] {
            assert_eq!(
                post_metric_counts(&outcome),
                (false, false, false),
                "{outcome:?}"
            );
        }
    }
}
