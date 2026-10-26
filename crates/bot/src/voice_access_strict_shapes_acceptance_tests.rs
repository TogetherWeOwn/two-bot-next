//! Protected acceptance: malformed `/access` shapes refuse before any
//! read, write, audit, Discord or live-actor effect.
//!
//! Consumer: server admins gating room creation and room commands with
//! `/access role`, `/access restrict`, `/access creation`, `/access show`
//! and `/access unrestrict`. User value: a mistyped or forged option can
//! never silently clear the required-role gate or save a truncated
//! restriction; it is refused with the store untouched.
//!
//! Baseline RED: the access decoder (`parse_access_action`) takes only the
//! first top-level option, drops wrong-typed `role`/`role2`/`role3` slots
//! to `None`, first-wins duplicate option names, and silently dedups nothing
//! else. A present-but-wrong-type `role` therefore decodes to
//! `RequiredRole(None)` and the save clears the gate with a `Saved`
//! confirmation. Every `Invalid` shape (wrong-typed `command`/`enabled`,
//! unknown subcommand, bare `/access`) reads the store before refusing,
//! and the refusal copy never says nothing changed. These tests pin the
//! strict contract through real dispatch (`voice_interaction` +
//! `handle_capture`), the real store doubles, a captured live-actor inbox,
//! and a poisoned store (healthy reply must equal poisoned reply, so the
//! refusal precedes the mutation-path store read).
//!
//! Builder contract (this file is preserved verbatim; see the manifest on
//! the acceptance card):
//!
//! - Top level: exactly one option, a `SubCommand` with a known name
//!   (`show`, `creation`, `role`, `restrict`, `unrestrict`). Bare
//!   `/access`, a non-subcommand first option, an unknown subcommand, and
//!   any trailing extra option refuse.
//! - Nested args, per subcommand: `role`/`role2`/`role3` must be `Role`,
//!   `command` must be `String`, `enabled` must be `Boolean`, `page` keeps
//!   the existing strict `decode_page`. A wrong-typed known option refuses.
//! - A duplicated known option *name* refuses in any order and adjacency.
//!   Duplicate role *values* across the distinct `role`/`role2`/`role3`
//!   slots keep the existing dedup-then-save behavior (dedup is pinned).
//! - Unknown or inapplicable option names stay ignored (status quo).
//! - Every `Invalid` shape refuses *before* the mutation-path store read
//!   (the command-gate authz read still happens; admin proceeds past a
//!   failed gate read as today). The refusal is short (<=500 UTF-16
//!   units), names `/access`, says nothing changed, never starts with
//!   `Saved`, never echoes the malformed input, and is mention-safe.
//! - Preserved semantics: an omitted `role` still clears the gate, valid
//!   sets save byte-identically and push exactly one `AccessChanged`
//!   carrying the saved controls, member denial still precedes validation,
//!   and the registry gains no option and no new required flag.
//!
//! No production/staging credentials or effects; in-memory `Store`/`Http`
//! doubles plus a captured actor inbox only, no disposable database needed.
//! Failing on baseline is the honest RED: gates clear and truncated
//! restrictions save with `Saved` confirmations.

#![cfg(test)]

use super::*;

const REFUSAL_BOUND: usize = 500;
const FOREIGN: u64 = 101;

fn utf16_len(text: &str) -> usize {
    two_bot_core::message_safety::text_len(text)
}

fn value_option(name: &str, value: CommandOptionValue) -> CommandDataOption {
    CommandDataOption {
        name: name.to_owned(),
        value,
    }
}

fn string_option(name: &str, value: &str) -> CommandDataOption {
    value_option(name, CommandOptionValue::String(value.to_owned()))
}

fn int_option(name: &str, value: i64) -> CommandDataOption {
    value_option(name, CommandOptionValue::Integer(value))
}

fn bool_option(name: &str, value: bool) -> CommandDataOption {
    value_option(name, CommandOptionValue::Boolean(value))
}

fn user_option(name: &str, id: u64) -> CommandDataOption {
    value_option(name, CommandOptionValue::User(Id::new(id)))
}

fn channel_option(name: &str, id: u64) -> CommandDataOption {
    value_option(name, CommandOptionValue::Channel(Id::new(id)))
}

fn access_top(options: Vec<CommandDataOption>) -> Interaction {
    voice_interaction(
        Some(command_data("access", options)),
        Some(Permissions::MANAGE_CHANNELS),
        true,
    )
}

struct Fixture {
    runtime: VoiceRuntime<Store, Http>,
    shared: std::sync::Arc<std::sync::Mutex<AccessControls>>,
    trace: Trace,
}

type LiveInbox = tokio::sync::mpsc::UnboundedReceiver<ActorCommand>;

/// Pre-insert a ready live actor whose inbox only this test drains, so a
/// live update is observable deterministically: no worker task is spawned
/// for a pre-existing entry, and `handle_capture` skips its snapshot
/// publish. A valid save must deliver exactly one `AccessChanged`; a
/// refusal must deliver nothing.
fn capture_live_updates(runtime: &VoiceRuntime<Store, Http>) -> LiveInbox {
    let live = LiveGuild::new(GUILD);
    assert!(
        live.publish(command_snapshot()),
        "command snapshot publishes"
    );
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
}

fn healthy_fixture(controls: AccessControls) -> (Fixture, LiveInbox) {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(controls));
    let trace = Trace::default();
    let runtime = shared_runtime(trace.clone(), shared.clone(), None);
    let rx = capture_live_updates(&runtime);
    (
        Fixture {
            runtime,
            shared,
            trace,
        },
        rx,
    )
}

/// A store whose reads and saves always fail. Admins proceed past the
/// failed command-gate read as today, so a refusal that precedes the
/// mutation-path read answers byte-identically to the healthy fixture;
/// any read or save attempt surfaces as the store-failure copy instead.
fn poisoned_fixture(controls: AccessControls) -> (Fixture, LiveInbox) {
    let shared = std::sync::Arc::new(std::sync::Mutex::new(controls));
    let trace = Trace::default();
    let runtime = VoiceRuntime::new(
        {
            let shared = shared.clone();
            let trace = trace.clone();
            move || {
                let mut store = Store::new(trace.clone());
                store.access = shared.clone();
                store.access_error = Some(StoreError::Unavailable);
                store.save_access_error = Some(StoreError::Unavailable);
                (store, Http::new(trace.clone()))
            }
        },
        Duration::from_millis(10),
        true,
    );
    let rx = capture_live_updates(&runtime);
    (
        Fixture {
            runtime,
            shared,
            trace,
        },
        rx,
    )
}

async fn access_text(fixture: &Fixture, interaction: &Interaction) -> String {
    let (owned, response) = handle_capture(&fixture.runtime, interaction).await;
    assert!(owned, "access must stay owned");
    response_text(response.as_ref().expect("access reply"))
}

fn assert_no_live_update(rx: &mut LiveInbox, what: &str) {
    match rx.try_recv() {
        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
            panic!("{what}: actor channel must stay open")
        }
        Ok(_) => panic!("{what}: refusal must not push a live update"),
    }
}

fn assert_one_live_update(rx: &mut LiveInbox, expected: &AccessControls, what: &str) {
    let update = rx
        .try_recv()
        .unwrap_or_else(|_| panic!("{what}: valid save must push one live update"));
    match update {
        ActorCommand::AccessChanged(controls) => assert_eq!(
            controls, *expected,
            "{what}: live update carries the saved controls"
        ),
        _ => panic!("{what}: expected AccessChanged"),
    }
    assert_no_live_update(rx, what);
}

fn assert_no_effects(fixture: &Fixture, before: &AccessControls, rx: &mut LiveInbox, what: &str) {
    assert_eq!(
        *fixture.shared.lock().unwrap(),
        *before,
        "{what}: store controls untouched"
    );
    let trace = fixture.trace.lock().unwrap();
    assert!(
        trace.is_empty(),
        "{what}: no Discord/store writes: {trace:?}"
    );
    drop(trace);
    assert_no_live_update(rx, what);
}

fn assert_strict_refusal(text: &str, what: &str) {
    assert!(
        utf16_len(text) <= REFUSAL_BOUND,
        "{what}: refusal must be short (≤{REFUSAL_BOUND} units, got {}): {text}",
        utf16_len(text)
    );
    assert!(
        !text.contains("Saved"),
        "{what}: refusal never acks a save: {text}"
    );
    assert!(
        text.contains("/access"),
        "{what}: refusal must name the remedy: {text}"
    );
    assert!(
        text.to_ascii_lowercase().contains("nothing was changed"),
        "{what}: refusal must say nothing changed: {text}"
    );
    assert!(
        !text.contains("<@&") && !text.contains("<#"),
        "{what}: refusal must not leak mentions: {text}"
    );
    assert!(
        !text.contains("@everyone") && !text.contains("@here"),
        "{what}: refusal must be mention-safe: {text}"
    );
}

fn assert_no_echo(text: &str, needles: &[&str], what: &str) {
    for needle in needles {
        assert!(
            !text.contains(needle),
            "{what}: refusal must not echo malformed input ({needle:?}): {text}"
        );
    }
}

fn seeded_controls() -> AccessControls {
    let mut controls = AccessControls {
        required_role: Some(9),
        ..AccessControls::default()
    };
    controls.command_roles.insert("kick".to_owned(), vec![7]);
    controls
        .command_roles
        .insert("limit".to_owned(), vec![5, 6]);
    controls
}

/// One malformed interaction, refused identically on a healthy and a
/// poisoned store with zero effects on either: the identity proves the
/// refusal precedes the mutation-path store read.
async fn assert_strictly_refused(what: &str, interaction: &Interaction, needles: &[&str]) {
    let seeded = seeded_controls();
    let (fixture, mut rx) = healthy_fixture(seeded.clone());
    let text = access_text(&fixture, interaction).await;
    assert_strict_refusal(&text, what);
    assert_no_echo(&text, needles, what);
    assert_no_effects(&fixture, &seeded, &mut rx, what);
    let (poisoned, mut poisoned_rx) = poisoned_fixture(seeded.clone());
    let poisoned_text = access_text(&poisoned, interaction).await;
    assert_eq!(
        poisoned_text, text,
        "{what}: identical under store failure, so no store read"
    );
    assert_no_effects(&poisoned, &seeded, &mut poisoned_rx, what);
}

/// Wrong-typed `role` payloads with the input-derived marker the refusal
/// must never echo (`None` where the value has no stable rendering).
fn wrong_role_values() -> Vec<(&'static str, CommandOptionValue, Option<&'static str>)> {
    vec![
        (
            "string",
            CommandOptionValue::String("seize-gate-9".to_owned()),
            Some("seize-gate-9"),
        ),
        (
            "mention-string",
            CommandOptionValue::String("@everyone seize-gate".to_owned()),
            Some("seize-gate"),
        ),
        (
            "empty-string",
            CommandOptionValue::String(String::new()),
            None,
        ),
        (
            "integer",
            CommandOptionValue::Integer(987_654_321),
            Some("987654321"),
        ),
        (
            "negative",
            CommandOptionValue::Integer(-987_654_321),
            Some("987654321"),
        ),
        ("boolean-true", CommandOptionValue::Boolean(true), None),
        ("boolean-false", CommandOptionValue::Boolean(false), None),
        ("number", CommandOptionValue::Number(9.0), None),
        (
            "user",
            CommandOptionValue::User(Id::new(987_654_322)),
            Some("987654322"),
        ),
        (
            "channel",
            CommandOptionValue::Channel(Id::new(987_654_323)),
            Some("987654323"),
        ),
        (
            "mentionable",
            CommandOptionValue::Mentionable(Id::new(987_654_324)),
            Some("987654324"),
        ),
        (
            "attachment",
            CommandOptionValue::Attachment(Id::new(987_654_325)),
            Some("987654325"),
        ),
        (
            "subcommand",
            CommandOptionValue::SubCommand(Vec::new()),
            None,
        ),
        (
            "subcommand-group",
            CommandOptionValue::SubCommandGroup(Vec::new()),
            None,
        ),
    ]
}

fn wrong_command_values() -> Vec<(&'static str, CommandOptionValue, Option<&'static str>)> {
    vec![
        (
            "integer",
            CommandOptionValue::Integer(87_654_321),
            Some("87654321"),
        ),
        ("boolean", CommandOptionValue::Boolean(true), None),
        ("number", CommandOptionValue::Number(1.5), None),
        (
            "role",
            CommandOptionValue::Role(Id::new(87_654_322)),
            Some("87654322"),
        ),
        (
            "user",
            CommandOptionValue::User(Id::new(87_654_323)),
            Some("87654323"),
        ),
        (
            "channel",
            CommandOptionValue::Channel(Id::new(87_654_324)),
            Some("87654324"),
        ),
        (
            "mentionable",
            CommandOptionValue::Mentionable(Id::new(87_654_325)),
            Some("87654325"),
        ),
        (
            "attachment",
            CommandOptionValue::Attachment(Id::new(87_654_326)),
            Some("87654326"),
        ),
        (
            "subcommand",
            CommandOptionValue::SubCommand(Vec::new()),
            None,
        ),
    ]
}

#[tokio::test]
async fn role_wrong_typed_role_refuses_without_clearing_or_saving() {
    for (label, value, needle) in wrong_role_values() {
        let what = format!("role wrong-typed as {label}");
        let interaction =
            access_interaction(sub_option("role", vec![value_option("role", value)]), true);
        // The seeded gate (Some(9)) must survive: baseline clears it to
        // None with a `Saved` confirmation.
        let needles: Vec<&str> = needle.into_iter().collect();
        assert_strictly_refused(&what, &interaction, &needles).await;
    }
}

#[tokio::test]
async fn restrict_and_unrestrict_wrong_typed_command_refuse_before_any_store_effect() {
    for sub in ["restrict", "unrestrict"] {
        for (label, value, needle) in wrong_command_values() {
            let what = format!("{sub} wrong-typed command as {label}");
            let mut args = vec![value_option("command", value)];
            if sub == "restrict" {
                args.push(role_option("role", 8));
            }
            let interaction = access_interaction(sub_option(sub, args), true);
            let needles: Vec<&str> = needle.into_iter().collect();
            assert_strictly_refused(&what, &interaction, &needles).await;
        }
        // An omitted command is the same malformed shape: it refuses
        // before the read, with the same honest copy.
        let what = format!("{sub} omitted command");
        let interaction = access_interaction(sub_option(sub, Vec::new()), true);
        assert_strictly_refused(&what, &interaction, &[]).await;
    }
}

#[tokio::test]
async fn secondary_role_slots_wrong_typed_refuse_without_saving() {
    for slot in ["role", "role2", "role3"] {
        for (label, value) in [
            (
                "string",
                CommandOptionValue::String("@here seize-gate".to_owned()),
            ),
            ("integer", CommandOptionValue::Integer(8)),
            ("boolean", CommandOptionValue::Boolean(true)),
            ("user", CommandOptionValue::User(Id::new(MEMBER))),
            ("channel", CommandOptionValue::Channel(Id::new(CATEGORY))),
        ] {
            let what = format!("restrict {slot} wrong-typed as {label}");
            let mut args = vec![command_option("command", "kick")];
            // Well-typed neighbors stay: only the wrong-typed slot is bad,
            // and the whole shape must refuse instead of saving partial.
            if slot != "role" {
                args.push(role_option("role", 7));
            }
            if slot != "role2" {
                args.push(role_option("role2", 8));
            }
            args.push(value_option(slot, value));
            let interaction = access_interaction(sub_option("restrict", args), true);
            assert_strictly_refused(&what, &interaction, &["seize-gate"]).await;
        }
    }
}

#[tokio::test]
async fn duplicate_option_names_refuse_in_both_orders() {
    let cases: Vec<(String, CommandDataOption)> = vec![
        // Duplicate `role` names on the role subcommand, both value orders.
        (
            "role [7, 8]".to_owned(),
            sub_option("role", vec![role_option("role", 7), role_option("role", 8)]),
        ),
        (
            "role [8, 7]".to_owned(),
            sub_option("role", vec![role_option("role", 8), role_option("role", 7)]),
        ),
        // Mixed well-typed and wrong-typed duplicates, both orders.
        (
            "role [typed, wrong-typed]".to_owned(),
            sub_option(
                "role",
                vec![role_option("role", 7), string_option("role", "seize-gate")],
            ),
        ),
        (
            "role [wrong-typed, typed]".to_owned(),
            sub_option(
                "role",
                vec![string_option("role", "seize-gate"), role_option("role", 7)],
            ),
        ),
        // Duplicate `command` names, both value orders.
        (
            "restrict [kick, limit]".to_owned(),
            sub_option(
                "restrict",
                vec![
                    command_option("command", "kick"),
                    command_option("command", "limit"),
                ],
            ),
        ),
        (
            "restrict [limit, kick]".to_owned(),
            sub_option(
                "restrict",
                vec![
                    command_option("command", "limit"),
                    command_option("command", "kick"),
                ],
            ),
        ),
        (
            "unrestrict [kick, limit]".to_owned(),
            sub_option(
                "unrestrict",
                vec![
                    command_option("command", "kick"),
                    command_option("command", "limit"),
                ],
            ),
        ),
        (
            "unrestrict [limit, kick]".to_owned(),
            sub_option(
                "unrestrict",
                vec![
                    command_option("command", "limit"),
                    command_option("command", "kick"),
                ],
            ),
        ),
        // Non-adjacent duplicate `role` names on restrict.
        (
            "restrict role twice around command".to_owned(),
            sub_option(
                "restrict",
                vec![
                    role_option("role", 7),
                    command_option("command", "kick"),
                    role_option("role", 8),
                ],
            ),
        ),
        // Duplicate `enabled` names, both orders.
        (
            "creation [true, false]".to_owned(),
            sub_option(
                "creation",
                vec![bool_option("enabled", true), bool_option("enabled", false)],
            ),
        ),
        (
            "creation [false, true]".to_owned(),
            sub_option(
                "creation",
                vec![bool_option("enabled", false), bool_option("enabled", true)],
            ),
        ),
    ];
    for (label, sub) in &cases {
        let what = format!("duplicate names {label}");
        let interaction = access_interaction(sub.clone(), true);
        assert_strictly_refused(&what, &interaction, &["seize-gate"]).await;
    }
}

#[tokio::test]
async fn creation_wrong_typed_or_missing_enabled_refuses_before_any_store_effect() {
    for (label, value) in [
        (
            "string",
            CommandOptionValue::String("seize-gate".to_owned()),
        ),
        ("integer", CommandOptionValue::Integer(1)),
        ("role", CommandOptionValue::Role(Id::new(7))),
        ("user", CommandOptionValue::User(Id::new(MEMBER))),
    ] {
        let what = format!("creation wrong-typed enabled as {label}");
        let interaction = access_interaction(
            sub_option("creation", vec![value_option("enabled", value)]),
            true,
        );
        assert_strictly_refused(&what, &interaction, &["seize-gate"]).await;
    }
    let interaction = access_interaction(sub_option("creation", Vec::new()), true);
    assert_strictly_refused("creation omitted enabled", &interaction, &[]).await;
}

#[tokio::test]
async fn trailing_top_level_options_and_bare_access_refuse_before_read() {
    let role_valid = sub_option("role", vec![role_option("role", 11)]);
    let restrict_valid = sub_option(
        "restrict",
        vec![command_option("command", "kick"), role_option("role", 8)],
    );
    let show_valid = sub_option("show", Vec::new());
    let cases: Vec<(String, Vec<CommandDataOption>)> = vec![
        ("bare access".to_owned(), Vec::new()),
        (
            "unknown subcommand".to_owned(),
            vec![sub_option("bogus", Vec::new())],
        ),
        (
            "first option not a subcommand".to_owned(),
            vec![int_option("page", 1)],
        ),
        (
            "role then show".to_owned(),
            vec![role_valid.clone(), show_valid.clone()],
        ),
        (
            "show then role".to_owned(),
            vec![show_valid.clone(), role_valid.clone()],
        ),
        (
            "restrict then role".to_owned(),
            vec![restrict_valid.clone(), role_valid.clone()],
        ),
        (
            "role twice".to_owned(),
            vec![role_valid.clone(), role_valid.clone()],
        ),
        (
            "show twice".to_owned(),
            vec![show_valid.clone(), show_valid],
        ),
        (
            "role then trailing integer".to_owned(),
            vec![role_valid.clone(), int_option("page", 1)],
        ),
        (
            "role then trailing role value".to_owned(),
            vec![role_valid, role_option("role", 7)],
        ),
        (
            "restrict then trailing string".to_owned(),
            vec![restrict_valid, string_option("command", "limit")],
        ),
    ];
    for (label, options) in &cases {
        let interaction = access_top(options.clone());
        assert_strictly_refused(label, &interaction, &["bogus"]).await;
    }
}

#[tokio::test]
async fn unknown_and_inapplicable_options_are_ignored() {
    // Unknown names ride along with valid shapes and change nothing.
    let (fixture, mut rx) = healthy_fixture(seeded_controls());
    let text = access_text(
        &fixture,
        &access_interaction(
            sub_option(
                "role",
                vec![
                    role_option("role", 11),
                    string_option("bogus", "seize-gate"),
                    int_option("bogus-int", 1),
                    bool_option("bogus-bool", true),
                    channel_option("bogus-channel", CATEGORY),
                    user_option("bogus-user", MEMBER),
                ],
            ),
            true,
        ),
    )
    .await;
    assert!(text.contains("Saved"), "valid set still saves: {text}");
    let mut expected = seeded_controls();
    expected.required_role = Some(11);
    assert_eq!(*fixture.shared.lock().unwrap(), expected);
    assert_one_live_update(&mut rx, &expected, "role set with unknowns");

    // Clearing with only unknown options still clears.
    let text = access_text(
        &fixture,
        &access_interaction(sub_option("role", vec![string_option("bogus", "x")]), true),
    )
    .await;
    assert!(text.contains("Saved"), "clear still saves: {text}");
    expected.required_role = None;
    assert_eq!(*fixture.shared.lock().unwrap(), expected);
    assert_one_live_update(&mut rx, &expected, "clear with unknowns");

    // Restrict plus unknowns saves exactly.
    let (fixture, mut rx) = healthy_fixture(seeded_controls());
    let text = access_text(
        &fixture,
        &access_interaction(
            sub_option(
                "restrict",
                vec![
                    command_option("command", "kick"),
                    role_option("role", 7),
                    role_option("role2", 8),
                    string_option("bogus", "seize-gate"),
                ],
            ),
            true,
        ),
    )
    .await;
    assert!(text.contains("Saved"), "restrict still saves: {text}");
    let mut expected = seeded_controls();
    expected.command_roles.insert("kick".to_owned(), vec![7, 8]);
    assert_eq!(*fixture.shared.lock().unwrap(), expected);
    assert_one_live_update(&mut rx, &expected, "restrict with unknowns");

    // Inapplicable known options stay ignored: roles do not belong to
    // unrestrict, and command/enabled do not belong to role.
    let text = access_text(
        &fixture,
        &access_interaction(
            sub_option(
                "unrestrict",
                vec![
                    command_option("command", "kick"),
                    role_option("role", 99),
                    role_option("role2", 100),
                ],
            ),
            true,
        ),
    )
    .await;
    assert!(text.contains("Saved"), "unrestrict proceeds: {text}");
    expected.command_roles.remove("kick");
    assert_eq!(*fixture.shared.lock().unwrap(), expected);
    assert_one_live_update(&mut rx, &expected, "unrestrict with roles");

    let text = access_text(
        &fixture,
        &access_interaction(
            sub_option(
                "role",
                vec![
                    role_option("role", 11),
                    command_option("command", "kick"),
                    bool_option("enabled", true),
                    int_option("page", 1),
                ],
            ),
            true,
        ),
    )
    .await;
    assert!(text.contains("Saved"), "role set proceeds: {text}");
    expected.required_role = Some(11);
    assert_eq!(*fixture.shared.lock().unwrap(), expected);
    assert_one_live_update(&mut rx, &expected, "role set with inapplicable");

    // Creation plus unknowns saves exactly.
    let text = access_text(
        &fixture,
        &access_interaction(
            sub_option(
                "creation",
                vec![
                    bool_option("enabled", false),
                    string_option("bogus", "seize-gate"),
                ],
            ),
            true,
        ),
    )
    .await;
    assert!(text.contains("Saved"), "creation still saves: {text}");
    expected.room_creation_enabled = false;
    assert_eq!(*fixture.shared.lock().unwrap(), expected);
    assert_one_live_update(&mut rx, &expected, "creation with unknowns");

    // Show plus unknowns still shows page one.
    let before = fixture.shared.lock().unwrap().clone();
    let text = access_text(
        &fixture,
        &access_interaction(sub_option("show", vec![string_option("bogus", "x")]), true),
    )
    .await;
    assert!(
        text.contains("Room creation:") && text.contains("Required role:"),
        "show still shows: {text}"
    );
    assert_no_effects(&fixture, &before, &mut rx, "show with unknowns");
}

#[tokio::test]
async fn genuine_clear_still_clears_and_valid_sets_save_byte_identically() {
    let (fixture, mut rx) = healthy_fixture(seeded_controls());

    // A genuine clear (omitted role) clears the seeded gate.
    let clear = access_interaction(sub_option("role", Vec::new()), true);
    let text = access_text(&fixture, &clear).await;
    assert!(
        text.contains("Saved") && text.contains("Required role: none"),
        "clear acks: {text}"
    );
    let mut expected = seeded_controls();
    expected.required_role = None;
    assert_eq!(*fixture.shared.lock().unwrap(), expected);
    assert_one_live_update(&mut rx, &expected, "genuine clear");

    // A valid set saves exactly, and repeating it is byte-identical.
    let set = access_interaction(sub_option("role", vec![role_option("role", 11)]), true);
    let first = access_text(&fixture, &set).await;
    expected.required_role = Some(11);
    assert_eq!(*fixture.shared.lock().unwrap(), expected);
    assert_one_live_update(&mut rx, &expected, "valid role set");
    let second = access_text(&fixture, &set).await;
    assert_eq!(first, second, "repeat set is byte-identical");
    assert_eq!(*fixture.shared.lock().unwrap(), expected);
    assert_one_live_update(&mut rx, &expected, "repeat role set");

    // A valid restrict saves exactly, and repeating it is byte-identical.
    let restrict = access_interaction(
        sub_option(
            "restrict",
            vec![
                command_option("command", "limit"),
                role_option("role", 5),
                role_option("role2", 6),
            ],
        ),
        true,
    );
    let first = access_text(&fixture, &restrict).await;
    expected
        .command_roles
        .insert("limit".to_owned(), vec![5, 6]);
    assert_eq!(*fixture.shared.lock().unwrap(), expected);
    assert_one_live_update(&mut rx, &expected, "valid restrict");
    let second = access_text(&fixture, &restrict).await;
    assert_eq!(first, second, "repeat restrict is byte-identical");
    assert_eq!(*fixture.shared.lock().unwrap(), expected);
    assert_one_live_update(&mut rx, &expected, "repeat restrict");

    // Duplicate role *values* across distinct slots keep the existing
    // dedup-then-save behavior: only duplicate *names* refuse.
    let text = access_text(
        &fixture,
        &access_interaction(
            sub_option(
                "restrict",
                vec![
                    command_option("command", "kick"),
                    role_option("role", 7),
                    role_option("role2", 7),
                ],
            ),
            true,
        ),
    )
    .await;
    assert!(text.contains("Saved"), "value dedup still saves: {text}");
    expected.command_roles.insert("kick".to_owned(), vec![7]);
    assert_eq!(*fixture.shared.lock().unwrap(), expected);
    assert_one_live_update(&mut rx, &expected, "value dedup save");

    // Creation round-trips exactly.
    for enabled in [false, true] {
        let text = access_text(
            &fixture,
            &access_interaction(
                sub_option("creation", vec![bool_option("enabled", enabled)]),
                true,
            ),
        )
        .await;
        assert!(text.contains("Saved"), "creation saves: {text}");
        expected.room_creation_enabled = enabled;
        assert_eq!(*fixture.shared.lock().unwrap(), expected);
        assert_one_live_update(&mut rx, &expected, "creation round-trip");
    }
}

#[tokio::test]
async fn admission_and_guild_controls_behave() {
    // Non-admins are denied before validation, for valid and malformed
    // shapes alike, with zero effects.
    for sub in [
        sub_option("role", vec![role_option("role", 11)]),
        sub_option("role", vec![string_option("role", "seize-gate")]),
        sub_option(
            "restrict",
            vec![command_option("command", "kick"), role_option("role", 7)],
        ),
        sub_option("show", Vec::new()),
    ] {
        let seeded = seeded_controls();
        let (fixture, mut rx) = healthy_fixture(seeded.clone());
        let (owned, response) =
            handle_capture(&fixture.runtime, &access_interaction(sub, false)).await;
        assert!(owned);
        assert_eq!(
            response_text(&response.expect("denial")),
            "You need Manage Channels to use /access."
        );
        assert_no_effects(&fixture, &seeded, &mut rx, "member denial");
    }

    // An interaction without a guild is not ours: unowned, unanswered,
    // nothing touched.
    let seeded = seeded_controls();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(seeded.clone()));
    let trace = Trace::default();
    let runtime = shared_runtime(trace.clone(), shared.clone(), None);
    let noguild = voice_interaction(
        Some(command_data(
            "access",
            vec![sub_option("role", vec![role_option("role", 11)])],
        )),
        Some(Permissions::MANAGE_CHANNELS),
        false,
    );
    let (owned, response) = handle_capture(&runtime, &noguild).await;
    assert!(!owned, "guildless interaction stays unowned");
    assert!(response.is_none(), "guildless interaction unanswered");
    assert_eq!(*shared.lock().unwrap(), seeded);
    assert!(trace.lock().unwrap().is_empty());

    // A foreign guild with no loaded state is still warming up: the
    // retryable reply touches nothing, including the home guild store.
    let seeded = seeded_controls();
    let (fixture, mut rx) = healthy_fixture(seeded.clone());
    let mut foreign = access_interaction(sub_option("role", vec![role_option("role", 11)]), true);
    foreign.guild_id = Some(Id::new(FOREIGN));
    let (owned, response) = handle_capture(&fixture.runtime, &foreign).await;
    assert!(owned);
    assert_eq!(response_text(&response.expect("warmup")), WARMING_UP_REPLY);
    assert_no_effects(&fixture, &seeded, &mut rx, "foreign warmup");
}

#[test]
fn registry_keeps_the_access_shapes_without_new_required_options() {
    let defs = voice_command_set(&VoiceGates { enabled: true });
    let access = defs
        .iter()
        .find(|def| def.name == "access")
        .expect("access stays published");
    let mut subs: Vec<&str> = access.options.iter().map(|sub| sub.name.as_str()).collect();
    subs.sort_unstable();
    assert_eq!(
        subs,
        ["creation", "restrict", "role", "show", "unrestrict"],
        "no new /access subcommand"
    );
    let shape = |name: &str| -> Vec<&str> {
        access
            .options
            .iter()
            .find(|sub| sub.name == name)
            .unwrap_or_else(|| panic!("{name} stays published"))
            .options
            .iter()
            .map(|opt| opt.name.as_str())
            .collect()
    };
    assert_eq!(shape("show"), ["page"], "show keeps only page");
    assert_eq!(
        shape("creation"),
        ["enabled"],
        "creation keeps only enabled"
    );
    assert_eq!(shape("role"), ["role"], "role keeps only role");
    assert_eq!(
        shape("restrict"),
        ["command", "role", "role2", "role3"],
        "restrict keeps its options"
    );
    assert_eq!(
        shape("unrestrict"),
        ["command"],
        "unrestrict keeps only command"
    );
    let required = |sub: &str, opt: &str| -> Option<bool> {
        access
            .options
            .iter()
            .find(|candidate| candidate.name == sub)
            .unwrap_or_else(|| panic!("{sub} stays published"))
            .options
            .iter()
            .find(|candidate| candidate.name == opt)
            .unwrap_or_else(|| panic!("{sub} {opt} stays published"))
            .required
    };
    // Clearing stays expressible: `role` never becomes required, and the
    // already-required keys stay required.
    assert_ne!(
        required("role", "role"),
        Some(true),
        "role stays optional so a genuine clear works"
    );
    assert_eq!(required("restrict", "command"), Some(true));
    assert_eq!(required("unrestrict", "command"), Some(true));
    assert_eq!(required("creation", "enabled"), Some(true));
    assert_ne!(
        required("show", "page"),
        Some(true),
        "show page stays optional"
    );
}
