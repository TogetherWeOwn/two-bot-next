//! Protected tests-only proposed-schedule contract. No implementation is supplied.
//! Breaks caught: preview falling through to create, minting IDs before validation,
//! repeat clock reads, offset/DST arithmetic, discarded metadata at the text cap,
//! unsafe rendering, lenient boolean decoding, and changing ordinary create audits.
//! The subject is the real handler; only clock, ID and store I/O are substituted.
//! Full CommandRuntime dispatch/private-defer controls live in the companion file.
#![cfg(test)]

extern crate self as two_bot_core;
extern crate two_bot_core as real_core;

pub use real_core::*;
#[path = "support/schedule_preview_spies.rs"]
mod spies;
pub use spies::{command_runtime, db_log, funnel, scheduled_store};
#[path = "../../discord/tests/common/mod.rs"]
pub mod discord_test_common;
#[path = "../src/schedule_runtime.rs"]
mod subject;

use discord_test_common::{MockRest, ScriptedResponse};
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use twilight_model::application::interaction::{
    application_command::{CommandDataOption, CommandOptionValue},
    Interaction, InteractionData,
};
use two_bot_discord::ActionExecutor;

fn option(name: &str, value: CommandOptionValue) -> CommandDataOption {
    CommandDataOption {
        name: name.into(),
        value,
    }
}

fn invocation(options: Vec<CommandDataOption>) -> Interaction {
    let mut interaction: Interaction = serde_json::from_value(json!({
        "id":"7", "application_id":"1111", "type":2, "token":"preview-fixture",
        "guild_id":"2222", "channel_id":"3333",
        "channel":{"id":"3333","guild_id":"2222","type":0},
        "authorizing_integration_owners":{"0":"2222"},
        "data":{"id":"1","name":"schedule","type":1},
        "member":{"user":{"id":"88","username":"staff","discriminator":"0","avatar":null},
                  "roles":[],"deaf":false,"mute":false,"flags":0,"permissions":"32"}
    }))
    .expect("complete synthetic guild interaction");
    let Some(InteractionData::ApplicationCommand(data)) = &mut interaction.data else {
        panic!("fixture application command");
    };
    data.options = options;
    interaction
}

fn proposal(
    body: &str,
    minutes: Option<i64>,
    every: Option<i64>,
    preview: Option<bool>,
) -> Vec<CommandDataOption> {
    let mut options = vec![option("body", CommandOptionValue::String(body.into()))];
    if let Some(minutes) = minutes {
        options.push(option("in-minutes", CommandOptionValue::Integer(minutes)));
    }
    if let Some(every) = every {
        options.push(option("every-minutes", CommandOptionValue::Integer(every)));
    }
    if let Some(preview) = preview {
        options.push(option("preview", CommandOptionValue::Boolean(preview)));
    }
    options
}

async fn pool() -> sqlx::PgPool {
    // No URL, credential, DNS lookup or connect attempt. Any accidental direct
    // SQL bypass of the store spies fails immediately against this closed pool.
    let pool = PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new());
    pool.close().await;
    pool
}

async fn answer(options: Vec<CommandDataOption>) -> String {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mock = MockRest::start(Vec::new(), ScriptedResponse::json(200, json!({"id":"99"}))).await;
    let executor = ActionExecutor::with_proxy("fixture-token".into(), Some(mock.origin()))
        .expect("loopback executor");
    subject::schedule_create(&pool().await, &executor, "2222", &invocation(options)).await;
    let requests = mock.requests();
    mock.shutdown().await;
    assert_eq!(requests.len(), 1, "only completion of the caller's private defer; no channel send, followup, lookup or execution: {requests:?}");
    assert_eq!(requests[0].method, "PATCH");
    assert!(requests[0].path.ends_with("/messages/@original"));
    let payload: Value = serde_json::from_slice(&requests[0].body).expect("reply payload");
    assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
    for key in ["users", "roles"] {
        assert!(payload["allowed_mentions"][key]
            .as_array()
            .is_none_or(Vec::is_empty));
    }
    let text = payload["content"]
        .as_str()
        .expect("text preview")
        .to_owned();
    assert!(text.encode_utf16().count() <= 2000, "bounded Discord reply");
    text
}

fn no_persistent_effects() {
    let trace = spies::snapshot();
    assert_eq!(
        trace.ids, 0,
        "preview must not mint even an unused persistent ID: {trace:?}"
    );
    assert!(
        trace.calls.is_empty(),
        "preview attempted store/audit/quota/queue I/O: {trace:?}"
    );
    assert!(trace.writes.is_empty() && trace.audits.is_empty());
}

fn preview(text: &str, iso: &str, timestamp: &str) {
    assert!(
        text.contains("Preview — not scheduled"),
        "missing explicit non-scheduled label: {text}"
    );
    assert!(
        text.contains("3333"),
        "show the invoking destination: {text}"
    );
    assert!(
        text.contains(iso),
        "show explicit canonical UTC, not local wall time: {text}"
    );
    assert!(
        text.contains(timestamp) || text.contains(&timestamp.replace(":F>", ":f>")),
        "Discord-local instant: {text}"
    );
    let lower = text.to_lowercase();
    assert!(
        lower.contains("creat")
            && (lower.contains("revalidat")
                || lower.contains("recheck")
                || lower.contains("check again")),
        "create must revalidate later: {text}"
    );
    assert!(
        (lower.contains("tim") || lower.contains("future"))
            && (lower.contains("availab") || lower.contains("capacity") || lower.contains("quota")),
        "disclose timing and availability can change: {text}"
    );
    for forbidden in [
        "acceptance-persistent-id-",
        "discord.com/channels/",
        "discordapp.com/channels/",
        "will be sent",
        "will be delivered",
        "scheduled successfully",
    ] {
        assert!(
            !lower.contains(forbidden),
            "no fabricated saved identity, active message link or delivery promise: {text}"
        );
    }
    no_persistent_effects();
    assert_eq!(
        spies::snapshot().clock_reads,
        1,
        "one frozen-now calculation"
    );
}

#[tokio::test]
async fn relative_preview_matches_create_body_and_due_under_one_frozen_clock() {
    spies::reset();
    let text = answer(proposal("team practice", Some(10), None, Some(true))).await;
    preview(&text, "2027-03-28T00:40:00.125Z", "<t:1806194400:F>");
    assert!(text.contains("team practice"));
    assert!(["one-off", "one-shot", "once"]
        .iter()
        .any(|word| text.to_lowercase().contains(word)));
    spies::rewind_clock();
    let created = answer(proposal("team practice", Some(10), None, None)).await;
    let trace = spies::snapshot();
    assert_eq!(trace.writes.len(), 1);
    assert_eq!(trace.writes[0].body, "team practice");
    assert_eq!(trace.writes[0].next_run_at, "2027-03-28T00:40:00.125Z");
    assert_eq!(trace.writes[0].interval_seconds, None);
    assert!(created.contains(&trace.writes[0].id));
}

#[tokio::test]
async fn recurring_preview_uses_interval_for_first_run_across_the_dst_boundary() {
    spies::reset();
    let text = answer(proposal("weekly games", None, Some(120), Some(true))).await;
    preview(&text, "2027-03-28T02:30:00.125Z", "<t:1806201000:F>");
    assert!(text.contains("weekly games"));
    let lower = text.to_lowercase();
    assert!(lower.contains("recurr") || lower.contains("every"));
    assert!(
        ["120", "2 hour", "2h"]
            .iter()
            .any(|word| lower.contains(word)),
        "show the actual interval: {text}"
    );
    spies::rewind_clock();
    answer(proposal("weekly games", None, Some(120), Some(false))).await;
    let trace = spies::snapshot();
    assert_eq!(trace.writes.len(), 1);
    assert_eq!(trace.writes[0].next_run_at, "2027-03-28T02:30:00.125Z");
    assert_eq!(trace.writes[0].interval_seconds, Some(7200));
}

#[tokio::test]
async fn recurring_preview_respects_a_separate_first_delay() {
    spies::reset();
    let text = answer(proposal("warmup", Some(10), Some(120), Some(true))).await;
    preview(&text, "2027-03-28T00:40:00.125Z", "<t:1806194400:F>");
    assert!(text.contains("120") || text.contains("2 hour") || text.contains("2h"));
}

#[tokio::test]
async fn equivalent_explicit_offsets_preview_the_same_utc_instant_without_storage() {
    for at in [
        "2027-03-28T02:30:00.125Z",
        "2027-03-28T08:00:00.125+05:30",
        "2027-03-27T19:30:00.125-07:00",
    ] {
        spies::reset();
        let mut options = proposal("offset meeting", None, None, Some(true));
        options.push(option("at", CommandOptionValue::String(at.into())));
        let text = answer(options).await;
        preview(&text, "2027-03-28T02:30:00.125Z", "<t:1806201000:F>");
    }
}

#[tokio::test]
async fn invalid_preview_uses_create_validation_but_never_mints_or_audits() {
    let cases = [
        ("", Some(10), None, "Message body"),
        ("\u{200b}", Some(10), None, "Message body"),
        ("valid", None, None, "either"),
        ("valid", Some(0), None, "in-minutes"),
        ("valid", Some(525_601), None, "in-minutes"),
        ("valid", None, Some(59), "every-minutes"),
        ("valid", None, Some(525_601), "every-minutes"),
    ];
    for (body, minutes, every, correction) in cases {
        spies::reset();
        let text = answer(proposal(body, minutes, every, Some(true))).await;
        assert!(
            text.contains(correction),
            "specific shared validation: {text}"
        );
        no_persistent_effects();
    }
    for body in [
        "x".repeat(2001),
        "😀".repeat(1001),
        format!("{}@everyone", "x".repeat(1991)),
    ] {
        spies::reset();
        let text = answer(proposal(&body, Some(10), None, Some(true))).await;
        assert!(
            text.contains("Message body"),
            "UTF-16 and post-neutralization bounds: {text}"
        );
        no_persistent_effects();
    }
}

#[tokio::test]
async fn invalid_absolute_preview_retains_future_offset_and_conflict_rules() {
    let cases = [
        ("2027-03-28T00:30:00.125Z", None, None, "future"),
        ("2027-03-28T00:30:00.125999Z", None, None, "future"),
        ("2027-03-28T00:29:59Z", None, None, "future"),
        ("2028-03-28T00:30:00Z", None, None, "horizon"),
        ("2027-03-28T02:30:00", None, None, "RFC3339"),
        ("@everyone not a timestamp", None, None, "RFC3339"),
        ("2027-03-28T02:30:00Z", Some(10), None, "either"),
        ("2027-03-28T02:30:00Z", None, Some(120), "one-off"),
    ];
    for (at, minutes, every, correction) in cases {
        spies::reset();
        let mut options = proposal("safe body", minutes, every, Some(true));
        options.push(option("at", CommandOptionValue::String(at.into())));
        let text = answer(options).await;
        assert!(
            text.contains(correction),
            "specific absolute correction: {text}"
        );
        assert!(!text.contains("@everyone"));
        no_persistent_effects();
    }
}

#[tokio::test]
async fn preview_decoding_rejects_duplicate_and_wrong_typed_booleans_before_any_effect() {
    let malformed = [
        vec![CommandOptionValue::String("true @everyone".into())],
        vec![CommandOptionValue::Integer(1)],
        vec![
            CommandOptionValue::Boolean(true),
            CommandOptionValue::Boolean(false),
        ],
        vec![
            CommandOptionValue::Boolean(false),
            CommandOptionValue::Boolean(true),
        ],
        vec![
            CommandOptionValue::Boolean(false),
            CommandOptionValue::Boolean(false),
        ],
    ];
    for values in malformed {
        spies::reset();
        let duplicate = values.len() > 1;
        let mut options = proposal("hello", Some(10), None, None);
        options.extend(values.into_iter().map(|value| option("preview", value)));
        let text = answer(options).await;
        let lower = text.to_lowercase();
        assert!(
            lower.contains("preview")
                && lower.contains(if duplicate { "duplicate" } else { "invalid" }),
            "private boolean correction, not create: {text}"
        );
        assert!(
            lower.contains("true") || lower.contains("boolean"),
            "name corrected boolean input: {text}"
        );
        no_persistent_effects();
    }
}

#[tokio::test]
async fn hostile_preview_is_rendered_as_data_and_never_executed() {
    spies::reset();
    let body = "@everyone @here <@123456789012345678> <@&223456789012345678> discord.gg/example /schedule-remove id:existing {{delete}}";
    let text = answer(proposal(body, Some(10), None, Some(true))).await;
    preview(&text, "2027-03-28T00:40:00.125Z", "<t:1806194400:F>");
    assert!(!text.contains("@everyone") && !text.contains("@here"));
    assert!(!text.to_lowercase().contains("discord.gg/"));
    assert!(
        text.contains("/schedule-remove") && text.contains("{{delete}}"),
        "render text without executing or interpolating it: {text}"
    );
}

#[tokio::test]
async fn maximum_body_keeps_timing_destination_and_explicit_excerpt_disclosure() {
    spies::reset();
    let body = format!("{}END-OF-BODY", "x".repeat(1989));
    let text = answer(proposal(&body, Some(10), None, Some(true))).await;
    preview(&text, "2027-03-28T00:40:00.125Z", "<t:1806194400:F>");
    assert!(
        text.contains("xxxxx"),
        "show proposed body, not only metadata"
    );
    assert!(
        text.contains("END-OF-BODY")
            || ["truncat", "excerpt", "shorten"]
                .iter()
                .any(|word| text.to_lowercase().contains(word)),
        "never silently hide a body tail: {text}"
    );
}

#[tokio::test]
async fn repeated_previews_do_not_consume_the_last_create_slot_or_use_existing_ids() {
    spies::reset();
    for _ in 0..8 {
        spies::rewind_clock();
        let text = answer(proposal("practice", Some(10), None, Some(true))).await;
        preview(&text, "2027-03-28T00:40:00.125Z", "<t:1806194400:F>");
    }
    spies::set_full();
    spies::rewind_clock();
    let text = answer(proposal("practice", Some(10), None, Some(true))).await;
    preview(&text, "2027-03-28T00:40:00.125Z", "<t:1806194400:F>");
    // A real create still reaches quota admission and refuses, proving the
    // quota spy is live rather than an inert mock that makes preview vacuous.
    spies::rewind_clock();
    let refused = answer(proposal("practice", Some(10), None, Some(false))).await;
    assert!(refused.contains("limit of 25"));
    assert!(spies::snapshot().writes.is_empty());
    assert_eq!(spies::snapshot().audits.len(), 1);
    assert_eq!(spies::snapshot().audits[0].reason.as_deref(), Some("quota"));
}

#[tokio::test]
async fn control_omitted_and_false_preview_keep_create_and_rejection_audits() {
    for preview_flag in [None, Some(false)] {
        spies::reset();
        let text = answer(proposal(
            "ordinary create",
            Some(10),
            Some(120),
            preview_flag,
        ))
        .await;
        let trace = spies::snapshot();
        assert_eq!(trace.writes.len(), 1);
        assert_eq!(trace.writes[0].guild_id, "2222");
        assert_eq!(trace.writes[0].channel_id, "3333");
        assert_eq!(trace.writes[0].body, "ordinary create");
        assert_eq!(trace.writes[0].next_run_at, "2027-03-28T00:40:00.125Z");
        assert_eq!(trace.writes[0].interval_seconds, Some(7200));
        assert!(trace.writes[0].enabled);
        assert!(text.contains(&trace.writes[0].id));
        assert_eq!(trace.audits.len(), 1);
        assert_eq!(trace.audits[0].action, "scheduled.create");
        assert_eq!(trace.audits[0].outcome, "ok");
        assert_eq!(trace.audits[0].actor_id.as_deref(), Some("88"));
        spies::reset();
        let text = answer(proposal("", Some(10), None, preview_flag)).await;
        assert!(text.contains("Message body"));
        let trace = spies::snapshot();
        assert_eq!(trace.ids, 2, "existing create attempt ID plus audit ID");
        assert!(trace.writes.is_empty());
        assert_eq!(trace.audits.len(), 1);
        assert_eq!(trace.audits[0].outcome, "rejected");
    }
}

#[tokio::test]
async fn concurrent_previews_reach_private_completion_with_no_shared_admission_effects() {
    spies::reset();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let response = ScriptedResponse::json(200, json!({"id":"99"}));
    let (mock, gate) =
        MockRest::start_gated(vec![response.clone(), response.clone()], response).await;
    let executor =
        ActionExecutor::with_proxy("fixture-token".into(), Some(mock.origin())).expect("executor");
    let pool = pool().await;
    let mut options = proposal("concurrent proposal", None, None, Some(true));
    options.push(option(
        "at",
        CommandOptionValue::String("2027-03-28T02:30:00.125Z".into()),
    ));
    let first = invocation(options.clone());
    let second = invocation(options);
    let invocations = async {
        tokio::join!(
            subject::schedule_create(&pool, &executor, "2222", &first),
            subject::schedule_create(&pool, &executor, "2222", &second),
        );
    };
    let witness = async {
        gate.wait_for_request().await;
        // Receipt of request 2 proves both invocations have reached their
        // completion boundary before the gated second response is released.
        let trace = spies::snapshot();
        let requests = mock.requests();
        gate.release();
        (trace, requests)
    };
    let (_, (at_boundary, requests)) =
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(invocations, witness)
        })
        .await
        .expect("both completion boundaries are reachable");
    mock.shutdown().await;
    assert_eq!(
        requests.len(),
        2,
        "two independently received private completion edits"
    );
    assert_eq!(
        at_boundary.ids, 0,
        "neither preview mints an ID before responding: {at_boundary:?}"
    );
    assert!(
        at_boundary.calls.is_empty(),
        "neither preview enters quota, queue or store admission: {at_boundary:?}"
    );
    no_persistent_effects();
}

#[tokio::test]
async fn control_existing_strict_decoders_and_list_remove_remain_live() {
    spies::reset();
    let input = invocation(proposal("control", Some(10), None, None));
    assert_eq!(
        subject::schedule_options(&input),
        (Some("control".into()), Some(10), None)
    );
    assert!(subject::schedule_options_strict(&input).is_ok());
    let bad = invocation(vec![option("body", CommandOptionValue::Integer(1))]);
    assert!(subject::schedule_options_strict(&bad).is_err());
    let remove = invocation(vec![option(
        "id",
        CommandOptionValue::String("saved-id".into()),
    )]);
    assert_eq!(
        subject::schedule_remove_option(&remove).as_deref(),
        Some("saved-id")
    );
    assert!(subject::schedule_remove_option_strict(&remove).is_ok());
    assert_eq!(subject::schedule_list_page_option_strict(&input), Ok(None));
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mock = MockRest::start(Vec::new(), ScriptedResponse::json(200, json!({"id":"99"}))).await;
    let executor =
        ActionExecutor::with_proxy("fixture-token".into(), Some(mock.origin())).expect("executor");
    let pool = pool().await;
    subject::schedule_remove(&pool, &executor, "2222", &remove).await;
    subject::schedule_list(&pool, &executor, "2222", &input).await;
    mock.shutdown().await;
    assert_eq!(spies::snapshot().calls, ["resolve", "list"]);
}
