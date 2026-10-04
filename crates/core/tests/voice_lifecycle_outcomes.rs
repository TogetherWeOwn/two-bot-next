//! Offline lifecycle-outcome contract for voice cutover verification.
//!
//! Pins the metric vocabulary the staging cutover queries depend on,
//! without a database, Discord, staging guild or production identity.
//! The worker emits lifecycle outcomes through [`Metrics`] (landed in the
//! TOG-13542/43 slices); these tests pin the full operation/outcome grid,
//! the reconcile/dead-letter/state/orphan semantics, and the REST result
//! classes under the voice route templates — all on a fresh per-test
//! [`Metrics::default`] so parallel tests never share state.

use two_bot_core::metrics::{
    Metrics, REST_ROUTES, VOICE_DEAD_ACTIONS, VOICE_OPERATIONS, VOICE_RECONCILE_ACTIONS,
};

/// Every series line for one metric family, in render order.
fn series_for(text: &str, family: &str) -> Vec<(String, u64)> {
    text.lines()
        .filter(|line| line.starts_with(family))
        .map(|line| {
            let (key, value) = line
                .rsplit_once(' ')
                .unwrap_or_else(|| panic!("bad sample: {line}"));
            (
                key.to_owned(),
                value
                    .parse()
                    .unwrap_or_else(|_| panic!("bad sample: {line}")),
            )
        })
        .collect()
}

// ---- (1) operation/outcome vocabulary ----

/// The cutover health queries name exact `op`/`outcome` pairs; every
/// emission site must survive to exposition. The grid below mirrors the
/// worker's call sites: `success` for create/move/delete, `category_full`
/// for create, `discord` for create/move/delete, `persistence` for
/// create/delete, and `cancelled` for create/delete.
#[test]
fn full_operation_outcome_grid_renders_every_cutover_series() {
    let metrics = Metrics::default();
    let grid: &[(&str, &[&str])] = &[
        (
            "create",
            &[
                "success",
                "category_full",
                "discord",
                "persistence",
                "cancelled",
            ],
        ),
        ("move", &["success", "discord"]),
        (
            "delete",
            &["success", "discord", "persistence", "cancelled"],
        ),
    ];
    for (op, outcomes) in grid {
        for outcome in *outcomes {
            metrics.voice_operation(op, outcome);
        }
    }
    let text = metrics.render(None);
    let series = series_for(&text, "two_bot_voice_operations_total{");
    // Fixed cardinality: 3 ops x 5 outcomes, zeros included.
    assert_eq!(series.len(), VOICE_OPERATIONS.len() * 5);
    for (op, outcomes) in grid {
        for outcome in *outcomes {
            let line =
                format!("two_bot_voice_operations_total{{op=\"{op}\",outcome=\"{outcome}\"}}");
            assert!(
                series
                    .iter()
                    .any(|(key, count)| key == &line && *count == 1),
                "missing or miscounted series: {line}"
            );
        }
    }
}

#[test]
fn repeated_outcomes_accumulate_without_new_series() {
    let metrics = Metrics::default();
    for _ in 0..7 {
        metrics.voice_operation("delete", "success");
    }
    let text = metrics.render(None);
    let series = series_for(&text, "two_bot_voice_operations_total{");
    assert_eq!(series.len(), VOICE_OPERATIONS.len() * 5);
    assert!(text.contains("two_bot_voice_operations_total{op=\"delete\",outcome=\"success\"} 7\n"));
}

// ---- (2) reconcile plan sizes ----

#[test]
fn reconcile_plan_sizes_are_additive_per_action() {
    let metrics = Metrics::default();
    metrics.voice_reconcile("delete_enqueued", 2);
    metrics.voice_reconcile("delete_enqueued", 3);
    metrics.voice_reconcile("suspended", 1);
    metrics.voice_reconcile("resumed", 4);
    metrics.voice_reconcile("succession_enqueued", 0);
    let text = metrics.render(None);
    let series = series_for(&text, "two_bot_voice_reconcile_actions_total{");
    assert_eq!(series.len(), VOICE_RECONCILE_ACTIONS.len());
    assert!(text.contains("two_bot_voice_reconcile_actions_total{action=\"delete_enqueued\"} 5\n"));
    assert!(text.contains("two_bot_voice_reconcile_actions_total{action=\"suspended\"} 1\n"));
    assert!(text.contains("two_bot_voice_reconcile_actions_total{action=\"resumed\"} 4\n"));
    assert!(
        text.contains("two_bot_voice_reconcile_actions_total{action=\"succession_enqueued\"} 0\n")
    );
}

#[test]
fn zero_reconcile_pass_adds_no_plan_size() {
    let metrics = Metrics::default();
    for action in VOICE_RECONCILE_ACTIONS {
        metrics.voice_reconcile(action, 0);
    }
    let text = metrics.render(None);
    for action in VOICE_RECONCILE_ACTIONS {
        assert!(text.contains(&format!(
            "two_bot_voice_reconcile_actions_total{{action=\"{action}\"}} 0\n"
        )));
    }
}

// ---- (3) dead-letter families ----

#[test]
fn every_dead_letter_family_has_its_own_series() {
    let metrics = Metrics::default();
    for family in VOICE_DEAD_ACTIONS {
        metrics.voice_dead_letter(family);
    }
    let text = metrics.render(None);
    let series = series_for(&text, "two_bot_voice_dead_letters_total{");
    assert_eq!(series.len(), VOICE_DEAD_ACTIONS.len());
    for family in VOICE_DEAD_ACTIONS {
        assert!(text.contains(&format!(
            "two_bot_voice_dead_letters_total{{action=\"{family}\"}} 1\n"
        )));
    }
}

// ---- (4) worker state gauges and orphans ----

#[test]
fn tracked_state_is_last_writer_wins_for_ghost_checks() {
    let metrics = Metrics::default();
    metrics.voice_state(7, 2);
    metrics.voice_state(3, 0);
    let text = metrics.render(None);
    assert!(text.contains("two_bot_voice_tracked_rooms 3\n"));
    assert!(text.contains("two_bot_voice_compensation_pending 0\n"));
}

#[test]
fn orphans_accumulate_until_manual_cleanup() {
    let metrics = Metrics::default();
    metrics.voice_orphan();
    metrics.voice_orphan();
    assert!(metrics
        .render(None)
        .contains("two_bot_voice_orphans_total 2\n"));
}

// ---- (5) REST outcomes under the voice templates ----

/// The RoomHttp send path records one `rest_response` per wire send; the
/// create/delete/move requests must land under allowlisted templates so the
/// 429 alert and cutover queries see voice traffic.
#[test]
fn voice_rest_outcomes_land_under_allowlisted_templates() {
    for template in [
        "POST /guilds/:guild/channels",
        "DELETE /channels/:channel",
        "PATCH /guilds/:guild/members/:member",
    ] {
        assert!(
            REST_ROUTES.contains(&template),
            "voice template missing from allowlist: {template}"
        );
    }
    let metrics = Metrics::default();
    // One finished exchange per result class, as Attempt::finish records.
    let exchanges: &[(&str, Option<u16>, &str)] = &[
        ("POST /guilds/:guild/channels", Some(200), "2xx"),
        ("POST /guilds/:guild/channels", Some(429), "429"),
        ("DELETE /channels/:channel", Some(403), "4xx"),
        ("DELETE /channels/:channel", Some(503), "5xx"),
        ("PATCH /guilds/:guild/members/:member", None, "transport"),
    ];
    for (route, status, _) in exchanges {
        metrics.rest_response(route, *status);
    }
    let text = metrics.render(None);
    for (route, _, result) in exchanges {
        assert!(text.contains(&format!(
            "two_bot_rest_requests_total{{route=\"{route}\",result=\"{result}\"}} 1\n"
        )));
    }
}
