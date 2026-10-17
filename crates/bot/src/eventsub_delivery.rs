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
use two_bot_core::stream_live_store::{LiveDeliveryConfig, LiveLedger, LiveSender};
use two_bot_discord::ActionExecutor;

/// Injected delivery dependencies: the durable ledger, the one-shot
/// sender, and the static allowlist/invite/channel inputs. Held behind
/// trait objects so receiver tests inject in-memory fakes and no test
/// touches a database or the Discord wire. The production shape is the
/// bot's Postgres pool as `ledger` (it implements `LiveLedger`) and the
/// shared REST executor as `sender` (its `LiveSender` impl lives with the
/// type in `crates/discord/src/executor.rs`); boot constructs that via
/// [`DeliveryDeps::production`] when delivery is enabled.
pub struct DeliveryDeps {
    pub ledger: Arc<dyn LiveLedger>,
    pub sender: Arc<dyn LiveSender>,
    pub config: LiveDeliveryConfig,
}

impl DeliveryDeps {
    /// Production dependencies: the bot Postgres pool as the durable
    /// ledger and the shared REST executor (built by boot with the
    /// pool-backed send admission) as the sender, with the validated
    /// delivery inputs mapped to the store's static config.
    #[must_use]
    pub fn production(
        pool: sqlx::PgPool,
        executor: ActionExecutor,
        config: &two_bot_core::eventsub_delivery_config::EventsubDeliveryConfig,
    ) -> Self {
        Self {
            ledger: Arc::new(pool),
            sender: Arc::new(executor),
            config: LiveDeliveryConfig {
                broadcasters: config.broadcasters().to_vec(),
                invite_url: config.invite_url().to_owned(),
                channel_id: config.channel_id().to_owned(),
            },
        }
    }
}

/// Build the production delivery dependencies from already-validated boot
/// inputs: the bot pool, the Discord bot token, the optional API override
/// and the validated delivery config. Pure construction (no I/O, no sends);
/// `Err` carries the catalogued `error_class` boot refuses startup with.
pub fn build_delivery_deps(
    pool: sqlx::PgPool,
    token: &str,
    proxy: Option<String>,
    config: &two_bot_core::eventsub_delivery_config::EventsubDeliveryConfig,
) -> Result<DeliveryDeps, &'static str> {
    let admission = two_bot_core::send_admission::PgSendAdmission::new(pool.clone(), token)
        .map_err(|_| "receiver_prerequisites_invalid")?;
    let executor = ActionExecutor::with_admission(token.to_owned(), proxy, Arc::new(admission))
        .map_err(|_| "receiver_prerequisites_invalid")?;
    Ok(DeliveryDeps::production(pool, executor, config))
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

    fn delivery_vars() -> std::collections::HashMap<String, String> {
        std::collections::HashMap::from([
            ("TWO_EVENTSUB_DELIVERY".to_owned(), "1".to_owned()),
            ("TWO_TWITCH_BROADCASTERS".to_owned(), "222,111".to_owned()),
            (
                "TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID".to_owned(),
                "777".to_owned(),
            ),
            (
                "TWO_EVENTSUB_INVITE_URL".to_owned(),
                "https://discord.gg/example".to_owned(),
            ),
        ])
    }

    fn delivery_config() -> two_bot_core::eventsub_delivery_config::EventsubDeliveryConfig {
        two_bot_core::eventsub_delivery_config::EventsubDeliveryConfig::from_map(&delivery_vars())
            .expect("valid snapshot")
            .expect("enabled")
    }

    fn lazy_pool() -> sqlx::PgPool {
        // Never connects: pool construction performs no I/O.
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://agent_test@agent-testdb/agent_test")
            .expect("lazy pool never connects")
    }

    fn governed_executor(pool: &sqlx::PgPool, token: &str) -> ActionExecutor {
        crate::gateway::ensure_crypto_provider();
        // Pure key derivation over a synthetic token; no I/O, no live request.
        let admission = Arc::new(
            two_bot_core::send_admission::PgSendAdmission::new(pool.clone(), token)
                .expect("synthetic token derives a key"),
        );
        ActionExecutor::with_admission(token.to_owned(), None, admission)
            .expect("governed admission builds for the real origin")
    }

    #[tokio::test]
    async fn production_maps_validated_inputs_to_store_config() {
        let pool = lazy_pool();
        let executor = governed_executor(&pool, "synthetic-delivery-token");
        let deps = DeliveryDeps::production(pool, executor, &delivery_config());
        assert_eq!(deps.config.broadcasters, ["111", "222"]);
        assert_eq!(deps.config.invite_url, "https://discord.gg/example");
        assert_eq!(deps.config.channel_id, "777");
    }

    #[tokio::test]
    async fn build_delivery_deps_wires_pool_ledger_and_governed_sender() {
        crate::gateway::ensure_crypto_provider();
        let deps = build_delivery_deps(
            lazy_pool(),
            "synthetic-delivery-token",
            None,
            &delivery_config(),
        )
        .expect("synthetic inputs build");
        assert_eq!(deps.config.broadcasters, ["111", "222"]);
        assert_eq!(deps.config.invite_url, "https://discord.gg/example");
        assert_eq!(deps.config.channel_id, "777");
    }

    #[tokio::test]
    async fn build_delivery_deps_refuses_credentials_the_admission_rejects() {
        assert!(matches!(
            build_delivery_deps(lazy_pool(), "has space", None, &delivery_config()),
            Err("receiver_prerequisites_invalid")
        ));
    }

    #[tokio::test]
    async fn build_delivery_deps_refuses_an_unusable_api_override() {
        crate::gateway::ensure_crypto_provider();
        assert!(matches!(
            build_delivery_deps(
                lazy_pool(),
                "synthetic-delivery-token",
                Some("http://exa mple.com".to_owned()),
                &delivery_config(),
            ),
            Err("receiver_prerequisites_invalid")
        ));
    }

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
