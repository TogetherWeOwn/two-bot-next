//! Required-audit atomicity and post-wait eligibility for increase-only LFG seats.
//! Real SQL/runtime; only Discord uses loopback I/O. Faults live in disposable DBs.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{
    commands::PERM_MANAGE_EVENTS, lfg, lfg_store as store, rsvp::RsvpAudit, RouterGates,
};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "2222";
const POST: &str = "seat-boundary-fixture";
const NEW_TIME: &str = "2099-09-12T19:15:00Z";
const MESSAGE_PATH: &str = "/api/v10/channels/4444/messages/5000";

async fn database() -> (tokio::sync::SemaphorePermit<'static>, TestDatabase) {
    static FIXTURES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let permit = FIXTURES.acquire().await.expect("fixture permit");
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database");
    (permit, db)
}

async fn seed(pool: &sqlx::PgPool) {
    let post = lfg::LfgPost {
        id: POST.into(),
        guild_id: GUILD.into(),
        channel_id: "4444".into(),
        message_id: Some("5000".into()),
        title: "Original raid".into(),
        starts_at: "2099-09-11T20:00:00.654321Z".into(),
        status: lfg::LfgStatus::Open,
        created_by: "7777".into(),
        created_at: "2026-09-30T00:00:00.123456Z".into(),
        closed_at: None,
    };
    let roles = lfg::spec_roles(
        POST,
        &lfg::parse_role_spec("tank:Main tank:1,healer:Healer:2,dps:Damage:4").expect("roles"),
    );
    store::put_lfg(pool, &post, &roles, true)
        .await
        .expect("seed post");
    for (user, role, joined) in [
        ("3334", "tank", "2026-09-30T00:01:00.123456Z"),
        ("3335", "healer", "2026-09-30T00:02:00.234567Z"),
        ("3336", "dps", "2026-09-30T00:03:00.345678Z"),
    ] {
        assert_eq!(
            store::signup_lfg(pool, GUILD, POST, role, user, joined)
                .await
                .expect("seed signup"),
            lfg::SignupOutcome::Joined
        );
    }
}

async fn snapshot(pool: &sqlx::PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
          'posts', (SELECT coalesce(jsonb_agg(to_jsonb(p) ORDER BY p.id), '[]'::jsonb) FROM lfg_posts p),
          'roles', (SELECT coalesce(jsonb_agg(to_jsonb(r) ORDER BY r.lfg_id, r.position), '[]'::jsonb) FROM lfg_roles r),
          'signups', (SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY s.lfg_id, s.user_id), '[]'::jsonb) FROM lfg_signups s),
          'audits', (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY a.id), '[]'::jsonb) FROM announcements_audit_log a),
          'versions', jsonb_build_object(
            'posts', (SELECT jsonb_agg(jsonb_build_array(p.ctid::text, p.xmin::text) ORDER BY p.id) FROM lfg_posts p),
            'roles', (SELECT jsonb_agg(jsonb_build_array(r.ctid::text, r.xmin::text) ORDER BY r.lfg_id, r.position) FROM lfg_roles r),
            'signups', (SELECT jsonb_agg(jsonb_build_array(s.ctid::text, s.xmin::text) ORDER BY s.lfg_id, s.user_id) FROM lfg_signups s),
            'audits', (SELECT jsonb_agg(jsonb_build_array(a.ctid::text, a.xmin::text) ORDER BY a.id) FROM announcements_audit_log a)))",
    ).fetch_one(pool).await.expect("whole durable snapshot")
}

fn seat_options() -> Vec<Value> {
    vec![
        json!({"type": 3, "name": "id", "value": POST}),
        json!({"type": 3, "name": "role", "value": "tank"}),
        json!({"type": 4, "name": "seats", "value": 3}),
    ]
}

fn edit(id: u64, options: Vec<Value>) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "guild_id": GUILD, "channel": {"id": "4444", "type": 0},
        "member": {"user": {"id": "3333", "username": "editor", "discriminator": "0", "avatar": null},
            "roles": [], "joined_at": null, "deaf": false, "mute": false, "flags": 0,
            "permissions": PERM_MANAGE_EVENTS.to_string()},
        "token": format!("seat-boundary-{id}"), "version": 1, "entitlements": [],
        "authorizing_integration_owners": {},
        "data": {"id": "1", "name": "lfg-edit", "type": 1, "options": options}
    }))
    .expect("synthetic wire interaction")
}

fn runtime(pool: sqlx::PgPool, mock: &MockRest) -> InteractionRuntime {
    InteractionRuntime::new(
        RouterGates {
            configured_guild: Some(2222),
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
        },
        pool,
        ActionExecutor::with_proxy("synthetic-seat-boundary-token".into(), Some(mock.origin()))
            .expect("loopback executor"),
        1111,
        two_bot_core::ClassifierConfig::default(),
    )
}

fn body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("REST JSON")
}

fn channel_requests(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests
        .iter()
        .filter(|r| r.path.starts_with("/api/v10/channels/"))
        .collect()
}

fn private_reply(requests: &[RestRequest]) -> String {
    let first = body(requests.first().expect("private acknowledgement"));
    assert_eq!(first["data"]["flags"].as_u64().map(|n| n & 64), Some(64));
    for request in requests {
        let v = body(request);
        let data = if request.path.ends_with("/callback") {
            &v["data"]
        } else {
            &v
        };
        assert_eq!(data["allowed_mentions"]["parse"], json!([]));
        assert!(
            request.path.ends_with("/callback") || request.path.contains("/webhooks/"),
            "refusal must not touch a channel: {} {}",
            request.method,
            request.path
        );
    }
    let text = requests
        .iter()
        .rev()
        .find_map(|request| {
            let v = body(request);
            if request.path.ends_with("/callback") {
                v["data"]["content"].as_str().map(str::to_owned)
            } else {
                v["content"].as_str().map(str::to_owned)
            }
        })
        .expect("private result");
    assert!(text.encode_utf16().count() <= 300, "bounded refusal");
    assert!(!text.contains("@everyone") && !text.contains("@here"));
    text.to_lowercase()
}

fn audit(id: &str) -> RsvpAudit {
    RsvpAudit {
        id: id.into(),
        guild_id: GUILD.into(),
        actor_id: Some("3333".into()),
        action: "lfg.seats".into(),
        target_key: Some(POST.into()),
        outcome: "updated".into(),
        reason: None,
        created_at: "2026-10-01T00:00:00Z".into(),
    }
}

fn committed_capacity(before: &Value, after: &Value, seats: u8, audit_count: usize) {
    let mut roles = before["roles"].clone();
    roles[0]["slots"] = json!(seats);
    assert_eq!(after["roles"], roles);
    for table in ["posts", "signups"] {
        assert_eq!(after[table], before[table]);
        assert_eq!(after["versions"][table], before["versions"][table]);
    }
    assert_eq!(
        after["audits"].as_array().expect("audits").len(),
        audit_count
    );
}

// Catches committing capacity before its required audit, swallowing INSERT errors,
// refreshing on failed storage, and exposing the SQL error to the command caller.
#[tokio::test]
async fn audit_insert_failure_rolls_back_capacity_and_never_refreshes_the_message() {
    let (_serial, db) = database().await;
    seed(db.pool()).await;
    assert_eq!(
        store::increase_lfg_seats(
            db.pool(),
            GUILD,
            POST,
            "tank",
            2,
            &audit("seat-boundary:prior")
        )
        .await
        .expect("seed prior committed audit history"),
        store::LfgSeatEditOutcome::Updated
    );
    sqlx::query("ALTER TABLE announcements_audit_log ADD CONSTRAINT seat_boundary_audit_fault CHECK (target_key IS DISTINCT FROM 'seat-boundary-fixture')")
        .execute(db.pool()).await.expect("disposable audit INSERT fault");
    let before = snapshot(db.pool()).await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "5000"}))).await;
    let rt = runtime(db.pool().clone(), &mock);
    let handled = rt.handle(&edit(23101, seat_options())).await;
    let failed = snapshot(db.pool()).await;
    let refusal_requests = mock.requests();

    sqlx::query("ALTER TABLE announcements_audit_log DROP CONSTRAINT seat_boundary_audit_fault")
        .execute(db.pool())
        .await
        .expect("remove only fixture fault");
    let offset = mock.requests().len();
    let control = rt.handle(&edit(23102, seat_options())).await;
    let saved = snapshot(db.pool()).await;
    let control_requests = mock.requests()[offset..].to_vec();
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("teardown");

    assert!(handled.expect("refusal transport"));
    assert_eq!(
        failed, before,
        "failed audit partially committed capacity or rewrote rows"
    );
    assert!(channel_requests(&refusal_requests).is_empty());
    let text = private_reply(&refusal_requests);
    assert!(
        text.contains("failed")
            || text.contains("could not")
            || text.contains("unavailable")
            || text.contains("unable")
    );
    assert!(!text.contains("saved") && !text.contains("updated"));
    assert!(!text.contains("seat_boundary_audit_fault") && !text.contains("23514"));
    assert!(control.expect("positive control transport"));
    committed_capacity(&before, &saved, 3, 2);
    assert!(saved["audits"]
        .as_array()
        .expect("saved audits")
        .contains(&before["audits"][0]));
    let refreshes = channel_requests(&control_requests);
    assert_eq!(
        refreshes.len(),
        1,
        "removing audit fault must restore a valid increase"
    );
    assert_eq!(refreshes[0].method, "PATCH");
    assert_eq!(refreshes[0].path, MESSAGE_PATH);
}

// Catches ON CONFLICT DO NOTHING allowing a new increase to reuse an older
// increase's identity and commit without a new truthful audit row.
#[tokio::test]
async fn duplicate_audit_identity_refuses_a_distinct_capacity_increase_atomically() {
    let (_serial, db) = database().await;
    seed(db.pool()).await;
    let original = snapshot(db.pool()).await;
    let first = store::increase_lfg_seats(
        db.pool(),
        GUILD,
        POST,
        "tank",
        2,
        &audit("seat-boundary:collision"),
    )
    .await
    .expect("first increase");
    let before = snapshot(db.pool()).await;
    let collision = store::increase_lfg_seats(
        db.pool(),
        GUILD,
        POST,
        "tank",
        3,
        &audit("seat-boundary:collision"),
    )
    .await;
    let refused = collision.is_err();
    let after = snapshot(db.pool()).await;
    let control = store::increase_lfg_seats(
        db.pool(),
        GUILD,
        POST,
        "tank",
        3,
        &audit("seat-boundary:fresh"),
    )
    .await;
    let saved = snapshot(db.pool()).await;
    db.close().await.expect("teardown");

    assert_eq!(first, store::LfgSeatEditOutcome::Updated);
    committed_capacity(&original, &before, 2, 1);
    assert!(
        refused,
        "a distinct increase reused an audit identity instead of refusing"
    );
    assert_eq!(
        after, before,
        "audit collision changed capacity, roster, metadata or audit history"
    );
    assert_eq!(
        control.expect("fresh identity control"),
        store::LfgSeatEditOutcome::Updated
    );
    committed_capacity(&before, &saved, 3, 2);
    assert_eq!(saved["audits"][0], before["audits"][0]);
}

async fn role_waiter_before_start(pool: &sqlx::PgPool, holder_pid: i32) -> bool {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity a CROSS JOIN lfg_posts p
                 WHERE a.datname = current_database() AND p.id = $1
                 AND $2 = ANY(pg_blocking_pids(a.pid)) AND a.wait_event_type = 'Lock'
                 AND a.query LIKE '%lfg_roles%' AND a.query LIKE '%FOR UPDATE%'
                 AND a.xact_start < p.starts_at AND a.query_start < p.starts_at
                 AND clock_timestamp() < p.starts_at)",
            )
            .bind(POST)
            .bind(holder_pid)
            .fetch_one(pool)
            .await
            .expect("observe real role-row waiter");
            if waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

async fn cross_start_boundary(pool: &sqlx::PgPool) -> bool {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let started: bool = sqlx::query_scalar(
                "SELECT clock_timestamp() >= starts_at FROM lfg_posts WHERE id = $1",
            )
            .bind(POST)
            .fetch_one(pool)
            .await
            .expect("database wall-clock boundary");
            if started {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

// Catches reading time before the role-row wait (including transaction-start
// now()), even when the post/advisory locks have already been acquired correctly.
#[tokio::test]
async fn crossing_start_while_blocked_on_the_role_row_refuses_without_audit_or_refresh() {
    let (_serial, db) = database().await;
    seed(db.pool()).await;
    let peer = db.independent_pool().await.expect("editor pool");
    let observer = db.independent_pool().await.expect("observer pool");
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "5000"}))).await;
    let rt = Arc::new(runtime(peer.clone(), &mock));
    let mut holder = db.pool().begin().await.expect("role-row holder");
    let holder_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holder)
        .await
        .expect("holder identity");
    sqlx::query(
        "SELECT role_key FROM lfg_roles WHERE lfg_id = $1 AND role_key = 'tank' FOR UPDATE",
    )
    .bind(POST)
    .fetch_one(&mut *holder)
    .await
    .expect("selected role row lock only");
    sqlx::query(
        "UPDATE lfg_posts SET starts_at = clock_timestamp() + interval '3 seconds' WHERE id = $1",
    )
    .bind(POST)
    .execute(&observer)
    .await
    .expect("near-future fixture start");
    let before = snapshot(&observer).await;
    let worker = tokio::spawn({
        let rt = rt.clone();
        async move { rt.handle(&edit(23201, seat_options())).await }
    });
    let waiting = role_waiter_before_start(&observer, holder_pid).await;
    let during = snapshot(&observer).await;
    let premature = worker.is_finished();
    let premature_channels = channel_requests(&mock.requests()).len();
    let crossed = cross_start_boundary(&observer).await;
    // A rollback releases the row without changing even its tuple version.
    holder
        .rollback()
        .await
        .expect("release selected role row after start");
    let result = tokio::time::timeout(Duration::from_secs(10), worker).await;
    let after = snapshot(&observer).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    peer.close().await;
    observer.close().await;
    db.close().await.expect("teardown");

    assert!(
        waiting,
        "no real editor blocked on the selected role while the post was future"
    );
    assert!(
        !premature,
        "editor completed before the role-row holder released"
    );
    assert_eq!(
        premature_channels, 0,
        "editor refreshed before row-lock admission"
    );
    assert_eq!(during, before, "editor mutated while blocked");
    assert!(
        crossed,
        "fixture never crossed the real database clock boundary"
    );
    assert!(result
        .expect("editor completion")
        .expect("editor task")
        .expect("editor transport"));
    assert_eq!(
        after, before,
        "started editor changed capacity, roster, metadata or audit"
    );
    assert!(channel_requests(&requests).is_empty());
    assert!(
        private_reply(&requests).contains("started"),
        "must report started refusal, not a lock timeout or generic error"
    );
}

// Catches accepting mixed modes, silently ignoring valid seat options, or breaking
// metadata-only edits while trying to reject the combined operation.
#[tokio::test]
async fn valid_seat_plus_metadata_modes_refuse_but_metadata_only_controls_still_save() {
    let (_serial, db) = database().await;
    seed(db.pool()).await;
    let before = snapshot(db.pool()).await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "5000"}))).await;
    let rt = runtime(db.pool().clone(), &mock);
    let title = json!({"type": 3, "name": "title", "value": "Updated raid"});
    let starts = json!({"type": 3, "name": "starts-at", "value": NEW_TIME});
    let mut observed = Vec::new();
    for (index, metadata) in [
        vec![title.clone()],
        vec![starts.clone()],
        vec![title.clone(), starts.clone()],
    ]
    .into_iter()
    .enumerate()
    {
        let mut options = seat_options();
        options.extend(metadata);
        let offset = mock.requests().len();
        let handled = rt.handle(&edit(23300 + index as u64, options)).await;
        observed.push((
            handled,
            snapshot(db.pool()).await,
            mock.requests()[offset..].to_vec(),
        ));
    }
    let control_offset = mock.requests().len();
    let mut controls = Vec::new();
    for (index, option) in [title, starts].into_iter().enumerate() {
        controls.push(
            rt.handle(&edit(
                23400 + index as u64,
                vec![json!({"type": 3, "name": "id", "value": POST}), option],
            ))
            .await,
        );
    }
    let saved = snapshot(db.pool()).await;
    let control_requests = mock.requests()[control_offset..].to_vec();
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("teardown");

    for (handled, after, requests) in observed {
        assert!(handled.expect("mixed-mode transport"));
        assert_eq!(
            after, before,
            "valid combined edit partially changed metadata or seats"
        );
        assert!(channel_requests(&requests).is_empty());
        let text = private_reply(&requests);
        assert!(!text.contains("saved") && !text.contains("updated"));
        assert!(
            text.contains("seat")
                || text.contains("role")
                || text.contains("separate")
                || text.contains("mix")
        );
    }
    assert!(controls
        .into_iter()
        .all(|result| result.expect("metadata control transport")));
    assert_eq!(saved["posts"][0]["title"], "Updated raid");
    let time = time::OffsetDateTime::parse(
        saved["posts"][0]["starts_at"]
            .as_str()
            .expect("stored timestamp"),
        &time::format_description::well_known::Rfc3339,
    )
    .expect("stored RFC3339 timestamp");
    assert_eq!(
        time,
        time::OffsetDateTime::parse(NEW_TIME, &time::format_description::well_known::Rfc3339)
            .expect("literal control time")
    );
    for table in ["roles", "signups"] {
        assert_eq!(saved[table], before[table]);
        assert_eq!(saved["versions"][table], before["versions"][table]);
    }
    let refreshes = channel_requests(&control_requests);
    assert_eq!(
        refreshes.len(),
        2,
        "each metadata-only control must save and refresh"
    );
    assert!(refreshes
        .iter()
        .all(|r| r.method == "PATCH" && r.path == MESSAGE_PATH));
}
