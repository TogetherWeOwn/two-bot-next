//! A held live-display PATCH must serialize votes and close on the same poll.
//! Only Discord HTTP is doubled; locks, commits and counts use real Postgres.
#![cfg(feature = "db")]

pub mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::{MockRest, ResponseGate, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use twilight_model::application::interaction::Interaction;
use two_bot_core::{ClassifierConfig, InteractionRouter, RouterGates};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "1660102030405060701";
const CHANNEL: &str = "1660102030405060702";
const AUTHOR: &str = "1660102030405060721";
const VOTER: &str = "1660102030405060722";
const WAIT: Duration = Duration::from_secs(10);

fn runtime(pool: &PgPool, mock: &MockRest) -> InteractionRuntime {
    InteractionRuntime::with_router(
        InteractionRouter::new(RouterGates {
            configured_guild: Some(GUILD.parse().unwrap()),
            announcements: true,
            scorecard: false,
            automations: false,
            moderation: false,
            voice: false,
            voice_assistant: false,
            tickets: false,
            self_roles: false,
            onboarding_picker: false,
            session_picker: false,
        }),
        pool.clone(),
        ActionExecutor::with_proxy("synthetic-poll-token".into(), Some(mock.origin())).unwrap(),
        0,
        ClassifierConfig::default(),
    )
}

fn packet(id: u64, user: &str, kind: u8, data: Value) -> Interaction {
    serde_json::from_value(json!({
        "application_id":"1111", "authorizing_integration_owners":{"0":GUILD},
        "id":id.to_string(), "token":"synthetic-poll-callback", "type":kind, "version":1,
        "guild_id":GUILD, "channel":{"id":CHANNEL,"type":0},
        "member":{"permissions":"0","roles":[],"deaf":false,"mute":false,"flags":0,
            "user":{"id":user,"username":"fixture","discriminator":"0","bot":false}},
        "data":data
    }))
    .expect("synthetic poll interaction")
}

fn vote(id: &str) -> Interaction {
    packet(
        9001,
        VOTER,
        3,
        json!({"custom_id":format!("poll:{id}:0"),"component_type":2}),
    )
}

async fn vote_count(pool: &PgPool, id: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM poll_votes WHERE guild_id=$1 AND poll_id=$2")
        .bind(GUILD)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

// Observe a real lock wait instead of inferring ordering from sleep/return order.
// Removing the parent lock lets the contender finish or reach its HTTP boundary
// before this witness, and the recorded pre-release vote/state assertions fail.
async fn parent_lock_waits<T>(pool: &PgPool, contender: &JoinHandle<T>) -> bool {
    timeout(WAIT, async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity a JOIN pg_locks l ON l.pid=a.pid \
                 WHERE a.datname=current_database() AND a.pid<>pg_backend_pid() \
                   AND a.wait_event_type='Lock' AND cardinality(pg_blocking_pids(a.pid))>0 \
                   AND l.relation='polls'::regclass)",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting {
                return true;
            }
            if contender.is_finished() {
                return false;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or(false)
}

#[tokio::test]
async fn refresh_vote_and_close_serialize_without_reopening_or_losing_committed_votes() {
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure disposable database poll lock acceptance"
            );
            eprintln!("SKIP poll lock acceptance: TWO_TEST_DATABASE_URL not set");
            return;
        }
        Err(error) => panic!("invalid test bootstrap configuration: {error}"),
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database");
    let refresh_gate = Arc::new(ResponseGate::default());
    let vote_gate = Arc::new(ResponseGate::default());
    let first = refresh_gate.clone();
    let second = vote_gate.clone();
    let patches = AtomicUsize::new(0);
    let mock = MockRest::with_gated_responder(move |request| {
        let response = if request.path.contains("/interactions/") {
            ScriptedResponse::status(204)
        } else {
            ScriptedResponse::json(200, json!({"id":"99"}))
        };
        let gate = if request.method == "PATCH" && request.path.contains("/channels/") {
            match patches.fetch_add(1, Ordering::SeqCst) {
                0 => Some(first.clone()),
                1 => Some(second.clone()),
                _ => None,
            }
        } else {
            None
        };
        (response, gate)
    })
    .await;
    let create = packet(
        9000,
        AUTHOR,
        2,
        json!({
            "id":"4444","name":"poll","type":1,"options":[{"name":"create","type":1,"options":[
                {"name":"question","type":3,"value":"Lock ordered poll"},
                {"name":"option1","type":3,"value":"one"},
                {"name":"option2","type":3,"value":"two"},
                {"name":"duration-minutes","type":4,"value":10}
            ]}]
        }),
    );
    assert!(runtime(db.pool(), &mock).handle(&create).await.unwrap());
    let id: String = sqlx::query_scalar("SELECT id FROM polls WHERE guild_id=$1")
        .bind(GUILD)
        .fetch_one(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE polls SET display_dirty=TRUE WHERE guild_id=$1 AND id=$2")
        .bind(GUILD)
        .bind(&id)
        .execute(db.pool())
        .await
        .unwrap();

    let refresh = runtime(db.pool(), &mock);
    let refreshing = tokio::spawn(async move { refresh.tick_polls().await });
    timeout(WAIT, refresh_gate.wait_for_request())
        .await
        .expect("dirty refresh PATCH");
    let peer = db.independent_pool().await.unwrap();
    let voting = runtime(&peer, &mock);
    let vote_packet = vote(&id);
    let voter = tokio::spawn(async move { voting.handle(&vote_packet).await });
    let vote_waited = parent_lock_waits(db.pool(), &voter).await;
    let votes_before_refresh_release = vote_count(db.pool(), &id).await;
    refresh_gate.release();
    timeout(WAIT, vote_gate.wait_for_request())
        .await
        .expect("vote display PATCH");
    // This count must be committed before close is even submitted. A close
    // result that omits it is not explained by an ambiguous race winner.
    let committed_before_close = vote_count(db.pool(), &id).await;
    let closer_pool = db.independent_pool().await.unwrap();
    let closer = runtime(&closer_pool, &mock);
    let close = packet(
        9002,
        AUTHOR,
        2,
        json!({
            "id":"4444","name":"poll","type":1,"options":[{"name":"close","type":1,
                "options":[{"name":"id","type":3,"value":id}]}]
        }),
    );
    let closing = tokio::spawn(async move { closer.handle(&close).await });
    let close_waited = parent_lock_waits(db.pool(), &closing).await;
    let status_before_vote_release: String =
        sqlx::query_scalar("SELECT status FROM polls WHERE guild_id=$1 AND id=$2")
            .bind(GUILD)
            .bind(&id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    vote_gate.release();
    timeout(WAIT, refreshing)
        .await
        .expect("refresh completes")
        .unwrap()
        .unwrap();
    assert!(timeout(WAIT, voter)
        .await
        .expect("vote completes")
        .unwrap()
        .unwrap());
    assert!(timeout(WAIT, closing)
        .await
        .expect("close completes")
        .unwrap()
        .unwrap());
    runtime(db.pool(), &mock).tick_polls().await.unwrap();
    runtime(db.pool(), &mock).handle(&vote(&id)).await.unwrap();
    runtime(db.pool(), &mock).tick_polls().await.unwrap();
    let row = sqlx::query(
        "SELECT status,counts_json::text AS counts,winner_index::int AS winner, \
         results_posted_at IS NOT NULL AS posted,display_dirty FROM polls WHERE guild_id=$1 AND id=$2"
    ).bind(GUILD).bind(&id).fetch_one(db.pool()).await.unwrap();
    let final_votes = vote_count(db.pool(), &id).await;
    let requests = mock.requests();
    let edits: Vec<Value> = requests
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.contains("/channels/"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    let posts: Vec<Value> = requests
        .iter()
        .filter(|r| r.method == "POST" && r.path == format!("/api/v10/channels/{CHANNEL}/messages"))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    peer.close().await;
    closer_pool.close().await;
    db.close().await.unwrap();

    assert!(
        vote_waited,
        "vote must wait on the refreshing parent's real row lock"
    );
    assert_eq!(votes_before_refresh_release, 0);
    assert_eq!(committed_before_close, 1);
    assert!(
        close_waited,
        "close must wait through the vote's display PATCH/dirty clear"
    );
    assert_eq!(status_before_vote_release, "open");
    assert_eq!(row.get::<String, _>("status"), "closed");
    assert_eq!(
        serde_json::from_str::<Value>(row.get::<&str, _>("counts")).unwrap(),
        json!([1, 0])
    );
    assert_eq!(row.get::<Option<i32>, _>("winner"), Some(0));
    assert!(row.get::<bool, _>("posted"));
    assert!(!row.get::<bool, _>("display_dirty"));
    assert_eq!(final_votes, 1, "late votes cannot change frozen results");
    assert_eq!(
        edits.len(),
        3,
        "refresh, accepted vote and final close each edit once"
    );
    let buttons: Vec<&Value> = edits.last().unwrap()["components"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row["components"].as_array().unwrap())
        .collect();
    assert_eq!(buttons.len(), 2);
    assert!(
        buttons.iter().all(|button| button["disabled"] == true),
        "no refresh may reopen the final poll display"
    );
    assert_eq!(
        posts.len(),
        2,
        "one creation plus exactly one final results post"
    );
    let results = posts.last().unwrap()["embeds"][0]["description"]
        .as_str()
        .unwrap();
    assert!(
        results.contains("1. one — 1"),
        "results must include the committed vote: {results}"
    );
    assert!(results.contains("2. two — 0"), "{results}");
}
