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
/// inputs: the bot pool, the Discord bot token, the optional API override,
/// the validated delivery config and the boot identity's announcements
/// permission. Pure construction (no I/O, no sends); `Err` carries the
/// catalogued `error_class` boot refuses startup with. A refused identity
/// fails here — before any key derivation — so an enabled delivery on a
/// live or unknown identity refuses startup instead of posting go-lives
/// under an identity where Announcements is not cleared.
pub fn build_delivery_deps(
    pool: sqlx::PgPool,
    token: &str,
    proxy: Option<String>,
    config: &two_bot_core::eventsub_delivery_config::EventsubDeliveryConfig,
    announcements_permitted: bool,
) -> Result<DeliveryDeps, &'static str> {
    if !announcements_permitted {
        return Err("receiver_prerequisites_invalid");
    }
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
            let attempted = matches!(
                *reason,
                "send_failed" | "send_failed_unreleased" | "ambiguous_send" | "stamp_failed"
            );
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

    #[test]
    fn boot_delivery_fence_delegates_to_tested_helper() {
        // The capability choice lives in the tested
        // `BootActivation::go_live_delivery_permitted` helper (pinned by
        // `go_live_delivery_is_staging_only` in activation.rs), and boot must
        // stay a one-line delegation: a swap to a live-cleared capability (or
        // a hardcoded `true`) at the call site would keep every other
        // delivery test green while restoring the live-identity bypass.
        // Mirrors the `rank_heal_permitted` source scan.
        let source = include_str!("main.rs");
        assert!(
            source.contains("go_live_delivery_permitted"),
            "main.rs must build delivery behind go_live_delivery_permitted"
        );
        let fence = source
            .split_once("let delivery = eventsub_delivery_config.map")
            .expect("delivery fence site moved")
            .1
            .split_once("bind_with_delivery")
            .expect("delivery bind site moved")
            .0;
        // Strip `//` comments before scanning: the fence-region comment names
        // the helper, so a bare `contains("go_live_delivery_permitted")`
        // passes on comments alone (and an inline `//` could smuggle the
        // exact statement past the pin while the code hardcodes `true`).
        let fence_code: String = fence
            .lines()
            .map(|line| line.split_once("//").map_or(line, |(code, _)| code))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            fence_code
                .contains("let announcements_permitted = activation.go_live_delivery_permitted();"),
            "the delivery fence must be exactly `let announcements_permitted = \
             activation.go_live_delivery_permitted();` so `= true` or `|| true` fails"
        );
        assert!(
            !fence_code.contains("LiveCapability::"),
            "the delivery fence must not name a LiveCapability directly; keep the choice in go_live_delivery_permitted"
        );
        assert!(
            !fence_code.contains("activation.permitted"),
            "the delivery fence must not call the generic fence directly; delegate to go_live_delivery_permitted"
        );
        let activation = include_str!("activation.rs");
        assert!(
            activation.contains("LiveCapability::Announcements"),
            "go-live delivery must sit behind the Announcements capability"
        );
    }

    #[tokio::test]
    async fn build_delivery_deps_wires_pool_ledger_and_governed_sender() {
        crate::gateway::ensure_crypto_provider();
        // The staging identity clears delivery through the same tested helper
        // boot calls, so this exercises the success path with a real fence
        // decision rather than a hardcoded `true`. The call-site delegation
        // itself is pinned by `boot_delivery_fence_delegates_to_tested_helper`.
        let permitted = crate::activation::fixtures::staging().go_live_delivery_permitted();
        assert!(permitted);
        let deps = build_delivery_deps(
            lazy_pool(),
            "synthetic-delivery-token",
            None,
            &delivery_config(),
            permitted,
        )
        .expect("synthetic inputs build");
        assert_eq!(deps.config.broadcasters, ["111", "222"]);
        assert_eq!(deps.config.invite_url, "https://discord.gg/example");
        assert_eq!(deps.config.channel_id, "777");
    }

    #[tokio::test]
    async fn build_delivery_deps_refuses_an_identity_without_announcements() {
        crate::gateway::ensure_crypto_provider();
        // Every refused identity (the live pair, cleared for self_roles only,
        // plus unknown/mismatched/missing identities) must refuse startup
        // with the catalogued class before any key derivation, so an enabled
        // delivery can never post go-lives under an uncleared identity.
        for (label, activation) in crate::activation::fixtures::refused() {
            let permitted = activation.go_live_delivery_permitted();
            assert!(!permitted, "{label} must not permit go-live delivery");
            assert!(
                matches!(
                    build_delivery_deps(
                        lazy_pool(),
                        "synthetic-delivery-token",
                        None,
                        &delivery_config(),
                        permitted,
                    ),
                    Err("receiver_prerequisites_invalid")
                ),
                "{label} must refuse delivery deps"
            );
        }
    }

    #[tokio::test]
    async fn build_delivery_deps_refuses_credentials_the_admission_rejects() {
        assert!(matches!(
            build_delivery_deps(lazy_pool(), "has space", None, &delivery_config(), true),
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
                true,
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
        for reason in [
            "send_failed",
            "send_failed_unreleased",
            "ambiguous_send",
            "stamp_failed",
        ] {
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
