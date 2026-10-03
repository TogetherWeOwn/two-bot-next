//! Router voice custom-id prefix non-collision acceptance (TOG-12628).
//!
//! Tests-only slice against the existing public APIs
//! (`InteractionRouter::route_component` / `route_modal`,
//! `join_custom_id` / `kick_custom_id` / `name_*_custom_id`,
//! `parse_voice_custom_id`); no `src` changes. Pins the wired-but-undispatched
//! seam: `voice_custom_id` mints `two:voice:` ids, but the router has no voice
//! branch, so every voice shape falls through to `Unknown` while `two:lfg:`
//! and `two:self-role:` ids keep routing to their own handlers. See
//! `docs/router-voice-prefix.md`; the future V-runtime slice adds the branch.

use two_bot_core::router::{ComponentHandler, ComponentOutcome, InteractionRouter, RouterGates};
use two_bot_core::voice_custom_id::{
    join_custom_id, kick_custom_id, name_custom_custom_id, name_modal_custom_id,
    name_restore_custom_id, parse_voice_custom_id, MAX_VOICE_CUSTOM_ID_CHARS,
};
use two_bot_core::voice_private::JoinDecision;
use two_bot_core::voice_vote_kick::VoteBallot;

const GUILD: u64 = 2222;

fn all_on() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
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

fn all_off() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        scorecard: false,
        automations: false,
        announcements: false,
        moderation: false,
        voice: false,
        tickets: false,
        self_roles: false,
        onboarding_picker: false,
        session_picker: false,
    }
}

/// Every currently minted voice shape, straight from the encoders.
fn minted_voice_ids() -> Vec<String> {
    vec![
        join_custom_id(JoinDecision::Approve, 10, 1),
        join_custom_id(JoinDecision::Deny, 10, 2),
        join_custom_id(JoinDecision::Block, 10, 3),
        kick_custom_id(VoteBallot::Yes, 99),
        kick_custom_id(VoteBallot::No, 99),
        name_custom_custom_id(10),
        name_restore_custom_id(10),
        name_modal_custom_id(10),
    ]
}

/// Every voice custom-id shape falls through to `Unknown`: never `Handled`
/// by the lfg/self-role handlers, never `Ignore` inside the guild fence.
#[test]
fn voice_shapes_route_unknown_on_components() {
    let router = InteractionRouter::new(all_on());
    let ids = minted_voice_ids();
    assert_eq!(ids.len(), 8, "all eight voice shapes covered");
    for id in &ids {
        // The codec still claims each minted shape, so the test pins the
        // router seam rather than a codec gap.
        assert!(
            parse_voice_custom_id(id).is_some(),
            "{id} must parse as a voice id"
        );
        assert_eq!(
            router.route_component(id, Some(GUILD)),
            ComponentOutcome::Unknown,
            "{id} must fall through to Unknown",
        );
    }
}

/// Modal submits follow the same table as components for voice shapes.
#[test]
fn voice_shapes_route_unknown_on_modals() {
    let router = InteractionRouter::new(all_on());
    for id in minted_voice_ids() {
        assert_eq!(
            router.route_modal(&id, Some(GUILD)),
            ComponentOutcome::Unknown,
            "{id} modal must fall through to Unknown",
        );
        assert_eq!(
            router.route_modal(&id, Some(GUILD)),
            router.route_component(&id, Some(GUILD)),
            "{id} modal must match the component table",
        );
    }
}

/// The undispatched seam holds regardless of feature gates: with every gate
/// off, voice shapes are still `Unknown` (not `Ignore`) inside the guild.
#[test]
fn voice_shapes_stay_unknown_with_all_gates_off() {
    let router = InteractionRouter::new(all_off());
    for id in minted_voice_ids() {
        assert_eq!(
            router.route_component(&id, Some(GUILD)),
            ComponentOutcome::Unknown,
            "{id} must stay Unknown with gates off",
        );
        assert_eq!(
            router.route_modal(&id, Some(GUILD)),
            ComponentOutcome::Unknown,
            "{id} modal must stay Unknown with gates off",
        );
    }
}

/// No regression: `two:lfg:` / `two:self-role:` ids still route to their own
/// handlers on both the component and modal paths.
#[test]
fn lfg_and_self_role_ids_keep_their_handlers() {
    let router = InteractionRouter::new(all_on());
    for (id, handler) in [
        ("two:lfg:abc123", ComponentHandler::LfgSignup),
        ("two:self-role:games:valorant", ComponentHandler::SelfRole),
        ("two:self-role:games", ComponentHandler::SelfRole),
    ] {
        assert_eq!(
            router.route_component(id, Some(GUILD)),
            ComponentOutcome::Handled { handler },
            "{id} must keep routing to its handler",
        );
        assert_eq!(
            router.route_modal(id, Some(GUILD)),
            ComponentOutcome::Handled { handler },
            "{id} modal must keep routing to its handler",
        );
    }
}

/// Malformed, overlong and zero-id voice shapes are `Unknown`, never a
/// panic, on both dispatch paths.
#[test]
fn malformed_voice_shapes_are_unknown_never_panic() {
    let router = InteractionRouter::new(all_on());
    let long = format!("two:voice:name-custom:{}", "1".repeat(90));
    assert!(long.len() > MAX_VOICE_CUSTOM_ID_CHARS);
    let malformed = vec![
        String::new(),
        "two:voice:".to_owned(),
        "two:voice:join-approve".to_owned(),
        "two:voice:join-approve:10".to_owned(),
        "two:voice:join-approve:10:1:extra".to_owned(),
        "two:voice:kick-yes".to_owned(),
        "two:voice:kick-yes:9:9".to_owned(),
        "two:voice:name-custom".to_owned(),
        "two:voice:explode:10".to_owned(),
        "two:voice:join-approve:0:1".to_owned(),
        "two:voice:join-approve:10:0".to_owned(),
        "two:voice:kick-yes:0".to_owned(),
        "two:voice:name-modal:0".to_owned(),
        "two:voice:kick-yes:-1".to_owned(),
        long,
        // Adversarial length far past Discord's limit: the router does plain
        // prefix matching and must not panic on any input length.
        format!("two:voice:{}", "x".repeat(500)),
    ];
    // Every malformed shape decodes to `None`, so routing `Unknown` agrees
    // with the codec rather than masking a parse gap.
    for id in &malformed {
        assert_eq!(parse_voice_custom_id(id), None, "{id}");
    }
    for id in &malformed {
        assert_eq!(
            router.route_component(id, Some(GUILD)),
            ComponentOutcome::Unknown,
            "{id} must be Unknown, never a panic",
        );
        assert_eq!(
            router.route_modal(id, Some(GUILD)),
            ComponentOutcome::Unknown,
            "{id} modal must be Unknown, never a panic",
        );
    }
}

/// The guild fence still binds voice shapes: outside the configured guild
/// they are `Ignore`, exactly like any other unknown id.
#[test]
fn voice_shapes_respect_the_guild_fence() {
    let router = InteractionRouter::new(all_on());
    let unconfigured = InteractionRouter::new(RouterGates {
        configured_guild: None,
        ..all_on()
    });
    for id in minted_voice_ids() {
        for (router, guild, label) in [
            (&router, Some(9999), "foreign guild"),
            (&router, None, "missing guild"),
            (&unconfigured, Some(GUILD), "unconfigured router"),
        ] {
            assert_eq!(
                router.route_component(&id, guild),
                ComponentOutcome::Ignore,
                "{id} ignores {label}",
            );
            assert_eq!(
                router.route_modal(&id, guild),
                ComponentOutcome::Ignore,
                "{id} modal ignores {label}",
            );
        }
    }
}
