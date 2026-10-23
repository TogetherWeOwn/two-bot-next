//! Protected behavioral contract: private inspection is not sticky reconfiguration.
//! All runtime/store/HTTP paths are real except the loopback Discord peer.
//! The new surface is `/sticky-status [view:metadata|body]` (metadata by default).
//! Catch missing routing, false absence/health, unsafe rendering, and even a
//! rolled-back or value-preserving write; never teach inspection to repair state.
#![cfg(test)]

use super::{
    gates, message, option, slash, CommandRuntime, MockRest, CHANNEL, CHANNEL_S, GUILD, GUILD_S,
};
use futures_util::FutureExt;
use sqlx::PgPool;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::instrument::WithSubscriber;
use twilight_model::application::interaction::application_command::CommandOptionValue;
use twilight_model::application::interaction::Interaction;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;
use two_bot_core::funnel::now_millis_for_test;
use two_bot_core::sticky::{store, ActivityOutcome, PutSticky};
use two_bot_discord::ActionExecutor;
use two_bot_testsupport::TestDatabase;

const BODY: &str = "Saved announcement sentinel";
const LOCK_OWNER: &str = "private-claim-owner-sentinel";
const POST: &str = "4444";
const POSTED_MS: i64 = 1_700_000_000_000;
const SQL_SENTINEL: &str = "private-sql-error-member-sentinel";

async fn fixture(
    run: impl for<'a> FnOnce(&'a TestDatabase) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
) {
    // No silent local success: this protected suite requires the disposable DB.
    let url = std::env::var("TWO_TEST_DATABASE_URL")
        .expect("sticky status acceptance requires the authorized disposable test database");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("owned migrated disposable database");
    let result = AssertUnwindSafe(run(&db)).catch_unwind().await;
    db.close()
        .await
        .expect("teardown also runs on behavioral RED");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn seed(pool: &PgPool) {
    store::put_sticky(
        pool,
        &PutSticky {
            guild_id: GUILD_S,
            channel_id: CHANNEL_S,
            body: BODY,
            debounce_seconds: 5,
            enabled: true,
            actor_id: "88",
            now_ms: POSTED_MS - 1000,
        },
    )
    .await
    .expect("seed existing saved configuration");
}

async fn confirmed(pool: &PgPool) {
    seed(pool).await;
    sqlx::query("UPDATE sticky_messages SET last_message_id = $1, last_posted_at = to_timestamp($2::DOUBLE PRECISION / 1000.0)")
        .bind(POST).bind(POSTED_MS).execute(pool).await.expect("seed last confirmed send");
}

async fn claim(pool: &PgPool, age_ms: i64) {
    sqlx::query("UPDATE sticky_messages SET claim_token = $1, claimed_at = to_timestamp($2::DOUBLE PRECISION / 1000.0), delivery_uncertain = TRUE")
        .bind(LOCK_OWNER).bind(now_millis_for_test() - age_ms).execute(pool).await.expect("seed attempt and durable uncertainty");
}

// nextval survives transaction rollback: catches a hidden put, claim renewal,
// audit, or fence write even when the writer restores the original values.
async fn arm_write_tripwire(pool: &PgPool) {
    sqlx::raw_sql(
        "CREATE SEQUENCE inspect_write_attempts;
         CREATE FUNCTION inspect_write_tripwire() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN PERFORM nextval('inspect_write_attempts'); RETURN NULL; END $$;
         CREATE TRIGGER inspect_sticky_write BEFORE INSERT OR UPDATE OR DELETE ON sticky_messages
           FOR EACH STATEMENT EXECUTE FUNCTION inspect_write_tripwire();
         CREATE TRIGGER inspect_audit_write BEFORE INSERT OR UPDATE OR DELETE ON automation_audit_log
           FOR EACH STATEMENT EXECUTE FUNCTION inspect_write_tripwire();",
    )
    .execute(pool)
    .await
    .expect("install write-attempt witnesses in owned fixture only");
}

async fn snapshot(pool: &PgPool) -> serde_json::Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
           'sticky', (SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY guild_id, channel_id), '[]'::jsonb) FROM sticky_messages s),
           'audit', (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY id), '[]'::jsonb) FROM automation_audit_log a),
           'writes', (SELECT jsonb_build_array(last_value, is_called) FROM inspect_write_attempts))",
    )
    .fetch_one(pool)
    .await
    .expect("full rows, timestamps, claim, uncertainty, audit, and write-attempt witness")
}

async fn runtime(
    pool: PgPool,
    enabled: bool,
    statuses: Vec<u16>,
) -> (Arc<CommandRuntime>, MockRest) {
    let (peer, origin) = MockRest::start(statuses).await;
    let executor = ActionExecutor::with_proxy("synthetic-token".into(), Some(origin))
        .expect("loopback-only HTTP executor");
    let runtime = CommandRuntime::new(
        pool,
        executor,
        super::router_with_commands(gates(enabled, true)),
        GUILD,
        enabled,
    );
    runtime.set_identity(7777, 1111);
    (runtime, peer)
}

fn request(view: Option<&str>) -> Interaction {
    slash(
        "sticky-status",
        Some(CHANNEL),
        view.map(|v| vec![option("view", CommandOptionValue::String(v.into()))])
            .unwrap_or_default(),
    )
}

fn private_content(peer: &MockRest) -> String {
    let requests = peer.requests();
    assert!(
        !requests.is_empty(),
        "inspection must finish a private response"
    );
    let mut result = None;
    for (index, req) in requests.iter().enumerate() {
        let wire: serde_json::Value =
            serde_json::from_slice(&req.body).expect("JSON reply payload");
        let data = if index == 0 {
            assert_eq!(req.method, "POST");
            assert!(req.path.ends_with("/callback"));
            assert_eq!(
                wire["data"]["flags"].as_u64().expect("private flag") & 64,
                64
            );
            assert!(
                wire["type"] == 4 || wire["type"] == 5,
                "reply or private defer only"
            );
            &wire["data"]
        } else {
            assert_eq!(
                index, 1,
                "at most one completion, no extra Discord requests"
            );
            assert_eq!(req.method, "PATCH");
            assert_eq!(
                req.path,
                "/api/v10/webhooks/1111/sticky-test-token/messages/@original"
            );
            &wire
        };
        if let Some(content) = data["content"].as_str() {
            assert_eq!(data["allowed_mentions"]["parse"], serde_json::json!([]));
            for key in ["users", "roles"] {
                assert!(
                    data["allowed_mentions"][key].is_null()
                        || data["allowed_mentions"][key] == serde_json::json!([])
                );
            }
            assert!(
                content.encode_utf16().count() <= 2000,
                "Discord UTF-16 content bound"
            );
            assert!(data["embeds"].is_null() || data["embeds"] == serde_json::json!([]));
            assert!(data["components"].is_null() || data["components"] == serde_json::json!([]));
            assert!(
                !content.contains(LOCK_OWNER),
                "claim ownership is not display metadata"
            );
            assert!(
                !content.contains(SQL_SENTINEL),
                "SQL error detail must stay private to storage"
            );
            result = Some(content.to_owned());
        }
    }
    result.expect("nonempty completion, not a permanently deferred command")
}

async fn inspect(db: &TestDatabase, interaction: Interaction) -> String {
    let before = snapshot(db.pool()).await;
    let (runtime, peer) = runtime(db.pool().clone(), true, vec![]).await;
    let logs = Logs(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(logs.clone())
        .finish();
    tokio::time::timeout(
        Duration::from_secs(5),
        runtime
            .on_interaction(&interaction)
            .with_subscriber(subscriber),
    )
    .await
    .expect("bounded inspection, no wait for an active delivery lock");
    let diagnostic = String::from_utf8(logs.0.lock().expect("logs").clone()).expect("UTF-8 logs");
    for secret in [
        LOCK_OWNER,
        BODY,
        "tail-sentinel",
        "secret-body",
        "sticky-test-token",
    ] {
        assert!(
            !diagnostic.contains(secret),
            "inspection logs must not disclose saved text or capabilities"
        );
    }
    let after = snapshot(db.pool()).await;
    let content = private_content(&peer);
    peer.shutdown().await;
    assert_eq!(
        after, before,
        "inspection must preserve every field and attempt zero writes"
    );
    content
}

fn includes(text: &str, expected: &[&str]) {
    for expected in expected {
        assert!(
            text.to_lowercase().contains(&expected.to_lowercase()),
            "missing {expected:?} in private response: {text}"
        );
    }
}

#[tokio::test]
async fn control_explicit_reconfigure_still_resets_uncertainty_and_preserves_last_confirmation() {
    fixture(|db| {
        Box::pin(async move {
            confirmed(db.pool()).await;
            claim(db.pool(), 120_000).await;
            arm_write_tripwire(db.pool()).await;
            let before = snapshot(db.pool()).await;
            let (runtime, peer) = runtime(db.pool().clone(), true, vec![]).await;
            runtime
                .on_interaction(&slash(
                    "sticky",
                    Some(CHANNEL),
                    vec![
                        option(
                            "body",
                            CommandOptionValue::String("Explicitly reconfigured text".into()),
                        ),
                        option("debounce", CommandOptionValue::Integer(9)),
                    ],
                ))
                .await;
            includes(&private_content(&peer), &["Sticky set", "9s debounce"]);
            let after = snapshot(db.pool()).await;
            assert_eq!(after["sticky"][0]["body"], "Explicitly reconfigured text");
            assert_eq!(after["sticky"][0]["debounce_seconds"], 9);
            assert_eq!(after["sticky"][0]["claim_token"], serde_json::Value::Null);
            assert_eq!(after["sticky"][0]["claimed_at"], serde_json::Value::Null);
            assert_eq!(after["sticky"][0]["delivery_uncertain"], false);
            assert_eq!(after["sticky"][0]["last_message_id"], POST);
            assert_eq!(
                after["sticky"][0]["last_posted_at"],
                before["sticky"][0]["last_posted_at"]
            );
            assert_eq!(after["audit"].as_array().expect("audit array").len(), 1);
            assert_ne!(
                after["writes"], before["writes"],
                "the same witness sees real management writes"
            );
            peer.shutdown().await;
        })
    })
    .await;
}

#[tokio::test]
async fn control_explicit_remove_still_deletes_saved_configuration_and_the_recorded_post() {
    fixture(|db| {
        Box::pin(async move {
            confirmed(db.pool()).await;
            arm_write_tripwire(db.pool()).await;
            let (runtime, peer) = runtime(db.pool().clone(), true, vec![]).await;
            runtime
                .on_interaction(&slash("sticky-remove", Some(CHANNEL), vec![]))
                .await;
            let after = snapshot(db.pool()).await;
            assert_eq!(after["sticky"], serde_json::json!([]));
            assert_eq!(after["audit"].as_array().expect("audit array").len(), 1);
            let deletes: Vec<_> = peer
                .requests()
                .into_iter()
                .filter(|r| r.method == "DELETE")
                .collect();
            assert_eq!(deletes.len(), 1);
            assert_eq!(deletes[0].path, "/api/v10/channels/3333/messages/4444");
            // Unlike inspection, explicit remove is allowed its one channel mutation.
            let reply = peer.deferred_reply();
            assert_eq!(reply["content"], "Sticky removed.");
            peer.shutdown().await;
        })
    })
    .await;
}

#[tokio::test]
async fn absent_configuration_is_not_an_outage_or_a_disabled_sticky() {
    fixture(|db| {
        Box::pin(async move {
            arm_write_tripwire(db.pool()).await;
            let text = inspect(db, request(None)).await;
            includes(&text, &["no sticky configured", "this channel"]);
            assert!(!text.contains(BODY));
        })
    })
    .await;
}

#[tokio::test]
async fn idle_configuration_reports_saved_settings_and_never_sent_truth() {
    fixture(|db| {
        Box::pin(async move {
            seed(db.pool()).await;
            arm_write_tripwire(db.pool()).await;
            let text = inspect(db, request(None)).await;
            includes(
                &text,
                &[
                    "configured",
                    "enabled: yes",
                    "debounce: 5s",
                    "never",
                    "claim: none",
                    "delivery: idle",
                    "view:body",
                ],
            );
            assert!(!text.contains("https://discord.com/channels/"));
        })
    })
    .await;
}

#[tokio::test]
async fn disabled_saved_configuration_is_not_reported_absent() {
    fixture(|db| {
        Box::pin(async move {
            seed(db.pool()).await;
            sqlx::query("UPDATE sticky_messages SET enabled = FALSE, debounce_seconds = 300")
                .execute(db.pool())
                .await
                .expect("disabled saved sticky");
            arm_write_tripwire(db.pool()).await;
            includes(
                &inspect(db, request(None)).await,
                &[
                    "configured",
                    "enabled: no",
                    "debounce: 300s",
                    "delivery: disabled",
                ],
            );
        })
    })
    .await;
}

#[tokio::test]
async fn recent_confirmed_send_reports_waiting_without_resetting_debounce() {
    fixture(|db| Box::pin(async move {
        confirmed(db.pool()).await;
        sqlx::query("UPDATE sticky_messages SET debounce_seconds = 300, last_posted_at = clock_timestamp()")
            .execute(db.pool()).await.expect("inside debounce window");
        arm_write_tripwire(db.pool()).await;
        includes(&inspect(db, request(None)).await, &["debounce: 300s", "delivery: waiting", "claim: none", "last confirmed"]);
    })).await;
}

#[tokio::test]
async fn last_confirmed_post_has_a_localized_time_and_only_a_historical_link() {
    fixture(|db| {
        Box::pin(async move {
            confirmed(db.pool()).await;
            arm_write_tripwire(db.pool()).await;
            includes(
                &inspect(db, request(None)).await,
                &[
                    "last confirmed",
                    "<t:1700000000:F>",
                    "https://discord.com/channels/2222/3333/4444",
                    "may no longer exist",
                    "delivery: idle",
                ],
            );
        })
    })
    .await;
}

#[tokio::test]
async fn active_attempt_and_unresolved_delivery_are_separate_from_last_confirmation() {
    fixture(|db| {
        Box::pin(async move {
            confirmed(db.pool()).await;
            claim(db.pool(), 0).await;
            let claimed_seconds: i64 = sqlx::query_scalar(
                "SELECT floor(extract(epoch from claimed_at))::BIGINT FROM sticky_messages",
            )
            .fetch_one(db.pool())
            .await
            .expect("fixture claim start timestamp");
            arm_write_tripwire(db.pool()).await;
            let text = inspect(db, request(None)).await;
            assert!(
                text.contains(&format!("<t:{claimed_seconds}:F>")),
                "show attempt start without disclosing its ownership token"
            );
            includes(
                &text,
                &[
                    "claim: active",
                    "delivery: uncertain",
                    "last confirmed",
                    "<t:1700000000:F>",
                    "inspect the channel",
                    "reconfigure",
                    "not reset",
                ],
            );
        })
    })
    .await;
}

#[tokio::test]
async fn expired_attempt_does_not_turn_an_uncertain_accepted_send_into_healthy_idle() {
    fixture(|db| {
        Box::pin(async move {
            confirmed(db.pool()).await;
            claim(db.pool(), 120_000).await;
            arm_write_tripwire(db.pool()).await;
            let text = inspect(db, request(None)).await;
            includes(
                &text,
                &[
                    "claim: stale",
                    "delivery: uncertain",
                    "inspect the channel",
                    "reconfigure",
                ],
            );
            assert!(!text.to_lowercase().contains("delivery: idle"));
        })
    })
    .await;
}

#[tokio::test]
async fn fence_without_claim_and_future_dated_claim_are_not_lost_in_the_projection() {
    fixture(|db| {
        Box::pin(async move {
            seed(db.pool()).await;
            sqlx::query("UPDATE sticky_messages SET delivery_uncertain = TRUE")
                .execute(db.pool())
                .await
                .expect("uncertainty without lease");
            arm_write_tripwire(db.pool()).await;
            includes(
                &inspect(db, request(None)).await,
                &["claim: none", "delivery: uncertain"],
            );
            claim(db.pool(), -120_000).await;
            includes(
                &inspect(db, request(None)).await,
                &["claim: active", "delivery: uncertain"],
            );
        })
    })
    .await;
}

#[tokio::test]
async fn body_view_is_bounded_plain_text_and_never_a_public_preview() {
    fixture(|db| Box::pin(async move {
        seed(db.pool()).await;
        let hostile = format!("Saved body @everyone @here <@88> <@&99> **bold** [click](https://invalid.example) ``` ` | > {} tail-sentinel", "😀".repeat(900));
        assert!(hostile.encode_utf16().count() <= 2000, "valid maximum-sized input fixture");
        let body = format!("{hostile}{}", "x".repeat(2000 - hostile.encode_utf16().count()));
        sqlx::query("UPDATE sticky_messages SET body = $1").bind(&body).execute(db.pool()).await.expect("maximum saved body");
        arm_write_tripwire(db.pool()).await;
        let text = inspect(db, request(Some("body"))).await;
        includes(&text, &["saved body"]);
        // A bounded safe view can truncate; it must retain useful actual text.
        assert!(text.contains("Saved body"));
        for unsafe_text in ["@everyone", "@here", "<@88>", "<@&99>", "**bold**", "[click](https://invalid.example)", "```", "https://invalid.example"] {
            assert!(!text.contains(unsafe_text), "untrusted formatting/link must be neutralized: {unsafe_text}");
        }
    })).await;
}

#[tokio::test]
async fn body_view_preserves_useful_ordinary_saved_text_without_changing_it() {
    fixture(|db| {
        Box::pin(async move {
            seed(db.pool()).await;
            arm_write_tripwire(db.pool()).await;
            includes(
                &inspect(db, request(Some("body"))).await,
                &["saved body", BODY],
            );
        })
    })
    .await;
}

#[tokio::test]
async fn corrupt_stored_message_ids_never_create_a_link_or_echo_unsafe_text() {
    fixture(|db| {
        Box::pin(async move {
            confirmed(db.pool()).await;
            arm_write_tripwire(db.pool()).await;
            for id in [
                "0",
                "-1",
                "18446744073709551616",
                "4444/5555",
                "<@88>",
                "4444?token=synthetic",
            ] {
                sqlx::query("UPDATE sticky_messages SET last_message_id = $1")
                    .bind(id)
                    .execute(db.pool())
                    .await
                    .expect("legacy corrupt id fixture");
                let text = inspect(db, request(None)).await;
                includes(
                    &text,
                    &["last confirmed", "<t:1700000000:F>", "unavailable"],
                );
                assert!(
                    !text.contains("https://discord.com/channels/"),
                    "invalid id cannot become a link"
                );
                assert!(!text.contains("<@88>"));
                assert!(!text.contains("?token="));
            }
        })
    })
    .await;
}

#[tokio::test]
async fn current_channel_and_current_guild_are_the_only_lookup_scope() {
    fixture(|db| Box::pin(async move {
        confirmed(db.pool()).await;
        sqlx::query("INSERT INTO sticky_messages SELECT '9999', channel_id, 'foreign-guild-secret-body', debounce_seconds, enabled, last_message_id, last_posted_at, created_by, created_at, updated_by, updated_at, claim_token, claimed_at, delivery_uncertain FROM sticky_messages")
            .execute(db.pool()).await.expect("same channel key in a different guild");
        sqlx::query("INSERT INTO sticky_messages SELECT guild_id, '5555', 'other-channel-secret-body', debounce_seconds, enabled, last_message_id, last_posted_at, created_by, created_at, updated_by, updated_at, claim_token, claimed_at, delivery_uncertain FROM sticky_messages WHERE guild_id = '2222'")
            .execute(db.pool()).await.expect("same guild different channel");
        arm_write_tripwire(db.pool()).await;
        let body = inspect(db, request(Some("body"))).await;
        includes(&body, &[BODY]);
        assert!(!body.contains("secret-body"));
        let absent = slash("sticky-status", Some(6666), vec![]);
        includes(&inspect(db, absent).await, &["no sticky configured"]);
    })).await;
}

#[tokio::test]
async fn concurrent_transition_is_observed_as_a_snapshot_not_a_torn_delivery_record() {
    fixture(|db| Box::pin(async move {
        confirmed(db.pool()).await;
        arm_write_tripwire(db.pool()).await;
        let mut tx = db.pool().begin().await.expect("concurrent writer");
        sqlx::query("UPDATE sticky_messages SET last_message_id = '5555', last_posted_at = to_timestamp(1800000000), debounce_seconds = 300, claim_token = $1, claimed_at = clock_timestamp(), delivery_uncertain = TRUE")
            .bind(LOCK_OWNER).execute(&mut *tx).await.expect("uncommitted delivery transition");
        let old = inspect(db, request(None)).await;
        includes(&old, &["debounce: 5s", "claim: none", "<t:1700000000:F>", "/4444"]);
        assert!(!old.contains("/5555"));
        tx.commit().await.expect("publish coherent transition");
        let new = inspect(db, request(None)).await;
        includes(&new, &["debounce: 300s", "claim: active", "delivery: uncertain", "<t:1800000000:F>", "/5555"]);
        assert!(!new.contains("/4444"));
    })).await;
}

#[tokio::test]
async fn metadata_uses_one_coherent_store_read_not_separate_config_and_claim_queries() {
    fixture(|db| Box::pin(async move {
        confirmed(db.pool()).await;
        arm_write_tripwire(db.pool()).await;
        let before = snapshot(db.pool()).await;
        // Each evaluation returns a different valid state. Two independent
        // reads can tear configuration/confirmation/claim; one projection cannot.
        sqlx::raw_sql("ALTER TABLE sticky_messages RENAME TO saved_sticky_fixture;
          CREATE SEQUENCE inspect_read_steps;
          CREATE FUNCTION inspect_changing_read() RETURNS SETOF saved_sticky_fixture LANGUAGE plpgsql AS $$
          DECLARE row_state saved_sticky_fixture; step BIGINT;
          BEGIN
            SELECT * INTO row_state FROM saved_sticky_fixture LIMIT 1;
            step := nextval('inspect_read_steps');
            IF step % 2 = 0 THEN
              row_state.last_message_id := '5555';
              row_state.last_posted_at := to_timestamp(1800000000);
              row_state.debounce_seconds := 300;
              row_state.claim_token := 'private-claim-owner-sentinel';
              row_state.claimed_at := clock_timestamp();
              row_state.delivery_uncertain := TRUE;
            END IF;
            RETURN NEXT row_state;
          END $$;
          CREATE VIEW sticky_messages AS SELECT * FROM inspect_changing_read();")
            .execute(db.pool()).await.expect("fixture snapshot witness");
        let (runtime, peer) = runtime(db.pool().clone(), true, vec![]).await;
        runtime.on_interaction(&request(None)).await;
        let text = private_content(&peer);
        peer.shutdown().await;
        let reads: (i64, bool) = sqlx::query_as("SELECT last_value, is_called FROM inspect_read_steps")
            .fetch_one(db.pool()).await.expect("projection read count");
        sqlx::raw_sql("DROP VIEW sticky_messages; DROP FUNCTION inspect_changing_read(); ALTER TABLE saved_sticky_fixture RENAME TO sticky_messages;")
            .execute(db.pool()).await.expect("restore original relation");
        assert_eq!(snapshot(db.pool()).await, before);
        assert_eq!(reads, (1, true), "one bounded sticky read, not a mixed config/claim projection");
        includes(&text, &["debounce: 5s", "claim: none", "delivery: idle", "<t:1700000000:F>", "/4444"]);
        assert!(!text.contains("/5555"));
    })).await;
}

#[tokio::test]
async fn write_witness_detects_even_value_preserving_rolled_back_attempts() {
    fixture(|db| {
        Box::pin(async move {
            confirmed(db.pool()).await;
            arm_write_tripwire(db.pool()).await;
            let before = snapshot(db.pool()).await;
            let mut tx = db
                .pool()
                .begin()
                .await
                .expect("witness control transaction");
            sqlx::query("UPDATE sticky_messages SET body = body")
                .execute(&mut *tx)
                .await
                .expect("no-op update");
            tx.rollback().await.expect("roll back control write");
            let after = snapshot(db.pool()).await;
            assert_eq!(after["sticky"], before["sticky"]);
            assert_eq!(after["audit"], before["audit"]);
            assert_ne!(
                after["writes"], before["writes"],
                "attempt cannot hide behind equal rows or rollback"
            );
        })
    })
    .await;
}

#[tokio::test]
async fn unauthorized_or_unresolved_management_never_discloses_saved_body() {
    fixture(|db| {
        Box::pin(async move {
            confirmed(db.pool()).await;
            claim(db.pool(), 0).await;
            arm_write_tripwire(db.pool()).await;
            for permissions in [
                None,
                Some(Permissions::empty()),
                Some(Permissions::ADMINISTRATOR),
            ] {
                let mut input = request(Some("body"));
                input.member.as_mut().expect("member").permissions = permissions;
                let text = inspect(db, input).await;
                includes(&text, &["Manage Server"]);
                assert!(!text.contains(BODY));
            }
        })
    })
    .await;
}

#[tokio::test]
async fn wrong_or_missing_guild_gets_a_private_scope_refusal_not_false_absence() {
    fixture(|db| {
        Box::pin(async move {
            confirmed(db.pool()).await;
            claim(db.pool(), 0).await;
            arm_write_tripwire(db.pool()).await;
            for guild in [None, Some(Id::new(9999))] {
                let mut input = request(Some("body"));
                input.guild_id = guild;
                let text = inspect(db, input).await;
                includes(&text, &["configured server"]);
                assert!(!text.contains(BODY));
                assert!(!text.to_lowercase().contains("no sticky configured"));
            }
        })
    })
    .await;
}

#[tokio::test]
async fn invalid_options_are_private_refusals_not_a_body_read_or_a_write() {
    fixture(|db| {
        Box::pin(async move {
            confirmed(db.pool()).await;
            claim(db.pool(), 0).await;
            arm_write_tripwire(db.pool()).await;
            for options in [
                vec![option(
                    "view",
                    CommandOptionValue::String("invalid-view-sentinel".into()),
                )],
                vec![option("view", CommandOptionValue::Integer(1))],
                vec![
                    option("view", CommandOptionValue::String("body".into())),
                    option("view", CommandOptionValue::String("metadata".into())),
                ],
                vec![option(
                    "channel",
                    CommandOptionValue::Channel(Id::new(5555)),
                )],
            ] {
                let text = inspect(db, slash("sticky-status", Some(CHANNEL), options)).await;
                includes(&text, &["invalid"]);
                assert!(!text.contains("invalid-view-sentinel"));
                assert!(!text.contains(BODY));
            }
        })
    })
    .await;
}

#[tokio::test]
async fn missing_current_channel_is_distinct_from_not_configured() {
    fixture(|db| {
        Box::pin(async move {
            confirmed(db.pool()).await;
            arm_write_tripwire(db.pool()).await;
            let input = slash("sticky-status", None, vec![]);
            let text = inspect(db, input).await;
            includes(&text, &["channel"]);
            assert!(!text.contains(BODY));
            assert!(!text.to_lowercase().contains("no sticky configured"));
        })
    })
    .await;
}

#[derive(Clone)]
struct Logs(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("captured logs")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn store_outage_is_unknown_not_absent_or_healthy_and_never_leaks_sql_details() {
    fixture(|db| Box::pin(async move {
        confirmed(db.pool()).await;
        claim(db.pool(), 0).await;
        arm_write_tripwire(db.pool()).await;
        let before = snapshot(db.pool()).await;
        // Make the real SELECT raise sensitive synthetic detail, without a
        // network outage or changing any real service's database permissions.
        sqlx::raw_sql("ALTER TABLE sticky_messages RENAME TO saved_sticky_fixture;
          CREATE FUNCTION inspect_failed_read() RETURNS SETOF saved_sticky_fixture LANGUAGE plpgsql AS $$
          BEGIN RAISE EXCEPTION 'private-sql-error-member-sentinel @everyone'; END $$;
          CREATE VIEW sticky_messages AS SELECT * FROM inspect_failed_read();")
            .execute(db.pool()).await.expect("fixture-only failing read");
        let logs = Logs(Arc::new(Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt().without_time().with_ansi(false).with_max_level(tracing::Level::TRACE).with_writer(logs.clone()).finish();
        let (runtime, peer) = runtime(db.pool().clone(), true, vec![]).await;
        runtime.on_interaction(&request(None)).with_subscriber(subscriber).await;
        let text = private_content(&peer);
        peer.shutdown().await;
        sqlx::raw_sql("DROP VIEW sticky_messages; DROP FUNCTION inspect_failed_read(); ALTER TABLE saved_sticky_fixture RENAME TO sticky_messages;")
            .execute(db.pool()).await.expect("restore fixture before comparison");
        assert_eq!(snapshot(db.pool()).await, before);
        includes(&text, &["unavailable", "unknown"]);
        assert!(!text.to_lowercase().contains("no sticky configured"));
        assert!(!text.to_lowercase().contains("delivery: idle"));
        assert!(!text.contains(BODY));
        let diagnostic = String::from_utf8(logs.0.lock().expect("logs").clone()).expect("UTF-8 logs");
        for secret in [SQL_SENTINEL, LOCK_OWNER, BODY, "sticky-test-token"] {
            assert!(!diagnostic.contains(secret), "sensitive detail cannot enter runtime logs");
        }
    })).await;
}

#[tokio::test]
async fn failed_private_acknowledgement_does_not_reset_delivery() {
    fixture(|db| {
        Box::pin(async move {
            confirmed(db.pool()).await;
            claim(db.pool(), 0).await;
            arm_write_tripwire(db.pool()).await;
            let before = snapshot(db.pool()).await;
            let (runtime, peer) = runtime(db.pool().clone(), true, vec![403]).await;
            runtime.on_interaction(&request(None)).await;
            assert_eq!(snapshot(db.pool()).await, before);
            assert_eq!(
                peer.requests().len(),
                1,
                "failed ACK permits no edit, send, delete, or retry"
            );
            peer.shutdown().await;
        })
    })
    .await;
}

#[tokio::test]
async fn inspecting_uncertainty_does_not_reopen_the_send_admission_window() {
    fixture(|db| {
        Box::pin(async move {
            confirmed(db.pool()).await;
            claim(db.pool(), 120_000).await;
            arm_write_tripwire(db.pool()).await;
            let _ = inspect(db, request(None)).await;
            let (runtime, peer) = runtime(db.pool().clone(), true, vec![]).await;
            assert_eq!(
                runtime
                    .on_message(&message(901, CHANNEL, false, Some(GUILD)))
                    .await,
                ActivityOutcome::Held
            );
            assert!(
                peer.requests().is_empty(),
                "uncertain delivery remains fenced past claim expiry"
            );
            peer.shutdown().await;
        })
    })
    .await;
}

#[tokio::test]
async fn ordinary_delivery_still_requires_real_claim_after_an_inspection() {
    fixture(|db| {
        Box::pin(async move {
            seed(db.pool()).await;
            arm_write_tripwire(db.pool()).await;
            let _ = inspect(db, request(None)).await;
            let (runtime, peer) = runtime(db.pool().clone(), true, vec![]).await;
            assert_eq!(
                runtime
                    .on_message(&message(902, CHANNEL, false, Some(GUILD)))
                    .await,
                ActivityOutcome::Reposted
            );
            assert_eq!(
                peer.requests()
                    .iter()
                    .filter(|r| r.method == "POST" && r.path == "/api/v10/channels/3333/messages")
                    .count(),
                1
            );
            let stored = store::get_sticky(db.pool(), GUILD_S, CHANNEL_S)
                .await
                .expect("saved row")
                .expect("configured");
            assert_eq!(stored.body, BODY);
            assert!(stored.last_posted_at_ms.is_some());
            peer.shutdown().await;
        })
    })
    .await;
}

#[tokio::test]
async fn disabled_automation_gate_still_refuses_status_without_disclosing_body() {
    fixture(|db| {
        Box::pin(async move {
            seed(db.pool()).await;
            arm_write_tripwire(db.pool()).await;
            let before = snapshot(db.pool()).await;
            let (runtime, peer) = runtime(db.pool().clone(), false, vec![]).await;
            runtime.on_interaction(&request(Some("body"))).await;
            let text = private_content(&peer);
            peer.shutdown().await;
            includes(&text, &["disabled"]);
            assert!(!text.contains(BODY));
            assert_eq!(snapshot(db.pool()).await, before);
        })
    })
    .await;
}
