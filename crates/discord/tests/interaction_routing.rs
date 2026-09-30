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
    // Disabled/missing rows and unknown names get the uniform private reply.
    for row in [Some(false), None] {
        assert_eq!(
            route_interaction(&router, &custom, row),
            RoutedInteraction::Slash {
                name: "faq".to_owned(),
                outcome: SlashOutcome::Unknown,
            }
        );
    }
    for name in ["rota-acknowledge", "definitely-not-a-command"] {
        assert_eq!(slash_outcome(&router, name), SlashOutcome::Unknown);
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
    // Unknown ids reply; disabled surfaces remain fenced.
    assert!(matches!(
        route_interaction(&router, &component("two:unknown:thing", Vec::new()), None),
        RoutedInteraction::Component {
            outcome: ComponentOutcome::Unknown,
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
            outcome: ComponentOutcome::Unknown,
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
async fn slow_dispatch_sends_defer_then_original_edit_on_the_wire() {
    use common::{MockRest, ScriptedResponse};
    use std::time::Duration;
    use two_bot_core::router::replies::{InteractionReply, ReplyPolicy};
    use two_bot_discord::{
        dispatch_interaction, ActionExecutor, DispatchOptions, InteractionReplyTransport,
    };
    for ephemeral in [true, false] {
        let mock = MockRest::start(Vec::new(), ScriptedResponse::status(204)).await;
        let executor =
            ActionExecutor::with_proxy("reply-test-token".into(), Some(mock.origin())).unwrap();
        let interaction = slash("rank", Some(0));
        let transport = InteractionReplyTransport::new(&executor, &interaction);
        let options = DispatchOptions {
            ephemeral,
            reply_policy: ReplyPolicy::new(Duration::from_millis(10)).unwrap(),
            ..Default::default()
        };
        dispatch_interaction(
            &InteractionRouter::new(all_on()),
            &interaction,
            &transport,
            options,
            |_, _| async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok::<_, &str>(InteractionReply::new("@everyone completed", ephemeral))
            },
        )
        .await
        .unwrap();
        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(
            requests[0].path,
            "/api/v10/interactions/7/routing-test-token/callback"
        );
        let ack: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(ack["type"], 5);
        assert_eq!(ack["data"]["flags"], if ephemeral { 64 } else { 0 });
        assert!(ack["data"].get("content").is_none());
        assert_eq!(requests[1].method, "PATCH");
        assert_eq!(
            requests[1].path.replace("%40", "@"),
            "/api/v10/webhooks/1111/routing-test-token/messages/@original"
        );
        let edit: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(edit["content"], "@everyone completed");
        assert_eq!(edit["allowed_mentions"]["parse"], serde_json::json!([]));
        assert!(
            edit.get("flags").is_none(),
            "visibility cannot change on an edit"
        );
        mock.shutdown().await;
    }
}

#[tokio::test]
async fn unknown_ids_names_and_refusals_reply_without_invoking_a_handler() {
    use common::{MockRest, ScriptedResponse};
    use two_bot_core::router::replies::{InteractionReply, UNKNOWN_INTERACTION_REPLY};
    use two_bot_discord::{
        dispatch_interaction, ActionExecutor, DispatchOptions, InteractionReplyTransport,
    };
    let mock = MockRest::start(Vec::new(), ScriptedResponse::status(204)).await;
    let executor =
        ActionExecutor::with_proxy("reply-test-token".into(), Some(mock.origin())).unwrap();
    let router = InteractionRouter::new(all_on());
    for interaction in [
        slash("missing", Some(0)),
        component("two:unknown", Vec::new()),
        modal("two:unknown"),
        slash("command", Some(0)),
    ] {
        let transport = InteractionReplyTransport::new(&executor, &interaction);
        assert!(dispatch_interaction(
            &router,
            &interaction,
            &transport,
            DispatchOptions::default(),
            |_, _| async {
                panic!("unknown/refused interactions must not execute a handler");
                #[allow(unreachable_code)]
                Ok::<InteractionReply, &str>(InteractionReply::new("wrong", false))
            }
        )
        .await
        .unwrap());
    }
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    for (i, request) in requests.iter().enumerate() {
        assert_eq!(request.method, "POST");
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["type"], 4);
        assert_eq!(body["data"]["flags"], 64);
        assert_eq!(
            body["data"]["content"],
            if i == 3 {
                MANAGE_SERVER_REQUIRED
            } else {
                UNKNOWN_INTERACTION_REPLY
            }
        );
    }
    let mut foreign = component("two:unknown", Vec::new());
    foreign.guild_id = Some(Id::new(9999));
    let transport = InteractionReplyTransport::new(&executor, &foreign);
    assert!(!dispatch_interaction(
        &router,
        &foreign,
        &transport,
        DispatchOptions::default(),
        |_, _| async {
            panic!("foreign guild must not execute a handler");
            #[allow(unreachable_code)]
            Ok::<InteractionReply, &str>(InteractionReply::new("wrong", false))
        }
    )
    .await
    .unwrap());
    assert_eq!(mock.requests().len(), 4);
    mock.shutdown().await;
}

#[tokio::test]
async fn public_deferred_error_is_deleted_then_sent_as_private_followup() {
    use common::{MockRest, ScriptedResponse};
    use std::time::Duration;
    use two_bot_core::router::replies::{InteractionReply, ReplyPolicy};
    use two_bot_discord::{
        dispatch_interaction, ActionExecutor, DispatchOptions, InteractionReplyTransport,
    };
    let mock = MockRest::start(Vec::new(), ScriptedResponse::status(204)).await;
    let executor =
        ActionExecutor::with_proxy("reply-test-token".into(), Some(mock.origin())).unwrap();
    let interaction = slash("rank", Some(0));
    let transport = InteractionReplyTransport::new(&executor, &interaction);
    dispatch_interaction(
        &InteractionRouter::new(all_on()),
        &interaction,
        &transport,
        DispatchOptions {
            reply_policy: ReplyPolicy::new(Duration::from_millis(10)).unwrap(),
            ..Default::default()
        },
        |_, _| async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Err::<InteractionReply, _>("internal database error")
        },
    )
    .await
    .unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].method, "DELETE");
    assert!(requests[1]
        .path
        .replace("%40", "@")
        .ends_with("/messages/@original"));
    assert_eq!(requests[2].method, "POST");
    assert_eq!(
        requests[2].path,
        "/api/v10/webhooks/1111/routing-test-token"
    );
    let body: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(body["flags"], 64);
    assert!(body["content"]
        .as_str()
        .unwrap()
        .starts_with("Something went wrong (ref "));
    assert!(!body.to_string().contains("database"));
    mock.shutdown().await;
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
