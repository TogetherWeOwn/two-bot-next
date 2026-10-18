//! Denied-path probes for the three gated smoke commands.
//!
//! Offline pins for run-record rows 6–8 (`docs/smoke-run-record.md`): `/rsvp`
//! with the announcements gate off, `/lfg` without Manage Events (plus the
//! gate-off variant), and `/ban` without Ban Members (plus the moderation
//! gate-off variant). Each probe asserts the two properties the live pass
//! needs:
//!
//! - read-only: the outcome is `Refuse`, never `Handled`, so no feature
//!   handler runs and no store or REST effect can follow; the reply is the
//!   immediate ephemeral type-4 refusal, not a deferred handler callback.
//! - gate-safe: every probe builds its own isolated router, and the always-on
//!   `/rank` still routes on the gated-off routers, so the probes neither
//!   mutate shared gate state nor break the live pass.
//!
//! Layer this pins: the slash router decision (`route_slash`) plus the reply
//! shape (`response_for_slash`). The full wire path (synthetic twilight
//! interactions through `route_interaction`) is pinned alongside the router in
//! `crates/discord/tests/top5_reply_fixtures.rs`; both layers share the same
//! `response_for_slash` constructor, so the ephemeral type-4 shape asserted
//! here is the shape the wire serves.
//!
//! Copy policy: every assertion compares against the owning constant or the
//! owning `message()` constructor by identity, never a transcribed literal.
//!
//! Offline only: pure router calls, no interaction decoding, no guild,
//! network, or database.
//!
//! Dev-only: never ships in the release binary.

use two_bot_core::{
    router::{ANNOUNCEMENTS_DISABLED_REPLY, MANAGE_EVENTS_REQUIRED, MODERATION_DISABLED_REPLY},
    HandlerId, InteractionRouter, ModerationAction, RouterGates, RouterRefusal, SlashContext,
    SlashOutcome,
};
use two_bot_discord::response_for_slash;

const GUILD_ID: u64 = 6666;

fn gates_with(announcements: bool, moderation: bool) -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD_ID),
        scorecard: true,
        automations: true,
        announcements,
        moderation,
        voice: false,
        voice_assistant: false,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    }
}

fn slash_outcome(router: &InteractionRouter, name: &str, permissions: Option<u64>) -> SlashOutcome {
    router.route_slash(&SlashContext {
        name,
        guild_id: Some(GUILD_ID),
        actor_permissions: permissions,
        custom_row: None,
    })
}

/// Refused outcomes answer at once: an ephemeral type-4 callback carrying the
/// refusal's own copy with mentions suppressed. Never `Handled`, so no
/// handler runs and nothing downstream can write.
fn assert_immediate_read_only_refusal(outcome: &SlashOutcome, refusal: RouterRefusal) {
    assert_eq!(
        *outcome,
        SlashOutcome::Refuse { refusal },
        "denied probe refuses instead of handling"
    );
    let response = response_for_slash(outcome).expect("refusal answers immediately");
    let json = serde_json::to_value(&response).expect("serializes");
    assert_eq!(
        json["type"], 4,
        "refusal is an immediate callback, not a defer"
    );
    assert_eq!(json["data"]["content"], refusal.message());
    assert_eq!(json["data"]["flags"], 64, "ephemeral");
    assert_eq!(
        json["data"]["allowed_mentions"]["parse"],
        serde_json::json!([]),
        "mentions suppressed"
    );
}

/// The always-on `/rank` still routes on a gated-off router: probing the
/// denied paths leaves the live pass intact.
fn assert_live_pass_intact(router: &InteractionRouter) {
    assert_eq!(
        slash_outcome(router, "rank", Some(u64::MAX)),
        SlashOutcome::Handled {
            handler: HandlerId::Rank
        },
        "/rank stays live while gates are off",
    );
}

#[test]
fn row6_rsvp_gate_off_refuses_read_only() {
    // Run-record row 6: announcements gate off. `/rsvp` is open to everyone,
    // so the gate is its only denied path.
    let router = InteractionRouter::new(gates_with(false, true));
    let outcome = slash_outcome(&router, "rsvp", Some(u64::MAX));
    assert_eq!(
        outcome,
        SlashOutcome::Refuse {
            refusal: RouterRefusal::AnnouncementsDisabled
        },
        "/rsvp refuses with the gate off",
    );
    assert_eq!(
        RouterRefusal::AnnouncementsDisabled.message(),
        ANNOUNCEMENTS_DISABLED_REPLY,
        "gate-off copy is the owning constant",
    );
    assert_immediate_read_only_refusal(&outcome, RouterRefusal::AnnouncementsDisabled);
    assert_live_pass_intact(&router);
}

#[test]
fn row7_lfg_denied_paths_refuse_read_only() {
    // Run-record row 7, probe A: member without Manage Events, gates on.
    let router = InteractionRouter::new(gates_with(true, true));
    let outcome = slash_outcome(&router, "lfg", Some(0));
    assert_eq!(
        outcome,
        SlashOutcome::Refuse {
            refusal: RouterRefusal::ManageEventsRequired
        },
        "/lfg refuses without Manage Events",
    );
    assert_eq!(
        RouterRefusal::ManageEventsRequired.message(),
        MANAGE_EVENTS_REQUIRED,
        "permission copy is the owning constant",
    );
    assert_immediate_read_only_refusal(&outcome, RouterRefusal::ManageEventsRequired);
    assert_live_pass_intact(&router);

    // Run-record row 7, probe B: announcements gate off. The gate check runs
    // before permissions, so even a fully-privileged member is refused.
    let gated_off = InteractionRouter::new(gates_with(false, true));
    let outcome = slash_outcome(&gated_off, "lfg", Some(u64::MAX));
    assert_immediate_read_only_refusal(&outcome, RouterRefusal::AnnouncementsDisabled);
    assert_live_pass_intact(&gated_off);
}

#[test]
fn row8_ban_denied_paths_refuse_read_only() {
    // Run-record row 8, probe A: member without Ban Members, gates on.
    let router = InteractionRouter::new(gates_with(true, true));
    let refusal = RouterRefusal::ModerationPermission(ModerationAction::Ban);
    let outcome = slash_outcome(&router, "ban", Some(0));
    assert_eq!(
        outcome,
        SlashOutcome::Refuse { refusal },
        "/ban refuses without Ban Members",
    );
    assert_immediate_read_only_refusal(&outcome, refusal);
    assert_live_pass_intact(&router);

    // Run-record row 8, probe B: moderation gate off. The gate check runs
    // before permissions, so even a fully-privileged member is refused.
    let gated_off = InteractionRouter::new(gates_with(true, false));
    let outcome = slash_outcome(&gated_off, "ban", Some(u64::MAX));
    assert_eq!(
        RouterRefusal::ModerationDisabled.message(),
        MODERATION_DISABLED_REPLY,
        "gate-off copy is the owning constant",
    );
    assert_immediate_read_only_refusal(&outcome, RouterRefusal::ModerationDisabled);
    assert_live_pass_intact(&gated_off);
}
