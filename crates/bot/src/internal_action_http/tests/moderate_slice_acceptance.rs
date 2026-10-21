#![cfg(test)]
//! Protected acceptance for the admin API moderate slice: warn / timeout /
//! kick / purge / slowmode + `message.delete` + `guild.add_member` via scoped
//! admin calls, with `role.assign` held at admin scope.
//!
//! Every `RED` test below FAILS on the current tree (a scope-claimed request
//! signs the extended canonical string, which the receiver does not verify
//! yet, so every scoped call answers 401 today) and passes once the
//! moderate-slice builder lands on top of slices 0-3. The `CONTROL` tests
//! pass before and after: the legacy unscoped v1 path keeps working and
//! unknown verbs stay refused.
//!
//! What is pinned here (the moderate slice owns the per-verb wiring; slices
//! 0-3 own the mechanisms, and this file is the cross-slice contract where
//! they meet — builders align to it or renegotiate on this card):
//!
//! - warn / timeout / kick / slowmode / `guild.add_member` execute direct at
//!   moderate scope (no confirm) with a gateway audit row (`allowed`) and an
//!   execution receipt carrying token attribution.
//! - purge and `message.delete` refuse without a confirm (403, non-retryable,
//!   zero effects, `denied` audit row), return a plan plus `confirm_id` on
//!   dry-run with zero effects and zero terminal rows, execute exactly once
//!   with a valid confirm (`confirmed` audit row), and refuse a reused
//!   confirm with no second effect.
//! - `role.assign` refuses at moderate scope and executes at admin scope
//!   (same body, same flags: the differential proves scope enforcement, not
//!   breakage). Read scope cannot execute moderate verbs; a scope claim
//!   beyond the token grant refuses; a cross-guild call refuses.
//! - The warn dry-run -> direct-execute -> audit journey mirrors the staging
//!   prove-out for this slice (criterion 3): dry-run plans with zero side
//!   effects, then the direct execute audits. Warn takes no confirm: the
//!   "confirm" in the staging criterion is the execute step itself.
//!
//! Pinned wire choices (slice 1 + 3 builders: these names are the contract):
//!
//! - Scope claims travel in `X-TWO-Scope` (`read` | `moderate` | `admin`),
//!   `X-TWO-Token-Id` (`adm_` + 16 lowercase hex) and `X-TWO-Guild-Id`
//!   (target guild snowflake), all-or-nothing: a partial set is 400
//!   `malformed`, like any other header-shape error.
//! - The MAC covers the v1 canonical string plus `\n{scope}\n{token_id}\n`
//!   `{guild_id}` when all three headers are present, and the frozen v1
//!   string otherwise (unscoped requests verify exactly as today).
//! - `dry_run: true` is a body flag (spec section 5.3). A dry-run answers
//!   200 with `result.dry_run == true`, a `result.plan` object, and — for
//!   confirm-gated verbs — a `result.confirm_id` string. A dry-run never
//!   executes, even when a valid `confirm_id` rides along, and never
//!   consumes the confirm it carries.
//! - `confirm_id` is a body field presenting a dry-run-minted confirm.
//!
//! Deliberately NOT pinned here (mechanism owners decide; these tests assert
//! behavior, not spellings):
//!
//! - Refusal code strings for scope and confirm refusals. These tests assert
//!   403 + `retryable == false` + zero effects + the audit row, and prove
//!   the refusal is the right one differentially (admin scope executes;
//!   a valid confirm executes). Recommended: `action_not_allowed` for scope
//!   refusals (the existing policy-refusal code) and `confirm_required` for
//!   missing or spent confirms, so clients can tell "send a confirm" apart
//!   from "forbidden".
//! - The `result.plan` shape (slice 3 carves it) and the refused-confirm
//!   status for a spent confirm (403 or 409 both satisfy "refused").
//! - `TWO_INTERNAL_ALLOW_ADMIN_ROLES=1` and `TWO_INTERNAL_ALLOW_ADMIN=1` are
//!   set by the role test and `TWO_INTERNAL_ALLOW_MESSAGE_DELETE=1` by the
//!   delete test: the builder MUST NOT require any other flag for these
//!   paths, and moderate verbs MUST NOT require admin flags. Unknown vars
//!   are ignored, so these pins are forward-compatible with a builder that
//!   needs fewer of them.
//!
//! Out of scope: `event.cancel` confirm-gating (slice 6 owns `event.*`),
//! token registry/expiry/revocation mechanics (slice 0), the Worker `/admin`
//! routes and `mcp:moderate` tools (the Worker has no Postgres, so its bearer
//! -> registry seam is slice 0/1 design, still unpublished; the builder
//! unit-tests the route -> verb mapping and the staging prove-out exercises
//! the Worker path end to end), and rate-limit buckets.
//!
//! Fixtures only: synthetic snowflakes, `adm_0*` token ids, fixture HMAC
//! secrets from the shared signing vector. No Discord, no network, no live
//! database: every test runs against its own disposable `TestDatabase` (the
//! suite skips vacuously without `TWO_TEST_DATABASE_URL`, like its siblings).

use super::*;
use hmac::{Hmac, KeyInit as _, Mac as _};
use sqlx::Row as _;
use two_bot_core::internal_actions::body_hash;

/// Guild the fixture tokens are issued for.
const GUILD: &str = "111111111111111111";
/// Guild outside every fixture token's allowlist.
const FOREIGN_GUILD: &str = "222222222222222222";
/// Channel / message fixtures for `message.delete` (raw snowflakes, like the
/// channel-moderation payloads — the guild allowlist, not a key map, is the
/// boundary on the admin path).
const DELETE_CHANNEL_ID: &str = "333333333333333333";
const DELETE_MESSAGE_ID: &str = "444444444444444444";

/// Serializes the tests that set the admin-scope fixture flags below.
/// Leaf under the moderation / add-member locks: acquire those first.
static ADMIN_SCOPE_FLAG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Pinned admin-path flags (see module docs). Unknown vars are ignored, so
/// setting these is harmless if the builder needs fewer of them.
fn set_admin_scope_flags(on: bool) {
    for var in [
        "TWO_INTERNAL_ALLOW_ADMIN",
        "TWO_INTERNAL_ALLOW_ADMIN_ROLES",
        "TWO_INTERNAL_ALLOW_MESSAGE_DELETE",
    ] {
        if on {
            std::env::set_var(var, "1");
        } else {
            std::env::remove_var(var);
        }
    }
}

/// A scope-claimed signed request: the v1 envelope plus the three claim
/// headers, with the MAC covering the extended canonical string.
fn scoped_signed(
    raw: &str,
    key: &str,
    intent: &str,
    scope: &str,
    token_id: &str,
    guild_id: &str,
) -> Request {
    let timestamp = (now_ms() / 1000).to_string();
    let nonce_value = nonce();
    let canonical = format!(
        "POST\n{ACTIONS_PATH}\n{timestamp}\n{nonce_value}\n{}\n{scope}\n{token_id}\n{guild_id}",
        body_hash(raw.as_bytes())
    );
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(secret(usize::from(key == "new")).as_bytes())
            .expect("HMAC accepts keys of any length");
    mac.update(canonical.as_bytes());
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    Request::builder()
        .method(Method::POST)
        .uri(ACTIONS_PATH)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-two-key-id", key)
        .header("x-two-timestamp", timestamp)
        .header("x-two-nonce", nonce_value)
        .header("x-two-signature", signature)
        .header("x-two-scope", scope)
        .header("x-two-token-id", token_id)
        .header("x-two-guild-id", guild_id)
        .header("idempotency-key", intent)
        .body(Body::from(raw.to_owned()))
        .expect("scoped test request")
}

/// Pinned `message.delete` body: actor + channel + message + reason, the same
/// conventions as the channel-moderation verbs.
fn delete_payload() -> String {
    serde_json::json!({
        "action": "message.delete",
        "actor_id": "111111111111111111",
        "channel_id": DELETE_CHANNEL_ID,
        "message_id": DELETE_MESSAGE_ID,
        "reason": "spam",
    })
    .to_string()
}

fn delete_payload_with(extra: Value) -> String {
    let mut body = serde_json::from_str::<Value>(&delete_payload()).expect("delete payload");
    for (key, value) in extra.as_object().expect("extra object") {
        body[key] = value.clone();
    }
    body.to_string()
}

/// True for "relation does not exist" (42P01) and "column does not exist"
/// (42703): the schema slice has not landed yet.
fn is_missing_schema(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(db) => matches!(db.code().as_deref(), Some("42P01" | "42703")),
        _ => false,
    }
}

/// Insert a fixture token straight into the registry (slice 0 schema, spec
/// section 4.1). Panics while the table is absent, naming the missing slice.
async fn ensure_token(pool: &sqlx::PgPool, token_id: &str, scopes: &[&str], guilds: &[i64]) {
    let outcome = sqlx::query(
        "INSERT INTO admin_api_tokens \
         (token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at) \
         VALUES ($1, $2, $3, $4, 'moderate-slice fixture', 'acceptance', now(), now() + interval '90 days')",
    )
    .bind(token_id)
    .bind(body_hash(b"moderate-slice-fixture-secret"))
    .bind(scopes)
    .bind(guilds)
    .execute(pool)
    .await;
    match outcome {
        Ok(_) => {}
        Err(error) if is_missing_schema(&error) => {
            panic!("RED: admin_api_tokens is missing; slice 0 (token registry) has not landed")
        }
        Err(error) => panic!("insert fixture admin token {token_id}: {error}"),
    }
}

/// Gateway audit rows for one token: (scope, guild, verb, dry_run, confirm,
/// decision, receipt). Empty while the audit table is absent, so RED tests
/// fail on their assertions instead of erroring on setup.
async fn gateway_audits(
    pool: &sqlx::PgPool,
    token_id: &str,
) -> Vec<(
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
)> {
    let rows = sqlx::query(
        "SELECT scope::text AS scope, guild_id::text AS guild_id, verb::text AS verb, \
         dry_run::text AS dry_run, confirm_id::text AS confirm_id, decision::text AS decision, \
         receipt::text AS receipt FROM admin_api_audit WHERE token_id = $1 ORDER BY created_at",
    )
    .bind(token_id)
    .fetch_all(pool)
    .await;
    match rows {
        Ok(rows) => rows
            .iter()
            .map(|row| {
                (
                    row.get::<String, _>("scope"),
                    row.get::<String, _>("guild_id"),
                    row.get::<String, _>("verb"),
                    row.get::<String, _>("dry_run"),
                    row.get::<Option<String>, _>("confirm_id"),
                    row.get::<String, _>("decision"),
                    row.get::<Option<String>, _>("receipt"),
                )
            })
            .collect(),
        Err(error) if is_missing_schema(&error) => Vec::new(),
        Err(error) => panic!("read admin_api_audit for {token_id}: {error}"),
    }
}

/// Token attribution on terminal execution rows: (token, scope, dry_run,
/// confirm), one entry per terminal row. Empty while the attribution columns
/// are absent.
async fn terminal_attribution(
    pool: &sqlx::PgPool,
    action: &str,
) -> Vec<(
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
)> {
    let rows = sqlx::query(
        "SELECT token_id::text AS token_id, scope::text AS scope, dry_run::text AS dry_run, \
         confirm_id::text AS confirm_id FROM internal_action_log \
         WHERE action = $1 AND phase = 'terminal'",
    )
    .bind(action)
    .fetch_all(pool)
    .await;
    match rows {
        Ok(rows) => rows
            .iter()
            .map(|row| {
                (
                    row.get::<Option<String>, _>("token_id"),
                    row.get::<Option<String>, _>("scope"),
                    row.get::<Option<String>, _>("dry_run"),
                    row.get::<Option<String>, _>("confirm_id"),
                )
            })
            .collect(),
        Err(error) if is_missing_schema(&error) => Vec::new(),
        Err(error) => panic!("read terminal attribution for {action}: {error}"),
    }
}

async fn terminal_count(pool: &sqlx::PgPool, action: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM internal_action_log WHERE action = $1 AND phase = 'terminal'",
    )
    .bind(action)
    .fetch_one(pool)
    .await
    .expect("count terminal rows")
}

/// Full app with observable doubles for all three moderate families.
fn moderate_app(
    pool: sqlx::PgPool,
    member: Arc<MockEffect>,
    moderation: Arc<MockModeration>,
    channel: Arc<MockChannel>,
) -> Router {
    router(state_full_with_channel(
        pool,
        member,
        Arc::new(MockEventRead::default()),
        Arc::new(MockEventMutate),
        moderation,
        channel,
    ))
}

/// A policy refusal: 403, non-retryable, and (at GREEN) a `denied` gateway
/// row. The code string itself is intentionally unpinned (module docs).
fn assert_policy_refusal(status: StatusCode, body: &Value, what: &str) {
    assert_eq!(status, StatusCode::FORBIDDEN, "{what}");
    assert_eq!(body["error"]["retryable"], false, "{what}");
}

#[tokio::test]
async fn moderate_member_verbs_execute_direct_with_audit_and_no_confirm() {
    let Some(db) = database().await else { return };
    let _flags = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let member = Arc::new(MockEffect::new(MockOutcome::Success));
    let moderation = Arc::new(MockModeration::default());
    let app = moderate_app(
        db.pool().clone(),
        member,
        Arc::clone(&moderation),
        Arc::new(MockChannel::default()),
    );
    let token = "adm_1000000000000001";
    ensure_token(
        db.pool(),
        token,
        &["moderate"],
        &[GUILD.parse().expect("guild")],
    )
    .await;
    for verb in ["moderation.warn", "moderation.timeout", "moderation.kick"] {
        let intent = format!("intent-modaccept-direct-{verb}");
        let (status, _, body) = answer(
            app.clone(),
            scoped_signed(
                &moderation_payload(verb),
                "old",
                &intent,
                "moderate",
                token,
                GUILD,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{verb}");
        assert_eq!(
            body["result"]["outcome"],
            moderation_outcome_for(verb),
            "{verb}"
        );
    }
    set_moderation_flags(false);
    assert_eq!(moderation.calls(), 3, "one effect per direct verb");
    for verb in ["moderation.warn", "moderation.timeout", "moderation.kick"] {
        let rows = gateway_audits(db.pool(), token).await;
        let allowed = rows
            .iter()
            .filter(|row| row.2 == verb && row.5 == "allowed")
            .collect::<Vec<_>>();
        assert_eq!(allowed.len(), 1, "{verb} writes one allowed audit row");
        assert_eq!(allowed[0].0, "moderate", "{verb} audit scope");
        assert_eq!(allowed[0].1, GUILD, "{verb} audit guild");
        assert_eq!(allowed[0].3, "false", "{verb} audit is not a dry-run");
        assert!(
            allowed[0].6.is_some(),
            "{verb} audit row links its execution receipt"
        );
        let terminal = terminal_attribution(db.pool(), verb).await;
        assert_eq!(terminal.len(), 1, "{verb} writes one terminal row");
        assert_eq!(
            terminal[0].0.as_deref(),
            Some(token),
            "{verb} terminal token"
        );
        assert_eq!(
            terminal[0].1.as_deref(),
            Some("moderate"),
            "{verb} terminal scope"
        );
        assert_ne!(
            terminal[0].2.as_deref(),
            Some("true"),
            "{verb} terminal is not a dry-run"
        );
    }
    db.close().await.expect("close");
}

#[tokio::test]
async fn moderate_slowmode_executes_direct_with_audit_and_no_confirm() {
    let Some(db) = database().await else { return };
    let _flags = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let channel = Arc::new(MockChannel::default());
    let app = moderate_app(
        db.pool().clone(),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        Arc::new(MockModeration::default()),
        Arc::clone(&channel),
    );
    let token = "adm_1000000000000002";
    ensure_token(
        db.pool(),
        token,
        &["moderate"],
        &[GUILD.parse().expect("guild")],
    )
    .await;
    let (status, _, body) = answer(
        app,
        scoped_signed(
            &channel_payload("moderation.slowmode"),
            "old",
            "intent-modaccept-direct-slowmode",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    set_moderation_flags(false);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"]["outcome"], "slowmode_updated");
    assert_eq!(channel.calls(), 1, "slowmode executes its effect");
    let rows = gateway_audits(db.pool(), token).await;
    let allowed = rows
        .iter()
        .filter(|row| row.2 == "moderation.slowmode" && row.5 == "allowed")
        .collect::<Vec<_>>();
    assert_eq!(allowed.len(), 1, "slowmode writes one allowed audit row");
    assert_eq!(allowed[0].0, "moderate");
    assert!(allowed[0].6.is_some(), "slowmode audit links its receipt");
    let terminal = terminal_attribution(db.pool(), "moderation.slowmode").await;
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0].0.as_deref(), Some(token));
    assert_eq!(terminal[0].1.as_deref(), Some("moderate"));
    db.close().await.expect("close");
}

#[tokio::test]
async fn moderate_add_member_executes_direct_with_audit_and_no_confirm() {
    let Some(db) = database().await else { return };
    let _flags = MODERATION_FLAG_LOCK.lock().await;
    let _add = ADD_MEMBER_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    set_add_member_flag(true);
    let member = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = moderate_app(
        db.pool().clone(),
        Arc::clone(&member),
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    );
    let token = "adm_1000000000000003";
    ensure_token(
        db.pool(),
        token,
        &["moderate"],
        &[GUILD.parse().expect("guild")],
    )
    .await;
    let raw = serde_json::json!({
        "action": "guild.add_member",
        "discord_id": "111111111111111111",
        "access_token": "fixture-oauth",
    })
    .to_string();
    let (status, _, _) = answer(
        app,
        scoped_signed(
            &raw,
            "old",
            "intent-modaccept-direct-add",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    set_add_member_flag(false);
    set_moderation_flags(false);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(member.calls(), 1, "add_member executes its effect");
    let rows = gateway_audits(db.pool(), token).await;
    let allowed = rows
        .iter()
        .filter(|row| row.2 == "guild.add_member" && row.5 == "allowed")
        .collect::<Vec<_>>();
    assert_eq!(allowed.len(), 1, "add_member writes one allowed audit row");
    assert_eq!(allowed[0].0, "moderate");
    assert!(allowed[0].6.is_some(), "add_member audit links its receipt");
    let terminal = terminal_attribution(db.pool(), "guild.add_member").await;
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0].0.as_deref(), Some(token));
    assert_eq!(terminal[0].1.as_deref(), Some("moderate"));
    db.close().await.expect("close");
}

#[tokio::test]
async fn moderate_purge_requires_confirm_then_executes_exactly_once() {
    let Some(db) = database().await else { return };
    let _flags = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let channel = Arc::new(MockChannel::default());
    let app = moderate_app(
        db.pool().clone(),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        Arc::new(MockModeration::default()),
        Arc::clone(&channel),
    );
    let token = "adm_1000000000000004";
    ensure_token(
        db.pool(),
        token,
        &["moderate"],
        &[GUILD.parse().expect("guild")],
    )
    .await;

    // Without a confirm the purge refuses: no effect, denied audit row.
    let (status, _, body) = answer(
        app.clone(),
        scoped_signed(
            &channel_payload("moderation.purge"),
            "old",
            "intent-modaccept-purge-noconfirm",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    assert_policy_refusal(status, &body, "purge without confirm");
    assert_eq!(channel.calls(), 0, "unconfirmed purge never purges");

    // A dry-run plans with zero effects and zero terminal rows, and mints the
    // confirm the execute step presents.
    let terminals_before = terminal_count(db.pool(), "moderation.purge").await;
    let (status, _, dry) = answer(
        app.clone(),
        scoped_signed(
            &channel_payload_with("moderation.purge", serde_json::json!({"dry_run": true})),
            "old",
            "intent-modaccept-purge-dry",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "purge dry-run plans");
    assert_eq!(dry["result"]["dry_run"], true);
    assert!(dry["result"]["plan"].is_object(), "dry-run carries a plan");
    let confirm = dry["result"]["confirm_id"]
        .as_str()
        .expect("purge dry-run mints a confirm_id")
        .to_owned();
    assert_eq!(channel.calls(), 0, "dry-run purges nothing");
    assert_eq!(
        terminal_count(db.pool(), "moderation.purge").await,
        terminals_before,
        "dry-run writes zero terminal rows"
    );

    // The confirm executes exactly once; its audit row says `confirmed` and
    // the terminal row carries the confirm id.
    let (status, _, body) = answer(
        app.clone(),
        scoped_signed(
            &channel_payload_with(
                "moderation.purge",
                serde_json::json!({"confirm_id": confirm}),
            ),
            "old",
            "intent-modaccept-purge-exec",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    set_moderation_flags(false);
    assert_eq!(status, StatusCode::OK, "confirmed purge executes");
    assert_eq!(body["result"]["outcome"], "purged");
    assert_eq!(channel.calls(), 1, "one effect for one confirm");

    // A spent confirm refuses with no second effect.
    let (status, _, _) = answer(
        app,
        scoped_signed(
            &channel_payload_with(
                "moderation.purge",
                serde_json::json!({"confirm_id": confirm}),
            ),
            "old",
            "intent-modaccept-purge-reuse",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    assert_ne!(status, StatusCode::OK, "a spent confirm never re-executes");
    assert_eq!(channel.calls(), 1, "no second effect for a spent confirm");

    let rows = gateway_audits(db.pool(), token).await;
    let purge_rows = rows
        .iter()
        .filter(|row| row.2 == "moderation.purge")
        .collect::<Vec<_>>();
    assert!(
        purge_rows.iter().any(|row| row.5 == "denied"),
        "the unconfirmed purge audits as denied"
    );
    assert!(
        purge_rows.iter().any(|row| row.5 == "dry_run"),
        "the dry-run audits as dry_run"
    );
    let confirmed = purge_rows
        .iter()
        .filter(|row| row.5 == "confirmed")
        .collect::<Vec<_>>();
    assert_eq!(confirmed.len(), 1, "the execute audits as confirmed");
    assert_eq!(confirmed[0].4.as_deref(), Some(confirm.as_str()));
    assert!(
        confirmed[0].6.is_some(),
        "confirmed audit links its receipt"
    );
    let terminal = terminal_attribution(db.pool(), "moderation.purge").await;
    assert_eq!(terminal.len(), 1, "one terminal row for one purge");
    assert_eq!(terminal[0].0.as_deref(), Some(token));
    assert_eq!(terminal[0].1.as_deref(), Some("moderate"));
    assert_eq!(terminal[0].3.as_deref(), Some(confirm.as_str()));
    db.close().await.expect("close");
}

#[tokio::test]
async fn moderate_message_delete_requires_confirm_then_executes_exactly_once() {
    let Some(db) = database().await else { return };
    let _flags = MODERATION_FLAG_LOCK.lock().await;
    let _admin = ADMIN_SCOPE_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    set_admin_scope_flags(true);
    let app = moderate_app(
        db.pool().clone(),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    );
    let token = "adm_1000000000000005";
    ensure_token(
        db.pool(),
        token,
        &["moderate"],
        &[GUILD.parse().expect("guild")],
    )
    .await;

    let (status, _, body) = answer(
        app.clone(),
        scoped_signed(
            &delete_payload(),
            "old",
            "intent-modaccept-del-noconfirm",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    assert_policy_refusal(status, &body, "delete without confirm");

    let terminals_before = terminal_count(db.pool(), "message.delete").await;
    let (status, _, dry) = answer(
        app.clone(),
        scoped_signed(
            &delete_payload_with(serde_json::json!({"dry_run": true})),
            "old",
            "intent-modaccept-del-dry",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "delete dry-run plans");
    assert_eq!(dry["result"]["dry_run"], true);
    assert!(dry["result"]["plan"].is_object(), "dry-run carries a plan");
    let confirm = dry["result"]["confirm_id"]
        .as_str()
        .expect("delete dry-run mints a confirm_id")
        .to_owned();
    assert_eq!(
        terminal_count(db.pool(), "message.delete").await,
        terminals_before,
        "dry-run writes zero terminal rows"
    );

    let (status, _, body) = answer(
        app.clone(),
        scoped_signed(
            &delete_payload_with(serde_json::json!({"confirm_id": confirm})),
            "old",
            "intent-modaccept-del-exec",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    set_admin_scope_flags(false);
    set_moderation_flags(false);
    assert_eq!(status, StatusCode::OK, "confirmed delete executes");
    assert_eq!(body["result"]["outcome"], "deleted");

    let (status, _, _) = answer(
        app,
        scoped_signed(
            &delete_payload_with(serde_json::json!({"confirm_id": confirm})),
            "old",
            "intent-modaccept-del-reuse",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    assert_ne!(status, StatusCode::OK, "a spent confirm never re-executes");

    let rows = gateway_audits(db.pool(), token).await;
    let delete_rows = rows
        .iter()
        .filter(|row| row.2 == "message.delete")
        .collect::<Vec<_>>();
    assert!(
        delete_rows.iter().any(|row| row.5 == "denied"),
        "the unconfirmed delete audits as denied"
    );
    assert!(
        delete_rows.iter().any(|row| row.5 == "dry_run"),
        "the dry-run audits as dry_run"
    );
    let confirmed = delete_rows
        .iter()
        .filter(|row| row.5 == "confirmed")
        .collect::<Vec<_>>();
    assert_eq!(confirmed.len(), 1, "the execute audits as confirmed");
    assert_eq!(confirmed[0].4.as_deref(), Some(confirm.as_str()));
    assert!(
        confirmed[0].6.is_some(),
        "confirmed audit links its receipt"
    );
    let terminal = terminal_attribution(db.pool(), "message.delete").await;
    assert_eq!(terminal.len(), 1, "one terminal row for one delete");
    assert_eq!(terminal[0].0.as_deref(), Some(token));
    assert_eq!(terminal[0].1.as_deref(), Some("moderate"));
    assert_eq!(terminal[0].3.as_deref(), Some(confirm.as_str()));
    db.close().await.expect("close");
}

#[tokio::test]
async fn moderate_scope_refuses_role_assign_while_admin_scope_allows() {
    let Some(db) = database().await else { return };
    let _admin = ADMIN_SCOPE_FLAG_LOCK.lock().await;
    set_admin_scope_flags(true);
    let member = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = moderate_app(
        db.pool().clone(),
        Arc::clone(&member),
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    );
    let moderate_token = "adm_1000000000000006";
    let admin_token = "adm_1000000000000007";
    ensure_token(
        db.pool(),
        moderate_token,
        &["moderate"],
        &[GUILD.parse().expect("guild")],
    )
    .await;
    ensure_token(
        db.pool(),
        admin_token,
        &["admin"],
        &[GUILD.parse().expect("guild")],
    )
    .await;
    let raw = serde_json::json!({
        "action": "role.assign",
        "discord_id": "111111111111111111",
        "role_key": "member",
    })
    .to_string();

    let (status, _, body) = answer(
        app.clone(),
        scoped_signed(
            &raw,
            "old",
            "intent-modaccept-role-mod",
            "moderate",
            moderate_token,
            GUILD,
        ),
    )
    .await;
    assert_policy_refusal(status, &body, "role.assign at moderate scope");
    assert_eq!(member.calls(), 0, "moderate scope never assigns roles");

    let (status, _, _) = answer(
        app,
        scoped_signed(
            &raw,
            "old",
            "intent-modaccept-role-admin",
            "admin",
            admin_token,
            GUILD,
        ),
    )
    .await;
    set_admin_scope_flags(false);
    assert_eq!(
        status,
        StatusCode::OK,
        "role.assign at admin scope executes"
    );
    assert_eq!(member.calls(), 1, "admin scope assigns the role");

    let rows = gateway_audits(db.pool(), moderate_token).await;
    assert!(
        rows.iter()
            .any(|row| row.2 == "role.assign" && row.5 == "denied"),
        "the moderate refusal audits as denied"
    );
    let rows = gateway_audits(db.pool(), admin_token).await;
    let allowed = rows
        .iter()
        .filter(|row| row.2 == "role.assign" && row.5 == "allowed")
        .collect::<Vec<_>>();
    assert_eq!(allowed.len(), 1, "the admin execute audits as allowed");
    assert!(allowed[0].6.is_some(), "admin audit links its receipt");
    db.close().await.expect("close");
}

#[tokio::test]
async fn moderate_verbs_honor_scope_and_guild_boundaries() {
    let Some(db) = database().await else { return };
    let _flags = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let moderation = Arc::new(MockModeration::default());
    let app = moderate_app(
        db.pool().clone(),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        Arc::clone(&moderation),
        Arc::new(MockChannel::default()),
    );
    let read_token = "adm_1000000000000008";
    let moderate_token = "adm_1000000000000009";
    ensure_token(
        db.pool(),
        read_token,
        &["read"],
        &[GUILD.parse().expect("guild")],
    )
    .await;
    ensure_token(
        db.pool(),
        moderate_token,
        &["moderate"],
        &[GUILD.parse().expect("guild")],
    )
    .await;
    let raw = moderation_payload("moderation.warn");

    // Read scope cannot warn.
    let (status, _, body) = answer(
        app.clone(),
        scoped_signed(
            &raw,
            "old",
            "intent-modaccept-bound-read",
            "read",
            read_token,
            GUILD,
        ),
    )
    .await;
    assert_policy_refusal(status, &body, "warn at read scope");

    // A moderate token claiming admin refuses: the claim exceeds the grant.
    let (status, _, body) = answer(
        app.clone(),
        scoped_signed(
            &raw,
            "old",
            "intent-modaccept-bound-claim",
            "admin",
            moderate_token,
            GUILD,
        ),
    )
    .await;
    assert_policy_refusal(status, &body, "admin claim on a moderate token");

    // A guild outside the token allowlist refuses.
    let (status, _, body) = answer(
        app.clone(),
        scoped_signed(
            &raw,
            "old",
            "intent-modaccept-bound-guild",
            "moderate",
            moderate_token,
            FOREIGN_GUILD,
        ),
    )
    .await;
    set_moderation_flags(false);
    assert_policy_refusal(status, &body, "cross-guild warn");
    assert_eq!(moderation.calls(), 0, "boundary refusals never warn");

    let rows = gateway_audits(db.pool(), read_token).await;
    assert!(
        rows.iter()
            .any(|row| row.2 == "moderation.warn" && row.5 == "denied"),
        "the read-scope refusal audits as denied"
    );
    let rows = gateway_audits(db.pool(), moderate_token).await;
    let denied = rows
        .iter()
        .filter(|row| row.2 == "moderation.warn" && row.5 == "denied")
        .collect::<Vec<_>>();
    assert_eq!(denied.len(), 2, "claim and guild refusals audit as denied");
    db.close().await.expect("close");
}

#[tokio::test]
async fn warn_dry_run_plans_without_effects_then_direct_execute_audits() {
    // Local mirror of the staging prove-out: dry-run, direct execute (warn
    // takes no confirm), audit. See the module docs on the "confirm" wording.
    let Some(db) = database().await else { return };
    let _flags = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let moderation = Arc::new(MockModeration::default());
    let app = moderate_app(
        db.pool().clone(),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        Arc::clone(&moderation),
        Arc::new(MockChannel::default()),
    );
    let token = "adm_1000000000000010";
    ensure_token(
        db.pool(),
        token,
        &["moderate"],
        &[GUILD.parse().expect("guild")],
    )
    .await;
    let terminals_before = terminal_count(db.pool(), "moderation.warn").await;

    let (status, _, dry) = answer(
        app.clone(),
        scoped_signed(
            &moderation_payload_with("moderation.warn", serde_json::json!({"dry_run": true})),
            "old",
            "intent-modaccept-warn-dry",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "warn dry-run plans");
    assert_eq!(dry["result"]["dry_run"], true);
    assert!(dry["result"]["plan"].is_object(), "dry-run carries a plan");
    assert_eq!(moderation.calls(), 0, "dry-run warns nothing");
    assert_eq!(
        terminal_count(db.pool(), "moderation.warn").await,
        terminals_before,
        "dry-run writes zero terminal rows"
    );

    let (status, _, body) = answer(
        app,
        scoped_signed(
            &moderation_payload("moderation.warn"),
            "old",
            "intent-modaccept-warn-exec",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    set_moderation_flags(false);
    assert_eq!(status, StatusCode::OK, "warn executes direct, no confirm");
    assert_eq!(body["result"]["outcome"], "warned");
    assert_eq!(moderation.calls(), 1, "the execute warns once");

    let rows = gateway_audits(db.pool(), token).await;
    let warn_rows = rows
        .iter()
        .filter(|row| row.2 == "moderation.warn")
        .collect::<Vec<_>>();
    assert!(
        warn_rows.iter().any(|row| row.5 == "dry_run"),
        "the dry-run audits as dry_run"
    );
    let allowed = warn_rows
        .iter()
        .filter(|row| row.5 == "allowed")
        .collect::<Vec<_>>();
    assert_eq!(allowed.len(), 1, "the execute audits as allowed");
    assert!(allowed[0].6.is_some(), "execute audit links its receipt");
    db.close().await.expect("close");
}

#[tokio::test]
async fn dry_run_with_confirm_id_still_plans_and_keeps_the_confirm_live() {
    // Purity in both directions: a dry-run never executes even when a valid
    // confirm rides along, and it never consumes that confirm.
    let Some(db) = database().await else { return };
    let _flags = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let channel = Arc::new(MockChannel::default());
    let app = moderate_app(
        db.pool().clone(),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        Arc::new(MockModeration::default()),
        Arc::clone(&channel),
    );
    let token = "adm_1000000000000011";
    ensure_token(
        db.pool(),
        token,
        &["moderate"],
        &[GUILD.parse().expect("guild")],
    )
    .await;

    let (status, _, dry) = answer(
        app.clone(),
        scoped_signed(
            &channel_payload_with("moderation.purge", serde_json::json!({"dry_run": true})),
            "old",
            "intent-modaccept-pure-dry1",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let confirm = dry["result"]["confirm_id"]
        .as_str()
        .expect("purge dry-run mints a confirm_id")
        .to_owned();

    let terminals_before = terminal_count(db.pool(), "moderation.purge").await;
    let (status, _, dry) = answer(
        app.clone(),
        scoped_signed(
            &channel_payload_with(
                "moderation.purge",
                serde_json::json!({"dry_run": true, "confirm_id": confirm}),
            ),
            "old",
            "intent-modaccept-pure-dry2",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "dry-run with a confirm still plans");
    assert_eq!(dry["result"]["dry_run"], true);
    assert_eq!(channel.calls(), 0, "a dry-run never executes");
    assert_eq!(
        terminal_count(db.pool(), "moderation.purge").await,
        terminals_before,
        "dry-run writes zero terminal rows"
    );

    // The confirm survived both dry-runs: it still executes exactly once.
    let (status, _, body) = answer(
        app,
        scoped_signed(
            &channel_payload_with(
                "moderation.purge",
                serde_json::json!({"confirm_id": confirm}),
            ),
            "old",
            "intent-modaccept-pure-exec",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    set_moderation_flags(false);
    assert_eq!(status, StatusCode::OK, "the live confirm executes");
    assert_eq!(body["result"]["outcome"], "purged");
    assert_eq!(channel.calls(), 1, "one effect for one confirm");
    db.close().await.expect("close");
}

#[tokio::test]
async fn unscoped_v1_moderate_verbs_still_execute() {
    // CONTROL: the legacy website path carries no scope claims and must keep
    // working after the admin surface lands. Passes before and after.
    let Some(db) = database().await else { return };
    let _flags = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let moderation = Arc::new(MockModeration::default());
    let app = moderate_app(
        db.pool().clone(),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        Arc::clone(&moderation),
        Arc::new(MockChannel::default()),
    );
    let (status, _, body) = answer(
        app,
        signed(
            &moderation_payload("moderation.warn"),
            "old",
            "intent-modaccept-legacy-v1",
        ),
    )
    .await;
    set_moderation_flags(false);
    assert_eq!(status, StatusCode::OK, "unscoped v1 warn executes");
    assert_eq!(body["result"]["outcome"], "warned");
    assert_eq!(moderation.calls(), 1);
    db.close().await.expect("close");
}

#[tokio::test]
async fn moderate_scope_unknown_verb_stays_refused_without_effect() {
    // CONTROL: scope claims never widen the verb allowlist. Passes before
    // (401, unknown signature) and after (403, unknown verb) alike.
    let Some(db) = database().await else { return };
    let _flags = MODERATION_FLAG_LOCK.lock().await;
    set_moderation_flags(true);
    let moderation = Arc::new(MockModeration::default());
    let channel = Arc::new(MockChannel::default());
    let member = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = moderate_app(
        db.pool().clone(),
        Arc::clone(&member),
        Arc::clone(&moderation),
        Arc::clone(&channel),
    );
    let token = "adm_1000000000000012";
    // No registry row on purpose: the verb is refused before any token row
    // could matter (today at the MAC, at GREEN at the verb allowlist), which
    // is what keeps this a control in both states.
    let raw = serde_json::json!({
        "action": "moderation.nonexistent",
        "actor_id": "111111111111111111",
        "discord_id": "333333333333333333",
        "reason": "spam",
    })
    .to_string();
    let (status, _, _) = answer(
        app,
        scoped_signed(
            &raw,
            "old",
            "intent-modaccept-unknown",
            "moderate",
            token,
            GUILD,
        ),
    )
    .await;
    set_moderation_flags(false);
    assert_ne!(status, StatusCode::OK, "unknown verbs never execute");
    assert_eq!(moderation.calls(), 0);
    assert_eq!(channel.calls(), 0);
    assert_eq!(member.calls(), 0);
    db.close().await.expect("close");
}
