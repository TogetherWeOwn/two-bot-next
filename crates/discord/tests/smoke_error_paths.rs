//! Smoke error-path fixtures: cooldown-hit, permission-denied, unknown-command.
//!
//! Offline expected-response pins for the three error paths of the core slash
//! commands, complementing the happy-path table in
//! `docs/smoke-expected-responses.md` and its harness in
//! `crates/discord/tests/top5_reply_fixtures.rs` (PR #483, under review).
//! Nothing here edits those files: this is a separate file with its own
//! rows, and it must stay that way so either side can merge independently.
//!
//! Layer this pins: the slash router (`route_interaction` /
//! `response_for_slash` / `refusal_response`). The full command runtime
//! (`CommandRuntime::on_interaction`, covered by
//! `crates/bot/src/smoke_error_contract_tests.rs`) answers unknown
//! interactions with the stale-control text instead — that is a different
//! layer with different copy, not a contradiction.
//!
//! Copy policy: every assertion compares against the owning constant or the
//! owning `message()` constructor by identity, never a transcribed literal.
//! Transcribed literals went stale against the router before (the unknown and
//! refusal texts diverged from runtime truth); identity cannot drift.
//!
//! Offline only: synthetic twilight interactions through the real router and
//! reply builders. No guild, network, or database.
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
    guild::{MemberFlags, Permissions},
    id::{AnonymizableId, Id},
    oauth::ApplicationIntegrationMap,
    user::User,
};
use two_bot_core::{
    router::{
        replies::{EXPIRED_COMPONENT_REPLY, UNKNOWN_COMMAND_REPLY},
        ANNOUNCEMENTS_DISABLED_REPLY, AUTOMATIONS_DISABLED_REPLY, GUILD_RESTRICTED_REPLY,
        MANAGE_EVENTS_REQUIRED, MANAGE_SERVER_REQUIRED, MODERATION_DISABLED_REPLY,
        SCORECARD_DISABLED_REPLY,
    },
    HandlerId, InteractionRouter, ModerationAction, RouterGates, RouterRefusal, SlashOutcome,
};
use two_bot_discord::{refusal_response, response_for_slash, route_interaction, RoutedInteraction};

const APP_ID: u64 = 3333;
const GUILD_ID: u64 = 4444;

fn all_on() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD_ID),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice: true,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
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
        token: "smoke-error-path-token".to_owned(),
        user: Some(user(99)),
    }
}

/// Every refusal the router can emit, paired with the constant that owns its
/// user-visible copy. Fixed variants pair with their module constant; each
/// moderation verb pairs with the template its `message()` builds from the
/// verb's own Discord permission name and slash-command name.
fn all_refusals() -> Vec<(RouterRefusal, String)> {
    let mut rows = vec![
        (
            RouterRefusal::AutomationsDisabled,
            AUTOMATIONS_DISABLED_REPLY.to_owned(),
        ),
        (
            RouterRefusal::AnnouncementsDisabled,
            ANNOUNCEMENTS_DISABLED_REPLY.to_owned(),
        ),
        (
            RouterRefusal::ModerationDisabled,
            MODERATION_DISABLED_REPLY.to_owned(),
        ),
        (
            RouterRefusal::ScorecardDisabled,
            SCORECARD_DISABLED_REPLY.to_owned(),
        ),
        (
            RouterRefusal::ManageServerRequired,
            MANAGE_SERVER_REQUIRED.to_owned(),
        ),
        (
            RouterRefusal::ManageEventsRequired,
            MANAGE_EVENTS_REQUIRED.to_owned(),
        ),
        (
            RouterRefusal::GuildRestricted,
            GUILD_RESTRICTED_REPLY.to_owned(),
        ),
    ];
    for action in ModerationAction::ALL {
        rows.push((
            RouterRefusal::ModerationPermission(action),
            format!(
                "You need the {} permission to use /{}. Ask a server moderator or admin to grant it.",
                action.discord_permission_name(),
                action.command_name()
            ),
        ));
    }
    rows
}

/// Exhaustive refusal names. This match must stay exhaustive: adding a new
/// `RouterRefusal` variant (for example a future cooldown refusal) fails to
/// compile here, which is the signal to pin its copy in this file.
fn refusal_name(refusal: RouterRefusal) -> &'static str {
    match refusal {
        RouterRefusal::AutomationsDisabled => "automations-disabled",
        RouterRefusal::AnnouncementsDisabled => "announcements-disabled",
        RouterRefusal::ModerationDisabled => "moderation-disabled",
        RouterRefusal::ScorecardDisabled => "scorecard-disabled",
        RouterRefusal::ManageServerRequired => "manage-server-required",
        RouterRefusal::ManageEventsRequired => "manage-events-required",
        RouterRefusal::GuildRestricted => "guild-restricted",
        RouterRefusal::ModerationPermission(_) => "moderation-permission",
    }
}

#[test]
fn unknown_slash_answers_with_the_router_owned_unknown_copy() {
    let router = InteractionRouter::new(all_on());
    // Names distinct from the happy-path harness: removed or renamed
    // commands, the case the copy describes.
    for name in ["removed-command", "renamed-command"] {
        let outcome = match route_interaction(&router, &slash(name, Some(u64::MAX)), None) {
            RoutedInteraction::Slash { outcome, .. } => outcome,
            other => panic!("{name} must route as slash, got {other:?}"),
        };
        assert_eq!(outcome, SlashOutcome::Unknown, "{name} is unknown");
        let response = response_for_slash(&outcome).expect("unknown answers");
        let json = serde_json::to_value(&response).expect("serializes");
        assert_eq!(json["type"], 4, "unknown answers with a type 4 callback");
        // Router truth: slash-unknown sends the unknown-command text, not the
        // expired-control text (that one belongs to stale buttons and menus).
        assert_eq!(json["data"]["content"], UNKNOWN_COMMAND_REPLY);
        assert_ne!(
            UNKNOWN_COMMAND_REPLY, EXPIRED_COMPONENT_REPLY,
            "slash-unknown and stale-control copies stay distinct"
        );
        assert_eq!(json["data"]["flags"], 64, "ephemeral");
        assert_eq!(
            json["data"]["allowed_mentions"]["parse"],
            serde_json::json!([]),
            "mentions suppressed"
        );
    }
}

#[test]
fn every_refusal_matches_its_owning_copy() {
    let rows = all_refusals();
    // Seven fixed refusals plus one row per moderation verb (nine verbs).
    assert_eq!(rows.len(), 7 + ModerationAction::ALL.len());
    let mut seen = std::collections::BTreeSet::new();
    for (refusal, expected) in &rows {
        assert_eq!(
            refusal.message(),
            *expected,
            "{} sends its owning copy",
            refusal_name(*refusal)
        );
        assert!(
            seen.insert(expected.clone()),
            "refusal copies stay distinct: {expected}"
        );
    }
}

#[test]
fn every_refusal_serializes_ephemeral_without_mentions() {
    for (refusal, _) in all_refusals() {
        let text = refusal.message();
        let response = refusal_response(refusal);
        let json = serde_json::to_value(&response).expect("serializes");
        assert_eq!(
            json["type"],
            4,
            "{} answers with a type 4 callback",
            refusal_name(refusal)
        );
        assert_eq!(json["data"]["content"], text);
        assert_eq!(json["data"]["flags"], 64, "ephemeral");
        assert_eq!(
            json["data"]["allowed_mentions"]["parse"],
            serde_json::json!([]),
            "mentions suppressed"
        );
    }
}

#[test]
fn open_core_commands_have_no_permission_denial() {
    // `rank`, `leaderboard` and `rsvp` require no permission bits, so no
    // permission-denied copy exists for them: bare members, zero-bit members
    // and fully-privileged members all route to the handler. Their only
    // error paths are the guild fence (silence) and unknown names.
    let router = InteractionRouter::new(all_on());
    for (name, handler) in [
        ("rank", HandlerId::Rank),
        ("leaderboard", HandlerId::Leaderboard),
        ("rsvp", HandlerId::Rsvp),
    ] {
        for permissions in [None, Some(0), Some(u64::MAX)] {
            match route_interaction(&router, &slash(name, permissions), None) {
                RoutedInteraction::Slash { outcome, .. } => assert_eq!(
                    outcome,
                    SlashOutcome::Handled { handler },
                    "/{name} never denies permissions {permissions:?}",
                ),
                other => panic!("{name} must route as slash, got {other:?}"),
            }
        }
    }
}

#[test]
fn repeat_invocations_are_never_cooldown_refused() {
    // The slash router holds no cooldown state and owns no cooldown refusal:
    // the same command routed three times in a row is handled three times.
    // Cooldowns live elsewhere and stay silent at this layer — the XP award
    // gate (`AWARD_COOLDOWN_SECONDS` in `crates/core/src/leveling.rs`) drops
    // the award without a reply, and send-admission / ratelimit governors
    // hold transport, never user copy. Voice vote re-raise cooldowns belong
    // to the room-command slices, out of scope here.
    let router = InteractionRouter::new(all_on());
    for _ in 0..3 {
        match route_interaction(&router, &slash("rank", Some(u64::MAX)), None) {
            RoutedInteraction::Slash { outcome, .. } => assert_eq!(
                outcome,
                SlashOutcome::Handled {
                    handler: HandlerId::Rank
                },
                "repeat invocation is handled, never cooldown-refused",
            ),
            other => panic!("rank must route as slash, got {other:?}"),
        }
    }
    // The exhaustive match inside `refusal_name` is the tripwire: a future
    // cooldown variant breaks compilation until its copy is pinned here.
    assert_eq!(
        refusal_name(RouterRefusal::ManageEventsRequired),
        "manage-events-required"
    );
}
