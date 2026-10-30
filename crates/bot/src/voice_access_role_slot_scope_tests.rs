#![cfg(test)]
//! Builder pin: the `/access role` subcommand consumes only its published
//! `role` slot.
//!
//! The registry publishes `role2`/`role3` only under `restrict`
//! (`crates/core/src/voice_rooms.rs`), so under `role` those names are
//! inapplicable and ignored like any unknown option: a secondary slot
//! alone is an omitted `role` (a genuine clear), and a wrong-typed or
//! duplicated secondary slot cannot refuse a valid `role`. These dispatch
//! controls go red if `parse_access_action` again decodes the `role`
//! subcommand from all three slots: the clear cases would save the
//! secondary id instead of clearing, and the malformed-secondary cases
//! would refuse instead of saving.

use super::*;

type LiveInbox = tokio::sync::mpsc::UnboundedReceiver<ActorCommand>;

fn seeded_controls() -> AccessControls {
    let mut controls = AccessControls {
        required_role: Some(9),
        ..AccessControls::default()
    };
    controls.command_roles.insert("kick".to_owned(), vec![7]);
    controls
}

/// Run one `/access role` dispatch on a freshly seeded fixture and return
/// the reply text, the saved controls, and the actor's first live update
/// (`None` when the dispatch pushed nothing).
async fn run_role_case(
    args: Vec<CommandDataOption>,
) -> (String, AccessControls, Option<ActorCommand>) {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(seeded_controls()));
    let trace = Trace::default();
    let runtime = shared_runtime(trace, shared.clone(), None);
    let mut rx: LiveInbox = {
        let live = LiveGuild::new(GUILD);
        assert!(live.publish(command_snapshot()), "snapshot publishes");
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        runtime.actors.lock().expect("voice runtime lock").insert(
            GUILD,
            GuildActor {
                live,
                tx,
                backpressure: ActorInboxBackpressure::default(),
            },
        );
        rx
    };
    let interaction = access_interaction(sub_option("role", args), true);
    let (owned, response) = handle_capture(&runtime, &interaction).await;
    assert!(owned, "access must stay owned");
    let text = response_text(response.as_ref().expect("access reply"));
    let saved = shared.lock().unwrap().clone();
    let first = rx.try_recv().ok();
    assert!(
        rx.try_recv().is_err(),
        "a role save pushes at most one live update"
    );
    (text, saved, first)
}

fn assert_saved_with_one_update(
    text: &str,
    saved: &AccessControls,
    update: Option<ActorCommand>,
    expected_required_role: Option<u64>,
    what: &str,
) {
    assert!(text.contains("Saved"), "{what}: valid shape saves: {text}");
    let mut expected = seeded_controls();
    expected.required_role = expected_required_role;
    assert_eq!(saved, &expected, "{what}: exact saved controls");
    match update {
        Some(ActorCommand::AccessChanged(controls)) => {
            assert_eq!(controls, expected, "{what}: live update carries the save");
        }
        _ => panic!("{what}: expected exactly one AccessChanged update"),
    }
}

#[tokio::test]
async fn access_role_slot_scope_secondary_only_clears() {
    // Only the published `role` slot can set the gate: a lone secondary
    // slot is an omitted `role`, so the seeded gate clears exactly as a
    // bare `/access role` does.
    for slot in ["role2", "role3"] {
        let what = format!("{slot}-only input");
        let (text, saved, update) = run_role_case(vec![role_option(slot, 11)]).await;
        assert_saved_with_one_update(&text, &saved, update, None, &what);
    }
}

#[tokio::test]
async fn access_role_slot_scope_malformed_secondary_is_ignored() {
    // Inapplicable secondary slots never refuse a valid `role`, whatever
    // shape they carry: wrong-typed values and duplicated names alike.
    let cases: Vec<(&str, Vec<CommandDataOption>)> = vec![
        (
            "wrong-typed role2",
            vec![
                role_option("role", 11),
                CommandDataOption {
                    name: "role2".to_owned(),
                    value: CommandOptionValue::String("@here seize-gate".to_owned()),
                },
            ],
        ),
        (
            "duplicated role2",
            vec![
                role_option("role", 11),
                role_option("role2", 7),
                role_option("role2", 8),
            ],
        ),
        (
            "duplicated wrong-typed role3",
            vec![
                role_option("role", 11),
                CommandDataOption {
                    name: "role3".to_owned(),
                    value: CommandOptionValue::Integer(12),
                },
                CommandDataOption {
                    name: "role3".to_owned(),
                    value: CommandOptionValue::Boolean(true),
                },
            ],
        ),
    ];
    for (what, args) in cases {
        let (text, saved, update) = run_role_case(args).await;
        assert_saved_with_one_update(&text, &saved, update, Some(11), what);
    }
}
