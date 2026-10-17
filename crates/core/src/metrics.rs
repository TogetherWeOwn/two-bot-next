//! Fixed-cardinality, process-local metrics; no background task or retained payloads.
//! Text format: <https://prometheus.io/docs/instrumenting/exposition_formats/#text-format-details>

use serde::Serialize;
use std::{
    fmt::Write,
    sync::{Mutex, OnceLock},
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

/// Complete error-class allowlist. With `JOBS`, this yields 112 fixed series.
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
/// Presence-capture outcomes for `two_bot_presence_capture_total{outcome}`
/// (event-store S3): `inserted` for a first delivery that stored a row,
/// `duplicate` for a same-key retry absorbed by the shared dedup key, and
/// `rejected` for a payload refused before the insert. Exactly these three
/// series exist — presence is the highest-volume family, so the outcome label
/// carries no user id, activity name, application id or any free text, and an
/// unrecognised outcome collapses to the last entry (`rejected`) rather than
/// growing the series vocabulary.
pub const PRESENCE_CAPTURE_OUTCOMES: &[&str] = &["inserted", "duplicate", "rejected"];
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
/// Verified EventSub deliveries by bounded receiver classification. `other`
/// captures unknown/future classification errors; no header, body, broadcaster,
/// subscription, or secret data reaches the label.
pub const EVENTSUB_DELIVERY_KINDS: [&str; 6] = [
    "challenge",
    "notification",
    "revocation",
    "tampered",
    "ignored",
    "other",
];
/// Go-live post attempts by bounded outcome. The outcome vocabulary is fixed;
/// post identifiers and error details are never retained.
pub const EVENTSUB_POST_OUTCOMES: [&str; 4] = ["attempted", "succeeded", "failed", "other"];
const BUCKETS_MICROS: &[u64] = &[
    1_000, 5_000, 10_000, 50_000, 100_000, 500_000, 1_000_000, 5_000_000,
];

fn bounded_index(value: &str, allowed: &[&str]) -> usize {
    allowed
        .iter()
        .position(|candidate| *candidate == value)
        .unwrap_or(allowed.len() - 1)
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

#[derive(Default)]
struct Values {
    events: [u64; EVENTS.len()],
    rest: [[u64; RESULTS.len()]; REST_ROUTES.len()],
    jobs: [JobMetrics; JOBS.len()],
    reconnects: u64,
    resumes: u64,
    disconnects: u64,
    missed_events: u64,
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
    /// Guild-actor halt latch (CredentialRefused): 1 while halted.
    voice_worker_halted: u64,
    /// Queued (not in-flight) action depth by lane; last writer wins.
    voice_queue_pending: [u64; VOICE_QUEUE_LANES.len()],
    /// Age of the oldest queued action in seconds; 0 when the queue is empty.
    voice_queue_oldest_age_secs: u64,
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
    voice_vote_kick: [u64; VOICE_VOTE_KICK_OUTCOMES.len()],
    voice_names: [u64; VOICE_NAME_OUTCOMES.len()],
    db_errors: [u64; DB_ERROR_OPS.len()],
    send_admissions: [u64; SEND_ADMISSION_OUTCOMES.len()],
    prefix_trigger_refused: [u64; PREFIX_TRIGGER_REFUSED_REASONS.len()],
    dispatch_drops: [u64; DISPATCH_LANES.len()],
    checkpoint_failures: [u64; CHECKPOINT_FAILURE_STAGES.len()],
    presence_capture: [u64; PRESENCE_CAPTURE_OUTCOMES.len()],
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
    /// Event-store consumer lag (S1): `received_at - event_time` spread in
    /// seconds for the newest observed capture row. Last writer wins; 0 when
    /// no row has been observed. Single label-free series — no message,
    /// channel, user, dedup-key, content or body label ever reaches
    /// exposition.
    event_store_consumer_lag_secs: f64,
    eventsub_received: u64,
    eventsub_classified: [u64; EVENTSUB_DELIVERY_KINDS.len()],
    eventsub_posts: [u64; EVENTSUB_POST_OUTCOMES.len()],
}

/// All storage is fixed-size. Unknown labels collapse to `other`, including hostile input.
#[derive(Default)]
pub struct Metrics(Mutex<Values>);

pub fn global() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(Metrics::default)
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

    /// Guild-actor halt latch state: 1 while CredentialRefused halted the
    /// worker, 0 otherwise. Last writer wins.
    pub fn set_voice_halted(&self, halted: bool) {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .voice_worker_halted = u64::from(halted);
    }

    /// Queued action depth by lane plus the oldest pending item's age.
    /// Last writer wins; render 0 age when the queue is empty.
    pub fn set_voice_queue(&self, urgent: u64, deferred: u64, oldest_age_secs: u64) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_queue_pending = [urgent, deferred];
        values.voice_queue_oldest_age_secs = oldest_age_secs;
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

    /// One untracked creator-channel orphan needing manual deletion after
    /// failed `/create` compensation (TOG-13543). No channel ID is retained.
    pub fn voice_orphan(&self) {
        let mut values = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        values.voice_orphans = values.voice_orphans.saturating_add(1);
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
        header(
            &mut out,
            "two_bot_gateway_events_total",
            "counter",
            "Gateway events by bounded event type.",
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
        header(
            &mut out,
            "two_bot_presence_capture_total",
            "counter",
            "Presence-update capture outcomes by bounded outcome; fixed three series.",
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
            "two_bot_gateway_latency_seconds",
            "gauge",
            "Last heartbeat ACK latency; NaN until measured.",
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
            "Creator join gateway receipt to successful member move in seconds.",
        );
        header(
            &mut out,
            "two_bot_voice_join_occupancy_total",
            "counter",
            "VOICE_STATE_UPDATE joins by humans already present in the target channel (7+ caps the tail).",
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
            "Adapter-visible REST attempts by route template and result, including retries.",
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
            "Completed job attempts by bounded job name and outcome.",
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
            "Unix time of last successful job completion; zero means never.",
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
            "Completed failures since the last success; resets on success.",
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
            "One-hot last failed-completion class by bounded job; zero before failure and after success.",
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
            "Finished room lifecycle outcomes by bounded operation and outcome; retries are not outcomes.",
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
            "Reconcile transitions and new enqueues by bounded action.",
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
            "Queue writes that exhausted QUEUE_MAX_ATTEMPTS by bounded family.",
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
            "Rooms tracked in memory; compare with live Discord channels for ghosts.",
        );
        writeln!(out, "two_bot_voice_tracked_rooms {}", values.voice_tracked).unwrap();
        header(
            &mut out,
            "two_bot_voice_compensation_pending",
            "gauge",
            "Tracked rooms awaiting compensating delete after a failed write.",
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
            "Actor-received creator joins refused before queueing, by bounded reason.",
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
        header(
            &mut out,
            "two_bot_voice_worker_halted",
            "gauge",
            "Guild-actor halt latch: 1 while CredentialRefused halted join processing.",
        );
        writeln!(
            out,
            "two_bot_voice_worker_halted {}",
            values.voice_worker_halted
        )
        .unwrap();
        header(
            &mut out,
            "two_bot_voice_queue_pending",
            "gauge",
            "Queued (not in-flight) voice actions by lane.",
        );
        for (lane, count) in VOICE_QUEUE_LANES.iter().zip(values.voice_queue_pending) {
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
            "Age of the oldest queued voice action in seconds; 0 when empty.",
        );
        writeln!(
            out,
            "two_bot_voice_queue_oldest_age_seconds {}",
            values.voice_queue_oldest_age_secs
        )
        .unwrap();
        header(
            &mut out,
            "two_bot_voice_rest_in_flight_age_seconds",
            "gauge",
            "Age of the oldest in-flight voice REST attempt in seconds; 0 when idle.",
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
            "Last merged creator-map generation; 0 before the first merge.",
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
            "Seconds since the last successful creator-map merge; 0 when never merged.",
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
            "two_bot_voice_vote_kick_total",
            "counter",
            "Vote-kick starts, refusals by refusal code and terminal enforcements by enforcement code; vote results are audit-only.",
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
            "two_bot_voice_names_total",
            "counter",
            "Automatic room-name decisions: created with the template name, template renders by outcome, and queued renames sent or dropped at dispatch.",
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
            "Storage-layer failures by bounded operation; pool gauges are pressure, this is errors.",
        );
        for (op, count) in DB_ERROR_OPS.iter().zip(values.db_errors) {
            writeln!(out, "two_bot_db_errors_total{{op=\"{op}\"}} {count}").unwrap();
        }
        header(
            &mut out,
            "two_bot_send_admissions_total",
            "counter",
            "Send-admission admit() decisions by bounded terminal outcome.",
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
            "Prefix candidates refused by the automod verdict before any trigger lookup, by bounded reason.",
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
            "Dispatch-lane saturation drops by bounded lane (reactions lane also counts per-member fairness refusals); logs sample one warn per 60 s per runtime.",
        );
        for (lane, count) in DISPATCH_LANES.iter().zip(values.dispatch_drops) {
            writeln!(
                out,
                "two_bot_dispatch_drops_total{{lane=\"{lane}\"}} {count}"
            )
            .unwrap();
        }
        header(
            &mut out,
            "two_bot_gateway_checkpoint_failures_total",
            "counter",
            "Failed gateway checkpoint commits by bounded stage; every failure stops the dispatch worker.",
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
            "two_bot_internal_actions_total",
            "counter",
            "Signed website-action receiver executions by bounded family and outcome; refusals share the warn-summary classes.",
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
            "two_bot_community_facts_drain_failures_total",
            "counter",
            "Community-facts drain failures by bounded reason; the worker warns and continues, so each failed dispatch counts once.",
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
            "Audit mirror delivery halt; 1 while the persistent kill switch is engaged, 0 otherwise. Set once per readable audit_retry sweep; an unreadable halt keeps the last reported state.",
        );
        writeln!(
            out,
            "two_bot_audit_delivery_halt {}",
            values.audit_delivery_halt
        )
        .unwrap();
        header(
            &mut out,
            "two_bot_event_store_consumer_lag_seconds",
            "gauge",
            "Event-store consumer lag in seconds (received_at minus event_time for the newest observed capture row); 0 when unobserved. Single label-free series.",
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
            "EventSub deliveries by bounded receiver classification.",
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
            "Go-live announcement post attempts by bounded outcome.",
        );
        for (outcome, count) in EVENTSUB_POST_OUTCOMES.iter().zip(values.eventsub_posts) {
            writeln!(
                out,
                "two_bot_eventsub_posts_total{{outcome=\"{outcome}\"}} {count}"
            )
            .unwrap();
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
            "Remaining global cooldown; pending response bodies may still hold admission at zero.",
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
    header(out, name, kind, "Process-local health metric.");
    writeln!(out, "{name} {value}").unwrap();
}

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
            metrics.internal_action(&hostile, &hostile);
        }
        let text = metrics.render(None);
        assert_eq!(text.lines().count(), before);
        assert!(!text.contains("secret"));
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
            assert_eq!(JOBS.len(), 16);
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
            assert_eq!(JOBS.len(), 16);
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
        assert!(!text.contains("secret"));
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
        assert!(!text.contains("secret"));
        assert!(text.contains("two_bot_gateway_checkpoint_failures_total{stage=\"commit\"} 100\n"));
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
        assert!(!text.contains("secret"));
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
        assert!(!text.contains("secret"));
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
        assert!(!text.contains("secret"));
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
    fn disconnects_and_missed_events_start_at_zero_and_saturate() {
        let metrics = Metrics::default();
        let text = metrics.render(None);
        assert!(text.contains("two_bot_gateway_disconnects_total 0\n"));
        assert!(text.contains("two_bot_gateway_missed_events_total 0\n"));
        metrics.gateway_disconnect();
        metrics.gateway_missed_events(0);
        metrics.gateway_missed_events(3);
        let text = metrics.render(None);
        assert!(text.contains("two_bot_gateway_disconnects_total 1\n"));
        assert!(text.contains("two_bot_gateway_missed_events_total 3\n"));
        {
            let mut values = metrics.0.lock().unwrap();
            values.disconnects = u64::MAX;
            values.missed_events = u64::MAX;
        }
        metrics.gateway_disconnect();
        metrics.gateway_missed_events(9);
        let text = metrics.render(None);
        assert!(text.contains(&format!("two_bot_gateway_disconnects_total {}\n", u64::MAX)));
        assert!(text.contains(&format!(
            "two_bot_gateway_missed_events_total {}\n",
            u64::MAX
        )));
    }
}
