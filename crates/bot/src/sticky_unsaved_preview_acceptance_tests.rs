//! Protected acceptance: preview proposed sticky input without consulting or
//! changing saved configuration. Only real interaction dispatch is exercised.
#![cfg(test)]

use super::*;
use futures_util::FutureExt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use two_bot_testsupport::TestDatabase;

const OLD: &str = "8000000000000000001";
const BODY_ERROR: &str = "Sticky body must be between 1 and 2000 characters.";
const DEBOUNCE_ERROR: &str = "Debounce must be between 1 and 300 seconds.";

fn proposed(body: &str, debounce: Option<i64>, preview: Option<bool>) -> Interaction {
    let mut options = vec![option("body", CommandOptionValue::String(body.into()))];
    if let Some(value) = debounce {
        options.push(option("debounce", CommandOptionValue::Integer(value)));
    }
    if let Some(value) = preview {
        options.push(option("preview", CommandOptionValue::Boolean(value)));
    }
    slash("sticky", Some(CHANNEL), options)
}

async fn closed_runtime(
    router_gates: RouterGates,
    automations: bool,
) -> (Arc<CommandRuntime>, MockRest) {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://agent_test@127.0.0.1:1/agent_test")
        .expect("unavailable fixture pool");
    pool.close().await;
    let runtime = CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(router_gates),
        GUILD,
        automations,
    );
    (runtime, mock)
}

// Accept either an immediate private response or the existing private defer/edit
// lifecycle. Extra REST calls (including channel reads/writes) are forbidden.
fn private_reply(mock: &MockRest) -> String {
    let requests = mock.requests();
    assert!(!requests.is_empty(), "interaction must receive an answer");
    let callback: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("callback JSON");
    assert_eq!(requests[0].method, "POST");
    assert!(requests[0].path.ends_with("/callback"));
    assert_eq!(callback["data"]["flags"].as_u64().unwrap_or(0) & 64, 64);
    let payload = if callback["type"] == 4 {
        assert_eq!(requests.len(), 1, "one complete private response only");
        callback["data"].clone()
    } else {
        assert_eq!(callback["type"], 5, "private reply or private defer");
        assert_eq!(
            requests.len(),
            2,
            "defer and one edit only; no channel effects"
        );
        assert_eq!(requests[1].method, "PATCH");
        assert_eq!(
            requests[1].path,
            "/api/v10/webhooks/1111/sticky-test-token/messages/@original"
        );
        serde_json::from_slice(&requests[1].body).expect("completion JSON")
    };
    assert_eq!(payload["allowed_mentions"]["parse"], serde_json::json!([]));
    for key in ["users", "roles"] {
        assert!(payload["allowed_mentions"][key]
            .as_array()
            .is_none_or(Vec::is_empty));
    }
    assert_ne!(payload["allowed_mentions"]["replied_user"], true);
    let text = payload["content"]
        .as_str()
        .expect("plain-text sticky response");
    assert!(!text.is_empty());
    assert!(text.encode_utf16().count() <= 2000, "Discord UTF-16 limit");
    assert!(!text.contains('\u{fffd}'), "never split a Unicode scalar");
    for unsafe_mention in ["@everyone", "@here", "<@"] {
        assert!(
            !text.contains(unsafe_mention),
            "neutralize {unsafe_mention}"
        );
    }
    text.into()
}

fn assert_preview(text: &str, debounce: u64) {
    assert!(
        text.contains("Preview — not saved"),
        "explicit unsaved label: {text}"
    );
    let lower = text.to_lowercase();
    assert!(text.contains(&debounce.to_string()) && lower.contains("debounce"));
    assert!(lower.contains("permission") && lower.contains("state"));
    assert!(lower.contains("not verified") || lower.contains("not checked"));
    assert!(lower.contains("save") && lower.contains("validat"));
    assert!(text.contains("/sticky"), "copyable save command");
    assert!(
        lower.contains("preview:false")
            || lower.contains("without preview")
            || lower.contains("omit"),
        "explain how to save instead of previewing: {text}"
    );
    assert!(
        !text.starts_with("Sticky set"),
        "preview is not a save receipt"
    );
}

async fn answer(input: &Interaction) -> String {
    let (runtime, mock) = closed_runtime(gates(true, true), true).await;
    runtime.on_interaction(input).await;
    let text = private_reply(&mock);
    mock.shutdown().await;
    text
}

// Catches falling through preview:true to save or requiring saved-state reads.
#[tokio::test]
async fn valid_preview_works_with_unavailable_store_and_preserves_proposed_fields() {
    for (body, debounce, expected) in [
        ("read the rules", None, 5),
        ("café 😀\nsecond line", Some(1), 1),
        ("می\u{200c}روم", Some(300), 300),
    ] {
        let text = answer(&proposed(body, debounce, Some(true))).await;
        assert_preview(&text, expected);
        assert!(
            text.contains(body),
            "show proposed text, not saved text: {text}"
        );
        assert!(
            !text.to_lowercase().contains("truncat"),
            "short input stays intact"
        );
    }
}

// Catches preview using a weaker validator, auditing invalid proposals, or
// truncating invalid input into apparently valid input before validation.
#[tokio::test]
async fn invalid_preview_uses_save_validation_before_persistence() {
    let cases = vec![
        (String::new(), None, BODY_ERROR),
        (" \t\n".into(), None, BODY_ERROR),
        ("\u{200b}\u{200c}\u{200d}\u{feff}".into(), None, BODY_ERROR),
        ("x".repeat(2001), None, BODY_ERROR),
        ("😀".repeat(1001), None, BODY_ERROR),
        ("x".into(), Some(-1), DEBOUNCE_ERROR),
        ("x".into(), Some(0), DEBOUNCE_ERROR),
        ("x".into(), Some(301), DEBOUNCE_ERROR),
        ("x".into(), Some(i64::MAX), DEBOUNCE_ERROR),
    ];
    for (body, debounce, expected) in cases {
        let text = answer(&proposed(&body, debounce, Some(true))).await;
        assert!(text.contains(expected), "same validation as save: {text}");
        assert!(!text.starts_with("Sticky set"));
        assert_ne!(
            text, STICKY_STORE_FAILURE_REPLY,
            "validation needs no store"
        );
    }
}

// Catches byte/char-based framing, silent truncation, mention reactivation, or
// losing the label/action when a valid maximum-size body fills the response.
#[tokio::test]
async fn maximum_valid_unicode_preview_is_bounded_and_explicitly_truncated() {
    for body in ["x".repeat(2000), "é".repeat(2000), "😀".repeat(1000)] {
        let text = answer(&proposed(&body, Some(300), Some(true))).await;
        assert_preview(&text, 300);
        assert!(
            text.to_lowercase().contains("truncat"),
            "explicit truncation marker"
        );
        assert!(text.contains(&body.chars().take(16).collect::<String>()));
    }
    let hostile = format!("{} @everyone @here <@123> <@&456>", "😀".repeat(975));
    assert!(hostile.encode_utf16().count() <= 2000);
    let text = answer(&proposed(&hostile, None, Some(true))).await;
    assert_preview(&text, 5);
}

// Catches dropping proposed mentions or leaving them active; short input cannot
// accidentally satisfy the mention-safety oracle merely by being truncated.
#[tokio::test]
async fn short_preview_neutralizes_mentions_without_dropping_proposed_content() {
    let text = answer(&proposed(
        "before @everyone @here <@123456789012345678> <@&234567890123456789> after",
        None,
        Some(true),
    ))
    .await;
    assert_preview(&text, 5);
    for retained in [
        "before",
        "everyone",
        "here",
        "123456789012345678",
        "234567890123456789",
        "after",
    ] {
        assert!(text.contains(retained), "preserve proposed content: {text}");
    }
    assert!(!text.to_lowercase().contains("truncat"));
}

fn malformed() -> Vec<Interaction> {
    let mut inputs = Vec::new();
    for value in [
        CommandOptionValue::String("true @everyone".into()),
        CommandOptionValue::String("false".into()),
        CommandOptionValue::Integer(1),
        CommandOptionValue::Integer(0),
        CommandOptionValue::Number(1.0),
        CommandOptionValue::SubCommand(vec![]),
    ] {
        let mut input = proposed("must not save", None, None);
        if let Some(InteractionData::ApplicationCommand(command)) = input.data.as_mut() {
            command.options.push(option("preview", value));
        }
        inputs.push(input);
    }
    for values in [[true, false], [false, true], [true, true], [false, false]] {
        let mut input = proposed("must not save", None, Some(values[0]));
        if let Some(InteractionData::ApplicationCommand(command)) = input.data.as_mut() {
            command
                .options
                .push(option("preview", CommandOptionValue::Boolean(values[1])));
        }
        inputs.push(input);
    }
    for name in ["Preview", "previewed", "dry-run", "unknown"] {
        for preview in [None, Some(true), Some(false)] {
            let mut input = proposed("must not save", None, preview);
            if let Some(InteractionData::ApplicationCommand(command)) = input.data.as_mut() {
                command
                    .options
                    .push(option(name, CommandOptionValue::Boolean(true)));
            }
            inputs.push(input);
        }
    }
    inputs
}

// Catches truthy coercion, first/last duplicate wins, and forged-flag save.
#[tokio::test]
async fn malformed_preview_flags_refuse_privately_instead_of_saving() {
    for input in malformed() {
        let text = answer(&input).await;
        let lower = text.to_lowercase();
        assert!(
            lower.contains("invalid") || lower.contains("unknown") || lower.contains("unexpected")
        );
        assert!(text.contains("/sticky"), "refusal gives a correction");
        assert!(
            !text.contains("Preview — not saved"),
            "malformed is not a successful preview"
        );
        assert_ne!(
            text, STICKY_STORE_FAILURE_REPLY,
            "malformed flags stop before storage"
        );
    }
}

// Catches a new preview path bypassing the existing guild/staff/feature gates.
#[tokio::test]
async fn preview_obeys_existing_authority_and_activation_fences() {
    for case in 0..5 {
        let mut input = proposed("private proposed sentinel", None, Some(true));
        let mut router_gates = gates(true, true);
        match case {
            0 => input.member.as_mut().expect("member").permissions = Some(Permissions::empty()),
            1 => input.member.as_mut().expect("member").permissions = None,
            2 => set_guild(&mut input, Some(Id::new(9999))),
            3 => set_guild(&mut input, None),
            4 => router_gates.automations = false,
            _ => unreachable!(),
        }
        let (runtime, mock) = closed_runtime(router_gates, router_gates.automations).await;
        runtime.on_interaction(&input).await;
        if matches!(case, 2 | 3) {
            assert!(
                mock.requests().is_empty(),
                "foreign and missing guilds retain the existing silent fence"
            );
        } else {
            let text = private_reply(&mock);
            assert!(!text.contains("private proposed sentinel"));
            assert!(!text.contains("Preview — not saved"));
            assert_ne!(text, STICKY_STORE_FAILURE_REPLY);
        }
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn publication_advertises_optional_boolean_preview_with_existing_bounds() {
    let (runtime, mock) = closed_runtime(gates(true, true), true).await;
    runtime
        .publish_registry_checked(Some(1111))
        .await
        .expect("fixture publication");
    let requests = mock.requests();
    let publication = requests
        .iter()
        .find(|r| r.method == "PUT")
        .expect("registry PUT");
    let commands: serde_json::Value =
        serde_json::from_slice(&publication.body).expect("registry JSON");
    let sticky = commands
        .as_array()
        .expect("commands")
        .iter()
        .find(|c| c["name"] == "sticky")
        .expect("sticky");
    let options = sticky["options"].as_array().expect("options");
    let preview: Vec<_> = options.iter().filter(|o| o["name"] == "preview").collect();
    assert_eq!(preview.len(), 1, "one published preview option");
    assert_eq!(preview[0]["type"], 5, "Discord Boolean");
    assert_ne!(preview[0]["required"], true);
    let body = options.iter().find(|o| o["name"] == "body").expect("body");
    assert_eq!(body["required"], true);
    assert_eq!(body["max_length"], 2000);
    let debounce = options
        .iter()
        .find(|o| o["name"] == "debounce")
        .expect("debounce");
    assert_eq!(debounce["min_value"], 1);
    assert_eq!(debounce["max_value"], 300);
    mock.shutdown().await;
}

#[tokio::test]
async fn private_help_discovers_preview_without_executing_sticky() {
    let (runtime, mock) = closed_runtime(gates(true, true), true).await;
    runtime
        .publish_registry_checked(Some(1111))
        .await
        .expect("fixture publication");
    mock.recorded.lock().expect("requests").clear();
    runtime
        .on_interaction(&slash(
            "help",
            Some(CHANNEL),
            vec![option(
                "command",
                CommandOptionValue::String("sticky".into()),
            )],
        ))
        .await;
    let text = private_reply(&mock).to_lowercase();
    for token in ["/sticky", "preview", "optional", "boolean"] {
        assert!(text.contains(token), "help exposes {token}: {text}");
    }
    mock.shutdown().await;
}

// Always tear down the owned test database, including assertion-failure RED.
async fn fixture(
    run: impl for<'a> FnOnce(&'a TestDatabase) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
) {
    let Ok(url) = std::env::var("TWO_TEST_DATABASE_URL") else {
        assert_ne!(
            std::env::var("GITHUB_ACTIONS").as_deref(),
            Ok("true"),
            "CI must configure sticky preview acceptance: TWO_TEST_DATABASE_URL"
        );
        eprintln!("SKIP sticky preview acceptance: TWO_TEST_DATABASE_URL is not configured");
        return;
    };
    let db = TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
        .await
        .expect("guarded disposable database");
    let result = AssertUnwindSafe(run(&db)).catch_unwind().await;
    db.close().await.expect("teardown even on behavioral RED");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn seed(db: &TestDatabase, uncertain: bool) {
    store::put_sticky(
        db.pool(),
        &PutSticky {
            guild_id: GUILD_S,
            channel_id: CHANNEL_S,
            body: "working saved sticky",
            debounce_seconds: 7,
            enabled: true,
            actor_id: "77",
            now_ms: 1_800_000_000_000,
        },
    )
    .await
    .expect("seed saved sticky");
    sqlx::query("UPDATE sticky_messages SET last_message_id = $1, last_posted_at = clock_timestamp() - interval '2 minutes', claim_token = 'prior-delivery', claimed_at = clock_timestamp() - interval '2 minutes', delivery_uncertain = $2")
        .bind(OLD).bind(uncertain).execute(db.pool()).await.expect("seed delivery state");
}

async fn snapshot(pool: &PgPool) -> serde_json::Value {
    sqlx::query_scalar("SELECT jsonb_build_object('sticky', (SELECT COALESCE(jsonb_agg(to_jsonb(s) ORDER BY guild_id, channel_id), '[]') FROM sticky_messages s), 'audit', (SELECT COALESCE(jsonb_agg(to_jsonb(a) ORDER BY id), '[]') FROM automation_audit_log a), 'jobs', (SELECT COALESCE(jsonb_agg(to_jsonb(j) ORDER BY id), '[]') FROM scheduled_messages j))")
        .fetch_one(pool).await.expect("whole rows including claim/fence/timestamps/audits/jobs")
}

// Observe acquisition at the real SQL boundary. The warmed single-connection
// pool avoids network timing; both reads and rolled-back writes trip the spy.
async fn observed_runtime(
    db: &TestDatabase,
) -> (Arc<CommandRuntime>, MockRest, PgPool, Arc<AtomicU64>) {
    let calls = Arc::new(AtomicU64::new(0));
    let spy = calls.clone();
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .min_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .before_acquire(move |_, _| {
            spy.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(true) })
        })
        .connect_with((*db.pool().connect_options()).clone())
        .await
        .expect("observed fixture pool");
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = CommandRuntime::new(
        pool.clone(),
        executor_at(origin),
        router_with_commands(gates(true, true)),
        GUILD,
        true,
    );
    calls.store(0, Ordering::SeqCst);
    (runtime, mock, pool, calls)
}

// Catches even read-only preview lookup and transient/rolled-back effects;
// row equality separately protects saved content, trigger/fence and timestamps.
#[tokio::test]
async fn valid_invalid_and_forged_preview_attempt_zero_store_operations() {
    fixture(|db| {
        Box::pin(async move {
            for existing in [false, true] {
                if existing {
                    seed(db, true).await;
                }
                let before = snapshot(db.pool()).await;
                let mut inputs = vec![
                    proposed("proposed replacement", Some(9), Some(true)),
                    proposed("", None, Some(true)),
                    proposed("body", Some(0), Some(true)),
                ];
                inputs.extend(malformed());
                let mut unauthorized = proposed("private proposal", None, Some(true));
                unauthorized.member.as_mut().expect("member").permissions =
                    Some(Permissions::empty());
                inputs.push(unauthorized);
                let mut foreign = proposed("private proposal", None, Some(true));
                set_guild(&mut foreign, Some(Id::new(9999)));
                inputs.push(foreign);
                for input in inputs {
                    let (runtime, mock, pool, calls) = observed_runtime(db).await;
                    runtime.on_interaction(&input).await;
                    assert_eq!(
                        calls.load(Ordering::SeqCst),
                        0,
                        "preview/refusal must not even acquire a store connection"
                    );
                    assert_eq!(
                        snapshot(db.pool()).await,
                        before,
                        "no audit, sticky, claim, fence or job mutation"
                    );
                    if input.guild_id == Some(Id::new(GUILD)) {
                        private_reply(&mock);
                    } else {
                        assert!(mock.requests().is_empty(), "foreign-guild silent fence");
                    }
                    drop(runtime);
                    pool.close().await;
                    mock.shutdown().await;
                }
            }
        })
    })
    .await;
}

// Catches resetting delivery uncertainty merely by previewing, including after
// restart/lease expiry. The explicit false save remains the supported reset.
#[tokio::test]
async fn preview_preserves_uncertainty_then_false_save_revalidates_and_recovers() {
    fixture(|db| Box::pin(async move {
        seed(db, true).await;
        let before = snapshot(db.pool()).await;
        let (runtime, mock, pool, calls) = observed_runtime(db).await;
        runtime.on_interaction(&proposed("replacement", Some(9), Some(true))).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(snapshot(db.pool()).await, before);
        assert_preview(&private_reply(&mock), 9);
        drop(runtime);
        pool.close().await;
        mock.shutdown().await;
        assert!(store::claim_sticky_post(db.pool(), GUILD_S, CHANNEL_S, "fresh-worker", now_millis_for_test() + 120_000).await.expect("fence probe").is_none());
        let (runtime, mock, pool, calls) = observed_runtime(db).await;
        runtime.on_interaction(&proposed("replacement", Some(9), Some(false))).await;
        assert!(calls.load(Ordering::SeqCst) > 0, "save is a live positive control for the store spy");
        assert_eq!(private_reply(&mock), "Sticky set for <#3333>, 9s debounce.");
        let row: (String, i32, bool, Option<String>, Option<String>) = sqlx::query_as("SELECT body, debounce_seconds, delivery_uncertain, claim_token, last_message_id FROM sticky_messages").fetch_one(db.pool()).await.expect("saved configuration");
        assert_eq!(row, ("replacement".into(), 9, false, None, Some(OLD.into())));
        let audit: Vec<(String, String)> = sqlx::query_as("SELECT action, outcome FROM automation_audit_log ORDER BY created_at, id").fetch_all(db.pool()).await.expect("save audit");
        assert_eq!(audit, vec![("sticky.update".into(), "ok".into())]);
        drop(runtime);
        pool.close().await;
        mock.shutdown().await;
    })).await;
}

// Catches changing omitted/false saving, raw content persistence or audit, and
// proves invalid save still audits while invalid preview is completely inert.
#[tokio::test]
async fn control_omitted_false_save_and_invalid_save_keep_existing_semantics() {
    fixture(|db| {
        Box::pin(async move {
            for (preview, body, debounce, expected) in [
                (None, "café 😀", None, 5_i32),
                (Some(false), "other proposed body", Some(300), 300),
            ] {
                if preview == Some(false) {
                    seed(db, true).await;
                }
                let (runtime, mock, pool, calls) = observed_runtime(db).await;
                runtime
                    .on_interaction(&proposed(body, debounce, preview))
                    .await;
                assert!(calls.load(Ordering::SeqCst) > 0);
                assert!(private_reply(&mock).starts_with("Sticky set for <#3333>"));
                let row: (String, i32) =
                    sqlx::query_as("SELECT body, debounce_seconds FROM sticky_messages")
                        .fetch_one(db.pool())
                        .await
                        .expect("saved fields");
                assert_eq!(row, (body.into(), expected));
                drop(runtime);
                pool.close().await;
                mock.shutdown().await;
            }
            let recovery: (bool, Option<String>, Option<String>) = sqlx::query_as(
                "SELECT delivery_uncertain, claim_token, last_message_id FROM sticky_messages",
            )
            .fetch_one(db.pool())
            .await
            .expect("false save resets uncertainty independently of preview RED");
            assert_eq!(recovery, (false, None, Some(OLD.into())));
            let before = snapshot(db.pool()).await;
            let (runtime, mock, pool, calls) = observed_runtime(db).await;
            runtime
                .on_interaction(&proposed("", None, Some(false)))
                .await;
            assert_eq!(private_reply(&mock), BODY_ERROR);
            assert!(
                calls.load(Ordering::SeqCst) > 0,
                "invalid save retains rejected audit"
            );
            let after = snapshot(db.pool()).await;
            assert_eq!(after["sticky"], before["sticky"]);
            let audit: Vec<(String, String)> = sqlx::query_as(
                "SELECT action, outcome FROM automation_audit_log ORDER BY created_at, id",
            )
            .fetch_all(db.pool())
            .await
            .expect("audits");
            assert_eq!(
                audit,
                vec![
                    ("sticky.create".into(), "ok".into()),
                    ("sticky.update".into(), "ok".into()),
                    ("sticky.update".into(), "rejected".into())
                ]
            );
            drop(runtime);
            pool.close().await;
            mock.shutdown().await;
        })
    })
    .await;
}

#[tokio::test]
async fn control_remove_still_deletes_saved_message_and_audits() {
    fixture(|db| {
        Box::pin(async move {
            seed(db, true).await;
            let (runtime, mock, pool, calls) = observed_runtime(db).await;
            runtime
                .on_interaction(&slash("sticky-remove", Some(CHANNEL), vec![]))
                .await;
            assert!(calls.load(Ordering::SeqCst) > 0);
            assert_eq!(mock.deferred_reply()["content"], "Sticky removed.");
            assert_eq!(mock.deletes().len(), 1);
            assert_eq!(
                mock.deletes()[0].path,
                format!("/api/v10/channels/{CHANNEL}/messages/{OLD}")
            );
            let after = snapshot(db.pool()).await;
            assert_eq!(after["sticky"], serde_json::json!([]));
            assert_eq!(after["audit"].as_array().expect("audit").len(), 1);
            assert_eq!(after["audit"][0]["action"], "sticky.delete");
            assert_eq!(after["audit"][0]["outcome"], "ok");
            drop(runtime);
            pool.close().await;
            mock.shutdown().await;
        })
    })
    .await;
}
