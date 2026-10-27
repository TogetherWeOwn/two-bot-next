use serde_json::{json, Value};
use std::collections::BTreeSet;
use two_bot_core::commands::{
    CommandChoice, CommandDefinition, CommandOption, CommandOptionType, EVENT_REFERENCE_MAX_CHARS,
    HELP_DESCRIPTION, MAX_RESOURCE_ID_CHARS, OCCURRENCE_ID_MAX_CHARS, PERM_MANAGE_GUILD,
    PRIVACY_DESCRIPTION, REOPEN_DESCRIPTION,
};
use two_bot_core::custom_commands::{
    MAX_COMMAND_NAME_CHARS, MAX_DESCRIPTION_CHARS, MAX_TEMPLATE_CHARS, MAX_TEXT_TRIGGER_CHARS,
};
use two_bot_core::feeds_http::MAX_FEED_SOURCE_BYTES;
use two_bot_core::lfg::{MAX_ROLE_SPEC_CHARS, MAX_TITLE_CHARS};
use two_bot_core::router::{InteractionRouter, RouterGates};
use two_bot_core::scheduled::MAX_BODY_CHARS;

pub const INTENTIONAL_DIFFERENCES: &[(&str, &str)] = &[
    ("rsvp-attendance", "docs/parity.md §1 #12 / #25"),
    ("rota-acknowledge", "docs/parity.md §1 #13 / §9 drop 1"),
    ("attendance", "docs/parity.md §1 #12 bound"),
    ("help", "docs/parity.md §1 help"),
    ("tempban", "docs/parity.md §1 #4 duration ceiling"),
    ("timeout", "docs/parity.md §1 #6 duration ceiling"),
    ("attendance", "docs/parity.md §1 #12 copy"),
    ("rsvp", "docs/parity.md §1 #24 copy"),
    ("rsvp-attendance", "docs/parity.md §1 #25 copy"),
    ("lfg", "docs/parity.md §1 #26 copy"),
    ("lfg-close", "docs/parity.md §1 #27 copy"),
    ("command", "docs/parity.md §1 #14 caps"),
    ("command-remove", "docs/parity.md §1 #15 caps"),
    ("schedule", "docs/parity.md §1 #17 caps"),
    ("schedule", "docs/parity.md §1 #17 absolute-time"),
    ("schedule", "docs/parity.md §1 #17 preview"),
    ("schedule-remove", "docs/parity.md §1 #18 caps"),
    ("sticky", "docs/parity.md §1 #20 caps"),
    ("sticky", "docs/parity.md §1 #20 preview"),
    ("lfg", "docs/parity.md §1 #26 caps"),
    ("lfg", "docs/parity.md §1 #26 preview"),
    ("lfg-close", "docs/parity.md §1 #27 caps"),
    ("feed-add", "docs/parity.md §1 #28 caps"),
    ("feed-remove", "docs/parity.md §1 #29 caps"),
    ("schedule-list", "docs/parity.md §1 #19 page"),
    ("feed-list", "docs/parity.md §1 #30 page"),
    ("privacy", "docs/parity.md §1 privacy"),
    ("command-list", "docs/parity.md §1 #16 page"),
    ("reopen", "docs/parity.md §1 reopen"),
    ("feed-inspect", "docs/parity.md §1 feed-inspect"),
    ("command-inspect", "docs/parity.md §1 command-inspect"),
    ("game-mix", "docs/parity.md §1 game-mix"),
    ("lfg-edit", "docs/parity.md §1 #26 Next-only companion"),
    ("command-disable", "docs/parity.md §1 command-disable"),
    ("command-enable", "docs/parity.md §1 command-enable"),
    ("giveaway", "docs/parity.md §1 giveaway"),
    ("feed-pause", "docs/parity.md §1 feed-pause"),
    ("feed-resume", "docs/parity.md §1 feed-resume"),
    ("welcome-preview", "docs/parity.md §1 welcome preview"),
    ("purge-preview", "docs/parity.md §1 purge-preview"),
    ("channel-inspect", "docs/parity.md §1 channel-inspect"),
    ("schedule-inspect", "docs/parity.md §1 #47 inspect"),
    ("schedule-edit", "docs/parity.md §1 #48 body correction"),
    ("lfg-list", "docs/parity.md §1 lfg-list"),
    ("rsvp", "docs/parity.md §1 #24 caps"),
    ("rsvp-attendance", "docs/parity.md §1 #25 caps"),
    ("tempban-list", "docs/parity.md §1 tempban-list"),
];

pub fn all_on_router() -> InteractionRouter {
    InteractionRouter::new(RouterGates {
        configured_guild: Some(1),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice: false,
        voice_assistant: false,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    })
}

pub fn legacy_snapshot() -> Value {
    serde_json::from_str(include_str!("../fixtures/legacy_registry.json"))
        .expect("frozen legacy registry JSON")
}

pub fn expected_registry() -> Value {
    let snapshot = legacy_snapshot();
    let mut commands = snapshot["commands"]
        .as_array()
        .expect("command array")
        .clone();
    assert_eq!(
        commands.len(),
        28,
        "full legacy registry, including collision"
    );
    assert_eq!(
        commands
            .iter()
            .filter(|c| c["name"] == "attendance")
            .count(),
        2,
        "do not silently dedupe the legacy attendance collision"
    );
    let rsvp = commands
        .iter_mut()
        .find(|c| c["name"] == "attendance" && c["options"][0]["name"] == "event-id")
        .expect(INTENTIONAL_DIFFERENCES[0].1);
    // The exception changes ONLY this name, never its options or permissions.
    rsvp["name"] = json!(INTENTIONAL_DIFFERENCES[0].0);
    // The bound exception advertises ONLY this max_length on the scorecard
    // `event-occurrence` option; every other field stays legacy-identical.
    let scorecard = commands
        .iter_mut()
        .find(|c| c["name"] == "attendance" && c["options"][0]["name"] == "event-occurrence")
        .expect(INTENTIONAL_DIFFERENCES[2].1);
    scorecard["options"][0]["max_length"] = json!(OCCURRENCE_ID_MAX_CHARS);
    // Duration exceptions add ONLY the service ceiling to each picker option;
    // preserve the frozen legacy payload and compare every other field strictly.
    for (name, max, reference) in [
        ("tempban", 31_536_000_i64, INTENTIONAL_DIFFERENCES[3].1),
        ("timeout", 2_419_200_i64, INTENTIONAL_DIFFERENCES[4].1),
    ] {
        let command = commands
            .iter_mut()
            .find(|c| c["name"] == name)
            .expect(reference);
        let duration = command["options"]
            .as_array_mut()
            .expect("legacy options array")
            .iter_mut()
            .find(|o| o["name"] == "duration_seconds")
            .expect(reference);
        assert_eq!(duration["min_value"], json!(60));
        assert!(duration["max_value"].is_null());
        duration["max_value"] = json!(max);
    }
    // Custom-command caps: advertise ONLY max_length matching the runtime
    // validators; every other field stays legacy-identical.
    for (name, option_name, max, reference) in [
        (
            "command",
            "name",
            MAX_COMMAND_NAME_CHARS,
            "docs/parity.md §1 #14 caps",
        ),
        (
            "command",
            "template",
            MAX_TEMPLATE_CHARS,
            "docs/parity.md §1 #14 caps",
        ),
        (
            "command",
            "description",
            MAX_DESCRIPTION_CHARS,
            "docs/parity.md §1 #14 caps",
        ),
        (
            "command",
            "text-trigger",
            MAX_TEXT_TRIGGER_CHARS,
            "docs/parity.md §1 #14 caps",
        ),
        (
            "command-remove",
            "name",
            MAX_COMMAND_NAME_CHARS,
            "docs/parity.md §1 #15 caps",
        ),
    ] {
        let command = commands
            .iter_mut()
            .find(|c| c["name"] == name)
            .expect(reference);
        let opt = command["options"]
            .as_array_mut()
            .expect("legacy options array")
            .iter_mut()
            .find(|o| o["name"] == option_name)
            .expect(reference);
        assert!(
            opt.get("max_length").is_none(),
            "legacy has no max_length on {name} {option_name}"
        );
        opt["max_length"] = json!(max);
    }
    let rota = commands
        .iter()
        .position(|c| c["name"] == INTENTIONAL_DIFFERENCES[1].0)
        .expect(INTENTIONAL_DIFFERENCES[1].1);
    commands.remove(rota);
    // Next-only `/help` discovery surface: no legacy counterpart. Insert at
    // the publish position (third, after the leveling core) as the exact
    // published definition, so parity pins its shape too.
    let help = serde_json::to_value(CommandDefinition::new("help", HELP_DESCRIPTION).options(
        vec![CommandOption::new(
                "command",
                "Command name (case-insensitive; optional leading /)",
                CommandOptionType::String,
            )
            .max_length(32)],
    ))
    .expect("help serializes");
    commands.insert(2, help);
    // Next-only `/privacy` member self-serve: no legacy counterpart. Insert
    // at the publish position (fourth, after help) as the exact published
    // definition — the same subcommand shape `core_commands` publishes.
    let privacy = serde_json::to_value(
        CommandDefinition::new("privacy", PRIVACY_DESCRIPTION).options(vec![
            CommandOption::new(
                "opt-out",
                "Delete your stored activity for this server and stop future analytics persistence.",
                CommandOptionType::SubCommand,
            ),
            CommandOption::new(
                "opt-in",
                "Resume analytics persistence for this server (already-deleted rows are not restored).",
                CommandOptionType::SubCommand,
            ),
            CommandOption::new(
                "status",
                "Show whether you are opted out and this server's analytics switches.",
                CommandOptionType::SubCommand,
            ),
            CommandOption::new(
                "delete",
                "Request deletion of your stored data for this server.",
                CommandOptionType::SubCommand,
            ),
            CommandOption::new(
                "deletion-status",
                "Show the status of your deletion request.",
                CommandOptionType::SubCommand,
            )
            .sub_options(vec![
                CommandOption::new(
                    "request-id",
                    "The request id returned when you requested deletion.",
                    CommandOptionType::String,
                )
                .required()
                .max_length(128),
            ]),
            CommandOption::new(
                "deletion-receipt",
                "Show the completed receipt for your deletion request.",
                CommandOptionType::SubCommand,
            )
            .sub_options(vec![
                CommandOption::new(
                    "request-id",
                    "The request id returned when you requested deletion.",
                    CommandOptionType::String,
                )
                .required()
                .max_length(128),
            ]),
        ]),
    )
    .expect("privacy serializes");
    commands.insert(3, privacy);
    // Next-only `/reopen` self-service picker recovery: no legacy
    // counterpart. Insert at the publish position (fifth, after privacy) as
    // the exact published definition, so parity pins its shape too.
    let reopen = serde_json::to_value(
        CommandDefinition::new("reopen", REOPEN_DESCRIPTION).options(vec![CommandOption::new(
            "kind",
            "Which picker to reopen.",
            CommandOptionType::String,
        )
        .required()
        .choices(vec![
            CommandChoice {
                name: "Games".to_owned(),
                value: "games".to_owned(),
            },
            CommandChoice {
                name: "Session".to_owned(),
                value: "session".to_owned(),
            },
        ])]),
    )
    .expect("reopen serializes");
    commands.insert(4, reopen);
    let welcome_preview = serde_json::to_value(
        CommandDefinition::new(
            "welcome-preview",
            "Privately preview the current welcome for a synthetic newcomer. No delivery.",
        )
        .permissions(32),
    )
    .expect("welcome preview serializes");
    commands.insert(5, welcome_preview);
    // Next-only private staff aggregate, immediately after welcome-preview. Keep the
    // legacy snapshot frozen and pin the whole additive definition literally,
    // including the optional browse selectors (never a guild/member override).
    commands.insert(
        6,
        json!({
            "name": "game-mix",
            "description": "Show this server's aggregate game activity.",
            "dm_permission": false,
            "default_member_permissions": "32",
            "options": [
                {
                    "name": "page",
                    "description": "Page of saved games to show (10 per page). Default: 1.",
                    "type": 4,
                    "min_value": 1,
                    "max_value": 10000
                },
                {
                    "name": "sort",
                    "description": "Order games by play time or in-voice time. Default: play.",
                    "type": 3,
                    "choices": [
                        {"name": "Play time", "value": "play"},
                        {"name": "In-voice time", "value": "voice"}
                    ]
                },
                {
                    "name": "format",
                    "description": "Show raw JSON or a readable summary. Default: JSON.",
                    "type": 3,
                    "choices": [
                        {"name": "JSON", "value": "json"},
                        {"name": "Summary", "value": "summary"}
                    ]
                }
            ]
        }),
    );
    // Next-only saved-response inspection or unsaved draft preview. This
    // independent literal pins the whole publication contract; never
    // manufacture it from production shapes.
    let inspect = json!({
        "name": "command-inspect",
        "description": "Inspect a saved command or privately preview an unsaved draft",
        "dm_permission": false,
        "default_member_permissions": "32",
        "options": [
            {"name": "name", "description": "Saved command name, a-z 0-9 _ - (choose name or draft)", "type": 3,
             "max_length": 32},
            {"name": "view", "description": "Private view (saved: summary by default; draft: preview only)", "type": 3,
             "choices": [
                 {"name": "Summary", "value": "summary"},
                 {"name": "Saved template", "value": "template"},
                 {"name": "Preview", "value": "preview"}
             ]},
            {"name": "arguments", "description": "Sample invocation arguments (ignored by current templates)",
             "type": 3, "max_length": 500},
            {"name": "draft", "description": "Proposed template to preview without saving (choose name or draft)",
             "type": 3, "max_length": 2000}
        ]
    });
    let after_list = commands
        .iter()
        .position(|command| command["name"] == "command-list")
        .expect("legacy command-list remains published")
        + 1;
    commands.insert(after_list, inspect);
    // Next-only metadata and increase-only seat editing reuse LFG's parity row,
    // never the frozen legacy snapshot. Pin the wire shape independently of production.
    assert!(commands.iter().all(|command| command["name"] != "lfg-edit"));
    let edit = json!({
        "name": "lfg-edit",
        "description": "Update LFG metadata or increase one role's seats",
        "dm_permission": false,
        "default_member_permissions": "8589934592",
        "options": [
            {"name": "id", "description": "LFG id from the posted signup", "type": 3,
             "required": true, "max_length": 128},
            {"name": "title", "description": "Event or group title", "type": 3,
             "max_length": 100},
            {"name": "starts-at", "description": "ISO-8601 start time, e.g. 2026-10-04T18:00:00Z",
             "type": 3},
            {"name": "role", "description": "Existing signup role key (use with seats)",
             "type": 3, "max_length": 32},
            {"name": "seats", "description": "Absolute seat target, increase only (1-99; use with role)",
             "type": 4, "min_value": 1, "max_value": 99}
        ]
    });
    let after_close = commands
        .iter()
        .position(|command| command["name"] == "lfg-close")
        .expect("legacy lfg-close remains published")
        + 1;
    commands.insert(after_close, edit);
    // Saved-response lifecycle is intentionally independent of replacement.
    for (offset, name, description) in [
        (
            1,
            "command-disable",
            "Pause a saved custom command without deleting it",
        ),
        (2, "command-enable", "Resume a saved custom command"),
    ] {
        commands.insert(
            after_list + offset,
            json!({
                "name": name,
                "description": description,
                "dm_permission": false,
                "default_member_permissions": "32",
                "options": [
                    {"name": "name", "description": "Saved command name, a-z 0-9 _ -", "type": 3,
                     "required": true, "max_length": 32}
                ]
            }),
        );
    }
    // Next-only current-channel discovery has its own independent literal.
    // No production constants or registry builders supply either expectation.
    let lfg_list = json!({
        "name": "lfg-list",
        "description": "List open game posts in this channel",
        "dm_permission": false,
        "default_member_permissions": "8589934592",
        "options": [
            {"name": "cursor", "description": "Continue a current-channel list", "type": 3,
             "max_length": 512}
        ]
    });
    let after_edit = commands
        .iter()
        .position(|c| c["name"] == "lfg-edit")
        .expect("lfg-edit is published")
        + 1;
    commands.insert(after_edit, lfg_list);
    // Next-only `/giveaway` timed entry: no legacy counterpart. Insert at the
    // publish position (index 19, immediately after sticky-remove, with
    // game-mix, welcome-preview, pause/resume and inspection inserts above)
    // as the exact published definition, so parity pins its shape too.
    let giveaway = serde_json::to_value(
        CommandDefinition::new("giveaway", "Create, draw, reroll or cancel a giveaway")
            .permissions(PERM_MANAGE_GUILD)
            .options(vec![
                CommandOption::new(
                    "create",
                    "Create a giveaway in this channel",
                    CommandOptionType::SubCommand,
                )
                .sub_options(vec![
                    CommandOption::new(
                        "prize",
                        "Prize text (up to 200 characters)",
                        CommandOptionType::String,
                    )
                    .required()
                    .max_length(200),
                    CommandOption::new(
                        "duration",
                        "Entry window in minutes (1–43200)",
                        CommandOptionType::Integer,
                    )
                    .required()
                    .int_range(1, 43200),
                    CommandOption::new("winners", "Number of winners", CommandOptionType::Integer)
                        .required()
                        .int_range(1, 100),
                    CommandOption::new(
                        "required-role",
                        "Role every entrant must hold",
                        CommandOptionType::Role,
                    ),
                    CommandOption::new(
                        "min-account-age-days",
                        "Minimum account age in days",
                        CommandOptionType::Integer,
                    )
                    .int_range(0, 3650),
                    CommandOption::new(
                        "min-server-age-days",
                        "Minimum time in this server in days",
                        CommandOptionType::Integer,
                    )
                    .int_range(0, 3650),
                ]),
                CommandOption::new("list", "List open giveaways", CommandOptionType::SubCommand),
                CommandOption::new(
                    "cancel",
                    "Cancel an open or paused giveaway",
                    CommandOptionType::SubCommand,
                )
                .sub_options(vec![CommandOption::new(
                    "id",
                    "Giveaway id",
                    CommandOptionType::String,
                )
                .required()
                .max_length(64)]),
                CommandOption::new(
                    "end",
                    "Draw now, before the deadline",
                    CommandOptionType::SubCommand,
                )
                .sub_options(vec![CommandOption::new(
                    "id",
                    "Giveaway id",
                    CommandOptionType::String,
                )
                .required()
                .max_length(64)]),
                CommandOption::new(
                    "pause",
                    "Pause entry without drawing",
                    CommandOptionType::SubCommand,
                )
                .sub_options(vec![CommandOption::new(
                    "id",
                    "Giveaway id",
                    CommandOptionType::String,
                )
                .required()
                .max_length(64)]),
                CommandOption::new(
                    "resume",
                    "Resume a paused giveaway",
                    CommandOptionType::SubCommand,
                )
                .sub_options(vec![CommandOption::new(
                    "id",
                    "Giveaway id",
                    CommandOptionType::String,
                )
                .required()
                .max_length(64)]),
                CommandOption::new(
                    "extend",
                    "Add minutes to the deadline",
                    CommandOptionType::SubCommand,
                )
                .sub_options(vec![
                    CommandOption::new("id", "Giveaway id", CommandOptionType::String)
                        .required()
                        .max_length(64),
                    CommandOption::new(
                        "duration-minutes",
                        "Minutes to add (1–43200)",
                        CommandOptionType::Integer,
                    )
                    .required()
                    .int_range(1, 43200),
                ]),
                CommandOption::new(
                    "reroll",
                    "Redraw the winners",
                    CommandOptionType::SubCommand,
                )
                .sub_options(vec![
                    CommandOption::new("id", "Giveaway id", CommandOptionType::String)
                        .required()
                        .max_length(64),
                    CommandOption::new("user", "Replace only this winner", CommandOptionType::User),
                ]),
                CommandOption::new(
                    "inspect",
                    "Privately inspect saved giveaway details in this channel",
                    CommandOptionType::SubCommand,
                )
                .sub_options(vec![CommandOption::new(
                    "id",
                    "Giveaway id",
                    CommandOptionType::String,
                )
                .required()
                .max_length(64)]),
            ]),
    )
    .expect("giveaway serializes");
    commands.insert(19, giveaway);
    // Next-only feed lifecycle pair. These independent literals pin the whole
    // publication contract; never manufacture them from production shapes.
    // The feed-remove lookup stays dynamic so the giveaway insert above does
    // not shift the pair's expected position.
    let pause = json!({
        "name": "feed-pause",
        "description": "Pause a feed relay without deleting its history",
        "dm_permission": false,
        "default_member_permissions": "32",
        "options": [
            {"name": "id", "description": "Feed id", "type": 3,
             "required": true, "max_length": 128}
        ]
    });
    let resume = json!({
        "name": "feed-resume",
        "description": "Resume a paused feed relay",
        "dm_permission": false,
        "default_member_permissions": "32",
        "options": [
            {"name": "id", "description": "Feed id", "type": 3,
             "required": true, "max_length": 128}
        ]
    });
    let after_remove = commands
        .iter()
        .position(|command| command["name"] == "feed-remove")
        .expect("legacy feed-remove remains published")
        + 1;
    commands.insert(after_remove, pause);
    commands.insert(after_remove + 1, resume);
    // Next-only read surfaces follow the nine historical mutations. Literal
    // payloads keep the frozen snapshot intact and compare every read field.
    let after_unlock = commands
        .iter()
        .position(|command| command["name"] == "unlock")
        .expect("legacy unlock remains published")
        + 1;
    commands.insert(
        after_unlock,
        json!({
            "name": "purge-preview",
            "description": "Privately preview a purge without deleting messages",
            "dm_permission": false,
            "default_member_permissions": "8192",
            "options": [{"name": "count", "description": "Newest messages to sample (1-100)",
                "type": 4, "required": true, "min_value": 1, "max_value": 100}]
        }),
    );
    commands.insert(
        after_unlock + 1,
        json!({
            "name": "channel-inspect",
            "description": "Privately inspect saved lockdown and live slowmode",
            "options": [],
            "dm_permission": false,
            "default_member_permissions": "16"
        }),
    );
    // Picker-copy exceptions (registry golden exceptions table): command and
    // option descriptions intentionally differ from legacy. Pointers mirror
    // the published option order.
    for (name, pointer, value) in [
        (
            "attendance",
            "/description",
            "Check in a verified human attendee for a scheduled event (scorecard)",
        ),
        (
            "attendance",
            "/options/0/description",
            "Event id, event link or {event_id}:{label}; bare slugs refuse, e.g. 12345:weekly",
        ),
        (
            "rsvp",
            "/options/0/description",
            "Scheduled event id or pasted event link, e.g. 12345",
        ),
        (
            "rsvp-attendance",
            "/description",
            "Show RSVP totals and your saved response for an event",
        ),
        (
            "rsvp-attendance",
            "/options/0/description",
            "Scheduled event id or pasted event link, e.g. 12345",
        ),
        (
            "lfg",
            "/options/1/description",
            "ISO-8601 start time, e.g. 2026-10-04T18:00:00Z",
        ),
        (
            "lfg",
            "/options/2/description",
            "Role slots as role:Label:count, comma-separated, e.g. tank:Tank:2,dps:DPS:6",
        ),
        (
            "lfg-close",
            "/options/0/description",
            "LFG id from the posted signup",
        ),
    ] {
        let command = commands
            .iter_mut()
            .find(|c| c["name"] == name)
            .expect("copy exception names a published command");
        *command.pointer_mut(pointer).expect("copy field exists") = json!(value);
    }
    // Further caps from the input-bound fix: advertise ONLY max_length
    // matching the runtime validators; the frozen legacy registry fixture
    // remains untouched. Custom-command caps are covered by the loop above.
    for (name, option_name, max_length, reference) in [
        (
            "schedule",
            "body",
            MAX_BODY_CHARS,
            "docs/parity.md §1 #17 caps",
        ),
        (
            "schedule-remove",
            "id",
            MAX_RESOURCE_ID_CHARS,
            "docs/parity.md §1 #18 caps",
        ),
        (
            "sticky",
            "body",
            MAX_BODY_CHARS,
            "docs/parity.md §1 #20 caps",
        ),
        (
            "lfg",
            "title",
            MAX_TITLE_CHARS,
            "docs/parity.md §1 #26 caps",
        ),
        (
            "lfg",
            "roles",
            MAX_ROLE_SPEC_CHARS,
            "docs/parity.md §1 #26 caps",
        ),
        (
            "lfg-close",
            "id",
            MAX_RESOURCE_ID_CHARS,
            "docs/parity.md §1 #27 caps",
        ),
        (
            "feed-add",
            "source",
            MAX_FEED_SOURCE_BYTES,
            "docs/parity.md §1 #28 caps",
        ),
        (
            "feed-remove",
            "id",
            MAX_RESOURCE_ID_CHARS,
            "docs/parity.md §1 #29 caps",
        ),
        (
            "rsvp",
            "event-id",
            EVENT_REFERENCE_MAX_CHARS,
            "docs/parity.md §1 #24 caps",
        ),
        (
            "rsvp-attendance",
            "event-id",
            EVENT_REFERENCE_MAX_CHARS,
            "docs/parity.md §1 #25 caps",
        ),
    ] {
        let _ = INTENTIONAL_DIFFERENCES
            .iter()
            .find(|(command, _)| *command == name)
            .expect("cap has a documented command exception");
        let command = commands
            .iter_mut()
            .find(|command| command["name"] == name)
            .expect(reference);
        let option = command["options"]
            .as_array_mut()
            .expect("command options array")
            .iter_mut()
            .find(|option| option["name"] == option_name)
            .expect(reference);
        assert!(
            option["max_length"].is_null(),
            "legacy {name}/{option_name}"
        );
        option["max_length"] = json!(max_length);
    }
    // Sticky preview adds only an optional Boolean to the legacy setup fields.
    {
        let reference = "docs/parity.md §1 #20 preview";
        assert!(INTENTIONAL_DIFFERENCES.contains(&("sticky", reference)));
        let command = commands
            .iter_mut()
            .find(|command| command["name"] == "sticky")
            .expect(reference);
        let options = command["options"].as_array_mut().expect("sticky options");
        assert_eq!(options.len(), 2, "legacy body and debounce stay intact");
        assert_eq!(options[0]["name"], json!("body"));
        assert_eq!(options[1]["name"], json!("debounce"));
        options.push(json!({
            "name": "preview",
            "description": "Privately preview without saving",
            "type": 5
        }));
    }
    // LFG private preview: the Next-only additive fourth `preview` boolean
    // keeps the signup form submittable without posting. The three legacy
    // string options (title, starts-at, roles) and their bounds stay
    // untouched; the option itself is optional (`required` omitted, which the
    // canonical form normalizes to false) with no bounds or choices.
    {
        let _ = INTENTIONAL_DIFFERENCES
            .iter()
            .find(|(command, reference)| {
                *command == "lfg" && *reference == "docs/parity.md §1 #26 preview"
            })
            .expect("preview has a documented command exception");
        let command = commands
            .iter_mut()
            .find(|command| command["name"] == "lfg")
            .expect("docs/parity.md §1 #26 preview");
        assert_eq!(command["default_member_permissions"], json!("8589934592"));
        assert_eq!(command["dm_permission"], json!(false));
        let options = command["options"]
            .as_array_mut()
            .expect("command options array");
        assert_eq!(
            options.len(),
            3,
            "legacy lfg publishes exactly title, starts-at, roles"
        );
        assert_eq!(options[0]["name"], json!("title"));
        assert_eq!(options[1]["name"], json!("starts-at"));
        assert_eq!(options[2]["name"], json!("roles"));
        for option in options.iter() {
            assert_eq!(option["type"], json!(3), "legacy lfg options stay strings");
            assert_eq!(option["required"], json!(true));
        }
        options.push(json!({
            "name": "preview",
            "description": "Privately preview without posting",
            "type": 5
        }));
    }
    // Absolute-time scheduling adds ONLY one optional string to the legacy
    // body / in-minutes / every-minutes options, leaving their fields intact.
    // Keep this independent of the compiled production registry.
    {
        let reference = "docs/parity.md §1 #17 absolute-time";
        assert!(INTENTIONAL_DIFFERENCES.contains(&("schedule", reference)));
        let command = commands
            .iter_mut()
            .find(|command| command["name"] == "schedule")
            .expect(reference);
        let options = command["options"]
            .as_array_mut()
            .expect("schedule options array");
        assert_eq!(options.len(), 3, "legacy schedule has three options");
        assert_eq!(options[0]["name"], json!("body"));
        assert_eq!(options[1]["name"], json!("in-minutes"));
        assert_eq!(options[2]["name"], json!("every-minutes"));
        options.push(json!({
            "name": "at",
            "description": "One-off RFC3339 time with Z or offset; cannot combine with other timing",
            "type": 3,
            "max_length": 64
        }));
    }
    // Proposed-post preview adds ONLY one optional boolean after `at`: the
    // picker shows the post and its first firing without saving anything.
    // The four earlier options and their bounds stay untouched; the option
    // itself is optional (`required` omitted, which the canonical form
    // normalizes to false) with no bounds or choices.
    {
        let reference = "docs/parity.md §1 #17 preview";
        assert!(INTENTIONAL_DIFFERENCES.contains(&("schedule", reference)));
        let command = commands
            .iter_mut()
            .find(|command| command["name"] == "schedule")
            .expect(reference);
        let options = command["options"]
            .as_array_mut()
            .expect("schedule options array");
        assert_eq!(options.len(), 4, "legacy schedule plus absolute-time");
        assert_eq!(options[3]["name"], json!("at"));
        options.push(json!({
            "name": "preview",
            "description": "Preview the proposed post and its first firing without saving it",
            "type": 5
        }));
    }
    // Schedule-list pagination: the Next-only additive `page` option keeps
    // every configured id discoverable within Discord's 2,000-unit cap.
    // Legacy publishes no options on `schedule-list`; permissions and the
    // command name stay legacy-identical.
    {
        let _ = INTENTIONAL_DIFFERENCES
            .iter()
            .find(|(command, _)| *command == "schedule-list")
            .expect("page has a documented command exception");
        let command = commands
            .iter_mut()
            .find(|command| command["name"] == "schedule-list")
            .expect("docs/parity.md §1 #19 page");
        assert!(
            command["options"]
                .as_array()
                .expect("command options array")
                .is_empty(),
            "legacy schedule-list has no options"
        );
        let page = serde_json::to_value(
            CommandOption::new(
                "page",
                "Page of the schedule list (1-based)",
                two_bot_core::commands::CommandOptionType::Integer,
            )
            .min_value(1),
        )
        .expect("page option serializes");
        command["options"] = json!([page]);
    }
    // Feed-list pagination: the Next-only additive `page` option keeps every
    // stored relay id discoverable as whole rows within Discord's 2,000-unit
    // cap. Legacy publishes no options on `feed-list`; permissions and the
    // command name stay legacy-identical.
    {
        let _ = INTENTIONAL_DIFFERENCES
            .iter()
            .find(|(command, _)| *command == "feed-list")
            .expect("page has a documented command exception");
        let command = commands
            .iter_mut()
            .find(|command| command["name"] == "feed-list")
            .expect("docs/parity.md §1 #30 page");
        assert!(
            command["options"]
                .as_array()
                .expect("command options array")
                .is_empty(),
            "legacy feed-list has no options"
        );
        let page = serde_json::to_value(
            CommandOption::new(
                "page",
                "Page of the feed relay list (1-based)",
                two_bot_core::commands::CommandOptionType::Integer,
            )
            .min_value(1),
        )
        .expect("page option serializes");
        command["options"] = json!([page]);
    }
    // Custom-command paging changes ONLY the formerly empty option list.
    // Keep descriptions, permissions and every other legacy field pinned.
    {
        let _ = INTENTIONAL_DIFFERENCES
            .iter()
            .find(|(command, _)| *command == "command-list")
            .expect("page has a documented command exception");
        let command = commands
            .iter_mut()
            .find(|command| command["name"] == "command-list")
            .expect("docs/parity.md §1 #16 page");
        assert!(
            command["options"]
                .as_array()
                .expect("command options array")
                .is_empty(),
            "legacy command-list has no options"
        );
        let page = serde_json::to_value(
            CommandOption::new(
                "page",
                "Page of the custom-command list (1-based)",
                CommandOptionType::Integer,
            )
            .min_value(1),
        )
        .expect("page option serializes");
        command["options"] = json!([page]);
    }
    // Additive management surface; never rewrite the frozen legacy payload or
    // relax the comparison for any existing command or option.
    let after_list = commands
        .iter()
        .position(|c| c["name"] == "schedule-list")
        .expect("schedule-list remains published")
        + 1;
    let additions = [
        CommandDefinition::new("schedule-inspect", "Inspect a saved scheduled message")
            .permissions(32)
            .options(vec![
                CommandOption::new("id", "Scheduled message id", CommandOptionType::String)
                    .required()
                    .max_length(128),
                CommandOption::new(
                    "view",
                    "metadata (default) or body",
                    CommandOptionType::String,
                )
                .max_length(8),
            ]),
        CommandDefinition::new("schedule-edit", "Correct only a scheduled message body")
            .permissions(32)
            .options(vec![
                CommandOption::new("id", "Scheduled message id", CommandOptionType::String)
                    .required()
                    .max_length(128),
                CommandOption::new(
                    "revision",
                    "Revision from schedule-inspect",
                    CommandOptionType::String,
                )
                .required()
                .max_length(128),
                CommandOption::new(
                    "body",
                    "Replacement message text",
                    CommandOptionType::String,
                )
                .required()
                .max_length(2000),
            ]),
    ];
    for (offset, definition) in additions.into_iter().enumerate() {
        commands.insert(
            after_list + offset,
            serde_json::to_value(definition).expect("management definition serializes"),
        );
    }
    // Next-only saved-relay inspection has no frozen legacy counterpart.
    // Pin the documented shape independently: guild-only, Manage Server, and
    // exactly one required stable-id string capped at 128.
    let feed_inspect_position = commands
        .iter()
        .position(|command| command["name"] == "feed-list")
        .expect("feed-list anchors the announcement set")
        + 1;
    commands.insert(
        feed_inspect_position,
        json!({
            "name": "feed-inspect",
            "description": "Show a saved feed relay's source and last attempt",
            "type": 1,
            "default_member_permissions": "32",
            "dm_permission": false,
            "options": [{
                "name": "id",
                "description": "Feed id",
                "type": 3,
                "required": true,
                "max_length": 128
            }]
        }),
    );
    let after_tempban = commands
        .iter()
        .position(|command| command["name"] == "tempban")
        .expect("tempban anchors saved-expiry inspection")
        + 1;
    commands.insert(
        after_tempban,
        json!({
            "name": "tempban-list",
            "description": "Inspect saved temporary-ban expiry work",
            "type": 1,
            "default_member_permissions": "4",
            "dm_permission": false,
            "options": [{
                "name": "page",
                "description": "Saved expiry page (10 rows, default 1; due time then work ID order)",
                "type": 4,
                "required": false,
                "min_value": 1,
                "max_value": 10000
            }]
        }),
    );
    canonical_registry(json!(commands))
}

// Equivalent API defaults, not behavioural waivers. This is a GUILD bulk-set:
// absent dm_permission and false both remain guild-only. Preserve true so an
// accidental change is reported. Command order, option order, descriptions,
// all bounds, choices, permissions, and unknown fields are compared verbatim.
pub fn canonical_registry(mut commands: Value) -> Value {
    for command in commands
        .as_array_mut()
        .expect("publish JSON must be an array")
    {
        let obj = command.as_object_mut().expect("command object");
        obj.entry("type").or_insert(json!(1)); // Discord defaults to ChatInput.
        obj.entry("options").or_insert(json!([]));
        if obj.get("default_member_permissions") == Some(&Value::Null) {
            obj.remove("default_member_permissions");
        }
        if obj.get("dm_permission") == Some(&json!(false)) {
            obj.remove("dm_permission");
        }
        // Twilight sends a server-assigned version placeholder on bulk set.
        // No other non-default field (including a different version) is ignored.
        if obj.get("version") == Some(&json!("1")) {
            obj.remove("version");
        }
        for option in obj["options"].as_array_mut().expect("options array") {
            option
                .as_object_mut()
                .expect("option object")
                .entry("required")
                .or_insert(json!(false));
        }
    }
    commands
}

pub fn registry_diff(actual: Value) -> Vec<String> {
    let mut differences = Vec::new();
    diff(
        "/commands",
        Some(&expected_registry()),
        Some(&canonical_registry(actual)),
        &mut differences,
    );
    differences
}

pub fn assert_registry_parity(actual: Value) {
    let differences = registry_diff(actual);
    assert!(differences.is_empty(),
        "Unlisted registry drift (legacy - / next +). Only {INTENTIONAL_DIFFERENCES:?} are allowed:\n{}",
        differences.join("\n"));
}

fn diff(path: &str, expected: Option<&Value>, actual: Option<&Value>, out: &mut Vec<String>) {
    if expected == actual {
        return;
    }
    match (expected, actual) {
        (Some(Value::Object(left)), Some(Value::Object(right))) => {
            let keys: BTreeSet<_> = left.keys().chain(right.keys()).collect();
            for key in keys {
                diff(&format!("{path}/{key}"), left.get(key), right.get(key), out);
            }
        }
        (Some(Value::Array(left)), Some(Value::Array(right))) => {
            for i in 0..left.len().max(right.len()) {
                let label = left
                    .get(i)
                    .or_else(|| right.get(i))
                    .and_then(|v| v.get("name"))
                    .and_then(Value::as_str)
                    .map(|name| format!(" ({name})"))
                    .unwrap_or_default();
                diff(
                    &format!("{path}/{i}{label}"),
                    left.get(i),
                    right.get(i),
                    out,
                );
            }
        }
        _ => out.push(format!(
            "{path}\n  - {}\n  + {}",
            expected
                .map(Value::to_string)
                .unwrap_or_else(|| "<missing>".to_owned()),
            actual
                .map(Value::to_string)
                .unwrap_or_else(|| "<missing>".to_owned())
        )),
    }
}
