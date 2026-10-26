#![cfg(feature = "db")]

//! Protected saved-but-pending registry publication acceptance.
//!
//! Real command management, storage and publication run against an isolated test
//! database and loopback Discord. A 403 refuses the registry PUT only; command
//! acknowledgement and the private completion still succeed.
//!
//! Durable observation contract: `automation_command_registry_resync` is a
//! table OR view exposing one `guild_id` (text) row per pending guild. No other
//! columns or physical storage layout are prescribed. A missing relation means
//! zero pending rows on the unimplemented baseline, not a skipped assertion.
//! Recovery uses the existing public `sync_registry` publication opportunity;
//! no new retry API, job cadence or production test helper is prescribed.

#[allow(dead_code)]
mod common;

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use common::{MockRest, RestRequest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::PgPool;
use twilight_model::application::interaction::Interaction;
use two_bot_core::{
    custom_command_store as store, custom_commands::StoredCommand, InteractionRouter, RouterGates,
};
use two_bot_discord::{
    custom_commands::{CustomCommandError, CustomCommandRuntime},
    ActionExecutor,
};
use two_bot_testsupport::TestDatabase;

const GUILD: u64 = 2222;
const FOREIGN: u64 = 9999;
const TEMPLATE: &str = "Saved response for {username}";

fn gates(guild: u64) -> RouterGates {
    RouterGates {
        configured_guild: Some(guild),
        automations: true,
        moderation: false,
        voice: false,
        voice_assistant: false,
        scorecard: false,
        announcements: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn runtime(pool: PgPool, executor: ActionExecutor, guild: u64) -> CustomCommandRuntime {
    let mut router = InteractionRouter::new(gates(guild));
    CustomCommandRuntime::register(&mut router);
    CustomCommandRuntime::new(pool, Arc::new(router), executor, 1111)
}

#[derive(Clone, Copy, Debug)]
enum Mutation {
    Define,
    Disable,
    Enable,
}

impl Mutation {
    fn command(self) -> &'static str {
        match self {
            Self::Define => "command",
            Self::Disable => "command-disable",
            Self::Enable => "command-enable",
        }
    }

    fn expected_name(self) -> &'static str {
        match self {
            Self::Define => "newfaq",
            Self::Disable | Self::Enable => "faq",
        }
    }

    fn enabled(self) -> bool {
        !matches!(self, Self::Disable)
    }

    fn input(self, id: u64, guild: u64) -> Interaction {
        let options = match self {
            Self::Define => json!([
                {"name": "name", "type": 3, "value": "newfaq"},
                {"name": "template", "type": 3, "value": TEMPLATE},
                {"name": "description", "type": 3, "value": "A saved answer"}
            ]),
            Self::Disable | Self::Enable => json!([
                {"name": "name", "type": 3, "value": "faq"}
            ]),
        };
        serde_json::from_value(json!({
            "id": id.to_string(), "application_id": "1111", "type": 2,
            "token": format!("resync-fixture-{id}"), "version": 1,
            "guild_id": guild.to_string(),
            "channel": {"id": "4444", "type": 0, "name": "commands"},
            "authorizing_integration_owners": {"0": guild.to_string()}, "entitlements": [],
            "member": {
                "permissions": "32", "roles": [], "joined_at": null,
                "deaf": false, "mute": false, "flags": 0,
                "user": {"id": "3333", "username": "tester", "discriminator": "0000", "avatar": null}
            },
            "data": {"id": "5555", "name": self.command(), "type": 1, "options": options}
        }))
        .expect("Manage Server interaction fixture")
    }
}

fn reply_to(request: &RestRequest, remaining_failures: &AtomicUsize) -> ScriptedResponse {
    match request.method.as_str() {
        "POST" if request.path.ends_with("/callback") => ScriptedResponse::status(204),
        "PATCH" if request.path.ends_with("/messages/@original") => {
            ScriptedResponse::json(200, json!({"id": "9000"}))
        }
        "PUT"
            if [GUILD, FOREIGN].iter().any(|guild| {
                request.path == format!("/api/v10/applications/1111/guilds/{guild}/commands")
            }) =>
        {
            if remaining_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return ScriptedResponse::status(403);
            }
            let mut receipt = body(request);
            for (index, command) in receipt
                .as_array_mut()
                .expect("full registry PUT")
                .iter_mut()
                .enumerate()
            {
                command["id"] = json!((8000 + index).to_string());
                command["application_id"] = json!("1111");
                command["version"] = json!("1");
                command["type"] = json!(1);
            }
            ScriptedResponse::json(200, receipt)
        }
        _ => ScriptedResponse::status(403),
    }
}

struct Fixture {
    db: TestDatabase,
    observer: PgPool,
    mock: MockRest,
    executor: ActionExecutor,
    runtime: CustomCommandRuntime,
    remaining_failures: Arc<AtomicUsize>,
}

impl Fixture {
    async fn new(mutation: Mutation) -> Self {
        let url = std::env::var("TWO_TEST_DATABASE_URL")
            .expect("explicit disposable test bootstrap required; an absent URL is not a pass");
        let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("isolated migrated test database");
        for guild in [GUILD, FOREIGN] {
            seed(
                db.pool(),
                guild,
                "faq",
                matches!(mutation, Mutation::Disable),
            )
            .await;
            seed(db.pool(), guild, "peer", true).await;
        }
        let observer = db
            .independent_pool()
            .await
            .expect("independent durable reader");
        let remaining_failures = Arc::new(AtomicUsize::new(0));
        let policy = Arc::clone(&remaining_failures);
        let mock = MockRest::with_responder(move |request| reply_to(request, &policy)).await;
        let executor = ActionExecutor::with_proxy("resync-fixture".to_owned(), Some(mock.origin()))
            .expect("loopback-only Discord executor");
        let runtime = runtime(db.pool().clone(), executor.clone(), GUILD);
        runtime
            .sync_registry()
            .await
            .expect("initial boot publication");
        Self {
            db,
            observer,
            mock,
            executor,
            runtime,
            remaining_failures,
        }
    }

    async fn mutate(&self, mutation: Mutation, id: u64) -> Observed {
        observe(&self.runtime, &self.mock, mutation.input(id, GUILD)).await
    }

    async fn close(self) {
        drop(self.runtime);
        drop(self.executor);
        self.mock.shutdown().await;
        self.observer.close().await;
        self.db
            .close()
            .await
            .expect("drop only the owned test database");
    }
}

async fn seed(pool: &PgPool, guild: u64, name: &str, enabled: bool) {
    store::put_command(
        pool,
        &StoredCommand {
            guild_id: guild.to_string(),
            name: name.to_owned(),
            description: "Saved answer".to_owned(),
            template: TEMPLATE.to_owned(),
            text_trigger: None,
            enabled,
        },
        "66",
        "67",
        "2026-10-01T00:00:00Z",
        "2026-10-01T00:00:00Z",
    )
    .await
    .expect("seed through the real store");
}

async fn pending(pool: &PgPool, guild: u64) -> i64 {
    let relation: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public.automation_command_registry_resync')::text")
            .fetch_one(pool)
            .await
            .expect("inspect the disposable schema");
    if relation.is_none() {
        return 0;
    }
    sqlx::query_scalar(
        "SELECT count(*) FROM public.automation_command_registry_resync WHERE guild_id = $1",
    )
    .bind(guild.to_string())
    .fetch_one(pool)
    .await
    .expect("durable pending rows from an independent connection")
}

async fn definitions(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar("SELECT to_jsonb(c) FROM automation_commands c ORDER BY guild_id, name")
        .fetch_all(pool)
        .await
        .expect("every saved field, including mutation metadata")
}

async fn audit(pool: &PgPool) -> Vec<Value> {
    sqlx::query_scalar("SELECT to_jsonb(a) FROM automation_audit_log a ORDER BY id")
        .fetch_all(pool)
        .await
        .expect("immutable mutation audit facts")
}

struct Observed {
    result: Result<bool, CustomCommandError>,
    requests: Vec<RestRequest>,
}

async fn observe(runtime: &CustomCommandRuntime, mock: &MockRest, input: Interaction) -> Observed {
    let start = mock.requests().len();
    let result = runtime.handle_interaction(&input, Some("Test guild")).await;
    Observed {
        result,
        requests: mock.requests()[start..].to_vec(),
    }
}

fn body(request: &RestRequest) -> Value {
    serde_json::from_slice(&request.body).expect("wire JSON")
}

fn publications(requests: &[RestRequest]) -> Vec<Value> {
    requests
        .iter()
        .filter(|r| r.method == "PUT")
        .map(body)
        .collect()
}

fn private_reply(observed: &Observed) -> String {
    assert!(
        matches!(observed.result, Ok(true)),
        "owned interaction: {:?}",
        observed.result
    );
    let replies: Vec<_> = observed
        .requests
        .iter()
        .filter(|r| r.method != "PUT")
        .collect();
    assert_eq!(
        replies.len(),
        2,
        "one defer and one completion, never a channel send"
    );
    assert_eq!(replies[0].method, "POST");
    assert!(replies[0].path.ends_with("/callback"));
    let initial = body(replies[0]);
    assert_eq!(initial["type"], 5);
    assert_eq!(initial["data"]["flags"].as_u64().unwrap_or(0) & 64, 64);
    assert_eq!(replies[1].method, "PATCH");
    assert!(replies[1].path.ends_with("/messages/@original"));
    let completion = body(replies[1]);
    for value in [&initial["data"], &completion] {
        assert_eq!(value["allowed_mentions"]["parse"], json!([]));
        assert_ne!(value["allowed_mentions"]["replied_user"], true);
    }
    completion["content"]
        .as_str()
        .expect("private result text")
        .to_owned()
}

fn assert_saved(rows: &[Value], mutation: Mutation) {
    let row = rows
        .iter()
        .find(|row| row["guild_id"] == GUILD.to_string() && row["name"] == mutation.expected_name())
        .expect("mutation committed, not deleted or rolled back after the failed PUT");
    assert_eq!(row["enabled"], mutation.enabled());
    assert_eq!(row["template"], TEMPLATE);
}

fn assert_pending_reply(observed: &Observed, mutation: Mutation) {
    let text = private_reply(observed).to_ascii_lowercase();
    assert!(
        text.contains(mutation.expected_name()),
        "name the saved response: {text}"
    );
    let state = match mutation {
        Mutation::Define => "saved",
        Mutation::Disable => "disabled",
        Mutation::Enable => "enabled",
    };
    assert!(text.contains(state), "truthful committed state: {text}");
    assert!(
        text.contains("pending"),
        "name the durable pending state, not just a failed request: {text}"
    );
    assert!(
        ["sync", "publish", "publication", "picker"]
            .iter()
            .any(|word| text.contains(word)),
        "pending registry synchronization: {text}"
    );
    for forbidden in [
        "restart",
        "delete",
        "/command-remove",
        "retry the command",
        "run the command again",
        "now synchronized",
        "now synchronised",
        "now published",
        "successfully published",
    ] {
        assert!(
            !text.contains(forbidden),
            "no false success or mutation/restart advice ({forbidden}): {text}"
        );
    }
}

fn assert_full_set(set: &Value, mutation: Mutation) {
    let names: Vec<_> = set
        .as_array()
        .expect("whole replacement set")
        .iter()
        .map(|row| row["name"].as_str().expect("command name"))
        .collect();
    assert_eq!(names.iter().filter(|&&name| name == "peer").count(), 1);
    assert_eq!(
        names
            .iter()
            .filter(|&&name| name == mutation.expected_name())
            .count(),
        usize::from(mutation.enabled())
    );
    for builtin in [
        "help",
        "command",
        "command-list",
        "command-enable",
        "command-disable",
    ] {
        assert_eq!(
            names.iter().filter(|&&name| name == builtin).count(),
            1,
            "retain /{builtin}"
        );
    }
    assert!(names.len() <= 100);
}

async fn failed_commit(mutation: Mutation) {
    let f = Fixture::new(mutation).await;
    let confirmed: Vec<_> = f
        .runtime
        .published_commands()
        .expect("initial confirmed snapshot")
        .iter()
        .map(|d| d.name.clone())
        .collect();
    f.remaining_failures.store(1, Ordering::SeqCst);
    let observed = f.mutate(mutation, 1001).await;
    let rows = definitions(&f.observer).await;
    let facts = audit(&f.observer).await;
    let pending_rows = pending(&f.observer, GUILD).await;
    let foreign_pending = pending(&f.observer, FOREIGN).await;
    let after: Vec<_> = f
        .runtime
        .published_commands()
        .expect("last confirmed snapshot survives failure")
        .iter()
        .map(|d| d.name.clone())
        .collect();
    f.close().await;
    private_reply(&observed);
    assert_saved(&rows, mutation);
    assert_eq!(facts.len(), 1, "one committed mutation audit fact");
    assert_eq!(
        publications(&observed.requests).len(),
        1,
        "fail exactly one PUT, not the mutation or its reply"
    );
    assert_eq!(
        confirmed, after,
        "unconfirmed registry must not advance discovery"
    );
    assert_eq!(foreign_pending, 0, "no cross-guild marker");
    assert_eq!(
        pending_rows, 1,
        "a committed mutation with failed publication needs one durable resync-pending row"
    );
}

#[tokio::test]
async fn define_commit_records_one_durable_pending_scope_after_publish_failure() {
    failed_commit(Mutation::Define).await;
}

#[tokio::test]
async fn disable_commit_records_one_durable_pending_scope_after_publish_failure() {
    failed_commit(Mutation::Disable).await;
}

#[tokio::test]
async fn enable_commit_records_one_durable_pending_scope_after_publish_failure() {
    failed_commit(Mutation::Enable).await;
}

#[tokio::test]
async fn failed_define_and_toggle_replies_name_saved_pending_state_without_restart_advice() {
    let mut replies = Vec::new();
    for mutation in [Mutation::Define, Mutation::Disable, Mutation::Enable] {
        let f = Fixture::new(mutation).await;
        f.remaining_failures.store(1, Ordering::SeqCst);
        replies.push((mutation, f.mutate(mutation, 1002).await));
        f.close().await;
    }
    for (mutation, reply) in replies {
        assert_pending_reply(&reply, mutation);
    }
}

async fn recovery(mutation: Mutation) {
    let f = Fixture::new(mutation).await;
    f.remaining_failures.store(1, Ordering::SeqCst);
    let observed = f.mutate(mutation, 1003).await;
    let before = definitions(&f.observer).await;
    let before_audit = audit(&f.observer).await;
    let before_pending = pending(&f.observer, GUILD).await;
    // Same runtime, executor and process. The double recovers after its one 403.
    let start = f.mock.requests().len();
    let result = f.runtime.sync_registry().await;
    let retried = f.mock.requests()[start..].to_vec();
    let after = definitions(&f.observer).await;
    let after_audit = audit(&f.observer).await;
    let after_pending = pending(&f.observer, GUILD).await;
    let snapshot: Vec<_> = f
        .runtime
        .published_commands()
        .expect("confirmed recovery snapshot")
        .iter()
        .map(|d| d.name.clone())
        .collect();
    f.close().await;
    private_reply(&observed);
    assert_saved(&before, mutation);
    assert!(
        result.is_ok(),
        "next publication opportunity succeeds: {result:?}"
    );
    assert_eq!(
        before, after,
        "publication retry must not rerun the committed mutation or touch its metadata"
    );
    assert_eq!(
        before_audit, after_audit,
        "retry must not append another mutation fact"
    );
    assert_eq!(
        retried.len(),
        1,
        "recovery only publishes; never repeats the interaction"
    );
    let sets = publications(&retried);
    assert_eq!(sets.len(), 1);
    assert_full_set(&sets[0], mutation);
    assert_eq!(
        snapshot
            .iter()
            .filter(|name| name.as_str() == mutation.expected_name())
            .count(),
        usize::from(mutation.enabled())
    );
    assert_eq!(
        after_pending, 0,
        "confirmed publication clears pending work"
    );
    assert_eq!(
        before_pending, 1,
        "recovery must begin with a durable pending marker, not an unconditional untracked PUT"
    );
}

#[tokio::test]
async fn define_recovers_on_next_publication_without_restart_or_mutation_replay() {
    recovery(Mutation::Define).await;
}

#[tokio::test]
async fn disable_recovers_on_next_publication_without_restart_or_mutation_replay() {
    recovery(Mutation::Disable).await;
}

#[tokio::test]
async fn enable_recovers_on_next_publication_without_restart_or_mutation_replay() {
    recovery(Mutation::Enable).await;
}

#[tokio::test]
async fn repeated_failed_publication_keeps_exactly_one_pending_row_until_success() {
    let f = Fixture::new(Mutation::Define).await;
    f.remaining_failures.store(2, Ordering::SeqCst);
    let observed = f.mutate(Mutation::Define, 1004).await;
    let first = pending(&f.observer, GUILD).await;
    let before = definitions(&f.observer).await;
    let before_audit = audit(&f.observer).await;
    let failed = f.runtime.sync_registry().await;
    let second = pending(&f.observer, GUILD).await;
    let succeeded = f.runtime.sync_registry().await;
    let cleared = pending(&f.observer, GUILD).await;
    let after = definitions(&f.observer).await;
    let after_audit = audit(&f.observer).await;
    let sets = publications(&f.mock.requests());
    f.close().await;
    private_reply(&observed);
    assert!(matches!(failed, Err(CustomCommandError::Publication)));
    assert!(succeeded.is_ok());
    assert_eq!(before, after);
    assert_eq!(before_audit, after_audit);
    assert_eq!(sets.len(), 4, "initial success, two failures, one recovery");
    assert_full_set(&sets[3], Mutation::Define);
    assert_eq!(cleared, 0);
    assert_eq!(
        (first, second),
        (1, 1),
        "retry failure retains one guild marker, not zero or duplicate work"
    );
}

#[tokio::test]
async fn successful_define_and_toggle_leave_no_pending_work() {
    for mutation in [Mutation::Define, Mutation::Disable, Mutation::Enable] {
        let f = Fixture::new(mutation).await;
        let observed = f.mutate(mutation, 1005).await;
        let rows = definitions(&f.observer).await;
        let pending_rows = pending(&f.observer, GUILD).await;
        f.close().await;
        let reply = private_reply(&observed).to_ascii_lowercase();
        assert_saved(&rows, mutation);
        assert_eq!(pending_rows, 0);
        assert!(!reply.contains("pending") && !reply.contains("restart"));
        let sets = publications(&observed.requests);
        assert_eq!(sets.len(), 1);
        assert_full_set(&sets[0], mutation);
    }
}

#[tokio::test]
async fn boot_resync_still_loads_saved_commands_without_replaying_mutations() {
    let f = Fixture::new(Mutation::Disable).await;
    let before = definitions(&f.observer).await;
    let before_audit = audit(&f.observer).await;
    let fresh = runtime(f.observer.clone(), f.executor.clone(), GUILD);
    assert!(fresh.published_commands().is_none());
    let start = f.mock.requests().len();
    let result = fresh.sync_registry().await;
    let requests = f.mock.requests()[start..].to_vec();
    let after = definitions(&f.observer).await;
    let after_audit = audit(&f.observer).await;
    let pending_rows = pending(&f.observer, GUILD).await;
    drop(fresh);
    f.close().await;
    assert!(result.is_ok());
    assert_eq!(before, after);
    assert_eq!(before_audit, after_audit);
    assert_eq!(pending_rows, 0);
    let sets = publications(&requests);
    assert_eq!(requests.len(), 1);
    assert_eq!(sets.len(), 1);
    assert_full_set(&sets[0], Mutation::Enable);
}

#[tokio::test]
async fn repairing_one_guild_preserves_another_guilds_durable_pending_work() {
    let f = Fixture::new(Mutation::Define).await;
    let foreign = runtime(f.observer.clone(), f.executor.clone(), FOREIGN);
    f.remaining_failures.store(2, Ordering::SeqCst);
    let local_reply = f.mutate(Mutation::Define, 1006).await;
    let foreign_reply = observe(&foreign, &f.mock, Mutation::Define.input(1007, FOREIGN)).await;
    let initial = (
        pending(&f.observer, GUILD).await,
        pending(&f.observer, FOREIGN).await,
    );
    let result = f.runtime.sync_registry().await;
    let after = (
        pending(&f.observer, GUILD).await,
        pending(&f.observer, FOREIGN).await,
    );
    drop(foreign);
    f.close().await;
    private_reply(&local_reply);
    private_reply(&foreign_reply);
    assert!(result.is_ok());
    assert_eq!(initial, (1, 1), "one durable marker for each failed guild");
    assert_eq!(
        after,
        (0, 1),
        "one guild's confirmed publication cannot clear another's pending work"
    );
}
