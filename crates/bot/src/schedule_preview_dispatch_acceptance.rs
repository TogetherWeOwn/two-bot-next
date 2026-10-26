//! Protected real-dispatch companion: no test-only router or handler clone.
//! A closed pool forbids SQL. Loopback REST records every private/public effect.
#![cfg(test)]

use super::*;
use serde_json::{json, Value};

fn preview_input() -> Interaction {
    slash(
        "schedule",
        Some(CHANNEL),
        vec![
            option(
                "body",
                CommandOptionValue::String("proposed practice".into()),
            ),
            option("in-minutes", CommandOptionValue::Integer(10)),
            option("preview", CommandOptionValue::Boolean(true)),
        ],
    )
}

async fn closed_preview_runtime(
    router_gates: RouterGates,
    activated: bool,
    origin: String,
) -> Arc<CommandRuntime> {
    crate::gateway::ensure_crypto_provider();
    let pool = PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new());
    pool.close().await;
    CommandRuntime::new(
        pool,
        executor_at(origin),
        router_with_commands(router_gates),
        GUILD,
        activated,
    )
}

#[tokio::test]
async fn valid_preview_dispatch_completes_the_private_defer_without_a_working_store() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = closed_preview_runtime(gates(true, false), true, origin).await;
    runtime.on_interaction(&preview_input()).await;
    let requests = mock.requests();
    mock.shutdown().await;
    assert_eq!(
        requests.len(),
        2,
        "defer and completion only, no public send, followup or discovery"
    );
    assert!(requests[0].path.ends_with("/callback"));
    let ack: Value = serde_json::from_slice(&requests[0].body).expect("ack JSON");
    assert_eq!(ack["type"], 5);
    assert_eq!(
        ack["data"]["flags"], 64,
        "preview is private from initial acknowledgement"
    );
    assert_eq!(requests[1].method, "PATCH");
    assert!(requests[1].path.ends_with("/messages/@original"));
    let reply: Value = serde_json::from_slice(&requests[1].body).expect("completion JSON");
    assert_eq!(reply["allowed_mentions"]["parse"], json!([]));
    assert!(
        reply["content"]
            .as_str()
            .expect("completion")
            .contains("Preview — not scheduled"),
        "a preview must not fall through to the closed store: {reply}"
    );
}

#[tokio::test]
async fn control_preview_keeps_staff_feature_activation_and_guild_fences() {
    for denied in ["staff", "feature", "dm", "foreign-guild"] {
        let (mock, origin) = MockRest::start(Vec::new()).await;
        let runtime = closed_preview_runtime(
            gates(denied != "feature", false),
            denied != "feature",
            origin,
        )
        .await;
        let mut input = preview_input();
        match denied {
            "staff" => {
                input.member.as_mut().expect("member").permissions = Some(Permissions::empty())
            }
            "dm" => set_guild(&mut input, None),
            "foreign-guild" => set_guild(&mut input, Some(Id::new(9999))),
            _ => {}
        }
        runtime.on_interaction(&input).await;
        let requests = mock.requests();
        mock.shutdown().await;
        if denied == "foreign-guild" || denied == "dm" {
            assert!(requests.is_empty(), "guild fence stays silent for {denied}");
            continue;
        }
        assert_eq!(
            requests.len(),
            1,
            "{denied}: refusal before any deferred handler or public effect"
        );
        let reply: Value = serde_json::from_slice(&requests[0].body).expect("refusal");
        assert_eq!(reply["type"], 4);
        assert_eq!(reply["data"]["flags"], 64);
        let text = reply["data"]["content"].as_str().expect("refusal content");
        assert!(!text.contains("Preview — not scheduled"));
        if denied == "staff" {
            assert_eq!(text, MANAGE_SERVER_REQUIRED);
        }
        if denied == "feature" {
            assert_eq!(text, AUTOMATIONS_DISABLED_REPLY);
        }
    }
}

#[tokio::test]
async fn preview_is_discoverable_as_optional_boolean_in_the_actual_publication_payload() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = closed_preview_runtime(gates(true, false), true, origin).await;
    runtime
        .publish_registry_checked(Some(1111))
        .await
        .expect("fixture publication");
    let requests = mock.requests();
    mock.shutdown().await;
    let put = requests
        .iter()
        .find(|request| request.method == "PUT" && request.path.ends_with("/commands"))
        .expect("actual command publication");
    let commands: Value = serde_json::from_slice(&put.body).expect("wire registry");
    let schedule = commands
        .as_array()
        .expect("commands")
        .iter()
        .find(|command| command["name"] == "schedule")
        .expect("schedule published");
    let options = schedule["options"].as_array().expect("options");
    let previews: Vec<_> = options
        .iter()
        .filter(|option| option["name"] == "preview")
        .collect();
    assert_eq!(
        previews.len(),
        1,
        "one explicit preview picker, not a global command-count pin"
    );
    assert_eq!(previews[0]["type"], 5, "Discord boolean option");
    assert_ne!(
        previews[0]["required"], true,
        "ordinary creation must remain possible"
    );
    let description = previews[0]["description"]
        .as_str()
        .expect("preview explanation")
        .to_lowercase();
    assert!(description.contains("preview"));
    assert!(
        description.contains("sav")
            || description.contains("schedul")
            || description.contains("creat"),
        "explain non-persistence in the picker"
    );
}

#[tokio::test]
async fn detailed_schedule_help_explains_the_preview_without_running_it() {
    let (mock, origin) = MockRest::start(Vec::new()).await;
    let runtime = closed_preview_runtime(gates(true, false), true, origin).await;
    runtime
        .publish_registry_checked(Some(1111))
        .await
        .expect("fixture publication");
    let before = mock.requests().len();
    runtime
        .on_interaction(&slash(
            "help",
            Some(CHANNEL),
            vec![option(
                "command",
                CommandOptionValue::String("schedule".into()),
            )],
        ))
        .await;
    let requests = mock.requests();
    mock.shutdown().await;
    assert_eq!(
        requests.len(),
        before + 1,
        "help neither creates nor dispatches a preview"
    );
    let reply: Value = serde_json::from_slice(&requests[before].body).expect("help payload");
    assert_eq!(reply["type"], 4);
    assert_eq!(reply["data"]["flags"], 64);
    let text = reply["data"].to_string().to_lowercase();
    assert!(
        text.contains("preview"),
        "runtime help must advertise the optional preview: {reply}"
    );
}
