//! Protected increase-only seat editing through the real router, SQL store and executor.
//! Only Discord is replaced by loopback I/O. No future API or fallback implementation.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{commands::PERM_MANAGE_EVENTS, lfg, lfg_store as store, RouterGates};
use two_bot_discord::{interactions::InteractionRuntime, ActionExecutor};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "2222";
const POST: &str = "seat-edit-fixture";
const MESSAGE_PATH: &str = "/api/v10/channels/4444/messages/5000";

fn invocation(id: u64, name: &str, options: Vec<Value>, permissions: u64) -> Interaction {
    serde_json::from_value(json!({
        "id": id.to_string(), "application_id": "1111", "type": 2,
        "guild_id": GUILD, "channel": {"id": "4444", "type": 0},
        "member": {"user": {"id": "3333", "username": "editor", "discriminator": "0", "avatar": null},
            "roles": [], "joined_at": null, "deaf": false, "mute": false, "flags": 0,
            "permissions": permissions.to_string()},
        "token": format!("seat-fixture-{id}"), "version": 1, "entitlements": [],
        "authorizing_integration_owners": {},
        "data": {"id": "1", "name": name, "type": 1, "options": options}
    }))
    .expect("valid synthetic wire interaction")
}

fn options(role: &str, seats: i64) -> Vec<Value> {
    vec![
        json!({"type": 3, "name": "id", "value": POST}),
        json!({"type": 3, "name": "role", "value": role}),
        json!({"type": 4, "name": "seats", "value": seats}),
    ]
}

fn edit(id: u64, role: &str, seats: i64) -> Interaction {
    invocation(id, "lfg-edit", options(role, seats), PERM_MANAGE_EVENTS)
}

fn signup(id: u64, role: &str) -> Interaction {
    let mut wire = serde_json::to_value(invocation(id, "lfg-edit", vec![], 0)).expect("wire");
    wire["type"] = json!(3);
    wire["member"]["user"]["id"] = json!("3337");
    wire["data"] =
        json!({"custom_id": "two:lfg:seat-edit-fixture", "component_type": 3, "values": [role]});
    serde_json::from_value(wire).expect("signup interaction")
}

fn runtime(pool: sqlx::PgPool, mock: &MockRest, enabled: bool) -> InteractionRuntime {
    InteractionRuntime::new(
        RouterGates {
            configured_guild: Some(2222),
            announcements: enabled,
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
        ActionExecutor::with_proxy("synthetic-seat-test-token".into(), Some(mock.origin()))
            .expect("loopback executor"),
        1111,
        two_bot_core::ClassifierConfig::default(),
    )
}

async fn database() -> (tokio::sync::SemaphorePermit<'static>, TestDatabase) {
    static FIXTURES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let serial = FIXTURES.acquire().await.expect("fixture permit");
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database");
    (serial, db)
}

async fn seed(pool: &sqlx::PgPool, guild: &str, starts: &str) {
    let post = lfg::LfgPost {
        id: POST.into(),
        guild_id: guild.into(),
        channel_id: "4444".into(),
        message_id: Some("5000".into()),
        title: "Original raid".into(),
        starts_at: starts.into(),
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
    // Seed before the start even in the already-started eligibility fixture.
    for (user, role, joined) in [
        ("3334", "tank", "1999-09-30T00:01:00.123456Z"),
        ("3335", "healer", "1999-09-30T00:02:00.234567Z"),
        ("3336", "dps", "1999-09-30T00:03:00.345678Z"),
    ] {
        assert_eq!(
            store::signup_lfg(pool, guild, POST, role, user, joined)
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
          'roles', (SELECT coalesce(jsonb_agg(to_jsonb(r) ORDER BY r.position), '[]'::jsonb) FROM lfg_roles r),
          'signups', (SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY s.joined_at, s.user_id), '[]'::jsonb) FROM lfg_signups s),
          'versions', jsonb_build_object(
            'posts', (SELECT jsonb_agg(jsonb_build_array(p.ctid::text, p.xmin::text) ORDER BY p.id) FROM lfg_posts p),
            'roles', (SELECT jsonb_object_agg(r.role_key, jsonb_build_array(r.ctid::text, r.xmin::text)) FROM lfg_roles r),
            'signups', (SELECT jsonb_agg(jsonb_build_array(s.ctid::text, s.xmin::text) ORDER BY s.joined_at, s.user_id) FROM lfg_signups s)),
          'audits', (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY a.id), '[]'::jsonb) FROM announcements_audit_log a))",
    ).fetch_one(pool).await.expect("whole durable snapshot")
}

fn body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("REST JSON")
}

fn channels(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests
        .iter()
        .filter(|r| r.path.starts_with("/api/v10/channels/"))
        .collect()
}

fn reply(requests: &[RestRequest]) -> String {
    requests
        .iter()
        .rev()
        .find_map(|r| {
            let v = body(r);
            if r.path.contains("/webhooks/") {
                v["content"].as_str().map(str::to_owned)
            } else if r.path.ends_with("/callback") {
                v["data"]["content"].as_str().map(str::to_owned)
            } else {
                None
            }
        })
        .expect("private result reply")
}

fn safe_private(requests: &[RestRequest]) {
    assert!(!requests.is_empty(), "interaction was silently abandoned");
    let first = body(&requests[0]);
    assert_eq!(
        first["data"]["flags"].as_u64().map(|n| n & 64),
        Some(64),
        "initial acknowledgement must be private"
    );
    for request in requests {
        let v = body(request);
        let data = if request.path.ends_with("/callback") {
            &v["data"]
        } else {
            &v
        };
        assert_eq!(data["allowed_mentions"]["parse"], json!([]));
        if let Some(text) = data["content"].as_str() {
            assert!(text.encode_utf16().count() <= 2000);
            assert!(!text.contains("@everyone") && !text.contains("@here"));
        }
        assert!(
            request.path.ends_with("/callback")
                || request.path.contains("/webhooks/")
                || (request.method == "PATCH" && request.path == MESSAGE_PATH),
            "unexpected Discord effect: {} {}",
            request.method,
            request.path
        );
    }
}

fn seats_only(before: &Value, after: &Value, seats: i64) {
    let mut expected_roles = before["roles"].clone();
    expected_roles[0]["slots"] = json!(seats);
    assert_eq!(
        after["roles"], expected_roles,
        "only the existing tank role's absolute seat count may change"
    );
    for table in ["posts", "signups"] {
        assert_eq!(after[table], before[table], "changed {table}");
        assert_eq!(
            after["versions"][table], before["versions"][table],
            "rewrote {table}"
        );
    }
    for role in ["healer", "dps"] {
        assert_eq!(
            after["versions"]["roles"][role], before["versions"]["roles"][role],
            "rewrote unrelated role {role}"
        );
    }
}

fn truthful_audit(after: &Value, count: usize) {
    let rows = after["audits"].as_array().expect("audit array");
    assert_eq!(
        rows.len(),
        count,
        "one audit per committed increase, none per replay/refusal"
    );
    for row in rows {
        assert_eq!(row["guild_id"], GUILD);
        assert_eq!(row["actor_id"], "3333");
        assert_eq!(row["target_key"], POST);
        let action = row["action"]
            .as_str()
            .expect("bounded action")
            .to_lowercase();
        let outcome = row["outcome"]
            .as_str()
            .expect("truthful outcome")
            .to_lowercase();
        assert!(action.contains("edit") || action.contains("seat") || action.contains("capacity"));
        assert!(
            ["updated", "edited", "increased", "saved", "ok"].contains(&outcome.as_str()),
            "not a saved increase outcome: {outcome}"
        );
        assert!(action.len() <= 64 && outcome.len() <= 64);
        assert!(row["reason"]
            .as_str()
            .is_none_or(|s| s.encode_utf16().count() <= 300));
    }
}

// Catches role replacement/cascade deletion, additive instead of absolute updates,
// unrelated metadata rewrites, missing refresh, public replies and duplicate audits.
#[tokio::test]
async fn increase_preserves_signup_bytes_and_refreshes_only_the_original_message() {
    let (_serial, db) = database().await;
    seed(db.pool(), GUILD, "2099-09-11T20:00:00.654321Z").await;
    let before = snapshot(db.pool()).await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "5000"}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    let handled = rt
        .handle(&edit(21001, "tank", 3))
        .await
        .expect("edit transport");
    let after = snapshot(db.pool()).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("teardown");

    assert!(handled, "edit must be dispatched");
    seats_only(&before, &after, 3);
    truthful_audit(&after, 1);
    let edits = channels(&requests);
    assert_eq!(edits.len(), 1, "exactly one existing-message refresh");
    assert_eq!(edits[0].method, "PATCH");
    assert_eq!(edits[0].path, MESSAGE_PATH);
    let rendered = body(edits[0]);
    let text = rendered["content"].as_str().expect("rendered roster");
    for token in [
        "Original raid",
        "**Main tank** 1/3",
        "<@3334>",
        "**Healer** 1/2",
        "<@3335>",
        "**Damage** 1/4",
        "<@3336>",
    ] {
        assert!(text.contains(token), "roster lost {token}");
    }
    assert_eq!(
        rendered["components"][0]["components"][0]["custom_id"],
        "two:lfg:seat-edit-fixture"
    );
    let values = rendered["components"][0]["components"][0]["options"]
        .as_array()
        .expect("select options")
        .iter()
        .map(|o| o["value"].as_str().expect("stable identity"))
        .collect::<Vec<_>>();
    assert_eq!(values, ["tank", "healer", "dps", "__leave__"]);
    let receipt = reply(&requests).to_lowercase();
    assert!(
        receipt.contains("saved") || receipt.contains("updated") || receipt.contains("increased")
    );
    safe_private(&requests);
}

#[tokio::test]
async fn equal_target_is_a_true_noop_and_a_replayed_increase_does_not_add_seats() {
    let (_serial, db) = database().await;
    seed(db.pool(), GUILD, "2099-09-11T20:00:00Z").await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "5000"}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    let initial = snapshot(db.pool()).await;
    rt.handle(&edit(21101, "tank", 1))
        .await
        .expect("equal transport");
    let equal = snapshot(db.pool()).await;
    let equal_requests = mock.requests();
    rt.handle(&edit(21102, "tank", 3))
        .await
        .expect("increase transport");
    let saved = snapshot(db.pool()).await;
    let boundary = mock.requests().len();
    rt.handle(&edit(21102, "tank", 3))
        .await
        .expect("same interaction replay");
    rt.handle(&edit(21103, "tank", 3))
        .await
        .expect("fresh equal request");
    let replayed = snapshot(db.pool()).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("teardown");

    assert_eq!(equal, initial, "equal target must not rewrite or audit");
    assert!(channels(&equal_requests).is_empty());
    let no_change = reply(&equal_requests).to_lowercase();
    assert!(
        no_change.contains("unchanged")
            || no_change.contains("already")
            || no_change.contains("no change"),
        "equal target must be distinguished from invalid input: {no_change}"
    );
    seats_only(&initial, &saved, 3);
    truthful_audit(&saved, 1);
    assert_eq!(
        replayed, saved,
        "absolute replay added seats, rewrote rows or duplicated audit"
    );
    assert!(
        channels(&requests[boundary..]).is_empty(),
        "no-op must not refresh"
    );
    let replay_copy = reply(&requests[boundary..]).to_lowercase();
    assert!(
        replay_copy.contains("unchanged")
            || replay_copy.contains("already")
            || replay_copy.contains("no change")
    );
    safe_private(&equal_requests);
    safe_private(&requests[boundary..]);
}

#[tokio::test]
async fn ninety_nine_is_accepted_and_the_new_capacity_admits_a_player_to_the_full_role() {
    let (_serial, db) = database().await;
    seed(db.pool(), GUILD, "2099-09-11T20:00:00Z").await;
    let initial = snapshot(db.pool()).await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "5000"}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    rt.handle(&edit(21201, "tank", 99))
        .await
        .expect("upper bound edit");
    let saved = snapshot(db.pool()).await;
    rt.handle(&signup(21202, "tank"))
        .await
        .expect("new player select");
    let joined = snapshot(db.pool()).await;
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("teardown");

    seats_only(&initial, &saved, 99);
    truthful_audit(&saved, 1);
    assert_eq!(joined["roles"][0]["slots"], 99);
    let signups = joined["signups"].as_array().expect("signups");
    assert_eq!(
        signups.len(),
        4,
        "increased seats must be usable by the real signup path"
    );
    for row in initial["signups"].as_array().expect("initial roster") {
        assert!(signups.contains(row));
    }
    assert!(signups
        .iter()
        .any(|s| s["user_id"] == "3337" && s["role_key"] == "tank"));
}

// A valid title is deliberately included: silently dropping malformed new options
// must not turn an invalid seat edit into a successful metadata-only edit.
#[tokio::test]
async fn malformed_duplicate_unknown_and_shrinking_targets_reject_before_any_mutation() {
    let (_serial, db) = database().await;
    seed(db.pool(), GUILD, "2099-09-11T20:00:00Z").await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "5000"}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    let mut cases = vec![
        ("zero", options("tank", 0)),
        ("negative", options("tank", -1)),
        ("over maximum", options("tank", 100)),
        ("shrink", options("healer", 1)),
        ("unknown role", options("absent", 3)),
        ("label is not identity", options("Main tank", 3)),
        ("empty role", options("", 3)),
        ("hostile role", options("@everyone <@&8888>", 3)),
        (
            "missing role",
            vec![
                json!({"type": 3, "name": "id", "value": POST}),
                json!({"type": 4, "name": "seats", "value": 3}),
            ],
        ),
        (
            "missing seats",
            vec![
                json!({"type": 3, "name": "id", "value": POST}),
                json!({"type": 3, "name": "role", "value": "tank"}),
            ],
        ),
    ];
    let mut wrong_role = options("tank", 3);
    wrong_role[1] = json!({"type": 4, "name": "role", "value": 7});
    cases.push(("wrong role type", wrong_role));
    let mut wrong_seats = options("tank", 3);
    wrong_seats[2] = json!({"type": 3, "name": "seats", "value": "3"});
    cases.push(("wrong seat type", wrong_seats));
    for (label, extra) in [
        (
            "duplicate role",
            json!({"type": 3, "name": "role", "value": "healer"}),
        ),
        (
            "duplicate seats",
            json!({"type": 4, "name": "seats", "value": 4}),
        ),
        (
            "identical duplicate role",
            json!({"type": 3, "name": "role", "value": "tank"}),
        ),
        (
            "identical duplicate seats",
            json!({"type": 4, "name": "seats", "value": 3}),
        ),
        (
            "duplicate id",
            json!({"type": 3, "name": "id", "value": POST}),
        ),
        (
            "unknown option",
            json!({"type": 3, "name": "capacity-typo", "value": "3"}),
        ),
        (
            "role replacement",
            json!({"type": 3, "name": "roles", "value": "new:Renamed:4"}),
        ),
    ] {
        let mut opts = options("tank", 3);
        opts.push(extra);
        cases.push((label, opts));
    }
    let mut observed = Vec::new();
    for (index, (label, mut opts)) in cases.into_iter().enumerate() {
        opts.push(json!({"type": 3, "name": "title", "value": "Must not commit"}));
        let before = snapshot(db.pool()).await;
        let offset = mock.requests().len();
        let handled = rt
            .handle(&invocation(
                21300 + index as u64,
                "lfg-edit",
                opts,
                PERM_MANAGE_EVENTS,
            ))
            .await
            .expect("refusal transport");
        observed.push((
            label,
            handled,
            before,
            snapshot(db.pool()).await,
            mock.requests()[offset..].to_vec(),
        ));
    }
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("teardown");

    for (label, handled, before, after, requests) in observed {
        assert!(handled, "{label} was ignored");
        assert_eq!(
            after, before,
            "{label}: invalid edit changed rows, tuple versions or audit"
        );
        assert!(
            channels(&requests).is_empty(),
            "{label}: invalid edit touched Discord"
        );
        let text = reply(&requests).to_lowercase();
        assert!(
            !text.contains("saved") && !text.contains("updated"),
            "{label}: false success {text}"
        );
        assert!(
            text.contains("role")
                || text.contains("seat")
                || text.contains("option")
                || text.contains("invalid")
                || text.contains("id"),
            "{label}: no readable correction: {text}"
        );
        assert!(
            text.encode_utf16().count() <= 300,
            "{label}: unbounded refusal"
        );
        safe_private(&requests);
    }
}

#[tokio::test]
async fn closed_started_foreign_and_missing_posts_are_not_capacity_editable() {
    let mut observed = Vec::new();
    for (index, case) in ["closed", "started", "foreign", "missing"]
        .into_iter()
        .enumerate()
    {
        let (_serial, db) = database().await;
        seed(
            db.pool(),
            if case == "foreign" { "9999" } else { GUILD },
            if case == "started" {
                "2000-01-01T00:00:00Z"
            } else {
                "2099-09-11T20:00:00Z"
            },
        )
        .await;
        if case == "closed" {
            store::close_lfg(db.pool(), GUILD, POST, "2026-10-01T00:00:00Z")
                .await
                .expect("seed closure");
        }
        let before = snapshot(db.pool()).await;
        let mock =
            MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "5000"}))).await;
        let rt = runtime(db.pool().clone(), &mock, true);
        let mut opts = options("tank", 3);
        if case == "missing" {
            opts[0]["value"] = json!("absent-post");
        }
        let handled = rt
            .handle(&invocation(
                21400 + index as u64,
                "lfg-edit",
                opts,
                PERM_MANAGE_EVENTS,
            ))
            .await
            .expect("eligibility transport");
        let after = snapshot(db.pool()).await;
        let requests = mock.requests();
        mock.shutdown().await;
        drop(rt);
        db.close().await.expect("teardown");
        observed.push((case, handled, before, after, requests));
    }
    for (case, handled, before, after, requests) in observed {
        assert!(handled);
        assert_eq!(after, before, "{case} target changed durable state");
        assert!(channels(&requests).is_empty());
        let text = reply(&requests).to_lowercase();
        let expected = match case {
            "closed" => "closed",
            "started" => "started",
            _ => "missing",
        };
        assert!(
            text.contains(expected),
            "{case} needs an actionable eligibility refusal, not an unrelated parse error: {text}"
        );
        safe_private(&requests);
    }
}

#[tokio::test]
async fn permission_is_required_on_each_fresh_edit_and_feature_guild_fences_remain() {
    let (_serial, db) = database().await;
    seed(db.pool(), GUILD, "2099-09-11T20:00:00Z").await;
    let before = snapshot(db.pool()).await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "5000"}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    let mut outcomes = Vec::new();
    for (index, permission) in [0, 8, 32, 16].into_iter().enumerate() {
        let offset = mock.requests().len();
        rt.handle(&invocation(
            21500 + index as u64,
            "lfg-edit",
            options("tank", 3),
            permission,
        ))
        .await
        .expect("permission transport");
        outcomes.push(mock.requests()[offset..].to_vec());
    }
    let off = runtime(db.pool().clone(), &mock, false);
    let offset = mock.requests().len();
    off.handle(&edit(21510, "tank", 3))
        .await
        .expect("disabled transport");
    let disabled = mock.requests()[offset..].to_vec();
    let mut foreign = edit(21511, "tank", 3);
    foreign.guild_id = Some(twilight_model::id::Id::new(9999));
    let offset = mock.requests().len();
    let foreign_handled = rt.handle(&foreign).await.expect("foreign transport");
    let foreign_requests = mock.requests()[offset..].to_vec();
    let after = snapshot(db.pool()).await;
    mock.shutdown().await;
    drop((rt, off));
    db.close().await.expect("teardown");

    assert_eq!(after, before);
    for requests in outcomes {
        assert_eq!(reply(&requests), two_bot_core::MANAGE_EVENTS_REQUIRED);
        assert!(channels(&requests).is_empty());
        safe_private(&requests);
    }
    assert_eq!(reply(&disabled), two_bot_core::ANNOUNCEMENTS_DISABLED_REPLY);
    assert!(channels(&disabled).is_empty());
    safe_private(&disabled);
    assert!(!foreign_handled);
    assert!(foreign_requests.is_empty());
}

#[tokio::test]
async fn committed_increase_with_failed_refresh_says_saved_and_does_not_repost_or_rollback() {
    let (_serial, db) = database().await;
    seed(db.pool(), GUILD, "2099-09-11T20:00:00Z").await;
    let before = snapshot(db.pool()).await;
    let mock = MockRest::with_responder(|request| {
        if request.path.starts_with("/api/v10/channels/") {
            ScriptedResponse::status(500)
        } else if request.path.ends_with("/callback") {
            ScriptedResponse::status(204)
        } else {
            ScriptedResponse::json(200, json!({"id": "5000"}))
        }
    })
    .await;
    let rt = runtime(db.pool().clone(), &mock, true);
    rt.handle(&edit(21601, "tank", 3))
        .await
        .expect("degraded refresh transport");
    let after = snapshot(db.pool()).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("teardown");

    seats_only(&before, &after, 3);
    truthful_audit(&after, 1);
    assert_eq!(
        channels(&requests).len(),
        1,
        "one attempted PATCH, never a replacement POST"
    );
    let text = reply(&requests).to_lowercase();
    assert!(
        text.contains("saved"),
        "must state that capacity committed: {text}"
    );
    assert!(
        text.contains("refresh")
            && (text.contains("failed")
                || text.contains("could not")
                || text.contains("not refreshed")),
        "must distinguish stale visible message from saved state: {text}"
    );
    assert!(
        text.contains("retry") || text.contains("check"),
        "give safe recovery guidance: {text}"
    );
    safe_private(&requests);
}

#[tokio::test]
async fn database_failure_changes_no_role_signup_audit_or_message_and_hides_sql_details() {
    let (_serial, db) = database().await;
    seed(db.pool(), GUILD, "2099-09-11T20:00:00Z").await;
    sqlx::query("ALTER TABLE lfg_roles ADD CONSTRAINT seat_update_fixture_fault CHECK (role_key <> 'tank' OR slots < 3)")
        .execute(db.pool())
        .await
        .expect("test-owned update fault");
    let before = snapshot(db.pool()).await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "5000"}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    rt.handle(&edit(21701, "tank", 3))
        .await
        .expect("store failure transport");
    let after = snapshot(db.pool()).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("teardown");

    assert_eq!(after, before);
    assert!(channels(&requests).is_empty());
    let text = reply(&requests).to_lowercase();
    assert!(
        text.contains("failed") || text.contains("could not") || text.contains("unavailable"),
        "not an honest store failure: {text}"
    );
    assert!(!text.contains("seat_update_fixture_fault"));
    safe_private(&requests);
}

async fn lock_waiter(pool: &sqlx::PgPool) -> bool {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
                 WHERE a.datname = current_database() AND l.locktype = 'advisory' AND NOT l.granted
                 AND cardinality(pg_blocking_pids(l.pid)) > 0)",
            )
            .fetch_one(pool)
            .await
            .expect("real database lock observation");
            if waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

#[derive(Clone, Copy, Debug)]
enum Operation {
    Seats(i64),
    Signup(&'static str),
    Close,
    Metadata,
}

impl Operation {
    fn wire(self, id: u64) -> Interaction {
        match self {
            Self::Seats(n) => edit(id, "tank", n),
            Self::Signup(role) => signup(id, role),
            Self::Close => invocation(
                id,
                "lfg-close",
                vec![json!({"type": 3, "name": "id", "value": POST})],
                PERM_MANAGE_EVENTS,
            ),
            Self::Metadata => invocation(
                id,
                "lfg-edit",
                vec![
                    json!({"type": 3, "name": "id", "value": POST}),
                    json!({"type": 3, "name": "title", "value": "Serialized raid"}),
                ],
                PERM_MANAGE_EVENTS,
            ),
        }
    }
}

async fn contended(
    first: Operation,
    second: Operation,
    capacity: i64,
    joined: bool,
    closed: bool,
    edits: usize,
) {
    let (_serial, db) = database().await;
    seed(db.pool(), GUILD, "2099-09-11T20:00:00Z").await;
    let initial = snapshot(db.pool()).await;
    let peer = db
        .independent_pool()
        .await
        .expect("independent runtime pool");
    let observer = db.independent_pool().await.expect("independent observer");
    let (mock, gate) = MockRest::start_gated(
        vec![
            ScriptedResponse::status(204),
            ScriptedResponse::json(200, json!({"id": "5000"})),
        ],
        ScriptedResponse::json(200, json!({"id": "5000"})),
    )
    .await;
    let first_rt = Arc::new(runtime(db.pool().clone(), &mock, true));
    let second_rt = Arc::new(runtime(peer.clone(), &mock, true));
    let a = tokio::spawn({
        let rt = first_rt.clone();
        async move { rt.handle(&first.wire(21801)).await }
    });
    let gated = tokio::time::timeout(Duration::from_secs(3), gate.wait_for_request())
        .await
        .is_ok();
    let at_refresh = gated && channels(&mock.requests()).len() == 1;
    let intermediate = snapshot(&observer).await;
    let b = tokio::spawn({
        let rt = second_rt.clone();
        async move { rt.handle(&second.wire(21802)).await }
    });
    let waiting = lock_waiter(&observer).await;
    let during = snapshot(&observer).await;
    let premature = channels(&mock.requests()).len();
    // Cleanup precedes assertions, including on the missing-feature RED baseline.
    gate.release();
    let a_result = tokio::time::timeout(Duration::from_secs(10), a)
        .await
        .expect("first completion")
        .expect("first task")
        .expect("first transport");
    let b_result = tokio::time::timeout(Duration::from_secs(10), b)
        .await
        .expect("second completion")
        .expect("second task")
        .expect("second transport");
    let after = snapshot(&observer).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop((first_rt, second_rt));
    peer.close().await;
    observer.close().await;
    db.close().await.expect("teardown");

    assert!(a_result && b_result);
    assert!(
        at_refresh,
        "{first:?} never reached the existing-message refresh"
    );
    assert!(
        waiting,
        "{second:?} did not contend on the real shared post lock"
    );
    assert_eq!(
        during, intermediate,
        "contender mutated while the first refresh held the lock"
    );
    assert_eq!(premature, 1, "out-of-order refresh");
    assert_eq!(
        after["roles"][0]["slots"], capacity,
        "stale edit rolled back a newer capacity"
    );
    for i in [1, 2] {
        assert_eq!(after["roles"][i], initial["roles"][i]);
    }
    let signups = after["signups"].as_array().expect("final signups");
    for row in initial["signups"].as_array().expect("initial signups") {
        assert!(signups.contains(row), "existing signup lost or rewritten");
    }
    assert_eq!(signups.len(), if joined { 4 } else { 3 });
    assert_eq!(
        after["posts"][0]["status"],
        if closed { "closed" } else { "open" }
    );
    let metadata = matches!(first, Operation::Metadata) || matches!(second, Operation::Metadata);
    assert_eq!(
        after["posts"][0]["title"],
        if metadata {
            "Serialized raid"
        } else {
            "Original raid"
        }
    );
    for key in [
        "id",
        "guild_id",
        "channel_id",
        "message_id",
        "starts_at",
        "created_by",
        "created_at",
    ] {
        assert_eq!(
            after["posts"][0][key], initial["posts"][0][key],
            "changed {key}"
        );
    }
    let channel = channels(&requests);
    assert_eq!(channel.len(), edits);
    assert!(channel
        .iter()
        .all(|r| r.method == "PATCH" && r.path == MESSAGE_PATH));
    if closed {
        assert_eq!(
            body(channel.last().expect("close refresh"))["components"],
            json!([])
        );
    }
}

#[tokio::test]
async fn increase_then_signup_serializes_through_refresh_and_admits_the_full_role() {
    contended(
        Operation::Seats(3),
        Operation::Signup("tank"),
        3,
        true,
        false,
        2,
    )
    .await;
}

#[tokio::test]
async fn signup_then_increase_preserves_the_concurrently_added_signup() {
    contended(
        Operation::Signup("dps"),
        Operation::Seats(3),
        3,
        true,
        false,
        2,
    )
    .await;
}

#[tokio::test]
async fn increase_then_close_keeps_capacity_and_signups_but_disables_controls() {
    contended(Operation::Seats(3), Operation::Close, 3, false, true, 2).await;
}

#[tokio::test]
async fn close_then_increase_refuses_without_reopening_or_refreshing_again() {
    contended(Operation::Close, Operation::Seats(3), 1, false, true, 1).await;
}

#[tokio::test]
async fn stale_lower_target_cannot_reduce_a_newer_committed_increase() {
    contended(Operation::Seats(5), Operation::Seats(3), 5, false, false, 1).await;
}

#[tokio::test]
async fn metadata_then_increase_uses_the_same_lock_and_preserves_saved_metadata() {
    contended(Operation::Metadata, Operation::Seats(3), 3, false, false, 2).await;
}

// This holder uses the STORE lock, not the distinct runtime/refresh lock above.
// Its committed state change happens only after observing a real blocked editor.
async fn store_lock_recheck(close_while_waiting: bool) {
    let (_serial, db) = database().await;
    seed(db.pool(), GUILD, "2099-09-11T20:00:00Z").await;
    let before = snapshot(db.pool()).await;
    let peer = db.independent_pool().await.expect("editor pool");
    let observer = db.independent_pool().await.expect("observer pool");
    let mut holder = db.pool().begin().await.expect("store-lock holder");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind("lfg:2222:seat-edit-fixture")
        .execute(&mut *holder)
        .await
        .expect("shared signup/close/store lock");
    sqlx::query("SELECT id FROM lfg_posts WHERE id = $1 FOR UPDATE")
        .bind(POST)
        .fetch_one(&mut *holder)
        .await
        .expect("post row lock after advisory lock");
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "5000"}))).await;
    let rt = Arc::new(runtime(peer.clone(), &mock, true));
    let worker = tokio::spawn({
        let rt = rt.clone();
        async move { rt.handle(&edit(21901, "tank", 3)).await }
    });
    let waiting = lock_waiter(&observer).await;
    let during = snapshot(&observer).await;
    if close_while_waiting {
        sqlx::query("UPDATE lfg_posts SET status = 'closed', closed_at = '2026-10-01T00:00:00Z' WHERE id = $1")
            .bind(POST).execute(&mut *holder).await.expect("fixture closure under the real store lock");
    } else {
        sqlx::query("UPDATE lfg_roles SET slots = 5 WHERE lfg_id = $1 AND role_key = 'tank'")
            .bind(POST)
            .execute(&mut *holder)
            .await
            .expect("fixture newer capacity under the real store lock");
    }
    holder.commit().await.expect("release store and row locks");
    let handled = tokio::time::timeout(Duration::from_secs(10), worker)
        .await
        .expect("editor completed")
        .expect("editor task")
        .expect("editor transport");
    let after = snapshot(&observer).await;
    let requests = mock.requests();
    mock.shutdown().await;
    drop(rt);
    peer.close().await;
    observer.close().await;
    db.close().await.expect("teardown");

    assert!(handled);
    assert!(
        waiting,
        "capacity edit did not wait on the signup/close STORE advisory lock"
    );
    assert_eq!(during, before, "editor mutated before store-lock admission");
    assert_eq!(after["signups"], before["signups"]);
    assert_eq!(after["versions"]["signups"], before["versions"]["signups"]);
    assert_eq!(
        after["audits"],
        json!([]),
        "refused contender must not audit a success"
    );
    assert!(
        channels(&requests).is_empty(),
        "refused contender refreshed stale state"
    );
    if close_while_waiting {
        assert_eq!(after["posts"][0]["status"], "closed");
        assert_eq!(after["roles"], before["roles"]);
        assert!(reply(&requests).to_lowercase().contains("closed"));
    } else {
        assert_eq!(
            after["roles"][0]["slots"], 5,
            "stale target reduced newer committed seats"
        );
        assert_eq!(after["posts"], before["posts"]);
        let text = reply(&requests).to_lowercase();
        assert!(
            text.contains("increase")
                || text.contains("smaller")
                || text.contains("shrink")
                || text.contains("lower")
                || text.contains("current"),
            "explain stale target, not an unrelated parse failure: {text}"
        );
    }
    safe_private(&requests);
}

#[tokio::test]
async fn store_lock_is_shared_with_signup_and_close_and_newer_capacity_is_rechecked() {
    store_lock_recheck(false).await;
}

#[tokio::test]
async fn eligibility_is_rechecked_after_a_close_commits_while_waiting_for_the_store_lock() {
    store_lock_recheck(true).await;
}

#[tokio::test]
async fn a_prior_authorized_edit_does_not_authorize_a_later_unprivileged_interaction() {
    let (_serial, db) = database().await;
    seed(db.pool(), GUILD, "2099-09-11T20:00:00Z").await;
    let mock = MockRest::start(vec![], ScriptedResponse::json(200, json!({"id": "5000"}))).await;
    let rt = runtime(db.pool().clone(), &mock, true);
    rt.handle(&edit(22001, "tank", 3))
        .await
        .expect("authorized edit");
    let saved = snapshot(db.pool()).await;
    let offset = mock.requests().len();
    rt.handle(&invocation(22002, "lfg-edit", options("tank", 5), 0))
        .await
        .expect("fresh authority refusal");
    let after = snapshot(db.pool()).await;
    let requests = mock.requests()[offset..].to_vec();
    mock.shutdown().await;
    drop(rt);
    db.close().await.expect("teardown");

    assert_eq!(
        saved["roles"][0]["slots"], 3,
        "positive authorization control must actually save"
    );
    assert_eq!(
        after, saved,
        "previous authorization leaked into a fresh request"
    );
    assert_eq!(reply(&requests), two_bot_core::MANAGE_EVENTS_REQUIRED);
    assert!(channels(&requests).is_empty());
    safe_private(&requests);
}
