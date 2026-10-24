//! Protected failing-first acceptance. Real command dispatch, migrated disposable
//! Postgres, and loopback Discord; no implementation adapter or in-memory store.
//! A revision returned by inspect binds an edit to that saved definition and its
//! delivery fences. Only body/updated_by/updated_at may change on a saved edit.

#![cfg(test)]

use super::*;
use crate::discord_test_common::{MockRest as TickerRest, ScriptedResponse};
use serde_json::{json, Value};
use two_bot_core::scheduled::format_iso_ms;
use two_bot_testsupport::TestDatabase;

const ID: &str = "aabbccdd001122334455667788990001";
const OTHER: &str = "aabbccdd001122334455667788990002";
const OLD: &str = "Original announcement: Tuesady at seven.";
const NEW: &str = "Corrected announcement: Tuesday at seven.";
const DUE: &str = "2030-01-01T00:00:00.000Z";
const TICK: u64 = 1_893_456_000_000;
const POST_PATH: &str = "/api/v10/channels/3333/messages";

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit disposable test bootstrap");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded migrated database; never application credentials")
}

async fn seed(pool: &PgPool, guild: &str, id: &str, interval: Option<i64>) {
    sqlx::query(
        "INSERT INTO scheduled_messages
          (id,guild_id,channel_id,body,next_run_at,interval_seconds,enabled,
           created_by,created_at,updated_by,updated_at,last_run_at,last_message_id)
         VALUES ($1,$2,'3333',$3,'2030-01-01T00:00:00.000Z',$4,TRUE,
           '77','2026-10-01T00:00:00.000Z','77','2026-10-01T00:00:00.000Z',
           '2026-09-30T00:00:00.000Z','9000')",
    )
    .bind(id)
    .bind(guild)
    .bind(OLD)
    .bind(interval)
    .execute(pool)
    .await
    .expect("seed saved definition and historical delivery evidence");
}

async fn row(pool: &PgPool, id: &str) -> Value {
    sqlx::query_scalar("SELECT to_jsonb(s) FROM scheduled_messages s WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("snapshot every schedule field")
}

async fn rows(pool: &PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(to_jsonb(s) ORDER BY id),'[]'::jsonb) FROM scheduled_messages s",
    )
    .fetch_one(pool)
    .await
    .expect("snapshot all guilds, including fences and metadata")
}

async fn audits(pool: &PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY id),'[]'::jsonb) FROM automation_audit_log a",
    ).fetch_one(pool).await.expect("snapshot all audit fields")
}

async fn mutation_audits(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT to_jsonb(a) FROM automation_audit_log a
         WHERE action='scheduled.edit' AND outcome='ok' ORDER BY created_at,id",
    )
    .fetch_all(pool)
    .await
    .expect("read committed body-edit mutation audits")
}

fn text_option(name: &str, text: &str) -> CommandDataOption {
    option(name, CommandOptionValue::String(text.to_owned()))
}

fn inspect(id: &str, view: &str) -> Interaction {
    slash(
        "schedule-inspect",
        Some(CHANNEL),
        vec![text_option("id", id), text_option("view", view)],
    )
}

fn edit(id: &str, revision: &str, body: &str) -> Interaction {
    slash(
        "schedule-edit",
        Some(CHANNEL),
        vec![
            text_option("id", id),
            text_option("revision", revision),
            text_option("body", body),
        ],
    )
}

async fn invoke_script(pool: PgPool, request: Interaction, script: Vec<u16>) -> Vec<RestRequest> {
    let (mock, origin) = MockRest::start(script).await;
    let runtime = CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(gates(true, true)),
        GUILD,
        true,
    );
    let guard = runtime.dispatch_guard();
    assert!(
        runtime.dispatch(&interaction_event(request)),
        "gateway admits the management invocation"
    );
    wait_for(
        || (0..6).all(|lane| runtime.lane_in_flight(lane) == 0),
        "schedule management settles",
    )
    .await;
    let requests = mock.requests();
    drop(guard);
    drop(runtime);
    mock.shutdown().await;
    requests
}

async fn invoke(pool: &PgPool, request: Interaction) -> Vec<RestRequest> {
    invoke_script(pool.clone(), request, Vec::new()).await
}

// The wire result, rather than a mocked handler return, must remain private,
// bounded and mention-safe. No channel/registry/message deletion is permitted.
fn private_text(requests: &[RestRequest]) -> String {
    assert!(!requests.is_empty(), "management invocation must answer");
    let mut texts = Vec::new();
    let mut callbacks = 0;
    for (index, request) in requests.iter().enumerate() {
        let payload: Value = serde_json::from_slice(&request.body).expect("Discord JSON");
        let data = if request.path.ends_with("/callback") {
            callbacks += 1;
            assert_eq!(index, 0, "acknowledge before I/O effects");
            assert_eq!(request.method, "POST");
            assert_eq!(
                payload["data"]["flags"], 64,
                "initial reply/defer is private"
            );
            assert!(payload["type"] == 4 || payload["type"] == 5);
            if payload["type"] == 5 {
                continue;
            }
            payload["data"].clone()
        } else {
            assert!(
                request.path.starts_with("/api/v10/webhooks/1111/"),
                "no channel writes: {}",
                request.path
            );
            if request.method == "POST" {
                assert_eq!(payload["flags"], 64, "followup must also be private");
            } else {
                assert_eq!(request.method, "PATCH");
                assert!(request.path.ends_with("/messages/@original"));
            }
            payload
        };
        assert_eq!(data["allowed_mentions"]["parse"], json!([]));
        let content = data["content"]
            .as_str()
            .expect("inspect/edit has human-readable content");
        assert!(
            content.encode_utf16().count() <= 2000,
            "every Discord message fits"
        );
        assert!(!content.contains("@everyone") && !content.contains("@here"));
        texts.push(content.to_owned());
    }
    assert_eq!(callbacks, 1, "one initial acknowledgement");
    assert!(!texts.is_empty(), "deferred invocation must complete");
    assert!(
        texts.len() <= 2,
        "bounded metadata/body views, not an unbounded dump"
    );
    texts.join("\n")
}

fn revision(metadata: &str) -> String {
    let token = metadata
        .lines()
        .find_map(|line| line.trim().strip_prefix("Revision:"))
        .expect("inspect metadata must give an actionable Revision: token")
        .trim()
        .trim_matches('`')
        .to_owned();
    assert!(!token.is_empty() && token.len() <= 128);
    assert!(token
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    token
}

async fn inspect_revision(pool: &PgPool, id: &str) -> String {
    revision(&private_text(&invoke(pool, inspect(id, "metadata")).await))
}

fn saved(text: &str) {
    assert!(
        text.to_ascii_lowercase().contains("saved"),
        "acknowledge persisted state: {text}"
    );
    for promise in [
        "will be delivered",
        "will be posted",
        "delivery guaranteed",
        "sent successfully",
    ] {
        assert!(
            !text.to_ascii_lowercase().contains(promise),
            "a save is not a delivery receipt"
        );
    }
}

fn retry_instruction(text: &str) {
    assert!(
        text.contains("/schedule-inspect"),
        "give an inspect/recovery/retry instruction: {text}"
    );
    assert!(
        !text.to_ascii_lowercase().contains("saved"),
        "refusal is not a save: {text}"
    );
}

fn unchanged_except_body_and_updater(before: &Value, after: &Value, expected_body: &str) {
    assert_eq!(after["body"], expected_body);
    assert_eq!(
        after["updated_by"], "88",
        "record invoking member, not interaction.user fallback"
    );
    assert_ne!(after["updated_at"], before["updated_at"]);
    let updated = after["updated_at"].as_str().expect("saved edit timestamp");
    time::OffsetDateTime::parse(updated, &time::format_description::well_known::Rfc3339)
        .expect("updater time is a real instant");
    let (mut left, mut right) = (before.clone(), after.clone());
    for field in ["body", "updated_by", "updated_at"] {
        left.as_object_mut().expect("row").remove(field);
        right.as_object_mut().expect("row").remove(field);
    }
    assert_eq!(left, right, "no upsert/reset: preserve identity, timing, recurrence, creator, last run and every delivery fence");
}

fn assert_content_free_audits(value: &Value) {
    let encoded = value.to_string();
    for body in [
        OLD,
        NEW,
        "private-replacement-sentinel",
        "audit-error-sentinel",
        "@everyone",
        "discord.gg/",
    ] {
        assert!(
            !encoded.contains(body),
            "audit must never retain message bodies or raw DB error text"
        );
    }
}

// Break caught: missing private inspection, dropped metadata, or truncation of a
// maximum-length saved body to make room for a header.
#[tokio::test]
async fn schedule_body_edit_inspect_exposes_full_definition_in_separate_private_views() {
    let db = database().await;
    seed(db.pool(), GUILD_S, ID, Some(3600)).await;
    let body = format!("{}TAIL", "界".repeat(1996));
    sqlx::query("UPDATE scheduled_messages SET body=$1 WHERE id=$2")
        .bind(&body)
        .bind(ID)
        .execute(db.pool())
        .await
        .expect("maximum stored body");
    let before = rows(db.pool()).await;
    let audit_before = audits(db.pool()).await;
    let metadata = private_text(&invoke(db.pool(), inspect(ID, "metadata")).await);
    for fact in [ID, CHANNEL_S, DUE, "3600", "9000", "2026-09-30"] {
        assert!(
            metadata.contains(fact),
            "inspect must reveal saved fact {fact}: {metadata}"
        );
    }
    assert!(
        metadata.to_ascii_lowercase().contains("idle"),
        "known current state, not an optimistic delivery claim"
    );
    revision(&metadata);
    let default_view = slash(
        "schedule-inspect",
        Some(CHANNEL),
        vec![text_option("id", ID)],
    );
    assert_eq!(
        private_text(&invoke(db.pool(), default_view).await),
        metadata,
        "omitted view selects metadata"
    );
    let rendered = private_text(&invoke(db.pool(), inspect(ID, "body")).await);
    assert_eq!(
        rendered, body,
        "body view preserves all 2000 UTF-16 units, with no header budget loss"
    );
    assert_eq!(rows(db.pool()).await, before);
    assert_eq!(
        audits(db.pool()).await,
        audit_before,
        "inspect is read-only"
    );
    db.close().await.expect("drop own fixture");
}

// Break caught: using put_scheduled or allocating a fresh schedule silently
// resets timing, creator/last-run facts or delivery fencing metadata.
#[tokio::test]
async fn schedule_body_edit_saves_only_body_updater_and_atomic_content_free_audit() {
    let db = database().await;
    for (id, interval) in [(ID, None), (OTHER, Some(3600))] {
        seed(db.pool(), GUILD_S, id, interval).await;
        let before = row(db.pool(), id).await;
        let token = inspect_revision(db.pool(), id).await;
        let response = private_text(&invoke(db.pool(), edit(id, &token, NEW)).await);
        saved(&response);
        unchanged_except_body_and_updater(&before, &row(db.pool(), id).await, NEW);
        let edits = mutation_audits(db.pool()).await;
        let matching: Vec<_> = edits.iter().filter(|a| a["target_key"] == id).collect();
        assert_eq!(
            matching.len(),
            1,
            "one successful mutation audit per changed row"
        );
        assert_eq!(matching[0]["guild_id"], GUILD_S);
        assert_eq!(matching[0]["actor_id"], "88");
        let audit_at = matching[0]["created_at"]
            .as_str()
            .expect("audit created_at instant");
        let saved = row(db.pool(), id).await;
        let row_at = saved["updated_at"].as_str().expect("saved edit timestamp");
        assert_eq!(
            time::OffsetDateTime::parse(audit_at, &time::format_description::well_known::Rfc3339)
                .expect("audit instant parses"),
            time::OffsetDateTime::parse(row_at, &time::format_description::well_known::Rfc3339)
                .expect("row instant parses"),
            "audit and row record the same edit instant"
        );
        assert_content_free_audits(&audits(db.pool()).await);
    }
    assert_eq!(
        rows(db.pool()).await.as_array().expect("rows").len(),
        2,
        "never create replacement ids"
    );
    let peer = db.independent_pool().await.expect("restart connection");
    assert_eq!(row(&peer, ID).await["body"], NEW);
    assert_eq!(row(&peer, OTHER).await["body"], NEW);
    peer.close().await;
    db.close().await.expect("drop own fixture");
}

#[tokio::test]
async fn schedule_body_edit_same_body_is_explicit_idempotent_noop() {
    let db = database().await;
    seed(db.pool(), GUILD_S, ID, None).await;
    let token = inspect_revision(db.pool(), ID).await;
    let before = rows(db.pool()).await;
    for _ in 0..2 {
        let reply = private_text(&invoke(db.pool(), edit(ID, &token, OLD)).await);
        let lower = reply.to_ascii_lowercase();
        assert!(
            lower.contains("unchanged") || lower.contains("no change"),
            "explicit no-op: {reply}"
        );
        assert_eq!(
            rows(db.pool()).await,
            before,
            "no timestamp, revision or fence churn"
        );
        assert!(mutation_audits(db.pool()).await.is_empty());
    }
    db.close().await.expect("drop own fixture");
}

#[tokio::test]
async fn schedule_body_edit_reuses_body_safety_and_utf16_boundaries_without_truncation() {
    let db = database().await;
    seed(db.pool(), GUILD_S, ID, None).await;
    for invalid in [
        "".to_owned(),
        " \n\t\u{200b}".to_owned(),
        "x".repeat(2001),
        "😀".repeat(1001),
        format!("{}@everyone", "x".repeat(1991)),
    ] {
        let token = inspect_revision(db.pool(), ID).await;
        let before = rows(db.pool()).await;
        let reply = private_text(&invoke(db.pool(), edit(ID, &token, &invalid)).await);
        assert!(
            reply.contains("2000") || reply.to_ascii_lowercase().contains("body"),
            "bounded body refusal: {reply}"
        );
        assert!(!reply.to_ascii_lowercase().contains("saved"));
        assert_eq!(rows(db.pool()).await, before);
        assert!(mutation_audits(db.pool()).await.is_empty());
    }
    for (raw, stored) in [
        (
            "@everyone @here discord.gg/abc".to_owned(),
            "@\u{200b}everyone @\u{200b}here discord.\u{200b}gg/abc".to_owned(),
        ),
        ("😀".repeat(1000), "😀".repeat(1000)),
        ("x".repeat(2000), "x".repeat(2000)),
    ] {
        let token = inspect_revision(db.pool(), ID).await;
        saved(&private_text(
            &invoke(db.pool(), edit(ID, &token, &raw)).await,
        ));
        assert_eq!(
            row(db.pool(), ID).await["body"],
            stored,
            "same safety policy as schedule creation, not silent truncation"
        );
    }
    assert_content_free_audits(&audits(db.pool()).await);
    db.close().await.expect("drop own fixture");
}

#[tokio::test]
async fn schedule_body_edit_refuses_active_expired_uncertain_and_legacy_unknown_fences() {
    let db = database().await;
    seed(db.pool(), GUILD_S, ID, Some(3600)).await;
    for (claim, claimed_at, nonce, uncertain, state_word) in [
        (
            Some("live-claim"),
            Some("2030-01-01T00:00:00.000Z"),
            Some("live-nonce"),
            Some(false),
            "claim",
        ),
        (
            Some("old-claim"),
            Some("2020-01-01T00:00:00.000Z"),
            Some("old-nonce"),
            Some(false),
            "claim",
        ),
        (None, None, Some("retry-nonce"), Some(false), "nonce"),
        (None, None, Some("uncertain-nonce"), Some(true), "uncertain"),
        (None, None, None, None, "uncertain"),
    ] {
        sqlx::query("UPDATE scheduled_messages SET claim_token=$1,claimed_at=$2,occurrence_nonce=$3,delivery_uncertain=$4 WHERE id=$5")
            .bind(claim).bind(claimed_at).bind(nonce).bind(uncertain).bind(ID)
            .execute(db.pool()).await.expect("seed existing delivery state");
        let before = rows(db.pool()).await;
        let metadata = private_text(&invoke(db.pool(), inspect(ID, "metadata")).await);
        assert!(
            metadata.to_ascii_lowercase().contains(state_word),
            "truthful inspect of {state_word}: {metadata}"
        );
        let token = revision(&metadata);
        let reply = private_text(&invoke(db.pool(), edit(ID, &token, NEW)).await);
        retry_instruction(&reply);
        assert_eq!(
            rows(db.pool()).await,
            before,
            "refusal preserves the entire row including expired/NULL evidence"
        );
        assert!(mutation_audits(db.pool()).await.is_empty());
    }
    db.close().await.expect("drop own fixture");
}

#[tokio::test]
async fn schedule_body_edit_prefixes_remain_guild_scoped_literal_and_ambiguity_refuses() {
    let db = database().await;
    seed(db.pool(), GUILD_S, ID, None).await;
    seed(db.pool(), GUILD_S, OTHER, None).await;
    seed(db.pool(), "9999", "foreign-unique", None).await;
    let before = rows(db.pool()).await;
    for id in [
        "",
        " ",
        "aabb",
        "%",
        "_",
        "foreign-unique",
        "ffffffffffffffffffffffffffffffff",
    ] {
        for command in [inspect(id, "metadata"), edit(id, "unused-revision", NEW)] {
            let reply = private_text(&invoke(db.pool(), command).await);
            assert!(
                reply.contains("/schedule-list"),
                "invalid/missing/ambiguous id should direct to canonical id: {reply}"
            );
            assert!(!reply.contains(OLD), "never disclose another guild's body");
            assert_eq!(rows(db.pool()).await, before);
            assert!(mutation_audits(db.pool()).await.is_empty());
        }
    }
    // A unique prefix still resolves to, and edits, the canonical stable id.
    let prefix = &ID[..31];
    // The two seeded ids share 31 characters, so first use a genuinely unique
    // third row rather than accidentally blessing ambiguous prefix matching.
    seed(db.pool(), GUILD_S, "11223344556677889900aabbccddeeff", None).await;
    let metadata = private_text(&invoke(db.pool(), inspect("112233", "metadata")).await);
    assert!(metadata.contains("11223344556677889900aabbccddeeff"));
    saved(&private_text(
        &invoke(db.pool(), edit("112233", &revision(&metadata), NEW)).await,
    ));
    assert_eq!(
        row(db.pool(), "11223344556677889900aabbccddeeff").await["body"],
        NEW
    );
    assert_eq!(row(db.pool(), ID).await["body"], OLD);
    assert_eq!(row(db.pool(), OTHER).await["body"], OLD);
    assert_eq!(prefix, &OTHER[..31], "ambiguous fixture is intentional");
    db.close().await.expect("drop own fixture");
}

#[tokio::test]
async fn schedule_body_edit_unauthorized_wrong_guild_and_failed_ack_have_zero_effects() {
    let db = database().await;
    seed(db.pool(), GUILD_S, ID, None).await;
    let before = rows(db.pool()).await;
    let audit_before = audits(db.pool()).await;
    for base in [
        inspect(ID, "metadata"),
        inspect(ID, "body"),
        edit(ID, "unused-revision", NEW),
    ] {
        for mode in 0..4 {
            let mut request = base.clone();
            match mode {
                0 => {
                    request.member.as_mut().expect("member").permissions =
                        Some(Permissions::empty())
                }
                1 => request.guild_id = Some(Id::new(9999)),
                2 => request.guild_id = None,
                _ => request.member.as_mut().expect("member").permissions = None,
            }
            let requests = invoke(db.pool(), request).await;
            if mode == 1 || mode == 2 {
                assert!(
                    requests.is_empty(),
                    "preserve existing wrong/missing-guild silence"
                );
            } else {
                let reply = private_text(&requests);
                assert!(!reply.contains(OLD) && !reply.contains(NEW));
                assert!(!reply.to_ascii_lowercase().contains("saved"));
                assert_eq!(reply, MANAGE_SERVER_REQUIRED);
            }
            assert_eq!(rows(db.pool()).await, before);
            assert_eq!(
                audits(db.pool()).await,
                audit_before,
                "router authority fence before store/audit"
            );
        }
        let requests = invoke_script(db.pool().clone(), base, vec![403]).await;
        assert_eq!(
            requests.len(),
            1,
            "failed acknowledgement cannot start store work"
        );
        assert_eq!(rows(db.pool()).await, before);
        assert_eq!(audits(db.pool()).await, audit_before);
    }
    db.close().await.expect("drop own fixture");
}

#[tokio::test]
async fn schedule_body_edit_closed_database_never_acknowledges_a_save_or_leaks_details() {
    let db = database().await;
    seed(db.pool(), GUILD_S, ID, None).await;
    let token = inspect_revision(db.pool(), ID).await;
    let before = rows(db.pool()).await;
    let dead = db
        .independent_pool()
        .await
        .expect("independent disposable pool");
    dead.close().await;
    for request in [
        inspect(ID, "metadata"),
        inspect(ID, "body"),
        edit(ID, &token, "private-replacement-sentinel"),
    ] {
        let reply = private_text(&invoke(&dead, request).await);
        let lower = reply.to_ascii_lowercase();
        assert!(
            lower.contains("could not")
                || lower.contains("unavailable")
                || lower.contains("try again"),
            "useful DB refusal: {reply}"
        );
        for raw in [
            "postgres",
            "sqlx",
            "private-replacement-sentinel",
            "poolclosed",
            OLD,
        ] {
            assert!(!lower.contains(&raw.to_ascii_lowercase()));
        }
        assert!(!lower.contains("saved"));
        assert_eq!(rows(db.pool()).await, before);
        assert!(mutation_audits(db.pool()).await.is_empty());
    }
    db.close().await.expect("drop own fixture");
}

#[tokio::test]
async fn schedule_body_edit_audit_failure_rolls_back_body_and_updater_atomically() {
    let db = database().await;
    seed(db.pool(), GUILD_S, ID, None).await;
    let token = inspect_revision(db.pool(), ID).await;
    let before = rows(db.pool()).await;
    let audit_before = audits(db.pool()).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(
        "CREATE FUNCTION reject_body_edit_audit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF NEW.action='scheduled.edit' THEN RAISE EXCEPTION 'audit-error-sentinel'; END IF; RETURN NEW; END $$;
         CREATE TRIGGER reject_body_edit_audit BEFORE INSERT ON automation_audit_log
         FOR EACH ROW EXECUTE FUNCTION reject_body_edit_audit();"
    )).execute(db.pool()).await.expect("fixture-only audit failure");
    let reply =
        private_text(&invoke(db.pool(), edit(ID, &token, "private-replacement-sentinel")).await);
    assert!(!reply.to_ascii_lowercase().contains("saved"));
    assert!(
        !reply.contains("audit-error-sentinel") && !reply.contains("private-replacement-sentinel")
    );
    assert_eq!(
        rows(db.pool()).await,
        before,
        "audit failure cannot leave an unaudited body or timestamp"
    );
    assert_eq!(audits(db.pool()).await, audit_before);
    db.close().await.expect("drop own fixture");
}

#[tokio::test]
async fn schedule_body_edit_stale_inspect_and_concurrent_writers_cannot_overwrite_the_winner() {
    let db = database().await;
    seed(db.pool(), GUILD_S, ID, Some(3600)).await;
    let token = inspect_revision(db.pool(), ID).await;
    let before = row(db.pool(), ID).await;
    let (a, b) = tokio::join!(
        invoke(db.pool(), edit(ID, &token, NEW)),
        invoke(
            db.pool(),
            edit(ID, &token, "Another corrected announcement.")
        ),
    );
    let (a, b) = (private_text(&a), private_text(&b));
    let replies = [&a, &b];
    assert_eq!(
        replies
            .iter()
            .filter(|s| s.to_ascii_lowercase().contains("saved"))
            .count(),
        1,
        "exactly one CAS winner"
    );
    let losing = replies
        .iter()
        .find(|s| !s.to_ascii_lowercase().contains("saved"))
        .expect("stale loser");
    retry_instruction(losing);
    let winner = row(db.pool(), ID).await;
    let body = winner["body"].as_str().expect("winning body");
    assert!(body == NEW || body == "Another corrected announcement.");
    unchanged_except_body_and_updater(&before, &winner, body);
    assert_eq!(mutation_audits(db.pool()).await.len(), 1);
    retry_instruction(&private_text(
        &invoke(db.pool(), edit(ID, &token, "Stale third edit.")).await,
    ));
    assert_eq!(row(db.pool(), ID).await, winner);
    assert_eq!(mutation_audits(db.pool()).await.len(), 1);
    db.close().await.expect("drop own fixture");
}

#[tokio::test]
async fn schedule_body_edit_revision_binds_non_body_definition_and_delivery_evidence_too() {
    let db = database().await;
    seed(db.pool(), GUILD_S, ID, Some(3600)).await;
    for statement in [
        "UPDATE scheduled_messages SET next_run_at='2030-01-01T01:00:00.000Z' WHERE id=$1",
        "UPDATE scheduled_messages SET interval_seconds=7200 WHERE id=$1",
        "UPDATE scheduled_messages SET channel_id='4444' WHERE id=$1",
        "UPDATE scheduled_messages SET enabled=FALSE WHERE id=$1",
        "UPDATE scheduled_messages SET last_message_id='9009',last_run_at='2026-10-02T00:00:00.000Z' WHERE id=$1",
    ] {
        let token = inspect_revision(db.pool(), ID).await;
        sqlx::query(sqlx::AssertSqlSafe(statement)).bind(ID)
            .execute(db.pool()).await.expect("concurrent definition/evidence change");
        let changed = rows(db.pool()).await;
        retry_instruction(&private_text(&invoke(db.pool(), edit(ID, &token, NEW)).await));
        assert_eq!(rows(db.pool()).await, changed, "stale edit cannot win merely because the body is unchanged");
        assert!(mutation_audits(db.pool()).await.is_empty());
    }
    db.close().await.expect("drop own fixture");
}

#[tokio::test]
async fn schedule_body_edit_invalid_id_bounds_refuse_before_database_access() {
    let dead = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("never-connected parser fixture");
    dead.close().await;
    for id in [
        "".to_owned(),
        "x".repeat(129),
        "é".repeat(129),
        "😀".repeat(65),
        format!("{}@everyone", "x".repeat(129)),
    ] {
        for request in [inspect(&id, "metadata"), edit(&id, "unused-revision", NEW)] {
            let text = private_text(&invoke(&dead, request).await);
            assert!(
                text.contains("/schedule-list"),
                "id validation must precede the closed pool: {text}"
            );
            assert!(!text.to_ascii_lowercase().contains("could not"));
            assert!(!text.contains("@everyone"));
        }
    }
}

#[tokio::test]
async fn schedule_body_edit_missing_or_malformed_revision_is_not_a_blind_update() {
    let db = database().await;
    seed(db.pool(), GUILD_S, ID, None).await;
    let before = rows(db.pool()).await;
    let mut missing = edit(ID, "unused-revision", NEW);
    if let Some(InteractionData::ApplicationCommand(command)) = missing.data.as_mut() {
        command.options.retain(|o| o.name != "revision");
    }
    let requests = [
        missing,
        edit(ID, "", NEW),
        edit(ID, &"x".repeat(129), NEW),
        edit(ID, "forged-revision", NEW),
    ];
    for request in requests {
        retry_instruction(&private_text(&invoke(db.pool(), request).await));
        assert_eq!(rows(db.pool()).await, before);
        assert!(mutation_audits(db.pool()).await.is_empty());
    }
    db.close().await.expect("drop own fixture");
}

#[tokio::test]
async fn schedule_body_edit_disabled_definition_is_truthful_and_stays_disabled_after_correction() {
    let db = database().await;
    seed(db.pool(), GUILD_S, ID, None).await;
    sqlx::query("UPDATE scheduled_messages SET enabled=FALSE WHERE id=$1")
        .bind(ID)
        .execute(db.pool())
        .await
        .expect("completed/disabled definition");
    let before = row(db.pool(), ID).await;
    let metadata = private_text(&invoke(db.pool(), inspect(ID, "metadata")).await);
    assert!(metadata.to_ascii_lowercase().contains("disabled"));
    assert!(
        metadata.to_ascii_lowercase().contains("one-shot")
            || metadata.to_ascii_lowercase().contains("once")
    );
    saved(&private_text(
        &invoke(db.pool(), edit(ID, &revision(&metadata), NEW)).await,
    ));
    unchanged_except_body_and_updater(&before, &row(db.pool(), ID).await, NEW);
    assert_eq!(row(db.pool(), ID).await["enabled"], false);
    db.close().await.expect("drop own fixture");
}

// GREEN control: validates the shared command/DB/HTTP fixture even before the
// new commands exist, and retains list discoverability and ticker nonce safety.
#[tokio::test]
async fn schedule_body_edit_control_existing_list_and_ticker_are_real_and_read_only_until_due() {
    let db = database().await;
    seed(db.pool(), GUILD_S, ID, None).await;
    let before = rows(db.pool()).await;
    let listed =
        private_text(&invoke(db.pool(), slash("schedule-list", Some(CHANNEL), Vec::new())).await);
    assert_eq!(
        schedule_list_row_lines(&listed)
            .into_iter()
            .map(schedule_list_row_id)
            .collect::<Vec<_>>(),
        vec![ID],
        "list the complete canonical id exactly once"
    );
    assert!(
        matches!(schedule_list_page_indicator(&listed), None | Some((1, 1))),
        "the existing single-page contract allows an omitted indicator: {listed}"
    );
    assert!(
        !listed.contains(OLD),
        "list remains bounded metadata, not a body dump"
    );
    assert_eq!(rows(db.pool()).await, before);
    assert_eq!(audits(db.pool()).await, json!([]));
    let mock = TickerRest::start(
        vec![ScriptedResponse::json(200, json!({"id":"9001"}))],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = ticker_executor(&mock);
    crate::scheduled_jobs::tick(db.pool(), &rest, GUILD_S, || TICK - 1)
        .await
        .expect("before due");
    assert!(ticker_posts(&mock).is_empty());
    crate::scheduled_jobs::tick(db.pool(), &rest, GUILD_S, || TICK)
        .await
        .expect("exact due boundary");
    let posts = ticker_posts(&mock);
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["content"], OLD);
    assert_eq!(posts[0]["allowed_mentions"]["parse"], json!([]));
    assert_eq!(posts[0]["enforce_nonce"], true);
    assert_eq!(row(db.pool(), ID).await["enabled"], false);
    crate::scheduled_jobs::tick(db.pool(), &rest, GUILD_S, || TICK + 120_000)
        .await
        .expect("later tick");
    assert_eq!(ticker_posts(&mock).len(), 1);
    mock.shutdown().await;
    db.close().await.expect("drop own fixture");
}

fn ticker_posts(mock: &TickerRest) -> Vec<Value> {
    mock.requests()
        .into_iter()
        .filter(|r| r.method == "POST" && r.path == POST_PATH)
        .map(|r| serde_json::from_slice(&r.body).expect("ticker channel payload"))
        .collect()
}

fn ticker_executor(mock: &TickerRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("synthetic-ticker-token".to_owned(), Some(mock.origin()))
        .expect("loopback executor")
}

// Claim wins: the posted old occurrence is held at real HTTP. The attempted edit
// must not clear its claim, nonce or uncertain-send evidence while it is live.
#[tokio::test]
async fn schedule_body_edit_claim_wins_without_mixing_or_duplicating_the_occurrence() {
    let db = database().await;
    seed(db.pool(), GUILD_S, ID, None).await;
    let token = inspect_revision(db.pool(), ID).await;
    let (mock, gate) = TickerRest::start_gated(
        vec![ScriptedResponse::json(200, json!({"id":"9001"}))],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = ticker_executor(&mock);
    let tick_pool = db.pool().clone();
    let task = tokio::spawn(async move {
        crate::scheduled_jobs::tick(&tick_pool, &rest, GUILD_S, || TICK).await
    });
    tokio::time::timeout(Duration::from_secs(3), gate.wait_for_request())
        .await
        .expect("ticker reached channel POST");
    let claimed = row(db.pool(), ID).await;
    assert!(!claimed["claim_token"].is_null());
    assert!(!claimed["occurrence_nonce"].is_null());
    assert_eq!(claimed["delivery_uncertain"], true);
    let reply = private_text(&invoke(db.pool(), edit(ID, &token, NEW)).await);
    let during = row(db.pool(), ID).await;
    gate.release();
    task.await
        .expect("ticker join")
        .expect("ticker settles old occurrence");
    retry_instruction(&reply);
    assert_eq!(
        during, claimed,
        "losing edit cannot modify any live claim field"
    );
    let sent = ticker_posts(&mock);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["content"], OLD);
    let rest = ticker_executor(&mock);
    crate::scheduled_jobs::tick(db.pool(), &rest, GUILD_S, || TICK + 120_000)
        .await
        .expect("later tick");
    assert_eq!(
        ticker_posts(&mock).len(),
        1,
        "one eligible occurrence, no replacement duplicate"
    );
    assert!(mutation_audits(db.pool()).await.is_empty());
    mock.shutdown().await;
    db.close().await.expect("drop own fixture");
}

async fn wait_for_blocked_writer(pool: &PgPool, blocker: i32) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))",
            ).bind(blocker).fetch_one(pool).await.expect("observe real DB lock contention");
            if waiting { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("edit must reach its atomic audit while the definition is locked");
}

// Edit wins: hold its audit INSERT on an advisory lock. A concurrent ticker
// must skip the locked definition, then see exactly the committed new body.
#[tokio::test]
async fn schedule_body_edit_commit_is_linearized_before_a_ticker_claim_and_survives_restart() {
    let db = database().await;
    seed(db.pool(), GUILD_S, ID, Some(3600)).await;
    let token = inspect_revision(db.pool(), ID).await;
    let before = row(db.pool(), ID).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(
        "CREATE FUNCTION hold_body_edit_audit() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN IF NEW.action='scheduled.edit' THEN PERFORM pg_advisory_xact_lock(7128001); END IF; RETURN NEW; END $$;
         CREATE TRIGGER hold_body_edit_audit BEFORE INSERT ON automation_audit_log
         FOR EACH ROW EXECUTE FUNCTION hold_body_edit_audit();"
    )).execute(db.pool()).await.expect("fixture-only audit commit barrier");
    let mut barrier = db.pool().begin().await.expect("barrier transaction");
    let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *barrier)
        .await
        .expect("barrier pid");
    sqlx::query("SELECT pg_advisory_xact_lock(7128001)")
        .execute(&mut *barrier)
        .await
        .expect("hold audit gate");
    let edit_pool = db.pool().clone();
    let task = tokio::spawn(async move {
        invoke(
            &edit_pool,
            edit(ID, &token, "Corrected @everyone discord.gg/event"),
        )
        .await
    });
    wait_for_blocked_writer(db.pool(), blocker).await;
    let mock = TickerRest::start(
        vec![ScriptedResponse::json(200, json!({"id":"9002"}))],
        ScriptedResponse::status(500),
    )
    .await;
    let rest = ticker_executor(&mock);
    let during = tokio::time::timeout(
        Duration::from_secs(2),
        crate::scheduled_jobs::tick(db.pool(), &rest, GUILD_S, || TICK),
    )
    .await;
    barrier.commit().await.expect("release audit barrier");
    let response = task.await.expect("edit join");
    during
        .expect("ticker skips locked rows instead of waiting for the edit")
        .expect("concurrent tick");
    assert!(
        ticker_posts(&mock).is_empty(),
        "no old body escapes before edit commit"
    );
    saved(&private_text(&response));
    unchanged_except_body_and_updater(
        &before,
        &row(db.pool(), ID).await,
        "Corrected @\u{200b}everyone discord.\u{200b}gg/event",
    );
    assert_eq!(mutation_audits(db.pool()).await.len(), 1);
    let peer = db
        .independent_pool()
        .await
        .expect("fresh pool after restart");
    let (a, b) = tokio::join!(
        crate::scheduled_jobs::tick(&peer, &rest, GUILD_S, || TICK),
        crate::scheduled_jobs::tick(db.pool(), &rest, GUILD_S, || TICK),
    );
    a.expect("restart ticker");
    b.expect("concurrent ticker");
    let sent = ticker_posts(&mock);
    assert_eq!(
        sent.len(),
        1,
        "two tickers still post exactly one occurrence"
    );
    assert_eq!(
        sent[0]["content"],
        "Corrected @\u{200b}everyone discord.\u{200b}gg/event"
    );
    assert_eq!(
        sent[0]["allowed_mentions"],
        json!({"parse":[],"roles":[],"users":[],"replied_user":false})
    );
    assert_eq!(sent[0]["enforce_nonce"], true);
    let delivered = row(&peer, ID).await;
    assert_eq!(delivered["id"], ID);
    assert_eq!(delivered["next_run_at"], format_iso_ms(TICK + 3_600_000));
    assert_eq!(delivered["last_message_id"], "9002");
    assert_eq!(delivered["claim_token"], Value::Null);
    crate::scheduled_jobs::tick(&peer, &rest, GUILD_S, || TICK + 15_000)
        .await
        .expect("not due again");
    assert_eq!(ticker_posts(&mock).len(), 1);
    peer.close().await;
    mock.shutdown().await;
    db.close().await.expect("drop own fixture");
}
