//! Acceptance for malformed AFK options and cross-worker guild-cap races.
//! The command runtime is real; only Discord REST is a loopback double.
//! Database fixtures are private disposable databases, never application data.

#![cfg(test)]

use super::*;
use serde_json::Value;
use two_bot_core::router::replies::UNKNOWN_COMMAND_REPLY;
use two_bot_testsupport::TestDatabase;

const NOTE: &str = "grabbing lunch back soon";
const UPDATED_NOTE: &str = "in a meeting until three";

async fn disposable_db(test: &str) -> Option<TestDatabase> {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert!(
            std::env::var("GITHUB_ACTIONS").is_err(),
            "CI must supply the guarded test database"
        );
        eprintln!("SKIP {test}: TWO_TEST_DATABASE_URL is not set");
        return None;
    };
    Some(
        TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
            .await
            .expect("migrated disposable test database"),
    )
}

fn afk_runtime(pool: PgPool, origin: String) -> Arc<CommandRuntime> {
    CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(gates(true, false)),
        GUILD,
        true,
    )
}

fn afk_slash(id: u64, member: u64, options: Vec<CommandDataOption>) -> Interaction {
    let mut interaction = from_member(
        slash("afk", Some(CHANNEL), options),
        member,
        Permissions::empty(),
    );
    interaction.id = Id::new(id);
    interaction.token = format!("afk-input-test-token-{id}");
    interaction
}

fn note_option(note: &str) -> CommandDataOption {
    option("message", CommandOptionValue::String(note.to_owned()))
}

type StatusRows = Vec<(String, String, Option<String>, String)>;

async fn status_rows(pool: &PgPool) -> StatusRows {
    sqlx::query_as(
        "SELECT guild_id, user_id, note, set_at::text FROM afk_status ORDER BY guild_id, user_id",
    )
    .fetch_all(pool)
    .await
    .expect("read AFK fixture state")
}

async fn assert_malformed_options_refused(options: Vec<CommandDataOption>, case: &str) {
    let Some(db) = disposable_db(case).await else {
        return;
    };
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = afk_runtime(db.pool().clone(), origin);
    runtime
        .on_interaction(&afk_slash(800, 77, vec![note_option(NOTE)]))
        .await;
    let before = status_rows(db.pool()).await;
    assert_eq!(before.len(), 1, "valid setup creates one AFK row");
    assert_eq!(before[0].2.as_deref(), Some(NOTE));

    // Invalid shapes must neither replace an existing row nor create a new
    // noteless status by treating a malformed message as an omitted option.
    let mut after_each = Vec::new();
    for (id, member) in [(801, 77), (802, 88)] {
        runtime
            .on_interaction(&afk_slash(id, member, options.clone()))
            .await;
        after_each.push(status_rows(db.pool()).await);
    }
    let callbacks = mock.posts_to("/callback").await;
    let bodies: Vec<Value> = callbacks
        .iter()
        .map(|request| serde_json::from_slice(&request.body).expect("callback JSON"))
        .collect();
    let effects = mock.requests().len();
    mock.shutdown().await;
    drop(runtime);
    db.close().await.expect("drop disposable test database");

    for after in after_each {
        assert_eq!(
            after, before,
            "{case}: malformed input must make zero writes"
        );
    }
    assert_eq!(bodies.len(), 3, "setup and both refusals answer once");
    assert_eq!(
        effects, 3,
        "malformed AFK input causes no other REST effects"
    );
    for body in &bodies[1..] {
        assert_eq!(body["type"], 4, "refusal is immediate, never deferred");
        assert_eq!(body["data"]["flags"], 64, "refusal is ephemeral");
        let content = body["data"]["content"].as_str().expect("refusal text");
        assert_ne!(content, UNKNOWN_COMMAND_REPLY, "AFK owns the refusal");
        assert!(
            !content.contains(UPDATED_NOTE),
            "refusal never echoes malformed input"
        );
        assert!(!content.contains("<@"), "refusal never pings members");
    }
}

#[tokio::test]
async fn afk_boolean_message_is_refused_without_writes() {
    assert_malformed_options_refused(
        vec![option("message", CommandOptionValue::Boolean(true))],
        "boolean message",
    )
    .await;
}

#[tokio::test]
async fn afk_integer_message_is_refused_without_writes() {
    assert_malformed_options_refused(
        vec![option("message", CommandOptionValue::Integer(7))],
        "integer message",
    )
    .await;
}

#[tokio::test]
async fn afk_nested_message_is_refused_without_writes() {
    assert_malformed_options_refused(
        vec![option(
            "message",
            CommandOptionValue::SubCommand(vec![note_option(UPDATED_NOTE)]),
        )],
        "nested message",
    )
    .await;
}

#[tokio::test]
async fn afk_duplicate_messages_are_refused_without_writes() {
    assert_malformed_options_refused(
        vec![note_option(UPDATED_NOTE), note_option("second note")],
        "duplicate message",
    )
    .await;
}

#[tokio::test]
async fn afk_unknown_option_is_refused_without_writes() {
    assert_malformed_options_refused(
        vec![option(
            "unexpected",
            CommandOptionValue::String(UPDATED_NOTE.to_owned()),
        )],
        "unknown option",
    )
    .await;
}

#[tokio::test]
async fn afk_valid_message_with_extra_option_is_refused_without_writes() {
    assert_malformed_options_refused(
        vec![
            note_option(UPDATED_NOTE),
            option("unexpected", CommandOptionValue::Boolean(true)),
        ],
        "message plus extra option",
    )
    .await;
}

#[tokio::test]
async fn afk_concurrent_setters_cannot_exceed_one_hundred_and_updates_still_work() {
    let Some(db) = disposable_db("concurrent AFK guild cap").await else {
        return;
    };
    sqlx::query(
        "INSERT INTO afk_status (guild_id, user_id, note, set_at) \
         SELECT $1, n::text, $2, NOW() FROM generate_series(1, 99) AS n",
    )
    .bind(GUILD_S)
    .bind(NOTE)
    .execute(db.pool())
    .await
    .expect("seed ninety-nine AFK members");

    // Stall before INSERT inside this disposable fixture. An unfenced
    // check-then-insert lets both workers reach the trigger with a count of99;
    // a correct per-guild transaction instead makes the second worker wait
    // behind the first. Observing two waiting sessions forces real overlap
    // without sleeps guessing when either caller read the count.
    sqlx::query(
        "CREATE FUNCTION afk_acceptance_insert_gate() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN PERFORM pg_advisory_xact_lock(741003); RETURN NEW; END $$",
    )
    .execute(db.pool())
    .await
    .expect("create fixture insert gate");
    sqlx::query(
        "CREATE TRIGGER afk_acceptance_insert_gate BEFORE INSERT ON afk_status \
         FOR EACH ROW EXECUTE FUNCTION afk_acceptance_insert_gate()",
    )
    .execute(db.pool())
    .await
    .expect("install fixture insert gate");
    let mut gate = db.pool().acquire().await.expect("fixture gate connection");
    sqlx::query("SELECT pg_advisory_lock(741003)")
        .execute(&mut *gate)
        .await
        .expect("hold fixture gate");

    let peer = db
        .independent_pool()
        .await
        .expect("independent worker pool");
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let first = afk_runtime(db.pool().clone(), origin.clone());
    let second = afk_runtime(peer.clone(), origin);
    let start = Arc::new(tokio::sync::Barrier::new(3));
    let mut tasks = Vec::new();
    for (id, member, runtime) in [(901, 101, first.clone()), (902, 102, second.clone())] {
        let start = start.clone();
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            runtime
                .on_interaction(&afk_slash(id, member, vec![note_option(UPDATED_NOTE)]))
                .await;
        }));
    }
    start.wait().await;
    let overlap = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity \
                 WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(db.pool())
            .await
            .expect("observe contending fixture workers");
            if waiting >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock(741003)")
        .fetch_one(&mut *gate)
        .await
        .expect("release fixture gate");
    drop(gate);
    for task in tasks {
        tokio::time::timeout(Duration::from_secs(8), task)
            .await
            .expect("setter completes after gate release")
            .expect("setter task completes");
    }
    let after_race = status_rows(db.pool()).await;
    first
        .on_interaction(&afk_slash(903, 77, vec![note_option(UPDATED_NOTE)]))
        .await;
    let after_update = status_rows(db.pool()).await;
    let callbacks = mock.posts_to("/callback").await;
    let bodies: Vec<Value> = callbacks
        .iter()
        .map(|request| serde_json::from_slice(&request.body).expect("callback JSON"))
        .collect();
    mock.shutdown().await;
    drop(first);
    drop(second);
    peer.close().await;
    db.close().await.expect("drop disposable test database");

    assert!(unlocked, "fixture gate was actually held");
    assert!(
        overlap.is_ok(),
        "both independent workers must contend before release"
    );
    assert_eq!(
        after_race.len(),
        100,
        "100 is a hard guild bound, even across workers"
    );
    assert_eq!(
        after_race
            .iter()
            .filter(|row| row.1 == "101" || row.1 == "102")
            .count(),
        1,
        "exactly one newcomer takes the last slot"
    );
    assert_eq!(
        after_update.len(),
        100,
        "updates at capacity never add a row"
    );
    let updated = after_update
        .iter()
        .find(|row| row.1 == "77")
        .expect("existing member retained");
    assert_eq!(
        updated.2.as_deref(),
        Some(UPDATED_NOTE),
        "existing members can update at capacity"
    );
    assert_eq!(
        bodies.len(),
        3,
        "both contenders and the existing-member update answer once"
    );
    for body in &bodies {
        assert_eq!(body["type"], 4, "capacity replies are immediate");
        assert_eq!(body["data"]["flags"], 64, "capacity replies are ephemeral");
        assert_ne!(body["data"]["content"], UNKNOWN_COMMAND_REPLY);
    }
    assert_eq!(
        bodies[..2]
            .iter()
            .filter(|body| body["data"]["content"]
                .as_str()
                .is_some_and(|text| text.contains(UPDATED_NOTE)))
            .count(),
        1,
        "only the winning newcomer receives a successful set confirmation"
    );
    assert!(bodies[2]["data"]["content"]
        .as_str()
        .expect("update confirmation")
        .contains(UPDATED_NOTE));
}
