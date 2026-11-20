//! Kill/restart acceptance at the real dispatch transaction seam. Fixtures only.

use std::collections::BTreeSet;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use two_bot_core::database_tls::TlsPolicy;
use two_bot_core::gateway_funnel::FunnelBatch;
use two_bot_core::gateway_session::{DispatchAction, GatewaySession};
use two_bot_core::{idempotency_key, EventType, FunnelEvent};
use two_bot_cutover::gateway_session::GatewaySessionStore;
use two_bot_testsupport::pipeline_integrity::verify_durable_events;
use two_bot_testsupport::{guard_database_url, TestDatabase};

const CHILD_DATABASE: &str = "TWO_PIPELINE_CRASH_DATABASE";
const BARRIER: i64 = 28_890_001;

fn event(member: u64) -> FunnelEvent {
    FunnelEvent {
        guild_id: 2222,
        member_id: Some(member),
        event_type: EventType::MemberJoin,
        occurred_at: "2026-10-20T12:00:00.000Z".into(),
        source: "unknown".into(),
        metadata: None,
        dedupe_token: None,
    }
}

fn checkpoint(sequence: u64) -> GatewaySession {
    GatewaySession {
        session_id: "pipeline-crash".into(),
        sequence,
        resume_url: "ws://synthetic".into(),
        updated_at_ms: 1_760_000_000_000 + sequence as i64,
    }
}

fn batch(members: &[u64]) -> FunnelBatch {
    FunnelBatch {
        events: members.iter().copied().map(event).collect(),
        ..Default::default()
    }
}

fn expected(members: &[u64]) -> BTreeSet<String> {
    members
        .iter()
        .copied()
        .map(|member| idempotency_key(&event(member)))
        .collect()
}

struct Writer(Child);

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn kill_mid_batch_then_replay_and_drain() {
    // Invoke the same test binary as a writer, rather than add a production hook
    // or pretend cancelling a Rust future is a process crash.
    if let Ok(raw) = std::env::var(CHILD_DATABASE) {
        guard_database_url(&raw).unwrap();
        let db = two_bot_cutover::connect_with_tls(&raw, 1, true, TlsPolicy::LocalOnly)
            .await
            .expect("child connects only to owned fixture");
        let store = GatewaySessionStore::new(db.pool().clone(), "2222".into(), 0);
        store
            .commit_dispatch(&checkpoint(2), batch(&[201, 202, 203]))
            .await
            .unwrap();
        panic!("parent must kill writer while second event insert is blocked");
    }
    let raw =
        std::env::var("TWO_TEST_DATABASE_URL").expect("explicit disposable test database required");
    let db = TestDatabase::create(&raw, &sqlx::migrate!("./migrations"))
        .await
        .unwrap();
    two_bot_store::migrate(db.pool()).await.unwrap();
    let store = GatewaySessionStore::new(db.pool().clone(), "2222".into(), 0);
    assert_eq!(
        store
            .commit_dispatch(&checkpoint(1), batch(&[100]))
            .await
            .unwrap(),
        DispatchAction::Apply
    );
    verify_durable_events(db.pool(), &expected(&[100]))
        .await
        .unwrap();

    // The second INSERT waits only after the first row and its journal have
    // been written in the same transaction. An AFTER trigger makes the crash
    // point deterministic without touching candidate code or relying on sleep.
    sqlx::raw_sql(
        "CREATE FUNCTION pipeline_crash_barrier() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN PERFORM pg_advisory_xact_lock(28890001); RETURN NEW; END $$;
         CREATE TRIGGER pipeline_crash_barrier AFTER INSERT ON events
         FOR EACH ROW WHEN (NEW.member_id = '202') EXECUTE FUNCTION pipeline_crash_barrier();",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let mut lock = db.pool().acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(BARRIER)
        .execute(&mut *lock)
        .await
        .unwrap();
    let (prefix, _) = raw.rsplit_once('/').unwrap();
    let fixture_url = format!("{prefix}/{}", db.name());
    let mut writer = Writer(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "kill_mid_batch_then_replay_and_drain",
                "--nocapture",
            ])
            .env(CHILD_DATABASE, fixture_url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn bounded fixture writer"),
    );
    let pid = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let pid: Option<i32> = sqlx::query_scalar(
                "SELECT pid FROM pg_stat_activity WHERE datname = current_database()
                 AND wait_event_type = 'Lock' AND wait_event = 'advisory'
                 AND pid <> pg_backend_pid()",
            )
            .fetch_optional(db.pool())
            .await
            .unwrap();
            if let Some(pid) = pid {
                break pid;
            }
            assert!(
                writer.0.try_wait().unwrap().is_none(),
                "writer exited before crash barrier"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("writer reached second INSERT crash barrier");
    verify_durable_events(db.pool(), &expected(&[100]))
        .await
        .unwrap();
    assert_eq!(store.load().await.unwrap().unwrap().sequence, 1);
    writer
        .0
        .kill()
        .expect("kill the writer process, not the PostgreSQL server");
    assert!(!writer.0.wait().unwrap().success());
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(BARRIER)
        .execute(&mut *lock)
        .await
        .unwrap();
    drop(lock);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid = $1)")
                    .bind(pid)
                    .fetch_one(db.pool())
                    .await
                    .unwrap();
            if !exists {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("killed writer backend exited and rolled back");
    verify_durable_events(db.pool(), &expected(&[100]))
        .await
        .unwrap();
    assert_eq!(store.load().await.unwrap().unwrap().sequence, 1);
    sqlx::query("DROP TRIGGER pipeline_crash_barrier ON events")
        .execute(db.pool())
        .await
        .unwrap();

    // A fresh pool/store simulates restart. Replay the entire unacknowledged
    // dispatch, drain it, and prove every expected row plus journal is durable.
    let restarted_pool = db.independent_pool().await.unwrap();
    let restarted = GatewaySessionStore::new(restarted_pool.clone(), "2222".into(), 0);
    assert_eq!(
        restarted
            .commit_dispatch(&checkpoint(2), batch(&[201, 202, 203]))
            .await
            .unwrap(),
        DispatchAction::Apply
    );
    assert_eq!(restarted.load().await.unwrap().unwrap().sequence, 2);
    verify_durable_events(&restarted_pool, &expected(&[100, 201, 202, 203]))
        .await
        .unwrap();
    assert_eq!(
        restarted
            .commit_dispatch(&checkpoint(2), batch(&[201, 202, 203]))
            .await
            .unwrap(),
        DispatchAction::Duplicate
    );
    verify_durable_events(&restarted_pool, &expected(&[100, 201, 202, 203]))
        .await
        .unwrap();

    // Known-invalid durability candidates: losing a row can still preserve
    // perfect journal coverage for all survivors; losing or duplicating a
    // journal entry must also be rejected by the same executable oracle.
    sqlx::query("DELETE FROM rollback_journal WHERE table_name = 'events' AND row_identity = (SELECT id::text FROM events WHERE member_id = '203')")
        .execute(&restarted_pool).await.unwrap();
    assert!(
        verify_durable_events(&restarted_pool, &expected(&[100, 201, 202, 203]))
            .await
            .is_err()
    );
    sqlx::query("DELETE FROM events WHERE member_id = '203'")
        .execute(&restarted_pool)
        .await
        .unwrap();
    assert!(
        verify_durable_events(&restarted_pool, &expected(&[100, 201, 202, 203]))
            .await
            .is_err()
    );
    // Repair only the injected corruption with the real persistence seam.
    assert_eq!(
        restarted
            .commit_dispatch(&checkpoint(3), batch(&[203]))
            .await
            .unwrap(),
        DispatchAction::Apply
    );
    verify_durable_events(&restarted_pool, &expected(&[100, 201, 202, 203]))
        .await
        .unwrap();
    sqlx::query("INSERT INTO rollback_journal (table_name, row_identity, op, pre_image) SELECT table_name, row_identity, op, pre_image FROM rollback_journal WHERE table_name = 'events' LIMIT 1")
        .execute(&restarted_pool).await.unwrap();
    assert!(
        verify_durable_events(&restarted_pool, &expected(&[100, 201, 202, 203]))
            .await
            .is_err()
    );
    db.close().await.unwrap();
}
