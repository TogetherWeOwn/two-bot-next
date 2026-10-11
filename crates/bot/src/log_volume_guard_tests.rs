//! Log-volume and cardinality guard: offline test-only pin of
//! `docs/log-volume-guard.md`.
//!
//! The guard bounds cutover log risk without touching staging, webhooks or
//! tokens: every gateway metric label, every job label and every cataloged
//! session-path log line has exactly one cap row here. Adding a dispatch
//! label, a job, a voice family member or a metric series fails this suite
//! until the doc and the row are updated together. Nothing here changes
//! runtime behavior.

use two_bot_core::metrics;

/// Volume class from the guard doc. Session and steady rows never shed;
/// hot rows shed their downstream pipeline work first and never gain a
/// per-event log line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum VolumeClass {
    Session,
    Steady,
    Hot,
}

#[derive(Clone, Copy)]
struct EventCap {
    label: &'static str,
    class: VolumeClass,
    shed_order: Option<u8>,
}

/// One row per `metrics::EVENTS` entry, in the same order.
const EVENT_CAPS: [EventCap; 21] = [
    EventCap {
        label: "READY",
        class: VolumeClass::Session,
        shed_order: None,
    },
    EventCap {
        label: "RESUMED",
        class: VolumeClass::Session,
        shed_order: None,
    },
    EventCap {
        label: "GUILD_CREATE",
        class: VolumeClass::Steady,
        shed_order: None,
    },
    EventCap {
        label: "GUILD_DELETE",
        class: VolumeClass::Steady,
        shed_order: None,
    },
    EventCap {
        label: "GUILD_UPDATE",
        class: VolumeClass::Steady,
        shed_order: None,
    },
    EventCap {
        label: "GUILD_MEMBER_ADD",
        class: VolumeClass::Steady,
        shed_order: None,
    },
    EventCap {
        label: "GUILD_MEMBER_REMOVE",
        class: VolumeClass::Steady,
        shed_order: None,
    },
    EventCap {
        label: "GUILD_MEMBER_UPDATE",
        class: VolumeClass::Hot,
        shed_order: Some(7),
    },
    EventCap {
        label: "MESSAGE_CREATE",
        class: VolumeClass::Hot,
        shed_order: Some(1),
    },
    EventCap {
        label: "MESSAGE_UPDATE",
        class: VolumeClass::Hot,
        shed_order: Some(2),
    },
    EventCap {
        label: "MESSAGE_DELETE",
        class: VolumeClass::Hot,
        shed_order: Some(3),
    },
    EventCap {
        label: "MESSAGE_REACTION_ADD",
        class: VolumeClass::Hot,
        shed_order: Some(5),
    },
    EventCap {
        label: "MESSAGE_REACTION_REMOVE",
        class: VolumeClass::Hot,
        shed_order: Some(6),
    },
    EventCap {
        label: "VOICE_STATE_UPDATE",
        class: VolumeClass::Hot,
        shed_order: Some(4),
    },
    EventCap {
        label: "PRESENCE_UPDATE",
        class: VolumeClass::Hot,
        shed_order: Some(8),
    },
    EventCap {
        label: "INVITE_CREATE",
        class: VolumeClass::Steady,
        shed_order: None,
    },
    EventCap {
        label: "INVITE_DELETE",
        class: VolumeClass::Steady,
        shed_order: None,
    },
    EventCap {
        label: "INTERACTION_CREATE",
        class: VolumeClass::Steady,
        shed_order: None,
    },
    EventCap {
        label: "HEARTBEAT_ACK",
        class: VolumeClass::Steady,
        shed_order: None,
    },
    EventCap {
        label: "GATEWAY_CLOSE",
        class: VolumeClass::Session,
        shed_order: None,
    },
    EventCap {
        label: "other",
        class: VolumeClass::Hot,
        shed_order: Some(9),
    },
];

#[derive(Clone, Copy)]
struct JobCap {
    job: &'static str,
    shed_order: Option<u8>,
}

/// One row per `metrics::JOBS` entry, in the same order. The hourly
/// presence probe parks first; the durable checkpoint marker never sheds.
const JOB_CAPS: [JobCap; 12] = [
    JobCap {
        job: "invite_snapshot",
        shed_order: None,
    },
    JobCap {
        job: "session_checkpoint",
        shed_order: None,
    },
    JobCap {
        job: "counter",
        shed_order: None,
    },
    JobCap {
        job: "rank",
        shed_order: None,
    },
    JobCap {
        job: "scheduled_events",
        shed_order: None,
    },
    JobCap {
        job: "settings",
        shed_order: None,
    },
    JobCap {
        job: "presence_probe",
        shed_order: Some(1),
    },
    JobCap {
        job: "community_scorecard",
        shed_order: Some(2),
    },
    JobCap {
        job: "inactivity",
        shed_order: Some(3),
    },
    JobCap {
        job: "audit_retry",
        shed_order: Some(4),
    },
    JobCap {
        job: "scheduled_messages",
        shed_order: Some(5),
    },
    JobCap {
        job: "other",
        shed_order: None,
    },
];

/// Closed-world pins for the storage/send-gate families: a new op or outcome
/// fails here until it gets a budget row in the guard doc. The
/// send-admission `blocked` outcome is the pipeline's own shed meter:
/// refused admits are counted, never silently dropped.
const DB_ERROR_CAP_OPS: [&str; 2] = ["admission", "other"];
const SEND_ADMISSION_CAP_OUTCOMES: [&str; 4] = ["admitted", "blocked", "storage_error", "other"];
const PREFIX_TRIGGER_REFUSED_CAP_REASONS: [&str; 2] = ["verdict", "other"];
/// One row per `metrics::DISPATCH_LANES` entry, in the same order. A new
/// dispatch lane fails here until it gets a budget row in the guard doc.
const DISPATCH_LANE_CAPS: [&str; 6] = [
    "messages",
    "interactions",
    "registry",
    "privileged",
    "busy",
    "reactions",
];
/// One row per `metrics::VOICE_VOTE_KICK_OUTCOMES` entry, in the same order.
/// A new vote-kick outcome fails here until it gets a budget row in the guard
/// doc.
const VOTE_KICK_CAP_OUTCOMES: [&str; 26] = [
    "started",
    "evidence_unavailable",
    "not_a_room",
    "initiator_not_occupant",
    "target_not_occupant",
    "self_target",
    "protected_target",
    "privileged_target",
    "authority_unavailable",
    "active_vote_exists",
    "cooldown",
    "initiator_limited",
    "reused_vote_id",
    "unknown_vote",
    "wrong_vote_boundary",
    "ineligible_voter",
    "repeated_vote",
    "invalid_time",
    "connect_denied_and_disconnected",
    "connect_denied_target_absent",
    "skipped_room_gone",
    "skipped_target_protected",
    "permission_missing",
    "discord_error",
    "gave_up",
    "other",
];
/// One row per `metrics::INTERNAL_ACTION_FAMILIES` entry, in the same order.
/// A new receiver family fails here until it gets a budget row in the guard
/// doc. Unknown verbs collapse to the trailing `other`.
const INTERNAL_ACTION_CAP_FAMILIES: [&str; 6] = [
    "announcement",
    "event",
    "settings",
    "moderation",
    "membership",
    "other",
];
/// One row per `metrics::INTERNAL_ACTION_OUTCOMES` entry, in the same order.
/// A new receiver outcome fails here until it gets a budget row in the guard
/// doc. Unknown outcomes collapse to the trailing `internal`, never to a
/// dynamic label or secret.
const INTERNAL_ACTION_CAP_OUTCOMES: [&str; 12] = [
    "executed",
    "auth_failure",
    "unknown_key",
    "clock_skew",
    "nonce_replay",
    "rate_limit",
    "unknown_action",
    "action_disabled",
    "malformed_body",
    "conflict",
    "upstream",
    "internal",
];

#[test]
fn storage_and_send_gate_labels_match_their_caps() {
    assert_eq!(
        metrics::DB_ERROR_OPS,
        &DB_ERROR_CAP_OPS[..],
        "DB_ERROR_OPS grew without a budget row; update docs/log-volume-guard.md"
    );
    assert_eq!(
        metrics::SEND_ADMISSION_OUTCOMES,
        &SEND_ADMISSION_CAP_OUTCOMES[..],
        "SEND_ADMISSION_OUTCOMES grew without a budget row; update docs/log-volume-guard.md"
    );
    assert_eq!(
        metrics::PREFIX_TRIGGER_REFUSED_REASONS,
        &PREFIX_TRIGGER_REFUSED_CAP_REASONS[..],
        "PREFIX_TRIGGER_REFUSED_REASONS grew without a budget row; update docs/log-volume-guard.md"
    );
    assert_eq!(
        metrics::DISPATCH_LANES,
        &DISPATCH_LANE_CAPS[..],
        "DISPATCH_LANES grew without a budget row; update docs/log-volume-guard.md"
    );
    assert_eq!(
        metrics::VOICE_VOTE_KICK_OUTCOMES,
        &VOTE_KICK_CAP_OUTCOMES[..],
        "VOICE_VOTE_KICK_OUTCOMES grew without a budget row; update docs/log-volume-guard.md"
    );
    assert_eq!(
        metrics::INTERNAL_ACTION_FAMILIES,
        &INTERNAL_ACTION_CAP_FAMILIES[..],
        "INTERNAL_ACTION_FAMILIES grew without a budget row; update docs/log-volume-guard.md"
    );
    assert_eq!(
        metrics::INTERNAL_ACTION_OUTCOMES,
        &INTERNAL_ACTION_CAP_OUTCOMES[..],
        "INTERNAL_ACTION_OUTCOMES grew without a budget row; update docs/log-volume-guard.md"
    );
}

/// The per-dispatch hot path: the observer in `gateway_metrics.rs` and the
/// pipeline in `dispatch.rs`. Both must stay free of `tracing`; hot
/// dispatches only bump counters.
const HOT_PATH_SOURCES: [(&str, &str); 2] = [
    ("dispatch.rs", include_str!("dispatch.rs")),
    ("gateway_metrics.rs", include_str!("gateway_metrics.rs")),
];

/// The six catalog-scoped source files, read at compile time so the suite
/// is fully offline.
const LOG_SCOPED_SOURCES: [(&str, &str); 6] = [
    ("gateway.rs", include_str!("gateway.rs")),
    ("main.rs", include_str!("main.rs")),
    ("server.rs", include_str!("server.rs")),
    ("shutdown.rs", include_str!("shutdown.rs")),
    ("jobs.rs", include_str!("jobs.rs")),
    ("voice_rooms.rs", include_str!("voice_rooms.rs")),
];

/// (file, message) for the 33 catalog session-path rows: the 31 traced
/// messages plus the 2 drain outcomes that surface as error text. Every row
/// is class `session` in the guard doc, so the table records presence, not
/// a second class column. A cataloged line without a row here fails closed.
const SESSION_LOG_CAPS: [(&str, &str); 33] = [
    (
        "gateway.rs",
        "cold resume committed; requesting voice snapshot via identify",
    ),
    (
        "gateway.rs",
        "gateway reconnect failed; Twilight will retry",
    ),
    ("gateway.rs", "gateway shard loop started"),
    ("gateway.rs", "gateway leveling dispatch failed"),
    ("gateway.rs", "gateway community facts dispatch failed"),
    ("gateway.rs", "gateway community facts dispatch timed out"),
    ("gateway.rs", "gateway onboarding job invalid"),
    ("gateway.rs", "gateway ready; checkpoint committed"),
    (
        "gateway.rs",
        "onboarding interaction interrupted; member must reselect",
    ),
    (
        "gateway.rs",
        "invite counter read unavailable; retaining snapshot",
    ),
    (
        "gateway.rs",
        "TWO_VOICE=1 but no discord token; voice rooms disabled",
    ),
    (
        "gateway.rs",
        "TWO_VOICE=1 but no database URL; voice rooms disabled",
    ),
    (
        "gateway.rs",
        "voice database unavailable; voice rooms disabled",
    ),
    ("gateway.rs", "voice rooms enabled; gateway sink attached"),
    (
        "gateway.rs",
        "voice HTTP setup failed; voice rooms disabled",
    ),
    ("main.rs", "durable gateway initialized; shard connecting"),
    (
        "main.rs",
        "gateway prerequisites missing; gateway parked, /readyz reports down",
    ),
    ("main.rs", "container service failed"),
    (
        "main.rs",
        "durable gateway failed; checkpoint unchanged, readiness unavailable",
    ),
    (
        "main.rs",
        "shutdown_deadline_exceeded: abandoning in-flight work",
    ),
    (
        "main.rs",
        "gateway task stopped; container restart required",
    ),
    ("main.rs", "gateway drain failed; restart required"),
    ("server.rs", "listening"),
    ("server.rs", "SIGTERM received; draining"),
    ("server.rs", "SIGINT received; draining"),
    ("shutdown.rs", "invalid shutdown timeout; using default"),
    (
        "shutdown.rs",
        "second shutdown signal received; exiting immediately",
    ),
    ("jobs.rs", "periodic job failed"),
    ("voice_rooms.rs", "voice_operation succeeded"),
    ("voice_rooms.rs", "voice_operation failed"),
    ("voice_rooms.rs", "voice_reconcile planned"),
    ("voice_rooms.rs", "voice action dead-lettered"),
    (
        "voice_rooms.rs",
        "voice creator orphan needs manual deletion",
    ),
];

/// A new dispatch label ships without a cap until it gets a row in
/// `EVENT_CAPS` and in the guard doc.
#[test]
fn event_caps_cover_every_metric_label() {
    assert_eq!(
        metrics::EVENTS.len(),
        EVENT_CAPS.len(),
        "metrics::EVENTS grew without a cap row; add the label to EVENT_CAPS and docs/log-volume-guard.md"
    );
    for (cap, allowed) in EVENT_CAPS.iter().zip(metrics::EVENTS.iter()) {
        assert_eq!(
            cap.label, *allowed,
            "EVENT_CAPS drifted from metrics::EVENTS order; keep both in the same order"
        );
    }
}

/// Hot events shed downstream work first and never log per event; session
/// and steady events are the watch signal and never shed.
#[test]
fn hot_events_shed_first_and_session_events_never_shed() {
    for cap in EVENT_CAPS {
        match cap.class {
            VolumeClass::Hot => assert!(
                cap.shed_order.is_some(),
                "hot event without a shed order: {}",
                cap.label
            ),
            VolumeClass::Session | VolumeClass::Steady => assert!(
                cap.shed_order.is_none(),
                "{} must never shed; it is the watch signal",
                cap.label
            ),
        }
    }
    let mut shed: Vec<u8> = EVENT_CAPS.iter().filter_map(|cap| cap.shed_order).collect();
    shed.sort_unstable();
    assert_eq!(
        shed,
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
        "hot shed orders must be unique priorities 1-9; see docs/log-volume-guard.md"
    );
    let first = EVENT_CAPS
        .iter()
        .find(|cap| cap.shed_order == Some(1))
        .expect("one event sheds first");
    assert_eq!(
        first.label, "MESSAGE_CREATE",
        "the busiest dispatch sheds its pipeline work first"
    );
}

/// A new `tracing` use on the hot path is a log-volume change, not a drive-by:
/// hot dispatches must only bump counters.
#[test]
fn hot_path_emits_no_log_lines() {
    for (file, source) in HOT_PATH_SOURCES {
        assert!(
            !source.contains("tracing"),
            "{file} gained a tracing use; record the line in docs/log-volume-guard.md and SESSION_LOG_CAPS first"
        );
    }
}

/// A new job label ships without a cap until it gets a row in `JOB_CAPS`.
/// The hourly presence probe is trend signal only, so it parks first.
#[test]
fn job_caps_cover_every_job_label_and_presence_sheds_first() {
    assert_eq!(
        metrics::JOBS.len(),
        JOB_CAPS.len(),
        "metrics::JOBS grew without a cap row; add the job to JOB_CAPS and docs/log-volume-guard.md"
    );
    for (cap, allowed) in JOB_CAPS.iter().zip(metrics::JOBS.iter()) {
        assert_eq!(
            cap.job, *allowed,
            "JOB_CAPS drifted from metrics::JOBS order; keep both in the same order"
        );
    }
    let probe = JOB_CAPS
        .iter()
        .find(|cap| cap.job == "presence_probe")
        .expect("presence_probe cap");
    assert_eq!(
        probe.shed_order,
        Some(1),
        "presence_probe parks first; it is hourly trend signal, never gateway traffic"
    );
    for cap in JOB_CAPS {
        if cap.shed_order.is_none() {
            assert!(
                matches!(
                    cap.job,
                    "invite_snapshot"
                        | "session_checkpoint"
                        | "counter"
                        | "rank"
                        | "scheduled_events"
                        | "settings"
                        | "other"
                ),
                "{} never sheds; shed it only through its feature flag, not the log guard",
                cap.job
            );
        }
    }
}

/// Bounded families stay fixed-size with an `other` collapse trapdoor, so
/// one hostile guild cannot grow the exposition.
#[test]
fn bounded_families_stay_fixed_size_with_collapse_traps() {
    assert_eq!(
        metrics::VOICE_OPERATIONS.len(),
        3,
        "voice op family grew; update the cardinality budget and the guard doc"
    );
    assert_eq!(
        metrics::VOICE_RECONCILE_ACTIONS.len(),
        4,
        "voice reconcile family grew; update the cardinality budget and the guard doc"
    );
    assert_eq!(
        metrics::VOICE_DEAD_ACTIONS.len(),
        9,
        "voice dead-letter family grew; update the cardinality budget and the guard doc"
    );
    assert_eq!(
        metrics::DB_ERROR_OPS.len(),
        2,
        "db-error family grew; update the cardinality budget and the guard doc"
    );
    assert_eq!(
        metrics::SEND_ADMISSION_OUTCOMES.len(),
        4,
        "send-admission family grew; update the cardinality budget and the guard doc"
    );
    assert_eq!(
        metrics::PREFIX_TRIGGER_REFUSED_REASONS.len(),
        2,
        "prefix-refused family grew; update the cardinality budget and the guard doc"
    );
    assert_eq!(
        metrics::DISPATCH_LANES.len(),
        6,
        "dispatch-lane family grew; update the cardinality budget and the guard doc"
    );
    assert_eq!(
        metrics::VOICE_VOTE_KICK_OUTCOMES.len(),
        26,
        "vote-kick outcome family grew; update the cardinality budget and the guard doc"
    );
    assert_eq!(
        metrics::CHECKPOINT_FAILURE_STAGES.len(),
        2,
        "checkpoint-failure family grew; update the cardinality budget and the guard doc"
    );
    assert_eq!(
        metrics::INTERNAL_ACTION_FAMILIES.len(),
        6,
        "internal-action family grew; update the cardinality budget and the guard doc"
    );
    assert_eq!(
        metrics::INTERNAL_ACTION_OUTCOMES.len(),
        12,
        "internal-action outcome family grew; update the cardinality budget and the guard doc"
    );
    assert_eq!(
        metrics::COMMUNITY_FACTS_DRAIN_REASONS.len(),
        3,
        "community-facts drain-reason family grew; update the cardinality budget and the guard doc"
    );
    for (allowlist, name) in [
        (metrics::EVENTS, "EVENTS"),
        (metrics::REST_ROUTES, "routes"),
        (metrics::JOBS, "JOBS"),
        (metrics::VOICE_DEAD_ACTIONS, "dead-letter"),
        (metrics::DB_ERROR_OPS, "db-errors"),
        (metrics::SEND_ADMISSION_OUTCOMES, "send-admissions"),
        (metrics::VOICE_VOTE_KICK_OUTCOMES, "vote-kick"),
        (metrics::VOICE_NAME_OUTCOMES, "voice-names"),
        (
            metrics::PREFIX_TRIGGER_REFUSED_REASONS,
            "prefix-trigger-refused",
        ),
        (
            metrics::INTERNAL_ACTION_FAMILIES,
            "internal-action families",
        ),
        (
            metrics::COMMUNITY_FACTS_DRAIN_REASONS,
            "community-facts drain reasons",
        ),
    ] {
        assert_eq!(
            allowlist.last(),
            Some(&"other"),
            "{name} lost its `other` collapse trapdoor; unknowns must never become series"
        );
    }
    assert_eq!(
        metrics::INTERNAL_ACTION_OUTCOMES.last(),
        Some(&"internal"),
        "internal-action outcomes lost its `internal` collapse trapdoor; unknowns must never become series"
    );
}

/// Any new series anywhere in the exposition fails here until the
/// cardinality budget in the guard doc is updated with it.
#[test]
fn exposition_series_count_matches_the_cardinality_budget() {
    assert_eq!(metrics::EVENTS.len(), 21, "event family changed the budget");
    assert_eq!(
        metrics::REST_ROUTES.len(),
        26,
        "route family changed the budget"
    );
    assert_eq!(metrics::JOBS.len(), 12, "job family changed the budget");
    assert_eq!(
        metrics::PREFIX_TRIGGER_REFUSED_REASONS.len(),
        2,
        "prefix-refused family changed the budget"
    );
    assert_eq!(
        metrics::DISPATCH_LANES.len(),
        6,
        "dispatch-lane family changed the budget"
    );
    let text = metrics::Metrics::default().render(None);
    let help_headers = text
        .lines()
        .filter(|line| line.starts_with("# HELP "))
        .count();
    let type_headers = text
        .lines()
        .filter(|line| line.starts_with("# TYPE "))
        .count();
    assert_eq!(help_headers, 31, "rendered HELP family count changed");
    assert_eq!(type_headers, 31, "rendered TYPE family count changed");
    let series = text.lines().filter(|line| !line.starts_with('#')).count();
    assert_eq!(
        series, 489,
        "exposition grew past the 489-sample budget (21 events + 4 scalars + 1 latency \
         + 11 histogram + 156 rest + 48 jobs + 84 job-last-error-class + 31 voice + 26 vote-kick + 12 voice-names + 2 db-errors + 4 send-admissions + 2 prefix-refused + 6 dispatch-drops + 2 checkpoint-failures + 72 internal-actions + 3 facts-drain + 4 pool); \
         update docs/log-volume-guard.md with the new series"
    );
}

/// Every catalog session-path line is presence-checked in its documented
/// file, so a moved or deleted emission is caught with its callsite.
#[test]
fn every_catalog_session_line_has_a_cap() {
    for (file, message) in SESSION_LOG_CAPS {
        let source = LOG_SCOPED_SOURCES
            .iter()
            .find(|(name, _)| *name == file)
            .expect("cap names a catalog-scoped file")
            .1;
        assert!(
            source.contains(message),
            "catalog line missing from {file}: {message}"
        );
    }
}
