//! Protected acceptance: every `Handled` outcome is registered and claimed once.
//!
//! Two contract tests over the live publish set:
//!
//! 1. Every `route_slash`-reachable [`HandlerId`] is registered on the
//!    production router (RED: `Rsvp`/`RsvpAttendance`/`ScorecardAttendance`).
//! 2. Every `Handled` outcome is claimed by exactly one surface (RED: the
//!    three RSVP outcomes are claimed by zero surfaces while unregistered).
//!
//! Production wiring (`router_with_commands`, `CommandRuntime::build`) is
//! binary-private, so this file compiles on the baseline through the
//! test-local [`ProductionRegistrations`] trait, which mirrors that wiring
//! line for line (cited below) using the real public registration functions
//! where they exist. The conformance tests call the seam as
//! `InteractionRouter::production_router(..)` /
//! `InteractionRouter::intentional_registration_exceptions(..)`: those calls
//! hit this trait now and the builder's inherent methods later, because
//! inherent methods win. GREEN requires the builder to add, on
//! `InteractionRouter` with exactly these signatures:
//!
//! ```ignore
//! pub fn production_router(gates: RouterGates, member_present: bool) -> InteractionRouter;
//! pub fn intentional_registration_exceptions(member_present: bool) -> Vec<HandlerId>;
//! ```
//!
//! where `production_router` carries the full production registration set
//! (including the three RSVP ids) and `intentional_registration_exceptions`
//! is the single place blessing any id that intentionally stays unregistered
//! (today: the five member verbs while no member consumer exists). The
//! `baseline_mirror_keeps_the_adapter_trait_live` control keeps this trait
//! used in both states via explicit qualified calls.
//!
//! The claim model counts a surface only when the handler is registered (or
//! intentionally excepted) — the channel slice's `handler_for` pattern
//! generalized. Surfaces, in production dispatch order: the ordered RSVP
//! surface (`dispatch_remaining` skip + `prepare` claim), the channel slice,
//! the LFG arm, the leveling arm, the help arm, the owner-map execution, the
//! unavailable terminal (the owner-mismatch reply, for names that reach
//! `on_interaction`), and the custom-command surface.
//!
//! No guild, no network, no database. The builder must not edit this file.
#![cfg(feature = "db")]

use std::collections::HashSet;
use two_bot_core::moderation::ModerationAction;
use two_bot_core::router::{
    HandlerId, InteractionHandler, InteractionRouter, RouterGates, SlashContext, SlashOutcome,
};
use two_bot_discord::rsvp::is_rsvp_command;

const GUILD: u64 = 2222;
const PERMS: u64 = u64::MAX;
const CUSTOM_NAME: &str = "faq";

#[derive(Debug)]
struct Stub(HandlerId);

impl InteractionHandler for Stub {
    fn id(&self) -> HandlerId {
        self.0
    }
}

/// Baseline production wiring, mirrored from the binary crate.
///
/// - `router_with_commands` (`command_runtime.rs`): channel handlers while
///   `moderation`, sticky `AutomationAdmin` + custom commands while
///   `automations`, feeds while `announcements`, rank/leaderboard/help/
///   privacy/game-mix always. The channel and custom registrations below call the
///   real public functions; the rest are marker ids, as in production.
/// - `CommandRuntime::build` member verbs (`command_runtime.rs`): the five
///   member moderation ids while a member consumer exists.
/// - `InteractionRuntime::with_router` (`interactions.rs`, public, ungated):
///   the `Lfg`/`LfgClose`/`LfgEdit` handlers.
/// - Missing on the baseline: `Rsvp`, `RsvpAttendance`,
///   `ScorecardAttendance` — the RED this file pins.
trait ProductionRegistrations {
    fn production_router(gates: RouterGates, member_present: bool) -> InteractionRouter;
    fn intentional_registration_exceptions(member_present: bool) -> Vec<HandlerId>;
}

impl ProductionRegistrations for InteractionRouter {
    fn production_router(gates: RouterGates, member_present: bool) -> InteractionRouter {
        let mut router = InteractionRouter::new(gates);
        if gates.moderation {
            two_bot_discord::channel_moderation::register_channel_handlers(&mut router);
        }
        if gates.automations {
            router.register(Box::new(Stub(HandlerId::AutomationAdmin)));
            two_bot_discord::custom_commands::CustomCommandRuntime::register(&mut router);
        }
        if gates.announcements {
            for id in [
                HandlerId::FeedAdd,
                HandlerId::FeedRemove,
                HandlerId::FeedList,
                HandlerId::FeedInspect,
            ] {
                router.register(Box::new(Stub(id)));
            }
        }
        for id in [
            HandlerId::Rank,
            HandlerId::Leaderboard,
            HandlerId::Help,
            HandlerId::Privacy,
            HandlerId::GameMix,
        ] {
            router.register(Box::new(Stub(id)));
        }
        if member_present {
            for action in [
                ModerationAction::Ban,
                ModerationAction::TempBan,
                ModerationAction::Kick,
                ModerationAction::Timeout,
                ModerationAction::Warn,
            ] {
                router.register(Box::new(Stub(HandlerId::Moderation(action))));
            }
        }
        router.register(Box::new(Stub(HandlerId::Lfg)));
        router.register(Box::new(Stub(HandlerId::LfgClose)));
        router.register(Box::new(Stub(HandlerId::LfgEdit)));
        router.register(Box::new(Stub(HandlerId::LfgInspect)));
        // Next-only `/lfg-list`: name-based so the baseline still compiles
        // before the production `HandlerId::LfgList` arm lands. Resolve the
        // handler from the routing table when the builder lands it instead of
        // naming the enum.
        if let SlashOutcome::Handled { handler } =
            InteractionRouter::new(gates).route_slash(&SlashContext {
                name: "lfg-list",
                guild_id: gates.configured_guild,
                actor_permissions: Some(u64::MAX),
                custom_row: None,
            })
        {
            router.register(Box::new(Stub(handler)));
        }
        router
    }

    fn intentional_registration_exceptions(member_present: bool) -> Vec<HandlerId> {
        // Member verbs register only while the member consumer exists; without
        // it the gap is intentional and the owner map drops them to the
        // unavailable reply in the same shape.
        if member_present {
            Vec::new()
        } else {
            vec![
                HandlerId::Moderation(ModerationAction::Ban),
                HandlerId::Moderation(ModerationAction::TempBan),
                HandlerId::Moderation(ModerationAction::Kick),
                HandlerId::Moderation(ModerationAction::Timeout),
                HandlerId::Moderation(ModerationAction::Warn),
            ]
        }
    }
}

fn all_gates_on() -> RouterGates {
    RouterGates {
        configured_guild: Some(GUILD),
        scorecard: true,
        automations: true,
        announcements: true,
        moderation: true,
        voice: true,
        voice_assistant: true,
        tickets: true,
        self_roles: true,
        onboarding_picker: true,
        session_picker: true,
    }
}

fn ctx(name: &str, custom_row: Option<bool>) -> SlashContext<'_> {
    SlashContext {
        name,
        guild_id: Some(GUILD),
        actor_permissions: Some(PERMS),
        custom_row,
    }
}

/// Exact mirror of the `on_interaction` owner map (`command_runtime.rs`),
/// including the member-consumer guards. No `"help"` entry: the help arm
/// owns `/help` and returns first, so an owner-map entry could never execute.
fn owner_map(name: &str, member_present: bool) -> Option<HandlerId> {
    match name {
        "privacy" => Some(HandlerId::Privacy),
        "game-mix" => Some(HandlerId::GameMix),
        "sticky" | "sticky-remove" | "schedule" | "schedule-remove" | "schedule-list" => {
            Some(HandlerId::AutomationAdmin)
        }
        "feed-add" => Some(HandlerId::FeedAdd),
        "feed-remove" => Some(HandlerId::FeedRemove),
        "feed-list" => Some(HandlerId::FeedList),
        "feed-inspect" => Some(HandlerId::FeedInspect),
        "ban" if member_present => Some(HandlerId::Moderation(ModerationAction::Ban)),
        "tempban" if member_present => Some(HandlerId::Moderation(ModerationAction::TempBan)),
        "kick" if member_present => Some(HandlerId::Moderation(ModerationAction::Kick)),
        "timeout" if member_present => Some(HandlerId::Moderation(ModerationAction::Timeout)),
        "warn" if member_present => Some(HandlerId::Moderation(ModerationAction::Warn)),
        _ => None,
    }
}

fn is_channel_claim(name: &str, handler: HandlerId) -> bool {
    ModerationAction::ALL.into_iter().any(|a| {
        !a.targets_member() && a.command_name() == name && handler == HandlerId::Moderation(a)
    }) || matches!(
        (name, handler),
        ("purge-preview", HandlerId::PurgePreview) | ("channel-inspect", HandlerId::ChannelInspect)
    )
}

/// Surfaces claiming one `Handled` outcome, in production dispatch order. A
/// dispatch surface counts only when the handler is registered (or
/// intentionally excepted). The unavailable terminal counts for names that
/// reach `on_interaction`'s owner check with no earlier arm matching: RSVP
/// names never reach it (`dispatch_remaining` skips them to the ordered
/// owner) and custom outcomes never occur there (`custom_row` is `None`).
fn claiming_surfaces(
    router: &InteractionRouter,
    exceptions: &HashSet<HandlerId>,
    gates: RouterGates,
    name: &str,
    custom_row: Option<bool>,
    handler: HandlerId,
    member_present: bool,
) -> Vec<&'static str> {
    let admitted = router.handler_for(&handler).is_some() || exceptions.contains(&handler);
    let mut claims = Vec::new();
    if is_rsvp_command(name)
        && matches!(
            handler,
            HandlerId::Rsvp | HandlerId::RsvpAttendance | HandlerId::ScorecardAttendance
        )
        && admitted
    {
        claims.push("ordered-rsvp");
    }
    if is_channel_claim(name, handler) && admitted {
        claims.push("channel");
    }
    if (matches!(
        handler,
        HandlerId::Lfg | HandlerId::LfgClose | HandlerId::LfgEdit | HandlerId::LfgInspect
    ) || name == "lfg-list")
        && admitted
    {
        claims.push("lfg");
    }
    if matches!(handler, HandlerId::Rank | HandlerId::Leaderboard) && admitted {
        claims.push("leveling");
    }
    if name == "help" && handler == HandlerId::Help && admitted {
        claims.push("help");
    }
    if owner_map(name, member_present) == Some(handler) && admitted {
        claims.push("owner");
    }
    let earlier_arm_matches = is_channel_claim(name, handler)
        || matches!(
            handler,
            HandlerId::Lfg
                | HandlerId::LfgClose
                | HandlerId::LfgEdit
                | HandlerId::LfgInspect
                | HandlerId::Rank
                | HandlerId::Leaderboard
        )
        || name == "lfg-list"
        || (name == "help" && handler == HandlerId::Help)
        || owner_map(name, member_present) == Some(handler);
    if !is_rsvp_command(name) && custom_row.is_none() && !earlier_arm_matches {
        claims.push("unavailable");
    }
    if handler == HandlerId::AutomationCustom
        && custom_row == Some(true)
        && gates.automations
        && admitted
    {
        claims.push("custom");
    }
    claims
}

/// Every published name with `custom_row: None` (the `on_interaction` +
/// ordered-RSVP domain), plus one synthetic custom row for `AutomationCustom`.
fn routed_cases() -> Vec<(String, Option<bool>)> {
    let router = InteractionRouter::new(all_gates_on());
    let defs = router.publish_set(&[]).expect("full set assembles");
    let mut cases: Vec<(String, Option<bool>)> = defs.into_iter().map(|d| (d.name, None)).collect();
    cases.push((CUSTOM_NAME.to_owned(), Some(true)));
    cases
}

#[test]
fn every_routeable_handler_is_registered_or_intentionally_excepted() {
    let pure = InteractionRouter::new(all_gates_on());
    for member_present in [true, false] {
        // Unqualified: the test-local trait now, the builder's inherent seam
        // later.
        let router = InteractionRouter::production_router(all_gates_on(), member_present);
        let exceptions: HashSet<HandlerId> =
            InteractionRouter::intentional_registration_exceptions(member_present)
                .into_iter()
                .collect();
        let mut routeable = HashSet::new();
        for (name, custom_row) in routed_cases() {
            if let SlashOutcome::Handled { handler } = pure.route_slash(&ctx(&name, custom_row)) {
                routeable.insert(handler);
            }
        }
        assert!(
            routeable.contains(&HandlerId::Rsvp)
                && routeable.contains(&HandlerId::RsvpAttendance)
                && routeable.contains(&HandlerId::ScorecardAttendance),
            "precondition: the three RSVP ids are routeable"
        );
        let mut unregistered: Vec<HandlerId> = routeable
            .into_iter()
            .filter(|id| router.handler_for(id).is_none() && !exceptions.contains(id))
            .collect();
        unregistered.sort_by_key(|id| format!("{id:?}"));
        assert!(
            unregistered.is_empty(),
            "member_present={member_present}: routeable but neither registered nor \
             intentionally excepted: {unregistered:?}"
        );
    }
}

#[test]
fn every_handled_outcome_is_claimed_by_exactly_one_surface() {
    let pure = InteractionRouter::new(all_gates_on());
    for member_present in [true, false] {
        let router = InteractionRouter::production_router(all_gates_on(), member_present);
        let exceptions: HashSet<HandlerId> =
            InteractionRouter::intentional_registration_exceptions(member_present)
                .into_iter()
                .collect();
        let mut bad = Vec::new();
        for (name, custom_row) in routed_cases() {
            let SlashOutcome::Handled { handler } = pure.route_slash(&ctx(&name, custom_row))
            else {
                continue;
            };
            let claims = claiming_surfaces(
                &router,
                &exceptions,
                all_gates_on(),
                &name,
                custom_row,
                handler,
                member_present,
            );
            if claims.len() != 1 {
                bad.push((name, format!("{handler:?}"), claims));
            }
        }
        assert!(
            bad.is_empty(),
            "member_present={member_present}: Handled outcomes claimed by zero or \
             two+ surfaces (name, handler, surfaces): {bad:?}"
        );
    }
}

/// CONTROL (green before and after): explicit qualified calls keep the
/// [`ProductionRegistrations`] trait live after the builder's inherent seam
/// lands (inherent methods would otherwise shadow every unqualified call and
/// leave the trait unused, which fails `-D warnings`). These pin stable
/// truths of the baseline mirror only; conformance goes through the seam.
#[test]
fn baseline_mirror_keeps_the_adapter_trait_live() {
    let mirror =
        <InteractionRouter as ProductionRegistrations>::production_router(all_gates_on(), true);
    for id in [
        HandlerId::Rank,
        HandlerId::Help,
        HandlerId::AutomationCustom,
        HandlerId::Lfg,
        HandlerId::FeedList,
    ] {
        assert!(
            mirror.handler_for(&id).is_some(),
            "baseline mirror registers {id:?}"
        );
    }
    let no_member =
        <InteractionRouter as ProductionRegistrations>::production_router(all_gates_on(), false);
    assert!(
        no_member
            .handler_for(&HandlerId::Moderation(ModerationAction::Ban))
            .is_none(),
        "baseline mirror gates member verbs on the member consumer"
    );
    let exceptions =
        <InteractionRouter as ProductionRegistrations>::intentional_registration_exceptions(false);
    assert_eq!(
        exceptions.len(),
        5,
        "five member-verb exceptions without a consumer"
    );
    let none =
        <InteractionRouter as ProductionRegistrations>::intentional_registration_exceptions(true);
    assert!(none.is_empty(), "no exceptions with a member consumer");
}
