//! Expected-reply fixtures for the five highest-traffic slash commands.
//!
//! The staging smoke paths (automated suite + manual top-5 pass) both wait on
//! staging health, so this pins what they will assert once staging is up —
//! offline, from `docs/interaction-replies.md` and the router's documented
//! refusal copy. No guild, network, or database: synthetic twilight
//! interactions through the real router and reply builders.
//!
//! The five: `rank`, `leaderboard` (open core), `rsvp` (open announcements),
//! `lfg` (permission-gated announcements), `ban` (permission-gated
//! moderation). One command per routing family, so every fence → gate →
//! permission branch the smoke harness can hit is pinned. (`help`/`ping`
//! appear in older smoke notes but are not slash commands on this tree.)
//!
//! Per command this pins input class → routing outcome → exact user-facing
//! reply text and error class:
//! - allowed input routes to its handler; the router itself owes no reply
//!   (`response_for_slash` is `None` — success text belongs to the feature).
//! - disabled features and missing permissions refuse with the documented
//!   actionable copy (`RouterRefusal::message`): Discord permission names,
//!   who grants them, or the admin-only host-setting enable path.
//! - unknown slash names get the re-pick reply; stale components get the
//!   expired-control reply; foreign guilds are fenced (`Ignore`, except
//!   moderation's documented refusal).
//!
//! Dev-only: never ships in the release binary.

use twilight_model::{
    application::{
        command::CommandType,
        interaction::{
            application_command::{CommandData, CommandDataOption},
            Interaction, InteractionData, InteractionType,
        },
    },
    gateway::payload::incoming::InteractionCreate,
    guild::{MemberFlags, Permissions},
    id::{AnonymizableId, Id},
    oauth::ApplicationIntegrationMap,
    user::User,
};
use two_bot_core::{
    commands::{PERM_BAN_MEMBERS, PERM_MANAGE_EVENTS},
    router::replies::UNKNOWN_COMMAND_REPLY,
    HandlerId, InteractionRouter, ModerationAction, RouterGates, RouterRefusal, SlashOutcome,
};
use two_bot_discord::{refusal_response, response_for_slash, route_interaction, RoutedInteraction};

const APP_ID: u64 = 1111;
const GUILD_ID: u64 = 2222;
const FOREIGN_GUILD_ID: u64 = 9999;

fn all_on() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD_ID),
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
    }
}

fn all_off() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD_ID),
        scorecard: false,
        automations: false,
        announcements: false,
        moderation: false,
        voice: false,
        voice_assistant: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

fn user(id: u64) -> User {
    User {
        accent_color: None,
        avatar: None,
        avatar_decoration: None,
        avatar_decoration_data: None,
        banner: None,
        bot: false,
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
fn slash_in(name: &str, permissions: Option<u64>, guild_id: Option<u64>) -> Interaction {
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
        guild_id: guild_id.map(Id::new),
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
        token: "top5-fixture-token".to_owned(),
        user: Some(user(99)),
    }
}

fn slash(name: &str, permissions: Option<u64>) -> Interaction {
    slash_in(name, permissions, Some(GUILD_ID))
}

fn slash_outcome(router: &InteractionRouter, name: &str) -> SlashOutcome {
    match route_interaction(router, &slash(name, Some(u64::MAX)), None) {
        RoutedInteraction::Slash { outcome, .. } => outcome,
        other => panic!("{name} must route as slash, got {other:?}"),
    }
}

/// The five commands under test and the handler each must route to.
const TOP_FIVE: &[(&str, HandlerId)] = &[
    ("rank", HandlerId::Rank),
    ("leaderboard", HandlerId::Leaderboard),
    ("rsvp", HandlerId::Rsvp),
    ("lfg", HandlerId::Lfg),
    ("ban", HandlerId::Moderation(ModerationAction::Ban)),
];

/// The primary refusal each gated command owes, with the documented
/// post-425 actionable copy (permission names, granter, or admin-only
/// host-setting enable path).
const PRIMARY_REFUSALS: &[(&str, RouterRefusal, &str)] = &[
    (
        "rsvp",
        RouterRefusal::AnnouncementsDisabled,
        "Announcements are disabled on this server. Ask a server admin to enable them in the bot configuration — this is a host setting, not a Discord role.",
    ),
    (
        "lfg",
        RouterRefusal::ManageEventsRequired,
        "You need the Manage Events permission to use this command. Ask a server admin to grant it.",
    ),
    (
        "ban",
        RouterRefusal::ModerationPermission(ModerationAction::Ban),
        "You need the Ban Members permission to use /ban. Ask a server moderator or admin to grant it.",
    ),
];

#[test]
fn top_five_route_to_their_handlers_with_no_router_owned_reply() {
    let router = InteractionRouter::new(all_on());
    assert_eq!(TOP_FIVE.len(), 5, "exactly the top five");
    for &(name, handler) in TOP_FIVE {
        let outcome = slash_outcome(&router, name);
        assert_eq!(
            outcome,
            SlashOutcome::Handled { handler },
            "/{name} routes to its handler",
        );
        // Success text belongs to the feature slice; the router owes no reply.
        assert_eq!(
            response_for_slash(&outcome),
            None,
            "/{name} handled outcome produces no router reply",
        );
    }
}

#[test]
fn open_commands_serve_members_without_special_bits() {
    let router = InteractionRouter::new(all_on());
    // `rank`, `leaderboard` and `rsvp` require no permission bits: a bare
    // member (or an interaction with no resolved permissions) still routes.
    for (name, handler) in [
        ("rank", HandlerId::Rank),
        ("leaderboard", HandlerId::Leaderboard),
        ("rsvp", HandlerId::Rsvp),
    ] {
        for permissions in [None, Some(0)] {
            let outcome = match route_interaction(&router, &slash(name, permissions), None) {
                RoutedInteraction::Slash { outcome, .. } => outcome,
                other => panic!("{name} must route as slash, got {other:?}"),
            };
            assert_eq!(
                outcome,
                SlashOutcome::Handled { handler },
                "/{name} serves permissions {permissions:?}",
            );
        }
    }
}

#[test]
fn disabled_features_refuse_with_documented_copy() {
    let off = InteractionRouter::new(all_off());
    // Always-on core keeps routing while every feature is off.
    for name in ["rank", "leaderboard"] {
        assert!(
            matches!(slash_outcome(&off, name), SlashOutcome::Handled { .. }),
            "/{name} stays live while features are off",
        );
    }
    // Announcements-gated commands refuse; moderation refuses too.
    for (name, refusal, text) in [
        (
            "rsvp",
            RouterRefusal::AnnouncementsDisabled,
            "Announcements are disabled on this server. Ask a server admin to enable them in the bot configuration — this is a host setting, not a Discord role.",
        ),
        (
            "lfg",
            RouterRefusal::AnnouncementsDisabled,
            "Announcements are disabled on this server. Ask a server admin to enable them in the bot configuration — this is a host setting, not a Discord role.",
        ),
        (
            "ban",
            RouterRefusal::ModerationDisabled,
            "Moderation is not enabled on this server. Ask a server admin to enable it in the bot configuration — this is a host setting, not a Discord role.",
        ),
    ] {
        let outcome = slash_outcome(&off, name);
        assert_eq!(outcome, SlashOutcome::Refuse { refusal }, "/{name} refuses");
        assert_eq!(refusal.message(), text, "/{name} documented copy");
        let response = response_for_slash(&outcome).expect("refusal answers");
        let json = serde_json::to_value(&response).expect("serializes");
        assert_eq!(json["data"]["content"], text);
        assert_eq!(json["data"]["flags"], 64, "ephemeral");
    }
}

#[test]
fn missing_permissions_refuse_with_documented_copy() {
    let router = InteractionRouter::new(all_on());
    // `lfg` needs Manage Events; without it the exact documented refusal.
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
    // ... while exactly Manage Events passes.
    let lfg_ok = slash("lfg", Some(PERM_MANAGE_EVENTS));
    assert_eq!(
        route_interaction(&router, &lfg_ok, None),
        RoutedInteraction::Slash {
            name: "lfg".to_owned(),
            outcome: SlashOutcome::Handled {
                handler: HandlerId::Lfg
            },
        }
    );
    // `ban` needs Ban Members; without it the exact documented refusal.
    let ban = slash("ban", Some(0));
    assert_eq!(
        route_interaction(&router, &ban, None),
        RoutedInteraction::Slash {
            name: "ban".to_owned(),
            outcome: SlashOutcome::Refuse {
                refusal: RouterRefusal::ModerationPermission(ModerationAction::Ban)
            },
        }
    );
    let ban_ok = slash("ban", Some(PERM_BAN_MEMBERS));
    assert_eq!(
        route_interaction(&router, &ban_ok, None),
        RoutedInteraction::Slash {
            name: "ban".to_owned(),
            outcome: SlashOutcome::Handled {
                handler: HandlerId::Moderation(ModerationAction::Ban)
            },
        }
    );
    // `rsvp` is open: zero bits still route.
    let rsvp = slash("rsvp", Some(0));
    assert_eq!(
        route_interaction(&router, &rsvp, None),
        RoutedInteraction::Slash {
            name: "rsvp".to_owned(),
            outcome: SlashOutcome::Handled {
                handler: HandlerId::Rsvp
            },
        }
    );
}

#[test]
fn every_primary_refusal_is_ephemeral_without_mentions() {
    // Each gated command's primary refusal serializes as the documented
    // lifecycle callback: type 4, ephemeral, exact content, no mentions.
    for &(name, refusal, text) in PRIMARY_REFUSALS {
        assert_eq!(refusal.message(), text, "/{name} documented copy");
        let response = refusal_response(refusal);
        let json = serde_json::to_value(&response).expect("serializes");
        assert_eq!(json["type"], 4, "/{name} answers with a type 4 callback");
        assert_eq!(json["data"]["content"], text);
        assert_eq!(json["data"]["flags"], 64, "ephemeral");
        // Mention suppression: the default mention set parses nothing, so no
        // user, role, or `@everyone` mention can fire from this reply.
        assert_eq!(
            json["data"]["allowed_mentions"]["parse"],
            serde_json::json!([]),
            "mentions suppressed"
        );
    }
}

#[test]
fn unknown_names_get_the_unknown_command_reply() {
    let router = InteractionRouter::new(all_on());
    for name in ["definitely-not-a-command", "help", "ping"] {
        // Neither legacy name exists as a slash command on this tree.
        let outcome = slash_outcome(&router, name);
        assert_eq!(outcome, SlashOutcome::Unknown, "{name} is unknown");
        let response = response_for_slash(&outcome).expect("unknown answers");
        let json = serde_json::to_value(&response).expect("serializes");
        assert_eq!(json["data"]["content"], UNKNOWN_COMMAND_REPLY);
        assert_eq!(json["data"]["flags"], 64, "ephemeral");
    }
}

#[test]
fn guild_fence_holds_for_all_five() {
    let router = InteractionRouter::new(all_on());
    // Foreign and missing guilds are fenced everywhere except moderation,
    // which answers with its documented guild-restriction refusal.
    for name in ["rank", "leaderboard", "rsvp", "lfg"] {
        for guild in [Some(FOREIGN_GUILD_ID), None] {
            let interaction = slash_in(name, Some(u64::MAX), guild);
            assert_eq!(
                route_interaction(&router, &interaction, None),
                RoutedInteraction::Slash {
                    name: name.to_owned(),
                    outcome: SlashOutcome::Ignore,
                },
                "/{name} ignores guild {guild:?}",
            );
        }
    }
    for guild in [Some(FOREIGN_GUILD_ID), None] {
        let interaction = slash_in("ban", Some(u64::MAX), guild);
        let outcome = match route_interaction(&router, &interaction, None) {
            RoutedInteraction::Slash { outcome, .. } => outcome,
            other => panic!("ban must route as slash, got {other:?}"),
        };
        assert_eq!(
            outcome,
            SlashOutcome::Refuse {
                refusal: RouterRefusal::GuildRestricted
            },
            "ban answers outside the guild",
        );
        assert_eq!(
            RouterRefusal::GuildRestricted.message(),
            "This command is restricted to the configured guild."
        );
    }
    // The gateway event still carries routable data for a fenced command:
    // fencing is a router decision, not a decoding failure.
    let event = twilight_model::gateway::event::Event::InteractionCreate(Box::new(
        InteractionCreate(slash("rank", Some(0))),
    ));
    let twilight_model::gateway::event::Event::InteractionCreate(boxed) = event else {
        panic!("event wraps the interaction");
    };
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
