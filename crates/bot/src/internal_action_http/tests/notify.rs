#![cfg(test)]
//! Member-notify DM verbs (`reminder.send`, `place_offer.send`): flag gating,
//! fact validation, the `sent` / `dm_unavailable` terminal envelopes with
//! their audit scalars, replay-once, 429 release, and unknown retention.
//! Only guarded TestDatabase pools; Discord stays behind the shared mock.
use super::*;
use two_bot_core::internal_actions::{PlaceOfferSendRequest, ReminderSendRequest};

/// One flag gates both DM-delivery verbs; tests serialize on their own lock
/// and always restore the var, like the add-member gate.
static MEMBER_NOTIFY_FLAG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn set_member_notify_flag(on: bool) {
    if on {
        std::env::set_var("TWO_INTERNAL_ALLOW_MEMBER_NOTIFY", "1");
    } else {
        std::env::remove_var("TWO_INTERNAL_ALLOW_MEMBER_NOTIFY");
    }
}

const MEMBER: &str = "111111111111111111";

async fn assert_event_key_audit(pool: &sqlx::PgPool) {
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT event_key FROM internal_idempotency \
         UNION ALL SELECT event_key FROM internal_action_log",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(keys, vec!["launch:2026"; 3], "intent and both audit phases");
}

fn reminder_payload() -> String {
    serde_json::json!({
        "action": "reminder.send",
        "discord_id": MEMBER,
        "event_key": "launch:2026",
        "kind": "starting_soon",
        "starts_at": "2026-11-04T18:00:00Z",
        "event_name": "Launch Night",
    })
    .to_string()
}

fn offer_payload() -> String {
    serde_json::json!({
        "action": "place_offer.send",
        "discord_id": MEMBER,
        "event_key": "launch:2026",
        "event_name": "Launch Night",
        "expires_at": "2026-11-04T18:00:00Z",
    })
    .to_string()
}

/// Notify signing without an `Idempotency-Key` header: a repeat DM is not
/// harmless, so both verbs refuse keyless calls before any effect.
fn signed_notify_without_key(raw: &str, key: &str) -> Request {
    let timestamp = (now_ms() / 1000).to_string();
    let nonce_value = nonce();
    let signature = sign(
        secret(usize::from(key == "new")).as_bytes(),
        &timestamp,
        &nonce_value,
        raw.as_bytes(),
    );
    Request::builder()
        .method(Method::POST)
        .uri(ACTIONS_PATH)
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-two-key-id", key)
        .header("x-two-timestamp", timestamp)
        .header("x-two-nonce", nonce_value)
        .header("x-two-signature", signature)
        .body(Body::from(raw.to_owned()))
        .unwrap()
}

/// A 429-only notify double: proves the receiver releases the claim and
/// answers `rate_limited` with `Retry-After`, then lets the same key retry.
struct RateLimitedNotify {
    calls: AtomicUsize,
}

impl RateLimitedNotify {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn limited(&self) -> ActionError {
        ActionError::new(
            ErrorCode::RateLimited,
            "Discord rate-limited this request",
            "discord_rate_limited",
        )
        .with_retry_after(7)
    }
}

impl NotifyEffect for RateLimitedNotify {
    fn execute_reminder<'a>(
        &'a self,
        _: &'a ReminderSendRequest<'a>,
    ) -> BoxFuture<'a, Result<NotifyOutcome, ActionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(self.limited())
        })
    }

    fn execute_place_offer<'a>(
        &'a self,
        _: &'a PlaceOfferSendRequest<'a>,
    ) -> BoxFuture<'a, Result<NotifyOutcome, ActionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(self.limited())
        })
    }
}

#[tokio::test]
async fn notify_verbs_stay_dark_without_the_flag() {
    let Some(db) = database().await else { return };
    let _flag = MEMBER_NOTIFY_FLAG_LOCK.lock().await;
    set_member_notify_flag(false);
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(state(db.pool().clone(), effect.clone()));
    for (raw, intent) in [
        (reminder_payload(), "intent-notify-dark-reminder"),
        (offer_payload(), "intent-notify-dark-offer"),
    ] {
        let (_, _, refused) = answer(app.clone(), signed(&raw, "old", intent)).await;
        assert_eq!(refused["error"]["code"], "action_not_allowed", "{intent}");
    }
    assert_eq!(effect.calls(), 0);
    let claims: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(claims, 0, "authorize refuses before the claim");
    db.close().await.unwrap();
}

#[tokio::test]
async fn notify_missing_idempotency_key_is_refused_before_any_effect() {
    let Some(db) = database().await else { return };
    let _flag = MEMBER_NOTIFY_FLAG_LOCK.lock().await;
    set_member_notify_flag(true);
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(state(db.pool().clone(), effect.clone()));
    for (raw, intent) in [
        (reminder_payload(), "keyless must still refuse"),
        (offer_payload(), "keyless must still refuse"),
    ] {
        let (status, _, body) = answer(app.clone(), signed_notify_without_key(&raw, "old")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{intent}");
        assert_eq!(body["error"]["code"], "malformed");
    }
    assert_eq!(effect.calls(), 0, "keyless calls never reach the effect");
    set_member_notify_flag(false);
    db.close().await.unwrap();
}

#[tokio::test]
async fn notify_malformed_facts_are_refused_before_any_effect() {
    let Some(db) = database().await else { return };
    let _flag = MEMBER_NOTIFY_FLAG_LOCK.lock().await;
    set_member_notify_flag(true);
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(state(db.pool().clone(), effect.clone()));
    let reminder = reminder_payload();
    let offer = offer_payload();
    let mut bad_kind: Value = serde_json::from_str(&reminder).unwrap();
    bad_kind["kind"] = serde_json::json!("sesame_street");
    let mut with_body: Value = serde_json::from_str(&reminder).unwrap();
    with_body["body"] = serde_json::json!("caller words never reach a DM");
    let mut bad_key: Value = serde_json::from_str(&reminder).unwrap();
    bad_key["event_key"] = serde_json::json!("has space");
    let mut long_key: Value = serde_json::from_str(&reminder).unwrap();
    long_key["event_key"] = serde_json::json!("k".repeat(65));
    let mut long_name: Value = serde_json::from_str(&offer).unwrap();
    long_name["event_name"] = serde_json::json!("n".repeat(101));
    let mut bad_time: Value = serde_json::from_str(&reminder).unwrap();
    bad_time["starts_at"] = serde_json::json!("next Tuesday");
    let mut bad_member: Value = serde_json::from_str(&offer).unwrap();
    bad_member["discord_id"] = serde_json::json!("not-an-id");
    for (raw, intent) in [
        (bad_kind.to_string(), "intent-notify-bad-kind"),
        (with_body.to_string(), "intent-notify-body"),
        (bad_key.to_string(), "intent-notify-bad-key"),
        (long_key.to_string(), "intent-notify-long-key"),
        (long_name.to_string(), "intent-notify-long-name"),
        (bad_time.to_string(), "intent-notify-bad-time"),
        (bad_member.to_string(), "intent-notify-bad-member"),
    ] {
        let (status, _, body) = answer(app.clone(), signed(&raw, "old", intent)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{intent}");
        assert_eq!(body["error"]["code"], "malformed", "{intent}");
    }
    assert_eq!(effect.calls(), 0, "refusals must not reach Discord");
    let claims: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(claims, 0, "validation refuses before the claim");
    set_member_notify_flag(false);
    db.close().await.unwrap();
}

#[tokio::test]
async fn reminder_send_succeeds_with_sent_envelope_and_audit_scalars() {
    let Some(db) = database().await else { return };
    let _flag = MEMBER_NOTIFY_FLAG_LOCK.lock().await;
    set_member_notify_flag(true);
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(state(db.pool().clone(), effect.clone()));
    let raw = reminder_payload();
    let (status, _, first) =
        answer(app.clone(), signed(&raw, "old", "intent-notify-reminder")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["result"], json!({"outcome": "sent"}));
    assert!(first["request_id"].as_str().is_some());
    assert!(
        first["result"].get("message_id").is_none(),
        "wire carries the outcome word, not the message id"
    );
    assert_eq!(effect.calls(), 1);
    // Fresh nonces replay the receipt without spending member allowance.
    // More replays than the entire burst must still return the same receipt.
    for _ in 0..5 {
        let (status, headers, replay) =
            answer(app.clone(), signed(&raw, "new", "intent-notify-reminder")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["idempotent-replay"], "true");
        assert_eq!(replay["result"], json!({"outcome": "sent"}));
        assert_eq!(effect.calls(), 1);
    }
    // Stored receipt: completed success with the message-id scalar, the
    // member/event linkage, and no DM text.
    let rows: Vec<StoredReceiptRow> = sqlx::query_as(
        "SELECT action, state, response_code, http_status, resource_id, affected, \
         guild_id, actor_id, target_id, resolved_role_id \
         FROM internal_idempotency",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "replay creates no extra intent row");
    let row = &rows[0];
    assert_eq!(row.0, "reminder.send");
    assert_eq!(row.1, "completed");
    assert_eq!(row.2.as_deref(), Some("success"));
    assert_eq!(row.3, Some(200));
    assert_eq!(row.4.as_deref(), Some("444444444444444444"));
    assert_eq!(row.5, Some(1));
    assert_eq!(row.6.as_deref(), Some(staging_guild()));
    assert_eq!(row.7, None, "no actor on a member-notify receipt");
    assert_eq!(row.8.as_deref(), Some(MEMBER));
    assert_eq!(row.9, None, "no resolved role on a member-notify receipt");
    let audit: Vec<TerminalAuditRow> = sqlx::query_as(
        "SELECT action, phase, response_code, evidence_code, http_status, guild_id, \
         target_id, resolved_role_id FROM internal_action_log ORDER BY audit_id",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(audit.len(), 2, "intent + terminal audit rows");
    assert_eq!(audit[0].1, "intent");
    assert_eq!(audit[1].1, "terminal");
    assert_eq!(audit[1].2.as_deref(), Some("success"));
    assert_event_key_audit(db.pool()).await;
    set_member_notify_flag(false);
    db.close().await.unwrap();
}

#[tokio::test]
async fn place_offer_dm_unavailable_is_200_and_consumes_no_place() {
    let Some(db) = database().await else { return };
    let _flag = MEMBER_NOTIFY_FLAG_LOCK.lock().await;
    set_member_notify_flag(true);
    let effect = Arc::new(MockEffect::new(MockOutcome::NoEffect));
    let app = router(state(db.pool().clone(), effect.clone()));
    let raw = offer_payload();
    let (status, _, first) = answer(app.clone(), signed(&raw, "old", "intent-notify-offer")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["result"], json!({"outcome": "dm_unavailable"}));
    assert_eq!(effect.calls(), 1);
    // The refusal replays as the same 200 outcome, never a second Discord call.
    let (status, headers, replay) =
        answer(app.clone(), signed(&raw, "new", "intent-notify-offer")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(replay["result"], json!({"outcome": "dm_unavailable"}));
    assert_eq!(effect.calls(), 1);
    // Stored receipt: completed success with `affected` 0 and no resource —
    // the site reads "no place consumed" from the same outcome bit the
    // membership no-ops use.
    let rows: Vec<StoredReceiptRow> = sqlx::query_as(
        "SELECT action, state, response_code, http_status, resource_id, affected, \
         guild_id, actor_id, target_id, resolved_role_id \
         FROM internal_idempotency",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "replay creates no extra intent row");
    let row = &rows[0];
    assert_eq!(row.0, "place_offer.send");
    assert_eq!(row.1, "completed");
    assert_eq!(row.2.as_deref(), Some("success"));
    assert_eq!(row.3, Some(200));
    assert_eq!(row.4, None);
    assert_eq!(row.5, Some(0));
    assert_eq!(row.6.as_deref(), Some(staging_guild()));
    assert_eq!(row.8.as_deref(), Some(MEMBER));
    assert_event_key_audit(db.pool()).await;
    set_member_notify_flag(false);
    db.close().await.unwrap();
}

#[tokio::test]
async fn notify_suppressed_member_answers_dm_unavailable_without_sending() {
    let Some(db) = database().await else { return };
    let _flag = MEMBER_NOTIFY_FLAG_LOCK.lock().await;
    set_member_notify_flag(true);
    // Contract v2 §7 consent backstop: the member opted out of per-user
    // persistence in the bot's guild, so the bot never messages them.
    sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
        .bind(staging_guild())
        .bind(MEMBER)
        .execute(db.pool())
        .await
        .unwrap();
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(state(db.pool().clone(), effect.clone()));
    let raw = reminder_payload();
    let (status, _, first) =
        answer(app.clone(), signed(&raw, "old", "intent-notify-suppressed")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["result"], json!({"outcome": "dm_unavailable"}));
    assert_eq!(effect.calls(), 0, "suppressed members are never messaged");
    // The refusal replays as the same 200 outcome without re-checking.
    let (status, headers, replay) =
        answer(app.clone(), signed(&raw, "new", "intent-notify-suppressed")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["idempotent-replay"], "true");
    assert_eq!(replay["result"], json!({"outcome": "dm_unavailable"}));
    assert_eq!(effect.calls(), 0);
    // Stored receipt keeps the member linkage with no message id; the
    // terminal audit carries the `suppressed` scalar instead of `executor`.
    let rows: Vec<StoredReceiptRow> = sqlx::query_as(
        "SELECT action, state, response_code, http_status, resource_id, affected, \
         guild_id, actor_id, target_id, resolved_role_id \
         FROM internal_idempotency",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "replay creates no extra intent row");
    let row = &rows[0];
    assert_eq!(row.0, "reminder.send");
    assert_eq!(row.1, "completed");
    assert_eq!(row.4, None);
    assert_eq!(row.5, Some(0));
    assert_eq!(row.8.as_deref(), Some(MEMBER));
    let audit: Vec<TerminalAuditRow> = sqlx::query_as(
        "SELECT action, phase, response_code, evidence_code, http_status, guild_id, \
         target_id, resolved_role_id FROM internal_action_log ORDER BY audit_id",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(audit.len(), 2, "intent + terminal audit rows");
    assert_eq!(audit[0].1, "intent");
    assert_eq!(audit[1].1, "terminal");
    assert_eq!(audit[1].3.as_deref(), Some("suppressed"));
    assert_event_key_audit(db.pool()).await;
    set_member_notify_flag(false);
    db.close().await.unwrap();
}

#[tokio::test]
async fn notify_zero_padded_alias_cannot_message_a_suppressed_member() {
    let Some(db) = database().await else { return };
    let _flag = MEMBER_NOTIFY_FLAG_LOCK.lock().await;
    set_member_notify_flag(true);
    sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
        .bind(staging_guild())
        .bind(MEMBER)
        .execute(db.pool())
        .await
        .unwrap();
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(state(db.pool().clone(), effect.clone()));
    for (raw, intent) in [
        (reminder_payload(), "intent-notify-alias-reminder"),
        (offer_payload(), "intent-notify-alias-offer"),
    ] {
        let mut payload: Value = serde_json::from_str(&raw).unwrap();
        payload["discord_id"] = json!(format!("0{MEMBER}"));
        let (status, _, refused) =
            answer(app.clone(), signed(&payload.to_string(), "old", intent)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(refused["error"]["code"], "malformed");
    }
    assert_eq!(effect.calls(), 0, "numeric aliases never reach Discord");
    let claims: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM internal_idempotency")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(claims, 0, "aliases never create separate audit identities");
    let (status, _, body) = answer(
        app,
        signed(
            &offer_payload(),
            "old",
            "intent-notify-canonical-suppressed",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"], json!({"outcome": "dm_unavailable"}));
    assert_eq!(effect.calls(), 0);
    set_member_notify_flag(false);
    db.close().await.unwrap();
}

#[tokio::test]
async fn notify_consent_lookup_releases_a_single_connection_before_finalizing() {
    let Some(db) = database().await else { return };
    let _flag = MEMBER_NOTIFY_FLAG_LOCK.lock().await;
    set_member_notify_flag(true);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(db.pool().connect_options().as_ref().clone())
        .await
        .unwrap();
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(state(pool.clone(), effect.clone()));
    for (step, expected) in [
        ("sent", "sent"),
        ("suppressed", "dm_unavailable"),
        ("lookup-error", "needs_reconciliation"),
    ] {
        if step == "suppressed" {
            sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
                .bind(staging_guild())
                .bind(MEMBER)
                .execute(db.pool())
                .await
                .unwrap();
        } else if step == "lookup-error" {
            sqlx::query("ALTER TABLE privacy_opt_outs RENAME TO notify_hidden_opt_outs")
                .execute(db.pool())
                .await
                .unwrap();
        }
        let intent = format!("intent-notify-one-connection-{step}");
        let (status, _, body) = tokio::time::timeout(
            Duration::from_secs(5),
            answer(app.clone(), signed(&reminder_payload(), "old", &intent)),
        )
        .await
        .expect("consent lookup must return its connection before finalization");
        if step == "lookup-error" {
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(body["error"]["code"], expected);
        } else {
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["result"]["outcome"], expected);
        }
        assert_eq!(
            effect.calls(),
            1,
            "suppression and lookup errors never send"
        );
    }
    let states: Vec<String> =
        sqlx::query_scalar("SELECT state FROM internal_idempotency ORDER BY intent_id")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(states, vec!["completed", "completed", "unknown"]);
    set_member_notify_flag(false);
    pool.close().await;
    db.close().await.unwrap();
}

#[tokio::test]
async fn notify_member_bucket_rate_limits_after_burst() {
    let Some(db) = database().await else { return };
    let _flag = MEMBER_NOTIFY_FLAG_LOCK.lock().await;
    set_member_notify_flag(true);
    let effect = Arc::new(MockEffect::new(MockOutcome::Success));
    let app = router(state(db.pool().clone(), effect.clone()));
    let raw = reminder_payload();
    // Both verbs and caller keys share one member budget, under each
    // caller's independent 20-burst key bucket.
    for n in 0..3 {
        let intent = format!("intent-notify-burst-{n}");
        let (payload, key) = if n == 1 {
            (offer_payload(), "new")
        } else {
            (raw.clone(), "old")
        };
        let (status, _, body) = answer(app.clone(), signed(&payload, key, &intent)).await;
        assert_eq!(status, StatusCode::OK, "{intent}");
        assert_eq!(body["result"], json!({"outcome": "sent"}));
    }
    assert_eq!(effect.calls(), 3);
    // A restarted receiver must not refill the member's burst.
    let app = router(state(db.pool().clone(), effect.clone()));
    // The 4th send is a 429 with no committed claim or delivery effect.
    let (status, _, refused) = answer(
        app.clone(),
        signed(&raw, "old", "intent-notify-burst-capped"),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused["error"]["code"], "rate_limited");
    assert_eq!(refused["error"]["retryable"], true);
    assert_eq!(effect.calls(), 3, "capped members never reach Discord");
    let claims: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM internal_idempotency \
         WHERE action IN ('reminder.send', 'place_offer.send')",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(claims, 3, "budget denial rolls back the tentative claim");
    // Another member keeps its own budget on the same caller key.
    let mut other: Value = serde_json::from_str(&raw).unwrap();
    other["discord_id"] = json!("222222222222222222");
    let (status, _, body) = answer(
        app.clone(),
        signed(&other.to_string(), "old", "intent-notify-burst-other"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"], json!({"outcome": "sent"}));
    assert_eq!(effect.calls(), 4);
    set_member_notify_flag(false);
    db.close().await.unwrap();
}

#[tokio::test]
async fn notify_rate_limited_releases_claim_and_answers_429() {
    let Some(db) = database().await else { return };
    let _flag = MEMBER_NOTIFY_FLAG_LOCK.lock().await;
    set_member_notify_flag(true);
    let notify = Arc::new(RateLimitedNotify::new());
    let app = router(Arc::new(ReceiverState::new(
        config(),
        db.pool().clone(),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        notify.clone(),
        Arc::new(MockEventRead::default()),
        Arc::new(MockEventMutate),
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    )));
    let raw = reminder_payload();
    let (status, headers, refused) =
        answer(app.clone(), signed(&raw, "old", "intent-notify-limited")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(refused["error"]["code"], "rate_limited");
    assert_eq!(refused["error"]["retryable"], true);
    assert_eq!(headers["retry-after"], "7");
    assert_eq!(notify.calls(), 1);
    // A 429 proves no effect: release retains the payload binding and audit
    // evidence in a retryable `not_sent` row instead of deleting the intent.
    let claims: Vec<String> = sqlx::query_scalar("SELECT state FROM internal_idempotency")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(claims, vec!["not_sent"], "release retains the bound intent");
    let released: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM internal_action_log \
         WHERE phase = 'released' AND evidence_code = 'proven_not_sent'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(released, 1, "release keeps its audit evidence");
    // The same key may retry: a fresh nonce re-takes the claim and runs the
    // effect again, with no bursting past the refusal.
    let (status, _, again) =
        answer(app.clone(), signed(&raw, "new", "intent-notify-limited")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(again["error"]["code"], "rate_limited");
    assert_eq!(notify.calls(), 2);
    set_member_notify_flag(false);
    db.close().await.unwrap();
}

#[tokio::test]
async fn notify_unknown_retains_claim_for_reconciliation() {
    let Some(db) = database().await else { return };
    let _flag = MEMBER_NOTIFY_FLAG_LOCK.lock().await;
    set_member_notify_flag(true);
    let effect = Arc::new(MockEffect::new(MockOutcome::Unknown));
    let app = router(state(db.pool().clone(), effect.clone()));
    let raw = offer_payload();
    let (status, _, body) = answer(app.clone(), signed(&raw, "old", "intent-notify-unknown")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "needs_reconciliation");
    assert_eq!(effect.calls(), 1);
    let state: String = sqlx::query_scalar(
        "SELECT state FROM internal_idempotency WHERE action = 'place_offer.send'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(state, "unknown");
    set_member_notify_flag(false);
    db.close().await.unwrap();
}

#[tokio::test]
async fn notify_effect_forwards_through_the_real_transport() {
    let Some(db) = database().await else { return };
    let _flag = MEMBER_NOTIFY_FLAG_LOCK.lock().await;
    set_member_notify_flag(true);
    // The production `NotifyEffect` impl over a transport that cannot reach
    // Discord: covers the trait forwarders with real send code, and an
    // unreachable Discord stays an unknown outcome, never a phantom send.
    let discord = ActionExecutor::with_proxy(
        "not-a-credential".to_owned(),
        Some("http://127.0.0.1:1".into()),
    )
    .unwrap();
    let notify: Arc<dyn NotifyEffect> = Arc::new(discord);
    let app = router(Arc::new(ReceiverState::new(
        config(),
        db.pool().clone(),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        Arc::new(MockEffect::new(MockOutcome::Success)),
        notify,
        Arc::new(MockEventRead::default()),
        Arc::new(MockEventMutate),
        Arc::new(MockModeration::default()),
        Arc::new(MockChannel::default()),
    )));
    let raw = reminder_payload();
    let (status, _, body) =
        answer(app.clone(), signed(&raw, "old", "intent-notify-transport")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "needs_reconciliation");
    let state: String =
        sqlx::query_scalar("SELECT state FROM internal_idempotency WHERE action = 'reminder.send'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(state, "unknown");
    set_member_notify_flag(false);
    db.close().await.unwrap();
}
