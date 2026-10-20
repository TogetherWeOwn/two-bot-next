//! Protected behavioral acceptance for current dispatch authority.
//! Runs through the real command router, member runtime, REST executor and
//! disposable ledger. The interaction deliberately contains stale permission
//! and resolved-target evidence. No live Discord or application database.
//!
//! CI already executes this child of command_runtime_tests in its ignored
//! runtime lane. Do not move it into an unselected standalone ignored target.

use super::*;
use serde_json::{json, Value};
use tokio::sync::Notify;
use two_bot_core::member_moderation::MemberModerationStore;
use two_bot_testsupport::TestDatabase;

const ACTOR: u64 = 88;
const TARGET: u64 = 3333;
const BOT: u64 = 4444;
const MODERATOR_ROLE: &str = "101";
const HIGH_ROLE: &str = "102";
const BOT_ROLE: &str = "103";
const REMOTE_MARKER: &str = "untrusted-authority-response-fixture";
const VERBS: [&str; 5] = ["ban", "tempban", "kick", "timeout", "warn"];

#[derive(Clone)]
struct Facts {
    guild: Value,
    actor: Value,
    target: Value,
    bot: Value,
    fault: Option<(&'static str, u16)>,
    delay_actor: bool,
    mutation_status: u16,
}

fn member(id: u64, roles: &[&str], bot: bool) -> Value {
    json!({
        "user": user(id, bot), "roles": roles, "deaf": false, "mute": false,
        "flags": 0, "pending": false, "joined_at": "2026-10-01T00:00:00Z",
    })
}

impl Default for Facts {
    fn default() -> Self {
        let permissions =
            (Permissions::BAN_MEMBERS | Permissions::KICK_MEMBERS | Permissions::MODERATE_MEMBERS)
                .bits()
                .to_string();
        Self {
            guild: json!({
                "id": GUILD_S, "owner_id": "5555",
                "roles": [
                    {"id": GUILD_S, "position": 0, "permissions": "0"},
                    {"id": MODERATOR_ROLE, "position": 50, "permissions": permissions},
                    {"id": HIGH_ROLE, "position": 60, "permissions": "0"},
                    {"id": BOT_ROLE, "position": 100, "permissions": Permissions::all().bits().to_string()},
                ],
            }),
            actor: member(ACTOR, &[MODERATOR_ROLE], false),
            target: member(TARGET, &[], false),
            bot: member(BOT, &[BOT_ROLE], true),
            fault: None,
            delay_actor: false,
            mutation_status: 204,
        }
    }
}

impl Facts {
    fn respond(&self, method: &str, path: &str) -> RestResponse {
        let json_response = |body: Value| RestResponse {
            status: 200,
            body: Some(body.to_string()),
            delay: Duration::ZERO,
        };
        if method == "GET" {
            if let Some((suffix, status)) = self.fault {
                if path.ends_with(suffix) {
                    return RestResponse {
                        status,
                        body: Some(json!({"code": 10007, "message": REMOTE_MARKER}).to_string()),
                        delay: Duration::ZERO,
                    };
                }
            }
            if path.ends_with("/users/@me") {
                return json_response(serde_json::to_value(user(BOT, true)).expect("bot identity"));
            }
            if path == format!("/api/v10/guilds/{GUILD_S}") {
                return json_response(self.guild.clone());
            }
            if path.ends_with("/roles") {
                return json_response(self.guild["roles"].clone());
            }
            for (id, body) in [
                (ACTOR, &self.actor),
                (TARGET, &self.target),
                (BOT, &self.bot),
            ] {
                if path.ends_with(&format!("/members/{id}")) {
                    let mut response = json_response(body.clone());
                    if id == ACTOR && self.delay_actor {
                        response.delay = Duration::from_secs(6);
                    }
                    return response;
                }
            }
            return RestResponse::status(404);
        }
        if path.starts_with("/api/v10/guilds/") {
            return RestResponse {
                status: if self.mutation_status == 204 && method == "PATCH" {
                    200
                } else {
                    self.mutation_status
                },
                body: Some(self.target.to_string()),
                delay: Duration::ZERO,
            };
        }
        json_response(json!({"id": "99"}))
    }
}

/// Reuse the existing wire parser and request/response types, but select
/// responses by route and current state rather than a brittle request index.
struct AuthorityRest {
    facts: Arc<Mutex<Facts>>,
    requests: Arc<Mutex<Vec<RestRequest>>>,
    origin: String,
    task: JoinHandle<()>,
}

impl AuthorityRest {
    async fn start(facts: Facts) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock listener");
        let origin = format!("http://{}", listener.local_addr().expect("mock address"));
        let facts = Arc::new(Mutex::new(facts));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (state, recorded) = (facts.clone(), requests.clone());
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (state, recorded) = (state.clone(), recorded.clone());
                tokio::spawn(async move {
                    let Some((method, path, body)) = read_rest_request(&mut stream).await else {
                        return;
                    };
                    let response = state
                        .lock()
                        .expect("authority facts")
                        .respond(&method, &path);
                    recorded.lock().expect("requests").push(RestRequest {
                        method,
                        path,
                        body,
                        received_at: std::time::Instant::now(),
                    });
                    tokio::time::sleep(response.delay).await;
                    let body = response.body.unwrap_or_default();
                    let header = format!(
                        "HTTP/1.1 {} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        response.status,
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(body.as_bytes()).await;
                });
            }
        });
        Self {
            facts,
            requests,
            origin,
            task,
        }
    }

    fn requests(&self) -> Vec<RestRequest> {
        self.requests.lock().expect("recorded requests").clone()
    }

    fn change(&self, update: impl FnOnce(&mut Facts)) {
        update(&mut self.facts.lock().expect("authority facts"));
    }
}

impl Drop for AuthorityRest {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn database() -> TestDatabase {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("explicit disposable test bootstrap");
    TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("authorized disposable database, never a credential fallback")
}

fn runtime(pool: &PgPool, rest: &AuthorityRest) -> (Arc<CommandRuntime>, Arc<MemberRuntime>) {
    let member = MemberRuntime::for_test(pool.clone(), GUILD_S);
    let executor =
        ActionExecutor::with_proxy("fixture-only-token".into(), Some(rest.origin.clone()))
            .expect("loopback executor");
    let runtime = CommandRuntime::new_with_member(
        pool.clone(),
        executor,
        router_with_commands(member_gates()),
        GUILD,
        true,
        member.clone(),
    );
    (runtime, member)
}

fn stale_interaction(verb: &str, id: u64) -> Interaction {
    let mut interaction = member_slash(verb);
    interaction.id = Id::new(id);
    interaction.member.as_mut().expect("actor").roles = vec![Id::new(101)];
    if matches!(verb, "tempban" | "timeout") {
        let Some(InteractionData::ApplicationCommand(data)) = &mut interaction.data else {
            panic!("command fixture")
        };
        data.options
            .push(option("duration_seconds", CommandOptionValue::Integer(60)));
    }
    let target_member = twilight_model::application::interaction::InteractionMember {
        avatar: None,
        avatar_decoration_data: None,
        banner: None,
        communication_disabled_until: None,
        flags: MemberFlags::empty(),
        joined_at: None,
        nick: None,
        pending: false,
        permissions: Permissions::empty(),
        premium_since: None,
        roles: Vec::new(),
    };
    let mut value = serde_json::to_value(interaction).expect("interaction JSON");
    value["data"]["resolved"] = json!({
        "users": {TARGET.to_string(): user(TARGET, false)},
        "members": {TARGET.to_string(): target_member},
    });
    serde_json::from_value(value).expect("resolved-target interaction")
}

fn mutations(requests: &[RestRequest]) -> Vec<&RestRequest> {
    requests
        .iter()
        .filter(|r| r.method != "GET" && r.path.starts_with("/api/v10/guilds/"))
        .collect()
}

#[derive(Debug)]
struct Evidence {
    mutations: usize,
    audits: Vec<(String, String, String)>,
    ledger_rows: i64,
    reply: String,
    ephemeral_defer: bool,
}

async fn evidence(pool: &PgPool, rest: &AuthorityRest, key: u64) -> Evidence {
    let requests = rest.requests();
    let audits = sqlx::query_as(
        "SELECT actor_id, action, outcome FROM moderation_audit WHERE idempotency_key = $1",
    )
    .bind(key.to_string())
    .fetch_all(pool)
    .await
    .expect("audit evidence");
    let ledger_rows = sqlx::query_scalar("SELECT (SELECT count(*) FROM moderation_warnings) + (SELECT count(*) FROM moderation_member_bans) + (SELECT count(*) FROM moderation_scheduled_unbans)")
        .fetch_one(pool).await.expect("zero-effect ledger evidence");
    let replies: Vec<Value> = requests
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.ends_with("/messages/@original"))
        .map(|r| serde_json::from_slice(&r.body).expect("reply JSON"))
        .collect();
    assert_eq!(replies.len(), 1, "one bounded command completion");
    assert_eq!(replies[0]["allowed_mentions"]["parse"], json!([]));
    let defers: Vec<Value> = requests
        .iter()
        .filter(|r| r.path.ends_with("/callback"))
        .map(|r| serde_json::from_slice(&r.body).expect("defer JSON"))
        .collect();
    Evidence {
        mutations: mutations(&requests).len(),
        audits,
        ledger_rows,
        reply: replies[0]["content"]
            .as_str()
            .expect("completion content")
            .into(),
        ephemeral_defer: defers.len() == 1
            && defers[0]["type"] == 5
            && defers[0]["data"]["flags"] == 64,
    }
}

fn assert_refused(label: &str, verb: &str, evidence: &Evidence) {
    assert_eq!(
        evidence.mutations, 0,
        "{label}/{verb}: stale authority must not mutate; {evidence:?}"
    );
    assert_eq!(
        evidence.ledger_rows, 0,
        "{label}/{verb}: no warning, prepared intent or expiry"
    );
    assert_eq!(
        evidence.audits,
        vec![(
            ACTOR.to_string(),
            format!("moderation.{verb}"),
            "denied".into()
        )],
        "{label}/{verb}: bounded attributable refusal audit"
    );
    assert!(
        evidence.ephemeral_defer,
        "{label}/{verb}: private deferred reply"
    );
    assert!(!evidence.reply.is_empty() && evidence.reply.len() < 1000);
    assert!(
        !evidence.reply.contains(REMOTE_MARKER),
        "remote errors are never echoed"
    );
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn current_actor_permission_and_membership_override_the_interaction_for_every_verb() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = database().await;
    let mut outcomes = Vec::new();
    for (index, verb) in VERBS.into_iter().enumerate() {
        for missing in [false, true] {
            let mut facts = Facts::default();
            if missing {
                facts.fault = Some(("/members/88", 404));
            } else {
                facts.actor["roles"] = json!([]);
            }
            let rest = AuthorityRest::start(facts).await;
            let (runtime, _) = runtime(db.pool(), &rest);
            let key = 10_000 + index as u64 * 2 + u64::from(missing);
            runtime.on_interaction(&stale_interaction(verb, key)).await;
            outcomes.push((
                if missing {
                    "absent actor"
                } else {
                    "revoked permission"
                },
                verb,
                evidence(db.pool(), &rest, key).await,
            ));
        }
    }
    db.close().await.expect("dispose fixture");
    for (label, verb, result) in outcomes {
        assert_refused(label, verb, &result);
    }
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn current_target_protection_and_hierarchy_override_resolved_picker_data() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = database().await;
    let mut outcomes = Vec::new();
    for (index, verb) in VERBS.into_iter().enumerate() {
        for protected in [false, true] {
            let mut facts = Facts::default();
            if protected {
                facts.target["user"]["bot"] = json!(true);
            } else {
                facts.target["roles"] = json!([HIGH_ROLE]);
            }
            let rest = AuthorityRest::start(facts).await;
            let (runtime, _) = runtime(db.pool(), &rest);
            let key = 20_000 + index as u64 * 2 + u64::from(protected);
            runtime.on_interaction(&stale_interaction(verb, key)).await;
            outcomes.push((
                if protected {
                    "now protected bot"
                } else {
                    "now higher target"
                },
                verb,
                evidence(db.pool(), &rest, key).await,
            ));
        }
    }
    db.close().await.expect("dispose fixture");
    for (label, verb, result) in outcomes {
        assert_refused(label, verb, &result);
    }
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn wrong_identities_incomplete_roles_and_read_failures_refuse_without_effects() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = database().await;
    let mut outcomes = Vec::new();
    for (index, case) in [
        "actor id",
        "target id",
        "bot id",
        "guild id",
        "actor role",
        "target role",
        "role permissions",
        "everyone role",
        "actor 403",
        "target 403",
        "actor 500",
        "target 500",
        "actor timeout",
    ]
    .into_iter()
    .enumerate()
    {
        let mut facts = Facts::default();
        match case {
            "actor id" => facts.actor["user"]["id"] = json!("9999"),
            "target id" => facts.target["user"]["id"] = json!("9999"),
            "bot id" => facts.bot["user"]["id"] = json!("9999"),
            "guild id" => facts.guild["id"] = json!("9999"),
            "actor role" => facts.actor["roles"] = json!(["9999"]),
            "target role" => facts.target["roles"] = json!(["9999"]),
            "role permissions" => {
                facts.guild["roles"][1]
                    .as_object_mut()
                    .expect("role")
                    .remove("permissions");
            }
            "everyone role" => {
                facts.guild["roles"]
                    .as_array_mut()
                    .expect("roles")
                    .remove(0);
            }
            "actor 403" => facts.fault = Some(("/members/88", 403)),
            "target 403" => facts.fault = Some(("/members/3333", 403)),
            "actor 500" => facts.fault = Some(("/members/88", 500)),
            "target 500" => facts.fault = Some(("/members/3333", 500)),
            "actor timeout" => facts.delay_actor = true,
            _ => unreachable!(),
        }
        let rest = AuthorityRest::start(facts).await;
        let (runtime, _) = runtime(db.pool(), &rest);
        let key = 30_000 + index as u64;
        tokio::time::timeout(
            Duration::from_secs(12),
            runtime.on_interaction(&stale_interaction("ban", key)),
        )
        .await
        .expect("bounded authority read");
        outcomes.push((case, evidence(db.pool(), &rest, key).await));
    }
    db.close().await.expect("dispose fixture");
    for (label, result) in outcomes {
        assert_refused(label, "ban", &result);
    }
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn queued_ban_and_tempban_recheck_after_the_existing_member_lane_is_acquired() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = database().await;
    let mut outcomes = Vec::new();
    for (index, verb) in ["ban", "tempban"].into_iter().enumerate() {
        for (change_index, change) in ["actor revoked", "target promoted", "target protected"]
            .into_iter()
            .enumerate()
        {
            let rest = AuthorityRest::start(Facts::default()).await;
            let (runtime, member) = runtime(db.pool(), &rest);
            let store = member.store();
            let entered = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let (inside, gate) = (entered.clone(), release.clone());
            let holder = tokio::spawn(async move {
                store
                    .serialize_member(GUILD_S, "3333", || async {
                        inside.notify_one();
                        gate.notified().await;
                    })
                    .await;
            });
            tokio::time::timeout(Duration::from_secs(5), entered.notified())
                .await
                .expect("own lane before command starts");
            let key = 40_000 + index as u64 * 10 + change_index as u64;
            let interaction = stale_interaction(verb, key);
            let mut command = tokio::spawn(async move {
                runtime.on_interaction(&interaction).await;
            });
            // Drive actual REST/DB awaits while the lane is owned. A fix may
            // acquire the lane before ALL reads; it need not preserve preflight
            // request counts. Either way no effect may pass the held lane.
            let waiting = tokio::time::timeout(Duration::from_millis(150), &mut command)
                .await
                .is_err();
            let mutations_before_release = mutations(&rest.requests()).len();
            rest.change(|facts| match change {
                "actor revoked" => facts.actor["roles"] = json!([]),
                "target promoted" => facts.target["roles"] = json!([HIGH_ROLE]),
                "target protected" => facts.target["user"]["bot"] = json!(true),
                _ => unreachable!(),
            });
            release.notify_one();
            holder.await.expect("release lane");
            if waiting {
                tokio::time::timeout(Duration::from_secs(12), &mut command)
                    .await
                    .expect("command completes after release")
                    .expect("command task");
            }
            outcomes.push((
                change,
                verb,
                waiting,
                mutations_before_release,
                evidence(db.pool(), &rest, key).await,
            ));
        }
    }
    db.close().await.expect("dispose fixture");
    for (label, verb, waited, early, result) in outcomes {
        assert!(
            waited,
            "{label}/{verb}: fixture must reach the occupied member lane"
        );
        assert_eq!(early, 0, "no mutation before releasing the real lane");
        assert_refused(label, verb, &result);
    }
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn valid_current_facts_and_nonmember_bans_still_work_and_completed_replay_never_mutates() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = database().await;
    let mut outcomes = Vec::new();
    for (index, verb) in VERBS.into_iter().enumerate() {
        let rest = AuthorityRest::start(Facts::default()).await;
        let (runtime, _) = runtime(db.pool(), &rest);
        let key = 50_000 + index as u64;
        let interaction = stale_interaction(verb, key);
        runtime.on_interaction(&interaction).await;
        let first = mutations(&rest.requests()).len();
        let first_evidence = format!(
            "{:?}; requests: {:?}",
            evidence(db.pool(), &rest, key).await,
            rest.requests()
                .iter()
                .map(|r| (&r.method, &r.path))
                .collect::<Vec<_>>()
        );
        runtime.on_interaction(&interaction).await;
        let replay = mutations(&rest.requests()).len();
        let done: Option<String> = sqlx::query_scalar(
            "SELECT state FROM moderation_idempotency WHERE guild_id = $1 AND idempotency_key = $2",
        )
        .bind(GUILD_S)
        .bind(key.to_string())
        .fetch_optional(db.pool())
        .await
        .expect("replay ledger");
        let audits: i64 = sqlx::query_scalar("SELECT count(*) FROM moderation_audit WHERE idempotency_key = $1 AND outcome <> 'denied'")
            .bind(key.to_string()).fetch_one(db.pool()).await.expect("accepted audit");
        let effect: i64 = if verb == "warn" {
            sqlx::query_scalar("SELECT count(*) FROM moderation_warnings WHERE request_id IN (SELECT request_id FROM moderation_audit WHERE idempotency_key = $1)")
                .bind(key.to_string()).fetch_one(db.pool()).await.expect("durable warning")
        } else {
            first as i64
        };
        rest.change(|facts| facts.fault = Some(("/members/88", 403)));
        runtime.on_interaction(&interaction).await;
        let after_failed_read = mutations(&rest.requests()).len();
        outcomes.push((
            verb,
            first,
            replay,
            done,
            audits,
            effect,
            after_failed_read,
            first_evidence,
        ));
    }
    let mut controls = Vec::new();
    for (index, (case, expected)) in [
        ("nonmember ban", 1),
        ("nonmember tempban", 1),
        ("current administrator", 1),
        ("administrator below a higher target", 0),
        ("current guild owner", 1),
    ]
    .into_iter()
    .enumerate()
    {
        let mut facts = Facts::default();
        let verb = if case == "nonmember tempban" {
            "tempban"
        } else {
            "ban"
        };
        match case {
            "nonmember ban" | "nonmember tempban" => facts.fault = Some(("/members/3333", 404)),
            // Administrator expands permissions only: with no explicit ban bit it
            // can act on a lower target, but keeps its real top-role position.
            "current administrator" => {
                facts.guild["roles"][1]["permissions"] = json!("8");
            }
            "administrator below a higher target" => {
                facts.guild["roles"][1]["permissions"] = json!("8");
                facts.target["roles"] = json!([HIGH_ROLE]);
            }
            "current guild owner" => {
                facts.guild["owner_id"] = json!(ACTOR.to_string());
                facts.actor["roles"] = json!([]);
                facts.target["roles"] = json!([HIGH_ROLE]);
            }
            _ => unreachable!(),
        }
        let rest = AuthorityRest::start(facts).await;
        let (runtime, _) = runtime(db.pool(), &rest);
        runtime
            .on_interaction(&stale_interaction(verb, 50_100 + index as u64))
            .await;
        controls.push((case, expected, mutations(&rest.requests()).len()));
    }
    db.close().await.expect("dispose fixture");
    for (verb, first, replay, done, audits, effect, after_failed_read, first_evidence) in outcomes {
        assert_eq!(
            first,
            usize::from(verb != "warn"),
            "{verb}: positive control reaches the real effect; {first_evidence}"
        );
        assert_eq!(replay, first, "{verb}: replay does not dispatch twice");
        assert_eq!(done.as_deref(), Some("done"));
        assert_eq!(
            audits, 1,
            "{verb}: replay must not duplicate acceptance audit"
        );
        assert_eq!(
            effect, 1,
            "{verb}: the positive control actually persists or dispatches"
        );
        assert_eq!(
            after_failed_read, first,
            "{verb}: failed freshness reads never reopen a completed request"
        );
    }
    for (case, expected, calls) in controls {
        assert_eq!(
            calls, expected,
            "{case}: preserve intentional policy exemptions"
        );
    }
}

#[tokio::test]
#[ignore = "requires disposable agent-testdb and loopback mock REST"]
async fn a_failed_authority_read_on_redelivery_cannot_release_an_unknown_dispatch_fence() {
    let _metrics = MODERATION_METRICS_LOCK.lock().await;
    let db = database().await;
    let facts = Facts {
        mutation_status: 500,
        ..Facts::default()
    };
    let rest = AuthorityRest::start(facts).await;
    let (runtime, member) = runtime(db.pool(), &rest);
    let interaction = stale_interaction("tempban", 60_000);
    runtime.on_interaction(&interaction).await;
    let before = member
        .store()
        .surface_uncertain(GUILD_S)
        .await
        .expect("unknown dispatch evidence");
    rest.change(|facts| {
        facts.fault = Some(("/members/88", 403));
        facts.mutation_status = 204;
    });
    runtime.on_interaction(&interaction).await;
    let after = member
        .store()
        .surface_uncertain(GUILD_S)
        .await
        .expect("read failure cannot reconcile");
    let calls = mutations(&rest.requests()).len();
    let state: Option<String> = sqlx::query_scalar("SELECT state FROM moderation_idempotency WHERE guild_id = $1 AND idempotency_key = '60000'")
        .bind(GUILD_S).fetch_optional(db.pool()).await.expect("unknown idempotency fence");
    let expiry: Option<String> =
        sqlx::query_scalar("SELECT state FROM moderation_scheduled_unbans WHERE guild_id = $1")
            .bind(GUILD_S)
            .fetch_optional(db.pool())
            .await
            .expect("staged expiry fence");
    db.close().await.expect("dispose fixture");
    assert!(
        !before.is_empty(),
        "control must have dispatched an uncertain PUT"
    );
    assert_eq!(
        after, before,
        "failed reads never prove an old request had no effect"
    );
    assert_eq!(calls, 1);
    assert_eq!(state.as_deref(), Some("in_flight"));
    assert_eq!(expiry.as_deref(), Some("staged"));
}
