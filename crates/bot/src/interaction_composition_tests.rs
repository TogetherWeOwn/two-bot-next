#![cfg(test)]
//! Hermetic P1/P2 regressions for the ordered interaction surface composition.
//!
//! P1: the ordered surface sends through the command runtime's governed REST
//! executor (shared send admission). The ungoverned constructor stays rejected
//! for real origins, and the governed composition builds without a live
//! request. P2: malformed command gates park only the ordered surface; the
//! gateway still boots.
//!
//! No live Discord, no database I/O, no production/staging credentials:
//! synthetic tokens, lazy pools, and unroutable origins only. Gate cases run
//! in env-cleared child probes so no test mutates process-global env.

use std::sync::Arc;

const PROBE_ENV: &str = "INTERACTION_COMPOSITION_PROBE";
const SYNTHETIC_TOKEN: &str = "synthetic-composition-token";

fn lazy_pool() -> sqlx::Pool<sqlx::Postgres> {
    // Never connects: pool construction performs no I/O.
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://agent_test@agent-testdb/agent_test")
        .expect("lazy pool never connects")
}

fn legacy_onboarding() -> two_bot_core::OnboardingGates {
    two_bot_core::OnboardingGates::from_map(&std::collections::HashMap::new())
        .expect("empty gates are legacy defaults")
}

/// Staging guild/application pair (synthetic token segment, not a credential):
/// every capability permitted, so composition probes exercise env validation
/// rather than activation narrowing.
fn staging_activation() -> crate::activation::BootActivation {
    crate::activation::BootActivation::from_token(
        Some(1545644954272137297),
        Some("MTQ2OTEzNzYzNjY2Mzc1ODg4OA.mock.signature"),
    )
}

fn governed_executor() -> two_bot_discord::ActionExecutor {
    crate::gateway::ensure_crypto_provider();
    let pool = lazy_pool();
    // Pure key derivation over a synthetic token; no I/O, no live request.
    let admission = Arc::new(
        two_bot_core::send_admission::PgSendAdmission::new(pool, SYNTHETIC_TOKEN)
            .expect("synthetic token derives a key"),
    );
    two_bot_discord::ActionExecutor::with_admission(SYNTHETIC_TOKEN.to_owned(), None, admission)
        .expect("governed admission builds for the real origin")
}

#[test]
fn ungoverned_executor_stays_rejected_for_real_origins() {
    // P1: absent or non-loopback DISCORD_API_BASE without admission never
    // builds. Rejected before client construction: no I/O, no crypto needed.
    for proxy in [
        None,
        Some("https://discord.com".to_owned()),
        Some("https://proxy.invalid".to_owned()),
    ] {
        let error = two_bot_discord::ActionExecutor::with_proxy(SYNTHETIC_TOKEN.to_owned(), proxy)
            .expect_err("real origin without admission must not build");
        assert_eq!(error, "shared durable Discord send admission required");
        assert!(
            !error.contains("synthetic"),
            "rejection must not echo the token"
        );
    }
}

#[tokio::test]
async fn governed_executor_builds_for_real_origin_without_live_request() {
    // P1: the shared composition (lazy pool + derived key + admission) builds
    // for the real default origin and clones share the lane. Construction
    // performs no I/O and sends no request.
    let executor = governed_executor();
    let _shared = executor.clone();
}

fn run_child(name: &str, vars: &[(&str, &str)]) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .args([name, "--exact", "--nocapture"])
        .env(PROBE_ENV, "1")
        .envs(vars.iter().copied())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated composition probe failed: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[tokio::test]
async fn malformed_feed_poll_parks_ordered_surface_child() {
    if std::env::var_os(PROBE_ENV).is_none() {
        return;
    }
    // P2: malformed poll interval with automations off parks only the ordered
    // surface instead of failing gateway boot. No permissive default is
    // enabled: the surface is `None`, not built on invalid gates.
    let parked = crate::build_interaction_runtime(
        &lazy_pool(),
        100,
        &legacy_onboarding(),
        governed_executor(),
        &staging_activation(),
    );
    assert!(
        parked.is_none(),
        "malformed feed poll must park the ordered surface"
    );
}

#[test]
fn malformed_feed_poll_parks_ordered_surface() {
    run_child(
        "interaction_composition_tests::malformed_feed_poll_parks_ordered_surface_child",
        &[("TWO_FEED_POLL_SECONDS", "59")],
    );
}

#[tokio::test]
async fn malformed_protected_roles_park_ordered_surface_child() {
    if std::env::var_os(PROBE_ENV).is_none() {
        return;
    }
    // P2: malformed protected roles with moderation off parks only the
    // ordered surface instead of failing gateway boot.
    let parked = crate::build_interaction_runtime(
        &lazy_pool(),
        100,
        &legacy_onboarding(),
        governed_executor(),
        &staging_activation(),
    );
    assert!(
        parked.is_none(),
        "malformed protected roles must park the ordered surface"
    );
}

#[test]
fn malformed_protected_roles_park_ordered_surface() {
    run_child(
        "interaction_composition_tests::malformed_protected_roles_park_ordered_surface_child",
        &[("TWO_MODERATION_PROTECTED_ROLE_IDS", "not-an-id")],
    );
}

#[tokio::test]
async fn valid_gates_build_ordered_surface_child() {
    if std::env::var_os(PROBE_ENV).is_none() {
        return;
    }
    // P1/P2 together: default gates build the ordered surface over the
    // governed executor with no live request and no database I/O.
    let built = crate::build_interaction_runtime(
        &lazy_pool(),
        100,
        &legacy_onboarding(),
        governed_executor(),
        &staging_activation(),
    );
    assert!(
        built.is_some(),
        "default gates must build the ordered surface"
    );
}

#[test]
fn valid_gates_build_ordered_surface() {
    run_child(
        "interaction_composition_tests::valid_gates_build_ordered_surface_child",
        &[],
    );
}

/// F1: boot activation narrows the ordered router, never widens it. Pure
/// composition checks (no env, no I/O): the same all-on gates route `rsvp`
/// only under an identity cleared for announcements.
fn all_on_gates() -> two_bot_core::RouterGates {
    two_bot_core::RouterGates {
        configured_guild: Some(2222),
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

fn rsvp_outcome(gates: two_bot_core::RouterGates) -> two_bot_core::SlashOutcome {
    let router = two_bot_core::InteractionRouter::new(gates);
    let interaction = crate::command_runtime_tests::slash("rsvp", Some(1), Vec::new());
    match two_bot_discord::route_interaction(&router, &interaction, None) {
        two_bot_discord::RoutedInteraction::Slash { outcome, .. } => outcome,
        routed => panic!("rsvp slash must route as slash, got {routed:?}"),
    }
}

#[test]
fn staging_activation_keeps_rsvp_routable() {
    let narrowed = staging_activation().constrain_router(all_on_gates());
    assert!(narrowed.announcements, "staging clears announcements");
    assert!(
        matches!(
            rsvp_outcome(narrowed),
            two_bot_core::SlashOutcome::Handled { .. }
        ),
        "staging composition must route rsvp to its handler"
    );
}

#[test]
fn live_activation_refuses_uncleared_rsvp() {
    // Synthetic live pair (guild + token segment, not credentials): only the
    // reviewed self-role clearance survives.
    let live = crate::activation::BootActivation::from_token(
        Some(326474832151838730),
        Some("MTUzOTcxMTY4Mzg5ODExODE1NA.mock.signature"),
    );
    let narrowed = live.constrain_router(all_on_gates());
    assert!(!narrowed.announcements, "live must narrow announcements");
    assert!(!narrowed.automations, "live must narrow automations");
    assert!(!narrowed.moderation, "live must narrow moderation");
    assert!(!narrowed.tickets, "live must narrow tickets");
    assert!(
        narrowed.self_roles,
        "live keeps the cleared self-role surface"
    );
    assert!(
        matches!(
            rsvp_outcome(narrowed),
            two_bot_core::SlashOutcome::Refuse { .. }
        ),
        "live composition must refuse uncleared rsvp, never execute it"
    );
}

#[test]
fn unknown_activation_disables_every_surface() {
    for activation in [
        crate::activation::BootActivation::from_token(None, None),
        crate::activation::BootActivation::from_token(Some(100), None),
        crate::activation::BootActivation::from_token(Some(100), Some("not-a-token")),
    ] {
        let narrowed = activation.constrain_router(all_on_gates());
        assert!(!narrowed.announcements, "unknown must narrow announcements");
        assert!(!narrowed.automations, "unknown must narrow automations");
        assert!(!narrowed.moderation, "unknown must narrow moderation");
        assert!(!narrowed.tickets, "unknown must narrow tickets");
        assert!(!narrowed.self_roles, "unknown must narrow self-roles");
        assert!(
            matches!(
                rsvp_outcome(narrowed),
                two_bot_core::SlashOutcome::Refuse { .. }
            ),
            "unknown composition must refuse rsvp, never execute it"
        );
    }
}
