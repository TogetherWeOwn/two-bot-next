//! Test-only supplement to the protected invites acceptance.
//! Breaks caught: counting join history instead of current membership, using a
//! timestamp alone to choose a membership, crossing guilds, guessing an inviter,
//! and pinging an inviter in a welcome. Snapshot attribution itself stays owned
//! by the existing Discord pipeline suite. No implementation API is prescribed:
//! gateway dispatch -> durable commit -> public command / welcome is the oracle.
//! Every fixture owns a disposable test database; no ignored tests or live data.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::time::Duration;

use futures_util::FutureExt as _;
use serde_json::{json, Value};
use sqlx::PgPool;
use twilight_gateway::{Event, EventTypeFlags};
use twilight_model::application::interaction::application_command::{
    CommandDataOption, CommandOptionValue,
};
use twilight_model::id::Id;
use two_bot_core::gateway_funnel::GatewayFunnelBuffer;
use two_bot_core::onboarding::MembershipTrigger;
use two_bot_core::{InviteState, NoopFacts, NoopLeveling, RouterGates};
use two_bot_cutover::gateway_session::GatewaySessionStore;
use two_bot_discord::{ActionExecutor, NoClassification, Pipeline, ScriptedInvites};
use two_bot_testsupport::TestDatabase;

use super::checkpoint;
use crate::command_runtime::{router_with_commands, CommandRuntime};
use crate::command_runtime_tests::slash;
use crate::discord_test_common::{MockRest, RestRequest, ScriptedResponse};
use crate::onboarding::{OnboardingJob, OnboardingRuntime};

const GUILD: u64 = 2222;
const OTHER_GUILD: u64 = 3333;
const ALICE: u64 = 9004;
const BRAM: u64 = 9005;
const MEMBER: u64 = 9404;
const OTHER_MEMBER: u64 = 9405;
const ZERO_MEMBER: u64 = 9555;
const AT: &str = "2026-09-30T12:00:00.000Z";
const LATER: &str = "2026-09-30T13:00:00.000Z";
const LAST: &str = "2026-09-30T14:00:00.000Z";
const NOW: i64 = 1_790_780_400_000;

type Journey = Pipeline<GatewayFunnelBuffer, NoopLeveling, NoopFacts, ScriptedInvites>;

struct Fixture {
    db: TestDatabase,
    pipeline: Journey,
    mock: MockRest,
}

async fn with_fixture(
    run: impl for<'a> FnOnce(&'a Fixture) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
) {
    let url = std::env::var("TWO_TEST_DATABASE_URL").expect("disposable test bootstrap required");
    let fixture = Fixture {
        db: TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("create isolated migrated test database"),
        pipeline: Pipeline::new(
            GatewayFunnelBuffer::default(),
            None,
            None,
            ScriptedInvites::new(),
            NoClassification,
        ),
        mock: discord().await,
    };
    let result = AssertUnwindSafe(run(&fixture)).catch_unwind().await;
    fixture.mock.shutdown().await;
    fixture
        .db
        .close()
        .await
        .expect("drop only this test database");
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

fn counters(alice: u64, bram: u64) -> Vec<InviteState> {
    [("alice-code", alice, ALICE), ("bram-code", bram, BRAM)]
        .into_iter()
        .map(|(code, uses, inviter)| InviteState {
            code: code.into(),
            uses,
            inviter_id: Some(inviter),
            channel_id: Some(4444),
            max_uses: 0,
            temporary: false,
        })
        .collect()
}

impl Fixture {
    fn snapshot(&self, guild: u64, rows: Vec<InviteState>) {
        self.pipeline.invite_source().push(guild, rows);
    }

    async fn dispatch(&self, guild: u64, seq: u64, kind: &str, member: u64, at: &str) {
        let mut data = json!({
            "guild_id": guild.to_string(),
            "user": {"id": member.to_string(), "username": "fixture-member", "discriminator": "0", "bot": false}
        });
        if kind == "GUILD_MEMBER_ADD" {
            data["roles"] = json!([]);
            data["joined_at"] = json!(at.replace('Z', "+00:00"));
            data["pending"] = json!(false);
            data["deaf"] = json!(false);
            data["mute"] = json!(false);
            data["flags"] = json!(0);
        }
        let packet = json!({"op": 0, "s": seq, "t": kind, "d": data}).to_string();
        let parsed = twilight_gateway::parse(packet, EventTypeFlags::all())
            .expect("fixture packet parses")
            .expect("known member dispatch");
        self.pipeline.handle_at(&Event::from(parsed), at);
        GatewaySessionStore::new(self.db.pool().clone(), guild.to_string(), 0)
            .commit_dispatch(
                &checkpoint("invite-net-fixture", seq, "ws://fixture"),
                self.pipeline.handlers().store().take_batch(),
            )
            .await
            .expect("commit member event before reading membership");
    }

    async fn join(&self, guild: u64, seq: u64, member: u64, at: &str) {
        self.dispatch(guild, seq, "GUILD_MEMBER_ADD", member, at)
            .await;
    }

    async fn leave(&self, guild: u64, seq: u64, member: u64, at: &str) {
        self.dispatch(guild, seq, "GUILD_MEMBER_REMOVE", member, at)
            .await;
    }

    async fn credit_alice(&self) {
        self.snapshot(GUILD, counters(10, 20));
        self.join(GUILD, 1, ZERO_MEMBER, AT).await; // prime, never guessed
        self.snapshot(GUILD, counters(11, 20));
        self.join(GUILD, 2, MEMBER, LATER).await;
    }

    async fn counts(&self, guild: u64, alice: u64, bram: u64) {
        for (member, name, want) in [(ALICE, "Alice", alice), (BRAM, "Bram", bram)] {
            let content = command(self.db.pool(), &self.mock, guild, member).await;
            let line = content
                .lines()
                .find(|line| line.contains(name))
                .unwrap_or_else(|| panic!("count line names {name}: {content:?}"));
            if want == 0 {
                let lower = line.to_lowercase();
                assert!(
                    line.split(|c: char| !c.is_ascii_digit())
                        .any(|number| number == "0")
                        || lower.contains("yet")
                        || lower.contains("no ")
                        || lower.contains("none"),
                    "{name} has zero current attributed members, never historical credit: {line:?}"
                );
            } else {
                assert!(
                    line.contains(&format!(" {want} ")) || line.contains(&format!("({want})")),
                    "{name} has {want} current attributed members (not a rank or snowflake): {line:?}"
                );
            }
        }
    }

    async fn welcome(&self) -> Value {
        let vars = HashMap::from([
            ("DISCORD_GUILD_ID".into(), GUILD.to_string()),
            ("TWO_ONBOARDING_MODE".into(), "anchor".into()),
            ("TWO_ONBOARDING_DRY_RUN".into(), "0".into()),
            ("DISCORD_LANDING_CHANNEL_IDS".into(), "10".into()),
            ("DISCORD_ANCHOR_WELCOME_CHANNEL_ID".into(), "10".into()),
        ]);
        let runtime = OnboardingRuntime::new(
            self.db.pool().clone(),
            executor(&self.mock),
            &vars,
            GUILD,
            999,
        )
        .expect("onboarding fixture");
        runtime
            .handle(
                OnboardingJob::Welcome {
                    guild_id: GUILD,
                    member_id: MEMBER,
                    bot: false,
                    pending: false,
                    trigger: MembershipTrigger::Joined { pending: false },
                    roles: vec![],
                },
                NOW,
            )
            .await
            .expect("welcome succeeds using the committed current join");
        let posts: Vec<_> = self
            .mock
            .requests()
            .into_iter()
            .filter(|request| {
                request.method == "POST" && request.path.ends_with("/channels/10/messages")
            })
            .collect();
        assert_eq!(
            posts.len(),
            1,
            "exactly one welcome reaches the landing channel"
        );
        serde_json::from_slice(&posts[0].body).expect("welcome JSON")
    }
}

fn executor(mock: &MockRest) -> ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    ActionExecutor::with_proxy("test-token".into(), Some(mock.origin())).expect("mock executor")
}

async fn discord() -> MockRest {
    MockRest::with_responder(|request: &RestRequest| {
        let path = request.path.strip_prefix("/api/v10").unwrap_or(&request.path);
        if request.method == "GET" && path.ends_with("/invites") {
            return ScriptedResponse::json(200, json!([]));
        }
        if request.method == "GET" && path.contains("/members/") {
            let id = path.rsplit('/').next().expect("member id");
            let name = match id { "9004" => "Alice", "9005" => "Bram", _ => "Fixture Member" };
            return ScriptedResponse::json(200, json!({
                "user": {"id": id, "username": name, "global_name": name, "discriminator": "0", "bot": false},
                "nick": null, "roles": [], "pending": false, "deaf": false, "mute": false, "flags": 0
            }));
        }
        if request.method == "GET" && path == "/guilds/2222" {
            return ScriptedResponse::json(200, json!({"id": "2222", "owner_id": "888"}));
        }
        if request.method == "GET" && path.ends_with("/roles") {
            return ScriptedResponse::json(200, json!([{"id": "2222", "permissions": "3072"}]));
        }
        if request.method == "GET" && path == "/channels/10" {
            return ScriptedResponse::json(200, json!({
                "id": "10", "guild_id": "2222", "type": 0, "permission_overwrites": []
            }));
        }
        if request.method == "POST" && path == "/channels/10/messages" {
            return ScriptedResponse::json(201, json!({"id": "99"}));
        }
        if request.method == "POST" && path.ends_with("/callback") {
            return ScriptedResponse::status(204);
        }
        if request.method == "PATCH" && path.ends_with("/messages/@original") {
            return ScriptedResponse::json(200, json!({"id": "98"}));
        }
        panic!("unexpected mock Discord request: {} {}", request.method, path);
    }).await
}

async fn command(pool: &PgPool, mock: &MockRest, guild: u64, member: u64) -> String {
    let gates = RouterGates {
        configured_guild: Some(guild),
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
    };
    let runtime = CommandRuntime::new(
        pool.clone(),
        executor(mock),
        router_with_commands(gates),
        guild,
        false,
    );
    let mut interaction = slash(
        "invites",
        None,
        vec![CommandDataOption {
            name: "member".into(),
            value: CommandOptionValue::User(Id::new(member)),
        }],
    );
    interaction.guild_id = Some(Id::new(guild));
    let event = Event::InteractionCreate(Box::new(
        twilight_model::gateway::payload::incoming::InteractionCreate(interaction),
    ));
    let start = mock.requests().len();
    assert!(runtime.dispatch(&event), "gateway admits /invites");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !(0..6).all(|lane| runtime.lane_in_flight(lane) == 0) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("/invites settles");
    let requests = mock.requests();
    let replies = &requests[start..];
    let data =
        if let Some(edit) = replies.iter().rev().find(|request| {
            request.method == "PATCH" && request.path.ends_with("/messages/@original")
        }) {
            serde_json::from_slice::<Value>(&edit.body).expect("deferred reply")
        } else {
            let callback = replies
                .iter()
                .find(|request| request.method == "POST" && request.path.ends_with("/callback"))
                .expect("/invites answers");
            let value: Value = serde_json::from_slice(&callback.body).expect("callback JSON");
            assert_eq!(
                value["type"], 4,
                "immediate answer or deferred edit required"
            );
            value["data"].clone()
        };
    assert_eq!(
        data["allowed_mentions"]["parse"],
        json!([]),
        "command never pings"
    );
    data["content"].as_str().expect("text reply").to_owned()
}

#[tokio::test]
async fn current_membership_does_not_count_repeated_join_facts_twice() {
    with_fixture(|f| {
        Box::pin(async move {
            f.credit_alice().await;
            f.counts(GUILD, 1, 0).await;
            f.snapshot(GUILD, counters(12, 20));
            f.join(GUILD, 3, MEMBER, LAST).await;
            f.counts(GUILD, 1, 0).await;
            f.snapshot(GUILD, counters(13, 20));
            f.join(GUILD, 4, OTHER_MEMBER, "2026-09-30T15:00:00.000Z")
                .await;
            f.counts(GUILD, 2, 0).await;
        })
    })
    .await;
}

#[tokio::test]
async fn current_membership_latest_join_replaces_the_previous_inviter() {
    with_fixture(|f| {
        Box::pin(async move {
            f.credit_alice().await;
            f.snapshot(GUILD, counters(11, 21));
            f.join(GUILD, 3, MEMBER, LAST).await;
            f.counts(GUILD, 0, 1).await;
        })
    })
    .await;
}

#[tokio::test]
async fn leave_clears_only_that_membership_and_duplicate_leave_cannot_debit_a_peer() {
    with_fixture(|f| {
        Box::pin(async move {
            f.credit_alice().await;
            f.snapshot(GUILD, counters(12, 20));
            f.join(GUILD, 3, OTHER_MEMBER, LAST).await;
            f.counts(GUILD, 2, 0).await;
            f.leave(GUILD, 4, MEMBER, "2026-09-30T15:00:00.000Z").await;
            f.counts(GUILD, 1, 0).await;
            f.leave(GUILD, 5, MEMBER, "2026-09-30T16:00:00.000Z").await;
            f.counts(GUILD, 1, 0).await;
        })
    })
    .await;
}

async fn unattributed_rejoin(f: &Fixture, rows: Vec<InviteState>) {
    f.credit_alice().await;
    f.leave(GUILD, 3, MEMBER, LAST).await;
    f.snapshot(GUILD, rows);
    f.join(GUILD, 4, MEMBER, "2026-09-30T15:00:00.000Z").await;
    f.counts(GUILD, 0, 0).await;
}

#[tokio::test]
async fn unknown_rejoin_never_inherits_a_historical_inviter() {
    with_fixture(|f| Box::pin(unattributed_rejoin(f, counters(11, 20)))).await;
}

#[tokio::test]
async fn expired_invite_rejoin_never_guesses_the_deleted_codes_inviter() {
    with_fixture(|f| Box::pin(unattributed_rejoin(f, vec![]))).await;
}

#[tokio::test]
async fn ambiguous_growth_rejoin_never_credits_either_candidate() {
    with_fixture(|f| Box::pin(unattributed_rejoin(f, counters(12, 21)))).await;
}

#[tokio::test]
async fn same_timestamp_latest_unknown_join_wins_by_durable_event_order() {
    with_fixture(|f| {
        Box::pin(async move {
            f.snapshot(GUILD, counters(10, 20));
            f.join(GUILD, 1, ZERO_MEMBER, AT).await;
            f.snapshot(GUILD, counters(11, 20));
            f.join(GUILD, 2, MEMBER, LATER).await;
            f.leave(GUILD, 3, MEMBER, LATER).await;
            f.snapshot(GUILD, counters(11, 20));
            f.join(GUILD, 4, MEMBER, LATER).await;
            f.counts(GUILD, 0, 0).await;
        })
    })
    .await;
}

#[tokio::test]
async fn same_timestamp_latest_credited_join_wins_by_durable_event_order() {
    with_fixture(|f| {
        Box::pin(async move {
            f.snapshot(GUILD, counters(10, 20));
            f.join(GUILD, 1, ZERO_MEMBER, AT).await;
            f.snapshot(GUILD, counters(10, 20));
            f.join(GUILD, 2, MEMBER, LATER).await;
            f.leave(GUILD, 3, MEMBER, LATER).await;
            f.snapshot(GUILD, counters(11, 20));
            f.join(GUILD, 4, MEMBER, LATER).await;
            f.counts(GUILD, 1, 0).await;
        })
    })
    .await;
}

#[tokio::test]
async fn same_invitee_in_two_guilds_has_independent_membership_and_net_counts() {
    with_fixture(|f| {
        Box::pin(async move {
            f.credit_alice().await;
            f.snapshot(OTHER_GUILD, counters(30, 40));
            f.join(OTHER_GUILD, 1, ZERO_MEMBER, AT).await;
            f.snapshot(OTHER_GUILD, counters(30, 41));
            f.join(OTHER_GUILD, 2, MEMBER, LATER).await;
            f.counts(GUILD, 1, 0).await;
            f.counts(OTHER_GUILD, 0, 1).await;
            f.leave(GUILD, 3, MEMBER, LAST).await;
            f.counts(GUILD, 0, 0).await;
            f.counts(OTHER_GUILD, 0, 1).await;
        })
    })
    .await;
}

fn assert_welcome_mentions(body: &Value) {
    assert_eq!(
        body["allowed_mentions"],
        json!({
            "parse": [], "users": [MEMBER.to_string()], "roles": [], "replied_user": false
        }),
        "only the welcomed member may be notified, never an inviter or role"
    );
    assert!(
        body["content"]
            .as_str()
            .expect("welcome text")
            .contains(&format!("<@{MEMBER}>")),
        "the welcomed member is still addressed"
    );
}

#[tokio::test]
async fn credited_current_join_names_inviter_without_notifying_them() {
    with_fixture(|f| {
        Box::pin(async move {
            f.credit_alice().await;
            let body = f.welcome().await;
            assert_welcome_mentions(&body);
            let text = body["content"].as_str().expect("welcome text");
            assert!(
                text.contains("Alice")
                    || text.contains(&format!("<@{ALICE}>"))
                    || text.contains(&format!("<@!{ALICE}>")),
                "welcome names the inviter from the committed current join: {text:?}"
            );
        })
    })
    .await;
}

#[tokio::test]
async fn unknown_current_join_welcome_never_names_the_historical_inviter() {
    with_fixture(|f| {
        Box::pin(async move {
            f.credit_alice().await;
            f.leave(GUILD, 3, MEMBER, LAST).await;
            f.snapshot(GUILD, counters(11, 20));
            f.join(GUILD, 4, MEMBER, "2026-09-30T15:00:00.000Z").await;
            let body = f.welcome().await;
            assert_welcome_mentions(&body);
            let text = body["content"].as_str().expect("welcome text");
            assert!(
                !text.contains("Alice") && !text.contains(&ALICE.to_string()),
                "unknown current membership cannot name an old inviter: {text:?}"
            );
        })
    })
    .await;
}
