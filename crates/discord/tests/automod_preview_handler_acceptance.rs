#![cfg(feature = "db")]

//! Opt-in database proof for the `/automod-preview` Discord wiring
//! (docs/automod-preview-spec.md §6, T17 wire half; slice 3h). Restricted to
//! agent-testdb or the CI service container.
//! Run: cargo test -p two-bot-discord --features db --test automod_preview_handler_acceptance -- --ignored
//! No DATABASE_URL or inherited application credentials are consulted.
//!
//! Proves, through the real pool entry with an isolated migrated database:
//! a matched attempt commits exactly one content-free audit row and renders
//! the first reply from the winning receipt instant; redelivery of the same
//! interaction replays byte-identical with no second row and no metric; and
//! a saturated limiter commits a `rate_limited` row while evaluating
//! nothing.

use std::future::{ready, Future};

use serde_json::json;
use two_bot_core::automod::{AutomodConfig, AutomodPolicy};
use two_bot_core::automod_preview_audit::load_preview_audit;
use two_bot_core::automod_preview_limiter::{PreviewRateDecision, PreviewRateLimiter};
use two_bot_core::automod_preview_report::PREVIEW_RATE_LIMITED_REPLY;
use two_bot_core::automod_runtime::{
    AutomodClaimLedger, AutomodMatch, AutomodRuntime, AutomodScope, DeliveryKey, LedgerClaim,
    MessageSubject, StoredOutcome, TargetFacts, ViolationRecord, STAGING_GUILD_ID,
};
use two_bot_discord::automod_activation::{AutomodActivation, AutomodFacts, FetchedMessage};
use two_bot_discord::automod_preview_handler::handle_automod_preview;
use two_bot_discord::ActionExecutor;
use two_bot_testsupport::TestDatabase;

struct StubLedger;

impl AutomodClaimLedger for StubLedger {
    type Claim = ();
    type Error = String;

    fn ledger_claim(
        &self,
        _: &DeliveryKey,
    ) -> impl Future<Output = Result<LedgerClaim<()>, String>> + Send {
        ready(Err("preview path never claims".to_owned()))
    }

    fn ledger_preserve(
        &self,
        _: &(),
        _: &AutomodMatch,
    ) -> impl Future<Output = Result<bool, String>> + Send {
        ready(Ok(false))
    }

    fn ledger_mark_started(&self, _: &()) -> impl Future<Output = Result<bool, String>> + Send {
        ready(Ok(false))
    }

    fn ledger_complete(
        &self,
        _: &(),
        _: &StoredOutcome,
    ) -> impl Future<Output = Result<bool, String>> + Send {
        ready(Ok(false))
    }

    fn ledger_release(&self, _: &()) -> impl Future<Output = Result<bool, String>> + Send {
        ready(Ok(false))
    }

    fn ledger_record(
        &self,
        _: &(),
        _: &MessageSubject,
        _: two_bot_core::automod::AutomodFilter,
        _: &str,
    ) -> impl Future<Output = Result<ViolationRecord, String>> + Send {
        ready(Ok(ViolationRecord {
            count: 0,
            inserted: false,
        }))
    }
}

struct StubFacts;

impl AutomodFacts for StubFacts {
    fn fetch_message(
        &self,
        _: &str,
        _: &str,
        _: &str,
    ) -> impl Future<Output = Option<FetchedMessage>> + Send {
        ready(None)
    }

    fn target_facts(&self, _: &MessageSubject) -> impl Future<Output = Option<TargetFacts>> + Send {
        ready(None)
    }
}

fn staging_activation(bad_words: &[&str]) -> AutomodActivation<StubLedger, StubFacts> {
    let config = AutomodConfig {
        enabled: true,
        dry_run: true,
        policy: AutomodPolicy {
            bad_words: bad_words.iter().map(ToString::to_string).collect(),
            ..AutomodPolicy::default()
        },
    };
    let scope = AutomodScope {
        guild_id: STAGING_GUILD_ID.to_owned(),
        live_approved: false,
    };
    AutomodActivation::new(
        AutomodRuntime::new(config, scope),
        StubLedger,
        StubFacts,
        ActionExecutor::with_proxy(
            "synthetic-test-token".to_owned(),
            Some("http://127.0.0.1:1".to_owned()),
        )
        .expect("synthetic executor"),
    )
}

fn preview_interaction(
    id: &str,
    text: &str,
) -> twilight_model::application::interaction::Interaction {
    serde_json::from_value(json!({
        "id": id, "application_id": "1111", "type": 2,
        "guild_id": STAGING_GUILD_ID,
        "channel": {"id": "777", "type": 0},
        "data": {"id": "1", "type": 1, "name": "automod-preview",
            "options": [{"name": "text", "type": 3, "value": text}]},
        "member": {"user": {"id": "4242", "username": "previewer", "discriminator": "0"},
            "roles": ["9"], "flags": 0, "deaf": false, "mute": false},
        "token": "synthetic-test-token", "version": 1,
        "authorizing_integration_owners": {"0": STAGING_GUILD_ID},
        "entitlements": []
    }))
    .expect("synthetic preview interaction")
}

async fn row_count(db: &TestDatabase, id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM automation_audit_log WHERE id = $1")
        .bind(id)
        .fetch_one(db.pool())
        .await
        .expect("audit row count reads")
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn preview_commits_content_free_row_and_replays_byte_identical() {
    let Some(url) = std::env::var_os("TWO_TEST_DATABASE_URL") else {
        eprintln!("DB acceptance not exercised: TWO_TEST_DATABASE_URL unset");
        return;
    };
    let db = TestDatabase::create(
        url.to_str().expect("test database URL is UTF-8"),
        &sqlx::migrate!("../cutover/migrations"),
    )
    .await
    .expect("isolated migrated test database");
    let activation = staging_activation(&["bannedword"]);
    let mut limiter = PreviewRateLimiter::new();
    let now_ms = 1_700_000_000_000u64;
    let interaction = preview_interaction("4242", "has bannedword here");

    let first = handle_automod_preview(
        Some(&activation),
        db.pool(),
        &mut limiter,
        &interaction,
        now_ms,
    )
    .await;
    assert!(first.evaluated, "first delivery evaluates exactly once");
    assert!(
        first.reply.contains("TWO_AUTOMOD_BAD_WORDS"),
        "reply names the setting, not the content"
    );
    assert!(
        !first.reply.contains("bannedword"),
        "reply never echoes content"
    );
    let metric = first.metric.expect("evaluated attempt emits a metric");
    assert_eq!(metric.result, "matched");
    assert_eq!(metric.filter, "bad_words");
    assert_eq!(metric.mode, "dry_run");

    let row = load_preview_audit(db.pool(), "automod-preview:4242")
        .await
        .expect("audit read succeeds")
        .expect("winner commits exactly one content-free row");
    assert_eq!(row.outcome, "matched");
    assert!(
        row.reason
            .as_deref()
            .expect("reason persists")
            .starts_with("filter=bad_words;"),
        "row carries enums only, no content"
    );
    assert!(
        !row.reason
            .as_deref()
            .expect("reason persists")
            .contains("bannedword"),
        "row never persists content"
    );
    assert!(
        first.reply.contains(&row.created_at),
        "first reply renders the stored timestamp at database precision"
    );
    assert_eq!(row_count(&db, "automod-preview:4242").await, 1);

    // Redelivery of the same interaction replays the committed row
    // byte-identical: no evaluation, no metric, no second row.
    let second = handle_automod_preview(
        Some(&activation),
        db.pool(),
        &mut limiter,
        &interaction,
        now_ms + 5_000,
    )
    .await;
    assert_eq!(second.reply, first.reply);
    assert!(!second.evaluated, "replay evaluates nothing");
    assert_eq!(second.metric, None, "replay carries no metric");
    assert_eq!(row_count(&db, "automod-preview:4242").await, 1);

    db.close().await.expect("isolated database drops");
}

#[tokio::test]
#[ignore = "requires agent-testdb or the CI Postgres service"]
async fn preview_rate_refusal_commits_rate_limited_row_without_evaluating() {
    let Some(url) = std::env::var_os("TWO_TEST_DATABASE_URL") else {
        eprintln!("DB acceptance not exercised: TWO_TEST_DATABASE_URL unset");
        return;
    };
    let db = TestDatabase::create(
        url.to_str().expect("test database URL is UTF-8"),
        &sqlx::migrate!("../cutover/migrations"),
    )
    .await
    .expect("isolated migrated test database");
    let activation = staging_activation(&["bannedword"]);
    let mut limiter = PreviewRateLimiter::new();
    let base_ms = 1_700_000_000_000u64;
    for index in 1..=30u64 {
        assert_eq!(
            limiter.check_and_admit(
                "4242",
                STAGING_GUILD_ID,
                9_000 + index,
                base_ms + index * 6_000
            ),
            PreviewRateDecision::Admit,
            "pre-charge admits one attempt per interaction"
        );
    }

    let interaction = preview_interaction("7777", "hello world");
    let answer = handle_automod_preview(
        Some(&activation),
        db.pool(),
        &mut limiter,
        &interaction,
        base_ms + 31 * 6_000,
    )
    .await;
    assert_eq!(answer.reply, PREVIEW_RATE_LIMITED_REPLY);
    assert!(!answer.evaluated, "rate refusal evaluates nothing");
    let metric = answer.metric.expect("refusal emits a metric");
    assert_eq!(metric.result, "rate_limited");
    assert_eq!(metric.mode, "dry_run");

    let row = load_preview_audit(db.pool(), "automod-preview:7777")
        .await
        .expect("audit read succeeds")
        .expect("refusal commits exactly one content-free row");
    assert_eq!(row.outcome, "rate_limited");
    assert!(
        row.reason
            .as_deref()
            .expect("reason persists")
            .contains("mode=dry_run"),
        "rate row records the effective mode, no filter"
    );
    assert_eq!(row_count(&db, "automod-preview:7777").await, 1);

    db.close().await.expect("isolated database drops");
}
