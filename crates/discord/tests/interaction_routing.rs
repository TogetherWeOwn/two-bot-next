//! TOG-10075 acceptance: the interaction router routes every parity §1 row
//! to its registered handler (stub) or the refusal path, components route by
//! prefix, and publish sends the complete merged set once.
//!
//! Two layers, mirroring `funnel_replay.rs`:
//!
//! 1. `route_*` — in-memory twilight `Interaction`s through
//!    [`route_interaction`]: all 27 builtins, gates-off refusals, permission
//!    refusals, custom rows, unknown names/ids, modal submits, and the
//!    refusal-response wire shape.
//! 2. `publish_*` — the router's complete set through the real
//!    `twilight_http` bulk-set endpoint against the mock Discord double in
//!    `common`: one request, all 28 names, guild-only, permission bits as
//!    decimal strings. The REST executor ([TOG-10076]) owns the production
//!    call; this proves the payload it will send.
//!
//! Dev-only: never ships in the release binary.

// The shared double also serves the gateway half (S2's test); this target
// only drives its HTTP side, so the gateway members read as unused here.
#[allow(dead_code)]
mod common;

use common::{MockDiscord, APP_ID, GUILD_ID};
use twilight_model::{
    application::{
        command::CommandType,
        interaction::InteractionType,
        interaction::{
            application_command::{CommandData, CommandDataOption},
            message_component::MessageComponentInteractionData,
            modal::ModalInteractionData,
            Interaction, InteractionData,
        },
    },
    channel::message::component::ComponentType,
    gateway::payload::incoming::InteractionCreate,
    guild::{MemberFlags, Permissions},
    id::{AnonymizableId, Id},
    oauth::ApplicationIntegrationMap,
    user::User,
};
use two_bot_core::{
    ComponentHandler, ComponentOutcome, CustomCommand, HandlerId, InteractionHandler,
    InteractionRouter, ModerationAction, RouterGates, RouterRefusal, SlashOutcome,
    ANNOUNCEMENTS_DISABLED_REPLY, AUTOMATIONS_DISABLED_REPLY, GAME_SELECT_ID,
    GUILD_RESTRICTED_REPLY, LFG_PREFIX, MANAGE_EVENTS_REQUIRED, MANAGE_SERVER_REQUIRED,
    MODERATION_DISABLED_REPLY, SCORECARD_DISABLED_REPLY, SELF_ROLE_PREFIX, SESSION_SELECT_ID,
    TICKET_CLAIM_ID, TICKET_CLOSE_ID, TICKET_OPEN_ID,
};
use two_bot_discord::{
    command_to_twilight, publish_commands, refusal_response, response_for_slash, route_interaction,
    RoutedInteraction,
};

fn all_on() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD_ID),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    }
}

fn user(id: u64, bot: bool) -> User {
    User {
        accent_color: None,
        avatar: None,
        avatar_decoration: None,
        avatar_decoration_data: None,
        banner: None,
        bot,
        discriminator: 0,
        email: None,
        flags: None,
        global_name: None,
        id: Id::new(id),
        locale: None,
        mfa_enabled: None,
        name: "member".to_owned(),
        premium_type: None,
        primary_guild: None,
        public_flags: None,
        system: None,
        verified: None,
    }
}

#[allow(deprecated)]
fn slash(name: &str, permissions: Option<u64>) -> Interaction {
    Interaction {
        app_permissions: None,
        application_id: Id::new(APP_ID),
        authorizing_integration_owners: ApplicationIntegrationMap {
            guild: Some(AnonymizableId::Id(Id::new(GUILD_ID))),
            user: None,
        },
        channel: None,
        channel_id: None,
        context: None,
        data: Some(InteractionData::ApplicationCommand(Box::new(CommandData {
            guild_id: None,
            id: Id::new(1),
            name: name.to_owned(),
            kind: CommandType::ChatInput,
            options: Vec::<CommandDataOption>::new(),
            resolved: None,
            target_id: None,
        }))),
        entitlements: Vec::new(),
        guild: None,
        guild_id: Some(Id::new(GUILD_ID)),
        guild_locale: None,
        id: Id::new(7),
        kind: InteractionType::ApplicationCommand,
        locale: None,
        member: Some(twilight_model::guild::PartialMember {
            avatar: None,
            avatar_decoration_data: None,
            banner: None,
            communication_disabled_until: None,
            deaf: false,
            flags: MemberFlags::empty(),
            joined_at: None,
            mute: false,
            nick: None,
            permissions: permissions.map(Permissions::from_bits_truncate),
            premium_since: None,
            roles: Vec::new(),
            user: None,
        }),
        message: None,
        token: "routing-test-token".to_owned(),
        user: Some(user(99, false)),
    }
}

#[allow(deprecated)]
fn component(custom_id: &str, values: Vec<String>) -> Interaction {
    Interaction {
        app_permissions: None,
        application_id: Id::new(APP_ID),
        authorizing_integration_owners: ApplicationIntegrationMap {
            guild: Some(AnonymizableId::Id(Id::new(GUILD_ID))),
            user: None,
        },
        channel: None,
        channel_id: None,
        context: None,
        data: Some(InteractionData::MessageComponent(Box::new(
            MessageComponentInteractionData {
                custom_id: custom_id.to_owned(),
                component_type: ComponentType::TextSelectMenu,
                resolved: None,
                values,
            },
        ))),
        entitlements: Vec::new(),
        guild: None,
        guild_id: Some(Id::new(GUILD_ID)),
        guild_locale: None,
        id: Id::new(8),
        kind: InteractionType::MessageComponent,
        locale: None,
        member: None,
        message: None,
        token: "routing-test-token".to_owned(),
        user: Some(user(99, false)),
    }
}

#[allow(deprecated)]
fn modal(custom_id: &str) -> Interaction {
    Interaction {
        app_permissions: None,
        application_id: Id::new(APP_ID),
        authorizing_integration_owners: ApplicationIntegrationMap {
            guild: Some(AnonymizableId::Id(Id::new(GUILD_ID))),
            user: None,
        },
        channel: None,
        channel_id: None,
        context: None,
        data: Some(InteractionData::ModalSubmit(Box::new(
            ModalInteractionData {
                components: Vec::new(),
                custom_id: custom_id.to_owned(),
                resolved: None,
            },
        ))),
        entitlements: Vec::new(),
        guild: None,
        guild_id: Some(Id::new(GUILD_ID)),
        guild_locale: None,
        id: Id::new(9),
        kind: InteractionType::ModalSubmit,
        locale: None,
        member: None,
        message: None,
        token: "routing-test-token".to_owned(),
        user: Some(user(99, false)),
    }
}

#[derive(Debug)]
struct Stub(HandlerId);

impl InteractionHandler for Stub {
    fn id(&self) -> HandlerId {
        self.0
    }
}

// Capture structured security events without a global subscriber or a store.
type PermissionAuditFields = std::collections::BTreeMap<String, String>;

#[derive(Clone, Default)]
struct PermissionAudit(std::sync::Arc<std::sync::Mutex<Vec<PermissionAuditFields>>>);

impl tracing::Subscriber for PermissionAudit {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        #[derive(Default)]
        struct Fields(std::collections::BTreeMap<String, String>);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.insert(field.name().to_owned(), format!("{value:?}"));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.insert(field.name().to_owned(), value.to_owned());
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        fields
            .0
            .insert("target".to_owned(), event.metadata().target().to_owned());
        self.0.lock().unwrap().push(fields.0);
    }
}

#[test]
fn overridden_discord_defaults_cannot_bypass_permissions_and_denials_are_audited() {
    use twilight_model::channel::message::MessageFlags;
    use two_bot_core::command_permissions::{CommandSurface, COMMAND_PERMISSIONS};

    let router = InteractionRouter::new(all_on());
    let audit = PermissionAudit::default();
    let restricted: Vec<_> = COMMAND_PERMISSIONS
        .iter()
        .filter(|row| row.surface == CommandSurface::BuiltinSlash && row.required_permissions != 0)
        .collect();
    assert_eq!(restricted.len(), 23);
    tracing::subscriber::with_default(audit.clone(), || {
        for row in &restricted {
            for permissions in [None, Some(0), Some(!row.required_permissions)] {
                // Discord delivered a real ApplicationCommand despite the picker
                // defaults; bot permissions must not authorize the invoker.
                let mut interaction = slash(row.command, permissions);
                interaction.app_permissions = Some(Permissions::all());
                let RoutedInteraction::Slash { outcome, .. } =
                    route_interaction(&router, &interaction, Some(true))
                else {
                    panic!("expected slash refusal for {}", row.command);
                };
                let SlashOutcome::Refuse { refusal } = outcome else {
                    panic!(
                        "unauthorized invocation returned a handler for {}",
                        row.command
                    );
                };
                let response = response_for_slash(&outcome).expect("router owes a refusal reply");
                let data = response.data.unwrap();
                assert_eq!(data.flags, Some(MessageFlags::EPHEMERAL));
                assert_eq!(data.content, Some(refusal.message()));
            }
            let allowed = slash(row.command, Some(row.required_permissions));
            assert!(matches!(
                route_interaction(&router, &allowed, None),
                RoutedInteraction::Slash {
                    outcome: SlashOutcome::Handled { .. },
                    ..
                }
            ));
        }
    });
    let events = audit.0.lock().unwrap();
    assert_eq!(events.len(), restricted.len() * 3);
    for (row, events) in restricted.iter().zip(events.chunks_exact(3)) {
        for event in events {
            assert_eq!(event["message"], "command_permission_denied");
            assert_eq!(event["target"], "two_bot_core::command_permissions");
            assert_eq!(event["command"], row.command);
            assert_eq!(
                event["required_permissions"],
                row.required_permissions.to_string()
            );
            assert_eq!(event["guild_id"], format!("Some({GUILD_ID})"));
            // Exact metadata allowlist excludes tokens, options and user text.
            assert_eq!(
                event.keys().map(String::as_str).collect::<Vec<_>>(),
                vec![
                    "actor_permissions",
                    "command",
                    "guild_id",
                    "message",
                    "required_permissions",
                    "target"
                ]
            );
            assert!(!format!("{event:?}").contains("routing-test-token"));
        }
        assert_eq!(events[0]["actor_permissions"], "None");
        assert_eq!(events[1]["actor_permissions"], "Some(0)");
    }
}

#[test]
fn twilight_publication_permissions_equal_the_runtime_matrix() {
    use two_bot_core::command_permissions::{command_permission, COMMAND_PERMISSIONS};

    assert_eq!(COMMAND_PERMISSIONS.len(), 30);
    let router = InteractionRouter::new(all_on());
    let defs = router.publish_set(&[]).unwrap();
    let commands = publish_commands(&defs);
    assert_eq!(commands.len(), 27);
    for command in commands {
        let row = command_permission(&command.name).unwrap();
        assert_eq!(
            command
                .default_member_permissions
                .map(|p| p.bits())
                .unwrap_or(0),
            row.required_permissions,
            "{}",
            command.name
        );
        let wire = serde_json::to_value(&command).unwrap();
        if row.required_permissions == 0 {
            assert!(wire
                .get("default_member_permissions")
                .is_none_or(serde_json::Value::is_null));
        } else {
            assert_eq!(
                wire["default_member_permissions"],
                row.required_permissions.to_string()
            );
        }
    }
}

fn slash_outcome(router: &InteractionRouter, name: &str) -> SlashOutcome {
    let interaction = slash(name, Some(u64::MAX));
    match route_interaction(router, &interaction, None) {
        RoutedInteraction::Slash { outcome, .. } => outcome,
        other => panic!("{name} must route as slash, got {other:?}"),
    }
}

#[test]
fn every_section1_row_routes_to_its_registered_handler() {
    let mut router = InteractionRouter::new(all_on());
    let cases: &[(&str, HandlerId)] = &[
        ("rank", HandlerId::Rank),
        ("leaderboard", HandlerId::Leaderboard),
        ("ban", HandlerId::Moderation(ModerationAction::Ban)),
        ("tempban", HandlerId::Moderation(ModerationAction::TempBan)),
        ("kick", HandlerId::Moderation(ModerationAction::Kick)),
        ("timeout", HandlerId::Moderation(ModerationAction::Timeout)),
        ("warn", HandlerId::Moderation(ModerationAction::Warn)),
        ("purge", HandlerId::Moderation(ModerationAction::Purge)),
        (
            "slowmode",
            HandlerId::Moderation(ModerationAction::Slowmode),
        ),
        (
            "lockdown",
            HandlerId::Moderation(ModerationAction::Lockdown),
        ),
        ("unlock", HandlerId::Moderation(ModerationAction::Unlock)),
        ("attendance", HandlerId::ScorecardAttendance),
        ("command", HandlerId::AutomationAdmin),
        ("command-remove", HandlerId::AutomationAdmin),
        ("command-list", HandlerId::AutomationAdmin),
        ("schedule", HandlerId::AutomationAdmin),
        ("schedule-remove", HandlerId::AutomationAdmin),
        ("schedule-list", HandlerId::AutomationAdmin),
        ("sticky", HandlerId::AutomationAdmin),
        ("sticky-remove", HandlerId::AutomationAdmin),
        ("rsvp", HandlerId::Rsvp),
        ("rsvp-attendance", HandlerId::RsvpAttendance),
        ("lfg", HandlerId::Lfg),
        ("lfg-close", HandlerId::LfgClose),
        ("feed-add", HandlerId::FeedAdd),
        ("feed-remove", HandlerId::FeedRemove),
        ("feed-list", HandlerId::FeedList),
    ];
    assert_eq!(cases.len(), 27, "all 27 builtins covered");
    for (_, id) in cases {
        router.register(Box::new(Stub(*id)));
    }
    for (name, id) in cases {
        let SlashOutcome::Handled { handler } = slash_outcome(&router, name) else {
            panic!("/{name} must route to its handler");
        };
        assert_eq!(&handler, id, "/{name} routes to its handler");
        assert!(
            router
                .handler_for(&handler)
                .is_some_and(|h| h.id() == handler),
            "/{name} has its registered stub",
        );
    }
}

#[test]
fn rsvp_attendance_namespacing_holds_on_the_wire() {
    // Parity §1 #12 vs #25: `attendance` (scorecard) and `rsvp-attendance`
    // (RSVP totals) are different names with different owners.
    let router = InteractionRouter::new(all_on());
    assert_eq!(
        slash_outcome(&router, "attendance"),
        SlashOutcome::Handled {
            handler: HandlerId::ScorecardAttendance
        }
    );
    // RSVP totals are open to everyone: no permission bits at all.
    let open = slash("rsvp-attendance", Some(0));
    assert_eq!(
        route_interaction(&router, &open, None),
        RoutedInteraction::Slash {
            name: "rsvp-attendance".to_owned(),
            outcome: SlashOutcome::Handled {
                handler: HandlerId::RsvpAttendance
            },
        }
    );
}

#[test]
fn disabled_and_ungated_wire_interactions_take_the_refusal_path() {
    let off = InteractionRouter::new(RouterGates {
        scorecard: false,
        automations: false,
        announcements: false,
        moderation: false,
        ..all_on()
    });
    for (name, refusal, text) in [
        (
            "attendance",
            RouterRefusal::ScorecardDisabled,
            SCORECARD_DISABLED_REPLY,
        ),
        (
            "command",
            RouterRefusal::AutomationsDisabled,
            AUTOMATIONS_DISABLED_REPLY,
        ),
        (
            "rsvp",
            RouterRefusal::AnnouncementsDisabled,
            ANNOUNCEMENTS_DISABLED_REPLY,
        ),
        (
            "rsvp-attendance",
            RouterRefusal::AnnouncementsDisabled,
            ANNOUNCEMENTS_DISABLED_REPLY,
        ),
        (
            "ban",
            RouterRefusal::ModerationDisabled,
            MODERATION_DISABLED_REPLY,
        ),
    ] {
        let outcome = slash_outcome(&off, name);
        assert_eq!(outcome, SlashOutcome::Refuse { refusal }, "/{name} refuses");
        // The refusal path produces an ephemeral reply with the refusal text;
        // handled/ignored outcomes produce nothing from the router itself.
        let response = response_for_slash(&outcome).expect("refusal answers");
        let json = serde_json::to_value(&response).expect("serializes");
        assert_eq!(json["data"]["content"], text);
        assert_eq!(json["data"]["flags"], 64, "ephemeral");
    }
    // No-permission wire interaction hits the handler-level gate.
    let router = InteractionRouter::new(all_on());
    let bare = slash("command", Some(0));
    assert_eq!(
        route_interaction(&router, &bare, None),
        RoutedInteraction::Slash {
            name: "command".to_owned(),
            outcome: SlashOutcome::Refuse {
                refusal: RouterRefusal::ManageServerRequired
            },
        }
    );
    let response = refusal_response(RouterRefusal::ManageServerRequired);
    let json = serde_json::to_value(&response).expect("serializes");
    assert_eq!(json["data"]["content"], MANAGE_SERVER_REQUIRED);

    let lfg = slash("lfg", Some(0));
    assert_eq!(
        route_interaction(&router, &lfg, None),
        RoutedInteraction::Slash {
            name: "lfg".to_owned(),
            outcome: SlashOutcome::Refuse {
                refusal: RouterRefusal::ManageEventsRequired
            },
        }
    );
    let json = serde_json::to_value(refusal_response(RouterRefusal::ManageEventsRequired))
        .expect("serializes");
    assert_eq!(json["data"]["content"], MANAGE_EVENTS_REQUIRED);

    // Handled and ignored outcomes owe nothing.
    assert_eq!(
        response_for_slash(&SlashOutcome::Handled {
            handler: HandlerId::Rank
        }),
        None
    );
    assert_eq!(response_for_slash(&SlashOutcome::Ignore), None);
    // Moderation's guild fence answers, not silence.
    let json =
        serde_json::to_value(refusal_response(RouterRefusal::GuildRestricted)).expect("serializes");
    assert_eq!(json["data"]["content"], GUILD_RESTRICTED_REPLY);
}

#[test]
fn custom_rows_and_unknown_names_follow_legacy_fallthrough() {
    let router = InteractionRouter::new(all_on());
    let custom = slash("faq", Some(0));
    // Enabled row: everyone, no permission gate.
    assert_eq!(
        route_interaction(&router, &custom, Some(true)),
        RoutedInteraction::Slash {
            name: "faq".to_owned(),
            outcome: SlashOutcome::Handled {
                handler: HandlerId::AutomationCustom
            },
        }
    );
    // Disabled row / no row: silence — another app's command.
    assert_eq!(
        route_interaction(&router, &custom, Some(false)),
        RoutedInteraction::Slash {
            name: "faq".to_owned(),
            outcome: SlashOutcome::Ignore,
        }
    );
    assert_eq!(
        route_interaction(&router, &custom, None),
        RoutedInteraction::Slash {
            name: "faq".to_owned(),
            outcome: SlashOutcome::Ignore,
        }
    );
    // Unknown builtins (incl. the dropped rota command): silence.
    for name in ["rota-acknowledge", "definitely-not-a-command"] {
        assert_eq!(
            slash_outcome(&router, name),
            SlashOutcome::Ignore,
            "/{name} is not ours"
        );
    }
}

#[test]
fn component_ids_and_prefixes_route() {
    let router = InteractionRouter::new(all_on());
    for (id, handler) in [
        (GAME_SELECT_ID, ComponentHandler::GamePicker),
        (SESSION_SELECT_ID, ComponentHandler::SessionPicker),
        (TICKET_OPEN_ID, ComponentHandler::Tickets),
        (TICKET_CLAIM_ID, ComponentHandler::Tickets),
        (TICKET_CLOSE_ID, ComponentHandler::Tickets),
    ] {
        let interaction = component(id, vec!["valorant".to_owned()]);
        assert_eq!(
            route_interaction(&router, &interaction, None),
            RoutedInteraction::Component {
                custom_id: id.to_owned(),
                values: vec!["valorant".to_owned()],
                outcome: ComponentOutcome::Handled { handler },
            },
            "{id} routes",
        );
    }
    for id in [
        &format!("{LFG_PREFIX}abc123"),
        &format!("{SELF_ROLE_PREFIX}games"),
        &format!("{SELF_ROLE_PREFIX}games:valorant"),
    ] {
        let RoutedInteraction::Component { outcome, .. } =
            route_interaction(&router, &component(id, Vec::new()), None)
        else {
            panic!("{id} must route as a component");
        };
        assert!(
            matches!(outcome, ComponentOutcome::Handled { .. }),
            "{id} routes by prefix",
        );
    }
    // Unknown ids and disabled surfaces: silence.
    assert!(matches!(
        route_interaction(&router, &component("two:unknown:thing", Vec::new()), None),
        RoutedInteraction::Component {
            outcome: ComponentOutcome::Ignore,
            ..
        }
    ));
    let off = InteractionRouter::new(RouterGates {
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
        announcements: false,
        ..all_on()
    });
    for id in [
        GAME_SELECT_ID,
        SESSION_SELECT_ID,
        TICKET_OPEN_ID,
        &format!("{LFG_PREFIX}x"),
        &format!("{SELF_ROLE_PREFIX}p"),
    ] {
        assert!(
            matches!(
                route_interaction(&off, &component(id, Vec::new()), None),
                RoutedInteraction::Component {
                    outcome: ComponentOutcome::Ignore,
                    ..
                }
            ),
            "{id} ignored while disabled",
        );
    }
}

#[test]
fn modal_submits_share_the_component_table() {
    let router = InteractionRouter::new(all_on());
    // Legacy has no modals; known ids route for future slices, unknown stay silent.
    assert_eq!(
        route_interaction(&router, &modal(&format!("{LFG_PREFIX}abc123")), None),
        RoutedInteraction::Modal {
            custom_id: format!("{LFG_PREFIX}abc123"),
            outcome: ComponentOutcome::Handled {
                handler: ComponentHandler::LfgSignup
            },
        }
    );
    assert!(matches!(
        route_interaction(&router, &modal("two:unknown:thing"), None),
        RoutedInteraction::Modal {
            outcome: ComponentOutcome::Ignore,
            ..
        }
    ));
}

#[test]
fn interaction_create_event_carries_routable_data() {
    // The gateway delivers interactions boxed in the event; the payload
    // survives the trip with name + guild intact.
    let interaction = slash("rank", Some(0));
    let event = twilight_model::gateway::event::Event::InteractionCreate(Box::new(
        InteractionCreate(interaction),
    ));
    let twilight_model::gateway::event::Event::InteractionCreate(boxed) = event else {
        panic!("event wraps the interaction");
    };
    let router = InteractionRouter::new(all_on());
    assert_eq!(
        route_interaction(&router, &boxed, None),
        RoutedInteraction::Slash {
            name: "rank".to_owned(),
            outcome: SlashOutcome::Handled {
                handler: HandlerId::Rank
            },
        }
    );
}

#[test]
fn twilight_publish_shape_matches_the_registry_wire_shape() {
    // One definition converts losslessly: name/options casing, permission
    // bits as a decimal string, guild-only throughout.
    let router = InteractionRouter::new(all_on());
    for def in router
        .publish_set(&[])
        .expect("full set assembles")
        .iter()
        .take(3)
    {
        let command = command_to_twilight(def);
        let json = serde_json::to_value(&command).expect("serializes");
        assert_eq!(json["name"], def.name);
        assert_eq!(json["description"], def.description);
        assert_eq!(json["type"], 1, "chat-input");
        assert!(
            json.get("dm_permission").is_none(),
            "deprecated field unset"
        );
        match &def.default_member_permissions {
            Some(bits) => assert_eq!(json["default_member_permissions"], bits.as_str()),
            // Twilight serializes `None` as `null` (no skip attribute);
            // Discord reads that as "everyone".
            None => assert!(json["default_member_permissions"].is_null()),
        }
    }
    let ban = router
        .publish_set(&[])
        .expect("assembles")
        .into_iter()
        .find(|c| c.name == "ban")
        .expect("ban publishes");
    let json = serde_json::to_value(command_to_twilight(&ban)).expect("serializes");
    assert_eq!(json["default_member_permissions"], "4");
    assert_eq!(json["options"][0]["type"], 6);
    assert_eq!(json["options"][1]["max_length"], 512);
}

#[tokio::test]
async fn publish_sends_the_complete_merged_set_once() {
    // Publish-once against the mock double through the real bulk-set
    // endpoint: one request, all 28 names, no partial view.
    let router = InteractionRouter::new(all_on());
    let defs = router
        .publish_set(&[CustomCommand {
            name: "faq".to_owned(),
            description: "FAQ".to_owned(),
            enabled: true,
        }])
        .expect("full set assembles");
    assert_eq!(
        defs.len(),
        28,
        "2 core + 16 slice-2 + 9 moderation + 1 custom"
    );
    let commands = publish_commands(&defs);

    let mock = MockDiscord::start().await;
    let proxy_host = format!("{}:{}", mock.http_addr.ip(), mock.http_addr.port());
    let http = twilight_http::Client::builder()
        .token("routing-test-token".to_owned())
        .proxy(proxy_host, true)
        .build();

    http.interaction(Id::new(APP_ID))
        .set_guild_commands(Id::new(GUILD_ID), &commands)
        .await
        .expect("bulk set posts");

    let sets: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|r| {
            r.method == "PUT"
                && r.path == format!("/api/v10/applications/{APP_ID}/guilds/{GUILD_ID}/commands")
        })
        .collect();
    assert_eq!(sets.len(), 1, "published exactly once: {sets:?}");
    let body: serde_json::Value = serde_json::from_slice(&sets[0].body).expect("bulk body parses");
    let names: Vec<&str> = body
        .as_array()
        .expect("bulk body is an array")
        .iter()
        .map(|c| c["name"].as_str().expect("command has a name"))
        .collect();
    assert_eq!(names.len(), 28, "no partial view");
    for expected in [
        "rank",
        "leaderboard",
        "attendance",
        "command",
        "schedule-list",
        "sticky-remove",
        "rsvp",
        "rsvp-attendance",
        "lfg",
        "lfg-close",
        "feed-add",
        "feed-list",
        "ban",
        "purge",
        "lockdown",
        "unlock",
        "faq",
    ] {
        assert!(names.contains(&expected), "{expected} published");
    }
    assert_eq!(
        names.iter().filter(|n| **n == "attendance").count(),
        1,
        "rsvp-totals namespacing holds on the wire",
    );

    mock.shutdown().await;
}
