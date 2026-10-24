//! Protected acceptance extension: the Builder must not edit this file.
//! Direct store start-boundary and real runtime-lock contention regressions.
//! Only Discord is doubled; every store operation uses disposable Postgres.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use std::time::Duration;
use time::OffsetDateTime;
use two_bot_core::{
    lfg::{self, LfgPost, LfgStatus, SignupOutcome},
    lfg_store as store,
};
use two_bot_discord::{lfg_interactions::LfgInteractions, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "2222";
const CHANNEL: &str = "4444";
const BOT: u64 = 1111;
const OWNER: &str = "3333";
const RETAINED: &str = "3334";
const NEW_MEMBER: &str = "3335";
const OLD: &str = "2000-01-01T00:00:00.000Z";
const START: &str = "2100-01-01T00:00:00.000Z";
// Shorter than TestDatabase's five-second statement timeout: a blocking lock
// must fail this test even though the holder is deliberately kept alive.
const SWEEP_DEADLINE: Duration = Duration::from_secs(3);

type Membership = (String, String, OffsetDateTime);

fn post(id: &str, message: &str, created_at: &str) -> LfgPost {
    LfgPost {
        id: id.into(),
        guild_id: GUILD.into(),
        channel_id: CHANNEL.into(),
        message_id: Some(message.into()),
        title: "Offline raid".into(),
        starts_at: START.into(),
        status: LfgStatus::Open,
        created_by: OWNER.into(),
        created_at: created_at.into(),
        closed_at: None,
    }
}

struct Fixture {
    db: TestDatabase,
    service: LfgInteractions,
    executor: ActionExecutor,
    rest: MockRest,
}

impl Fixture {
    async fn new() -> Self {
        let url =
            std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create migrated disposable fixture");
        let rest =
            MockRest::with_responder(|_| ScriptedResponse::json(200, json!({"id": "9000"}))).await;
        let executor =
            ActionExecutor::with_proxy("offline-lfg-lock-test-token".into(), Some(rest.origin()))
                .expect("loopback executor");
        let service = LfgInteractions::new(db.pool().clone());
        Self {
            db,
            service,
            executor,
            rest,
        }
    }

    async fn seed(&self, row: &LfgPost) -> Result<SignupOutcome, sqlx::Error> {
        let roles = lfg::spec_roles(
            &row.id,
            &lfg::parse_role_spec("tank:Tank:2,healer:Healer:2").expect("two unfilled roles"),
        );
        store::put_lfg(self.db.pool(), row, &roles, true)
            .await
            .expect("seed open post");
        store::signup_lfg(self.db.pool(), GUILD, &row.id, "tank", RETAINED, OLD).await
    }

    async fn finish(self) -> Vec<RestRequest> {
        let requests = self.rest.requests();
        self.rest.shutdown().await;
        drop(self.service);
        drop(self.executor);
        self.db.close().await.expect("fixture teardown");
        requests
    }
}

async fn memberships(pool: &sqlx::PgPool, id: &str) -> Result<Vec<Membership>, sqlx::Error> {
    sqlx::query_as(
        "SELECT user_id, role_key, joined_at FROM lfg_signups WHERE lfg_id = $1 ORDER BY user_id",
    )
    .bind(id)
    .fetch_all(pool)
    .await
}

async fn snapshot(pool: &sqlx::PgPool, id: &str) -> Result<Value, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'post', (SELECT to_jsonb(p) FROM lfg_posts p WHERE p.id = $1),
            'roles', (SELECT coalesce(jsonb_agg(to_jsonb(r) ORDER BY r.role_key), '[]'::jsonb)
                      FROM lfg_roles r WHERE r.lfg_id = $1),
            'signups', (SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY s.user_id), '[]'::jsonb)
                        FROM lfg_signups s WHERE s.lfg_id = $1),
            'audits', (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY to_jsonb(a)::text), '[]'::jsonb)
                       FROM announcements_audit_log a WHERE a.target_key = $1))",
    )
    .bind(id)
    .fetch_one(pool)
    .await
}

#[tokio::test]
async fn direct_store_signup_joins_before_start_but_refuses_insert_and_move_at_or_after_start() {
    // Catches a status-only SELECT which forgets the under-lock starts_at fence.
    // Calling execute() would hide that bug behind the interaction's early check.
    let f = Fixture::new().await;
    let reader = f.db.independent_pool().await.expect("independent reader");
    let mut observations = Vec::new();
    for (id, when, expected) in [
        (
            "lfg-store-before-start",
            "2099-12-31T23:59:59.000Z",
            SignupOutcome::Joined,
        ),
        ("lfg-store-at-start", START, SignupOutcome::Closed),
        (
            "lfg-store-after-start",
            "2100-01-01T00:00:01.000Z",
            SignupOutcome::Closed,
        ),
    ] {
        let seeded = f.seed(&post(id, "5001", OLD)).await;
        let before = snapshot(&reader, id).await;
        let joined = store::signup_lfg(f.db.pool(), GUILD, id, "healer", NEW_MEMBER, when).await;
        let after_join = snapshot(&reader, id).await;
        let rows = memberships(&reader, id).await;
        let moved = if expected == SignupOutcome::Closed {
            Some(store::signup_lfg(f.db.pool(), GUILD, id, "healer", RETAINED, when).await)
        } else {
            None
        };
        let after_move = snapshot(&reader, id).await;
        observations.push((
            id, expected, seeded, before, joined, after_join, rows, moved, after_move,
        ));
    }
    reader.close().await;
    let requests = f.finish().await;

    let retained_at = OffsetDateTime::from_unix_timestamp(946_684_800).expect("2000 UTC");
    let before_start = OffsetDateTime::from_unix_timestamp(4_102_444_799).expect("start minus one");
    for (id, expected, seeded, before, joined, after_join, rows, moved, after_move) in observations
    {
        let before = before.expect("snapshot before direct signup");
        let after_join = after_join.expect("snapshot after direct signup");
        let after_move = after_move.expect("snapshot after direct move");
        let rows = rows.expect("real membership rows and timestamps");
        assert_eq!(
            seeded.expect("retained membership control"),
            SignupOutcome::Joined
        );
        assert_eq!(joined.expect("direct signup result"), expected, "{id}");
        let mut expected_rows = vec![(RETAINED.into(), "tank".into(), retained_at)];
        if expected == SignupOutcome::Joined {
            expected_rows.push((NEW_MEMBER.into(), "healer".into(), before_start));
        } else {
            assert_eq!(
                after_join, before,
                "{id}: refusal must not insert or mutate"
            );
            assert_eq!(
                moved
                    .expect("existing member attempted move")
                    .expect("direct move result"),
                SignupOutcome::Closed,
                "{id}: existing membership cannot move at/after start"
            );
            assert_eq!(
                after_move, before,
                "{id}: refusal retains original timestamp and role"
            );
        }
        assert_eq!(
            rows, expected_rows,
            "{id}: persisted rows and exact joined_at"
        );
    }
    assert!(
        requests.is_empty(),
        "direct store calls must never use Discord"
    );
}

#[tokio::test]
async fn sweep_skips_a_held_runtime_lock_continues_then_processes_it_after_release() {
    // Catches blocking pg_advisory_xact_lock, the wrong lfg: namespace, and
    // early abort when the first eligible row is busy. No sleep synchronizes it.
    let f = Fixture::new().await;
    let held = post("lfg-runtime-held-first", "5101", OLD);
    let unlocked = post(
        "lfg-runtime-unlocked-second",
        "5102",
        "2000-01-02T00:00:00.000Z",
    );
    let held_seed = f.seed(&held).await;
    let unlocked_seed = f.seed(&unlocked).await;
    let held_before = snapshot(f.db.pool(), &held.id).await;
    let unlocked_before = snapshot(f.db.pool(), &unlocked.id).await;
    let peer =
        f.db.independent_pool()
            .await
            .expect("separate lock-holder pool");
    let mut holder = peer.begin().await.expect("separate holder transaction");
    // Awaiting this query is the synchronization: the exact runtime key is held
    // before the sweep begins on a different connection, not just scheduled.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("lfg-runtime:{GUILD}:{}", held.id))
        .execute(&mut *holder)
        .await
        .expect("hold exact runtime key");
    let now = OffsetDateTime::from_unix_timestamp(4_102_445_700).expect("start plus grace");
    let first = tokio::time::timeout(
        SWEEP_DEADLINE,
        f.service.sweep_lifecycle(&f.executor, GUILD, BOT, now, 20),
    )
    .await;
    let held_during = snapshot(f.db.pool(), &held.id).await;
    let unlocked_during = snapshot(f.db.pool(), &unlocked.id).await;
    let requests_during = f.rest.requests();

    // Cleanup precedes every outcome assertion, including the timeout path.
    // Drop the cancelled sweep future, release its peer lock, and close the
    // independent pool before retrying; never strand a holder after a panic.
    let released = holder.rollback().await;
    peer.close().await;
    let second = tokio::time::timeout(
        SWEEP_DEADLINE,
        f.service.sweep_lifecycle(&f.executor, GUILD, BOT, now, 20),
    )
    .await;
    let held_after = snapshot(f.db.pool(), &held.id).await;
    let unlocked_after = snapshot(f.db.pool(), &unlocked.id).await;
    let requests = f.finish().await;

    released.expect("release runtime lock");
    let held_before = held_before.expect("held row before sweep");
    let unlocked_before = unlocked_before.expect("unlocked row before sweep");
    let held_during = held_during.expect("held row while lock held");
    let unlocked_during = unlocked_during.expect("unlocked row while lock held");
    let held_after = held_after.expect("held row after lock release");
    let unlocked_after = unlocked_after.expect("unlocked row after lock release");
    assert_eq!(
        held_seed.expect("held membership control"),
        SignupOutcome::Joined
    );
    assert_eq!(
        unlocked_seed.expect("unlocked membership control"),
        SignupOutcome::Joined
    );
    assert_eq!(
        first
            .expect("sweep must return promptly while runtime lock is held")
            .expect("first sweep"),
        1,
        "only the later unlocked row is processed while the first row is held"
    );
    assert_eq!(
        held_during, held_before,
        "held row has zero persisted mutations"
    );
    assert_eq!(unlocked_before["post"]["status"], "open");
    assert_eq!(
        unlocked_during["post"]["status"], "closed",
        "continue beyond held row"
    );
    assert_eq!(
        requests_during.len(),
        1,
        "only the unlocked row reaches HTTP"
    );
    assert_eq!(requests_during[0].method, "PATCH");
    assert_eq!(
        requests_during[0].path,
        "/api/v10/channels/4444/messages/5102"
    );
    assert_eq!(
        second
            .expect("retry after release must return promptly")
            .expect("second sweep"),
        1,
        "released row remains eligible for the next sweep"
    );
    assert_eq!(held_after["post"]["status"], "closed");
    assert!(!held_after["post"]["closed_at"].is_null());
    assert_eq!(held_after["roles"], held_before["roles"]);
    assert_eq!(held_after["signups"], held_before["signups"]);
    assert_eq!(
        unlocked_after, unlocked_during,
        "already processed row is not processed twice"
    );
    assert_eq!(
        requests.len(),
        2,
        "one edit per eligible row, no repost or history query"
    );
    assert_eq!(requests[1].method, "PATCH");
    assert_eq!(requests[1].path, "/api/v10/channels/4444/messages/5101");
}
