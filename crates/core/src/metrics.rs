//! Fixed-cardinality, process-local metrics; no background task or retained payloads.
//! Text format: <https://prometheus.io/docs/instrumenting/exposition_formats/#text-format-details>

use serde::Serialize;
use std::{
    collections::HashMap,
    fmt::Write,
    sync::{
        atomic::{AtomicU64, Ordering},
        LockResult, Mutex, MutexGuard, OnceLock,
    },
    time::Duration,
};

/// Scrape-time input, not a second retained guard or counter registry.
#[derive(Debug, Clone, Copy, Default)]
pub struct RestGuardSnapshot {
    pub invalid_requests_in_window: usize,
    pub invalid_requests_total: u64,
    pub rejected_requests_total: u64,
    pub breaker_opens_total: u64,
    pub breaker_closes_total: u64,
    pub global_pauses_total: u64,
    pub breaker_open: bool,
    pub token_invalid: bool,
    pub global_pause_remaining: Duration,
    pub pending_global_responses: usize,
}

pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Unix seconds; 0 when the clock is unreadable (never in practice).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Point-in-time gateway roll-up for the read-only `/ops/gateway` JSON
/// endpoint (TOG-22860). Fixed vocabulary: event and stage names come from
/// the same allowlists as the Prometheus exposition, so a rename there
/// renames here. No IDs, tokens or bodies are retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GatewaySnapshot {
    pub reconnects: u64,
    pub resumes: u64,
    pub disconnects: u64,
    pub missed_events: u64,
    /// `(event, count)` pairs in [`EVENTS`] order.
    pub events: Vec<(String, u64)>,
    /// `(stage, count)` pairs in [`CHECKPOINT_FAILURE_STAGES`] order.
    pub checkpoint_failures: Vec<(String, u64)>,
    /// Unix seconds of the last observed READY dispatch, if any.
    pub last_ready_unix: Option<u64>,
    /// Unix seconds of the last observed RESUME dispatch, if any.
    pub last_resume_unix: Option<u64>,
}
pub const EVENTS: &[&str] = &[
    "READY",
    "RESUMED",
    "GUILD_CREATE",
    "GUILD_DELETE",
    "GUILD_UPDATE",
    "GUILD_MEMBER_ADD",
    "GUILD_MEMBER_REMOVE",
    "GUILD_MEMBER_UPDATE",
    "MESSAGE_CREATE",
    "MESSAGE_UPDATE",
    "MESSAGE_DELETE",
    "MESSAGE_REACTION_ADD",
    "MESSAGE_REACTION_REMOVE",
    "VOICE_STATE_UPDATE",
    "PRESENCE_UPDATE",
    "INVITE_CREATE",
    "INVITE_DELETE",
    "INTERACTION_CREATE",
    "HEARTBEAT_ACK",
    "GATEWAY_CLOSE",
    "other",
];
pub const REST_ROUTES: &[&str] = &[
    "GET /channels/:channel",
    "GET /channels/:channel/messages",
    "GET /guilds/:guild",
    "GET /guilds/:guild/members",
    "GET /guilds/:guild/scheduled-events",
    "DELETE /guilds/:guild/bans/:member",
    "DELETE /channels/:channel/permissions/:overwrite",
    "PUT /applications/:application/commands",
    "PUT /applications/:application/guilds/:guild/commands",
    "POST /interactions/:interaction/:token/callback",
    "POST /channels/:channel/messages",
    "DELETE /channels/:channel/messages/:message",
    "DELETE /guilds/:guild/members/:member",
    "PUT /guilds/:guild/bans/:member",
    "PATCH /guilds/:guild/members/:member",
    "PATCH /channels/:channel",
    "PUT /channels/:channel/voice-status",
    "PUT /channels/:channel/permissions/:overwrite",
    "POST /channels/:channel/messages/bulk-delete",
    "PUT /guilds/:guild/members/:member/roles/:role",
    "DELETE /guilds/:guild/members/:member/roles/:role",
    "POST /guilds/:guild/scheduled-events",
    "PATCH /guilds/:guild/scheduled-events/:event",
    "DELETE /guilds/:guild/scheduled-events/:event",
    "POST /guilds/:guild/channels",
    "DELETE /channels/:channel",
    "GET /guilds/:guild/members/:member",
    "GET /users/@me",
    "PATCH /webhooks/:application/:token/messages/@original",
    "other",
];
pub const RESULTS: &[&str] = &["2xx", "3xx", "4xx", "429", "5xx", "transport"];
pub const JOBS: &[&str] = &[
    "invite_snapshot",
    "session_checkpoint",
    "counter",
    "rank",
    "scheduled_events",
    "settings",
    "presence_probe",
    "community_scorecard",
    "inactivity",
    "audit_retry",
    "scheduled_messages",
    "feeds",
    "self_role_recovery",
    "member_unban_sweep",
    "automod_expiry",
    "lfg_sweep",
    "event_voice",
    "other",
];
pub const JOB_OUTCOMES: &[&str] = &["success", "failure"];

/// Fixed, payload-free classes for the last completed job failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobErrorClass {
    Database,
    Rest,
    Configuration,
    Timeout,
    Panic,
    Feed,
    RecoveryRequired,
}

impl JobErrorClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Database => "database",
            Self::Rest => "rest",
            Self::Configuration => "configuration",
            Self::Timeout => "timeout",
            Self::Panic => "panic",
            Self::Feed => "feed",
            Self::RecoveryRequired => "recovery_required",
        }
    }
}

/// Complete error-class allowlist. With `JOBS`, this yields 126 fixed series.
pub const JOB_ERROR_CLASSES: &[JobErrorClass] = &[
    JobErrorClass::Database,
    JobErrorClass::Rest,
    JobErrorClass::Configuration,
    JobErrorClass::Timeout,
    JobErrorClass::Panic,
    JobErrorClass::Feed,
    JobErrorClass::RecoveryRequired,
];
/// Room lifecycle operations (TOG-13543): creator-channel create/move/delete
/// outcomes only. Retries (429/backoff) are not outcomes.
pub const VOICE_OPERATIONS: &[&str] = &["create", "move", "delete"];
pub const VOICE_OUTCOMES: &[&str] = &[
    "success",
    "category_full",
    "discord",
    "persistence",
    "refused",
    // Last: unknown outcomes fold here (bounded_index).
    "cancelled",
];
/// Reconcile plan actions (TOG-13543): what one `reconcile` pass enqueued.
pub const VOICE_RECONCILE_ACTIONS: &[&str] = &[
    "delete_enqueued",
    "suspended",
    "resumed",
    "succession_enqueued",
];
/// Occupancy-at-join buckets for `two_bot_voice_join_occupancy_total`
/// (seeding TEST): joins by humans already present in the target channel when
/// the join arrived, capped at `7+`. Fixed vocabulary, no channel/member/time
/// labels: per-channel/hour curves are sliced downstream from log timestamps
/// (see `docs/voice-occupancy-curve.md`).
pub const VOICE_JOIN_OCCUPANCY_BUCKETS: [&str; 8] = ["0", "1", "2", "3", "4", "5", "6", "7+"];
/// Dead-lettered queue actions by bounded family (TOG-13543). Companion
/// grants/revokes share `companion`; all unknown shapes share `other`.
pub const VOICE_DEAD_ACTIONS: &[&str] = &[
    "create",
    "move",
    "delete",
    "companion",
    "ownership",
    "kick",
    "rename",
    "limit",
    "other",
];
/// Storage-failure operations: `admission` is send-admission SQL
/// (admit/extend/complete). Every other store reports `other` until its op
/// joins this allowlist.
pub const DB_ERROR_OPS: &[&str] = &["admission", "other"];
/// Containment-store operations for
/// `two_bot_containment_store_errors_total{op}`: a failed completion also
/// surfaces as outcome-unknown in the staff alert, so the alert and the
/// durable record never silently disagree. Unknown ops collapse to `other`.
pub const CONTAINMENT_STORE_OPS: &[&str] = &["complete_incident", "other"];
/// Terminal outcomes of one send-admission `admit()` decision:
/// `admitted` on Ok, `blocked` on lane/cooldown refusal, `storage_error`
/// when the admission SQL itself failed (also counted in
/// `two_bot_db_errors_total{op="admission"}`); anything else collapses to
/// `other`. Failed `complete()`/`extend()` storage writes count only in
/// db_errors: the admit decision was already recorded.
pub const SEND_ADMISSION_OUTCOMES: &[&str] = &["admitted", "blocked", "storage_error", "other"];
/// Verdict-refused prefix triggers (TOG-19932): a prefix candidate refused by
/// the automod verdict before any trigger lookup. `verdict` is the only
/// production reason today; unknown reasons collapse to `other`. Unmatched
/// content (no trigger) never increments this family.
pub const PREFIX_TRIGGER_REFUSED_REASONS: &[&str] = &["verdict", "other"];
/// Automatic room-name decisions for `two_bot_voice_names_total{outcome}`:
/// a room created already carrying its template name; each template render
/// (`proposed`, `unchanged`, `waiting_for_name`, `blocked`, `channel_unseen`);
/// and each queued rename at dispatch (`rename_sent`, or why it was dropped
/// or held, including `rename_duplicate` for a flush-time folded duplicate).
/// `name_blocked` records a dispatch-time rename or custom-override refusal.
/// Unknown outcomes collapse to `other`.
pub const VOICE_NAME_OUTCOMES: &[&str] = &[
    "created_with_template",
    "proposed",
    "unchanged",
    "waiting_for_name",
    "blocked",
    "channel_unseen",
    "rename_sent",
    "rename_room_gone",
    "rename_superseded",
    "rename_already_applied",
    "rename_unseen",
    "rename_no_access",
    "rename_held",
    "rename_duplicate",
    "name_blocked",
    "other",
];
/// Creator-join accept refusals for `two_bot_voice_join_refused_total{reason}`:
/// every actor-received creator join that never becomes a queued CreateRoom.
/// Unknown reasons collapse to the trailing `other`.
pub const VOICE_JOIN_REFUSAL_REASONS: &[&str] = &[
    "halted",
    "access",
    "unknown_creator",
    "stale_ticket",
    "duplicate",
    "name_blocked",
    "other",
];
/// Rendered `le` labels for `two_bot_voice_join_to_move_seconds_bucket`: the
/// eight finite `BUCKETS_MICROS` bounds as seconds plus `+Inf`. The rendered
/// exposition is tested against this list, and the Worker scrape validator
/// (`wrangler/src/metrics-scrape.ts`) and its fixture are checked against it.
pub const VOICE_JOIN_TO_MOVE_BUCKET_LE: &[&str] = &[
    "0.001", "0.005", "0.01", "0.05", "0.1", "0.5", "1", "5", "+Inf",
];
/// Action-queue lanes for the `two_bot_voice_queue_pending{lane}` gauge.
pub const VOICE_QUEUE_LANES: &[&str] = &["urgent", "deferred"];
/// Vote-kick outcomes for `two_bot_voice_vote_kick_total{outcome}` (M4.30):
/// one `started` per successful `kick_start`, one refusal code per refused
/// `kick_start` (the worker-level `evidence_unavailable` / `not_a_room` plus
/// every `voice_vote_kick_audit::refusal_outcome` code, including `cooldown`
/// and `initiator_limited`), and one enforcement code per terminal
/// `KickMember` dispatch (`EnforcementOutcome::as_str`). Unknown outcomes
/// collapse to the trailing `other`, never to a dynamic label. No IDs,
/// tokens or bodies are retained.
pub const VOICE_VOTE_KICK_OUTCOMES: &[&str] = &[
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
/// Event-voice tick outcomes for `two_bot_event_voice_tick_total{outcome}`:
/// one observation per T-15 ticker tick that stays TBD, with the website
/// reason (`EventVoiceTbdReason::as_str`), including backoff-skipped ticks.
/// The ticker card extends this same series with its ready/create family.
/// Unknown outcomes collapse to the trailing `other`, never to a dynamic
/// label. No IDs, tokens or bodies are retained.
pub const EVENT_VOICE_TICK_OUTCOMES: &[&str] = &[
    "opens_soon",
    "rate_limited",
    "perm_denied",
    "no_category",
    "unavailable",
    "other",
];
/// Moderation enforcement verbs for `two_bot_moderation_enforcements_total{verb,outcome}`
/// (TOG-28482): exactly the nine `ModerationAction::command_name` verbs in
/// legacy publish order. A test pins this list to
/// `ModerationAction::ALL.map(command_name)` so a new verb fails the suite
/// until the budget rows join it. Unknown verbs collapse to the trailing
/// slot only when the allowlist grows; today every caller passes a real
/// verb, so no user id, reason text or free-form detail ever becomes a label.
pub const MODERATION_VERBS: &[&str] = &[
    "ban", "tempban", "kick", "timeout", "warn", "purge", "slowmode", "lockdown", "unlock",
];
/// Terminal outcomes for `two_bot_moderation_enforcements_total{verb,outcome}`
/// (TOG-28482): `success` for every completed enforcement (including
/// idempotent replays served without a second effect), `refused` for
/// policy/validation/definitive rejections that prove no mutation happened,
/// `discord_error` for uncertain Discord failures (timeout, unreachable,
/// rate-limited), `storage_error` for persistence failures, and `other` for
/// anything else. Unknown outcomes collapse to the trailing `other`, never
/// to a dynamic label. No IDs, tokens or bodies are retained.
pub const MODERATION_ENFORCEMENT_OUTCOMES: &[&str] = &[
    "success",
    "refused",
    "discord_error",
    "storage_error",
    "other",
];
/// Dispatch-lane names for `two_bot_dispatch_drops_total{lane}`, in the bot's
/// `DISPATCH_LIMITS` order (messages, interactions, registry, privileged,
/// busy, reactions). Length must equal the lane count; unknown names collapse
/// to the trailing `reactions` slot only when the allowlist grows, never to a
/// dynamic label.
///
/// Saturation-drop policy (TOG-19878): the counter increments on **every**
/// saturation drop in `spawn_first`, including the busy-lane path. Logs emit
/// at most one `warn!` per 60 s per runtime (the first drop in the window);
/// a burst of N drops is therefore O(1) log lines with N counter
/// increments. Alert-threshold hook for M2.1: alert when any lane's drop rate
/// is sustained above zero across consecutive scrapes (exact rule lands with
/// M2.1 once TOG-18943 unblocks); a single drop during a burst is not paging.
pub const DISPATCH_LANES: &[&str] = &[
    "messages",
    "interactions",
    "registry",
    "privileged",
    "busy",
    "reactions",
];
/// Checkpoint-failure stages for `two_bot_gateway_checkpoint_failures_total{stage}`.
/// `pre_commit` is a commit skipped after a funnel/leveling/acknowledgement
/// failure held in `dispatch_error` or `acknowledgement_held`; `commit` is the
/// durable store write itself failing. Every failure stops the dispatch worker
/// and is recorded on `operation`; the counter is the alertable signal.
/// Failure causes are never labels (no reason strings); unknown stages collapse
/// to the trailing `commit` slot only when the allowlist grows.
///
/// Alert-threshold hook for M2.1: alert on any increase across consecutive
/// scrapes (exact rule lands with M2.1); unlike bursty dispatch drops, a
/// single checkpoint failure stops the worker, so there is no
/// benign-singleton case.
pub const CHECKPOINT_FAILURE_STAGES: &[&str] = &["pre_commit", "commit"];

/// Fixed reasons a raid/join-risk observer could not enqueue a join.
#[derive(Clone, Copy)]
pub enum JoinQueueDropReason {
    QueueFull,
    QueueClosed,
}

const JOIN_QUEUE_DROP_REASONS: [(&str, JoinQueueDropReason); 2] = [
    ("queue_full", JoinQueueDropReason::QueueFull),
    ("queue_closed", JoinQueueDropReason::QueueClosed),
];
/// Drop-guard names for `two_bot_drop_poison_total{guard}`: the seven guards
/// whose `Drop` bodies recover a poisoned lock instead of panicking (which
/// would abort the process when the drop runs during unwinding).
/// `command_dispatch_guard` stops the command task scope,
/// `schedule_run` finishes a feed poll schedule, `user_slot` releases an
/// interaction admission count, `in_flight_guard` releases a voice REST
/// attempt slot, `ticket_supervisor` stops the ticket task scope,
/// `response_accounting` retires a REST response's rate-limit entries, and
/// `voice_connection_guard` disconnects the voice sink. Exactly these seven
/// series exist; unknown guards collapse to the trailing
/// `voice_connection_guard` slot only when the allowlist grows.
/// Panic payloads are never labels.
pub const DROP_POISON_GUARDS: &[&str] = &[
    "command_dispatch_guard",
    "schedule_run",
    "user_slot",
    "in_flight_guard",
    "ticket_supervisor",
    "response_accounting",
    "voice_connection_guard",
];
/// Presence-capture outcomes for `two_bot_presence_capture_total{outcome}`
/// (event-store S3): `inserted` for a first delivery that stored a row,
/// `duplicate` for a same-key retry absorbed by the shared dedup key, and
/// `rejected` for a payload refused before the insert. Exactly these three
/// series exist — presence is the highest-volume family, so the outcome label
/// carries no user id, activity name, application id or any free text, and an
/// unrecognised outcome collapses to the last entry (`rejected`) rather than
/// growing the series vocabulary.
pub const PRESENCE_CAPTURE_OUTCOMES: &[&str] = &["inserted", "duplicate", "rejected"];
/// Terminal automod decision outcomes for
/// `two_bot_automod_decisions_total{outcome}`. Mirrors
/// `automod_runtime::CompletionKind::as_str` in declaration order: `accepted`
/// for clean content, `already_processed` for a counted retry, `dry_run` for
/// a matched dry-run capture, `protected` for an exempt target, `deleted` /
/// `warned` / `timed_out` for enforced ladders, and `sanction_refused` when
/// the delete-adjacent sanction could not run. Exactly eight series from
/// process start; no message text, excerpts or user IDs ever become labels.
/// Unknown outcomes collapse to the trailing `sanction_refused` slot only
/// when the allowlist grows; today every caller passes a member, so that
/// slot stays zero unless hostile input arrives.
pub const AUTOMOD_DECISION_OUTCOMES: &[&str] = &[
    "accepted",
    "already_processed",
    "dry_run",
    "protected",
    "deleted",
    "warned",
    "timed_out",
    "sanction_refused",
];
/// Website-action receiver families for
/// `two_bot_internal_actions_total{family,outcome}` (TOG-20119, roadmap
/// M4.23). The signed receiver (`crates/bot/src/internal_action_http.rs`)
/// maps each bounded `action` verb to one family: `announcement.post` to
/// `announcement`, `event.*` to `event`, `settings.*` to `settings`,
/// `moderation.*` to `moderation`, `role.assign`/`guild.add_member` to
/// `membership`; anything else (including unwired catalog verbs and unknown
/// verbs) collapses to `other`. No key id, token, body or request bytes ever
/// become a family label.
///
/// Alert rule `receiver_refusals:<family>` in `wrangler/src/alert-rules.ts`:
/// refused outcomes rising in 3 consecutive scrapes per family pages; the
/// first sample and restarts clear the streak, and one forged pre-auth
/// probe (family `other`) stays silent.
pub const INTERNAL_ACTION_FAMILIES: &[&str] = &[
    "announcement",
    "event",
    "settings",
    "moderation",
    "membership",
    "other",
];
/// Receiver outcomes for `two_bot_internal_actions_total{family,outcome}`:
/// `executed` for every terminal success envelope (including idempotent
/// replays served without a second effect), otherwise the bounded refusal
/// class matching `RejectionClass::as_str` (`auth_failure`, `unknown_key`,
/// `clock_skew`, `nonce_replay`, `rate_limit`, `unknown_action`,
/// `action_disabled`, `malformed_body`, `conflict`, `upstream`, `internal`).
/// Each request counts exactly once. No secret, token, key id or raw request
/// bytes ever become an outcome label.
pub const INTERNAL_ACTION_OUTCOMES: &[&str] = &[
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
/// Reasons for `two_bot_community_facts_drain_failures_total{reason}`:
/// `error` when the facts writer failed, `timeout` when the drain missed the
/// dispatch deadline. Both warn and continue; `other` absorbs unknown reasons.
pub const COMMUNITY_FACTS_DRAIN_REASONS: &[&str] = &["error", "timeout", "other"];
/// Verified EventSub deliveries by bounded receiver classification. The
/// thirteen `EventSubError` snake_case labels record classify errors by
/// variant ([`crate::twitch_eventsub::eventsub_error_label`]); `other` stays
/// the trailing entry absorbing unknown/future strings. No header, body,
/// broadcaster, subscription, or secret data reaches the label.
pub const EVENTSUB_DELIVERY_KINDS: [&str; 19] = [
    "challenge",
    "notification",
    "revocation",
    "tampered",
    "ignored",
    "invalid_secret",
    "invalid_header",
    "body_too_large",
    "bad_signature",
    "invalid_timestamp",
    "stale_timestamp",
    "invalid_body",
    "duplicate_keys",
    "wrong_subscription",
    "invalid_challenge",
    "invalid_callback",
    "invalid_broadcaster_id",
    "too_many_broadcasters",
    "other",
];
/// Go-live post attempts by bounded outcome. The outcome vocabulary is fixed;
/// post identifiers and error details are never retained.
pub const EVENTSUB_POST_OUTCOMES: [&str; 4] = ["attempted", "succeeded", "failed", "other"];
/// Feed-fetch attempts by bounded outcome (TOG-28423): `success` for a 2xx
/// fetch, `invalid_source` for a source refused before dialling (bad URL or
/// non-public address), `refused_redirect` for a redirect refused by policy,
/// and `failure` for every other fetch failure (including non-2xx HTTP).
/// Per-outcome, never per-feed: no source URL, host or feed id reaches a label.
pub const FEED_FETCH_OUTCOMES: &[&str] =
    &["success", "invalid_source", "refused_redirect", "failure"];
/// Feed-fetch failure causes for `two_bot_feed_fetch_errors_total{error_class}`
/// (TOG-28423). The vocabulary mirrors [`crate::feeds_http::FetchError`] plus
/// the connector layers (`resolve`, `transport`, `deadline`,
/// `unsupported_encoding`) and `http_error` for a completed non-2xx fetch.
/// Unknown classes collapse to the trailing `other`. No URL, host, header or
/// body bytes ever reach a label.
pub const FEED_FETCH_ERROR_CLASSES: &[&str] = &[
    "invalid_source",
    "non_public_address",
    "redirect_refused",
    "too_large",
    "invalid_encoding",
    "unsupported_content_type",
    "unsupported_encoding",
    "resolve",
    "transport",
    "deadline",
    "http_error",
    "other",
];
/// Outcomes of one EventSub reconcile pass for
/// `two_bot_eventsub_reconcile_passes_total{outcome}`: `healthy` when every
/// desired broadcaster holds a healthy subscription afterwards, `pending` when
/// re-created but not all verified yet, `failed` when an action failed or a
/// healthy subscription is missing (including listing failures). Exactly these
/// three series exist; an unrecognised outcome collapses to the trailing
/// `failed` (fail-closed) rather than growing the series vocabulary.
pub const EVENTSUB_RECONCILE_OUTCOMES: &[&str] = &["healthy", "pending", "failed"];
/// Successful reconcile mutations for
/// `two_bot_eventsub_reconcile_actions_total{action}`: 2xx creates and deletes
/// (already-gone deletes count). 409 already-exists confirmations fold into the
/// pass outcome, not into actions. Unknown actions collapse to the trailing
/// `delete` slot only when the allowlist grows.
pub const EVENTSUB_RECONCILE_ACTIONS: &[&str] = &["create", "delete"];
/// Failure sites for `two_bot_eventsub_reconcile_errors_total{error_class}`:
/// the failure-site `error_class` log strings in `eventsub_reconcile`
/// (`reconcile_action_failed`, `reconcile_prior_failed`, `reconcile_unhealthy`,
/// `reconcile_failed`), so those logs and metrics join without response bytes.
/// `reconcile_pending`, config refusals and parked passes stay log-only. No
/// status code, body, broadcaster, subscription id or secret ever becomes a
/// label. Unknown classes collapse to the trailing `other`.
pub const EVENTSUB_RECONCILE_ERROR_CLASSES: &[&str] = &[
    "reconcile_action_failed",
    "reconcile_prior_failed",
    "reconcile_unhealthy",
    "reconcile_failed",
    "other",
];
/// Ticket lifecycle events for `two_bot_ticket_lifecycle_total{event,outcome}`:
/// every retire, abandon, purge and successful staff-button claim/close
/// executed through the button path counts once. The vocabulary is closed by
/// the lifecycle observability contract (see
/// `docs/ticket-lifecycle-observability.md`): every caller passes a member,
/// so no collapse traffic lands in the trailing `close` slot and that row
/// counts genuine close events only.
pub const TICKET_LIFECYCLE_EVENTS: &[&str] = &["retire", "abandon", "purge", "claim", "close"];
/// Terminal outcomes for `two_bot_ticket_lifecycle_total{event,outcome}`:
/// the six success outcomes plus the shared `db_cause` vocabulary for
/// storage failures (`crate::error_chain::db_cause`). Labels never carry
/// member text, channel names, ticket ids or error strings. The trailing
/// `query` slot is the collapse trapdoor, matching `db_cause` itself, which
/// maps unrecognised failures to `query`.
pub const TICKET_LIFECYCLE_OUTCOMES: &[&str] = &[
    "retired",
    "abandoned",
    "already_gone",
    "deleted",
    "claimed",
    "closed",
    "unique_violation",
    "foreign_key_violation",
    "not_null_violation",
    "check_violation",
    "exclusion_violation",
    "database",
    "pool_timeout",
    "pool_closed",
    "worker_crashed",
    "connection",
    "row_not_found",
    "query",
];
const BUCKETS_MICROS: &[u64] = &[
    1_000, 5_000, 10_000, 50_000, 100_000, 500_000, 1_000_000, 5_000_000,
];

fn bounded_index(value: &str, allowed: &[&str]) -> usize {
    allowed
        .iter()
        .position(|candidate| *candidate == value)
        .unwrap_or(allowed.len() - 1)
}

/// Lifecycle exposition renders per-event outcome subsets, not the full 5 x
/// 18 cross product: 90 zero series would not fit the Worker scrape body cap
/// (see `rest_guard_snapshot_is_fixed_size_label_free_and_finite`). Every
/// event renders its success outcomes plus the `query` collapse trapdoor so
/// collapsed traffic stays observable; purge additionally renders the full
/// `db_cause` set because it is the only storage-failure emitter. Storage
/// keeps the flat vocabulary, so every collapse still lands in-bounds;
/// exposition just omits the pairs no emitter can produce. Every allowlisted
/// value still renders at least once (pinned by
/// `crates/core/tests/metrics_docs.rs`).
fn lifecycle_series_rendered(event: &str, outcome: &str) -> bool {
    // The `query` slot is the collapse trapdoor for unknown outcomes in
    // every row; render it so collapse traffic stays observable.
    if outcome == "query" {
        return true;
    }
    match event {
        "retire" => outcome == "retired",
        "abandon" => matches!(outcome, "abandoned" | "already_gone"),
        // Purge is the only storage-failure emitter: `deleted` plus the
        // trailing `db_cause` classes (`TICKET_LIFECYCLE_OUTCOMES[6..]`).
        "purge" => outcome == "deleted" || TICKET_LIFECYCLE_OUTCOMES[6..].contains(&outcome),
        "claim" => outcome == "claimed",
        "close" => outcome == "closed",
        _ => false,
    }
}

#[derive(Default)]
struct Histogram {
    buckets: [u64; BUCKETS_MICROS.len()],
    count: u64,
    sum_micros: u64,
}

impl Histogram {
    fn observe(&mut self, duration: Duration) {
        let micros = duration.as_micros().min(u64::MAX as u128) as u64;
        self.count = self.count.saturating_add(1);
        self.sum_micros = self.sum_micros.saturating_add(micros);
        for (bucket, bound) in self.buckets.iter_mut().zip(BUCKETS_MICROS) {
            if micros <= *bound {
                *bucket = bucket.saturating_add(1);
            }
        }
    }

    fn render(&self, out: &mut String, name: &str, help: &str) {
        header(out, name, "histogram", help);
        for (bucket, bound) in self.buckets.iter().zip(BUCKETS_MICROS) {
            writeln!(
                out,
                "{name}_bucket{{le=\"{}\"}} {bucket}",
                *bound as f64 / 1_000_000.0
            )
            .unwrap();
        }
        writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {}", self.count).unwrap();
        writeln!(out, "{name}_sum {}", self.sum_micros as f64 / 1_000_000.0).unwrap();
        writeln!(out, "{name}_count {}", self.count).unwrap();
    }
}

#[derive(Default)]
struct JobMetrics {
    runs: [u64; JOB_OUTCOMES.len()],
    last_success: u64,
    consecutive_failures: u64,
    last_error_class: Option<JobErrorClass>,
}

/// One guild actor incarnation's stall contribution. `generation` fences
/// replacement: a report older than the recorded generation is ignored,
/// and a newer generation replaces the whole contribution.
#[derive(Clone, Copy, Default)]
struct VoiceActorStall {
    generation: u64,
    halted: bool,
    queue_pending: [u64; VOICE_QUEUE_LANES.len()],
    oldest_age_secs: u64,
    /// Seconds this actor's live view has been not ready while the gateway
    /// is connected (joins are dropped meanwhile); 0 when ready or offline.
    live_not_ready_secs: u64,
}

#[derive(Default)]
struct Values {
    events: [u64; EVENTS.len()],
    rest: [[u64; RESULTS.len()]; REST_ROUTES.len()],
    jobs: [JobMetrics; JOBS.len()],
    reconnects: u64,
    resumes: u64,
    disconnects: u64,
    missed_events: u64,
    rsvp_refused: u64,
    latency_micros: Option<u64>,
    handler: Histogram,
    voice_join_to_move: Histogram,
    voice_join_occupancy: [u64; 8],
    voice_ops: [[u64; 6]; 3],
    voice_reconcile: [u64; 4],
    voice_dead: [u64; 9],
    voice_tracked: u64,
    voice_compensation: u64,
    voice_orphans: u64,
    /// Creator joins received by the guild actor (every Join command).
    voice_joins_received: u64,
    /// Actor-received creator joins refused before queueing, by reason.
    voice_join_refused: [u64; VOICE_JOIN_REFUSAL_REASONS.len()],
    /// Refusals counted at admission, retained across actor/queue retirement.
    voice_inbox_overflow_dropped: u64,
    voice_queue_overflow_dropped: u64,
    /// Per-guild-actor stall contributions, keyed by an opaque
    /// process-local actor key that is never rendered, labelled or
    /// persisted. The public halt/queue series are aggregates over this
    /// set (any halted, per-lane sums, maximum oldest age), so a healthy
    /// actor cannot overwrite a stalled actor's signal. Entries live only
    /// while their actor does; removal deletes the entry.
    voice_actors: HashMap<u64, VoiceActorStall>,
    /// Age of the oldest in-flight voice REST attempt in seconds; 0 when idle.
    voice_rest_in_flight_age_secs: u64,
    /// Last merged creator-map generation (live snapshot generation stamped
    /// by `reload_creators`); 0 before the first merge. Last writer wins.
    creator_map_generation: u64,
    /// Seconds since the last successful creator-map merge on the actor clock;
    /// 0 when never merged. Last writer wins; the per-tick publish refreshes
    /// it so the gauge shows staleness, not the merge-time zero.
    creator_map_age_secs: u64,
    /// Global-backoff waits refused (capped) instead of parking the actor.
    voice_global_backoff_capped: u64,
    /// Actor dispatches that exceeded the per-dispatch timeout.
    voice_dispatch_timeouts: u64,
    /// Watchdog firings: pending work with no dispatch progress.
    voice_watchdog_stucks: u64,
    /// Halt-latch recoveries via credential re-probe.
    voice_halt_recoveries: u64,
    /// Fair-scheduling bound: at most this many consecutive guild-actor
    /// inbox turns before a ready dispatch, timer or maintenance branch
    /// runs. Published by each actor at startup from its single source of
    /// truth; 0 until the first actor publishes. Label-free gauge.
    voice_actor_inbox_turn_bound: u64,
    /// Guild-actor inbox commands processed, across all guild actors.
    /// Compare with dispatch turns: inbox turns climbing while dispatch
    /// turns stay flat means the actor is draining churn without
    /// dispatching. Label-free counter.
    voice_actor_inbox_turns: u64,
    /// Guild-actor dispatch turns that ran, across all guild actors,
    /// whether or not the turn progressed. Label-free counter.
    voice_actor_dispatch_turns: u64,
    /// Live views republished by the not-ready-while-connected self-heal.
    voice_live_self_heals: u64,
    voice_vote_kick: [u64; VOICE_VOTE_KICK_OUTCOMES.len()],
    event_voice_tick: [u64; EVENT_VOICE_TICK_OUTCOMES.len()],
    moderation_enforcements: [[u64; MODERATION_ENFORCEMENT_OUTCOMES.len()]; MODERATION_VERBS.len()],
    voice_names: [u64; VOICE_NAME_OUTCOMES.len()],
    db_errors: [u64; DB_ERROR_OPS.len()],
    containment_store_errors: [u64; CONTAINMENT_STORE_OPS.len()],
    send_admissions: [u64; SEND_ADMISSION_OUTCOMES.len()],
    prefix_trigger_refused: [u64; PREFIX_TRIGGER_REFUSED_REASONS.len()],
    dispatch_drops: [u64; DISPATCH_LANES.len()],
    raid_dropped_joins: [u64; JOIN_QUEUE_DROP_REASONS.len()],
    join_risk_dropped_joins: [u64; JOIN_QUEUE_DROP_REASONS.len()],
    checkpoint_failures: [u64; CHECKPOINT_FAILURE_STAGES.len()],
    gateway_drain_skips: u64,
    presence_capture: [u64; PRESENCE_CAPTURE_OUTCOMES.len()],
    feed_fetch: [u64; FEED_FETCH_OUTCOMES.len()],
    feed_fetch_errors: [u64; FEED_FETCH_ERROR_CLASSES.len()],
    automod_decisions: [u64; AUTOMOD_DECISION_OUTCOMES.len()],
    last_ready_unix: Option<u64>,
    last_resume_unix: Option<u64>,
    internal_actions: [[u64; INTERNAL_ACTION_OUTCOMES.len()]; INTERNAL_ACTION_FAMILIES.len()],
    community_facts_drain_failures: [u64; COMMUNITY_FACTS_DRAIN_REASONS.len()],
    /// Audit mirror delivery halt (M4.48): 1 while the persistent kill switch
    /// is engaged, 0 otherwise. Last writer wins; set once per readable
    /// `audit_retry` sweep from the store's `delivery_halt` read. No labels,
    /// so the vocabulary is the single series name — no actor ID, timestamp,
    /// error text or row content ever reaches exposition.
    audit_delivery_halt: u64,
    /// Skipped retention purges needing Operator replay: one per real guild
    /// removal whose purge-queue write hit the schema gap (the
    /// `retention_lifecycle` purge-skip arm). Successful queue writes and
    /// rejoin-cancel skips never increment it. Label-free counter; resets on
    /// process restart, so the runbook pairs it with the error log line. No
    /// guild id, SQLSTATE or error text ever becomes a label.
    retention_replay_needed: u64,
    /// Ticket component interactions refused before spawning because the
    /// runtime or the member was saturated. Label-free counter; resets on
    /// process restart, so the runbook pairs it with the sampled saturation
    /// log line. No member id, token or interaction id ever becomes a label.
    ticket_component_drops: u64,
    /// Event-store consumer lag (S1): `received_at - event_time` spread in
    /// seconds for the newest observed capture row. Last writer wins; 0 when
    /// no row has been observed. Single label-free series — no message,
    /// channel, user, dedup-key, content or body label ever reaches
    /// exposition.
    event_store_consumer_lag_secs: f64,
    eventsub_received: u64,
    eventsub_classified: [u64; EVENTSUB_DELIVERY_KINDS.len()],
    eventsub_posts: [u64; EVENTSUB_POST_OUTCOMES.len()],
    eventsub_reconcile_passes: [u64; EVENTSUB_RECONCILE_OUTCOMES.len()],
    eventsub_reconcile_actions: [u64; EVENTSUB_RECONCILE_ACTIONS.len()],
    eventsub_reconcile_errors: [u64; EVENTSUB_RECONCILE_ERROR_CLASSES.len()],
    ticket_lifecycle: [[u64; TICKET_LIFECYCLE_OUTCOMES.len()]; TICKET_LIFECYCLE_EVENTS.len()],
}

/// Storage is fixed-size, except the per-actor voice stall map, which is
/// bounded by the live guild-actor set. Unknown labels collapse to
/// `other`, including hostile input. Drop-poison counts live outside the
/// `Values` mutex in per-instance atomics: recording from a `Drop` body
/// must complete even while another thread holds (or poisoned) that mutex,
/// and must never block behind a scrape or an unrelated writer.
#[derive(Default)]
pub struct Metrics(Mutex<Values>, [AtomicU64; DROP_POISON_GUARDS.len()]);

pub fn global() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(Metrics::default)
}

/// Recover a drop-guard lock acquisition: on poison, count one
/// `two_bot_drop_poison_total{guard}`, log one fixed-vocabulary warn, and
/// hand back the recovered guard so the drop still releases its state.
/// Panicking inside `drop` aborts the process when the drop runs during
/// unwinding; this helper is the only poison path a `Drop` body needs.
/// `guard` must be a [`DROP_POISON_GUARDS`] member. Never panics and never
/// takes another lock: recording is a lock-free atomic increment.
pub fn recover_drop_poison<'a, T>(
    result: LockResult<MutexGuard<'a, T>>,
    guard: &str,
) -> MutexGuard<'a, T> {
    result.unwrap_or_else(|poisoned| {
        global().drop_poison(guard);
        tracing::warn!(guard, "drop guard recovered a poisoned lock");
        poisoned.into_inner()
    })
}

/// The recorded contribution for `actor`, fenced by generation: an older
/// generation gets `None` (a stale incarnation cannot erase or resurrect
/// state), a newer one first resets the whole contribution (replacement
/// retires the old incarnation).
fn voice_actor_entry(
    values: &mut Values,
    actor: u64,
    generation: u64,
) -> Option<&mut VoiceActorStall> {
    let entry = values.voice_actors.entry(actor).or_default();
    if generation < entry.generation {
        return None;
    }
    if generation > entry.generation {
        entry.generation = generation;
        entry.halted = false;
        entry.queue_pending = [0; VOICE_QUEUE_LANES.len()];
        entry.oldest_age_secs = 0;
        entry.live_not_ready_secs = 0;
    }
    Some(entry)
}

impl Metrics {
    pub fn gateway_event(&self, name: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.events[bounded_index(name, EVENTS)];
        *counter = counter.saturating_add(1);
        if name == "READY" {
            values.last_ready_unix = Some(unix_now());
        }
        if name == "RESUMED" {
            values.resumes = values.resumes.saturating_add(1);
            values.last_resume_unix = Some(unix_now());
        }
    }

    /// Copy the gateway counters for the read-only `/ops/gateway` JSON
    /// endpoint. Lock is held only for the copy; no network I/O.
    pub fn gateway_snapshot(&self) -> GatewaySnapshot {
        let values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        GatewaySnapshot {
            reconnects: values.reconnects,
            resumes: values.resumes,
            disconnects: values.disconnects,
            missed_events: values.missed_events,
            events: EVENTS
                .iter()
                .zip(values.events)
                .map(|(name, count)| ((*name).to_owned(), count))
                .collect(),
            checkpoint_failures: CHECKPOINT_FAILURE_STAGES
                .iter()
                .zip(values.checkpoint_failures)
                .map(|(stage, count)| ((*stage).to_owned(), count))
                .collect(),
            last_ready_unix: values.last_ready_unix,
            last_resume_unix: values.last_resume_unix,
        }
    }

    /// One presence-update capture decision on the hot path: `inserted`,
    /// `duplicate` or `rejected` (see `PRESENCE_CAPTURE_OUTCOMES`). The
    /// outcome is the whole label vocabulary — caller payloads never become
    /// labels — and an unrecognised outcome collapses to `rejected`.
    pub fn presence_capture(&self, outcome: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter =
            &mut values.presence_capture[bounded_index(outcome, PRESENCE_CAPTURE_OUTCOMES)];
        *counter = counter.saturating_add(1);
    }

    /// One feed-fetch attempt (TOG-28423): `success` for a 2xx fetch,
    /// `invalid_source` for a source refused before dialling,
    /// `refused_redirect` for a redirect refused by policy, `failure` for
    /// every other failure. Call once per fetch, never per feed id: the
    /// outcome is the whole label vocabulary and an unrecognised outcome
    /// collapses to `failure`.
    pub fn feed_fetch(&self, outcome: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.feed_fetch[bounded_index(outcome, FEED_FETCH_OUTCOMES)];
        *counter = counter.saturating_add(1);
    }

    /// One failed feed fetch by bounded error class (see
    /// [`FEED_FETCH_ERROR_CLASSES`]). Call once per failed fetch alongside
    /// [`Self::feed_fetch`]; successes record nothing here. Unknown classes
    /// collapse to `other`.
    pub fn feed_fetch_error(&self, error_class: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter =
            &mut values.feed_fetch_errors[bounded_index(error_class, FEED_FETCH_ERROR_CLASSES)];
        *counter = counter.saturating_add(1);
    }

    /// One terminal automod decision (see [`AUTOMOD_DECISION_OUTCOMES`]).
    /// Call once per committed [`StoredOutcome`](crate::automod_runtime::StoredOutcome):
    /// the shared activation settles exactly one receipt per inspection, so
    /// replays and retained claims never call this. The outcome is the whole
    /// label vocabulary — no message text, excerpts or user IDs ever become
    /// labels — and an unrecognised outcome collapses to `sanction_refused`.
    pub fn automod_decision(&self, outcome: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter =
            &mut values.automod_decisions[bounded_index(outcome, AUTOMOD_DECISION_OUTCOMES)];
        *counter = counter.saturating_add(1);
    }

    pub fn gateway_reconnect(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.reconnects = values.reconnects.saturating_add(1);
        // A stale ACK must not look like latency on the new connection.
        values.latency_micros = None;
    }

    /// One observed transport loss: reconnect failure, close frame, invalid
    /// session, or stream termination. The 48h watch pairs every disconnect
    /// with the matching RESUME or fresh READY; an unpaired disconnect means
    /// the gateway never came back.
    pub fn gateway_disconnect(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.disconnects = values.disconnects.saturating_add(1);
    }

    /// Dispatches Discord assigned but this process never received (sequence
    /// gaps inside one session). Zero is a no-op. Any nonzero increase over
    /// the watch window fails the zero-missed-events acceptance.
    pub fn gateway_missed_events(&self, count: u64) {
        if count == 0 {
            return;
        }
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.missed_events = values.missed_events.saturating_add(count);
    }

    /// One RSVP interaction refused at reception: the in-flight RSVP bound
    /// (`RSVP_IN_FLIGHT_LIMIT`) was exhausted, so no prepare task spawned
    /// and nothing entered the serial drain. The dispatch advances without
    /// an RSVP completion instead of wedging the funnel or checkpoint.
    pub fn gateway_rsvp_refused(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.rsvp_refused = values.rsvp_refused.saturating_add(1);
    }

    /// One fire-and-forget voice command refused by the actor inbox gate.
    /// Accepted sends and closed-channel rollbacks do not count.
    pub fn voice_inbox_overflow_dropped(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_inbox_overflow_dropped = values.voice_inbox_overflow_dropped.saturating_add(1);
    }

    /// One new voice action refused by opt-in `ActionQueue::try_enqueue`.
    /// Coalescing and must-admit `enqueue` calls do not count.
    pub fn voice_queue_overflow_dropped(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_queue_overflow_dropped = values.voice_queue_overflow_dropped.saturating_add(1);
    }

    pub fn gateway_latency(&self, latency: Duration) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .latency_micros = Some(latency.as_micros().min(u64::MAX as u128) as u64);
    }

    pub fn handler_duration(&self, duration: Duration) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .handler
            .observe(duration);
    }

    /// One adapter-visible REST attempt, including executor retries. No URL/body is retained.
    pub fn rest_response(&self, route: &str, status: Option<u16>) {
        let result = match status {
            Some(200..=299) => 0,
            Some(300..=399) => 1,
            Some(429) => 3,
            Some(400..=499) => 2,
            Some(500..=599) => 4,
            _ => 5,
        };
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.rest[bounded_index(route, REST_ROUTES)][result];
        *counter = counter.saturating_add(1);
    }

    /// Call only after successful completion, not when a job starts or is merely scheduled.
    pub fn job_success(&self, job: &str, unix_seconds: u64) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = &mut values.jobs[bounded_index(job, JOBS)];
        current.runs[0] = current.runs[0].saturating_add(1);
        current.last_success = unix_seconds;
        current.consecutive_failures = 0;
        current.last_error_class = None;
    }

    /// A completed failed attempt, including a timeout or isolated panic.
    pub fn job_failure(&self, job: &str, error_class: JobErrorClass) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let current = &mut values.jobs[bounded_index(job, JOBS)];
        current.runs[1] = current.runs[1].saturating_add(1);
        current.consecutive_failures = current.consecutive_failures.saturating_add(1);
        current.last_error_class = Some(error_class);
    }

    /// Gateway receipt of a creator join to a confirmed successful member move.
    /// No member/channel labels; failed or uncertain moves are not samples.
    pub fn voice_join_to_move(&self, elapsed: Duration) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .voice_join_to_move
            .observe(elapsed);
    }

    /// One VOICE_STATE_UPDATE join by occupancy-at-join (seeding TEST).
    /// `occupancy` is the humans already present in the target channel when
    /// the join arrived, capped into `VOICE_JOIN_OCCUPANCY_BUCKETS` (`7+`).
    /// No member/channel labels; leaves and same-channel updates are not
    /// samples (the caller only observes ticketed joins).
    pub fn voice_join_occupancy(&self, occupancy: u64) {
        let bucket = occupancy.min(7) as usize;
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.voice_join_occupancy[bucket];
        *counter = counter.saturating_add(1);
    }

    /// One finished room lifecycle outcome (TOG-13543). Call once per
    /// terminal create/move/delete: retries and rate-limit backoffs are not
    /// outcomes. Unknown `op`/`outcome` collapse to the trailing `other`
    /// slot only when the allowlists grow; today every caller passes a
    /// member, so `other` stays zero. No IDs, tokens or bodies are retained.
    pub fn voice_operation(&self, op: &str, outcome: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.voice_ops[bounded_index(op, VOICE_OPERATIONS)]
            [bounded_index(outcome, VOICE_OUTCOMES)];
        *counter = counter.saturating_add(1);
    }

    /// Add actual reconcile transitions or newly queued actions in this pass.
    /// Repeated unchanged room states contribute zero, which is a no-op.
    pub fn voice_reconcile(&self, action: &str, count: u64) {
        if count == 0 {
            return;
        }
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.voice_reconcile[bounded_index(action, VOICE_RECONCILE_ACTIONS)];
        *counter = counter.saturating_add(count);
    }

    /// One queue write exhausted `QUEUE_MAX_ATTEMPTS` (TOG-13543). Unknown
    /// families collapse to `other`.
    pub fn voice_dead_letter(&self, action: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.voice_dead[bounded_index(action, VOICE_DEAD_ACTIONS)];
        *counter = counter.saturating_add(1);
    }

    /// Current worker state for ghost verification (TOG-13543): tracked
    /// rooms plus compensation-pending orphans awaiting delete. Last writer
    /// wins; on staging (single guild) this is exact, on multi-guild it is
    /// the freshest reporter until runtimes aggregate.
    pub fn voice_state(&self, tracked: u64, compensation: u64) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_tracked = tracked;
        values.voice_compensation = compensation;
    }

    /// One vote-kick start, refusal or enforcement (M4.30). Call once per
    /// `kick_start` decision (`started` or the refusal code) and once per
    /// terminal `KickMember` enforcement (`EnforcementOutcome::as_str`).
    /// `vote_result` rows (passed/expired/cancelled) are not outcomes here.
    /// Unknown outcomes collapse to `other`.
    pub fn voice_vote_kick(&self, outcome: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.voice_vote_kick[bounded_index(outcome, VOICE_VOTE_KICK_OUTCOMES)];
        *counter = counter.saturating_add(1);
    }

    /// One event-voice tick that stayed TBD (host decision D4). Call once
    /// per preflight that returns `Waiting`, including backoff-skipped
    /// ticks, with the website reason (`EventVoiceTbdReason::as_str`).
    /// `Ready` ticks record nothing here; the ticker card extends this same
    /// series with its ready/create family. Unknown outcomes collapse to
    /// `other`.
    pub fn event_voice_tick(&self, outcome: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter =
            &mut values.event_voice_tick[bounded_index(outcome, EVENT_VOICE_TICK_OUTCOMES)];
        *counter = counter.saturating_add(1);
    }

    /// One moderation enforcement terminal outcome (TOG-28482). Call once per
    /// terminal member/channel enforcement from each dispatch site (slash
    /// member/channel runtimes and website member/channel executors): each
    /// request records exactly once, so an idempotent replay counts as
    /// `success` on its own serve while the in-flight duplicate that prompted
    /// it counts on its own terminal outcome. `verb` is a
    /// `ModerationAction::command_name` verb; `outcome` is one of
    /// [`MODERATION_ENFORCEMENT_OUTCOMES`]. Unknown verbs/outcomes collapse
    /// to the trailing slots only when the allowlists grow. No user id,
    /// reason text or free-form detail ever becomes a label.
    pub fn moderation_enforcement(&self, verb: &str, outcome: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.moderation_enforcements[bounded_index(verb, MODERATION_VERBS)]
            [bounded_index(outcome, MODERATION_ENFORCEMENT_OUTCOMES)];
        *counter = counter.saturating_add(1);
    }

    /// One automatic room-name decision (see [`VOICE_NAME_OUTCOMES`]).
    pub fn voice_name(&self, outcome: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.voice_names[bounded_index(outcome, VOICE_NAME_OUTCOMES)];
        *counter = counter.saturating_add(1);
    }

    /// One creator join received by the guild actor (Join command). Compare
    /// with create/move/refused outcomes: received persistently above handled
    /// means the actor is not processing joins.
    pub fn voice_join_received(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_joins_received = values.voice_joins_received.saturating_add(1);
    }

    /// One actor-received creator join refused before queueing (see
    /// [`VOICE_JOIN_REFUSAL_REASONS`]). Unknown reasons collapse to `other`.
    pub fn voice_join_refused(&self, reason: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter =
            &mut values.voice_join_refused[bounded_index(reason, VOICE_JOIN_REFUSAL_REASONS)];
        *counter = counter.saturating_add(1);
    }

    /// Record one guild actor's halt latch. `actor` is an opaque
    /// process-local key, never rendered. The public
    /// `two_bot_voice_worker_halted` gauge is 1 while ANY current actor
    /// is halted, so a healthy actor cannot hide another's stall.
    /// Reports from a generation older than the one recorded for the
    /// actor are ignored; a newer generation replaces that actor's
    /// whole recorded contribution.
    pub fn set_voice_actor_halted(&self, actor: u64, generation: u64, halted: bool) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = voice_actor_entry(&mut values, actor, generation) {
            entry.halted = halted;
        }
    }

    /// Record how long one guild actor's live view has been not ready while
    /// the gateway is connected (0 when ready or while the transport is
    /// down). The public `two_bot_voice_live_not_ready_seconds` gauge is the
    /// maximum across current actors, so a healthy guild cannot hide a
    /// guild whose joins are being dropped. Generation fencing matches
    /// [`Self::set_voice_actor_halted`].
    pub fn set_voice_actor_live_not_ready(&self, actor: u64, generation: u64, secs: u64) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = voice_actor_entry(&mut values, actor, generation) {
            entry.live_not_ready_secs = secs;
        }
    }

    /// One live view republished from the gateway cache by the self-heal
    /// path: it was not ready while the gateway was connected.
    pub fn voice_live_self_heal(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_live_self_heals = values.voice_live_self_heals.saturating_add(1);
    }

    /// Record one guild actor's queued depth by lane plus its oldest
    /// pending age (0 when its queue is empty). The public
    /// `two_bot_voice_queue_pending{lane}` gauges are saturating sums
    /// across current actors and
    /// `two_bot_voice_queue_oldest_age_seconds` is their maximum.
    /// Generation fencing matches [`Self::set_voice_actor_halted`].
    pub fn set_voice_actor_queue(
        &self,
        actor: u64,
        generation: u64,
        urgent: u64,
        deferred: u64,
        oldest_age_secs: u64,
    ) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = voice_actor_entry(&mut values, actor, generation) {
            entry.queue_pending = [urgent, deferred];
            entry.oldest_age_secs = oldest_age_secs;
        }
    }

    /// Retire one guild actor incarnation's stall contribution (actor
    /// shutdown / guild removal). A generation older than the recorded
    /// one must not erase a replacement actor's state; removing an
    /// unknown actor changes nothing.
    pub fn remove_voice_actor(&self, actor: u64, generation: u64) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if values
            .voice_actors
            .get(&actor)
            .is_some_and(|entry| generation >= entry.generation)
        {
            values.voice_actors.remove(&actor);
        }
    }

    /// One actor's recorded stall contribution as `(generation, halted,
    /// queue_pending by lane, oldest_age_secs)`, or `None` when the actor
    /// has no recorded contribution. Test/diagnostic seam only: the actor
    /// key is process-local and this lookup is never rendered, labelled
    /// or persisted.
    #[doc(hidden)]
    pub fn voice_actor_contribution(
        &self,
        actor: u64,
    ) -> Option<(u64, bool, [u64; VOICE_QUEUE_LANES.len()], u64)> {
        let values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_actors.get(&actor).map(|entry| {
            (
                entry.generation,
                entry.halted,
                entry.queue_pending,
                entry.oldest_age_secs,
            )
        })
    }

    /// One actor's recorded live-not-ready age in seconds, or `None` when
    /// the actor has no recorded contribution. Test/diagnostic seam only:
    /// the actor key is process-local and this lookup is never rendered,
    /// labelled or persisted. Pins the watchdog sibling's not-ready
    /// republication per actor, where the public maximum cannot.
    #[doc(hidden)]
    pub fn voice_actor_live_not_ready_secs(&self, actor: u64) -> Option<u64> {
        let values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values
            .voice_actors
            .get(&actor)
            .map(|entry| entry.live_not_ready_secs)
    }

    /// Age of the oldest in-flight voice REST attempt in seconds; 0 idles it.
    /// Last writer wins.
    pub fn set_voice_rest_in_flight_age_secs(&self, age_secs: u64) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .voice_rest_in_flight_age_secs = age_secs;
    }

    /// Creator-map freshness: last merged generation plus seconds since that
    /// merge. Last writer wins; the actor tick republishes every tick so age
    /// keeps climbing between merges instead of freezing at the merge-time 0.
    pub fn set_voice_creator_map(&self, generation: u64, age_secs: u64) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.creator_map_generation = generation;
        values.creator_map_age_secs = age_secs;
    }

    /// One global-backoff wait capped instead of parking the actor: the call
    /// returned RateLimited so the queue backs off tick-driven instead.
    pub fn voice_global_backoff_capped(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_global_backoff_capped = values.voice_global_backoff_capped.saturating_add(1);
    }

    /// One actor dispatch turn that ran past the observe bound (run to
    /// completion; counted after the fact). The queue keeps its retries.
    pub fn voice_dispatch_timeout(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_dispatch_timeouts = values.voice_dispatch_timeouts.saturating_add(1);
    }

    /// One watchdog firing: queued work with no dispatch progress inside the
    /// watchdog window.
    pub fn voice_watchdog_stuck(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_watchdog_stucks = values.voice_watchdog_stucks.saturating_add(1);
    }

    /// One halt-latch recovery: the credential re-probe succeeded and the
    /// worker resumed processing joins.
    pub fn voice_halt_recovered(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_halt_recoveries = values.voice_halt_recoveries.saturating_add(1);
    }

    /// Publish the fair-scheduling bound: at most `bound` consecutive
    /// guild-actor inbox turns before a ready dispatch, timer or
    /// maintenance branch runs. Called once per actor at startup with the
    /// actor's single source of truth; last writer wins.
    pub fn set_voice_actor_inbox_turn_bound(&self, bound: u64) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .voice_actor_inbox_turn_bound = bound;
    }

    /// One guild-actor inbox command processed.
    pub fn voice_actor_inbox_turn(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_actor_inbox_turns = values.voice_actor_inbox_turns.saturating_add(1);
    }

    /// One guild-actor dispatch turn that ran, whether or not it progressed.
    pub fn voice_actor_dispatch_turn(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_actor_dispatch_turns = values.voice_actor_dispatch_turns.saturating_add(1);
    }

    /// One untracked creator-channel orphan needing manual deletion after
    /// failed `/create` compensation (TOG-13543). No channel ID is retained.
    pub fn voice_orphan(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_orphans = values.voice_orphans.saturating_add(1);
    }

    /// One skipped retention purge needing Operator replay: a real guild
    /// removal whose purge-queue write hit the schema gap. Call exactly once
    /// from the purge-skip arm of `retention_lifecycle`; successful queue
    /// writes and rejoin-cancel skips never call this. Label-free counter;
    /// no guild id, SQLSTATE or error text ever becomes a label.
    pub fn retention_replay_needed(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.retention_replay_needed = values.retention_replay_needed.saturating_add(1);
    }

    /// One auxiliary leveling drain skipped for a missing table or grant.
    /// The checkpoint may advance without the failed awards. Label-free:
    /// SQLSTATE, member ids and error text belong nowhere in labels.
    pub fn gateway_drain_skip(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.gateway_drain_skips = values.gateway_drain_skips.saturating_add(1);
    }

    /// One ticket component interaction refused before spawning: the runtime
    /// held `MAX_TICKET_COMPONENT_IN_FLIGHT` executions or the member held
    /// the per-member cap. Call exactly once per refusal from the ticket
    /// dispatch path; admitted work never calls this. Label-free counter;
    /// no member id, token or interaction id ever becomes a label.
    pub fn ticket_component_drop(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.ticket_component_drops = values.ticket_component_drops.saturating_add(1);
    }

    /// One storage-layer failure. Unknown `op` values collapse
    /// to `other`; no error text, query or identifier is retained.
    pub fn db_error(&self, op: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.db_errors[bounded_index(op, DB_ERROR_OPS)];
        *counter = counter.saturating_add(1);
    }

    /// One containment-store failure. Unknown `op` values collapse
    /// to `other`; no error text, query or identifier is retained.
    pub fn containment_store_error(&self, op: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter =
            &mut values.containment_store_errors[bounded_index(op, CONTAINMENT_STORE_OPS)];
        *counter = counter.saturating_add(1);
    }

    /// One send-admission `admit()` terminal outcome. Call once
    /// per admit decision, not per retry or per completion. Unknown outcomes
    /// collapse to `other`.
    pub fn send_admission(&self, outcome: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.send_admissions[bounded_index(outcome, SEND_ADMISSION_OUTCOMES)];
        *counter = counter.saturating_add(1);
    }

    /// One verdict-refused prefix trigger (TOG-19932). Call once on the
    /// refusal arm only: the automod verdict refused the create before any
    /// trigger lookup. Unmatched content (no trigger) never calls this.
    /// Unknown reasons collapse to `other`.
    pub fn prefix_trigger_refused(&self, reason: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.prefix_trigger_refused
            [bounded_index(reason, PREFIX_TRIGGER_REFUSED_REASONS)];
        *counter = counter.saturating_add(1);
    }

    /// One dispatch-lane saturation drop (TOG-19878). Call once per saturated
    /// lane per dropped event from `spawn_first`: a single-lane drop increments
    /// that lane; a multi-lane attempt whose every lane is full increments each
    /// attempted lane. Unknown lanes collapse to `other` only when the
    /// allowlist grows; today every caller passes a member, so the trailing
    /// slot stays zero. No IDs, tokens or bodies are retained.
    pub fn dispatch_drop(&self, lane: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.dispatch_drops[bounded_index(lane, DISPATCH_LANES)];
        *counter = counter.saturating_add(1);
    }

    /// One join dropped before raid observation because its worker queue was
    /// full or closed. No member, guild or error text is retained.
    pub fn raid_dropped_join(&self, reason: JoinQueueDropReason) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.raid_dropped_joins[reason as usize];
        *counter = counter.saturating_add(1);
    }

    /// One join dropped before risk scoring because its worker queue was
    /// full or closed. Successful enqueueing records nothing here.
    pub fn join_risk_dropped_join(&self, reason: JoinQueueDropReason) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.join_risk_dropped_joins[reason as usize];
        *counter = counter.saturating_add(1);
    }

    /// One failed gateway checkpoint commit from `apply_dispatch`: `pre_commit`
    /// when the commit was skipped after a funnel/leveling/acknowledgement
    /// failure, `commit` when the durable store write itself failed. Call once
    /// per Err arm entry. Unknown stages collapse to `commit` only when the
    /// allowlist grows; failure causes are never labels. No IDs, tokens or
    /// bodies are retained.
    pub fn checkpoint_failure(&self, stage: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter =
            &mut values.checkpoint_failures[bounded_index(stage, CHECKPOINT_FAILURE_STAGES)];
        *counter = counter.saturating_add(1);
    }

    /// One poisoned lock recovered by a drop guard instead of panicking
    /// inside `drop` (which aborts the process during unwinding). Call once
    /// per poisoned drop, with a [`DROP_POISON_GUARDS`] member. Unknown
    /// guards collapse to `voice_connection_guard` only when the allowlist grows;
    /// panic payloads are never labels. No IDs, tokens or bodies are
    /// retained. Lock-free and saturating: safe to call from a `Drop` body
    /// while any thread holds or poisoned the `Values` mutex.
    pub fn drop_poison(&self, guard: &str) {
        let counter = &self.1[bounded_index(guard, DROP_POISON_GUARDS)];
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
            Some(count.saturating_add(1))
        });
    }

    /// One signed website-action receiver execution (TOG-20119). Call exactly
    /// once per request from the receiver: `executed` for every terminal
    /// success envelope (including idempotent replays), otherwise the bounded
    /// refusal class. Unknown families and outcomes collapse to `other` and
    /// `internal` respectively only when the allowlists grow; today every
    /// caller passes a member, so those slots stay zero unless hostile input
    /// arrives. No IDs, tokens, key ids or bodies are retained.
    pub fn internal_action(&self, family: &str, outcome: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.internal_actions[bounded_index(family, INTERNAL_ACTION_FAMILIES)]
            [bounded_index(outcome, INTERNAL_ACTION_OUTCOMES)];
        *counter = counter.saturating_add(1);
    }

    /// One warn-and-continue community-facts drain failure. Call once per
    /// failed dispatch; a committed drain records nothing. Unknown reasons
    /// collapse to `other`. No error text, SQL or identifiers are retained.
    pub fn community_facts_drain_failure(&self, reason: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.community_facts_drain_failures
            [bounded_index(reason, COMMUNITY_FACTS_DRAIN_REASONS)];
        *counter = counter.saturating_add(1);
    }

    /// One EventSub delivery accepted for verification, including a delivery
    /// whose signature will fail. Rejected requests before body intake do not
    /// count as deliveries.
    pub fn eventsub_received(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.eventsub_received = values.eventsub_received.saturating_add(1);
    }

    /// One EventSub receiver classification. Unknown values collapse to
    /// `other`; caller-controlled input can never become a label.
    pub fn eventsub_classified(&self, kind: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter =
            &mut values.eventsub_classified[bounded_index(kind, &EVENTSUB_DELIVERY_KINDS)];
        *counter = counter.saturating_add(1);
    }

    /// One go-live post outcome. Call `attempted` immediately before the send,
    /// then `succeeded` or `failed` once its result is known. Unknown outcomes
    /// collapse to `other`.
    pub fn eventsub_post(&self, outcome: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.eventsub_posts[bounded_index(outcome, &EVENTSUB_POST_OUTCOMES)];
        *counter = counter.saturating_add(1);
    }

    /// One completed EventSub reconcile pass by outcome (see
    /// [`EVENTSUB_RECONCILE_OUTCOMES`]). Call exactly once per pass from the
    /// reconcile executor: `healthy`, `pending`, or `failed`. Unknown outcomes
    /// collapse to `failed`.
    pub fn eventsub_reconcile_pass(&self, outcome: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.eventsub_reconcile_passes
            [bounded_index(outcome, EVENTSUB_RECONCILE_OUTCOMES)];
        *counter = counter.saturating_add(1);
    }

    /// Successful reconcile mutations by bounded action (`create`/`delete`),
    /// counted from the executed plan outcomes. Call with the per-pass counts;
    /// zero is a no-op. Unknown actions collapse to `delete` only when the
    /// allowlist grows. No broadcaster, subscription id or body is retained.
    pub fn eventsub_reconcile_action(&self, action: &str, count: u64) {
        if count == 0 {
            return;
        }
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.eventsub_reconcile_actions
            [bounded_index(action, EVENTSUB_RECONCILE_ACTIONS)];
        *counter = counter.saturating_add(count);
    }

    /// One reconcile failure site by log `error_class` (see
    /// [`EVENTSUB_RECONCILE_ERROR_CLASSES`]). Call once per site per pass,
    /// except `reconcile_action_failed`, which counts once per failed action.
    /// Unknown classes collapse to `other`; no status, body or response bytes
    /// ever become a label.
    pub fn eventsub_reconcile_error(&self, error_class: &str) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.eventsub_reconcile_errors
            [bounded_index(error_class, EVENTSUB_RECONCILE_ERROR_CLASSES)];
        *counter = counter.saturating_add(1);
    }

    /// One terminal ticket lifecycle event (see [`TICKET_LIFECYCLE_EVENTS`]
    /// and [`TICKET_LIFECYCLE_OUTCOMES`]). Call once per retire, abandon,
    /// and successful staff-button claim/close executed through the button
    /// path, alongside the matching `ticket_lifecycle` log line; the purge
    /// pass calls [`Self::ticket_lifecycle_n`] with its deleted count
    /// instead. Unknown events/outcomes collapse to the trailing slots only
    /// when the allowlists grow; today every caller passes a member. No
    /// member text, channel name, ticket id or error string ever becomes a
    /// label.
    pub fn ticket_lifecycle(&self, event: &str, outcome: &str) {
        self.ticket_lifecycle_n(event, outcome, 1);
    }

    /// Ticket lifecycle events counted with a pass-level multiplicity: the
    /// purge pass reports its deleted count here (one sample per deleted
    /// transcript body, not one per pass). Zero is a no-op. Unknown
    /// events/outcomes collapse to the trailing slots only when the
    /// allowlists grow. A normalized pair outside the rendered per-event
    /// subset routes to that event's `query` trapdoor before incrementing,
    /// so every call stays observable.
    pub fn ticket_lifecycle_n(&self, event: &str, outcome: &str, count: u64) {
        if count == 0 {
            return;
        }
        let event_index = bounded_index(event, TICKET_LIFECYCLE_EVENTS);
        let mut outcome_index = bounded_index(outcome, TICKET_LIFECYCLE_OUTCOMES);
        // Trapdoor: the exposition renders per-event outcome subsets, so a
        // normalized pair outside them would vanish from the scrape. Route to
        // this event's rendered `query` trapdoor instead, preserving the call.
        if !lifecycle_series_rendered(
            TICKET_LIFECYCLE_EVENTS[event_index],
            TICKET_LIFECYCLE_OUTCOMES[outcome_index],
        ) {
            outcome_index = bounded_index("query", TICKET_LIFECYCLE_OUTCOMES);
        }
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let counter = &mut values.ticket_lifecycle[event_index][outcome_index];
        *counter = counter.saturating_add(count);
    }

    /// Current audit mirror delivery-halt state (M4.48): `true` while the
    /// persistent kill switch is engaged, `false` once it is cleared. Call
    /// once per readable `audit_retry` sweep from the store's `delivery_halt`
    /// read; an unreadable halt leaves the last reported state untouched, so
    /// a database outage never fabricates a clear. Last writer wins. No
    /// actor ID, timestamp, error text or row content is retained.
    pub fn set_audit_delivery_halt(&self, halted: bool) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .audit_delivery_halt = u64::from(halted);
    }

    /// Current event-store consumer lag in seconds (S1): the `received_at -
    /// event_time` spread for the newest observed capture row. S1 exposes
    /// the setter contract; the periodic store-read caller lands with the
    /// consumer slice, so until then the series reads 0 (unobserved).
    /// Last writer wins. Non-finite or negative input clears to 0 so
    /// exposition stays a finite non-negative duration. No IDs, dedup keys,
    /// content or body text are retained.
    pub fn set_event_store_consumer_lag_secs(&self, lag_secs: f64) {
        let clamped = if lag_secs.is_finite() && lag_secs >= 0.0 {
            lag_secs
        } else {
            0.0
        };
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .event_store_consumer_lag_secs = clamped;
    }

    /// Pool samples are supplied at scrape time; this function never opens a DB connection.
    /// No supplied guard means a fresh/unconfigured zero snapshot.
    pub fn render(&self, pool: Option<(u32, usize, u32)>) -> String {
        self.render_with_rest_guard(pool, RestGuardSnapshot::default())
    }

    /// Pool and guard samples are supplied at scrape time; no network I/O.
    pub fn render_with_rest_guard(
        &self,
        pool: Option<(u32, usize, u32)>,
        guard: RestGuardSnapshot,
    ) -> String {
        let values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut out = String::with_capacity(16_384);
        // Process-lifetime identity, independent of image, clock and PID reuse.
        // Fixed numeric limbs avoid both unbounded labels and f64 precision loss.
        static PROCESS_INCARNATION: OnceLock<[u32; 4]> = OnceLock::new();
        let incarnation = PROCESS_INCARNATION.get_or_init(|| loop {
            let parts = rand::random::<[u32; 4]>();
            if parts != [0; 4] {
                return parts;
            }
        });
        header(
            &mut out,
            "two_bot_process_incarnation",
            "gauge",
            "Process identity: four exact u32s.",
        );
        for (part, value) in incarnation.iter().enumerate() {
            writeln!(
                out,
                "two_bot_process_incarnation{{part=\"{part}\"}} {value}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_gateway_events_total",
            "counter",
            "Events by type.",
        );
        for (name, count) in EVENTS.iter().zip(values.events) {
            writeln!(
                out,
                "two_bot_gateway_events_total{{event=\"{name}\"}} {count}"
            )
            .unwrap();
        }
        scalar(
            &mut out,
            "two_bot_gateway_reconnects_total",
            "counter",
            values.reconnects,
        );
        scalar(
            &mut out,
            "two_bot_gateway_resumes_total",
            "counter",
            values.resumes,
        );
        scalar(
            &mut out,
            "two_bot_gateway_disconnects_total",
            "counter",
            values.disconnects,
        );
        scalar(
            &mut out,
            "two_bot_gateway_missed_events_total",
            "counter",
            values.missed_events,
        );
        scalar(
            &mut out,
            "two_bot_gateway_rsvp_refused_total",
            "counter",
            values.rsvp_refused,
        );
        header(
            &mut out,
            "two_bot_presence_capture_total",
            "counter",
            "Capture outcomes.",
        );
        for (outcome, count) in PRESENCE_CAPTURE_OUTCOMES
            .iter()
            .zip(values.presence_capture)
        {
            writeln!(
                out,
                "two_bot_presence_capture_total{{outcome=\"{outcome}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_feed_fetches_total",
            "counter",
            "Fetch attempts by outcome.",
        );
        for (outcome, count) in FEED_FETCH_OUTCOMES.iter().zip(values.feed_fetch) {
            writeln!(
                out,
                "two_bot_feed_fetches_total{{outcome=\"{outcome}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_feed_fetch_errors_total",
            "counter",
            "Feed failures by class.",
        );
        for (error_class, count) in FEED_FETCH_ERROR_CLASSES
            .iter()
            .zip(values.feed_fetch_errors)
        {
            writeln!(
                out,
                "two_bot_feed_fetch_errors_total{{error_class=\"{error_class}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_automod_decisions_total",
            "counter",
            "Terminal inspections.",
        );
        for (outcome, count) in AUTOMOD_DECISION_OUTCOMES
            .iter()
            .zip(values.automod_decisions)
        {
            writeln!(
                out,
                "two_bot_automod_decisions_total{{outcome=\"{outcome}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_gateway_latency_seconds",
            "gauge",
            "ACK latency; NaN until measured.",
        );
        match values.latency_micros {
            Some(micros) => writeln!(
                out,
                "two_bot_gateway_latency_seconds {}",
                micros as f64 / 1_000_000.0
            )
            .unwrap(),
            None => writeln!(out, "two_bot_gateway_latency_seconds NaN").unwrap(),
        }
        values.handler.render(
            &mut out,
            "two_bot_handler_duration_seconds",
            "Handler elapsed time in seconds.",
        );
        values.voice_join_to_move.render(
            &mut out,
            "two_bot_voice_join_to_move_seconds",
            "Creator-join receipt to move, seconds.",
        );
        header(
            &mut out,
            "two_bot_voice_join_occupancy_total",
            "counter",
            "Human joins by prior occupancy (7+ cap).",
        );
        for (bucket, count) in VOICE_JOIN_OCCUPANCY_BUCKETS
            .iter()
            .zip(values.voice_join_occupancy)
        {
            writeln!(
                out,
                "two_bot_voice_join_occupancy_total{{occupancy=\"{bucket}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_rest_requests_total",
            "counter",
            "REST attempts incl. retries by route/result.",
        );
        for (route, results) in REST_ROUTES.iter().zip(values.rest) {
            for (result, count) in RESULTS.iter().zip(results) {
                writeln!(
                    out,
                    "two_bot_rest_requests_total{{route=\"{route}\",result=\"{result}\"}} {count}"
                )
                .unwrap();
            }
        }
        header(
            &mut out,
            "two_bot_job_runs_total",
            "counter",
            "Completed attempts by job/outcome.",
        );
        for (job, current) in JOBS.iter().zip(&values.jobs) {
            for (outcome, count) in JOB_OUTCOMES.iter().zip(current.runs) {
                writeln!(
                    out,
                    "two_bot_job_runs_total{{job=\"{job}\",outcome=\"{outcome}\"}} {count}"
                )
                .unwrap();
            }
        }
        header(
            &mut out,
            "two_bot_job_last_success_timestamp_seconds",
            "gauge",
            "Last success Unix time; 0 if never.",
        );
        for (job, current) in JOBS.iter().zip(&values.jobs) {
            writeln!(
                out,
                "two_bot_job_last_success_timestamp_seconds{{job=\"{job}\"}} {}",
                current.last_success
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_job_consecutive_failures",
            "gauge",
            "Failure streak; success resets.",
        );
        for (job, current) in JOBS.iter().zip(&values.jobs) {
            writeln!(
                out,
                "two_bot_job_consecutive_failures{{job=\"{job}\"}} {}",
                current.consecutive_failures
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_job_last_error_class",
            "gauge",
            "One-hot last error; success clears.",
        );
        for (job, current) in JOBS.iter().zip(&values.jobs) {
            for class in JOB_ERROR_CLASSES {
                writeln!(
                    out,
                    "two_bot_job_last_error_class{{job=\"{job}\",class=\"{}\"}} {}",
                    class.as_str(),
                    u8::from(current.last_error_class == Some(*class))
                )
                .unwrap();
            }
        }
        header(
            &mut out,
            "two_bot_voice_operations_total",
            "counter",
            "Terminal room outcomes; no retries.",
        );
        for (op, outcomes) in VOICE_OPERATIONS.iter().zip(values.voice_ops) {
            for (outcome, count) in VOICE_OUTCOMES.iter().zip(outcomes) {
                writeln!(
                    out,
                    "two_bot_voice_operations_total{{op=\"{op}\",outcome=\"{outcome}\"}} {count}"
                )
                .unwrap();
            }
        }
        header(
            &mut out,
            "two_bot_voice_reconcile_actions_total",
            "counter",
            "Reconcile transitions/new enqueues by action.",
        );
        for (action, count) in VOICE_RECONCILE_ACTIONS.iter().zip(values.voice_reconcile) {
            writeln!(
                out,
                "two_bot_voice_reconcile_actions_total{{action=\"{action}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_voice_dead_letters_total",
            "counter",
            "Queue writes exhausting QUEUE_MAX_ATTEMPTS.",
        );
        for (action, count) in VOICE_DEAD_ACTIONS.iter().zip(values.voice_dead) {
            writeln!(
                out,
                "two_bot_voice_dead_letters_total{{action=\"{action}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_voice_tracked_rooms",
            "gauge",
            "In-memory rooms; may include ghosts.",
        );
        writeln!(out, "two_bot_voice_tracked_rooms {}", values.voice_tracked).unwrap();
        header(
            &mut out,
            "two_bot_voice_compensation_pending",
            "gauge",
            "Rooms awaiting delete after failed write.",
        );
        writeln!(
            out,
            "two_bot_voice_compensation_pending {}",
            values.voice_compensation
        )
        .unwrap();
        scalar(
            &mut out,
            "two_bot_voice_orphans_total",
            "counter",
            values.voice_orphans,
        );
        scalar(
            &mut out,
            "two_bot_voice_joins_received_total",
            "counter",
            values.voice_joins_received,
        );
        header(
            &mut out,
            "two_bot_voice_join_refused_total",
            "counter",
            "Creator-join refusals before queueing.",
        );
        for (reason, count) in VOICE_JOIN_REFUSAL_REASONS
            .iter()
            .zip(values.voice_join_refused)
        {
            writeln!(
                out,
                "two_bot_voice_join_refused_total{{reason=\"{reason}\"}} {count}"
            )
            .unwrap();
        }
        scalar(
            &mut out,
            "two_bot_voice_inbox_overflow_dropped_total",
            "counter",
            values.voice_inbox_overflow_dropped,
        );
        scalar(
            &mut out,
            "two_bot_voice_queue_overflow_dropped_total",
            "counter",
            values.voice_queue_overflow_dropped,
        );
        // Aggregates over the current guild-actor set: any actor halted,
        // per-lane saturating sums, maximum oldest-pending age.
        let voice_halted = u64::from(values.voice_actors.values().any(|actor| actor.halted));
        let mut voice_queue_pending = [0_u64; VOICE_QUEUE_LANES.len()];
        let mut voice_queue_oldest_age_secs = 0_u64;
        let mut voice_live_not_ready_secs = 0_u64;
        for actor in values.voice_actors.values() {
            for (total, depth) in voice_queue_pending.iter_mut().zip(actor.queue_pending) {
                *total = total.saturating_add(depth);
            }
            voice_queue_oldest_age_secs = voice_queue_oldest_age_secs.max(actor.oldest_age_secs);
            voice_live_not_ready_secs = voice_live_not_ready_secs.max(actor.live_not_ready_secs);
        }
        header(
            &mut out,
            "two_bot_voice_worker_halted",
            "gauge",
            "1 while any guild actor is halted.",
        );
        writeln!(out, "two_bot_voice_worker_halted {voice_halted}").unwrap();
        header(
            &mut out,
            "two_bot_voice_queue_pending",
            "gauge",
            "Queued voice actions by lane; all guilds.",
        );
        for (lane, count) in VOICE_QUEUE_LANES.iter().zip(voice_queue_pending) {
            writeln!(
                out,
                "two_bot_voice_queue_pending{{lane=\"{lane}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_voice_queue_oldest_age_seconds",
            "gauge",
            "Max queued age, seconds; 0 if empty.",
        );
        writeln!(
            out,
            "two_bot_voice_queue_oldest_age_seconds {voice_queue_oldest_age_secs}"
        )
        .unwrap();
        header(
            &mut out,
            "two_bot_voice_live_not_ready_seconds",
            "gauge",
            "Max connected unready seconds; 0 if none.",
        );
        writeln!(
            out,
            "two_bot_voice_live_not_ready_seconds {voice_live_not_ready_secs}"
        )
        .unwrap();
        scalar(
            &mut out,
            "two_bot_voice_live_self_heal_total",
            "counter",
            values.voice_live_self_heals,
        );
        header(
            &mut out,
            "two_bot_voice_rest_in_flight_age_seconds",
            "gauge",
            "Oldest REST age, seconds; 0 if idle.",
        );
        writeln!(
            out,
            "two_bot_voice_rest_in_flight_age_seconds {}",
            values.voice_rest_in_flight_age_secs
        )
        .unwrap();
        header(
            &mut out,
            "two_bot_voice_creator_map_generation",
            "gauge",
            "Creator-map generation; 0 before first merge.",
        );
        writeln!(
            out,
            "two_bot_voice_creator_map_generation {}",
            values.creator_map_generation
        )
        .unwrap();
        header(
            &mut out,
            "two_bot_voice_creator_map_age_seconds",
            "gauge",
            "Seconds since creator-map merge; 0 if never merged.",
        );
        writeln!(
            out,
            "two_bot_voice_creator_map_age_seconds {}",
            values.creator_map_age_secs
        )
        .unwrap();
        scalar(
            &mut out,
            "two_bot_voice_global_backoff_capped_total",
            "counter",
            values.voice_global_backoff_capped,
        );
        scalar(
            &mut out,
            "two_bot_voice_dispatch_timeouts_total",
            "counter",
            values.voice_dispatch_timeouts,
        );
        scalar(
            &mut out,
            "two_bot_voice_watchdog_stucks_total",
            "counter",
            values.voice_watchdog_stucks,
        );
        scalar(
            &mut out,
            "two_bot_voice_halt_recoveries_total",
            "counter",
            values.voice_halt_recoveries,
        );
        header(
            &mut out,
            "two_bot_voice_actor_inbox_turn_bound",
            "gauge",
            "Inbox turns before other ready work; 0 if unset.",
        );
        writeln!(
            out,
            "two_bot_voice_actor_inbox_turn_bound {}",
            values.voice_actor_inbox_turn_bound
        )
        .unwrap();
        scalar(
            &mut out,
            "two_bot_voice_actor_inbox_turns_total",
            "counter",
            values.voice_actor_inbox_turns,
        );
        scalar(
            &mut out,
            "two_bot_voice_actor_dispatch_turns_total",
            "counter",
            values.voice_actor_dispatch_turns,
        );
        header(
            &mut out,
            "two_bot_voice_vote_kick_total",
            "counter",
            "Vote-kick outcomes; votes audit-only.",
        );
        for (outcome, count) in VOICE_VOTE_KICK_OUTCOMES.iter().zip(values.voice_vote_kick) {
            writeln!(
                out,
                "two_bot_voice_vote_kick_total{{outcome=\"{outcome}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_event_voice_tick_total",
            "counter",
            "T-15 TBD ticks incl. backoff skips.",
        );
        for (outcome, count) in EVENT_VOICE_TICK_OUTCOMES
            .iter()
            .zip(values.event_voice_tick)
        {
            writeln!(
                out,
                "two_bot_event_voice_tick_total{{outcome=\"{outcome}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_voice_names_total",
            "counter",
            "Name-template and rename outcomes.",
        );
        for (outcome, count) in VOICE_NAME_OUTCOMES.iter().zip(values.voice_names) {
            writeln!(
                out,
                "two_bot_voice_names_total{{outcome=\"{outcome}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_db_errors_total",
            "counter",
            "Storage failures by operation.",
        );
        for (op, count) in DB_ERROR_OPS.iter().zip(values.db_errors) {
            writeln!(out, "two_bot_db_errors_total{{op=\"{op}\"}} {count}").unwrap();
        }
        header(
            &mut out,
            "two_bot_containment_store_errors_total",
            "counter",
            "Containment-store failures by operation.",
        );
        for (op, count) in CONTAINMENT_STORE_OPS
            .iter()
            .zip(values.containment_store_errors)
        {
            writeln!(
                out,
                "two_bot_containment_store_errors_total{{op=\"{op}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_send_admissions_total",
            "counter",
            "Terminal admit() decisions by outcome.",
        );
        for (outcome, count) in SEND_ADMISSION_OUTCOMES.iter().zip(values.send_admissions) {
            writeln!(
                out,
                "two_bot_send_admissions_total{{outcome=\"{outcome}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_gateway_prefix_trigger_refused_total",
            "counter",
            "Automod prefix refusals before lookup.",
        );
        for (reason, count) in PREFIX_TRIGGER_REFUSED_REASONS
            .iter()
            .zip(values.prefix_trigger_refused)
        {
            writeln!(
                out,
                "two_bot_gateway_prefix_trigger_refused_total{{reason=\"{reason}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_dispatch_drops_total",
            "counter",
            "Saturation/fairness drops by lane.",
        );
        for (lane, count) in DISPATCH_LANES.iter().zip(values.dispatch_drops) {
            writeln!(
                out,
                "two_bot_dispatch_drops_total{{lane=\"{lane}\"}} {count}"
            )
            .unwrap();
        }
        for (name, counts) in [
            (
                "two_bot_raid_dropped_joins_total",
                values.raid_dropped_joins,
            ),
            (
                "two_bot_join_risk_dropped_joins_total",
                values.join_risk_dropped_joins,
            ),
        ] {
            header(
                &mut out,
                name,
                "counter",
                "Joins lost pre-observation; no retry.",
            );
            for (reason, index) in JOIN_QUEUE_DROP_REASONS {
                writeln!(
                    out,
                    "{name}{{reason=\"{reason}\"}} {}",
                    counts[index as usize]
                )
                .unwrap();
            }
        }
        header(
            &mut out,
            "two_bot_gateway_checkpoint_failures_total",
            "counter",
            "Dispatch-stopping failures by stage.",
        );
        for (stage, count) in CHECKPOINT_FAILURE_STAGES
            .iter()
            .zip(values.checkpoint_failures)
        {
            writeln!(
                out,
                "two_bot_gateway_checkpoint_failures_total{{stage=\"{stage}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_drop_poison_total",
            "counter",
            "Warned poisoned-lock recoveries by guard.",
        );
        let drop_poisons = self
            .1
            .each_ref()
            .map(|counter| counter.load(Ordering::Relaxed));
        for (guard, count) in DROP_POISON_GUARDS.iter().zip(drop_poisons) {
            writeln!(
                out,
                "two_bot_drop_poison_total{{guard=\"{guard}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_internal_actions_total",
            "counter",
            "Signed action outcomes by family.",
        );
        for (family, outcomes) in INTERNAL_ACTION_FAMILIES.iter().zip(values.internal_actions) {
            for (outcome, count) in INTERNAL_ACTION_OUTCOMES.iter().zip(outcomes) {
                writeln!(
                    out,
                    "two_bot_internal_actions_total{{family=\"{family}\",outcome=\"{outcome}\"}} {count}"
                )
                .unwrap();
            }
        }
        header(
            &mut out,
            "two_bot_moderation_enforcements_total",
            "counter",
            "Terminal verbs; no retries/backoffs.",
        );
        for (verb, outcomes) in MODERATION_VERBS.iter().zip(values.moderation_enforcements) {
            for (outcome, count) in MODERATION_ENFORCEMENT_OUTCOMES.iter().zip(outcomes) {
                writeln!(
                    out,
                    "two_bot_moderation_enforcements_total{{verb=\"{verb}\",outcome=\"{outcome}\"}} {count}"
                )
                .unwrap();
            }
        }
        header(
            &mut out,
            "two_bot_community_facts_drain_failures_total",
            "counter",
            "Failed dispatch drains by reason.",
        );
        for (reason, count) in COMMUNITY_FACTS_DRAIN_REASONS
            .iter()
            .zip(values.community_facts_drain_failures)
        {
            writeln!(
                out,
                "two_bot_community_facts_drain_failures_total{{reason=\"{reason}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_audit_delivery_halt",
            "gauge",
            "Audit halt (1/0); read failure retains.",
        );
        writeln!(
            out,
            "two_bot_audit_delivery_halt {}",
            values.audit_delivery_halt
        )
        .unwrap();
        // Empty HELP: main sits within 131 bytes of the scrape byte budget
        // (`eventsub_classification_growth_fits_the_scrape_byte_budget`), and
        // this counter's three lines must fit without relaxing that headroom.
        header(&mut out, "two_bot_gateway_drain_skips_total", "counter", "");
        writeln!(
            out,
            "two_bot_gateway_drain_skips_total {}",
            values.gateway_drain_skips
        )
        .unwrap();
        scalar(
            &mut out,
            "two_bot_retention_replay_needed_total",
            "counter",
            values.retention_replay_needed,
        );
        scalar(
            &mut out,
            "two_bot_ticket_component_drops_total",
            "counter",
            values.ticket_component_drops,
        );
        header(
            &mut out,
            "two_bot_event_store_consumer_lag_seconds",
            "gauge",
            "Newest capture lag, seconds; 0 if unseen.",
        );
        writeln!(
            out,
            "two_bot_event_store_consumer_lag_seconds {}",
            values.event_store_consumer_lag_secs
        )
        .unwrap();
        scalar(
            &mut out,
            "two_bot_eventsub_received_total",
            "counter",
            values.eventsub_received,
        );
        header(
            &mut out,
            "two_bot_eventsub_classified_total",
            "counter",
            "Receiver classifications.",
        );
        for (kind, count) in EVENTSUB_DELIVERY_KINDS
            .iter()
            .zip(values.eventsub_classified)
        {
            writeln!(
                out,
                "two_bot_eventsub_classified_total{{kind=\"{kind}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_eventsub_posts_total",
            "counter",
            "Go-live post attempts by outcome.",
        );
        for (outcome, count) in EVENTSUB_POST_OUTCOMES.iter().zip(values.eventsub_posts) {
            writeln!(
                out,
                "two_bot_eventsub_posts_total{{outcome=\"{outcome}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_eventsub_reconcile_passes_total",
            "counter",
            "Reconcile outcomes; alert on failure deltas.",
        );
        for (outcome, count) in EVENTSUB_RECONCILE_OUTCOMES
            .iter()
            .zip(values.eventsub_reconcile_passes)
        {
            writeln!(
                out,
                "two_bot_eventsub_reconcile_passes_total{{outcome=\"{outcome}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_eventsub_reconcile_actions_total",
            "counter",
            "Successful reconcile mutations by action.",
        );
        for (action, count) in EVENTSUB_RECONCILE_ACTIONS
            .iter()
            .zip(values.eventsub_reconcile_actions)
        {
            writeln!(
                out,
                "two_bot_eventsub_reconcile_actions_total{{action=\"{action}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_eventsub_reconcile_errors_total",
            "counter",
            "Failures by error_class.",
        );
        for (error_class, count) in EVENTSUB_RECONCILE_ERROR_CLASSES
            .iter()
            .zip(values.eventsub_reconcile_errors)
        {
            writeln!(
                out,
                "two_bot_eventsub_reconcile_errors_total{{error_class=\"{error_class}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_ticket_lifecycle_total",
            "counter",
            "Terminal ticket outcomes; purge counts bodies.",
        );
        for (event, outcomes) in TICKET_LIFECYCLE_EVENTS.iter().zip(values.ticket_lifecycle) {
            for (outcome, count) in TICKET_LIFECYCLE_OUTCOMES.iter().zip(outcomes) {
                if !lifecycle_series_rendered(event, outcome) {
                    continue;
                }
                writeln!(
                    out,
                    "two_bot_ticket_lifecycle_total{{event=\"{event}\",outcome=\"{outcome}\"}} {count}"
                )
                .unwrap();
            }
        }
        for (name, count) in [
            (
                "two_bot_rest_guard_invalid_requests_total",
                guard.invalid_requests_total,
            ),
            (
                "two_bot_rest_guard_rejected_requests_total",
                guard.rejected_requests_total,
            ),
            (
                "two_bot_rest_guard_breaker_opens_total",
                guard.breaker_opens_total,
            ),
            (
                "two_bot_rest_guard_breaker_closes_total",
                guard.breaker_closes_total,
            ),
            (
                "two_bot_rest_guard_global_pauses_total",
                guard.global_pauses_total,
            ),
        ] {
            scalar(&mut out, name, "counter", count);
        }
        for (name, value) in [
            (
                "two_bot_rest_guard_invalid_requests_in_window",
                guard.invalid_requests_in_window as u64,
            ),
            (
                "two_bot_rest_guard_breaker_open",
                u64::from(guard.breaker_open),
            ),
            (
                "two_bot_rest_guard_token_invalid",
                u64::from(guard.token_invalid),
            ),
            (
                "two_bot_rest_guard_pending_global_responses",
                guard.pending_global_responses as u64,
            ),
            (
                "two_bot_rest_guard_global_paused",
                u64::from(
                    !guard.global_pause_remaining.is_zero() || guard.pending_global_responses > 0,
                ),
            ),
        ] {
            scalar(&mut out, name, "gauge", value);
        }
        header(
            &mut out,
            "two_bot_rest_guard_global_pause_remaining_seconds",
            "gauge",
            "Cooldown seconds; pending bodies may block at zero.",
        );
        // Duration is nonnegative and its entire u64-seconds range is finite in f64.
        writeln!(
            out,
            "two_bot_rest_guard_global_pause_remaining_seconds {}",
            guard.global_pause_remaining.as_secs_f64()
        )
        .unwrap();
        let (size, idle, max) = pool.unwrap_or_default();
        scalar(
            &mut out,
            "two_bot_db_pool_configured",
            "gauge",
            u64::from(pool.is_some()),
        );
        scalar(
            &mut out,
            "two_bot_db_pool_connections",
            "gauge",
            u64::from(size),
        );
        scalar(
            &mut out,
            "two_bot_db_pool_idle_connections",
            "gauge",
            idle as u64,
        );
        scalar(
            &mut out,
            "two_bot_db_pool_max_connections",
            "gauge",
            u64::from(max),
        );
        out
    }
}

fn header(out: &mut String, name: &str, kind: &str, help: &str) {
    writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}").unwrap();
}

fn scalar(out: &mut String, name: &str, kind: &str, value: u64) {
    header(out, name, kind, "Local.");
    writeln!(out, "{name} {value}").unwrap();
}

#[cfg(test)]
#[path = "metrics_drop_poison_acceptance_tests.rs"]
mod drop_poison_acceptance_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_guard_snapshot_is_fixed_size_label_free_and_finite() {
        let metrics = Metrics::default();
        let empty = metrics.render(None);
        let snapshot = RestGuardSnapshot {
            invalid_requests_in_window: usize::MAX,
            invalid_requests_total: u64::MAX,
            rejected_requests_total: u64::MAX,
            breaker_opens_total: u64::MAX,
            breaker_closes_total: u64::MAX,
            global_pauses_total: u64::MAX,
            breaker_open: true,
            token_invalid: true,
            global_pause_remaining: Duration::MAX,
            pending_global_responses: usize::MAX,
        };
        let full = metrics.render_with_rest_guard(None, snapshot);
        let samples = |text: &str| -> std::collections::BTreeMap<String, f64> {
            text.lines()
                .filter(|line| line.starts_with("two_bot_rest_guard_"))
                .map(|line| {
                    let (name, value) = line.split_once(' ').unwrap();
                    assert!(!name.contains('{'), "guard metrics must have no labels");
                    (name.to_owned(), value.parse::<f64>().unwrap())
                })
                .collect()
        };
        let zeros = samples(&empty);
        let maxima = samples(&full);
        assert_eq!(zeros.len(), 11);
        assert_eq!(
            zeros.keys().collect::<Vec<_>>(),
            maxima.keys().collect::<Vec<_>>()
        );
        assert!(zeros.values().all(|value| *value == 0.0));
        assert!(maxima
            .values()
            .all(|value| value.is_finite() && *value >= 0.0));
        assert_eq!(full.lines().count(), empty.lines().count());
        assert!(full.len() < 65_536, "must fit the Worker scrape body cap");
        let pending = metrics.render_with_rest_guard(
            None,
            RestGuardSnapshot {
                pending_global_responses: 1,
                ..Default::default()
            },
        );
        assert!(pending.contains("two_bot_rest_guard_global_paused 1\n"));
        assert!(pending.contains("two_bot_rest_guard_global_pause_remaining_seconds 0\n"));
        let fractional = metrics.render_with_rest_guard(
            None,
            RestGuardSnapshot {
                global_pause_remaining: Duration::from_millis(1250),
                ..Default::default()
            },
        );
        assert!(fractional.contains("two_bot_rest_guard_global_pause_remaining_seconds 1.25\n"));
        assert!(fractional.contains("two_bot_rest_guard_global_paused 1\n"));
    }

    #[test]
    fn eventsub_classification_growth_fits_the_scrape_byte_budget() {
        let metrics = Metrics::default();
        let baseline = metrics.render(None);
        {
            let mut values = metrics.0.lock().unwrap();
            values.eventsub_received = u64::MAX;
            values.eventsub_classified.fill(u64::MAX);
        }
        let full = metrics.render_with_rest_guard(
            Some((u32::MAX, usize::MAX, u32::MAX)),
            RestGuardSnapshot {
                invalid_requests_in_window: usize::MAX,
                invalid_requests_total: u64::MAX,
                rejected_requests_total: u64::MAX,
                breaker_opens_total: u64::MAX,
                breaker_closes_total: u64::MAX,
                global_pauses_total: u64::MAX,
                breaker_open: true,
                token_invalid: true,
                global_pause_remaining: Duration::MAX,
                pending_global_responses: usize::MAX,
            },
        );
        // Leave room for other counters to grow without relaxing any consumer's
        // 64 KiB cap. This fixture saturates EventSub, pool and guard samples,
        // not every process metric simultaneously.
        assert!(
            full.len() < 65_536 - 1_024,
            "{} bytes leaves less than 1 KiB of scrape headroom",
            full.len()
        );
        let series = |text: &str| -> Vec<String> {
            text.lines()
                .filter(|line| !line.starts_with('#'))
                .map(|line| line.rsplit_once(' ').unwrap().0.to_owned())
                .collect()
        };
        assert_eq!(series(&full), series(&baseline));
        for kind in EVENTSUB_DELIVERY_KINDS {
            assert!(full.contains(&format!(
                "two_bot_eventsub_classified_total{{kind=\"{kind}\"}} {}\n",
                u64::MAX
            )));
        }
    }

    #[test]
    fn exposition_has_cumulative_histogram_and_unique_series() {
        let metrics = Metrics::default();
        metrics.handler_duration(Duration::from_millis(5));
        metrics.handler_duration(Duration::from_secs(9));
        metrics.gateway_event("RESUMED");
        metrics.gateway_latency(Duration::from_millis(12));
        metrics.job_success("invite_snapshot", 123);
        metrics.job_failure("rank", JobErrorClass::Configuration);
        let text = metrics.render(Some((3, 1, 5)));
        assert!(text.ends_with('\n'));
        assert!(text.contains("# TYPE two_bot_handler_duration_seconds histogram\n"));
        assert!(text.contains("two_bot_handler_duration_seconds_bucket{le=\"0.001\"} 0\n"));
        assert!(text.contains("two_bot_handler_duration_seconds_bucket{le=\"0.005\"} 1\n"));
        assert!(text.contains("two_bot_handler_duration_seconds_bucket{le=\"+Inf\"} 2\n"));
        assert!(text.contains("two_bot_handler_duration_seconds_sum 9.005\n"));
        assert!(text.contains("two_bot_handler_duration_seconds_count 2\n"));
        assert!(text.contains("two_bot_gateway_latency_seconds 0.012\n"));
        assert!(text.contains("two_bot_gateway_resumes_total 1\n"));
        assert!(text.contains("two_bot_db_pool_idle_connections 1\n"));
        assert!(text.contains("# TYPE two_bot_job_last_error_class gauge\n"));
        assert!(
            text.contains("two_bot_job_last_error_class{job=\"rank\",class=\"configuration\"} 1\n")
        );
        assert!(text.contains("two_bot_job_last_error_class{job=\"rank\",class=\"database\"} 0\n"));
        assert!(text.contains(
            "two_bot_job_last_error_class{job=\"invite_snapshot\",class=\"configuration\"} 0\n"
        ));
        let mut series = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(series.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn untrusted_labels_cannot_grow_or_inject_series() {
        let metrics = Metrics::default();
        let before = metrics.render(None).lines().count();
        for id in 0..10_000 {
            let hostile = format!("{id}\"\\\nsecret=value");
            metrics.gateway_event(&hostile);
            metrics.rest_response(&hostile, Some(429));
            metrics.job_success(&hostile, 123);
            metrics.job_failure(&hostile, JobErrorClass::Database);
            metrics.db_error(&hostile);
            metrics.send_admission(&hostile);
            metrics.prefix_trigger_refused(&hostile);
            metrics.dispatch_drop(&hostile);
            metrics.voice_vote_kick(&hostile);
            metrics.checkpoint_failure(&hostile);
            metrics.presence_capture(&hostile);
            metrics.feed_fetch(&hostile);
            metrics.feed_fetch_error(&hostile);
            metrics.internal_action(&hostile, &hostile);
            metrics.moderation_enforcement(&hostile, &hostile);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        // `invalid_secret` is a legitimate bounded kind: match the hostile
        // `secret=value` injection marker, not the bare substring.
        assert!(!text.contains("secret="));
        assert!(text.contains("two_bot_gateway_events_total{event=\"other\"} 10000\n"));
        assert!(
            text.contains("two_bot_rest_requests_total{route=\"other\",result=\"429\"} 10000\n")
        );
        assert!(text.contains("two_bot_db_pool_configured 0\n"));
        for outcome in JOB_OUTCOMES {
            assert!(text.contains(&format!(
                "two_bot_job_runs_total{{job=\"other\",outcome=\"{outcome}\"}} 10000\n"
            )));
        }
        assert!(text.contains("two_bot_job_last_success_timestamp_seconds{job=\"other\"} 123\n"));
        assert!(text.contains("two_bot_job_consecutive_failures{job=\"other\"} 1\n"));
        assert!(text.contains("two_bot_job_last_error_class{job=\"other\",class=\"database\"} 1\n"));
    }

    #[test]
    fn feeds_and_self_role_recovery_have_independent_job_series() {
        for (succeeded, failed) in [
            ("feeds", "self_role_recovery"),
            ("self_role_recovery", "feeds"),
        ] {
            let metrics = Metrics::default();
            metrics.job_success(failed, 123);
            metrics.job_failure(failed, JobErrorClass::Database);
            metrics.job_failure(failed, JobErrorClass::Database);
            metrics.job_success("unknown_job", 234);
            metrics.job_failure("another_unknown_job", JobErrorClass::Database);
            metrics.job_failure(succeeded, JobErrorClass::Database);
            metrics.job_success(succeeded, 456);
            let text = metrics.render(None);
            for (job, success, failure, last_success, streak) in [
                (succeeded, 1, 1, 456, 0),
                (failed, 1, 2, 123, 2),
                ("other", 1, 1, 234, 1),
            ] {
                for (outcome, count) in [("success", success), ("failure", failure)] {
                    assert!(text.contains(&format!(
                        "two_bot_job_runs_total{{job=\"{job}\",outcome=\"{outcome}\"}} {count}\n"
                    )));
                }
                assert!(text.contains(&format!(
                    "two_bot_job_last_success_timestamp_seconds{{job=\"{job}\"}} {last_success}\n"
                )));
                assert!(text.contains(&format!(
                    "two_bot_job_consecutive_failures{{job=\"{job}\"}} {streak}\n"
                )));
            }
            for (name, count) in [
                ("two_bot_job_runs_total", JOBS.len() * JOB_OUTCOMES.len()),
                ("two_bot_job_last_success_timestamp_seconds", JOBS.len()),
                ("two_bot_job_consecutive_failures", JOBS.len()),
            ] {
                assert_eq!(
                    text.lines().filter(|line| line.starts_with(name)).count(),
                    count
                );
            }
            assert_eq!(JOBS.len(), 18);
            assert!(!text.contains("unknown_job"));
        }
    }

    #[test]
    fn unban_sweep_and_automod_expiry_have_independent_job_series() {
        for (succeeded, failed) in [
            ("member_unban_sweep", "automod_expiry"),
            ("automod_expiry", "member_unban_sweep"),
        ] {
            let metrics = Metrics::default();
            metrics.job_success(failed, 123);
            metrics.job_failure(failed, JobErrorClass::Database);
            metrics.job_failure(failed, JobErrorClass::Database);
            metrics.job_success("unknown_job", 234);
            metrics.job_failure("another_unknown_job", JobErrorClass::Database);
            metrics.job_failure(succeeded, JobErrorClass::Database);
            metrics.job_success(succeeded, 456);
            let text = metrics.render(None);
            for (job, success, failure, last_success, streak) in [
                (succeeded, 1, 1, 456, 0),
                (failed, 1, 2, 123, 2),
                ("other", 1, 1, 234, 1),
            ] {
                for (outcome, count) in [("success", success), ("failure", failure)] {
                    assert!(text.contains(&format!(
                        "two_bot_job_runs_total{{job=\"{job}\",outcome=\"{outcome}\"}} {count}\n"
                    )));
                }
                assert!(text.contains(&format!(
                    "two_bot_job_last_success_timestamp_seconds{{job=\"{job}\"}} {last_success}\n"
                )));
                assert!(text.contains(&format!(
                    "two_bot_job_consecutive_failures{{job=\"{job}\"}} {streak}\n"
                )));
            }
            for (name, count) in [
                ("two_bot_job_runs_total", JOBS.len() * JOB_OUTCOMES.len()),
                ("two_bot_job_last_success_timestamp_seconds", JOBS.len()),
                ("two_bot_job_consecutive_failures", JOBS.len()),
            ] {
                assert_eq!(
                    text.lines().filter(|line| line.starts_with(name)).count(),
                    count
                );
            }
            assert_eq!(JOBS.len(), 18);
            assert!(!text.contains("unknown_job"));
        }
    }

    #[test]
    fn automod_success_cannot_clear_a_failing_unban_streak() {
        let metrics = Metrics::default();
        metrics.job_failure("member_unban_sweep", JobErrorClass::Database);
        metrics.job_failure("member_unban_sweep", JobErrorClass::Rest);
        metrics.job_success("automod_expiry", 456);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_job_consecutive_failures{job=\"member_unban_sweep\"} 2\n"));
        assert!(text.contains("two_bot_job_consecutive_failures{job=\"automod_expiry\"} 0\n"));
        assert!(text
            .contains("two_bot_job_last_success_timestamp_seconds{job=\"automod_expiry\"} 456\n"));
        assert!(text.contains(
            "two_bot_job_last_success_timestamp_seconds{job=\"member_unban_sweep\"} 0\n"
        ));
        assert!(text.contains("two_bot_job_consecutive_failures{job=\"other\"} 0\n"));
    }

    #[test]
    fn job_counters_saturate_and_success_resets_failures() {
        let metrics = Metrics::default();
        {
            let mut values = metrics.0.lock().unwrap();
            let current = &mut values.jobs[bounded_index("rank", JOBS)];
            current.runs = [u64::MAX; JOB_OUTCOMES.len()];
            current.consecutive_failures = u64::MAX;
        }
        metrics.job_failure("rank", JobErrorClass::RecoveryRequired);
        assert!(metrics.render(None).contains(&format!(
            "two_bot_job_consecutive_failures{{job=\"rank\"}} {}\n",
            u64::MAX
        )));
        assert!(metrics.render(None).contains(
            "two_bot_job_last_error_class{job=\"rank\",class=\"recovery_required\"} 1\n"
        ));
        metrics.job_success("rank", 456);
        let text = metrics.render(None);
        for outcome in JOB_OUTCOMES {
            assert!(text.contains(&format!(
                "two_bot_job_runs_total{{job=\"rank\",outcome=\"{outcome}\"}} {}\n",
                u64::MAX
            )));
        }
        assert!(text.contains("two_bot_job_last_success_timestamp_seconds{job=\"rank\"} 456\n"));
        assert!(text.contains("two_bot_job_consecutive_failures{job=\"rank\"} 0\n"));
        for class in JOB_ERROR_CLASSES {
            assert!(text.contains(&format!(
                "two_bot_job_last_error_class{{job=\"rank\",class=\"{}\"}} 0\n",
                class.as_str()
            )));
        }
    }

    #[test]
    fn a_later_failure_replaces_the_last_error_class() {
        let metrics = Metrics::default();
        metrics.job_failure("rank", JobErrorClass::Database);
        metrics.job_failure("rank", JobErrorClass::Rest);

        let text = metrics.render(None);
        assert!(text.contains("two_bot_job_last_error_class{job=\"rank\",class=\"database\"} 0\n"));
        assert!(text.contains("two_bot_job_last_error_class{job=\"rank\",class=\"rest\"} 1\n"));
    }

    #[test]
    fn voice_join_latency_is_a_label_free_seconds_histogram() {
        let metrics = Metrics::default();
        metrics.voice_join_to_move(Duration::from_millis(125));
        metrics.voice_join_to_move(Duration::from_millis(375));
        let text = metrics.render(None);
        assert!(text.contains("two_bot_voice_join_to_move_seconds_count 2\n"));
        assert!(text.contains("two_bot_voice_join_to_move_seconds_sum 0.5\n"));
        assert!(text.contains("two_bot_voice_join_to_move_seconds_bucket{le=\"0.1\"} 0\n"));
        assert!(text.contains("two_bot_voice_join_to_move_seconds_bucket{le=\"0.5\"} 2\n"));
    }

    #[test]
    fn voice_join_bucket_labels_match_the_pinned_contract() {
        let metrics = Metrics::default();
        metrics.voice_join_to_move(Duration::from_millis(1));
        let text = metrics.render(None);
        let rendered: Vec<&str> = text
            .lines()
            .filter_map(|line| {
                line.strip_prefix("two_bot_voice_join_to_move_seconds_bucket{le=\"")?
                    .split('"')
                    .next()
            })
            .collect();
        assert_eq!(rendered, VOICE_JOIN_TO_MOVE_BUCKET_LE);
    }

    #[test]
    fn voice_join_occupancy_buckets_cap_without_new_series() {
        // Seeding TEST (occupancy-to-join curve): one counter per occupancy
        // bucket, capped at 7+ so cardinality stays fixed. No channel, member
        // or time labels ever reach exposition.
        let metrics = Metrics::default();
        metrics.voice_join_occupancy(0);
        metrics.voice_join_occupancy(1);
        metrics.voice_join_occupancy(2);
        metrics.voice_join_occupancy(7);
        metrics.voice_join_occupancy(100);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_voice_join_occupancy_total{occupancy=\"0\"} 1\n"));
        assert!(text.contains("two_bot_voice_join_occupancy_total{occupancy=\"1\"} 1\n"));
        assert!(text.contains("two_bot_voice_join_occupancy_total{occupancy=\"2\"} 1\n"));
        assert!(text.contains("two_bot_voice_join_occupancy_total{occupancy=\"7+\"} 2\n"));
    }

    #[test]
    fn voice_signals_stay_bounded_and_saturate() {
        let metrics = Metrics::default();
        metrics.voice_operation("create", "success");
        metrics.voice_operation("move", "discord");
        metrics.voice_operation("delete", "persistence");
        metrics.voice_operation("create", "category_full");
        metrics.voice_operation("move", "cancelled");
        metrics.voice_reconcile("delete_enqueued", 2);
        metrics.voice_reconcile("suspended", 1);
        metrics.voice_reconcile("resumed", 0);
        metrics.voice_dead_letter("create");
        metrics.voice_dead_letter("other");
        metrics.voice_state(7, 2);
        metrics.voice_orphan();
        let text = metrics.render(None);
        assert!(
            text.contains("two_bot_voice_operations_total{op=\"create\",outcome=\"success\"} 1\n")
        );
        assert!(
            text.contains("two_bot_voice_operations_total{op=\"move\",outcome=\"discord\"} 1\n")
        );
        assert!(
            text.contains("two_bot_voice_reconcile_actions_total{action=\"delete_enqueued\"} 2\n")
        );
        assert!(text.contains("two_bot_voice_dead_letters_total{action=\"create\"} 1\n"));
        assert!(text.contains("two_bot_voice_tracked_rooms 7\n"));
        assert!(text.contains("two_bot_voice_compensation_pending 2\n"));
        assert!(text.contains("two_bot_voice_orphans_total 1\n"));
        // Fixed cardinality: 3x5 ops + 4 reconcile + 9 dead-letters.
        let mut series = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(series.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn voice_hostile_labels_collapse_without_new_series() {
        let metrics = Metrics::default();
        let before = metrics.render(None).lines().count();
        for id in 0..100 {
            let hostile = format!("{id}\"\\\nsecret=value");
            metrics.voice_operation(&hostile, &hostile);
            metrics.voice_reconcile(&hostile, 1);
            metrics.voice_dead_letter(&hostile);
            metrics.voice_vote_kick(&hostile);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        // `invalid_secret` is a legitimate bounded kind: match the hostile
        // `secret=value` injection marker, not the bare substring.
        assert!(!text.contains("secret="));
    }

    #[test]
    fn vote_kick_starts_refusals_and_enforcements_stay_bounded() {
        let metrics = Metrics::default();
        metrics.voice_vote_kick("started");
        metrics.voice_vote_kick("started");
        metrics.voice_vote_kick("cooldown");
        metrics.voice_vote_kick("initiator_limited");
        metrics.voice_vote_kick("connect_denied_and_disconnected");
        metrics.voice_vote_kick("gave_up");
        let text = metrics.render(None);
        assert!(text.contains("two_bot_voice_vote_kick_total{outcome=\"started\"} 2\n"));
        assert!(text.contains("two_bot_voice_vote_kick_total{outcome=\"cooldown\"} 1\n"));
        assert!(text.contains("two_bot_voice_vote_kick_total{outcome=\"initiator_limited\"} 1\n"));
        assert!(text.contains(
            "two_bot_voice_vote_kick_total{outcome=\"connect_denied_and_disconnected\"} 1\n"
        ));
        assert!(text.contains("two_bot_voice_vote_kick_total{outcome=\"gave_up\"} 1\n"));
        assert!(text.contains("two_bot_voice_vote_kick_total{outcome=\"other\"} 0\n"));
        // Fixed cardinality: every allowlisted outcome renders exactly once.
        let series: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("two_bot_voice_vote_kick_total{"))
            .collect();
        assert_eq!(series.len(), VOICE_VOTE_KICK_OUTCOMES.len());
        let mut unique = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(unique.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn vote_kick_counters_saturate_without_wrapping() {
        let metrics = Metrics::default();
        {
            let mut values = metrics.0.lock().unwrap();
            values.voice_vote_kick[bounded_index("started", VOICE_VOTE_KICK_OUTCOMES)] = u64::MAX;
        }
        metrics.voice_vote_kick("started");
        assert!(metrics.render(None).contains(&format!(
            "two_bot_voice_vote_kick_total{{outcome=\"started\"}} {}\n",
            u64::MAX
        )));
    }

    #[test]
    fn moderation_verbs_match_the_nine_command_names() {
        let names: Vec<&str> = crate::moderation::ModerationAction::ALL
            .iter()
            .map(|action| action.command_name())
            .collect();
        assert_eq!(names, MODERATION_VERBS);
        assert_eq!(MODERATION_VERBS.len(), 9);
        assert_eq!(MODERATION_ENFORCEMENT_OUTCOMES.len(), 5);
        assert_eq!(MODERATION_ENFORCEMENT_OUTCOMES.last(), Some(&"other"));
    }

    #[test]
    fn moderation_enforcements_cover_every_verb_and_outcome() {
        // TOG-28482 acceptance: each verb/outcome pair records with no network.
        let metrics = Metrics::default();
        for verb in MODERATION_VERBS {
            for outcome in MODERATION_ENFORCEMENT_OUTCOMES {
                metrics.moderation_enforcement(verb, outcome);
            }
        }
        let text = metrics.render(None);
        for verb in MODERATION_VERBS {
            for outcome in MODERATION_ENFORCEMENT_OUTCOMES {
                assert!(
                    text.contains(&format!(
                        "two_bot_moderation_enforcements_total{{verb=\"{verb}\",outcome=\"{outcome}\"}} 1\n"
                    )),
                    "missing {verb}/{outcome}"
                );
            }
        }
        // Fixed cardinality: 9 verbs x 5 outcomes.
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("two_bot_moderation_enforcements_total{"))
                .count(),
            MODERATION_VERBS.len() * MODERATION_ENFORCEMENT_OUTCOMES.len()
        );
        let mut series = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(series.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn moderation_hostile_labels_collapse_without_new_series() {
        let metrics = Metrics::default();
        let before = metrics.render(None).lines().count();
        for id in 0..100 {
            let hostile = format!("{id}\"\\\nsecret=value");
            metrics.moderation_enforcement(&hostile, &hostile);
            metrics.moderation_enforcement("ban", &hostile);
            metrics.moderation_enforcement(&hostile, "success");
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        // `invalid_secret` is a legitimate bounded kind: match the hostile
        // `secret=value` injection marker, not the bare substring.
        assert!(!text.contains("secret="));
    }

    #[test]
    fn db_and_admission_signals_stay_bounded_and_saturate() {
        let metrics = Metrics::default();
        metrics.db_error("admission");
        metrics.db_error("admission");
        metrics.send_admission("admitted");
        metrics.send_admission("blocked");
        metrics.send_admission("storage_error");
        let text = metrics.render(None);
        assert!(text.contains("two_bot_db_errors_total{op=\"admission\"} 2\n"));
        assert!(text.contains("two_bot_db_errors_total{op=\"other\"} 0\n"));
        assert!(text.contains("two_bot_send_admissions_total{outcome=\"admitted\"} 1\n"));
        assert!(text.contains("two_bot_send_admissions_total{outcome=\"blocked\"} 1\n"));
        assert!(text.contains("two_bot_send_admissions_total{outcome=\"storage_error\"} 1\n"));
        assert!(text.contains("two_bot_send_admissions_total{outcome=\"other\"} 0\n"));
        // Fixed cardinality: 2 db-error ops + 4 admission outcomes.
        let mut series = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(series.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn containment_store_errors_stay_bounded() {
        let metrics = Metrics::default();
        metrics.containment_store_error("complete_incident");
        metrics.containment_store_error("hostile\"\\\nlabel");
        let text = metrics.render(None);
        assert!(
            text.contains("two_bot_containment_store_errors_total{op=\"complete_incident\"} 1\n")
        );
        assert!(text.contains("two_bot_containment_store_errors_total{op=\"other\"} 1\n"));
    }

    #[test]
    fn prefix_trigger_refusals_stay_bounded_and_saturate() {
        // TOG-19932: verdict refusals count separately from unmatched content.
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(
            text.contains("two_bot_gateway_prefix_trigger_refused_total{reason=\"verdict\"} 0\n")
        );
        assert!(text.contains("two_bot_gateway_prefix_trigger_refused_total{reason=\"other\"} 0\n"));
        metrics.prefix_trigger_refused("verdict");
        metrics.prefix_trigger_refused("verdict");
        metrics.prefix_trigger_refused("hostile\"\\\nlabel");
        let text = metrics.render(None);
        assert!(
            text.contains("two_bot_gateway_prefix_trigger_refused_total{reason=\"verdict\"} 2\n")
        );
        assert!(text.contains("two_bot_gateway_prefix_trigger_refused_total{reason=\"other\"} 1\n"));
        assert!(!text.contains("hostile"));
        {
            let mut values = metrics.0.lock().unwrap();
            values.prefix_trigger_refused = [u64::MAX, u64::MAX];
        }
        metrics.prefix_trigger_refused("verdict");
        metrics.prefix_trigger_refused("other");
        let text = metrics.render(None);
        assert!(text.contains(&format!(
            "two_bot_gateway_prefix_trigger_refused_total{{reason=\"verdict\"}} {}\n",
            u64::MAX
        )));
        assert!(text.contains(&format!(
            "two_bot_gateway_prefix_trigger_refused_total{{reason=\"other\"}} {}\n",
            u64::MAX
        )));
        let mut series = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(series.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn dispatch_drops_stay_bounded_and_saturate() {
        let metrics = Metrics::default();
        metrics.dispatch_drop("messages");
        metrics.dispatch_drop("messages");
        metrics.dispatch_drop("busy");
        metrics.dispatch_drop("reactions");
        let text = metrics.render(None);
        assert!(text.contains("two_bot_dispatch_drops_total{lane=\"messages\"} 2\n"));
        assert!(text.contains("two_bot_dispatch_drops_total{lane=\"busy\"} 1\n"));
        assert!(text.contains("two_bot_dispatch_drops_total{lane=\"reactions\"} 1\n"));
        assert!(text.contains("two_bot_dispatch_drops_total{lane=\"interactions\"} 0\n"));
        // Fixed cardinality: six lanes.
        let mut series = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(series.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn checkpoint_failures_stay_bounded_and_saturate() {
        let metrics = Metrics::default();
        metrics.checkpoint_failure("pre_commit");
        metrics.checkpoint_failure("commit");
        metrics.checkpoint_failure("commit");
        let text = metrics.render(None);
        assert!(
            text.contains("two_bot_gateway_checkpoint_failures_total{stage=\"pre_commit\"} 1\n")
        );
        assert!(text.contains("two_bot_gateway_checkpoint_failures_total{stage=\"commit\"} 2\n"));
        // Fixed cardinality: two stages.
        let mut series = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(series.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn checkpoint_hostile_labels_collapse_without_new_series() {
        let metrics = Metrics::default();
        let before = metrics.render(None).lines().count();
        for id in 0..100 {
            let hostile = format!("{id}\"\\\nsecret=value");
            metrics.checkpoint_failure(&hostile);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        // `invalid_secret` is a legitimate bounded kind: match the hostile
        // `secret=value` injection marker, not the bare substring.
        assert!(!text.contains("secret="));
        assert!(text.contains("two_bot_gateway_checkpoint_failures_total{stage=\"commit\"} 100\n"));
    }

    #[test]
    fn drop_poisons_stay_bounded_and_count_per_guard() {
        let metrics = Metrics::default();
        metrics.drop_poison("command_dispatch_guard");
        metrics.drop_poison("schedule_run");
        metrics.drop_poison("user_slot");
        metrics.drop_poison("in_flight_guard");
        metrics.drop_poison("user_slot");
        metrics.drop_poison("ticket_supervisor");
        metrics.drop_poison("response_accounting");
        metrics.drop_poison("response_accounting");
        metrics.drop_poison("voice_connection_guard");
        let text = metrics.render(None);
        assert!(text.contains("two_bot_drop_poison_total{guard=\"command_dispatch_guard\"} 1\n"));
        assert!(text.contains("two_bot_drop_poison_total{guard=\"schedule_run\"} 1\n"));
        assert!(text.contains("two_bot_drop_poison_total{guard=\"user_slot\"} 2\n"));
        assert!(text.contains("two_bot_drop_poison_total{guard=\"in_flight_guard\"} 1\n"));
        assert!(text.contains("two_bot_drop_poison_total{guard=\"ticket_supervisor\"} 1\n"));
        assert!(text.contains("two_bot_drop_poison_total{guard=\"response_accounting\"} 2\n"));
        assert!(text.contains("two_bot_drop_poison_total{guard=\"voice_connection_guard\"} 1\n"));
        // Fixed cardinality: exactly the allowlisted series render.
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("two_bot_drop_poison_total{"))
                .count(),
            DROP_POISON_GUARDS.len()
        );
    }

    fn global_drop_poison_count(guard: &str) -> u64 {
        let prefix = format!("two_bot_drop_poison_total{{guard=\"{guard}\"}} ");
        global()
            .render(None)
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .and_then(|count| count.parse().ok())
            .expect("allowlisted guard series renders")
    }

    #[test]
    fn recover_drop_poison_counts_only_a_poisoned_lock() {
        // Only this test touches the `schedule_run` series in the core lib
        // test binary, so exact deltas on the process-global counter hold.
        let lock = Mutex::new(7_u32);
        let start = global_drop_poison_count("schedule_run");
        {
            let guard = recover_drop_poison(lock.lock(), "schedule_run");
            assert_eq!(*guard, 7);
        }
        assert_eq!(global_drop_poison_count("schedule_run"), start);

        let holder = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _held = lock.lock().expect("uncontended");
            std::panic::panic_any("poison the lock");
        }));
        assert!(holder.is_err());
        assert!(lock.is_poisoned());
        {
            let mut guard = recover_drop_poison(lock.lock(), "schedule_run");
            assert_eq!(*guard, 7);
            *guard = 8;
        }
        assert_eq!(global_drop_poison_count("schedule_run"), start + 1);
        // Poison is sticky: a second recovery counts again.
        drop(recover_drop_poison(lock.lock(), "schedule_run"));
        assert_eq!(global_drop_poison_count("schedule_run"), start + 2);
    }

    #[test]
    fn drop_poison_counters_saturate_at_u64_max() {
        // Seeds the atomic seam directly: no stable public API sets a
        // counter, and acceptance must not bind to the storage layout.
        let metrics = Metrics::default();
        let slot = bounded_index("user_slot", DROP_POISON_GUARDS);
        metrics.1[slot].store(u64::MAX - 1, Ordering::Relaxed);
        metrics.drop_poison("user_slot");
        metrics.drop_poison("user_slot");
        metrics.drop_poison("user_slot");
        let text = metrics.render(None);
        assert!(text.contains(&format!(
            "two_bot_drop_poison_total{{guard=\"user_slot\"}} {}\n",
            u64::MAX
        )));
    }

    #[test]
    fn drop_poison_hostile_labels_collapse_without_new_series() {
        let metrics = Metrics::default();
        let before = metrics.render(None).lines().count();
        for id in 0..100 {
            let hostile = format!("{id}\"\\\nsecret=value");
            metrics.drop_poison(&hostile);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        // `invalid_secret` is a legitimate bounded kind: match the hostile
        // `secret=value` injection marker, not the bare substring.
        assert!(!text.contains("secret="));
        assert!(text.contains("two_bot_drop_poison_total{guard=\"voice_connection_guard\"} 100\n"));
    }

    #[test]
    fn gateway_snapshot_reports_counters_and_last_ready_resume() {
        let metrics = Metrics::default();
        let fresh = metrics.gateway_snapshot();
        assert_eq!(fresh.reconnects, 0);
        assert_eq!(fresh.resumes, 0);
        assert_eq!(fresh.disconnects, 0);
        assert_eq!(fresh.missed_events, 0);
        assert_eq!(fresh.last_ready_unix, None);
        assert_eq!(fresh.last_resume_unix, None);
        // Vocabulary matches the exposition allowlists, in order.
        let names: Vec<&str> = fresh.events.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, EVENTS);
        let stages: Vec<&str> = fresh
            .checkpoint_failures
            .iter()
            .map(|(stage, _)| stage.as_str())
            .collect();
        assert_eq!(stages, CHECKPOINT_FAILURE_STAGES);

        metrics.gateway_event("READY");
        metrics.gateway_event("RESUMED");
        metrics.gateway_event("RESUMED");
        metrics.gateway_reconnect();
        metrics.gateway_disconnect();
        metrics.gateway_missed_events(3);
        metrics.checkpoint_failure("pre_commit");
        let snapshot = metrics.gateway_snapshot();
        assert_eq!(snapshot.reconnects, 1);
        assert_eq!(snapshot.resumes, 2);
        assert_eq!(snapshot.disconnects, 1);
        assert_eq!(snapshot.missed_events, 3);
        let ready = snapshot
            .events
            .iter()
            .find(|(name, _)| name == "READY")
            .map(|(_, count)| *count);
        assert_eq!(ready, Some(1));
        let resumed = snapshot
            .events
            .iter()
            .find(|(name, _)| name == "RESUMED")
            .map(|(_, count)| *count);
        assert_eq!(resumed, Some(2));
        let pre_commit = snapshot
            .checkpoint_failures
            .iter()
            .find(|(stage, _)| stage == "pre_commit")
            .map(|(_, count)| *count);
        assert_eq!(pre_commit, Some(1));
        let before = unix_now();
        assert!(snapshot.last_ready_unix.is_some());
        assert!(snapshot.last_resume_unix.is_some());
        assert!(snapshot.last_ready_unix.unwrap() <= before);
        assert!(snapshot.last_resume_unix.unwrap() <= before);
        // The snapshot serializes for the JSON endpoint.
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["reconnects"], 1);
        assert_eq!(json["resumes"], 2);
    }

    #[test]
    fn dispatch_hostile_labels_collapse_without_new_series() {
        let metrics = Metrics::default();
        let before = metrics.render(None).lines().count();
        for id in 0..100 {
            let hostile = format!("{id}\"\\\nsecret=value");
            metrics.dispatch_drop(&hostile);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        // `invalid_secret` is a legitimate bounded kind: match the hostile
        // `secret=value` injection marker, not the bare substring.
        assert!(!text.contains("secret="));
    }

    #[test]
    fn internal_actions_count_executed_and_refused_per_family() {
        let metrics = Metrics::default();
        // Executed membership action (role.assign served through the signed
        // receiver) and a refused unknown verb (other family, unknown_action).
        metrics.internal_action("membership", "executed");
        metrics.internal_action("membership", "executed");
        metrics.internal_action("other", "unknown_action");
        metrics.internal_action("announcement", "executed");
        let text = metrics.render(None);
        assert!(text.contains(
            "two_bot_internal_actions_total{family=\"membership\",outcome=\"executed\"} 2\n"
        ));
        assert!(text.contains(
            "two_bot_internal_actions_total{family=\"other\",outcome=\"unknown_action\"} 1\n"
        ));
        assert!(text.contains(
            "two_bot_internal_actions_total{family=\"announcement\",outcome=\"executed\"} 1\n"
        ));
        assert!(text
            .contains("two_bot_internal_actions_total{family=\"event\",outcome=\"executed\"} 0\n"));
        // Fixed cardinality: six families by twelve outcomes.
        let mut series = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(series.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn internal_action_hostile_labels_collapse_without_new_series() {
        let metrics = Metrics::default();
        let before = metrics.render(None).lines().count();
        for id in 0..100 {
            let hostile = format!("{id}\"\\\nsecret=value");
            metrics.internal_action(&hostile, &hostile);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        // `invalid_secret` is a legitimate bounded kind: match the hostile
        // `secret=value` injection marker, not the bare substring.
        assert!(!text.contains("secret="));
    }

    #[test]
    fn community_facts_drain_failures_count_per_reason() {
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("two_bot_community_facts_drain_failures_total{reason=\"error\"} 0\n"));
        assert!(
            text.contains("two_bot_community_facts_drain_failures_total{reason=\"timeout\"} 0\n")
        );
        metrics.community_facts_drain_failure("error");
        metrics.community_facts_drain_failure("error");
        metrics.community_facts_drain_failure("timeout");
        let text = metrics.render(None);
        assert!(text.contains("two_bot_community_facts_drain_failures_total{reason=\"error\"} 2\n"));
        assert!(
            text.contains("two_bot_community_facts_drain_failures_total{reason=\"timeout\"} 1\n")
        );
    }

    #[test]
    fn community_facts_drain_hostile_reasons_collapse_to_other() {
        let metrics = Metrics::default();
        let before = metrics.render(None).lines().count();
        for id in 0..100 {
            let hostile = format!("{id}\"\\\nsecret=value");
            metrics.community_facts_drain_failure(&hostile);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        assert!(
            text.contains("two_bot_community_facts_drain_failures_total{reason=\"other\"} 100\n")
        );
        // `invalid_secret` is a legitimate bounded kind: match the hostile
        // `secret=value` injection marker, not the bare substring.
        assert!(!text.contains("secret="));
    }

    #[test]
    fn audit_delivery_halt_gauge_is_a_label_free_switch() {
        // M4.48: the kill-switch state renders as one gauge series with no
        // labels — 1 engaged, 0 cleared — so no actor ID, timestamp, error
        // text or row content can reach exposition.
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("# TYPE two_bot_audit_delivery_halt gauge\n"));
        assert!(text.contains("two_bot_audit_delivery_halt 0\n"));
        metrics.set_audit_delivery_halt(true);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_audit_delivery_halt 1\n"));
        assert!(!text.contains("two_bot_audit_delivery_halt{"));
        // Clearing returns to zero; the last writer wins.
        metrics.set_audit_delivery_halt(false);
        metrics.set_audit_delivery_halt(false);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_audit_delivery_halt 0\n"));
        metrics.set_audit_delivery_halt(true);
        metrics.set_audit_delivery_halt(false);
        metrics.set_audit_delivery_halt(true);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_audit_delivery_halt 1\n"));
        // Fixed cardinality: exactly one series, a parseable number.
        let series: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("two_bot_audit_delivery_halt "))
            .collect();
        assert_eq!(series.len(), 1);
        let (_, value) = series[0].rsplit_once(' ').unwrap();
        assert!(value.parse::<f64>().is_ok(), "bad sample: {}", series[0]);
        let mut unique = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, sample) = line.rsplit_once(' ').unwrap();
            assert!(unique.insert(key), "duplicate series: {key}");
            assert!(sample.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn retention_replay_needed_counter_is_label_free_and_zero_fresh() {
        // TOG-29785: skipped purges render as one label-free counter series —
        // 0 on a fresh registry, +1 per purge-skip — so no guild id, SQLSTATE
        // or error text can reach exposition.
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("# TYPE two_bot_retention_replay_needed_total counter\n"));
        assert!(text.contains("two_bot_retention_replay_needed_total 0\n"));
        metrics.retention_replay_needed();
        metrics.retention_replay_needed();
        let text = metrics.render(None);
        assert!(text.contains("two_bot_retention_replay_needed_total 2\n"));
        assert!(!text.contains("two_bot_retention_replay_needed_total{"));
        let series: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("two_bot_retention_replay_needed_total "))
            .collect();
        assert_eq!(series.len(), 1);
        let (_, value) = series[0].rsplit_once(' ').unwrap();
        assert!(value.parse::<f64>().is_ok(), "bad sample: {}", series[0]);
    }

    #[test]
    fn gateway_drain_skips_counter_is_label_free_and_saturates() {
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("# TYPE two_bot_gateway_drain_skips_total counter\n"));
        assert!(text.contains("two_bot_gateway_drain_skips_total 0\n"));
        metrics.gateway_drain_skip();
        metrics.gateway_drain_skip();
        let text = metrics.render(None);
        assert!(text.contains("two_bot_gateway_drain_skips_total 2\n"));
        assert!(!text.contains("two_bot_gateway_drain_skips_total{"));
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("two_bot_gateway_drain_skips_total "))
                .count(),
            1
        );
        metrics.0.lock().unwrap().gateway_drain_skips = u64::MAX;
        metrics.gateway_drain_skip();
        assert!(metrics
            .render(None)
            .contains(&format!("two_bot_gateway_drain_skips_total {}\n", u64::MAX)));
    }

    #[test]
    fn ticket_component_drops_counter_is_label_free_and_zero_fresh() {
        // Refused ticket components render as one label-free counter series —
        // 0 on a fresh registry, +1 per refusal — so no member id, token or
        // interaction id can reach exposition.
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("# TYPE two_bot_ticket_component_drops_total counter\n"));
        assert!(text.contains("two_bot_ticket_component_drops_total 0\n"));
        metrics.ticket_component_drop();
        metrics.ticket_component_drop();
        let text = metrics.render(None);
        assert!(text.contains("two_bot_ticket_component_drops_total 2\n"));
        assert!(!text.contains("two_bot_ticket_component_drops_total{"));
        let series: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("two_bot_ticket_component_drops_total "))
            .collect();
        assert_eq!(series.len(), 1);
        let (_, value) = series[0].rsplit_once(' ').unwrap();
        assert!(value.parse::<f64>().is_ok(), "bad sample: {}", series[0]);
    }

    /// Adapter: today nothing counts voice overflow drops, so the adapter is
    /// a no-op. After the Builder adds inherent
    /// `Metrics::voice_inbox_overflow_dropped` /
    /// `Metrics::voice_queue_overflow_dropped`, method resolution prefers the
    /// inherent methods and the tests below exercise the real counters. This
    /// is what makes RED behavioral: this file compiles today and the
    /// assertions fail on the missing series.
    trait VoiceOverflowCounters {
        fn voice_inbox_overflow_dropped(&self) {}
        fn voice_queue_overflow_dropped(&self) {}
    }

    impl VoiceOverflowCounters for Metrics {}

    #[test]
    fn voice_inbox_overflow_dropped_counter_is_label_free_and_zero_fresh() {
        // Refused voice actor inbox sends render as one label-free counter
        // series — 0 on a fresh registry, +1 per refused send — so no guild,
        // member or channel label can reach exposition.
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("# TYPE two_bot_voice_inbox_overflow_dropped_total counter\n"));
        assert!(text.contains("two_bot_voice_inbox_overflow_dropped_total 0\n"));
        metrics.voice_inbox_overflow_dropped();
        metrics.voice_inbox_overflow_dropped();
        let text = metrics.render(None);
        assert!(text.contains("two_bot_voice_inbox_overflow_dropped_total 2\n"));
        assert!(!text.contains("two_bot_voice_inbox_overflow_dropped_total{"));
        let series: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("two_bot_voice_inbox_overflow_dropped_total "))
            .collect();
        assert_eq!(series.len(), 1);
        let (_, value) = series[0].rsplit_once(' ').unwrap();
        assert!(value.parse::<f64>().is_ok(), "bad sample: {}", series[0]);
    }

    #[test]
    fn voice_queue_overflow_dropped_counter_is_label_free_and_zero_fresh() {
        // Refused voice action queue enqueues render as one label-free counter
        // series — 0 on a fresh registry, +1 per refused enqueue — so no
        // guild or channel label can reach exposition.
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("# TYPE two_bot_voice_queue_overflow_dropped_total counter\n"));
        assert!(text.contains("two_bot_voice_queue_overflow_dropped_total 0\n"));
        metrics.voice_queue_overflow_dropped();
        metrics.voice_queue_overflow_dropped();
        let text = metrics.render(None);
        assert!(text.contains("two_bot_voice_queue_overflow_dropped_total 2\n"));
        assert!(!text.contains("two_bot_voice_queue_overflow_dropped_total{"));
        let series: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("two_bot_voice_queue_overflow_dropped_total "))
            .collect();
        assert_eq!(series.len(), 1);
        let (_, value) = series[0].rsplit_once(' ').unwrap();
        assert!(value.parse::<f64>().is_ok(), "bad sample: {}", series[0]);
    }

    #[test]
    fn voice_overflow_dropped_counters_are_isolated_from_each_other() {
        // The inbox and queue overflow meters count independently: bumping one
        // never moves the other.
        let metrics = Metrics::default();
        metrics.voice_inbox_overflow_dropped();
        metrics.voice_inbox_overflow_dropped();
        metrics.voice_queue_overflow_dropped();
        let text = metrics.render(None);
        assert!(text.contains("two_bot_voice_inbox_overflow_dropped_total 2\n"));
        assert!(text.contains("two_bot_voice_queue_overflow_dropped_total 1\n"));
    }

    #[test]
    fn voice_overflow_legacy_adapter_path_stays_available() {
        // Control pinning the legacy adapter path: qualified calls bypass any
        // inherent overflow gate by design, so this passes both before and
        // after the Builder adds inherent `voice_inbox_overflow_dropped` /
        // `voice_queue_overflow_dropped`. It also keeps the adapter trait
        // referenced once inherent methods shadow unqualified calls, avoiding
        // never-used warnings under `-D warnings`.
        let metrics = Metrics::default();
        <Metrics as VoiceOverflowCounters>::voice_inbox_overflow_dropped(&metrics);
        <Metrics as VoiceOverflowCounters>::voice_queue_overflow_dropped(&metrics);
    }

    #[test]
    fn voice_actor_fair_scheduling_series_are_label_free() {
        // The fair-scheduling diagnosis is one gauge (the bound) plus two
        // counters (inbox turns vs dispatch turns), all label-free, so no
        // guild, member or channel label can reach exposition. The bound
        // reads 0 until the first actor publishes; the counters only move
        // when their turn kind runs.
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("# TYPE two_bot_voice_actor_inbox_turn_bound gauge\n"));
        assert!(text.contains("two_bot_voice_actor_inbox_turn_bound 0\n"));
        assert!(text.contains("# TYPE two_bot_voice_actor_inbox_turns_total counter\n"));
        assert!(text.contains("two_bot_voice_actor_inbox_turns_total 0\n"));
        assert!(text.contains("# TYPE two_bot_voice_actor_dispatch_turns_total counter\n"));
        assert!(text.contains("two_bot_voice_actor_dispatch_turns_total 0\n"));
        metrics.set_voice_actor_inbox_turn_bound(32);
        metrics.voice_actor_inbox_turn();
        metrics.voice_actor_inbox_turn();
        metrics.voice_actor_dispatch_turn();
        let text = metrics.render(None);
        assert!(text.contains("two_bot_voice_actor_inbox_turn_bound 32\n"));
        assert!(text.contains("two_bot_voice_actor_inbox_turns_total 2\n"));
        assert!(text.contains("two_bot_voice_actor_dispatch_turns_total 1\n"));
        assert!(!text.contains("two_bot_voice_actor_inbox_turn_bound{"));
        assert!(!text.contains("two_bot_voice_actor_inbox_turns_total{"));
        assert!(!text.contains("two_bot_voice_actor_dispatch_turns_total{"));
        // Last writer wins for the gauge.
        metrics.set_voice_actor_inbox_turn_bound(8);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_voice_actor_inbox_turn_bound 8\n"));
    }

    #[test]
    fn event_store_consumer_lag_gauge_is_a_label_free_duration() {
        // S1 AC 4: the consumer lag setter renders as one label-free gauge
        // series — the newest observed `received_at - event_time` spread, 0
        // when unobserved — so no message, channel, user, dedup-key, content
        // or body label can reach exposition. Non-finite or negative input
        // clears to 0 so exposition stays a finite non-negative duration.
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("# TYPE two_bot_event_store_consumer_lag_seconds gauge\n"));
        assert!(text.contains("two_bot_event_store_consumer_lag_seconds 0\n"));
        metrics.set_event_store_consumer_lag_secs(1.5);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_event_store_consumer_lag_seconds 1.5\n"));
        assert!(!text.contains("two_bot_event_store_consumer_lag_seconds{"));
        // The clamp is pinned: NaN, negative and infinite input must clear
        // to 0 rather than pass through to exposition.
        for bad in [f64::NAN, -0.25, f64::NEG_INFINITY, f64::INFINITY] {
            metrics.set_event_store_consumer_lag_secs(bad);
            let text = metrics.render(None);
            assert!(
                text.contains("two_bot_event_store_consumer_lag_seconds 0\n"),
                "clamp must clear {bad:?} to 0"
            );
        }
        // Last writer wins.
        metrics.set_event_store_consumer_lag_secs(2.25);
        metrics.set_event_store_consumer_lag_secs(0.5);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_event_store_consumer_lag_seconds 0.5\n"));
        // Fixed cardinality: exactly one series, a finite non-negative number.
        let series: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("two_bot_event_store_consumer_lag_seconds "))
            .collect();
        assert_eq!(series.len(), 1);
        let (_, value) = series[0].rsplit_once(' ').unwrap();
        let parsed: f64 = value.parse().expect("bad sample");
        assert!(
            parsed.is_finite() && parsed >= 0.0,
            "bad sample: {}",
            series[0]
        );
    }

    #[test]
    fn creator_map_gauges_are_label_free_generation_and_age() {
        // Observability v1: the creator-map freshness renders as two label-free
        // gauge series — the last merged generation and the seconds since that
        // merge, both 0 before the first merge — so no channel, creator,
        // guild or timestamp label can reach exposition.
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("# TYPE two_bot_voice_creator_map_generation gauge\n"));
        assert!(text.contains("two_bot_voice_creator_map_generation 0\n"));
        assert!(text.contains("# TYPE two_bot_voice_creator_map_age_seconds gauge\n"));
        assert!(text.contains("two_bot_voice_creator_map_age_seconds 0\n"));
        metrics.set_voice_creator_map(7, 42);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_voice_creator_map_generation 7\n"));
        assert!(text.contains("two_bot_voice_creator_map_age_seconds 42\n"));
        assert!(!text.contains("two_bot_voice_creator_map_generation{"));
        assert!(!text.contains("two_bot_voice_creator_map_age_seconds{"));
        // Last writer wins on both series together.
        metrics.set_voice_creator_map(9, 300);
        metrics.set_voice_creator_map(8, 301);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_voice_creator_map_generation 8\n"));
        assert!(text.contains("two_bot_voice_creator_map_age_seconds 301\n"));
        // Fixed cardinality: exactly one series each, parseable numbers.
        for name in [
            "two_bot_voice_creator_map_generation",
            "two_bot_voice_creator_map_age_seconds",
        ] {
            let series: Vec<_> = text
                .lines()
                .filter(|line| line.starts_with(&format!("{name} ")))
                .collect();
            assert_eq!(series.len(), 1, "missing series: {name}");
            let (_, value) = series[0].rsplit_once(' ').unwrap();
            value.parse::<u64>().expect("bad sample");
        }
    }

    #[test]
    fn live_not_ready_gauge_is_the_label_free_maximum_across_actors() {
        // A live voice view left not ready on a connected gateway drops every
        // creator join silently (production 2026-10-17/18). The host watchdog
        // alerts on this gauge directly; a healthy guild must not hide it.
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("# TYPE two_bot_voice_live_not_ready_seconds gauge\n"));
        assert!(text.contains("two_bot_voice_live_not_ready_seconds 0\n"));
        assert!(text.contains("two_bot_voice_live_self_heal_total 0\n"));
        metrics.set_voice_actor_live_not_ready(1, 1, 0);
        metrics.set_voice_actor_live_not_ready(2, 1, 95);
        assert_eq!(metrics.voice_actor_live_not_ready_secs(1), Some(0));
        assert_eq!(metrics.voice_actor_live_not_ready_secs(2), Some(95));
        assert_eq!(metrics.voice_actor_live_not_ready_secs(3), None);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_voice_live_not_ready_seconds 95\n"));
        assert!(!text.contains("two_bot_voice_live_not_ready_seconds{"));
        // A stale incarnation cannot overwrite; a newer one resets the entry.
        metrics.set_voice_actor_halted(2, 2, false);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_voice_live_not_ready_seconds 0\n"));
        metrics.set_voice_actor_live_not_ready(2, 1, 400);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_voice_live_not_ready_seconds 0\n"));
        // Retiring the stuck actor removes its contribution.
        metrics.set_voice_actor_live_not_ready(2, 2, 30);
        metrics.remove_voice_actor(2, 2);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_voice_live_not_ready_seconds 0\n"));
        metrics.voice_live_self_heal();
        metrics.voice_live_self_heal();
        let text = metrics.render(None);
        assert!(text.contains("# TYPE two_bot_voice_live_self_heal_total counter\n"));
        assert!(text.contains("two_bot_voice_live_self_heal_total 2\n"));
    }

    #[test]
    fn rest_errors_and_reconnects_are_distinguished() {
        let metrics = Metrics::default();
        for status in [Some(204), Some(302), Some(403), Some(429), Some(503), None] {
            metrics.rest_response(REST_ROUTES[0], status);
        }
        metrics.gateway_latency(Duration::from_millis(20));
        metrics.gateway_reconnect();
        let text = metrics.render(None);
        for result in RESULTS {
            assert!(text.contains(&format!(
                "route=\"{}\",result=\"{result}\"}} 1\n",
                REST_ROUTES[0]
            )));
        }
        assert!(text.contains("two_bot_gateway_reconnects_total 1\n"));
        assert!(text.contains("two_bot_gateway_latency_seconds NaN\n"));
    }

    #[test]
    fn eventsub_reconcile_signals_stay_bounded_and_saturate() {
        // Reconcile health: one pass series per outcome, one action series per
        // mutation kind, one error series per log error_class. Per-subscription
        // labels never reach exposition.
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"healthy\"} 0\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"pending\"} 0\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"failed\"} 0\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_actions_total{action=\"create\"} 0\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_actions_total{action=\"delete\"} 0\n"));
        assert!(text.contains(
            "two_bot_eventsub_reconcile_errors_total{error_class=\"reconcile_unhealthy\"} 0\n"
        ));
        metrics.eventsub_reconcile_pass("healthy");
        metrics.eventsub_reconcile_pass("pending");
        metrics.eventsub_reconcile_pass("failed");
        metrics.eventsub_reconcile_action("create", 2);
        metrics.eventsub_reconcile_action("delete", 1);
        metrics.eventsub_reconcile_action("create", 0);
        metrics.eventsub_reconcile_error("reconcile_action_failed");
        metrics.eventsub_reconcile_error("reconcile_prior_failed");
        metrics.eventsub_reconcile_error("reconcile_unhealthy");
        metrics.eventsub_reconcile_error("reconcile_failed");
        let text = metrics.render(None);
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"healthy\"} 1\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"pending\"} 1\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"failed\"} 1\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_actions_total{action=\"create\"} 2\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_actions_total{action=\"delete\"} 1\n"));
        assert!(text.contains(
            "two_bot_eventsub_reconcile_errors_total{error_class=\"reconcile_action_failed\"} 1\n"
        ));
        assert!(text.contains(
            "two_bot_eventsub_reconcile_errors_total{error_class=\"reconcile_prior_failed\"} 1\n"
        ));
        assert!(text.contains(
            "two_bot_eventsub_reconcile_errors_total{error_class=\"reconcile_unhealthy\"} 1\n"
        ));
        assert!(text.contains(
            "two_bot_eventsub_reconcile_errors_total{error_class=\"reconcile_failed\"} 1\n"
        ));
        // Fixed cardinality: 3 pass outcomes + 2 actions + 5 error classes.
        let series: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("two_bot_eventsub_reconcile_"))
            .collect();
        assert_eq!(
            series.len(),
            EVENTSUB_RECONCILE_OUTCOMES.len()
                + EVENTSUB_RECONCILE_ACTIONS.len()
                + EVENTSUB_RECONCILE_ERROR_CLASSES.len()
        );
        let mut unique = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(unique.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn eventsub_reconcile_hostile_labels_collapse_without_new_series() {
        let metrics = Metrics::default();
        let before = metrics.render(None).lines().count();
        for id in 0..100 {
            let hostile = format!("{id}\"\\\nsecret=value");
            metrics.eventsub_reconcile_pass(&hostile);
            metrics.eventsub_reconcile_action(&hostile, 1);
            metrics.eventsub_reconcile_error(&hostile);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        // `invalid_secret` is a legitimate bounded kind: match the hostile
        // `secret=value` injection marker, not the bare substring.
        assert!(!text.contains("secret="));
        assert!(text.contains("two_bot_eventsub_reconcile_passes_total{outcome=\"failed\"} 100\n"));
        assert!(text.contains("two_bot_eventsub_reconcile_actions_total{action=\"delete\"} 100\n"));
        assert!(
            text.contains("two_bot_eventsub_reconcile_errors_total{error_class=\"other\"} 100\n")
        );
    }

    #[test]
    fn ticket_lifecycle_counts_once_per_event_and_by_deleted_count() {
        let metrics = Metrics::default();
        metrics.ticket_lifecycle("retire", "retired");
        metrics.ticket_lifecycle("abandon", "already_gone");
        metrics.ticket_lifecycle("claim", "claimed");
        metrics.ticket_lifecycle("close", "closed");
        metrics.ticket_lifecycle_n("purge", "deleted", 2);
        metrics.ticket_lifecycle_n("purge", "deleted", 0);
        metrics.ticket_lifecycle("purge", "pool_closed");
        let text = metrics.render(None);
        assert!(text
            .contains("two_bot_ticket_lifecycle_total{event=\"retire\",outcome=\"retired\"} 1\n"));
        assert!(text.contains(
            "two_bot_ticket_lifecycle_total{event=\"abandon\",outcome=\"already_gone\"} 1\n"
        ));
        assert!(text
            .contains("two_bot_ticket_lifecycle_total{event=\"claim\",outcome=\"claimed\"} 1\n"));
        assert!(
            text.contains("two_bot_ticket_lifecycle_total{event=\"close\",outcome=\"closed\"} 1\n")
        );
        assert!(text
            .contains("two_bot_ticket_lifecycle_total{event=\"purge\",outcome=\"deleted\"} 2\n"));
        assert!(text.contains(
            "two_bot_ticket_lifecycle_total{event=\"purge\",outcome=\"pool_closed\"} 1\n"
        ));
        // Fixed cardinality: per-event render subsets (22 series), not the
        // full 5 x 18 cross product, so exposition fits the Worker scrape
        // body cap. Every event renders its success outcomes plus the
        // `query` collapse trapdoor; purge adds the full `db_cause` set.
        let series: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("two_bot_ticket_lifecycle_total{"))
            .collect();
        assert_eq!(series.len(), 22);
        for (event, outcomes) in [
            ("retire", vec!["retired", "query"]),
            ("abandon", vec!["abandoned", "already_gone", "query"]),
            (
                "purge",
                vec![
                    "deleted",
                    "unique_violation",
                    "foreign_key_violation",
                    "not_null_violation",
                    "check_violation",
                    "exclusion_violation",
                    "database",
                    "pool_timeout",
                    "pool_closed",
                    "worker_crashed",
                    "connection",
                    "row_not_found",
                    "query",
                ],
            ),
            ("claim", vec!["claimed", "query"]),
            ("close", vec!["closed", "query"]),
        ] {
            let prefix = format!("two_bot_ticket_lifecycle_total{{event=\"{event}\",outcome=\"");
            let rendered: Vec<_> = series
                .iter()
                .filter_map(|line| line.strip_prefix(prefix.as_str()))
                .filter_map(|rest| rest.split_once('"').map(|(outcome, _)| outcome))
                .collect();
            assert_eq!(rendered, outcomes, "render subset for {event}");
        }
        assert_eq!(TICKET_LIFECYCLE_EVENTS.len(), 5);
        assert_eq!(TICKET_LIFECYCLE_OUTCOMES.len(), 18);
    }

    #[test]
    fn lifecycle_render_subsets_are_closed_world() {
        // Unknown events never render, and known events render only their
        // subset: a pair no emitter can produce stays out of exposition.
        assert!(!lifecycle_series_rendered("evil", "retired"));
        assert!(!lifecycle_series_rendered("retire", "evil"));
        assert!(!lifecycle_series_rendered("retire", "deleted"));
        assert!(!lifecycle_series_rendered("claim", "pool_closed"));
        assert!(lifecycle_series_rendered("purge", "pool_closed"));
        assert!(lifecycle_series_rendered("close", "query"));
    }

    #[test]
    fn ticket_lifecycle_hostile_labels_collapse_without_new_series() {
        let metrics = Metrics::default();
        let before = metrics.render(None).lines().count();
        for id in 0..100 {
            let hostile = format!("{id}\"\\\nsecret=value");
            metrics.ticket_lifecycle(&hostile, &hostile);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        // `invalid_secret` is a legitimate bounded kind: match the hostile
        // `secret=value` injection marker, not the bare substring.
        assert!(!text.contains("secret="));
        assert!(text
            .contains("two_bot_ticket_lifecycle_total{event=\"close\",outcome=\"query\"} 100\n"));
    }

    #[test]
    fn eventsub_reconcile_counters_saturate_without_wrapping() {
        let metrics = Metrics::default();
        {
            let mut values = metrics.0.lock().unwrap();
            values.eventsub_reconcile_passes
                [bounded_index("healthy", EVENTSUB_RECONCILE_OUTCOMES)] = u64::MAX;
            values.eventsub_reconcile_actions
                [bounded_index("create", EVENTSUB_RECONCILE_ACTIONS)] = u64::MAX;
        }
        metrics.eventsub_reconcile_pass("healthy");
        metrics.eventsub_reconcile_action("create", 9);
        let text = metrics.render(None);
        assert!(text.contains(&format!(
            "two_bot_eventsub_reconcile_passes_total{{outcome=\"healthy\"}} {}\n",
            u64::MAX
        )));
        assert!(text.contains(&format!(
            "two_bot_eventsub_reconcile_actions_total{{action=\"create\"}} {}\n",
            u64::MAX
        )));
    }

    #[test]
    fn disconnects_and_missed_events_start_at_zero_and_saturate() {
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("two_bot_gateway_disconnects_total 0\n"));
        assert!(text.contains("two_bot_gateway_missed_events_total 0\n"));
        assert!(text.contains("two_bot_gateway_rsvp_refused_total 0\n"));
        metrics.gateway_disconnect();
        metrics.gateway_missed_events(0);
        metrics.gateway_missed_events(3);
        metrics.gateway_rsvp_refused();
        let text = metrics.render(None);
        assert!(text.contains("two_bot_gateway_disconnects_total 1\n"));
        assert!(text.contains("two_bot_gateway_missed_events_total 3\n"));
        assert!(text.contains("two_bot_gateway_rsvp_refused_total 1\n"));
        {
            let mut values = metrics.0.lock().unwrap();
            values.disconnects = u64::MAX;
            values.missed_events = u64::MAX;
            values.rsvp_refused = u64::MAX;
        }
        metrics.gateway_disconnect();
        metrics.gateway_missed_events(9);
        metrics.gateway_rsvp_refused();
        let text = metrics.render(None);
        assert!(text.contains(&format!("two_bot_gateway_disconnects_total {}\n", u64::MAX)));
        assert!(text.contains(&format!(
            "two_bot_gateway_missed_events_total {}\n",
            u64::MAX
        )));
        assert!(text.contains(&format!(
            "two_bot_gateway_rsvp_refused_total {}\n",
            u64::MAX
        )));
    }

    #[test]
    fn feed_fetches_stay_bounded_and_saturate() {
        // TOG-28423: per-outcome, never per-feed. Four outcome series plus
        // twelve error-class series exist from process start at zero.
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("# TYPE two_bot_feed_fetches_total counter\n"));
        assert!(text.contains("# TYPE two_bot_feed_fetch_errors_total counter\n"));
        for outcome in FEED_FETCH_OUTCOMES {
            assert!(text.contains(&format!(
                "two_bot_feed_fetches_total{{outcome=\"{outcome}\"}} 0\n"
            )));
        }
        for error_class in FEED_FETCH_ERROR_CLASSES {
            assert!(text.contains(&format!(
                "two_bot_feed_fetch_errors_total{{error_class=\"{error_class}\"}} 0\n"
            )));
        }
        assert_eq!(FEED_FETCH_OUTCOMES.len(), 4);
        assert_eq!(FEED_FETCH_ERROR_CLASSES.len(), 12);
        metrics.feed_fetch("success");
        metrics.feed_fetch("invalid_source");
        metrics.feed_fetch("refused_redirect");
        metrics.feed_fetch("failure");
        metrics.feed_fetch_error("invalid_source");
        metrics.feed_fetch_error("redirect_refused");
        metrics.feed_fetch_error("http_error");
        metrics.feed_fetch_error("transport");
        let text = metrics.render(None);
        assert!(text.contains("two_bot_feed_fetches_total{outcome=\"success\"} 1\n"));
        assert!(text.contains("two_bot_feed_fetches_total{outcome=\"invalid_source\"} 1\n"));
        assert!(text.contains("two_bot_feed_fetches_total{outcome=\"refused_redirect\"} 1\n"));
        assert!(text.contains("two_bot_feed_fetches_total{outcome=\"failure\"} 1\n"));
        assert!(
            text.contains("two_bot_feed_fetch_errors_total{error_class=\"invalid_source\"} 1\n")
        );
        assert!(
            text.contains("two_bot_feed_fetch_errors_total{error_class=\"redirect_refused\"} 1\n")
        );
        assert!(text.contains("two_bot_feed_fetch_errors_total{error_class=\"http_error\"} 1\n"));
        assert!(text.contains("two_bot_feed_fetch_errors_total{error_class=\"transport\"} 1\n"));
        // Fixed cardinality: exactly the allowlisted series render.
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("two_bot_feed_fetches_total{"))
                .count(),
            FEED_FETCH_OUTCOMES.len()
        );
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("two_bot_feed_fetch_errors_total{"))
                .count(),
            FEED_FETCH_ERROR_CLASSES.len()
        );
        {
            let mut values = metrics.0.lock().unwrap();
            values.feed_fetch = [u64::MAX; FEED_FETCH_OUTCOMES.len()];
            values.feed_fetch_errors = [u64::MAX; FEED_FETCH_ERROR_CLASSES.len()];
        }
        metrics.feed_fetch("success");
        metrics.feed_fetch_error("transport");
        let text = metrics.render(None);
        assert!(text.contains(&format!(
            "two_bot_feed_fetches_total{{outcome=\"success\"}} {}\n",
            u64::MAX
        )));
        assert!(text.contains(&format!(
            "two_bot_feed_fetch_errors_total{{error_class=\"transport\"}} {}\n",
            u64::MAX
        )));
        let mut series = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(series.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn feed_fetch_hostile_labels_collapse_without_new_series() {
        let metrics = Metrics::default();
        let before = metrics.render(None).lines().count();
        for id in 0..100 {
            let hostile = format!("{id}\"\\\nsecret=value");
            metrics.feed_fetch(&hostile);
            metrics.feed_fetch_error(&hostile);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        // `invalid_secret` is a legitimate bounded kind: match the hostile
        // `secret=value` injection marker, not the bare substring.
        assert!(!text.contains("secret="));
        assert!(text.contains("two_bot_feed_fetches_total{outcome=\"failure\"} 100\n"));
        assert!(text.contains("two_bot_feed_fetch_errors_total{error_class=\"other\"} 100\n"));
    }

    #[test]
    fn automod_decisions_cover_each_completion_kind_without_new_series() {
        // TOG-28481: one counter per terminal CompletionKind, fixed 8-value
        // vocabulary. No message text, excerpts or user IDs ever reach labels.
        use crate::automod_runtime::CompletionKind;
        let kinds = [
            CompletionKind::Accepted,
            CompletionKind::AlreadyProcessed,
            CompletionKind::DryRun,
            CompletionKind::Protected,
            CompletionKind::Deleted,
            CompletionKind::Warned,
            CompletionKind::TimedOut,
            CompletionKind::SanctionRefused,
        ];
        assert_eq!(
            kinds.map(CompletionKind::as_str).as_slice(),
            AUTOMOD_DECISION_OUTCOMES,
            "CompletionKind::as_str must mirror the metrics allowlist in order"
        );
        let metrics = Metrics::default();
        let text = metrics.render(None);
        for outcome in AUTOMOD_DECISION_OUTCOMES {
            assert!(text.contains(&format!(
                "two_bot_automod_decisions_total{{outcome=\"{outcome}\"}} 0\n"
            )));
        }
        for kind in kinds {
            metrics.automod_decision(kind.as_str());
        }
        metrics.automod_decision("deleted");
        let text = metrics.render(None);
        for (kind, count) in [
            (CompletionKind::Accepted, 1),
            (CompletionKind::AlreadyProcessed, 1),
            (CompletionKind::DryRun, 1),
            (CompletionKind::Protected, 1),
            (CompletionKind::Deleted, 2),
            (CompletionKind::Warned, 1),
            (CompletionKind::TimedOut, 1),
            (CompletionKind::SanctionRefused, 1),
        ] {
            assert!(text.contains(&format!(
                "two_bot_automod_decisions_total{{outcome=\"{}\"}} {count}\n",
                kind.as_str()
            )));
        }
        // Fixed cardinality: exactly eight series, parseable samples.
        let series: Vec<_> = text
            .lines()
            .filter(|line| line.starts_with("two_bot_automod_decisions_total{"))
            .collect();
        assert_eq!(series.len(), AUTOMOD_DECISION_OUTCOMES.len());
        let mut unique = std::collections::HashSet::new();
        for line in text.lines().filter(|line| !line.starts_with('#')) {
            let (key, value) = line.rsplit_once(' ').unwrap();
            assert!(unique.insert(key), "duplicate series: {key}");
            assert!(value.parse::<f64>().is_ok(), "bad sample: {line}");
        }
    }

    #[test]
    fn automod_decision_hostile_labels_collapse_and_saturate() {
        let metrics = Metrics::default();
        let before = metrics.render(None).lines().count();
        for id in 0..100 {
            let hostile = format!("{id}\"\\\nsecret=value");
            metrics.automod_decision(&hostile);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        // `invalid_secret` is a legitimate bounded kind: match the hostile
        // `secret=value` injection marker, not the bare substring.
        assert!(!text.contains("secret="));
        assert!(
            text.contains("two_bot_automod_decisions_total{outcome=\"sanction_refused\"} 100\n")
        );
        {
            let mut values = metrics.0.lock().unwrap();
            values.automod_decisions = [u64::MAX; AUTOMOD_DECISION_OUTCOMES.len()];
        }
        metrics.automod_decision("accepted");
        let text = metrics.render(None);
        assert!(text.contains(&format!(
            "two_bot_automod_decisions_total{{outcome=\"accepted\"}} {}\n",
            u64::MAX
        )));
    }
}
