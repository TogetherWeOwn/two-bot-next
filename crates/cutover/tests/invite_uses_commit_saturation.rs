//! Out-of-range invite `uses` saturate through the gateway dispatch commit.
//!
//! Builder-owned pin for the live gateway path: a poison counter must degrade
//! to `i32::MAX`, never roll back the dispatch transaction. The direct-bridge
//! contract lives in the store crate's protected acceptance file, which this
//! file never duplicates: every test here targets the commit flush.

use std::collections::HashSet;

use two_bot_core::database_tls::TlsPolicy;
use two_bot_core::gateway_funnel::{
    FunnelBatch, GatewayFunnelBuffer, InviteSnapshotWrite, SnapshotWrite,
};
use two_bot_core::gateway_session::{DispatchAction, GatewaySession};
use two_bot_core::{FunnelStore, InviteSnapshotStore, InviteState};
use two_bot_cutover::db::CutoverDb;
use two_bot_cutover::gateway_session::GatewaySessionStore;
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1545644954272137297";

fn guild_u64() -> u64 {
    GUILD.parse().unwrap()
}

fn invite(code: &str, uses: u64) -> InviteState {
    InviteState {
        code: code.to_owned(),
        uses,
        inviter_id: Some(123_456_789_012_345_678),
        channel_id: None,
        max_uses: 0,
        temporary: false,
    }
}

fn session(id: &str, sequence: u64) -> GatewaySession {
    GatewaySession {
        session_id: id.into(),
        sequence,
        resume_url: "ws://saturation".into(),
        updated_at_ms: 1_760_000_000_000,
    }
}

async fn database() -> Option<(TestDatabase, CutoverDb)> {
    let base_url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the commit saturation database suite"
            );
            eprintln!("SKIP commit saturation integration: TWO_TEST_DATABASE_URL not set");
            return None;
        }
    };
    let test_db = TestDatabase::create(&base_url, &sqlx::migrate!("./migrations"))
        .await
        .expect("disposable migrated database");
    let fixture_url = match base_url.rsplit_once('/') {
        Some((head, _)) => format!("{head}/{}", test_db.name()),
        None => base_url,
    };
    let db = two_bot_cutover::connect_with_tls(&fixture_url, 5, true, TlsPolicy::LocalOnly)
        .await
        .expect("connect to owned disposable database");
    Some((test_db, db))
}

async fn stored_uses(db: &CutoverDb, code: &str) -> i32 {
    sqlx::query_scalar("SELECT uses FROM invite_snapshots WHERE guild_id = $1 AND code = $2")
        .bind(GUILD)
        .bind(code)
        .fetch_one(db.pool())
        .await
        .expect("read back saturated counter")
}

/// A hand-built batch bypasses the buffer ingest clamp, so both commit binds
/// must saturate on their own: the `SnapshotWrite` flush and the staged
/// `InviteSnapshotWrite` flush.
#[tokio::test]
async fn commit_dispatch_saturates_hand_built_out_of_range_uses() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let store = GatewaySessionStore::new(db.pool().clone(), GUILD.to_owned(), 0);
    let batch = FunnelBatch {
        events: Vec::new(),
        activity: Vec::new(),
        bots: Vec::new(),
        snapshots: vec![SnapshotWrite::StoreAll(
            guild_u64(),
            vec![invite("commit-snapshot", u64::MAX)],
        )],
        invite_snapshots: vec![InviteSnapshotWrite {
            guild_id: guild_u64(),
            states: vec![invite("commit-staged", u64::MAX)],
            observed_at: "2026-10-21T00:00:00.000Z".to_owned(),
            replace_all: false,
        }],
    };
    assert_eq!(
        store
            .commit_dispatch(&session("saturation", 1), batch)
            .await
            .unwrap(),
        DispatchAction::Apply
    );
    assert_eq!(stored_uses(&db, "commit-snapshot").await, i32::MAX);
    assert_eq!(stored_uses(&db, "commit-staged").await, i32::MAX);

    db.close().await;
    test_db.close().await.unwrap();
}

/// The production route: buffer ingest clamps the queued states, staging
/// coalesces with the clamped tail, and the commit persists the clamp.
#[tokio::test]
async fn commit_dispatch_saturates_buffer_queued_out_of_range_uses() {
    let Some((test_db, db)) = database().await else {
        return;
    };
    let buffer = GatewayFunnelBuffer::default();
    buffer.store_all(guild_u64(), &[invite("buffered", u64::MAX)]);
    buffer.delete_missing(guild_u64(), &HashSet::from(["buffered".to_owned()]));
    buffer.stage_invite_snapshot(InviteSnapshotWrite {
        guild_id: guild_u64(),
        states: vec![invite("buffered", u64::MAX)],
        observed_at: "2026-10-21T00:00:00.000Z".to_owned(),
        replace_all: true,
    });
    let batch = buffer.take_batch();
    assert!(
        batch.snapshots.is_empty(),
        "the saturated full read must coalesce, not write twice"
    );

    let store = GatewaySessionStore::new(db.pool().clone(), GUILD.to_owned(), 0);
    assert_eq!(
        store
            .commit_dispatch(&session("saturation-buffered", 1), batch)
            .await
            .unwrap(),
        DispatchAction::Apply
    );
    assert_eq!(stored_uses(&db, "buffered").await, i32::MAX);

    db.close().await;
    test_db.close().await.unwrap();
}
