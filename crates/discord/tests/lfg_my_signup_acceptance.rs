//! Protected RED acceptance: members inspect only their own saved LFG signup.
//!
//! Builder contract: incorporate this exact test commit, keep this file
//! immutable, implement GREEN separately, and ship tests with implementation.
//! No standalone failing-test PR. No application, staging or production DB.
//!
//! Dedicated wire identity: `two:lfg-mine:<post>`, button type 2, no values or
//! target-member parameter, at most 100 UTF-16 units. The existing `two:lfg:`
//! select and every formerly valid role key remain usable. New/open refreshed
//! messages offer exactly one enabled "My signup" button. Closed messages may
//! remove it, but a genuine retained button can still inspect closed history.
//!
//! Replies may be immediate ephemeral or deferred ephemeral + one original
//! response edit. They report the caller's role label, saved join instant,
//! open/closed state and start instant. Time may be Discord-localized or ISO.
//! Absence, unavailable post and failed lookup are different outcomes. Reads
//! never append mutation audits, refresh a channel, consume signup cooldown,
//! reopen a post, change a timestamp, or notify anybody else.
//!
//! Run (guarded disposable bootstrap required; no ignored-test routing):
//! `python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db --test lfg_my_signup_acceptance -- --test-threads=1`
//! Positive/negative baseline controls: `control_` filter. The help test uses
//! the real gate-filtered registry, not a source-string or frozen-copy oracle.
//! The signup view trap is test-only DDL in a uniquely owned database: an
//! unscoped roster fetch errors, while an own-row query continues to work.
#![cfg(feature = "db")]

#[allow(dead_code)]
mod common;

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use twilight_model::application::interaction::Interaction;
use two_bot_core::{commands::PERM_MANAGE_EVENTS, lfg, lfg_store as store, RouterGates};
use two_bot_discord::{
    interactions::InteractionRuntime, lfg_interactions::message_payload, ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

const GUILD: &str = "2222";
const CHANNEL: &str = "4444";
const MESSAGE: &str = "5000";
const BOT: u64 = 1111;
const A: u64 = 3333;
const B: u64 = 3334;
const POST: &str = "lfg-own-fixture";
const JOINED: &str = "2026-10-20T10:11:12Z";
const STARTS: &str = "2099-09-11T20:00:00Z";
const ROLES: &str = "tank:Own tank label:99,healer:Other private healer:99,__mine__:Legacy mine:99,__status__:Legacy status:99,my-signup:Legacy button word:99";

fn gates(enabled: bool) -> RouterGates {
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
    }
}

fn runtime(pool: sqlx::PgPool, mock: &MockRest, enabled: bool) -> InteractionRuntime {
    let rt = InteractionRuntime::new(
        gates(enabled),
        pool,
        ActionExecutor::with_proxy("lfg-fixture-token".into(), Some(mock.origin()))
            .expect("fixture executor"),
        BOT,
        two_bot_core::ClassifierConfig::default(),
    );
    rt.set_application_id(BOT);
    rt
}

fn wire(id: u64, member: u64, kind: u8, data: Value, message: bool) -> Interaction {
    let mut value = json!({
        "id": id.to_string(), "application_id": BOT.to_string(), "type": kind,
        "guild_id": GUILD, "channel": {"id": CHANNEL, "type": 0},
        "member": {"user": {"id": member.to_string(), "username": "fixture-member", "discriminator": "0", "avatar": null},
            "roles": [], "joined_at": null, "deaf": false, "mute": false, "flags": 0, "permissions": "0"},
        "token": format!("lfg-fixture-{id}"), "version": 1, "entitlements": [],
        "authorizing_integration_owners": {"0": GUILD}, "data": data
    });
    if message {
        value["message"] = json!({
            "id": MESSAGE, "guild_id": GUILD, "channel_id": CHANNEL, "type": 0,
            "author": {"id": BOT.to_string(), "username": "fixture-bot", "discriminator": "0", "avatar": null, "bot": true},
            "content": "Fixture raid", "timestamp": "2026-10-20T10:00:00+00:00",
            "edited_timestamp": null, "tts": false, "mention_everyone": false,
            "mentions": [], "mention_roles": [], "attachments": [], "embeds": [], "pinned": false,
            "components": [{"type": 1, "components": [{"type": 2, "style": 2,
                "label": "My signup", "custom_id": format!("two:lfg-mine:{POST}")}]}]
        });
    }
    serde_json::from_value(value).expect("complete synthetic interaction")
}

fn inspect(id: u64, member: u64) -> Interaction {
    wire(
        id,
        member,
        3,
        json!({"custom_id": format!("two:lfg-mine:{POST}"), "component_type": 2}),
        true,
    )
}

fn select(id: u64, member: u64, role: &str) -> Interaction {
    wire(
        id,
        member,
        3,
        json!({"custom_id": format!("two:lfg:{POST}"), "component_type": 3, "values": [role]}),
        true,
    )
}

fn command(id: u64, name: &str, options: Value) -> Interaction {
    let mut interaction = wire(
        id,
        A,
        2,
        json!({"id": "1", "name": name, "type": 1, "options": options}),
        false,
    );
    interaction
        .member
        .as_mut()
        .expect("fixture member")
        .permissions = Some(twilight_model::guild::Permissions::from_bits_truncate(
        PERM_MANAGE_EVENTS,
    ));
    interaction
}

fn rewrite(interaction: Interaction, change: impl FnOnce(&mut Value)) -> Interaction {
    let mut value = serde_json::to_value(interaction).expect("serialize fixture");
    change(&mut value);
    serde_json::from_value(value).expect("altered fixture remains valid wire data")
}

fn post() -> lfg::LfgPost {
    lfg::LfgPost {
        id: POST.into(),
        guild_id: GUILD.into(),
        channel_id: CHANNEL.into(),
        message_id: Some(MESSAGE.into()),
        title: "Fixture raid".into(),
        starts_at: STARTS.into(),
        status: lfg::LfgStatus::Open,
        created_by: A.to_string(),
        created_at: "2026-10-20T10:00:00Z".into(),
        closed_at: None,
    }
}

fn body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("captured Discord JSON")
}

async fn snapshot(pool: &sqlx::PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'posts', (SELECT coalesce(jsonb_agg(to_jsonb(p) ORDER BY p.id), '[]'::jsonb) FROM lfg_posts p),
            'roles', (SELECT coalesce(jsonb_agg(to_jsonb(r) ORDER BY r.lfg_id, r.position), '[]'::jsonb) FROM lfg_roles r),
            'signups', (SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY s.lfg_id, s.user_id), '[]'::jsonb) FROM lfg_signups s),
            'audits', (SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY a.id), '[]'::jsonb) FROM announcements_audit_log a),
            'privacy', (SELECT coalesce(jsonb_agg(to_jsonb(o) ORDER BY o.guild_id, o.member_id), '[]'::jsonb) FROM privacy_opt_outs o))",
    ).fetch_one(pool).await.expect("snapshot complete durable rows")
}

struct Observation {
    handled: bool,
    requests: Vec<RestRequest>,
    before: Value,
    after: Value,
}

struct Fixture {
    db: TestDatabase,
    rt: InteractionRuntime,
    mock: MockRest,
}

impl Fixture {
    async fn new() -> Self {
        let url =
            std::env::var("TWO_TEST_DATABASE_URL").expect("guarded disposable bootstrap required");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create guarded disposable fixture");
        let roles = lfg::spec_roles(
            POST,
            &lfg::parse_role_spec(ROLES).expect("valid fixture roles"),
        );
        store::put_lfg(db.pool(), &post(), &roles, true)
            .await
            .expect("seed real post");
        let mock = MockRest::with_responder(|request| {
            if request.path.ends_with("/callback") {
                ScriptedResponse::status(204)
            } else if request.method == "PATCH" && request.path.ends_with("/messages/@original") {
                ScriptedResponse::json(200, json!({"id": "6000"}))
            } else if request.path.starts_with("/api/v10/channels/")
                && matches!(request.method.as_str(), "POST" | "PATCH")
            {
                ScriptedResponse::json(200, json!({"id": MESSAGE}))
            } else {
                ScriptedResponse::status(404)
            }
        })
        .await;
        let rt = runtime(db.pool().clone(), &mock, true);
        Self { db, rt, mock }
    }

    async fn signup(&self, member: u64, role: &str, at: &str) {
        let result = store::signup_lfg(self.db.pool(), GUILD, POST, role, &member.to_string(), at)
            .await
            .expect("seed signup through real capacity store");
        assert!(matches!(
            result,
            lfg::SignupOutcome::Joined | lfg::SignupOutcome::Moved
        ));
    }

    async fn observe(&self, interaction: &Interaction) -> Observation {
        let before = snapshot(self.db.pool()).await;
        let offset = self.mock.requests().len();
        let handled = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            self.rt.handle(interaction),
        )
        .await
        .expect("component must finish within bounded fixture timeout")
        .expect("runtime transport succeeds against scripted Discord");
        let requests = self.mock.requests()[offset..].to_vec();
        let after = snapshot(self.db.pool()).await;
        Observation {
            handled,
            requests,
            before,
            after,
        }
    }

    async fn finish(self) {
        drop(self.rt);
        self.mock.shutdown().await;
        self.db
            .close()
            .await
            .expect("explicit disposable database teardown");
    }
}

fn private_reply(observation: &Observation) -> String {
    assert!(
        observation.handled,
        "My signup must be handled by the public interaction runtime"
    );
    private_payload(&observation.requests)["content"]
        .as_str()
        .expect("private summary text")
        .to_owned()
}

fn private_payload(requests: &[RestRequest]) -> Value {
    assert!(
        requests
            .iter()
            .all(|r| !r.path.starts_with("/api/v10/channels/")),
        "inspection must never refresh, fetch, or notify a channel: {requests:?}"
    );
    let callbacks: Vec<_> = requests
        .iter()
        .filter(|r| r.path.ends_with("/callback"))
        .collect();
    assert_eq!(callbacks.len(), 1, "exactly one initial acknowledgement");
    let initial = body(callbacks[0]);
    assert_eq!(
        initial["data"]["flags"].as_u64().unwrap_or(0) & 64,
        64,
        "summary is always private"
    );
    let reply = if initial["type"] == 4 {
        assert_eq!(
            requests.len(),
            1,
            "one immediate completion, no follow-up notification"
        );
        initial["data"].clone()
    } else {
        assert_eq!(
            initial["type"], 5,
            "only ephemeral reply or ephemeral deferral is allowed"
        );
        assert_eq!(
            requests.len(),
            2,
            "one deferral plus exactly one completion"
        );
        let edit = &requests[1];
        assert_eq!(edit.method, "PATCH");
        assert!(
            edit.path.ends_with("/messages/@original"),
            "complete the original private response"
        );
        body(edit)
    };
    assert_eq!(reply["allowed_mentions"]["parse"], json!([]));
    for field in ["users", "roles"] {
        assert!(
            reply["allowed_mentions"][field].is_null()
                || reply["allowed_mentions"][field] == json!([])
        );
    }
    let text = reply["content"].as_str().expect("bounded text summary");
    assert!(!text.is_empty() && text.encode_utf16().count() <= 2000);
    for mention in ["@everyone", "@here", "<@", "<@&"] {
        assert!(
            !text.contains(mention),
            "summary must neutralize mention syntax"
        );
    }
    assert!(
        reply["embeds"].is_null() || reply["embeds"] == json!([]),
        "no roster hidden in embeds"
    );
    reply
}

fn unchanged(observation: &Observation) {
    assert_eq!(
        observation.after, observation.before,
        "inspection changed a signup, timestamp, post, audit, role or privacy row"
    );
}

fn contains_instant(text: &str, iso: &str) -> bool {
    let instant = time::OffsetDateTime::parse(iso, &time::format_description::well_known::Rfc3339)
        .expect("literal fixture instant");
    text.contains(&format!("<t:{}:", instant.unix_timestamp()))
        || text.contains(iso.trim_end_matches('Z'))
}

fn saved_summary(observation: &Observation, role: &str, joined: &str, state: &str) -> String {
    unchanged(observation);
    let text = private_reply(observation);
    assert!(
        text.contains(role),
        "summary must use the caller's stored role label, got {text:?}"
    );
    assert!(
        contains_instant(&text, joined),
        "summary must include the saved joined instant: {text:?}"
    );
    assert!(
        contains_instant(&text, STARTS),
        "summary must include the post start instant: {text:?}"
    );
    assert!(
        text.to_lowercase().contains(state),
        "summary must report current post state: {text:?}"
    );
    assert!(
        !text.contains("Other private healer"),
        "another member's role leaked"
    );
    assert!(
        !text.contains(&B.to_string()),
        "another member's identity leaked"
    );
    text
}

fn absent_summary(observation: &Observation) -> String {
    unchanged(observation);
    let text = private_reply(observation);
    assert!(
        text.to_lowercase().contains("not signed up"),
        "absence must be explicit: {text:?}"
    );
    assert!(!text.contains("Other private healer"));
    assert!(!text.contains("Own tank label"));
    assert!(!text.contains(&B.to_string()));
    text
}

fn buttons(payload: &Value) -> Vec<&Value> {
    payload["components"]
        .as_array()
        .expect("component rows")
        .iter()
        .flat_map(|row| row["components"].as_array().expect("row children"))
        .filter(|component| component["type"] == 2 && component["label"] == "My signup")
        .collect()
}

fn assert_button(payload: &Value, id: &str) -> String {
    let buttons = buttons(payload);
    assert_eq!(
        buttons.len(),
        1,
        "every new/refreshed open post offers one My signup button"
    );
    let button = buttons[0];
    assert_ne!(button["disabled"], true);
    let custom_id = button["custom_id"]
        .as_str()
        .expect("dedicated button identity");
    assert_eq!(custom_id, format!("two:lfg-mine:{id}"));
    assert!(!custom_id.is_empty() && custom_id.encode_utf16().count() <= 100);
    assert!(button["values"].is_null());
    custom_id.to_owned()
}

#[test]
fn open_render_offers_a_dedicated_button_without_reserving_select_values() {
    let post = post();
    let roles = lfg::spec_roles(
        POST,
        &lfg::parse_role_spec(ROLES).expect("legacy valid roles"),
    );
    let payload = message_payload(&post, &roles, &[]);
    assert_button(&payload, POST);
    let select = &payload["components"][0]["components"][0];
    assert_eq!(select["type"], 3);
    let values: Vec<_> = select["options"]
        .as_array()
        .expect("existing select options")
        .iter()
        .map(|option| option["value"].as_str().expect("role key"))
        .collect();
    assert_eq!(
        values,
        [
            "tank",
            "healer",
            "__mine__",
            "__status__",
            "my-signup",
            "__leave__"
        ]
    );
}

#[tokio::test]
async fn newly_created_and_normally_refreshed_posts_offer_a_working_button() {
    let fixture = Fixture::new().await;
    let create = command(
        8001,
        "lfg",
        json!([
            {"type": 3, "name": "title", "value": "New fixture raid"},
            {"type": 3, "name": "starts-at", "value": STARTS},
            {"type": 3, "name": "roles", "value": ROLES}
        ]),
    );
    let created = fixture.observe(&create).await;
    let refreshed = fixture.observe(&select(8002, A, "tank")).await;
    let mut pressed = inspect(8003, A);
    // The synthetic component is the literal independently specified wire ID.
    // The assertions below also compare that ID to both actual sent payloads.
    pressed.message.as_mut().expect("fixture message").content = "New fixture raid".into();
    let read = fixture.observe(&pressed).await;
    fixture.finish().await;
    assert!(created.handled && refreshed.handled);
    let create_payload = created
        .requests
        .iter()
        .find(|r| r.method == "POST" && r.path.starts_with("/api/v10/channels/"))
        .map(body)
        .expect("actual create REST payload");
    assert_button(&create_payload, "lfg-8001");
    let refresh_payload = refreshed
        .requests
        .iter()
        .find(|r| r.method == "PATCH" && r.path.starts_with("/api/v10/channels/"))
        .map(body)
        .expect("actual refresh REST payload");
    assert_button(&refresh_payload, POST);
    unchanged(&read);
    assert!(private_reply(&read).contains("Own tank label"));
}

#[tokio::test]
async fn joined_summary_is_private_own_only_and_does_not_change_saved_rows() {
    let fixture = Fixture::new().await;
    fixture.signup(A, "tank", JOINED).await;
    fixture.signup(B, "healer", "2026-10-20T12:13:14Z").await;
    let read = fixture.observe(&inspect(8101, A)).await;
    fixture.finish().await;
    saved_summary(&read, "Own tank label", JOINED, "open");
}

#[tokio::test]
async fn member_b_sees_their_own_row_not_member_as_role_or_joined_time() {
    let fixture = Fixture::new().await;
    fixture.signup(A, "tank", JOINED).await;
    fixture.signup(B, "healer", "2026-10-22T12:13:14Z").await;
    let read = fixture.observe(&inspect(8103, B)).await;
    fixture.finish().await;
    unchanged(&read);
    let text = private_reply(&read);
    assert!(text.contains("Other private healer"));
    assert!(!text.contains("Own tank label") && !contains_instant(&text, JOINED));
    assert!(contains_instant(&text, "2026-10-22T12:13:14Z"));
    assert!(contains_instant(&text, STARTS));
}

#[tokio::test]
async fn another_members_saved_signup_is_not_the_callers_absence_or_response() {
    let fixture = Fixture::new().await;
    fixture.signup(B, "healer", JOINED).await;
    let read = fixture.observe(&inspect(8102, A)).await;
    fixture.finish().await;
    absent_summary(&read);
}

#[tokio::test]
async fn moved_and_left_states_are_read_from_storage_not_old_component_content() {
    let fixture = Fixture::new().await;
    fixture.signup(A, "__mine__", JOINED).await;
    let old = fixture.observe(&inspect(8110, A)).await;
    fixture
        .signup(A, "__status__", "2026-10-21T10:11:12Z")
        .await;
    let moved = fixture.observe(&inspect(8111, A)).await;
    store::leave_lfg(fixture.db.pool(), POST, &A.to_string())
        .await
        .expect("seed subsequent leave");
    let left = fixture.observe(&inspect(8112, A)).await;
    fixture.finish().await;
    saved_summary(&old, "Legacy mine", JOINED, "open");
    saved_summary(&moved, "Legacy status", "2026-10-21T10:11:12Z", "open");
    absent_summary(&left);
}

#[tokio::test]
async fn stale_button_reads_retained_closed_signup_without_reopening() {
    let fixture = Fixture::new().await;
    fixture.signup(A, "tank", JOINED).await;
    store::close_lfg(fixture.db.pool(), GUILD, POST, "2026-10-21T00:00:00Z")
        .await
        .expect("close fixture");
    let read = fixture.observe(&inspect(8120, A)).await;
    fixture.finish().await;
    saved_summary(&read, "Own tank label", JOINED, "closed");
    assert_eq!(read.after["posts"][0]["status"], "closed");
}

#[tokio::test]
async fn deleted_post_is_unavailable_not_not_signed_up() {
    let fixture = Fixture::new().await;
    store::delete_lfg(fixture.db.pool(), GUILD, POST)
        .await
        .expect("delete owned fixture post");
    let read = fixture.observe(&inspect(8130, A)).await;
    fixture.finish().await;
    unchanged(&read);
    let text = private_reply(&read).to_lowercase();
    assert!(
        text.contains("unavailable") || text.contains("no longer available"),
        "deleted post needs a distinct unavailable outcome: {text}"
    );
    assert!(!text.contains("not signed up"));
}

#[tokio::test]
async fn closed_pool_failure_is_honest_and_never_rendered_as_absence() {
    let mut fixture = Fixture::new().await;
    fixture.signup(A, "tank", JOINED).await;
    let closed = fixture
        .db
        .independent_pool()
        .await
        .expect("independent fixture pool");
    closed.close().await;
    fixture.rt = runtime(closed, &fixture.mock, true);
    let read = fixture.observe(&inspect(8140, A)).await;
    fixture.finish().await;
    unchanged(&read);
    let text = private_reply(&read).to_lowercase();
    assert!(
        text.contains("failed") || text.contains("could not") || text.contains("unable"),
        "store outage must be explicit: {text}"
    );
    assert!(!text.contains("not signed up") && !text.contains("no longer available"));
    for sensitive in ["postgres", "sqlx", "agent_test", "lfg_signups"] {
        assert!(!text.contains(sensitive));
    }
}

#[tokio::test]
async fn hostile_role_label_is_bounded_and_mention_safe_in_the_private_completion() {
    let fixture = Fixture::new().await;
    sqlx::query("UPDATE lfg_roles SET label = $1 WHERE lfg_id = $2 AND role_key = 'tank'")
        .bind("Saved 🦀 @everyone @here <@3334> <@&9999>")
        .bind(POST)
        .execute(fixture.db.pool())
        .await
        .expect("seed hostile label");
    fixture.signup(A, "tank", JOINED).await;
    let read = fixture.observe(&inspect(8150, A)).await;
    fixture.finish().await;
    unchanged(&read);
    let text = private_reply(&read);
    assert!(text.contains("Saved 🦀"));
    assert!(contains_instant(&text, JOINED) && contains_instant(&text, STARTS));
}

#[tokio::test]
async fn large_roster_omits_caller_from_public_suffix_but_private_lookup_finds_them() {
    let fixture = Fixture::new().await;
    // 240 earlier signups, within the five roles' aggregate capacity. The
    // caller's fifth-role row lies beyond the public message's 2000-unit cap.
    sqlx::query("INSERT INTO lfg_signups (lfg_id, user_id, role_key, joined_at)
        SELECT $1, (100000000000000000 + n)::text,
        CASE WHEN n < 80 THEN 'tank' WHEN n < 160 THEN 'healer' ELSE '__mine__' END,
        '2026-10-19T00:00:00Z'::timestamptz + n * interval '1 second' FROM generate_series(0,239) n")
        .bind(POST).execute(fixture.db.pool()).await.expect("seed bounded large roster");
    fixture.signup(A, "my-signup", JOINED).await;
    let roles = store::list_lfg_roles(fixture.db.pool(), POST)
        .await
        .expect("roles");
    let signups = store::list_lfg_signups(fixture.db.pool(), POST)
        .await
        .expect("roster fixture");
    let rendered = message_payload(&post(), &roles, &signups);
    let read = fixture.observe(&inspect(8160, A)).await;
    fixture.finish().await;
    assert_eq!(signups.len(), 241);
    assert!(
        !rendered["content"]
            .as_str()
            .expect("public render")
            .contains("3333"),
        "non-vacuous truncated public roster fixture"
    );
    saved_summary(&read, "Legacy button word", JOINED, "open");
}

#[tokio::test]
async fn lookup_does_not_fetch_another_members_signup_even_when_the_roster_is_unreadable() {
    let fixture = Fixture::new().await;
    fixture.signup(A, "tank", JOINED).await;
    fixture.signup(B, "healer", JOINED).await;
    let before = snapshot(fixture.db.pool()).await;
    // A test-only view traps a full-roster projection. It neither filters nor
    // fabricates rows: own-row WHERE queries work; fetching B's role raises.
    sqlx::raw_sql("ALTER TABLE lfg_signups RENAME TO acceptance_signups;
        CREATE FUNCTION acceptance_forbid_other_signup() RETURNS text LANGUAGE plpgsql VOLATILE AS $$
        BEGIN RAISE EXCEPTION 'acceptance: another member row was fetched'; END $$;
        CREATE VIEW lfg_signups AS SELECT lfg_id, user_id,
        CASE WHEN user_id = '3333' THEN role_key ELSE acceptance_forbid_other_signup() END AS role_key,
        joined_at FROM acceptance_signups;")
        .execute(fixture.db.pool()).await.expect("arm own-row projection trap in owned DB");
    let trapped = store::list_lfg_signups(fixture.db.pool(), POST)
        .await
        .is_err();
    let own: String =
        sqlx::query_scalar("SELECT role_key FROM lfg_signups WHERE lfg_id = $1 AND user_id = $2")
            .bind(POST)
            .bind(A.to_string())
            .fetch_one(fixture.db.pool())
            .await
            .expect("own-row query remains readable");
    let offset = fixture.mock.requests().len();
    let handled = fixture
        .rt
        .handle(&inspect(8170, A))
        .await
        .expect("real runtime with view trap");
    let requests = fixture.mock.requests()[offset..].to_vec();
    // Restore only fixture DDL before taking the all-row state snapshot.
    sqlx::raw_sql("DROP VIEW lfg_signups; ALTER TABLE acceptance_signups RENAME TO lfg_signups;")
        .execute(fixture.db.pool())
        .await
        .expect("restore owned fixture table");
    let after = snapshot(fixture.db.pool()).await;
    fixture.finish().await;
    assert!(
        trapped,
        "negative control: full roster read must hit the trap"
    );
    assert_eq!(
        own, "tank",
        "positive control: exact own row remains available"
    );
    assert!(handled, "own-row button is registered");
    let text = private_payload(&requests)["content"]
        .as_str()
        .expect("summary")
        .to_owned();
    assert!(
        text.contains("Own tank label"),
        "inspection must not depend on reading another row: {text:?}"
    );
    assert_eq!(
        after, before,
        "the scoped read must preserve every durable row"
    );
}

#[tokio::test]
async fn read_only_database_session_can_complete_inspection() {
    let mut fixture = Fixture::new().await;
    fixture.signup(A, "tank", JOINED).await;
    // Pool connections inherit this setting on checkout. All work remains in
    // this test's unique database; no grants, users or global settings change.
    let read_pool = fixture
        .db
        .pool_with_search_path("public")
        .await
        .expect("fresh fixture pool");
    let mut held = Vec::new();
    for _ in 0..5 {
        let mut connection = read_pool
            .acquire()
            .await
            .expect("hold distinct pool connection");
        sqlx::query("SET default_transaction_read_only = on")
            .execute(&mut *connection)
            .await
            .expect("read-only session");
        held.push(connection);
    }
    drop(held);
    fixture.rt = runtime(read_pool.clone(), &fixture.mock, true);
    let read = fixture.observe(&inspect(8180, A)).await;
    fixture.rt = runtime(fixture.db.pool().clone(), &fixture.mock, true);
    read_pool.close().await;
    fixture.finish().await;
    saved_summary(&read, "Own tank label", JOINED, "open");
}

#[tokio::test]
async fn analytics_opt_out_does_not_erase_transactional_signup_or_leak_other_rows() {
    let fixture = Fixture::new().await;
    fixture.signup(A, "tank", JOINED).await;
    sqlx::query("INSERT INTO privacy_opt_outs (guild_id, member_id) VALUES ($1, $2)")
        .bind(GUILD)
        .bind(A.to_string())
        .execute(fixture.db.pool())
        .await
        .expect("opt-out fixture");
    let read = fixture.observe(&inspect(8190, A)).await;
    store::leave_lfg(fixture.db.pool(), POST, &A.to_string())
        .await
        .expect("fixture simulates erased signup row");
    fixture.signup(B, "healer", JOINED).await;
    let erased = fixture.observe(&inspect(8191, A)).await;
    fixture.finish().await;
    saved_summary(&read, "Own tank label", JOINED, "open");
    absent_summary(&erased);
}

#[tokio::test]
async fn repeated_reads_ignore_and_do_not_consume_the_signup_cooldown() {
    let fixture = Fixture::new().await;
    let reads = [
        fixture.observe(&inspect(8200, A)).await,
        fixture.observe(&inspect(8201, A)).await,
        fixture.observe(&inspect(8202, A)).await,
    ];
    let joined = fixture.observe(&select(8203, A, "__mine__")).await;
    let during_cooldown = fixture.observe(&inspect(8204, A)).await;
    let leave = fixture.observe(&select(8205, A, "__leave__")).await;
    fixture.finish().await;
    for read in &reads {
        absent_summary(read);
    }
    assert!(joined.handled);
    assert_eq!(
        private_payload(
            &joined
                .requests
                .iter()
                .filter(|r| !r.path.starts_with("/api/v10/channels/"))
                .cloned()
                .collect::<Vec<_>>()
        )["content"],
        "LFG joined.",
        "reads must not consume signup admission"
    );
    unchanged(&during_cooldown);
    assert!(
        private_reply(&during_cooldown).contains("Legacy mine"),
        "a recent signup must not cooldown read-only inspection"
    );
    assert!(leave.handled);
    assert!(
        leave.after["signups"].as_array().expect("rows").is_empty(),
        "leave remains usable after reads"
    );
}

#[tokio::test]
async fn forged_message_channel_author_and_target_member_are_refused_without_effects() {
    let fixture = Fixture::new().await;
    fixture.signup(A, "tank", JOINED).await;
    fixture.signup(B, "healer", JOINED).await;
    let invalid = [
        rewrite(inspect(8300, A), |v| v["message"]["id"] = json!("5001")),
        rewrite(inspect(8301, A), |v| {
            v["message"]["channel_id"] = json!("4445");
            v["channel"]["id"] = json!("4445");
        }),
        rewrite(inspect(8302, A), |v| {
            v["message"]["author"]["id"] = json!("9999")
        }),
        rewrite(inspect(8303, A), |v| {
            v["message"]["author"]["bot"] = json!(false);
        }),
        rewrite(inspect(8304, A), |v| {
            v.as_object_mut().expect("object").remove("message");
        }),
        rewrite(inspect(8305, A), |v| {
            v["data"]["custom_id"] = json!(format!("two:lfg-mine:{POST}:3334"))
        }),
        rewrite(inspect(8306, A), |v| {
            v["data"]["component_type"] = json!(3);
            v["data"]["values"] = json!(["3334"]);
        }),
    ];
    let mut observations = Vec::new();
    for interaction in invalid {
        observations.push(fixture.observe(&interaction).await);
    }
    let genuine = fixture.observe(&inspect(8307, A)).await;
    fixture.finish().await;
    for read in observations {
        unchanged(&read);
        if read.handled {
            let text = private_reply(&read);
            assert!(!text.contains("Own tank label") && !text.contains("Other private healer"));
            assert!(!contains_instant(&text, JOINED));
        } else {
            assert!(read.requests.is_empty());
        }
    }
    saved_summary(&genuine, "Own tank label", JOINED, "open");
}

#[tokio::test]
async fn guild_application_feature_and_actor_fences_precede_own_row_disclosure() {
    let mut fixture = Fixture::new().await;
    fixture.signup(A, "tank", JOINED).await;
    let foreign = rewrite(inspect(8400, A), |v| v["guild_id"] = json!("9999"));
    let dm = rewrite(inspect(8401, A), |v| {
        v.as_object_mut().expect("object").remove("guild_id");
    });
    let application = rewrite(inspect(8402, A), |v| v["application_id"] = json!("9999"));
    let no_actor = rewrite(inspect(8403, A), |v| {
        v.as_object_mut().expect("object").remove("member");
    });
    let mut refusals = Vec::new();
    for interaction in [foreign, dm, application, no_actor] {
        refusals.push(fixture.observe(&interaction).await);
    }
    fixture.rt = runtime(fixture.db.pool().clone(), &fixture.mock, false);
    refusals.push(fixture.observe(&inspect(8404, A)).await);
    fixture.rt = runtime(fixture.db.pool().clone(), &fixture.mock, true);
    let valid = fixture.observe(&inspect(8405, A)).await;
    fixture.finish().await;
    for read in refusals {
        unchanged(&read);
        if read.handled {
            let text = private_reply(&read);
            assert!(!text.contains("Own tank label") && !contains_instant(&text, JOINED));
        } else {
            assert!(read.requests.is_empty());
        }
    }
    saved_summary(&valid, "Own tank label", JOINED, "open");
}

#[tokio::test]
async fn stored_guild_and_post_identity_cannot_be_substituted_by_the_button() {
    let fixture = Fixture::new().await;
    fixture.signup(A, "tank", JOINED).await;
    sqlx::query("UPDATE lfg_posts SET guild_id = '9999' WHERE id = $1")
        .bind(POST)
        .execute(fixture.db.pool())
        .await
        .expect("foreign-owned post fixture");
    let foreign = fixture.observe(&inspect(8410, A)).await;
    sqlx::query("UPDATE lfg_posts SET guild_id = $1, message_id = NULL WHERE id = $2")
        .bind(GUILD)
        .bind(POST)
        .execute(fixture.db.pool())
        .await
        .expect("unaccepted post fixture");
    let unaccepted = fixture.observe(&inspect(8411, A)).await;
    fixture.finish().await;
    for read in [foreign, unaccepted] {
        unchanged(&read);
        let text = private_reply(&read);
        assert!(!text.contains("Own tank label") && !contains_instant(&text, JOINED));
        assert!(!text.to_lowercase().contains("not signed up"));
    }
}

#[tokio::test]
async fn malformed_and_overlong_button_ids_never_disclose_or_mutate_saved_state() {
    let fixture = Fixture::new().await;
    fixture.signup(A, "tank", JOINED).await;
    // Real matching rows make the length refusal non-vacuous: without the
    // component bound these IDs would disclose an otherwise valid own signup.
    for id in ["x".repeat(101), "🦀".repeat(51)] {
        let mut long_post = post();
        long_post.id = id.clone();
        let roles = lfg::spec_roles(&id, &lfg::parse_role_spec(ROLES).expect("roles"));
        store::put_lfg(fixture.db.pool(), &long_post, &roles, true)
            .await
            .expect("legacy long post");
        store::signup_lfg(
            fixture.db.pool(),
            GUILD,
            &id,
            "tank",
            &A.to_string(),
            JOINED,
        )
        .await
        .expect("real signup behind overlong component identity");
    }
    let ids = [
        "two:lfg-mine:".to_owned(),
        format!("two:lfg-mine:{}", "x".repeat(101)),
        format!("two:lfg-mine:{}", "🦀".repeat(51)),
        "two:lfg-mine:missing-post".to_owned(),
    ];
    let mut observations = Vec::new();
    for (index, id) in ids.into_iter().enumerate() {
        let interaction = rewrite(inspect(8420 + index as u64, A), |v| {
            v["data"]["custom_id"] = json!(id)
        });
        observations.push(fixture.observe(&interaction).await);
    }
    let genuine = fixture.observe(&inspect(8430, A)).await;
    fixture.finish().await;
    for read in observations {
        unchanged(&read);
        if read.handled {
            let text = private_reply(&read);
            assert!(!text.contains("Own tank label") && !contains_instant(&text, JOINED));
        } else {
            assert!(read.requests.is_empty());
        }
    }
    saved_summary(&genuine, "Own tank label", JOINED, "open");
}

#[tokio::test]
async fn concurrent_close_and_inspection_yield_coherent_old_or_new_read_not_a_mutation() {
    let fixture = Fixture::new().await;
    fixture.signup(A, "tank", JOINED).await;
    let before = snapshot(fixture.db.pool()).await;
    let read_interaction = inspect(8500, A);
    let close_interaction = command(
        8501,
        "lfg-close",
        json!([{ "type": 3, "name": "id", "value": POST }]),
    );
    let close_pool = fixture
        .db
        .independent_pool()
        .await
        .expect("second worker pool");
    let close_runtime = runtime(close_pool.clone(), &fixture.mock, true);
    let (read_result, close_result) = tokio::join!(
        fixture.rt.handle(&read_interaction),
        close_runtime.handle(&close_interaction)
    );
    let after = snapshot(fixture.db.pool()).await;
    let requests = fixture.mock.requests();
    drop(close_runtime);
    close_pool.close().await;
    fixture.finish().await;
    assert!(
        read_result.expect("read transport"),
        "concurrent read must be handled"
    );
    assert!(close_result.expect("close transport"));
    let own_requests: Vec<_> = requests
        .iter()
        .filter(|r| r.path.contains("lfg-fixture-8500"))
        .cloned()
        .collect();
    let text = private_payload(&own_requests)["content"]
        .as_str()
        .expect("read summary")
        .to_owned();
    assert!(
        text.contains("Own tank label")
            && contains_instant(&text, JOINED)
            && contains_instant(&text, STARTS)
    );
    let lower = text.to_lowercase();
    assert_ne!(
        lower.contains("open"),
        lower.contains("closed"),
        "coherent old-or-new status, never both/neither"
    );
    assert_eq!(after["posts"][0]["status"], "closed");
    assert_eq!(after["signups"], before["signups"]);
    assert_eq!(after["roles"], before["roles"]);
    assert_eq!(
        after["audits"].as_array().expect("audits").len(),
        1,
        "only close may audit"
    );
    assert_eq!(after["audits"][0]["action"], "lfg.close");
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.path.starts_with("/api/v10/channels/"))
            .count(),
        1,
        "only close refreshes"
    );
}

#[tokio::test]
async fn control_existing_select_reserved_looking_role_leave_and_close_still_work() {
    let fixture = Fixture::new().await;
    let joined = fixture.observe(&select(8600, A, "__mine__")).await;
    let left = fixture.observe(&select(8601, A, "__leave__")).await;
    let closed = fixture
        .observe(&command(
            8602,
            "lfg-close",
            json!([{ "type": 3, "name": "id", "value": POST }]),
        ))
        .await;
    fixture.finish().await;
    assert!(joined.handled && left.handled && closed.handled);
    assert_eq!(joined.after["signups"][0]["role_key"], "__mine__");
    assert!(left.after["signups"].as_array().expect("rows").is_empty());
    assert_eq!(closed.after["posts"][0]["status"], "closed");
    assert_eq!(closed.after["audits"].as_array().expect("audits").len(), 3);
}

#[tokio::test]
async fn control_existing_permission_and_feature_refusals_preserve_all_rows() {
    let mut fixture = Fixture::new().await;
    let unprivileged = wire(
        8610,
        A,
        2,
        json!({"id": "1", "name": "lfg-close", "type": 1,
        "options": [{"type": 3, "name": "id", "value": POST}]}),
        false,
    );
    let permission = fixture.observe(&unprivileged).await;
    fixture.rt = runtime(fixture.db.pool().clone(), &fixture.mock, false);
    let disabled = fixture
        .observe(&command(
            8611,
            "lfg-close",
            json!([{ "type": 3, "name": "id", "value": POST }]),
        ))
        .await;
    fixture.finish().await;
    for read in [permission, disabled] {
        unchanged(&read);
        assert!(read.handled);
        private_payload(&read.requests);
    }
}

#[test]
fn member_help_discovers_private_inspection_without_adding_a_target_member_command() {
    let router = two_bot_core::InteractionRouter::new(gates(true));
    let definitions = router
        .publish_set(&[])
        .expect("valid gate-filtered registry");
    let help = two_bot_core::help_text(&definitions);
    assert!(
        help.contains("My signup"),
        "member-facing help must explain the new message button"
    );
    assert!(
        help.to_lowercase().contains("private"),
        "help must state private visibility"
    );
    let lfg = definitions
        .iter()
        .find(|definition| definition.name == "lfg")
        .expect("existing lfg command");
    assert_eq!(
        lfg.options
            .iter()
            .map(|option| option.name.as_str())
            .collect::<Vec<_>>(),
        ["title", "starts-at", "roles"]
    );
}
