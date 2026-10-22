//! Protected acceptance: real CommandRuntime and boot-fenced shared poll supervisor.
//! Product fixes belong to the implementation owner; do not edit these assertions.
#![cfg(test)]

use super::*;
use crate::command_runtime::{router_with_commands, CommandRuntime};
use crate::poll_jobs;
use sqlx::Row;
use tokio::sync::Notify;
use twilight_model::application::interaction::Interaction;
use two_bot_core::RouterGates;

fn gates() -> RouterGates {
    RouterGates {
        configured_guild: Some(fixtures::STAGING_GUILD),
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
    }
}

fn slash(id: u64, options: Value) -> Interaction {
    serde_json::from_value(json!({
        "application_id":"1111","authorizing_integration_owners":{"0":fixtures::STAGING_GUILD.to_string()},
        "id":id.to_string(),"token":"synthetic-poll-callback","type":2,"version":1,
        "guild_id":fixtures::STAGING_GUILD.to_string(),"channel":{"id":"3333","type":0},
        "member":{"permissions":"0","roles":[],"deaf":false,"mute":false,"flags":0,
            "user":{"id":"88","username":"fixture","discriminator":"0","bot":false}},
        "data":{"id":"4444","name":"poll","type":1,"options":options}
    })).expect("wire slash")
}

fn context(pool: PgPool, rest: ActionExecutor) -> Arc<Context> {
    let cell = OnceCell::new();
    cell.set(pool).expect("fixture pool once");
    Arc::new(Context {
        url: String::new(),
        pool: cell,
        rest,
        guild: fixtures::STAGING_GUILD.to_string(),
        observation: Mutex::new(()),
        events_gate: JobGate::default(),
    })
}

// Env-sensitive registration is run in isolated child test processes, never by
// mutating the environment shared with other Rust tests.
#[test]
fn poll_boot_gate_and_shared_supervisor_runtime_acceptance() {
    let target = format!(
        "{}::poll_boot_runtime_fixture",
        module_path!().split_once("::").unwrap().1
    );
    for gate in ["0", "1", "invalid"] {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", &target, "--nocapture", "--test-threads=1"])
            .env("TWO_POLL_RUNTIME_ACCEPTANCE", "1")
            .env("TWO_ANNOUNCEMENTS", gate)
            .output()
            .expect("isolated poll registration process");
        assert!(
            output.status.success(),
            "poll gate {gate}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }
}

#[tokio::test]
async fn poll_boot_runtime_fixture() {
    if std::env::var("TWO_POLL_RUNTIME_ACCEPTANCE").as_deref() != Ok("1") {
        return;
    }
    let mock = MockRest::with_responder(|r| {
        if r.path.contains("/interactions/") {
            ScriptedResponse::status(204)
        } else {
            ScriptedResponse::json(200, json!({"id":"99"}))
        }
    })
    .await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test:@agent-testdb:5432/two_bot_test_local")
        .unwrap();
    let ctx = context(pool, executor(&mock));
    for (_, refused) in fixtures::refused() {
        assert!(
            poll_jobs::register(ctx.clone(), &refused).is_none(),
            "uncleared identity must never schedule poll writes"
        );
    }
    let enabled = std::env::var("TWO_ANNOUNCEMENTS").as_deref() == Ok("1");
    let registered = poll_jobs::register(ctx, &fixtures::staging());
    assert_eq!(
        registered.is_some(),
        enabled,
        "the announcements flag is narrowing, invalid fails closed"
    );
    if !enabled {
        let statuses = registered_statuses(&[], &[])
            .await
            .expect("parked production status inventory");
        let entries = statuses.read().await;
        assert!(entries["polls"].parked && !entries["polls"].running);
        assert!(mock.requests().is_empty());
        return;
    }
    let job = registered.unwrap();
    assert_eq!(job.name, "polls");
    assert_eq!(job.cadence, Duration::from_secs(15));
    // Lazy registration above must not open a DB or touch Discord.
    assert!(mock.requests().is_empty());
    let url = match std::env::var("TWO_TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(std::env::VarError::NotPresent) => {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "CI must configure the disposable poll runtime DB"
            );
            eprintln!("SKIP poll runtime DB journey: TWO_TEST_DATABASE_URL not set");
            return;
        }
        Err(_) => panic!("Unicode fixture URL"),
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable DB");
    let runtime = CommandRuntime::new(
        db.pool().clone(),
        executor(&mock),
        router_with_commands(gates()),
        fixtures::STAGING_GUILD,
        false,
    );
    runtime
        .on_interaction(&slash(
            9100,
            json!([{"name":"create","type":1,"options":[
                {"name":"question","type":3,"value":"real bot dispatch"},
                {"name":"option1","type":3,"value":"one"},
                {"name":"option2","type":3,"value":"two"},
                {"name":"duration-minutes","type":4,"value":1}
            ]}]),
        ))
        .await;
    let poll: String =
        sqlx::query_scalar("SELECT id FROM polls WHERE question='real bot dispatch'")
            .fetch_one(db.pool())
            .await
            .expect("CommandRuntime must dispatch a member poll create");
    let mut vote: Value = serde_json::to_value(slash(9101, json!([]))).unwrap();
    vote["type"] = json!(3);
    vote["data"] = json!({"custom_id":format!("poll:{poll}:0"),"component_type":2});
    runtime
        .on_interaction(&serde_json::from_value(vote).expect("wire vote"))
        .await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM poll_votes WHERE poll_id=$1")
        .bind(&poll)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "real bot component dispatch persists the member vote"
    );
    sqlx::query("UPDATE polls SET ends_at=now()-interval '1 minute' WHERE id=$1")
        .bind(&poll)
        .execute(db.pool())
        .await
        .unwrap();
    let ctx = context(db.pool().clone(), executor(&mock));
    let mut job = poll_jobs::register(ctx, &fixtures::staging()).expect("staging registration");
    let completed = Arc::new(Notify::new());
    let signal = completed.clone();
    let action = job.action.clone();
    job.action = Arc::new(move || {
        let future = action();
        let signal = signal.clone();
        Box::pin(async move {
            let outcome = future.await;
            signal.notify_one();
            outcome
        })
    });
    let status = registered_statuses(std::slice::from_ref(&job), &[])
        .await
        .expect("production job-status inventory");
    let (shutdown, rx) = watch::channel(false);
    let supervisor = tokio::spawn(jobs::supervise(vec![job], status.clone(), rx));
    tokio::time::timeout(Duration::from_secs(25), completed.notified())
        .await
        .expect("real shared supervisor runs registered poll job");
    shutdown.send_replace(true);
    supervisor
        .await
        .expect("supervisor task")
        .expect("registered status");
    let row=sqlx::query("SELECT status,counts_json::text AS counts,results_posted_at IS NOT NULL AS posted FROM polls WHERE id=$1")
        .bind(&poll).fetch_one(db.pool()).await.unwrap();
    let statuses = status.read().await;
    let succeeded = statuses["polls"].last_success.is_some();
    drop(statuses);
    let posts = mock
        .requests()
        .iter()
        .filter(|r| r.method == "POST" && r.path == "/api/v10/channels/3333/messages")
        .count();
    let before_restart = mock.requests().len();
    let restarted = poll_jobs::register(
        context(db.pool().clone(), executor(&mock)),
        &fixtures::staging(),
    )
    .unwrap();
    (restarted.action)()
        .await
        .expect("fresh job recovers durable state");
    let after_restart = mock.requests().len();
    db.close().await.expect("drop fixture");
    assert_eq!(
        after_restart, before_restart,
        "fresh job must not replay posted results"
    );
    assert_eq!(row.get::<String, _>("status"), "closed");
    assert_eq!(row.get::<String, _>("counts"), "[1, 0]");
    assert!(row.get::<bool, _>("posted"));
    assert!(succeeded, "job readiness records successful real execution");
    assert_eq!(
        posts, 2,
        "one creation and one supervised result, not a duplicate"
    );
}
