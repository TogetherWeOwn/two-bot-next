#![cfg(feature = "db")]
//! Protected acceptance: a role-grant failure never hides the committed level-up.
//!
//! BUILDER CONTRACT: this file is frozen. Build on the acceptance branch, keep
//! every test here verbatim, and make the RED tests pass by changing
//! production code only. Do not edit the fixtures or assertions: they are the
//! spec.
//!
//! Baseline behavior under test: the store award commits XP, then role grants
//! run and any grant error propagates with `?`, so the caller sees an opaque
//! leveling error and cannot tell that XP was committed or that the member
//! leveled up.
//!
//! Required behavior:
//! - When role application fails (mid-list Discord refusal or planner cap
//!   refusal), the pipeline still returns `Err`, but the error carries the
//!   committed award: `LevelingRuntimeError::committed_awards()` returns the
//!   award(s) with their computed XP totals and `leveled_up` flags. Add that
//!   inherent method with exactly this signature:
//!   `pub fn committed_awards(&self) -> &[two_bot_core::leveling::XpAward]`.
//!   It shadows the [`AwardEvidence`] fallback below, which returns empty on
//!   the baseline. For non-role errors it returns an empty slice.
//! - The error text keeps the role evidence: its `Display` or `Debug` form
//!   contains `role` (case-insensitive) plus, for a Discord refusal, the
//!   failed role id and `403`/`forbidden`; for a cap refusal, the word `cap`.
//! - The store still commits exactly one award row with the asserted totals.
//! - Fail-fast is preserved end to end: the refused role's successors are
//!   never attempted, and an over-cap ladder performs zero role writes
//!   (reads only).
//!
//! This file compiles on the baseline and fails on behavior (the fallback
//! reports no awards), not on a missing API.
//!
//! Database: the authorized disposable test database only
//! (`TWO_TEST_DATABASE_URL`); each test skips without it and CI always
//! supplies it. Discord stays on the mock double. Never production/staging.

#[allow(dead_code)]
mod common;

use common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::Arc;
use twilight_model::gateway::event::Event;
use two_bot_core::leveling::{LevelRoleReward, XpAward};
use two_bot_core::{leveling_store as store, MemStore, OnboardingGates, OnboardingMode};
use two_bot_discord::leveling_runtime::LevelingRuntimeError;
use two_bot_discord::{
    pipeline::MessageEligibility, ActionExecutor, LevelingRuntime, OrderedLevelingPipeline,
};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 1545644954272137297;
const MEMBER: u64 = 100000000000000001;
const CHANNEL: u64 = 300000000000000001;

// Each test-local adapter shadows a future inherent method: calls resolve to
// the trait on the baseline and to production once the builder lands it.
trait AwardEvidence {
    fn committed_awards(&self) -> &[XpAward];
}

impl AwardEvidence for LevelingRuntimeError {
    fn committed_awards(&self) -> &[XpAward] {
        &[]
    }
}

fn at(seconds: i64) -> String {
    two_bot_core::format_iso_millis(1_786_771_200_000 + seconds * 1000)
}

fn user(id: u64) -> Value {
    json!({"id":id.to_string(),"username":"username","global_name":"Global name","discriminator":"0","avatar":null,"bot":false})
}

fn runtime(pool: PgPool, mock: &MockRest) -> LevelingRuntime {
    LevelingRuntime::new(
        pool,
        Arc::new(
            ActionExecutor::with_proxy("mock-only-token".into(), Some(mock.origin())).unwrap(),
        ),
        GUILD,
        OnboardingGates {
            mode: OnboardingMode::Legacy,
            dry_run: false,
        },
    )
}

fn message(seconds: i64) -> Event {
    Event::MessageCreate(Box::new(serde_json::from_value(json!({
        "id":(4_000_000_000_000_000_000_u64 + seconds as u64).to_string(),"guild_id":GUILD.to_string(),"channel_id":CHANNEL.to_string(),"author":user(MEMBER),"content":"hello there","timestamp":at(seconds).replace('Z', "+00:00"),"edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0,"components":[]
    })).unwrap()))
}

fn test_database_url() -> Option<String> {
    std::env::var("TWO_TEST_DATABASE_URL").ok()
}

struct TestDb {
    fixture: TestDatabase,
    pool: PgPool,
}

impl TestDb {
    async fn new(url: &str) -> Self {
        let fixture = TestDatabase::create(url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated test database; no credential fallback");
        let pool = fixture.pool().clone();
        Self { fixture, pool }
    }

    async fn seed(&self, xp: i64, rewards: &[(u64, &str)]) {
        sqlx::query(
            "INSERT INTO member_levels (guild_id,member_id,xp,imported_xp,updated_at) VALUES ($1,$2,$3,$3,NOW())",
        )
        .bind(GUILD.to_string())
        .bind(MEMBER.to_string())
        .bind(xp)
        .execute(&self.pool)
        .await
        .unwrap();
        let ladder: Vec<LevelRoleReward> = rewards
            .iter()
            .map(|(level, role_id)| LevelRoleReward {
                level: *level,
                role_id: (*role_id).to_owned(),
            })
            .collect();
        store::replace_role_rewards(&self.pool, &GUILD.to_string(), &ladder)
            .await
            .unwrap();
    }

    async fn close(self) {
        self.fixture.close().await.expect("drop test database");
    }
}

async fn award_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM xp_awards")
        .fetch_one(pool)
        .await
        .unwrap()
}

// The client percent-encodes the audit-log reason on the wire, so decode
// before asserting on its text (same helper as the moderation suites).
fn decode_reason(header: &str) -> String {
    let mut bytes = Vec::new();
    let input = header.as_bytes();
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            bytes.push(u8::from_str_radix(&header[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            bytes.push(input[i]);
            i += 1;
        }
    }
    String::from_utf8(bytes).unwrap()
}

#[tokio::test]
async fn grant_refusal_preserves_committed_xp_and_level_up() {
    let Some(url) = test_database_url() else {
        eprintln!("SKIP leveling role xp preservation: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    // Three rungs, member one message from level 3: the award commits
    // 485 XP and levels up, then the second grant is refused midway.
    const FAILED: &str = "400000000000000002";
    let db = TestDb::new(&url).await;
    db.seed(
        470,
        &[
            (1, "400000000000000001"),
            (2, FAILED),
            (3, "400000000000000003"),
        ],
    )
    .await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"roles":[]})),
            ScriptedResponse::status(204),
            ScriptedResponse::status(403),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let pipeline =
        OrderedLevelingPipeline::new(MemStore::new(), Some(runtime(db.pool.clone(), &mock)));
    let error = pipeline
        .handle_at(&message(0), &at(0), MessageEligibility::default())
        .await
        .expect_err("a grant refusal must surface as an error");
    let awards = error.committed_awards();
    assert_eq!(
        awards.len(),
        1,
        "the committed award travels with the role error"
    );
    assert_eq!(
        awards[0],
        XpAward {
            awarded: 15,
            total_xp: 485,
            level: 3,
            previous_level: 2,
            leveled_up: true,
        }
    );
    let text = format!("{error}; {error:?}");
    assert!(
        text.to_ascii_lowercase().contains("role"),
        "the error identifies the failure as role application: {text}"
    );
    assert!(
        text.contains(FAILED),
        "the error names the refused role: {text}"
    );
    assert!(
        text.contains("403") || text.to_ascii_lowercase().contains("forbidden"),
        "the error preserves the refusal evidence: {text}"
    );
    let profile = store::profile(&db.pool, &GUILD.to_string(), &MEMBER.to_string())
        .await
        .unwrap();
    assert_eq!(
        (profile.xp, profile.message_xp, profile.level),
        (485, 15, 3)
    );
    assert_eq!(award_count(&db.pool).await, 1);

    // Fail-fast end to end: held-roles read, one applied PUT, one refused PUT,
    // and the third role never attempted. Both PUTs carry the grant reason.
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[1].method, "PUT");
    assert!(requests[1].path.ends_with("/roles/400000000000000001"));
    assert_eq!(requests[2].method, "PUT");
    assert!(requests[2].path.ends_with("/roles/400000000000000002"));
    for request in &requests[1..] {
        let reason = decode_reason(request.header("x-audit-log-reason").unwrap_or_default());
        assert!(
            reason.contains("reached level 3"),
            "attempted writes keep the grant audit reason: {reason}"
        );
    }
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn over_cap_ladder_preserves_committed_xp_and_reports_the_cap() {
    let Some(url) = test_database_url() else {
        eprintln!("SKIP leveling role xp preservation: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    // Eleven rungs, member one message from level 11: the award commits
    // 5785 XP and levels up, then the eleven-grant plan refuses the cap.
    let db = TestDb::new(&url).await;
    let rewards: Vec<(u64, String)> = (1..=11)
        .map(|level| (level, format!("4000000000000001{level:02}")))
        .collect();
    let ladder: Vec<(u64, &str)> = rewards
        .iter()
        .map(|(level, id)| (*level, id.as_str()))
        .collect();
    db.seed(5770, &ladder).await;
    let mock = MockRest::start(
        vec![ScriptedResponse::json(200, json!({"roles":[]}))],
        ScriptedResponse::status(500),
    )
    .await;
    let pipeline =
        OrderedLevelingPipeline::new(MemStore::new(), Some(runtime(db.pool.clone(), &mock)));
    let error = pipeline
        .handle_at(&message(0), &at(0), MessageEligibility::default())
        .await
        .expect_err("an over-cap ladder must surface as an error");
    let awards = error.committed_awards();
    assert_eq!(
        awards.len(),
        1,
        "the committed award travels with the cap refusal"
    );
    assert_eq!(
        awards[0],
        XpAward {
            awarded: 15,
            total_xp: 5785,
            level: 11,
            previous_level: 10,
            leveled_up: true,
        }
    );
    let text = format!("{error}; {error:?}");
    assert!(
        text.to_ascii_lowercase().contains("role"),
        "the error identifies the failure as role application: {text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("cap"),
        "the error preserves the cap refusal: {text}"
    );
    let profile = store::profile(&db.pool, &GUILD.to_string(), &MEMBER.to_string())
        .await
        .unwrap();
    assert_eq!(
        (profile.xp, profile.message_xp, profile.level),
        (5785, 15, 11)
    );
    assert_eq!(award_count(&db.pool).await, 1);

    // Zero role writes: only the held-roles read may run.
    let requests = mock.requests();
    assert!(
        requests.iter().all(|r| r.method == "GET"),
        "an over-cap ladder performs no PUT or DELETE: {}",
        requests.len()
    );
    mock.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn successful_grants_apply_and_report_the_level_up() {
    let Some(url) = test_database_url() else {
        eprintln!("SKIP leveling role xp preservation: TWO_TEST_DATABASE_URL is not set");
        return;
    };
    let db = TestDb::new(&url).await;
    db.seed(90, &[(1, "400000000000000001")]).await;
    let mock = MockRest::start(
        vec![
            ScriptedResponse::json(200, json!({"roles":[]})),
            ScriptedResponse::status(204),
        ],
        ScriptedResponse::status(500),
    )
    .await;
    let pipeline =
        OrderedLevelingPipeline::new(MemStore::new(), Some(runtime(db.pool.clone(), &mock)));
    let awards = pipeline
        .handle_at(&message(0), &at(0), MessageEligibility::default())
        .await
        .expect("a clean level-up reports its award");
    assert_eq!(
        awards,
        vec![XpAward {
            awarded: 15,
            total_xp: 105,
            level: 1,
            previous_level: 0,
            leveled_up: true,
        }]
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].method, "PUT");
    assert!(requests[1].path.ends_with("/roles/400000000000000001"));
    let reason = decode_reason(requests[1].header("x-audit-log-reason").unwrap_or_default());
    assert!(
        reason.contains("reached level 1"),
        "the applied write keeps the grant audit reason: {reason}"
    );
    mock.shutdown().await;
    db.close().await;
}

#[test]
fn legacy_error_view_carries_no_awards() {
    // Keeps [`AwardEvidence`] live after the inherent method shadows it: the
    // qualified call always resolves to this fallback, which stays empty.
    let error = LevelingRuntimeError::InvalidInteraction;
    assert!(<LevelingRuntimeError as AwardEvidence>::committed_awards(&error).is_empty());
    assert!(error.committed_awards().is_empty());
}
