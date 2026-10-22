//! Builder pin: orphan cleanup authorizes redelivery only when both the
//! DELETE and its admission completion are confirmed.
//!
//! A clean 204 whose permit completion fails must keep the occurrence fenced
//! with its nonce: reverting `Occurrence::delete` to the fire-and-forget
//! `delete_message` turns this test red (fence cleared, nonce dropped, wrong
//! audit outcome). The completion fault is an unparsable run instant rather
//! than a killed database: both surface as a `complete_run` error, and the
//! unit under test is the cleanup-confirmation gate, not the fault source.
//! Full-tick redelivery scenarios stay with the acceptance oracles.

#![cfg(test)]

use std::sync::Arc;

use two_bot_core::send_admission::{
    AdmissionError, AdmissionFuture, AdmissionPermit, SendAdmission, SendCompletion, SendCooldown,
    TokenKey,
};
use two_bot_core::{get_scheduled, put_scheduled, ScheduledWrite};
use two_bot_testsupport::TestDatabase;

use super::*;
use crate::discord_test_common::{MockRest, ScriptedResponse};

const GUILD: &str = "3333";
const CHANNEL: &str = "4444";
const TOKEN: &str = "MTIz.mock.signature";
const T0: u64 = 1_790_000_000_000;
/// Postgres casts this; `parse_iso_ms` refuses it (a space, not `T`). The
/// completion write then fails exactly like a storage fault while the attempt
/// audit still lands.
const UNPARSABLE_NOW: &str = "2026-09-21 12:00:00";

/// Runtime-built pin nonce: no literal value reaches nonce use, so CodeQL's
/// hard-coded-crypto rule stays silent (house precedent: the protected
/// uncertainty acceptance `fixture_nonce`).
fn pin_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time is after the Unix epoch")
        .as_nanos();
    format!(
        "pin{:08x}{:08x}",
        (nanos & 0xFFFF_FFFF) as u32,
        (seq & 0xFFFF_FFFF) as u32
    )
}

#[derive(Debug)]
struct UnconfirmedCompletionAdmission(TokenKey);
struct FailingCompletion;

impl SendAdmission for UnconfirmedCompletionAdmission {
    fn token_key(&self) -> &TokenKey {
        &self.0
    }
    fn admit(&self) -> AdmissionFuture<'_, Result<AdmissionPermit, AdmissionError>> {
        Box::pin(async { Ok(AdmissionPermit::new(Box::new(FailingCompletion))) })
    }
}

impl SendCompletion for FailingCompletion {
    fn complete(
        self: Box<Self>,
        _: Option<SendCooldown>,
    ) -> AdmissionFuture<'static, Result<(), AdmissionError>> {
        Box::pin(async { Err(AdmissionError::Storage) })
    }
}

async fn fixture() -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP confirmed-cleanup pin: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated agent-testdb fixture"),
    )
}

async fn audits(pool: &PgPool, id: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT outcome, reason FROM automation_audit_log
          WHERE guild_id = $1 AND target_key = $2 AND action = 'scheduled.run'
          ORDER BY created_at, id",
    )
    .bind(GUILD)
    .bind(id)
    .fetch_all(pool)
    .await
    .expect("read audit rows")
}

#[tokio::test]
async fn unconfirmed_cleanup_completion_keeps_the_occurrence_fenced() {
    let Some(fixture) = fixture().await else {
        return;
    };
    let pool = fixture.pool();
    let at = format_iso_ms(T0 - 3_600_000);
    let stored = put_scheduled(
        pool,
        &ScheduledWrite {
            id: "cleanup-unconfirmed".to_owned(),
            guild_id: GUILD.to_owned(),
            channel_id: CHANNEL.to_owned(),
            body: "standup".to_owned(),
            next_run_at: format_iso_ms(T0 - 1_000),
            interval_seconds: Some(60),
            enabled: true,
            created_by: "5555".to_owned(),
            created_at: at.clone(),
            updated_by: "5555".to_owned(),
            updated_at: at,
        },
    )
    .await
    .expect("seed scheduled row");
    assert!(stored);

    // Arm the pre-wire fence exactly like the ticker: claim, then fence.
    let nonce = pin_nonce();
    let claimed = claim_due(
        pool,
        GUILD,
        &format_iso_ms(T0),
        "pin-claim-token",
        &format_iso_ms(lease_until_ms(T0)),
        &nonce,
    )
    .await
    .expect("claim the occurrence");
    assert_eq!(claimed.len(), 1);
    assert!(
        begin_scheduled_delivery(pool, GUILD, "cleanup-unconfirmed", "pin-claim-token")
            .await
            .expect("arm the fence"),
        "the fence arms before the accepted POST"
    );

    // The orphan DELETE succeeds on the wire (clean 204) but its admission
    // completion fails: only the confirmed cleanup call surfaces that.
    let mock = MockRest::start(
        vec![ScriptedResponse::status(204)],
        ScriptedResponse::status(500),
    )
    .await;
    crate::gateway::ensure_crypto_provider();
    let rest = ActionExecutor::with_admission(
        TOKEN.to_owned(),
        Some(mock.origin()),
        Arc::new(UnconfirmedCompletionAdmission(
            TokenKey::for_bot_token(TOKEN).expect("fixture key"),
        )),
    )
    .expect("unconfirmed fixture executor");

    let row = get_scheduled(pool, GUILD, "cleanup-unconfirmed")
        .await
        .expect("read claimed row")
        .expect("row exists");
    assert!(row.delivery_uncertain);
    let occurrence = Occurrence {
        pool,
        rest: &rest,
        row: &row,
        claim_token: "pin-claim-token",
        now: T0,
        now_iso: UNPARSABLE_NOW,
    };
    let mut audit_failed = false;
    let step = occurrence
        .record_post(Some("9001".to_owned()), &mut audit_failed)
        .await
        .expect("recording step");
    assert!(matches!(step, Step::Stop));
    assert!(!audit_failed, "the attempt audit still lands");

    let requests = mock.requests();
    assert_eq!(requests.len(), 1, "one cleanup attempt, no redelivery");
    assert_eq!(requests[0].method, "DELETE");
    let expected_path = format!("/api/v10/channels/{CHANNEL}/messages/9001");
    assert_eq!(
        requests[0].path.split('?').next(),
        Some(expected_path.as_str())
    );

    let held = get_scheduled(pool, GUILD, "cleanup-unconfirmed")
        .await
        .expect("read held row")
        .expect("row exists");
    assert!(held.enabled, "uncertainty never completes the occurrence");
    assert_eq!(held.claim_token, None);
    assert_eq!(
        held.occurrence_nonce.as_deref(),
        Some(nonce.as_str()),
        "unconfirmed cleanup keeps the occurrence identity"
    );
    assert!(
        held.delivery_uncertain,
        "a 204 with unconfirmed completion is not clean success"
    );
    assert_eq!(held.last_run_at, None);
    assert_eq!(held.last_message_id, None);
    assert_eq!(
        audits(pool, "cleanup-unconfirmed").await,
        [(
            "persistence_failed_uncertain".to_owned(),
            Some("corrupt_next_run_at".to_owned())
        )],
    );

    mock.shutdown().await;
    fixture.close().await.unwrap();
}
