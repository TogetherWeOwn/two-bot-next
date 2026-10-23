#![cfg(test)]

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use futures_util::FutureExt as _;
use serde_json::{json, Value};
use sqlx::PgPool;
use twilight_gateway::Event;
use twilight_model::application::interaction::application_command::{
    CommandDataOption, CommandOptionValue,
};
use twilight_model::gateway::payload::incoming::InteractionCreate;
use twilight_model::id::Id;
use two_bot_core::onboarding::MembershipTrigger;
use two_bot_core::RouterGates;
use two_bot_testsupport::TestDatabase;

use crate::command_runtime::{router_with_commands, CommandRuntime};
use crate::command_runtime_tests::slash;
use crate::discord_test_common::{MockRest, ResponseGate, RestRequest, ScriptedResponse};
use crate::onboarding::{OnboardingJob, OnboardingRuntime};
use two_bot_discord::ActionExecutor;

pub(super) const GUILD: u64 = 2222;
pub(super) const ALICE: u64 = 9004;
pub(super) const BRAM: u64 = 9005;
pub(super) const MEMBER: u64 = 9404;
pub(super) const AT: &str = "2026-09-30T12:00:00.000Z";
pub(super) const NOW: i64 = 1_790_780_400_000;

#[derive(Clone)]
pub(super) struct Discord {
    pub invites: ScriptedResponse,
    pub members: HashMap<u64, Value>,
    pub member_delay: Duration,
    /// Gates only the first request for this member. A retry can then progress.
    pub gate: Option<(u64, Arc<ResponseGate>)>,
}

impl Default for Discord {
    fn default() -> Self {
        Self {
            invites: ScriptedResponse::json(200, json!([])),
            members: HashMap::new(),
            member_delay: Duration::ZERO,
            gate: None,
        }
    }
}

pub(super) fn member(id: u64, nick: Value, global: Value, username: Value) -> Value {
    json!({
        "user": {"id": id.to_string(), "username": username, "global_name": global,
                 "discriminator": "0", "bot": false},
        "nick": nick, "roles": [], "pending": false, "deaf": false, "mute": false, "flags": 0
    })
}

pub(super) async fn discord(config: Discord) -> MockRest {
    let gated = AtomicBool::new(false);
    MockRest::with_gated_responder(move |request: &RestRequest| {
        let path = request
            .path
            .strip_prefix("/api/v10")
            .unwrap_or(&request.path);
        let response = match (request.method.as_str(), path) {
            ("GET", "/guilds/2222/invites") => config.invites.clone(),
            ("GET", "/guilds/2222") => {
                ScriptedResponse::json(200, json!({"id": "2222", "owner_id": "888"}))
            }
            ("GET", "/guilds/2222/roles") => {
                ScriptedResponse::json(200, json!([{"id": "2222", "permissions": "3072"}]))
            }
            ("GET", "/channels/10") => ScriptedResponse::json(
                200,
                json!({
                    "id": "10", "guild_id": "2222", "type": 0, "permission_overwrites": []
                }),
            ),
            ("GET", path) if path.starts_with("/guilds/2222/members/") => {
                let id: u64 = path
                    .rsplit('/')
                    .next()
                    .expect("member id")
                    .parse()
                    .expect("numeric member");
                let body = config.members.get(&id).cloned().unwrap_or_else(|| {
                    let name = match id {
                        ALICE => "Alice".to_owned(),
                        BRAM => "Bram".to_owned(),
                        _ => format!("Person {id}"),
                    };
                    member(id, Value::Null, Value::Null, json!(name))
                });
                let gate = config.gate.as_ref().and_then(|(target, gate)| {
                    (*target == id && !gated.swap(true, Ordering::SeqCst)).then(|| Arc::clone(gate))
                });
                return (
                    ScriptedResponse::json(200, body).delayed(config.member_delay),
                    gate,
                );
            }
            ("POST", "/channels/10/messages") => ScriptedResponse::json(201, json!({"id": "99"})),
            ("POST", path) if path.ends_with("/callback") => ScriptedResponse::status(204),
            ("PATCH", path) if path.ends_with("/messages/@original") => {
                ScriptedResponse::json(200, json!({"id": "98"}))
            }
            _ => panic!(
                "unexpected mock Discord request: {} {}",
                request.method, path
            ),
        };
        (response, None)
    })
    .await
}

pub(super) struct Fixture {
    pub db: TestDatabase,
    pub mock: MockRest,
}

pub(super) async fn with_fixture(
    config: Discord,
    run: impl for<'a> FnOnce(&'a Fixture) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
) {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let fixture = Fixture {
        db: TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create isolated migrated test database"),
        mock: discord(config).await,
    };
    let result = AssertUnwindSafe(run(&fixture)).catch_unwind().await;
    fixture.mock.shutdown().await;
    fixture
        .db
        .close()
        .await
        .expect("drop only this fixture database");
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

impl Fixture {
    /// Persist hostile/legacy metadata verbatim through the real event schema.
    pub async fn seed(&self, invitee: u64, metadata: &str) {
        sqlx::query(
            "INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
             VALUES ('member_join', $1, '2222', $2::text::timestamptz, 'invite:fixture', $3, $4)"
        )
        .bind(invitee.to_string()).bind(AT).bind(metadata).bind(format!("review-join-{invitee}"))
        .execute(self.db.pool()).await.expect("seed committed membership evidence");
    }

    pub async fn credit(&self, invitee: u64, inviter: u64) {
        self.seed(
            invitee,
            &json!({"inviterId": inviter.to_string()}).to_string(),
        )
        .await;
    }

    pub async fn command(&self, target: u64) -> Value {
        command(self.db.pool(), &self.mock, target).await
    }

    pub async fn welcome(&self, target: u64) -> Value {
        let before = welcome_posts(&self.mock).len();
        onboarding(self.db.pool(), &self.mock)
            .handle(welcome_job(target), NOW)
            .await
            .expect("welcome succeeds");
        let posts = welcome_posts(&self.mock);
        assert_eq!(posts.len(), before + 1, "exactly one welcome is sent");
        let body = posts.last().expect("welcome JSON").clone();
        assert_eq!(
            body["allowed_mentions"],
            json!({
                "parse": [], "users": [target.to_string()], "roles": [], "replied_user": false
            }),
            "only the recipient is allowed to notify"
        );
        body
    }
}

pub(super) fn gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        scorecard: false,
        automations: false,
        announcements: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("test-token".into(), Some(mock.origin())).expect("mock executor")
}

pub(super) async fn command(pool: &PgPool, mock: &MockRest, target: u64) -> Value {
    let runtime = CommandRuntime::new(
        pool.clone(),
        executor(mock),
        router_with_commands(gates()),
        GUILD,
        false,
    );
    let interaction = slash(
        "invites",
        None,
        vec![CommandDataOption {
            name: "member".into(),
            value: CommandOptionValue::User(Id::new(target)),
        }],
    );
    let start = mock.requests().len();
    assert!(
        runtime.dispatch(&Event::InteractionCreate(Box::new(InteractionCreate(
            interaction
        )))),
        "gateway admits /invites"
    );
    tokio::time::timeout(Duration::from_secs(35), async {
        while !(0..6).all(|lane| runtime.lane_in_flight(lane) == 0) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("/invites settles within the fixture deadline");
    let requests = mock.requests();
    let replies = &requests[start..];
    let callbacks: Vec<_> = replies
        .iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/callback"))
        .collect();
    assert_eq!(
        callbacks.len(),
        1,
        "one acknowledgement, never a second initial reply"
    );
    let callback: Value = serde_json::from_slice(&callbacks[0].body).expect("callback JSON");
    assert_eq!(
        callback["type"], 5,
        "deferred public acknowledgement precedes reads"
    );
    let edits: Vec<_> = replies
        .iter()
        .filter(|r| r.method == "PATCH" && r.path.ends_with("/messages/@original"))
        .collect();
    assert_eq!(
        edits.len(),
        1,
        "every accepted request edits the deferred original exactly once"
    );
    let body: Value = serde_json::from_slice(&edits[0].body).expect("edit JSON");
    assert_eq!(
        body["allowed_mentions"]["parse"],
        json!([]),
        "command never pings"
    );
    assert_eq!(
        body["flags"].as_u64().unwrap_or(0) & 64,
        0,
        "reply is public"
    );
    body
}

pub(super) fn text(body: &Value) -> &str {
    body["content"].as_str().expect("rendered text")
}

pub(super) fn board(body: &Value) -> Vec<(String, u64)> {
    text(body)
        .lines()
        .filter_map(|line| {
            let (name, tail) = line.rsplit_once(": ")?;
            let count = tail
                .strip_suffix(" invites")?
                .parse()
                .expect("plain board count");
            Some((name.to_owned(), count))
        })
        .collect()
}

pub(super) fn assert_personal(body: &Value, name: &str, count: u64) {
    assert_eq!(
        text(body).lines().next(),
        Some(format!("{name}: {count} attributed invites (net)").as_str())
    );
}

pub(super) fn onboarding(pool: &PgPool, mock: &MockRest) -> OnboardingRuntime {
    let vars = HashMap::from([
        ("DISCORD_GUILD_ID".into(), GUILD.to_string()),
        ("TWO_ONBOARDING_MODE".into(), "anchor".into()),
        ("TWO_ONBOARDING_DRY_RUN".into(), "0".into()),
        ("DISCORD_LANDING_CHANNEL_IDS".into(), "10".into()),
        ("DISCORD_ANCHOR_WELCOME_CHANNEL_ID".into(), "10".into()),
    ]);
    OnboardingRuntime::new(pool.clone(), executor(mock), &vars, GUILD, 999)
        .expect("onboarding fixture")
}

pub(super) fn welcome_job(target: u64) -> OnboardingJob {
    OnboardingJob::Welcome {
        guild_id: GUILD,
        member_id: target,
        bot: false,
        pending: false,
        trigger: MembershipTrigger::Joined { pending: false },
        roles: vec![],
    }
}

pub(super) fn welcome_posts(mock: &MockRest) -> Vec<Value> {
    mock.requests()
        .iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/channels/10/messages"))
        .map(|r| serde_json::from_slice(&r.body).expect("welcome JSON"))
        .collect()
}
