/**
 * Alert rules over the Rust process's `/metrics` exposition (TOG-11154).
 *
 * The Container DO pulls `/metrics` on each keepalive tick (it is the only
 * thing that can reach the container-internal listener), evaluates these rules
 * against the sample and the previous one, and posts transitions to the
 * optional `OPS_ALERT_WEBHOOK_URL`. Every rule links to a section of
 * docs/runbook.md; a test keeps the anchors honest.
 */

export interface Sample {
  name: string;
  labels: Record<string, string>;
  value: number;
}

export interface RuleDef {
  id: string;
  summary: string;
  /** Anchor inside docs/runbook.md. */
  runbook: string;
  /** Explicit non-paging classification; existing rules retain their alert copy. */
  severity?: "ticket";
}

/** Cadence in seconds of every scheduled job (crates/core/src/*_INTERVAL_MS). */
export const JOB_INTERVAL_SECONDS: Record<string, number> = {
  counter: 60,
  rank: 600,
  scheduled_events: 600,
  presence_probe: 3600,
  community_scorecard: 60,
  inactivity: 3600,
  lfg_sweep: 60,
  event_voice: 60,
};

export const STALE_INTERVALS = 2;
/**
 * Staleness window in seconds for the 15 s tickers (`scheduled_messages`,
 * `settings`). Ten minutes is 40 missed ticks: far above the keepalive scrape
 * cadence (~60 s, so ~10 consecutive missed scrapes must agree), the
 * supervisor startup jitter (up to 5 s), and the per-attempt timeouts
 * (`scheduled_messages` 120 s, `settings` 10 s), so one slow scrape or a
 * single timed-out tick never pages; yet a truly wedged ticker (skipped busy
 * deadlines count neither as success nor failure, so neither `job_stale` nor
 * `job_consecutive_failures` can see it) still pages well within the 48 h
 * watch. A zero success timestamp suppresses both boot (never succeeded) and
 * parked (never registered: `DATABASE_URL` unset, or the automations gate
 * off) since those stay zero; see `docs/metrics.md`.
 */
export const TICKER_STALE_SECONDS = 600;
/** Jobs covered by `ticker_stale` instead of `job_stale`. */
export const TICKER_STALE_JOBS: readonly string[] = ["scheduled_messages", "settings"];
/**
 * Fail-closed staleness window for jobs without a cadence entry
 * (`invite_snapshot`, `audit_retry`, `other`, and any future label the scrape
 * accepts before the catalog maps it). Two hours is twice the longest mapped
 * cadence (`presence_probe`/`inactivity`, 3600 s), so the fallback never fires
 * sooner than the slowest mapped `job_stale` while still catching a wedged
 * unmapped job the same watch shift. A zero success timestamp suppresses both
 * boot (never succeeded) and parked (never registered), exactly like
 * `job_stale` and `ticker_stale`. The firing rule is a ticket, never a page:
 * without a mapped cadence the monitor cannot tell a slow-but-healthy schedule
 * from a wedge, so it nags for an explicit cadence, ticker coverage, or
 * event-driven exclusion instead of paging. `session_checkpoint` is
 * event-driven (one success per durable gateway commit) and excluded below:
 * it never tickets however stale.
 */
export const UNKNOWN_JOB_STALE_SECONDS = 7200;
/**
 * Event-driven labels excluded from the `job_unknown_stale` fallback.
 * `session_checkpoint` advances only on a durable gateway dispatch commit
 * (`DispatchTimer::committed` in `crates/bot/src/gateway_metrics.rs`), so an
 * idle guild (no messages, reactions, voice or member events for 2 hours)
 * goes stale while healthy. Gateway liveness is already covered by readiness
 * and `gateway_missed_events`. A reasoned catalog note alone never excludes a
 * label: only this list, a `JOB_INTERVAL_SECONDS` cadence, or
 * `TICKER_STALE_JOBS` coverage retires the fallback.
 */
export const UNKNOWN_STALE_EXCLUDED: readonly string[] = ["session_checkpoint"];
export const FAILURE_THRESHOLD = 3;
export const JOB_ERROR_CLASSES = [
  "database",
  "rest",
  "configuration",
  "timeout",
  "panic",
  "feed",
  "recovery_required",
] as const;
export type JobErrorClass = (typeof JOB_ERROR_CLASSES)[number];
const JOB_ERROR_CLASS_SET: ReadonlySet<string> = new Set(JOB_ERROR_CLASSES);
/** 429s must exceed this share of REST requests between two samples... */
export const REST_429_RATIO = 0.1;
/** ...and the window must hold at least this many requests. */
export const REST_429_MIN_REQUESTS = 10;
/** 5xxs must exceed this share of REST requests between two samples... */
export const REST_5XX_RATIO = 0.1;
/** ...and the window must hold at least this many requests. */
export const REST_5XX_MIN_REQUESTS = 10;
/** Failed EventSub announcement posts must exceed this share of attempted posts... */
export const EVENTSUB_POST_FAILED_SHARE_RATIO = 0.1;
/** ...and the window must hold at least this many attempted posts. */
export const EVENTSUB_POST_FAILED_SHARE_MIN_ATTEMPTED = 10;
/** Fixed outcomes for `two_bot_eventsub_posts_total`, pinned to Rust `EVENTSUB_POST_OUTCOMES`. */
export const EVENTSUB_POST_OUTCOMES: readonly string[] = ["attempted", "succeeded", "failed", "other"];
/**
 * Promotion canary gate: firing keys that fail a promotion stage so the
 * existing per-stage auto-rollback restores the pre-promote version. 5xx
 * only: a 5xx burst is never healthy backpressure (Discord-side failure or
 * a broken request shape from the fresh build), while 429s are rate-limit
 * backpressure and `transport` failures already surface through job timeout
 * classes. See docs/runbook.md#alert-rest-5xx.
 */
export const PROMOTION_ERROR_GATE_RULES: readonly string[] = ["rest_5xx_rate"];
/** Engaged audit halt for three valid keepalive samples (~2 min from first to third). */
export const AUDIT_HALT_SAMPLES = 3;
/** Pool at max with zero idle for this many consecutive samples. */
export const POOL_SATURATED_SAMPLES = 3;
/** DB errors must reach this many between two samples... */
export const DB_ERROR_MIN_ERRORS = 3;
/** ...and send-admission refusals must appear in this many consecutive samples. */
export const SEND_BLOCKED_SAMPLES = 3;
/** REST admission breaker openings must appear in this many consecutive samples. */
export const REST_BREAKER_OPEN_SAMPLES = 3;
/**
 * Combined new guard invalid observations (401/403/429 at headers) plus local
 * refusals (breaker/global/token, pre-wire) between two samples that pages.
 * Healthy windows sit at zero and one-off accidents land in the single
 * digits, while a permission loop, a bad deploy or a Discord-side 429 storm
 * mints dozens per minute; 10 pages an order of magnitude above accidents
 * with ~50x headroom below the ~500/min pace that trips the 5,000/600 s
 * breaker, so the page arrives while the IP budget still has room.
 */
export const REST_REFUSAL_SURGE_MIN = 10;
/**
 * Consecutive valid samples with unresolved global-pause response bodies
 * before the page. Body reads carry a 5 s deadline and expiry drops their
 * accounting, so three ~60 s samples with `pending_global_responses` still
 * nonzero is 24-36x past the expected drain: the bodies are stuck and
 * admission stays closed behind them. Mirrors the breaker/lag 3-sample bound.
 */
export const REST_PENDING_STUCK_SAMPLES = 3;
/** Verdict-refused prefix candidates must rise in this many consecutive samples. */
export const PREFIX_TRIGGER_VERDICT_SAMPLES = 3;
/** Website-action receiver refusals must appear in this many consecutive samples. */
export const RECEIVER_REFUSAL_SAMPLES = 3;
/** EventSub tampered-signature deliveries must rise in this many consecutive samples. */
export const EVENTSUB_TAMPERED_SAMPLES = 3;
/** Consecutive unpaired gateway samples before the page; bound justified in the runbook. */
export const GATEWAY_UNPAIRED_SAMPLES = 3;
/**
 * Consecutive valid samples with the scraped /readyz gateway component
 * non-ready before the page; bound justified in the runbook. Same 3-sample
 * bound as the sibling unpaired rule: samples arrive about every 60 s, so a
 * gateway that stays starting/down/parked with no disconnect edge pages about
 * two to three minutes after the outage starts, while a clean boot (which
 * reports Connected within about a minute) never reaches the third sample.
 */
export const GATEWAY_NOT_READY_SAMPLES = 3;
/** Voice failures must exceed this share of room operations between two samples... */
export const VOICE_FAILURE_RATIO = 0.05;
/** ...and the window must hold at least this many operations. */
export const VOICE_FAILURE_MIN_OPS = 10;
/**
 * Vote-kick enforcement outcomes that extend the `voice_failures` rule: any
 * increase between two samples fires. These are the terminal
 * `EnforcementOutcome` codes where the room-scoped enforcement of a passed
 * vote did not complete (`crates/core/src/voice_vote_kick_audit.rs`), a
 * subset of `VOICE_VOTE_KICK_OUTCOMES` in `crates/core/src/metrics.rs`.
 * Every other outcome stays quiet: refusals (e.g. `cooldown`) are admission
 * policy working as designed, and the remaining enforcement codes
 * (`connect_denied_and_disconnected`, `connect_denied_target_absent`,
 * `skipped_room_gone`, `skipped_target_protected`) are successful or
 * already-terminal writes. Fixed enum only: no voter or target labels.
 */
export const VOICE_VOTE_KICK_FAILURE_OUTCOMES = ["discord_error", "gave_up", "permission_missing"] as const;
/** Dispatch drops must grow in this many consecutive completed sample windows. */
export const DISPATCH_DROP_SAMPLES = 3;
/** Fixed reasons for the community-facts drain-failure counter (mirrors `COMMUNITY_FACTS_DRAIN_REASONS` in `crates/core/src/metrics.rs`). */
export const COMMUNITY_FACTS_DRAIN_REASONS: readonly string[] = ["error", "timeout", "other"];
/** Summed drain-failure growth must persist this many consecutive completed windows before ticketing. */
export const COMMUNITY_FACTS_DRAIN_SAMPLES = 3;
/** Sustained above-threshold event-store consumer lag samples before the ticket. */
export const EVENT_STORE_LAG_SAMPLES = 3;
/**
 * Lag above this many seconds counts toward the ticket; a valid sample at or
 * below it resolves. 60 s is one keepalive window (`docs/event-store.md`):
 * a healthy `received_at - event_time` spread is always slightly positive
 * (insert latency), so a bare `> 0` bar would fire forever once the setter
 * has a caller.
 */
export const EVENT_STORE_LAG_THRESHOLD_SECONDS = 60;
/** Mirrors the fixed DISPATCH_LANES allowlist in crates/core/src/metrics.rs. */
export const DISPATCH_LANES: readonly string[] = ["messages", "interactions", "registry", "privileged", "busy", "reactions"];
/** Creator-join SLO: this share of genuine joins must move within 5 s (99.5%). */
export const JOIN_SLO_BUDGET = 0.005;
/** Fast burn: alert when both the 1 h and 5 min windows burn faster than this. */
export const JOIN_FAST_BURN = 14.4;
/** Long window for the fast-burn rule, seconds. */
export const JOIN_FAST_LONG_SECONDS = 3600;
/** Short confirmation window for the fast-burn rule, seconds. */
export const JOIN_FAST_SHORT_SECONDS = 300;
/** Slow burn: ticket when both the 6 h and 30 min windows burn faster than this. */
export const JOIN_SLOW_BURN = 6;
/** Long window for the slow-burn rule, seconds. */
export const JOIN_SLOW_LONG_SECONDS = 21600;
/** Short confirmation window for the slow-burn rule, seconds. */
export const JOIN_SLOW_SHORT_SECONDS = 1800;
/** A burn window with fewer genuine joins than this never fires (quiet guilds stay silent). */
export const JOIN_BURN_MIN_JOINS = 10;
/** One bad join alone never pages or tickets, even at the volume floor. */
export const JOIN_BURN_MIN_BAD = 2;
/** Burn snapshots older than the slow window plus this margin are pruned. */
export const JOIN_BURN_HISTORY_MARGIN_SECONDS = 600;
/** Creator-map refresh cadence in seconds (`CREATORS_REFRESH_INTERVAL_MS` in `crates/bot/src/voice_rooms.rs`). */
export const CREATOR_MAP_REFRESH_SECONDS = 300;
/**
 * Creator-map staleness bound: two missed refresh intervals. The age gauge
 * counts from the last *successful* merge, so one failed refresh only reaches
 * the bound as the next attempt also fails; scheduling jitter inside one
 * interval never fires. Mirrors the `STALE_INTERVALS` two-interval rule.
 */
export const CREATOR_MAP_STALE_SECONDS = 2 * CREATOR_MAP_REFRESH_SECONDS;
/**
 * Live-view not-ready bound in seconds: promotes the host watchdog's ">120 s"
 * bound (`docs/observability-event-catalog.md`) into the standard alert
 * pipeline. While the gauge is above 0 that guild's creator joins are
 * dropped without an accept or refusal line; past this bound the runbook
 * calls for a restart (fresh IDENTIFY + GUILD_CREATE) and a bug report. A
 * hung dispatch does not freeze the published age: the watchdog sibling
 * reads the shared live view directly, so a hang still pages past the bound.
 */
export const VOICE_LIVE_NOT_READY_THRESHOLD_SECONDS = 120;
/**
 * Queue-stall bound in seconds: matches the in-process stuck horizon
 * (`watchdog_stuck_after` in `run_actor_with_limits`,
 * `crates/bot/src/voice_rooms.rs`), which fires when queued actions see no
 * completed dispatch turn for this long. An oldest-wait past it means a
 * guild's queue is draining unacceptably slowly. A hung dispatch does not
 * freeze publication: the watchdog sibling recomputes the age from the
 * snapshot's first-enqueue stamp on its own clock, so a hang that starts
 * below the bound still tickets past it.
 */
export const VOICE_QUEUE_STALL_THRESHOLD_SECONDS = 120;
/** Error-budget ratio of the 48h watch: 99.9% SLO leaves 0.1% spendable. */
export const ERROR_BUDGET_RATIO = 0.001;
/** Fast burn: page when both the 15 min and 2 min windows burn faster than this. */
export const ERROR_BUDGET_FAST_BURN = 12;
/** Long window for the error-budget fast-burn rule, seconds. */
export const ERROR_BUDGET_FAST_LONG_SECONDS = 900;
/** Short confirmation window for the error-budget fast-burn rule, seconds. */
export const ERROR_BUDGET_FAST_SHORT_SECONDS = 120;
/** Burn snapshots older than the long window plus this margin are pruned. */
export const ERROR_BUDGET_FAST_HISTORY_MARGIN_SECONDS = 120;
/**
 * A window baseline older than its boundary plus this tolerance is stale, not
 * covering: keepalive scrapes land about every 60 s (slower when a probe is
 * slow), so a baseline up to two nominal intervals past the boundary is still
 * ordinary scrape alignment, while an older one means a boundary scrape was
 * missed and the window would sweep errors from outside the claimed span.
 */
export const ERROR_BUDGET_FAST_BASELINE_TOLERANCE_SECONDS = 120;
/** A burn window with fewer eligible REST requests than this never fires. */
export const ERROR_BUDGET_FAST_MIN_EVENTS = 10;
/** A burn window with fewer budget errors than this never fires. */
export const ERROR_BUDGET_FAST_MIN_ERRORS = 2;
/**
 * Relative allowance below the nominal fast-burn threshold. Window edges land
 * on whole ~60 s keepalive scrapes, so a burn sustained exactly at the policy
 * rate can read up to ~1% low when the edges shave a scrape; the comparison
 * carries this allowance so threshold-sustained burn still pages instead of
 * flapping. Healthy traffic (near 1x) and single-window spikes stay silent
 * with orders of magnitude to spare.
 */
export const ERROR_BUDGET_FAST_TOLERANCE = 0.01;
/** Slow burn: ticket when both the 2 h and 15 min windows burn faster than this. Mirrors `SLOW_BURN_RATE` in `crates/bot/src/burn_rate.rs`. */
export const ERROR_BUDGET_SLOW_BURN = 3;
/** Long window for the error-budget slow-burn rule, seconds. */
export const ERROR_BUDGET_SLOW_LONG_SECONDS = 7200;
/** Short confirmation window for the error-budget slow-burn rule, seconds. */
export const ERROR_BUDGET_SLOW_SHORT_SECONDS = 900;
/** A slow-burn window with fewer eligible REST requests than this never fires. */
export const ERROR_BUDGET_SLOW_MIN_EVENTS = 10;
/** A slow-burn window with fewer budget errors than this never fires. */
export const ERROR_BUDGET_SLOW_MIN_ERRORS = 2;
/** Consecutive valid samples with no heartbeat-ACK evidence before the dead-gateway page; bound justified in the runbook. */
export const GATEWAY_LATENCY_DEAD_SAMPLES = 3;
/** Consecutive rising reconnect windows before the flap page; single blips stay silent. */
export const GATEWAY_RECONNECT_FLAP_SAMPLES = 3;
/**
 * Rendered `le` spellings of `two_bot_handler_duration_seconds_bucket`, from
 * the private BUCKETS_MICROS bounds in crates/core/src/metrics.rs (seconds
 * rendering, plus `+Inf`). No pub label const exists (unlike
 * VOICE_JOIN_TO_MOVE_BUCKET_LE), so the validator, the fixture and this list
 * carry the same literal, pinned equal in tests.
 */
export const HANDLER_DURATION_BUCKET_LE: readonly string[] = ["0.001", "0.005", "0.01", "0.05", "0.1", "0.5", "1", "5", "+Inf"];
/**
 * Handler-tail bound in seconds: a window whose p99 crosses it is saturated.
 * The first seconds-scale bucket; a healthy parse/pipeline/commit p99 sits
 * in the millisecond buckets, so a sustained crossing means second-scale
 * tail work (slow commits, contention), not ordinary jitter.
 */
export const HANDLER_SATURATED_P99_SECONDS = 1;
/** A saturated window needs at least this many completed dispatches; quiet windows stay silent. */
export const HANDLER_SATURATED_MIN_OBS = 10;
/** ...and at least this many dispatches past the bound; one slow dispatch stays silent. */
export const HANDLER_SATURATED_MIN_SLOW = 2;
/** ...and the past-bound share must exceed this (tail share past 1% reads as p99 past the bound). */
export const HANDLER_SATURATED_SHARE = 0.01;
/** Consecutive saturated handler windows before the ticket. */
export const HANDLER_SATURATED_SAMPLES = 3;
/** Fixed job labels for `two_bot_job_runs_total`, pinned to Rust `JOBS`. */
export const JOB_RUNS_JOBS: readonly string[] = ["invite_snapshot", "session_checkpoint", "counter", "rank", "scheduled_events", "settings", "presence_probe", "community_scorecard", "inactivity", "audit_retry", "scheduled_messages", "feeds", "self_role_recovery", "member_unban_sweep", "automod_expiry", "lfg_sweep", "event_voice", "other"];
/** Fixed outcomes for `two_bot_job_runs_total`, pinned to Rust `JOB_OUTCOMES`. */
export const JOB_RUNS_OUTCOMES: readonly string[] = ["success", "failure"];
/** Failed runs must exceed this share of a judged window... */
export const JOB_FAILURE_SHARE_RATIO = 0.1;
/** ...covering at least this many completed runs (sub-floor windows accumulate)... */
export const JOB_FAILURE_SHARE_MIN_RUNS = 10;
/** ...with at least this many failures; one bad run never tickets. */
export const JOB_FAILURE_SHARE_MIN_FAILURES = 2;
/** Fixed error classes for `two_bot_feed_fetch_errors_total`, pinned to Rust `FEED_FETCH_ERROR_CLASSES`. */
export const FEED_FETCH_ERROR_CLASSES: readonly string[] = ["invalid_source", "non_public_address", "redirect_refused", "too_large", "invalid_encoding", "unsupported_content_type", "unsupported_encoding", "resolve", "transport", "deadline", "http_error", "other"];
/** Fixed kinds for `two_bot_eventsub_classified_total`, pinned to Rust `EVENTSUB_DELIVERY_KINDS`. */
export const EVENTSUB_CLASSIFIED_KINDS: readonly string[] = ["challenge", "notification", "revocation", "tampered", "ignored", "invalid_secret", "invalid_header", "body_too_large", "bad_signature", "invalid_timestamp", "stale_timestamp", "invalid_body", "duplicate_keys", "wrong_subscription", "invalid_challenge", "invalid_callback", "invalid_broadcaster_id", "too_many_broadcasters", "other"];
/** Consecutive valid samples with received deliveries ahead of classified ones before the ticket. */
export const EVENTSUB_RECEIVE_GAP_SAMPLES = 3;
/** Fixed verbs for `two_bot_moderation_enforcements_total`, pinned to Rust `MODERATION_VERBS`. */
export const MODERATION_VERBS: readonly string[] = ["ban", "tempban", "kick", "timeout", "warn", "purge", "slowmode", "lockdown", "unlock"];
/**
 * Moderation enforcement outcomes that ticket: `discord_error` (uncertain
 * Discord failures: timeout, unreachable, rate-limited) and `storage_error`
 * (persistence failures). `success` landed, `refused` is policy proving no
 * mutation (admission policy working as designed, like the `refused` voice
 * operations the `voice_failures` rule skips), and `other` is an unknown
 * collapse that is not evidence of failure. A subset of
 * `MODERATION_ENFORCEMENT_OUTCOMES` in `crates/core/src/metrics.rs`.
 */
export const MODERATION_FAILURE_OUTCOMES = ["discord_error", "storage_error"] as const;
/**
 * Voice REST in-flight age above this many whole seconds counts toward the
 * ticket; a valid sample at or below it resolves. 60 s is six times the 10 s
 * wire timeout around every voice REST send (`crates/discord/src/voice_rooms.rs`),
 * so a healthy attempt (which always completes, times out or drops its guard
 * inside ~10 s) never reaches it, while a wedged attempt surfaces on the
 * next publish and persists. REST begin/drop events, actor ticks and the
 * independent observer republish the gauge, including during a hung dispatch.
 */
export const VOICE_REST_STUCK_SECONDS = 60;
/** Sustained above-threshold in-flight REST age samples before the ticket. */
export const VOICE_REST_STUCK_SAMPLES = 3;

export const RULES: readonly RuleDef[] = [
  { id: "job_stale", summary: `scheduled job has no success for more than ${STALE_INTERVALS} intervals`, runbook: "runbook.md#alert-job-stale" },
  { id: "job_consecutive_failures", summary: `scheduled job failed ${FAILURE_THRESHOLD}+ times in a row`, runbook: "runbook.md#alert-job-failures" },
  { id: "rest_429_rate", summary: `Discord REST 429s exceed ${REST_429_RATIO * 100}% of requests`, runbook: "runbook.md#alert-rest-429" },
  { id: "rest_5xx_rate", summary: `Discord REST 5xx responses exceed ${REST_5XX_RATIO * 100}% of requests`, runbook: "runbook.md#alert-rest-5xx" },
  { id: "db_pool_saturated", summary: `database pool exhausted for ${POOL_SATURATED_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-db-pool" },
  { id: "db_errors", summary: `database errors reached ${DB_ERROR_MIN_ERRORS}+ between samples`, runbook: "runbook.md#alert-db-errors" },
  { id: "send_admission_blocked", summary: `Discord sends refused admission for ${SEND_BLOCKED_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-send-admission-blocked" },
  { id: "voice_failures", summary: `voice room lifecycle failures exceed ${VOICE_FAILURE_RATIO * 100}% of operations (min ${VOICE_FAILURE_MIN_OPS} ops), or new dead-letters/orphans/vote-kick enforcement failures`, runbook: "runbook.md#alert-voice-failures" },
  { id: "gateway_missed_events", summary: `gateway missed events increased between samples`, runbook: "runbook.md#alert-gateway-missed-events" },
  { id: "gateway_unpaired_disconnect", summary: `gateway disconnect with no RESUMED or fresh READY for ${GATEWAY_UNPAIRED_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-gateway-unpaired-disconnect" },
  { id: "gateway_not_ready", summary: `/readyz gateway component non-ready for ${GATEWAY_NOT_READY_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-gateway-not-ready" },
  { id: "ticker_stale", summary: `15 s ticker has no success for more than ${TICKER_STALE_SECONDS / 60} minutes`, runbook: "runbook.md#alert-ticker-stale" },
  { id: "receiver_refusals", summary: `website-action receiver refusals for ${RECEIVER_REFUSAL_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-receiver-refusals" },
  { id: "dispatch_drops", summary: `dispatch-lane drops grew for ${DISPATCH_DROP_SAMPLES} consecutive sample windows (reactions include fairness refusals; not proof of gateway packet loss)`, runbook: "runbook.md#alert-dispatch-drops", severity: "ticket" },
  { id: "job_unknown_stale", summary: `job without a cadence entry has no success for more than ${UNKNOWN_JOB_STALE_SECONDS / 3600} hours`, runbook: "runbook.md#alert-job-unknown-stale", severity: "ticket" },
  { id: "gateway_checkpoint_failures", summary: `gateway checkpoint commit failures increased between samples`, runbook: "runbook.md#alert-checkpoint-failures" },
  { id: "voice_join_fast_burn", summary: `creator-join error budget burning faster than ${JOIN_FAST_BURN}x over 1 h and 5 min (99.5% moved within 5 s)`, runbook: "runbook.md#alert-voice-join-fast-burn" },
  { id: "voice_join_slow_burn", summary: `creator-join error budget burning faster than ${JOIN_SLOW_BURN}x over 6 h and 30 min (99.5% moved within 5 s)`, runbook: "runbook.md#alert-voice-join-slow-burn", severity: "ticket" },
  { id: "voice_creator_map_stale", summary: `voice creator map has not merged for more than ${CREATOR_MAP_STALE_SECONDS / 60} minutes`, runbook: "runbook.md#alert-voice-creator-map-stale" },
  { id: "community_facts_drain_failures", summary: `community-facts drain failures grew for ${COMMUNITY_FACTS_DRAIN_SAMPLES} consecutive sample windows (warn-and-continue drain loss; scorecard fails closed)`, runbook: "runbook.md#alert-community-facts-drain-failures", severity: "ticket" },
  { id: "eventsub_post_failed_share", summary: `EventSub announcement post failures exceed ${EVENTSUB_POST_FAILED_SHARE_RATIO * 100}% of attempted posts (min ${EVENTSUB_POST_FAILED_SHARE_MIN_ATTEMPTED} attempted)`, runbook: "runbook.md#alert-eventsub-post-failed-share", severity: "ticket" },
  { id: "error_budget_fast_burn", summary: `REST error budget burning faster than ${ERROR_BUDGET_FAST_BURN}x over ${ERROR_BUDGET_FAST_LONG_SECONDS / 60} min and ${ERROR_BUDGET_FAST_SHORT_SECONDS / 60} min (99.9% success over 48 h)`, runbook: "runbook.md#alert-error-budget-fast-burn" },
  { id: "error_budget_slow_burn", summary: `REST error budget burning faster than ${ERROR_BUDGET_SLOW_BURN}x over ${ERROR_BUDGET_SLOW_LONG_SECONDS / 3600} h and ${ERROR_BUDGET_SLOW_SHORT_SECONDS / 60} min (99.9% success over 48 h)`, runbook: "runbook.md#alert-error-budget-slow-burn", severity: "ticket" },
  { id: "audit_delivery_halt", summary: `audit delivery halt engaged for ${AUDIT_HALT_SAMPLES} consecutive valid samples`, runbook: "runbook.md#alert-audit-delivery-halt", severity: "ticket" },
  { id: "rest_admission_breaker_open", summary: `REST admission breaker open for ${REST_BREAKER_OPEN_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-rest-admission-breaker-open", severity: "ticket" },
  { id: "eventsub_tampered", summary: `EventSub tampered-signature deliveries rose in ${EVENTSUB_TAMPERED_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-eventsub-tampered" },
  { id: "eventsub_reconcile_failed", summary: `EventSub reconcile failed passes increased between samples`, runbook: "runbook.md#alert-eventsub-reconcile-failed", severity: "ticket" },
  { id: "event_store_consumer_lag", summary: `event-store consumer lag above ${EVENT_STORE_LAG_THRESHOLD_SECONDS} s sustained for ${EVENT_STORE_LAG_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-event-store-consumer-lag", severity: "ticket" },
  { id: "prefix_trigger_verdict_refusals", summary: `prefix candidates refused by the automod verdict for ${PREFIX_TRIGGER_VERDICT_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-prefix-trigger-verdict-refusals", severity: "ticket" },
  { id: "voice_worker_halted", summary: `voice worker halt latch engaged (a guild actor is halted)`, runbook: "runbook.md#alert-voice-worker-halted" },
  { id: "voice_live_self_heal", summary: `live voice view self-heal repairs increased between samples`, runbook: "runbook.md#alert-voice-live-self-heal", severity: "ticket" },
  { id: "voice_live_not_ready", summary: `voice live view not ready for more than ${VOICE_LIVE_NOT_READY_THRESHOLD_SECONDS} s on a connected gateway`, runbook: "runbook.md#alert-voice-live-not-ready" },
  { id: "voice_queue_stall", summary: `oldest queued voice action waiting past ${VOICE_QUEUE_STALL_THRESHOLD_SECONDS} s`, runbook: "runbook.md#alert-voice-queue-stall", severity: "ticket" },
  { id: "retention_replay_needed", summary: `retention purge skips needing Operator replay increased between samples`, runbook: "runbook.md#alert-retention-replay-needed", severity: "ticket" },
  { id: "voice_dispatch_timeouts", summary: `voice dispatch turns past the observe bound increased between samples`, runbook: "runbook.md#alert-voice-dispatch-timeouts", severity: "ticket" },
  { id: "voice_watchdog_stucks", summary: `voice watchdog stuck windows increased between samples`, runbook: "runbook.md#alert-voice-watchdog-stucks", severity: "ticket" },
  { id: "voice_halt_recoveries", summary: `voice halt-latch recoveries increased between samples`, runbook: "runbook.md#alert-voice-halt-recoveries", severity: "ticket" },
  { id: "rest_breaker_opens", summary: `invalid-request breaker opens increased between samples`, runbook: "runbook.md#alert-rest-breaker-opens", severity: "ticket" },
  { id: "rest_global_pauses", summary: `Discord global-pause episodes increased between samples`, runbook: "runbook.md#alert-rest-global-pauses", severity: "ticket" },
  { id: "gateway_latency_dead", summary: `no gateway heartbeat-ACK evidence for ${GATEWAY_LATENCY_DEAD_SAMPLES} consecutive samples (unknown or unchanged latency)`, runbook: "runbook.md#alert-gateway-latency-dead" },
  { id: "gateway_reconnect_flapping", summary: `gateway reconnects rose in ${GATEWAY_RECONNECT_FLAP_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-gateway-reconnect-flapping" },
  { id: "handler_duration_saturated", summary: `handler dispatch tail past ${HANDLER_SATURATED_P99_SECONDS} s (p99) for ${HANDLER_SATURATED_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-handler-duration-saturated", severity: "ticket" },
  { id: "job_failure_share", summary: `job failures exceed ${JOB_FAILURE_SHARE_RATIO * 100}% of recent runs (min ${JOB_FAILURE_SHARE_MIN_RUNS} runs)`, runbook: "runbook.md#alert-job-failure-share", severity: "ticket" },
  { id: "rest_token_invalid", summary: `bot token latched invalid (credential death; every REST send refuses)`, runbook: "runbook.md#alert-rest-token-invalid" },
  { id: "rest_refusal_surge", summary: `guard invalid + refused requests grew ${REST_REFUSAL_SURGE_MIN}+ between samples`, runbook: "runbook.md#alert-rest-refusal-surge" },
  { id: "rest_global_responses_stuck", summary: `global-pause response bodies unresolved for ${REST_PENDING_STUCK_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-rest-global-responses-stuck" },
  { id: "feed_fetch_failures", summary: `feed fetch errors increased between samples`, runbook: "runbook.md#alert-feed-fetch-failures", severity: "ticket" },
  { id: "eventsub_receive_gap", summary: `EventSub received deliveries exceed classified deliveries for ${EVENTSUB_RECEIVE_GAP_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-eventsub-receive-gap", severity: "ticket" },
  { id: "eventsub_reconcile_prior_failed", summary: `EventSub reconcile prior-failed subscription findings increased between samples`, runbook: "runbook.md#alert-eventsub-reconcile-prior-failed", severity: "ticket" },
  { id: "automod_sanction_refused", summary: `automod sanction refusals increased between samples`, runbook: "runbook.md#alert-automod-sanction-refused", severity: "ticket" },
  { id: "moderation_enforcement_failures", summary: `moderation enforcement failures increased between samples`, runbook: "runbook.md#alert-moderation-enforcement-failures", severity: "ticket" },
  { id: "voice_rest_stuck", summary: `voice REST attempt in flight past ${VOICE_REST_STUCK_SECONDS} s for ${VOICE_REST_STUCK_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-voice-rest-stuck", severity: "ticket" },
  { id: "voice_global_backoff_capped", summary: `voice global-backoff caps increased between samples`, runbook: "runbook.md#alert-voice-global-backoff-capped", severity: "ticket" },
  { id: "gateway_drain_skips", summary: `gateway auxiliary drains skipped for schema gaps increased between samples`, runbook: "runbook.md#alert-gateway-drain-skips", severity: "ticket" },
];

/**
 * Public docs base for fired-packet runbook deep links. The packet carries the
 * full URL (not the relative `docs/...` path) so the soak operator can jump
 * straight from the webhook message to the matching runbook section.
 */
export const RUNBOOK_BASE_URL = "https://github.com/TogetherWeOwn/two-bot-next/blob/main/docs/";

/** Full deep link for a rule's runbook anchor. */
export function runbookUrl(rule: RuleDef): string {
  return `${RUNBOOK_BASE_URL}${rule.runbook}`;
}

/**
 * Public docs link for the Worker/Container log query backing every alert.
 * A stable docs URL (not an account-scoped dashboard link) so webhook
 * messages stay public-safe: the operator opens the environment's Workers
 * Observability logs from there.
 */
export const LOG_QUERY_URL = "https://developers.cloudflare.com/workers/observability/logs/workers-logs/";

/** Build/SHA context appended to every alert message and alert log line. */
export interface AlertContext {
  /** Container Git SHA from `/readyz` (`unknown` when the probe failed). */
  buildRevision?: string | undefined;
  /** Failing gateway step from `/readyz` (`gateway_failure.failed_step`). */
  failedStep?: string | undefined;
  /** SQLSTATE class from `/readyz` (`gateway_failure.error_code`). */
  errorCode?: string | undefined;
}

/** Shared suffix carrying build, failure detail and the log-query link. */
export function alertContextSuffix(context?: AlertContext): string {
  const build = context?.buildRevision?.trim() ? context.buildRevision.trim() : "unknown";
  let suffix = ` Build: ${build}. Logs: ${LOG_QUERY_URL}`;
  const step = context?.failedStep?.trim();
  const code = context?.errorCode?.trim();
  if (step || code) {
    suffix += ` Failure: ${step || "unknown"}/${code || "none"}.`;
  }
  return suffix;
}

export interface MetricsAlertState {
  /** Rule ids (with subject) currently firing, e.g. `job_stale:rank`. */
  firing: string[];
  /** Latest fixed failure class by job; absent on state stored by older Worker versions. */
  jobErrorClasses?: Record<string, JobErrorClass>;
  rest429: number;
  /** Missing on state stored before REST 5xx tracking was introduced. */
  rest5xx?: number;
  restTotal: number;
  poolStreak: number;
  /** Consecutive valid engaged halt samples; absent in older DO storage. */
  auditHaltStreak?: number;
  dbErrors: number;
  sendBlocked: number;
  sendBlockedStreak: number;
  /**
   * Consecutive valid samples with the REST admission breaker open, capped
   * at REST_BREAKER_OPEN_SAMPLES. Absent on state stored by older Worker
   * versions; legacy storage reads as no streak, never as sustained evidence.
   */
  breakerOpenStreak?: number;
  /**
   * Latest guard invalid-observation count for the refusal-surge rule. Absent
   * on state stored before this rule existed: the next valid sample stores a
   * new baseline instead of a delta.
   */
  restRefusalInvalid?: number;
  /** Latest guard local-refusal count, same absence semantics as above. */
  restRefusalRejected?: number;
  /** False until the first valid sample stores a surge baseline: the first sample never fires. */
  restRefusalSeen?: boolean;
  /**
   * Consecutive valid samples with unresolved global-pause response bodies,
   * capped at REST_PENDING_STUCK_SAMPLES. Absent on state stored by older
   * Worker versions; legacy storage reads as no streak, never as sustained
   * evidence.
   */
  restPendingStuckStreak?: number;
  /**
   * Latest `reason="verdict"` prefix-refusal count. Absent on state stored
   * before this rule existed and after a missing/invalid sample: the next
   * valid sample stores a new baseline instead of a rise.
   */
  prefixVerdictRefused?: number;
  /** Consecutive rising verdict windows, capped at PREFIX_TRIGGER_VERDICT_SAMPLES. */
  prefixVerdictStreak?: number;
  /** False until the first valid sample stores a baseline: the first sample never fires. */
  prefixVerdictSeen?: boolean;
  voiceOps: number;
  voiceFailures: number;
  voiceDeadLetters: number;
  voiceOrphans: number;
  /** Summed `discord_error`/`gave_up`/`permission_missing` vote-kick outcomes at the last valid sample. */
  voiceVoteKickFailures: number;
  /** False until a valid vote-kick family sample stores a baseline: the first sample never fires. */
  voiceVoteKickFailuresSeen: boolean;
  /** True when the last evaluation left `voice_failures` firing via the vote-kick arm (growth or hold). */
  voiceVoteKickFiring: boolean;
  gatewayMissed: number;
  /** False until the first evaluation stores a baseline: the first sample never fires. */
  gatewayMissedSeen: boolean;
  /** `two_bot_voice_live_self_heal_total` at the last valid sample. */
  voiceLiveSelfHeal: number;
  /** False until a valid sample stores the self-heal baseline: the first sample never fires. */
  voiceLiveSelfHealSeen: boolean;
  checkpointFailures: number;
  /** False until the first evaluation stores a baseline: the first sample never fires. */
  checkpointFailuresSeen: boolean;
  /** Refused `two_bot_internal_actions_total` outcomes summed by family. */
  receiverRefusals: Record<string, number>;
  /** False until the first evaluation stores a baseline: the first sample never fires. */
  receiverRefusalsSeen: boolean;
  /** Per-family consecutive windows with new refusals (sustained surge, not one probe). */
  receiverRefusalStreaks: Record<string, number>;
  /** EventSub `tampered` counter at the last valid baseline or window. */
  eventsubTampered: number;
  /** False until a valid sample stores the EventSub baseline: the first sample never fires. */
  eventsubTamperedSeen: boolean;
  /** Consecutive windows with new tampered deliveries (sustained surge, not one probe). */
  eventsubTamperedStreak: number;
  /** Gateway disconnect counter at the last valid baseline or window. */
  gatewayDisconnects: number;
  /** RESUMED plus fresh READY counters at the last valid baseline or window. */
  gatewayRecoveries: number;
  /** False until a valid sample stores the gateway baseline: the first sample never fires. */
  gatewayUnpairedSeen: boolean;
  /** Disconnect outstanding with no RESUMED or fresh READY since (0 or 1). */
  gatewayUnpaired: number;
  /** Consecutive completed samples with an unpaired disconnect outstanding. */
  gatewayUnpairedStreak: number;
  /** Same-scrape process identity for the gateway baseline; absent in legacy storage. */
  gatewayProcessIncarnation?: string | undefined;
  /** False until the first valid gateway-readiness sample: the first sample never fires. */
  gatewayNotReadySeen: boolean;
  /** Consecutive valid samples with the /readyz gateway component non-ready. */
  gatewayNotReadyStreak: number;
  /** Per-lane baseline; absent after a missing/invalid sample or in legacy storage. */
  dispatchDrops?: Record<string, number>;
  /** Consecutive positive deltas, capped at DISPATCH_DROP_SAMPLES. */
  dispatchDropStreaks?: Record<string, number>;
  /** Per-reason community-facts drain-failure baseline; absent after a missing/invalid sample or in legacy storage. */
  communityFactsDrainFailures?: Record<string, number>;
  /** Consecutive completed windows with positive summed drain-failure growth, capped at COMMUNITY_FACTS_DRAIN_SAMPLES. */
  communityFactsDrainStreak?: number;
  /** Consecutive valid positive event-store lag samples; absent in legacy storage reads as 0. */
  eventStoreLagStreak?: number;
  /** Healthy reconcile passes at the last valid baseline; absent in legacy storage. */
  reconcileHealthy?: number;
  /** Verification-pending reconcile passes at the last valid baseline; absent in legacy storage. */
  reconcilePending?: number;
  /** Failed reconcile passes at the last valid baseline; absent in legacy storage. */
  reconcileFailed?: number;
  /** False until a valid sample stores the reconcile baseline: the first sample never fires. */
  reconcileSeen?: boolean;
  /** `two_bot_retention_replay_needed_total` at the last valid sample; absent in legacy storage. */
  retentionReplayNeeded?: number;
  /** False until a valid sample stores the retention baseline: the first sample never fires. */
  retentionReplayNeededSeen?: boolean;
  /** Last valid auxiliary-drain skip count; legacy storage baselines first. */
  gatewayDrainSkips?: number;
  gatewayDrainSkipsSeen?: boolean;
  /** `two_bot_voice_dispatch_timeouts_total` at the last valid sample; absent in legacy storage. */
  voiceDispatchTimeouts?: number;
  /** False until a valid sample stores the dispatch-timeout baseline: the first sample never fires. */
  voiceDispatchTimeoutsSeen?: boolean;
  /** `two_bot_voice_watchdog_stucks_total` at the last valid sample; absent in legacy storage. */
  voiceWatchdogStucks?: number;
  /** False until a valid sample stores the watchdog-stuck baseline: the first sample never fires. */
  voiceWatchdogStucksSeen?: boolean;
  /** `two_bot_voice_halt_recoveries_total` at the last valid sample; absent in legacy storage. */
  voiceHaltRecoveries?: number;
  /** False until a valid sample stores the halt-recovery baseline: the first sample never fires. */
  voiceHaltRecoveriesSeen?: boolean;
  /** `two_bot_rest_guard_breaker_opens_total` at the last valid sample; absent in legacy storage. */
  restBreakerOpens?: number;
  /** False until a valid sample stores the breaker-opens baseline: the first sample never fires. */
  restBreakerOpensSeen?: boolean;
  /** `two_bot_rest_guard_global_pauses_total` at the last valid sample; absent in legacy storage. */
  restGlobalPauses?: number;
  /** False until a valid sample stores the global-pause baseline: the first sample never fires. */
  restGlobalPausesSeen?: boolean;
  /** Summed `two_bot_feed_fetch_errors_total` at the last valid sample; absent in legacy storage. */
  feedFetchErrors?: number;
  /** False until a valid sample stores the feed baseline: the first sample never fires. */
  feedFetchErrorsSeen?: boolean;
  /** Consecutive valid samples with received ahead of classified, capped at EVENTSUB_RECEIVE_GAP_SAMPLES; absent in legacy storage. */
  eventsubReceiveGapStreak?: number;
  /** `reconcile_prior_failed` errors at the last judged pass window; absent in legacy storage. */
  reconcilePriorFailed?: number;
  /** Healthy-pass baseline paired with the finding window; absent in legacy storage. */
  reconcilePriorFailedPassHealthy?: number;
  /** Pending-pass baseline paired with the finding window; absent in legacy storage. */
  reconcilePriorFailedPassPending?: number;
  /** Failed-pass counter baseline for prior-failed suppression; absent in legacy storage. */
  reconcilePriorFailedPassFailed?: number;
  /** False until a valid sample stores the prior-failed baseline: the first sample never fires. */
  reconcilePriorFailedSeen?: boolean;
  /** `sanction_refused` automod decisions at the last valid sample; absent in legacy storage. */
  automodSanctionRefused?: number;
  /** False until a valid sample stores the sanction baseline: the first sample never fires. */
  automodSanctionRefusedSeen?: boolean;
  /** Summed moderation `discord_error`/`storage_error` outcomes at the last valid sample; absent in legacy storage. */
  moderationEnforcementFailures?: number;
  /** False until a valid sample stores the moderation baseline: the first sample never fires. */
  moderationEnforcementFailuresSeen?: boolean;
  /** Consecutive valid samples with a stuck voice REST attempt, capped at VOICE_REST_STUCK_SAMPLES; absent in legacy storage. */
  voiceRestStuckStreak?: number;
  /** `two_bot_voice_global_backoff_capped_total` at the last valid sample; absent in legacy storage. */
  voiceBackoffCapped?: number;
  /** False until a valid sample stores the backoff baseline: the first sample never fires. */
  voiceBackoffCappedSeen?: boolean;
  /**
   * Last finite gateway latency reading in seconds; absent when the last
   * valid sample was unknown (NaN/absent). NaN is never stored: JSON state
   * cannot round-trip it, so unknown clears this field instead.
   */
  gatewayLatencyLast?: number | undefined;
  /** Consecutive valid samples with no fresh ACK evidence (unknown or unchanged), capped at GATEWAY_LATENCY_DEAD_SAMPLES; absent in legacy storage. */
  gatewayLatencyDeadStreak?: number;
  /** False until the first valid latency sample: the first sample never fires. */
  gatewayLatencySeen?: boolean;
  /** `two_bot_gateway_reconnects_total` at the last valid sample; absent in legacy storage. */
  gatewayReconnects?: number;
  /** False until a valid sample stores the reconnect baseline: the first sample never fires. */
  gatewayReconnectsSeen?: boolean;
  /** Consecutive rising reconnect windows, capped at GATEWAY_RECONNECT_FLAP_SAMPLES. */
  gatewayReconnectStreak?: number;
  /** Per-`le` handler bucket baseline; absent after a missing/invalid sample or in legacy storage. */
  handlerBuckets?: Record<string, number>;
  /** Consecutive saturated handler windows, capped at HANDLER_SATURATED_SAMPLES. */
  handlerStreak?: number;
  /** Per-job accumulated run baseline since the last judged window; absent in legacy storage. */
  jobShareBase?: Record<string, { failure: number; total: number }>;
  /** False until the first valid job-runs sample: the first sample never fires. */
  jobShareSeen?: boolean;
  /**
   * Creator-join burn snapshots for the SLO windows, oldest first. Each entry
   * carries the scrape time plus cumulative counters (received joins,
   * benign duplicate refusals, moves within 5 s). Absent on state stored by
   * older Worker versions; the first valid sample only stores a baseline.
   */
  joinBurn?: JoinBurnSnapshot[];
  /** Baseline for `two_bot_eventsub_posts_total` by fixed outcome; absent before the first valid sample or in legacy storage. */
  eventsubPosts?: Record<string, number>;
  /** False until the first valid sample stores the post baseline: the first sample never fires. */
  eventsubPostsSeen?: boolean;
  /**
   * Error-budget burn snapshots for the 15 min / 2 min fast-burn windows,
   * oldest first. Each entry carries the scrape time plus cumulative REST
   * counters (eligible requests, 5xx plus transport errors). Absent on state
   * stored by older Worker versions; the first valid sample only stores a
   * baseline.
   */
  errorBudgetBurn?: ErrorBudgetBurnSnapshot[];
}

/** One cumulative creator-join SLI snapshot feeding the burn-rate windows. */
export interface JoinBurnSnapshot {
  t: number;
  received: number;
  duplicate: number;
  fastMoves: number;
}

/** One cumulative REST error-budget snapshot feeding the fast-burn windows. */
export interface ErrorBudgetBurnSnapshot {
  t: number;
  total: number;
  errors: number;
}

interface ErrorBudgetBurnResult {
  fast: boolean;
  slow: boolean;
  /** Inputs were missing or malformed: the caller retains the previous firing state. */
  held: boolean;
  /** Slow-burn windows were unknown: the caller holds a firing slow ticket. */
  slowHeld: boolean;
  ring: ErrorBudgetBurnSnapshot[];
}

/**
 * Multiwindow burn rate over the REST error budget (`docs/error-budget-alerts.md`):
 * 99.9% of eligible REST requests must succeed over the 48h watch, so every
 * `two_bot_rest_requests_total` sample is eligible traffic and `5xx` plus
 * `transport` outcomes spend the budget (`4xx`/`429` are caller or
 * rate-limit outcomes, not spend). Pages when both the 15 min long window
 * and the 2 min short window burn faster than `ERROR_BUDGET_FAST_BURN`,
 * less the scrape-quantization allowance; tickets when both the 2 h long
 * window and the 15 min short window burn faster than
 * `ERROR_BUDGET_SLOW_BURN`, less the same allowance. `held` is true when the
 * REST series are absent or malformed and the ring is carried unchanged (both
 * rules hold); `slowHeld` tracks unknown slow-window coverage separately,
 * holding only while neither slow window is covered below its threshold. A
 * backwards counter (process restart) rebuilds the ring from this sample,
 * fires nothing, and holds a firing slow ticket across the restart.
 * Otherwise the sample is appended, entries older than
 * the 2 h slow window plus margin are pruned but the newest pre-cutoff
 * snapshot is always kept as the window anchor, and each window burns as
 * errors / (requests x budget) against its threshold. A window fires only
 * with full history coverage (a sample at or before its start and no older
 * than the boundary plus the baseline tolerance), enough eligible requests,
 * and at least two budget errors, so fresh rings, quiet traffic and one bad
 * minute stay silent. A window without coverage is unknown rather than
 * quiet: the caller holds an existing firing state instead of resolving a
 * page or ticket on missing history. A fast page resolves only on full
 * coverage below threshold; a slow ticket already resolves when either
 * slow window is covered quiet or below threshold, even while the other
 * window is still unknown.
 */
function errorBudgetBurnEvaluation(
  samples: Sample[],
  ring: ErrorBudgetBurnSnapshot[],
  nowSeconds: number,
): ErrorBudgetBurnResult {
  const rows = samples.filter((s) => s.name === "two_bot_rest_requests_total");
  if (rows.length === 0) {
    return { fast: false, slow: false, held: true, slowHeld: true, ring };
  }
  let total = 0;
  let errors = 0;
  for (const s of rows) {
    if (!Number.isSafeInteger(s.value) || s.value < 0) {
      return { fast: false, slow: false, held: true, slowHeld: true, ring };
    }
    total += s.value;
    if (s.labels["result"] === "5xx" || s.labels["result"] === "transport") {
      errors += s.value;
    }
  }
  if (!Number.isSafeInteger(total) || !Number.isSafeInteger(errors)) {
    return { fast: false, slow: false, held: true, slowHeld: true, ring };
  }
  const snapshot: ErrorBudgetBurnSnapshot = { t: nowSeconds, total, errors };
  const last = ring[ring.length - 1];
  if (last !== undefined && (total < last.total || errors < last.errors)) {
    // A backwards counter (process restart) rebuilds the ring from this
    // sample and fires nothing, but a firing slow ticket survives the
    // restart: the fresh ring covers neither slow window, so resolving here
    // would post a meaningless RESOLVED and blind the rule for a full 2 h.
    // A covered quiet or below-threshold 15 min window still resolves it.
    return { fast: false, slow: false, held: false, slowHeld: true, ring: [snapshot] };
  }
  const cutoff = nowSeconds - ERROR_BUDGET_SLOW_LONG_SECONDS - ERROR_BUDGET_FAST_HISTORY_MARGIN_SECONDS;
  const keep = ring.filter((s) => s.t > cutoff && s.t <= nowSeconds);
  // Anchor: always retain the newest pre-cutoff snapshot. A failed scrape
  // never evaluates, so after a scrape gap the first samples past the gap
  // have no baseline at or before the long-window start; without the anchor
  // a still-firing burn reads as uncovered and spuriously resolves, then
  // re-pages on the next scrape. Counters are cumulative, so the anchored
  // delta still measures exactly the errors and requests since the anchor.
  const anchor = [...ring].reverse().find((s) => s.t <= cutoff);
  const next = [...(anchor ? [anchor] : []), ...keep, snapshot];
  // A window is a measured rate, an unknown coverage gap, or a covered quiet
  // window. Unknown holds an existing page or ticket; only a covered window
  // below the volume floors or the threshold resolves one.
  const burn = (
    windowSeconds: number,
    minEvents: number,
    minErrors: number,
  ): { rate: number } | { unknown: true } | { quiet: true } => {
    let base: ErrorBudgetBurnSnapshot | undefined;
    for (const s of next) {
      if (s.t <= nowSeconds - windowSeconds) base = s;
    }
    // No sample at or before the window start means the ring does not yet
    // cover this full window, so wait for history rather than firing.
    if (base === undefined) return { unknown: true };
    // A baseline older than the boundary plus the scrape-alignment tolerance
    // means a boundary scrape was missed: the window would sweep errors from
    // outside the claimed span, so treat it as unknown rather than firing on
    // stale errors.
    if (nowSeconds - windowSeconds - base.t > ERROR_BUDGET_FAST_BASELINE_TOLERANCE_SECONDS) {
      return { unknown: true };
    }
    const windowTotal = total - base.total;
    const windowErrors = errors - base.errors;
    if (windowTotal < minEvents) return { quiet: true };
    if (windowErrors < minErrors) return { quiet: true };
    return { rate: windowErrors / (windowTotal * ERROR_BUDGET_RATIO) };
  };
  const fastThreshold = ERROR_BUDGET_FAST_BURN * (1 - ERROR_BUDGET_FAST_TOLERANCE);
  const fastLong = burn(
    ERROR_BUDGET_FAST_LONG_SECONDS,
    ERROR_BUDGET_FAST_MIN_EVENTS,
    ERROR_BUDGET_FAST_MIN_ERRORS,
  );
  const fastShort = burn(
    ERROR_BUDGET_FAST_SHORT_SECONDS,
    ERROR_BUDGET_FAST_MIN_EVENTS,
    ERROR_BUDGET_FAST_MIN_ERRORS,
  );
  // Window edges land on whole keepalive scrapes for the slow windows too,
  // so the same ~1% allowance keeps threshold-sustained burn from flapping.
  const slowThreshold = ERROR_BUDGET_SLOW_BURN * (1 - ERROR_BUDGET_FAST_TOLERANCE);
  const slowLong = burn(
    ERROR_BUDGET_SLOW_LONG_SECONDS,
    ERROR_BUDGET_SLOW_MIN_EVENTS,
    ERROR_BUDGET_SLOW_MIN_ERRORS,
  );
  const slowShort = burn(
    ERROR_BUDGET_SLOW_SHORT_SECONDS,
    ERROR_BUDGET_SLOW_MIN_EVENTS,
    ERROR_BUDGET_SLOW_MIN_ERRORS,
  );
  return {
    fast:
      "rate" in fastLong &&
      "rate" in fastShort &&
      fastLong.rate >= fastThreshold &&
      fastShort.rate >= fastThreshold,
    slow:
      "rate" in slowLong &&
      "rate" in slowShort &&
      slowLong.rate >= slowThreshold &&
      slowShort.rate >= slowThreshold,
    held: "unknown" in fastLong || "unknown" in fastShort,
    // An unknown slow window holds a firing ticket only while the other
    // window can still meet its threshold. A covered quiet or
    // below-threshold window resolves the ticket even while the other
    // window is still unknown (for example a quiet 15 min window after a
    // restart or a scrape gap that aged the 2 h baseline out of tolerance).
    slowHeld:
      ("unknown" in slowLong || "unknown" in slowShort) &&
      !(
        "quiet" in slowLong ||
        "quiet" in slowShort ||
        ("rate" in slowLong && slowLong.rate < slowThreshold) ||
        ("rate" in slowShort && slowShort.rate < slowThreshold)
      ),
    ring: next,
  };
}

export const EMPTY_STATE: MetricsAlertState = { firing: [], jobErrorClasses: {}, rest429: 0, rest5xx: 0, restTotal: 0, poolStreak: 0, dbErrors: 0, sendBlocked: 0, sendBlockedStreak: 0, breakerOpenStreak: 0, restPendingStuckStreak: 0, voiceOps: 0, voiceFailures: 0, voiceDeadLetters: 0, voiceOrphans: 0, voiceVoteKickFailures: 0, voiceVoteKickFailuresSeen: false, voiceVoteKickFiring: false, gatewayMissed: 0, gatewayMissedSeen: false, voiceLiveSelfHeal: 0, voiceLiveSelfHealSeen: false, checkpointFailures: 0, checkpointFailuresSeen: false, receiverRefusals: {}, receiverRefusalsSeen: false, receiverRefusalStreaks: {}, gatewayDisconnects: 0, gatewayRecoveries: 0, gatewayUnpairedSeen: false, gatewayUnpaired: 0, gatewayUnpairedStreak: 0, gatewayNotReadySeen: false, gatewayNotReadyStreak: 0, dispatchDrops: {}, dispatchDropStreaks: {}, communityFactsDrainFailures: {}, communityFactsDrainStreak: 0, eventsubPosts: {}, eventsubPostsSeen: false, eventStoreLagStreak: 0, eventsubTampered: 0, eventsubTamperedSeen: false, eventsubTamperedStreak: 0, reconcileHealthy: 0, reconcilePending: 0, reconcileFailed: 0, reconcileSeen: false, retentionReplayNeeded: 0, retentionReplayNeededSeen: false, voiceDispatchTimeouts: 0, voiceDispatchTimeoutsSeen: false, voiceWatchdogStucks: 0, voiceWatchdogStucksSeen: false, voiceHaltRecoveries: 0, voiceHaltRecoveriesSeen: false, restBreakerOpens: 0, restBreakerOpensSeen: false, restGlobalPauses: 0, restGlobalPausesSeen: false, gatewayLatencyDeadStreak: 0, gatewayLatencySeen: false, gatewayReconnects: 0, gatewayReconnectsSeen: false, gatewayReconnectStreak: 0, handlerBuckets: {}, handlerStreak: 0, jobShareBase: {}, jobShareSeen: false, feedFetchErrors: 0, feedFetchErrorsSeen: false, eventsubReceiveGapStreak: 0, reconcilePriorFailed: 0, reconcilePriorFailedPassHealthy: 0, reconcilePriorFailedPassPending: 0, reconcilePriorFailedPassFailed: 0, reconcilePriorFailedSeen: false, automodSanctionRefused: 0, automodSanctionRefusedSeen: false, moderationEnforcementFailures: 0, moderationEnforcementFailuresSeen: false, voiceRestStuckStreak: 0, voiceBackoffCapped: 0, voiceBackoffCappedSeen: false };

/** An unsuccessful scrape breaks the dispatch, breaker, lag, pending-global, latency, reconnect, handler, receive-gap, and REST-stuck streaks, never an existing alert. */
export function interruptDispatchDrops(state: MetricsAlertState): MetricsAlertState {
  return { ...state, dispatchDrops: {}, dispatchDropStreaks: {}, breakerOpenStreak: 0, eventStoreLagStreak: 0, restPendingStuckStreak: 0, gatewayLatencyDeadStreak: 0, gatewayLatencySeen: false, gatewayReconnectStreak: 0, gatewayReconnectsSeen: false, handlerBuckets: {}, handlerStreak: 0, eventsubReceiveGapStreak: 0, voiceRestStuckStreak: 0 };
}

/** A failed scrape breaks the engaged run, but cannot prove a halt cleared. */
export function interruptAuditHalt(state: MetricsAlertState): MetricsAlertState {
  return { ...state, auditHaltStreak: 0 };
}

/** A failed scrape breaks drain-growth evidence, never an active drain ticket. */
export function interruptCommunityFactsDrain(state: MetricsAlertState): MetricsAlertState {
  return { ...state, communityFactsDrainFailures: {}, communityFactsDrainStreak: 0 };
}

/** A failed scrape breaks verdict-growth evidence, never an active verdict ticket. */
export function interruptPrefixVerdictRefusals(state: MetricsAlertState): MetricsAlertState {
  // The verdict fields are optional: deleting them restores the absent shape
  // legacy storage already has, so the next valid sample re-baselines.
  const next = { ...state };
  delete next.prefixVerdictRefused;
  delete next.prefixVerdictStreak;
  delete next.prefixVerdictSeen;
  return next;
}

export function parseExposition(text: string): Sample[] {
  const samples: Sample[] = [];
  for (const line of text.split("\n")) {
    if (line === "" || line.startsWith("#")) continue;
    const m = /^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{([^}]*)\})?\s+(\S+)/.exec(line);
    if (!m) continue;
    const labels: Record<string, string> = {};
    for (const part of (m[2] ?? "").matchAll(/([a-zA-Z_][a-zA-Z0-9_]*)="((?:[^"\\]|\\.)*)"/g)) {
      labels[part[1]!] = part[2]!;
    }
    samples.push({ name: m[1]!, labels, value: Number(m[3]) });
  }
  return samples;
}

const GATEWAY_UNPAIRED_RULE = "gateway_unpaired_disconnect";
const REST_BREAKER_OPEN_RULE = "rest_admission_breaker_open";

type GatewayPairingFields = Pick<
  MetricsAlertState,
  "gatewayDisconnects" | "gatewayRecoveries" | "gatewayUnpairedSeen" | "gatewayUnpaired" | "gatewayUnpairedStreak" | "gatewayProcessIncarnation"
>;

/**
 * One counter series value, or undefined when absent, duplicated, or not a
 * count. Takes the per-series pre-filter from the gauge helper below, so the
 * scrape-validation guard ("New consumed families cannot silently bypass
 * validation") covers these series: a Rust rename or malformed value fails
 * the scrape instead of silently holding the rule off. The
 * safe-integer/exact-label/single-row checks stay as a backstop for direct
 * `evaluateMetrics` callers.
 */
function gatewayCounter(matches: Sample[], event?: string): number | undefined {
  const scoped = event === undefined ? matches : matches.filter((s) => s.labels["event"] === event);
  if (scoped.length !== 1) return undefined;
  const row = scoped[0]!;
  if (Object.keys(row.labels).length !== (event === undefined ? 0 : 1)) return undefined;
  return Number.isSafeInteger(row.value) && row.value >= 0 ? row.value : undefined;
}

/** Validates the complete optional family, shared with the strict scrape parser. */
export function processIncarnation(rows: Sample[]): string | undefined {
  if (rows.length !== 4) return undefined;
  const parts = ["0", "1", "2", "3"].map((part) => exactSeries(rows, { part }));
  if (parts.some((part) => part === undefined || part > 4294967295)
    || parts.every((part) => part === 0)) return undefined;
  return parts.map((part) => part!.toString(16).padStart(8, "0")).join("");
}

function validProcessIncarnation(value: unknown): value is string {
  return typeof value === "string" && /^[0-9a-f]{32}$/.test(value) && value !== "0".repeat(32);
}

/** Pairs disconnects with RESUMED plus fresh READY; invalid samples hold, restarts re-baseline. */
function pairGatewayDisconnects(samples: Sample[], prev: MetricsAlertState): { firing: boolean; fields: GatewayPairingFields; restarted: boolean } {
  const previousIncarnation = validProcessIncarnation(prev.gatewayProcessIncarnation) ? prev.gatewayProcessIncarnation : undefined;
  const carried: GatewayPairingFields = {
    gatewayDisconnects: prev.gatewayDisconnects ?? 0,
    gatewayRecoveries: prev.gatewayRecoveries ?? 0,
    gatewayUnpairedSeen: prev.gatewayUnpairedSeen ?? false,
    gatewayUnpaired: prev.gatewayUnpaired ?? 0,
    gatewayUnpairedStreak: prev.gatewayUnpairedStreak ?? 0,
    gatewayProcessIncarnation: previousIncarnation,
  };
  const wasFiring = prev.firing.includes(GATEWAY_UNPAIRED_RULE);
  const gauge = (name: string) => samples.filter((s) => s.name === name);
  const incarnationRows = gauge("two_bot_process_incarnation");
  const incarnation = processIncarnation(incarnationRows);
  // Missing identity remains compatible with old state + old containers. Once
  // attributed, losing identity is not a valid counter window or a recovery.
  if (incarnation === undefined && (incarnationRows.length > 0 || previousIncarnation !== undefined)) {
    return { firing: wasFiring, fields: carried, restarted: false };
  }
  const disconnects = gatewayCounter(gauge("two_bot_gateway_disconnects_total"));
  const resumes = gatewayCounter(gauge("two_bot_gateway_resumes_total"));
  const ready = gatewayCounter(gauge("two_bot_gateway_events_total"), "READY");
  if (disconnects === undefined || resumes === undefined || ready === undefined) {
    return { firing: wasFiring, fields: carried, restarted: false };
  }
  const recoveries = resumes + ready;
  if (!Number.isSafeInteger(recoveries)) return { firing: wasFiring, fields: carried, restarted: false };
  const baseline: GatewayPairingFields = {
    gatewayDisconnects: disconnects,
    gatewayRecoveries: recoveries,
    gatewayUnpairedSeen: true,
    gatewayUnpaired: 0,
    gatewayUnpairedStreak: 0,
    gatewayProcessIncarnation: incarnation,
  };
  if (!carried.gatewayUnpairedSeen) return { firing: false, fields: baseline, restarted: false };
  // Only two validated, attributed samples prove a process change. Adopting
  // identity into legacy state is not itself evidence of restart or recovery.
  if ((previousIncarnation !== undefined && incarnation !== previousIncarnation)
    || disconnects < carried.gatewayDisconnects || recoveries < carried.gatewayRecoveries) {
    return { firing: wasFiring, fields: baseline, restarted: true };
  }
  // Any recovery growth means the gateway came back: a RESUMED or fresh READY
  // landed since the previous sample. It always clears the outstanding loss,
  // even when the same window also added disconnects. The disconnect series
  // counts every failed Twilight retry, close frame, op 9 invalid session and
  // cold-resume IDENTIFY (`transport_disconnected` in crates/bot/src/gateway.rs),
  // so one recovered outage routinely adds N disconnects against a single
  // recovery; subtracting the growths would leave a permanent N-1 remainder
  // that pages forever (docs/cutover-dashboard-queries.md forbids exactly this
  // subtraction). Presence, not arithmetic, pairs each loss with its recovery.
  if (recoveries > carried.gatewayRecoveries) {
    return {
      firing: false,
      fields: baseline,
      restarted: false,
    };
  }
  // No recovery since the previous sample: new disconnect growth opens an
  // outstanding loss, and flat counters keep it outstanding. Only consecutive
  // outstanding samples count toward the bound. `> 0` (not `=== 1`) also
  // retires balances stored by older Worker versions that carried a count.
  const outstanding = disconnects > carried.gatewayDisconnects || carried.gatewayUnpaired > 0;
  const streak = outstanding ? carried.gatewayUnpairedStreak + 1 : 0;
  return {
    firing: outstanding && (streak >= GATEWAY_UNPAIRED_SAMPLES || wasFiring),
    fields: {
      gatewayDisconnects: disconnects,
      gatewayRecoveries: recoveries,
      gatewayUnpairedSeen: true,
      gatewayUnpaired: outstanding ? 1 : 0,
      gatewayUnpairedStreak: streak,
      gatewayProcessIncarnation: incarnation,
    },
    restarted: false,
  };
}

/**
 * One series (counter or gauge) with exactly these labels, or undefined when
 * absent, duplicated, mislabeled or not a safe nonnegative integer. Keeps a
 * Rust rename or malformed value from silently holding its rules off.
 */
function exactSeries(matches: Sample[], labels: Record<string, string>): number | undefined {
  const rows = matches.filter((s) =>
    Object.keys(s.labels).length === Object.keys(labels).length
    && Object.entries(labels).every(([key, value]) => s.labels[key] === value));
  if (rows.length !== 1) return undefined;
  const value = rows[0]!.value;
  return Number.isSafeInteger(value) && value >= 0 ? value : undefined;
}

/**
 * Fixed `two_bot_eventsub_posts_total` counters by outcome, or undefined when
 * the sample is missing or malformed. Each fixed outcome needs exactly one row
 * with only its `outcome` label and a safe nonnegative integer. Unknown
 * outcomes (including hostile `__proto__` rows) are ignored, never subjects;
 * a Rust rename or malformed value fails closed via the scrape validator and
 * holds here for direct `evaluateMetrics` callers.
 */
function eventsubPostCounts(matches: Sample[]): Record<string, number> | undefined {
  const counts: Record<string, number> = Object.create(null);
  for (const outcome of EVENTSUB_POST_OUTCOMES) {
    const value = exactSeries(matches, { outcome });
    if (value === undefined) return undefined;
    counts[outcome] = value;
  }
  return counts;
}

interface JoinBurnResult {
  fast: boolean;
  slow: boolean;
  /** Inputs were missing or malformed: the caller retains the previous firing state. */
  held: boolean;
  /** Both slow windows have a baseline snapshot: silence here is a confirmed clear. */
  slowCovered: boolean;
  ring: JoinBurnSnapshot[];
}

/**
 * Multiwindow burn rate over the creator-join SLI ring. `held` is true when
 * the join series are unusable and the ring is carried unchanged; a backwards
 * counter (process restart) rebuilds the ring from this sample and fires
 * nothing. Otherwise the sample is appended, entries older than the slow
 * window plus margin are pruned, and each fully covered window burns as
 * bad / (genuine x budget) against its threshold. Each window also needs the
 * minimum join volume and at least two bad moves to fire. `slowCovered` is
 * true only when both slow windows have baseline history; callers that
 * resolve on silence must also require it, because an uncovered window
 * reports no burn for lack of history rather than lack of errors.
 */
function joinBurnEvaluation(samples: Sample[], ring: JoinBurnSnapshot[], nowSeconds: number): JoinBurnResult {
  const gauge = (name: string) => samples.filter((s) => s.name === name);
  const received = exactSeries(gauge("two_bot_voice_joins_received_total"), {});
  const duplicate = exactSeries(gauge("two_bot_voice_join_refused_total"), { reason: "duplicate" });
  const fastMoves = exactSeries(gauge("two_bot_voice_join_to_move_seconds_bucket"), { le: "5" });
  if (received === undefined || duplicate === undefined || fastMoves === undefined) {
    return { fast: false, slow: false, held: true, slowCovered: false, ring };
  }
  const snapshot: JoinBurnSnapshot = { t: nowSeconds, received, duplicate, fastMoves };
  const last = ring[ring.length - 1];
  if (last !== undefined && (received < last.received || duplicate < last.duplicate || fastMoves < last.fastMoves)) {
    return { fast: false, slow: false, held: false, slowCovered: false, ring: [snapshot] };
  }
  const cutoff = nowSeconds - JOIN_SLOW_LONG_SECONDS - JOIN_BURN_HISTORY_MARGIN_SECONDS;
  const next = [...ring.filter((s) => s.t > cutoff && s.t <= nowSeconds), snapshot];
  // Receive/duplicate counters lag one scrape so joins can finish moving before
  // they enter the denominator. The denominator ends at the prior scrape while
  // moves end at the current histogram; compute bad moves from both the shifted
  // move-window baseline and the common denominator baseline, keeping the lower
  // estimate so an oldest-edge burst cannot masquerade as failed moves.
  const end = ring[ring.length - 1] ?? snapshot;
  const baselineAtOrBefore = (time: number, ceiling: number): JoinBurnSnapshot | undefined => {
    const eligible = next.filter((s) => s.t <= time && s.t <= ceiling);
    return eligible[eligible.length - 1];
  };
  const burn = (windowSeconds: number): number | undefined => {
    const receivedBase = baselineAtOrBefore(end.t - windowSeconds, end.t);
    const moveBase = baselineAtOrBefore(nowSeconds - windowSeconds, nowSeconds);
    // No sample at or before either boundary means the ring does not yet cover
    // this full window, so wait for history rather than treating it as complete.
    if (receivedBase === undefined || moveBase === undefined) return undefined;
    const genuine = (end.received - end.duplicate) - (receivedBase.received - receivedBase.duplicate);
    if (genuine < JOIN_BURN_MIN_JOINS) return undefined;
    const good = fastMoves - moveBase.fastMoves;
    const commonGenuine = (snapshot.received - snapshot.duplicate) - (moveBase.received - moveBase.duplicate);
    if (commonGenuine < JOIN_BURN_MIN_JOINS) return undefined;
    // Window-boundary skew can briefly move more joins than arrived: never
    // report negative burn, just no burn. Compare the offset-window burn with
    // an estimate whose arrival and move counters share the move baseline; the
    // latter excludes an oldest-edge arrival with no matching completion.
    const offsetBad = Math.max(0, genuine - good);
    const commonBad = Math.max(0, commonGenuine - good);
    if (offsetBad < JOIN_BURN_MIN_BAD || commonBad < JOIN_BURN_MIN_BAD) return undefined;
    return Math.min(offsetBad / (genuine * JOIN_SLO_BUDGET), commonBad / (commonGenuine * JOIN_SLO_BUDGET));
  };
  const fastLong = burn(JOIN_FAST_LONG_SECONDS);
  const fastShort = burn(JOIN_FAST_SHORT_SECONDS);
  const slowLong = burn(JOIN_SLOW_LONG_SECONDS);
  const slowShort = burn(JOIN_SLOW_SHORT_SECONDS);
  // Coverage is about history, not burn: a quiet-but-covered window still
  // counts, while a window without baselines must never read as a clear.
  const covers = (windowSeconds: number): boolean =>
    baselineAtOrBefore(end.t - windowSeconds, end.t) !== undefined
    && baselineAtOrBefore(nowSeconds - windowSeconds, nowSeconds) !== undefined;
  return {
    fast: fastLong !== undefined && fastShort !== undefined && fastLong > JOIN_FAST_BURN && fastShort > JOIN_FAST_BURN,
    slow: slowLong !== undefined && slowShort !== undefined && slowLong > JOIN_SLOW_BURN && slowShort > JOIN_SLOW_BURN,
    held: false,
    slowCovered: covers(JOIN_SLOW_LONG_SECONDS) && covers(JOIN_SLOW_SHORT_SECONDS),
    ring: next,
  };
}

export interface Evaluation {
  /** Firing keys `ruleId` or `ruleId:subject`. */
  firing: string[];
  state: MetricsAlertState;
  /** The creator-join burn inputs were unusable, so the previous firing state holds. */
  joinHeld: boolean;
  /** Both slow burn windows have baseline history; silence without it is unknown, not clear. */
  joinSlowCovered: boolean;
}

export function evaluateMetrics(samples: Sample[], prev: MetricsAlertState, nowSeconds: number, gatewayReady?: boolean): Evaluation {
  const firing: string[] = [];
  const gauge = (name: string) => samples.filter((s) => s.name === name);

  for (const s of gauge("two_bot_job_last_success_timestamp_seconds")) {
    const job = s.labels["job"] ?? "";
    const interval = JOB_INTERVAL_SECONDS[job];
    // Zero means never succeeded since start (parked/just started): not stale.
    if (interval !== undefined && s.value > 0 && nowSeconds - s.value > STALE_INTERVALS * interval) {
      firing.push(`job_stale:${job}`);
    }
  }
  for (const s of gauge("two_bot_job_consecutive_failures")) {
    if (s.value >= FAILURE_THRESHOLD) firing.push(`job_consecutive_failures:${s.labels["job"] ?? "other"}`);
  }
  const activeJobErrorClasses = new Map<string, string[]>();
  for (const s of gauge("two_bot_job_last_error_class")) {
    const job = s.labels["job"];
    if (s.value !== 1 || job === undefined) continue;
    const active = activeJobErrorClasses.get(job) ?? [];
    active.push(s.labels["class"] ?? "");
    activeJobErrorClasses.set(job, active);
  }
  const jobErrorClasses = Object.create(null) as Record<string, JobErrorClass>;
  for (const [job, classes] of activeJobErrorClasses) {
    const [errorClass] = classes;
    if (classes.length === 1 && errorClass !== undefined && JOB_ERROR_CLASS_SET.has(errorClass)) {
      jobErrorClasses[job] = errorClass as JobErrorClass;
    }
  }
  for (const s of gauge("two_bot_job_last_success_timestamp_seconds")) {
    const job = s.labels["job"] ?? "";
    // 15 s tickers wedge silently: skipped busy deadlines are neither success
    // nor failure, so neither job_stale nor job_consecutive_failures sees
    // them. Zero means never succeeded since start (boot) or never registered
    // (parked: DATABASE_URL unset, or the automations gate off): not stale.
    if (TICKER_STALE_JOBS.includes(job) && s.value > 0 && nowSeconds - s.value > TICKER_STALE_SECONDS) {
      firing.push(`ticker_stale:${job}`);
    }
  }
  for (const s of gauge("two_bot_job_last_success_timestamp_seconds")) {
    const job = s.labels["job"] ?? "";
    // Fail-closed staleness for jobs without a cadence entry: mapped jobs use
    // job_stale above and 15 s tickers use ticker_stale, so any other label
    // (today invite_snapshot, audit_retry and other; tomorrow any new series
    // the scrape accepts before the catalog maps it) would otherwise stay
    // silent however stale. A stale nonzero success timestamp raises a ticket
    // until the job gets an explicit cadence, ticker coverage, or event-driven
    // exclusion. `session_checkpoint` is excluded via UNKNOWN_STALE_EXCLUDED
    // (event-driven: idle guilds go stale while healthy). Zero means never
    // succeeded since start (boot) or never registered (parked): not stale.
    if (job !== "" && JOB_INTERVAL_SECONDS[job] === undefined && !TICKER_STALE_JOBS.includes(job)
      && !UNKNOWN_STALE_EXCLUDED.includes(job)
      && s.value > 0 && nowSeconds - s.value > UNKNOWN_JOB_STALE_SECONDS) {
      firing.push(`job_unknown_stale:${job}`);
    }
  }

  // An unreadable sweep retains the old gauge, not an absent sample. Its
  // supervisor failure count is therefore a veto on both firing and recovery.
  // Conservatively hold on other failed audit sweeps too, until one succeeds.
  // Standalone evaluator callers may supply just the halt; real scrapes require
  // the complete job-failure family in parseAlertScrape.
  const haltRows = gauge("two_bot_audit_delivery_halt");
  const halt = haltRows.length === 1 ? exactSeries(haltRows, {}) : undefined;
  const jobFailures = gauge("two_bot_job_consecutive_failures");
  const auditHealthy = jobFailures.length === 0
    || exactSeries(jobFailures, { job: "audit_retry" }) === 0;
  // The gauge defaults to 0 at process boot. Unlike an observed 1, a zero
  // cannot prove a read happened: require a successful sweep before recovery.
  const auditSuccessRows = gauge("two_bot_job_last_success_timestamp_seconds").filter((s) => s.labels["job"] === "audit_retry");
  const auditSuccess = exactSeries(auditSuccessRows, { job: "audit_retry" });
  const recoveryConfirmed = auditSuccessRows.length === 0 || (auditSuccess !== undefined && auditSuccess > 0);
  const validHalt = auditHealthy && (halt === 1 || (halt === 0 && recoveryConfirmed));
  const auditHaltStreak = validHalt && halt === 1
    ? Math.min((prev.auditHaltStreak ?? 0) + 1, AUDIT_HALT_SAMPLES) : 0;
  const haltWasFiring = prev.firing.includes("audit_delivery_halt");
  if ((!validHalt && haltWasFiring) || (validHalt && halt === 1
    && (haltWasFiring || auditHaltStreak >= AUDIT_HALT_SAMPLES))) {
    firing.push("audit_delivery_halt");
  }

  let rest429 = 0;
  let rest5xx = 0;
  let restTotal = 0;
  for (const s of gauge("two_bot_rest_requests_total")) {
    restTotal += s.value;
    if (s.labels["result"] === "429") rest429 += s.value;
    if (s.labels["result"] === "5xx") rest5xx += s.value;
  }
  // A counter that went backwards means the process restarted: no window.
  const reset = rest429 < prev.rest429 || restTotal < prev.restTotal;
  const dTotal = restTotal - prev.restTotal;
  const d429 = rest429 - prev.rest429;
  if (!reset && dTotal >= REST_429_MIN_REQUESTS && d429 / dTotal > REST_429_RATIO) firing.push("rest_429_rate");

  // Error-rate sibling of the 429 rule: a 5xx burst (Discord-side failures or
  // a broken request shape from a fresh deploy) pages like a hot 429 route.
  // `transport` (failures before response headers) stays out: timeouts and
  // cancellations already surface through job timeout classes. Missing 5xx
  // state from older DO storage establishes a baseline without firing.
  const hasRest5xxBaseline = prev.rest5xx !== undefined;
  const prevRest5xx = prev.rest5xx ?? 0;
  const reset5xx = rest5xx < prevRest5xx || restTotal < prev.restTotal;
  const d5xx = rest5xx - prevRest5xx;
  if (hasRest5xxBaseline && !reset5xx && dTotal >= REST_5XX_MIN_REQUESTS && d5xx / dTotal > REST_5XX_RATIO) firing.push("rest_5xx_rate");

  // Failed share of go-live announcement posts: failed delta over attempted
  // delta (mirroring `rest_5xx_rate`) raises one ticket. Announcement traffic
  // is low-volume (about one post per go-live per keepalive window), so a
  // strict per-sample window with a 10-attempt floor would stay silent
  // through a sustained total outage: sub-floor valid windows therefore hold
  // the baseline and accumulate attempts across consecutive valid samples,
  // and only a window reaching the floor is judged and rebaselines.
  // Low-volume windows stay quiet. The first valid sample is baseline only;
  // missing/invalid samples preserve firing and baseline; any backwards
  // counter (restart) rebaselines. Only a judged window (floor met) at or
  // below the line resolves; flat or sub-floor windows hold an active ticket
  // so a sustained outage stays one ALERT instead of flapping ALERT/RESOLVED.
  // Unknown outcomes are ignored, never subjects; no broadcaster, body,
  // token or free-text label ever reaches firing keys or messages.
  const eventsubWasFiring = prev.firing.includes("eventsub_post_failed_share");
  const eventsubCounts = eventsubPostCounts(gauge("two_bot_eventsub_posts_total"));
  let eventsubPosts: Record<string, number>;
  let eventsubPostsSeen = true;
  if (eventsubCounts === undefined) {
    eventsubPosts = prev.eventsubPosts ?? {};
    eventsubPostsSeen = prev.eventsubPostsSeen ?? false;
    if (eventsubWasFiring) firing.push("eventsub_post_failed_share");
  } else {
    const prevCounts = prev.eventsubPosts;
    const seen = prev.eventsubPostsSeen ?? false;
    const hasBaseline = seen && prevCounts !== undefined
      && EVENTSUB_POST_OUTCOMES.every((outcome) =>
        typeof prevCounts[outcome] === "number" && Number.isSafeInteger(prevCounts[outcome]) && (prevCounts[outcome] as number) >= 0);
    if (!hasBaseline) {
      eventsubPosts = { ...eventsubCounts };
      eventsubPostsSeen = true;
    } else {
      const resetPost = EVENTSUB_POST_OUTCOMES.some((outcome) =>
        (eventsubCounts[outcome] as number) < (prevCounts[outcome] as number));
      if (resetPost) {
        eventsubPosts = { ...eventsubCounts };
        eventsubPostsSeen = true;
      } else {
        const dAttempted = (eventsubCounts["attempted"] as number) - (prevCounts["attempted"] as number);
        const dFailed = (eventsubCounts["failed"] as number) - (prevCounts["failed"] as number);
        if (dAttempted >= EVENTSUB_POST_FAILED_SHARE_MIN_ATTEMPTED) {
          if (dFailed / dAttempted > EVENTSUB_POST_FAILED_SHARE_RATIO) {
            firing.push("eventsub_post_failed_share");
          }
          eventsubPosts = { ...eventsubCounts };
          eventsubPostsSeen = true;
        } else {
          // Sub-floor windows stay quiet and hold the baseline so attempts
          // accumulate toward the floor across consecutive valid samples.
          // An active ticket survives them: only a judged window clears it,
          // otherwise a sustained outage flaps ALERT/RESOLVED every window.
          eventsubPosts = prev.eventsubPosts ?? {};
          eventsubPostsSeen = true;
          if (eventsubWasFiring) firing.push("eventsub_post_failed_share");
        }
      }
    }
  }
  // REST error-budget burns (docs/error-budget-alerts.md): the fast rule
  // pages when both the 15 min and 2 min windows burn faster than 12x; the
  // slow rule tickets when both the 2 h and 15 min windows burn faster than
  // 3x. One bad window alone never fires. A window without coverage holds an
  // existing firing state; a fast page resolves only on full coverage below
  // threshold, while a slow ticket also resolves when either slow window is
  // covered quiet or below threshold. A counter reset holds a firing slow
  // ticket instead of resolving it.
  const errorBudgetBurn = errorBudgetBurnEvaluation(samples, prev.errorBudgetBurn ?? [], nowSeconds);
  if (errorBudgetBurn.fast) firing.push("error_budget_fast_burn");
  else if (prev.firing.includes("error_budget_fast_burn") && errorBudgetBurn.held) firing.push("error_budget_fast_burn");
  if (errorBudgetBurn.slow) firing.push("error_budget_slow_burn");
  else if (prev.firing.includes("error_budget_slow_burn") && errorBudgetBurn.slowHeld) firing.push("error_budget_slow_burn");

  const configured = gauge("two_bot_db_pool_configured")[0]?.value === 1;
  const size = gauge("two_bot_db_pool_connections")[0]?.value;
  const max = gauge("two_bot_db_pool_max_connections")[0]?.value;
  const idle = gauge("two_bot_db_pool_idle_connections")[0]?.value;
  const saturated = configured && max !== undefined && max > 0 && size === max && idle === 0;
  const poolStreak = saturated ? prev.poolStreak + 1 : 0;
  if (poolStreak >= POOL_SATURATED_SAMPLES) firing.push("db_pool_saturated");

  let dbErrors = 0;
  for (const s of gauge("two_bot_db_errors_total")) dbErrors += s.value;
  // A counter that went backwards means the process restarted: no window.
  // `?? 0` covers DO storage written before these fields existed.
  const prevDbErrors = prev.dbErrors ?? 0;
  const dbReset = dbErrors < prevDbErrors;
  if (!dbReset && dbErrors - prevDbErrors >= DB_ERROR_MIN_ERRORS) firing.push("db_errors");

  let sendBlocked = 0;
  for (const s of gauge("two_bot_send_admissions_total")) {
    if (s.labels["outcome"] === "blocked") sendBlocked += s.value;
  }
  const prevSendBlocked = prev.sendBlocked ?? 0;
  const sendReset = sendBlocked < prevSendBlocked;
  // Sustained refusal, not one busy tick: only windows with new refusals
  // extend the streak, so idle or self-clearing contention never pages.
  const sendBlockedStreak = sendReset || sendBlocked === prevSendBlocked ? 0 : (prev.sendBlockedStreak ?? 0) + 1;
  if (sendBlockedStreak >= SEND_BLOCKED_SAMPLES) firing.push("send_admission_blocked");

  // Sustained automod-verdict refusal of in-scope prefix candidates (mirror
  // `send_admission_blocked`): only `reason="verdict"` counts — `other` is
  // normal refusal volume and never advances the streak. Three consecutive
  // rising windows ticket (never page). The first valid sample only stores a
  // baseline, flat samples and counter resets (process restarts) clear the
  // streak and resolve, while missing/invalid samples break growth but hold
  // an active ticket (the return sample re-baselines). Unknown or hostile
  // reasons never fire and never enter messages: only the fixed enum value
  // is read, so no message content, verdict text or user label can leak in.
  const wasPrefixVerdictFiring = prev.firing.includes("prefix_trigger_verdict_refusals");
  const prefixVerdict = exactSeries(gauge("two_bot_gateway_prefix_trigger_refused_total"), { reason: "verdict" });
  // `??` covers DO storage written before these fields existed.
  const prevPrefixVerdict = prev.prefixVerdictRefused ?? 0;
  const prevPrefixVerdictSeen = prev.prefixVerdictSeen ?? false;
  const prevPrefixVerdictStreak = prev.prefixVerdictStreak ?? 0;
  let nextPrefixVerdict = prevPrefixVerdict;
  let nextPrefixVerdictStreak = 0;
  let nextPrefixVerdictSeen = prevPrefixVerdictSeen;
  let prefixVerdictFiring = false;
  if (prefixVerdict === undefined) {
    // Missing/invalid evidence breaks consecutive growth and drops the
    // baseline, but never falsely resolves an active ticket.
    nextPrefixVerdictSeen = false;
    prefixVerdictFiring = wasPrefixVerdictFiring;
  } else if (!prevPrefixVerdictSeen) {
    // The first valid sample (including after a gap) is only a baseline. A
    // new baseline is not a quiet window, so a held ticket stays held.
    nextPrefixVerdict = prefixVerdict;
    nextPrefixVerdictSeen = true;
    prefixVerdictFiring = wasPrefixVerdictFiring;
  } else if (prefixVerdict < prevPrefixVerdict) {
    // A backwards counter means the process restarted: re-baseline and resolve.
    nextPrefixVerdict = prefixVerdict;
  } else if (prefixVerdict === prevPrefixVerdict) {
    // A flat valid sample proves recovery and resolves.
    nextPrefixVerdict = prefixVerdict;
  } else {
    nextPrefixVerdictStreak = Math.min(prevPrefixVerdictStreak + 1, PREFIX_TRIGGER_VERDICT_SAMPLES);
    nextPrefixVerdict = prefixVerdict;
    prefixVerdictFiring = wasPrefixVerdictFiring || nextPrefixVerdictStreak >= PREFIX_TRIGGER_VERDICT_SAMPLES;
  }
  if (prefixVerdictFiring) firing.push("prefix_trigger_verdict_refusals");

  // REST admission breaker (local refusals before any REST wire attempt, so
  // the request counter and its 429/5xx rules never see them): three
  // consecutive valid open samples of the label-free 0/1 gauge file one
  // ticket, and one valid closed sample recovers it. Missing or malformed
  // samples break the streak but hold a firing ticket: they manufacture
  // neither sustained evidence nor recovery. `?? 0` covers DO storage
  // written before this field existed, and the cap keeps persisted state
  // bounded no matter how long the breaker stays open.
  const breakerRows = gauge("two_bot_rest_guard_breaker_open");
  const breakerOpen = breakerRows.length === 1 && Object.keys(breakerRows[0]!.labels).length === 0
    && (breakerRows[0]!.value === 0 || breakerRows[0]!.value === 1)
    ? breakerRows[0]!.value
    : undefined;
  const wasBreakerFiring = prev.firing.includes(REST_BREAKER_OPEN_RULE);
  let breakerOpenStreak: number;
  if (breakerOpen === 1) {
    breakerOpenStreak = Math.min((prev.breakerOpenStreak ?? 0) + 1, REST_BREAKER_OPEN_SAMPLES);
    if (breakerOpenStreak >= REST_BREAKER_OPEN_SAMPLES || wasBreakerFiring) firing.push(REST_BREAKER_OPEN_RULE);
  } else if (breakerOpen === 0) {
    breakerOpenStreak = 0;
  } else {
    breakerOpenStreak = 0;
    if (wasBreakerFiring) firing.push(REST_BREAKER_OPEN_RULE);
  }

  // Fatal bot-token latch (credential death): a valid 1 pages immediately,
  // with no streak bound. The latch defaults to 0 at boot and is set only by
  // a bot-authenticated 401 (`ratelimit_guard.rs`), so a 1 is never a boot
  // artifact and the first valid sample may fire. Every new REST attempt,
  // including essential interaction acknowledgements, refuses while latched,
  // and `/readyz` reports 503 until a restart with a corrected token. A valid
  // 0 resolves (only a new process without the latch reads 0); missing or
  // malformed samples hold a firing page without manufacturing recovery.
  const REST_TOKEN_RULE = "rest_token_invalid";
  const tokenValue = exactSeries(gauge("two_bot_rest_guard_token_invalid"), {});
  const tokenInvalid = tokenValue === 0 || tokenValue === 1 ? tokenValue : undefined;
  const wasTokenFiring = prev.firing.includes(REST_TOKEN_RULE);
  if (tokenInvalid === 1) {
    firing.push(REST_TOKEN_RULE);
  } else if (tokenInvalid === undefined && wasTokenFiring) {
    firing.push(REST_TOKEN_RULE);
  }

  // Guard refusal surge (mirror `db_errors`): combined invalid observations
  // (401/403/429 at headers) plus local refusals (breaker/global/token,
  // pre-wire) grew by REST_REFUSAL_SURGE_MIN or more between two consecutive
  // valid samples. Either family can surge alone: a 403 permission loop or a
  // Discord 429 storm grows invalid, while a global pause or an open breaker
  // grows rejected, and both burn the same IP-invalid budget toward the
  // breaker trip. The first valid sample only stores a baseline and never
  // fires, so lifetime counts predating the deploy do not page; a
  // below-threshold window resolves like every single-window rate rule.
  // Backwards movement is judged per counter, never as a whole-window
  // restart: the invalid family is not monotonic — a 429 tallied at headers
  // is withdrawn when its body shows shared scope (`retract_invalid`), so a
  // backwards invalid counter is a retraction, not a restart. A backwards
  // counter contributes zero growth for the window and re-baselines only
  // itself, while the other family's growth is still judged; a true restart
  // drives both counters back and re-baselines both quietly. Missing or
  // malformed samples keep the previous baseline and hold a firing page
  // instead of resolving it. `??` covers DO storage written before these
  // fields existed.
  const REST_SURGE_RULE = "rest_refusal_surge";
  const surgeWasFiring = prev.firing.includes(REST_SURGE_RULE);
  const surgeInvalid = exactSeries(gauge("two_bot_rest_guard_invalid_requests_total"), {});
  const surgeRejected = exactSeries(gauge("two_bot_rest_guard_rejected_requests_total"), {});
  const prevSurgeInvalid = prev.restRefusalInvalid ?? 0;
  const prevSurgeRejected = prev.restRefusalRejected ?? 0;
  const prevSurgeSeen = prev.restRefusalSeen ?? false;
  let restRefusalInvalid = prevSurgeInvalid;
  let restRefusalRejected = prevSurgeRejected;
  let restRefusalSeen = prevSurgeSeen;
  if (surgeInvalid === undefined || surgeRejected === undefined) {
    if (surgeWasFiring) firing.push(REST_SURGE_RULE);
  } else if (!prevSurgeSeen) {
    restRefusalInvalid = surgeInvalid;
    restRefusalRejected = surgeRejected;
    restRefusalSeen = true;
  } else {
    const invalidDelta = surgeInvalid >= prevSurgeInvalid ? surgeInvalid - prevSurgeInvalid : 0;
    const rejectedDelta = surgeRejected >= prevSurgeRejected ? surgeRejected - prevSurgeRejected : 0;
    restRefusalInvalid = surgeInvalid;
    restRefusalRejected = surgeRejected;
    restRefusalSeen = true;
    if (invalidDelta + rejectedDelta >= REST_REFUSAL_SURGE_MIN) firing.push(REST_SURGE_RULE);
  }

  // Stuck global-pause response bodies (mirror `event_store_consumer_lag`):
  // the label-free pending count stayed nonzero for
  // REST_PENDING_STUCK_SAMPLES consecutive valid samples. A pending global
  // body still blocks admission when its provisional remaining time is zero
  // (`metrics.rs`), so three stuck samples mean REST sends have been refused
  // for minutes while no deadline elapses to release them. A valid zero
  // sample proves the bodies drained and resolves; missing or malformed
  // samples break the streak but preserve a firing page, and a failed scrape
  // breaks it through interruptDispatchDrops. The paused/remaining gauges
  // stay out of this rule by design: a live deadline is healthy Discord
  // backpressure (pausing during a Discord cooldown is the guard working as
  // designed), and the remaining countdown derives from a monotonic deadline
  // that cannot wedge high. `?? 0` covers DO storage written before this
  // field existed.
  const REST_STUCK_RULE = "rest_global_responses_stuck";
  const wasStuckFiring = prev.firing.includes(REST_STUCK_RULE);
  const pendingValue = exactSeries(gauge("two_bot_rest_guard_pending_global_responses"), {});
  const prevStuckStreak = prev.restPendingStuckStreak ?? 0;
  let restPendingStuckStreak: number;
  if (pendingValue === undefined) {
    restPendingStuckStreak = 0;
    if (wasStuckFiring) firing.push(REST_STUCK_RULE);
  } else if (pendingValue === 0) {
    restPendingStuckStreak = 0;
  } else {
    restPendingStuckStreak = Math.min(prevStuckStreak + 1, REST_PENDING_STUCK_SAMPLES);
    if (wasStuckFiring || restPendingStuckStreak >= REST_PENDING_STUCK_SAMPLES) firing.push(REST_STUCK_RULE);
  }

  // Voice lifecycle failures (T3 room create/move/delete budget in
  // docs/voice-cutover-rollback-triggers.md): any outcome other than
  // `success` (`category_full`, `discord`, `persistence`, `cancelled`)
  // counts as a failure — the join did not place a room, or the move/delete
  // did not complete. `refused` is admission policy (a member at their cap or
  // burst limit, working as designed), so it is neither an operation nor a
  // failure here; it stays on the counter for the manual checkpoint. A new
  // dead-letter (a queue write that exhausted 10 attempts) or orphan (an
  // untracked creator-channel orphan needing manual deletion) fires on its
  // own, so a low-volume stranded-member failure still pages when the
  // operation window is too small for the ratio.
  let voiceOps = 0;
  let voiceFailures = 0;
  for (const s of gauge("two_bot_voice_operations_total")) {
    const outcome = s.labels["outcome"];
    if (outcome === "refused") continue;
    voiceOps += s.value;
    if (outcome !== "success") voiceFailures += s.value;
  }
  let voiceDeadLetters = 0;
  for (const s of gauge("two_bot_voice_dead_letters_total")) voiceDeadLetters += s.value;
  let voiceOrphans = 0;
  for (const s of gauge("two_bot_voice_orphans_total")) voiceOrphans += s.value;
  // `?? 0` covers DO storage written before these fields existed.
  const prevVoiceOps = prev.voiceOps ?? 0;
  const prevVoiceFailures = prev.voiceFailures ?? 0;
  const prevVoiceDeadLetters = prev.voiceDeadLetters ?? 0;
  const prevVoiceOrphans = prev.voiceOrphans ?? 0;
  // A counter that went backwards means the process restarted: no window.
  const voiceReset = voiceOps < prevVoiceOps || voiceFailures < prevVoiceFailures
    || voiceDeadLetters < prevVoiceDeadLetters || voiceOrphans < prevVoiceOrphans;
  const dVoiceOps = voiceOps - prevVoiceOps;
  const dVoiceFailures = voiceFailures - prevVoiceFailures;
  const dVoiceDead = voiceDeadLetters - prevVoiceDeadLetters;
  const dVoiceOrphans = voiceOrphans - prevVoiceOrphans;
  // Vote-kick enforcement failures extend the same rule: any increase in the
  // summed `discord_error`/`gave_up`/`permission_missing` outcomes of
  // `two_bot_voice_vote_kick_total` fires, so a vote-kick path that always
  // fails enforcement pages even while room operations look healthy. Every
  // other vote-kick outcome is never summed: refusals (e.g. `cooldown`) and
  // successful or already-terminal enforcements never fire. Each failure
  // series needs exactly one row carrying only its `outcome` label with a
  // safe nonnegative integer, so a Rust rename, a duplicate row, a malformed
  // value, or an extra (e.g. voter/target) label fails this arm instead of
  // silently holding it off. A missing family (older containers never
  // emitted it) or a malformed sample holds a vote-kick-caused firing alert
  // instead of resolving it, and never fires on its own; a backwards counter
  // (process restart) rebaselines, and a room-operation counter reset breaks
  // the hold and resolves like the window above. The first valid sample only
  // stores a baseline and never fires, so lifetime counts predating the
  // deploy do not page.
  let voiceVoteKickFailures: number | undefined;
  const voteKickRows = gauge("two_bot_voice_vote_kick_total");
  if (voteKickRows.length > 0) {
    let sum = 0;
    let valid = true;
    for (const outcome of VOICE_VOTE_KICK_FAILURE_OUTCOMES) {
      const value = exactSeries(voteKickRows, { outcome });
      if (value === undefined) { valid = false; break; }
      sum += value;
    }
    if (valid) voiceVoteKickFailures = sum;
  }
  // `??` covers DO storage written before these fields existed: the first
  // valid sample only stores a baseline and never fires (mirrors the
  // `gatewayMissedSeen`/`checkpointFailuresSeen`/`receiverRefusalsSeen` arms),
  // and the missing-family hold applies only to a vote-kick-caused alert so a
  // room-operation alert still resolves on a quiet window.
  const prevVoiceVoteKickFailures = prev.voiceVoteKickFailures ?? 0;
  const prevVoteKickSeen = prev.voiceVoteKickFailuresSeen ?? false;
  const prevVoteKickFiring = prev.voiceVoteKickFiring ?? false;
  const voteKickReset = voiceVoteKickFailures !== undefined && voiceVoteKickFailures < prevVoiceVoteKickFailures;
  const voteKickGrowth = voiceVoteKickFailures !== undefined && prevVoteKickSeen && !voteKickReset && voiceVoteKickFailures > prevVoiceVoteKickFailures;
  const voteKickHold = voiceVoteKickFailures === undefined && !voiceReset && prevVoteKickFiring;
  const voiceVoteKickFiring = voteKickGrowth || voteKickHold;
  if (!voiceReset && ((dVoiceOps >= VOICE_FAILURE_MIN_OPS && dVoiceFailures / dVoiceOps > VOICE_FAILURE_RATIO) || dVoiceDead >= 1 || dVoiceOrphans >= 1 || voteKickGrowth || voteKickHold)) {
    firing.push("voice_failures");
  }

  // Creator-join SLO burn rate (Google SRE workbook multiwindow): 99.5% of
  // genuine creator joins must move within 5 s. Good events are the
  // `two_bot_voice_join_to_move_seconds_bucket{le="5"}` cumulative count;
  // genuine joins are `two_bot_voice_joins_received_total` minus the benign
  // post-move `duplicate` refusals (`two_bot_voice_join_refused_total`).
  // Every other refusal, failed move and slow (>5 s) move is budget burn.
  // The fast rule pages when both the 1 h and 5 min windows burn faster
  // than 14.4x; the slow rule tickets when both the 6 h and 30 min windows
  // burn faster than 6x. A window with fewer genuine joins than
  // JOIN_BURN_MIN_JOINS never fires, so quiet guilds stay silent. A counter
  // that went backwards means the process restarted: the ring is rebuilt
  // from this sample and nothing fires. Missing or malformed join series
  // hold the previous firing state without appending a snapshot.
  const joinBurn = joinBurnEvaluation(samples, prev.joinBurn ?? [], nowSeconds);
  if (joinBurn.fast) firing.push("voice_join_fast_burn");
  else if (prev.firing.includes("voice_join_fast_burn") && joinBurn.held) firing.push("voice_join_fast_burn");
  if (joinBurn.slow) firing.push("voice_join_slow_burn");
  else if (prev.firing.includes("voice_join_slow_burn") && joinBurn.held) firing.push("voice_join_slow_burn");

  // Creator-map staleness: the voice actor re-merges the creator map from
  // the store every 5 minutes; `two_bot_voice_creator_map_age_seconds`
  // counts from the last successful merge (a failed read never resets it,
  // and the stall watchdog keeps it climbing while the actor is hung), so
  // an age past two intervals means merges stopped landing and joins are
  // served from a stale map. Generation 0 means no merge has landed yet —
  // boot, or voice parked — where the age is defined as 0 and this rule
  // cannot tell parked from wedged, so it stays silent. The gauges postdate
  // older containers and are optional scrape inputs: missing or malformed
  // series hold the previous firing state instead of resolving it.
  const creatorMapGeneration = exactSeries(gauge("two_bot_voice_creator_map_generation"), {});
  const creatorMapAge = exactSeries(gauge("two_bot_voice_creator_map_age_seconds"), {});
  if (creatorMapGeneration !== undefined && creatorMapAge !== undefined) {
    if (creatorMapGeneration > 0 && creatorMapAge > CREATOR_MAP_STALE_SECONDS) firing.push("voice_creator_map_stale");
  } else if (prev.firing.includes("voice_creator_map_stale")) {
    firing.push("voice_creator_map_stale");
  }

  // Voice worker halt latch: `two_bot_voice_worker_halted` reads 1 while any
  // guild actor is halted by a store CredentialRefused or Discord 401, so the
  // rule pages on a valid 1 and a valid 0 clears at once. Only store halts
  // auto-recover (the 60 s credential re-probe); a 401 needs a token fix and a
  // restart, so a latched 1 always names operator work. The latch is published
  // every actor tick from live state and a halt is never a transient blip, so
  // a single 1 reading pages without a streak. The gauge postdates older
  // containers and is an optional scrape input: missing, malformed or
  // non-boolean readings hold the previous firing state instead of resolving it.
  const WORKER_HALTED_RULE = "voice_worker_halted";
  const haltedRows = gauge("two_bot_voice_worker_halted");
  const halted = haltedRows.length === 1 ? exactSeries(haltedRows, {}) : undefined;
  if (halted === 1) {
    firing.push(WORKER_HALTED_RULE);
  } else if (halted !== 0 && prev.firing.includes(WORKER_HALTED_RULE)) {
    firing.push(WORKER_HALTED_RULE);
  }

  // Live-view self-heal repairs (one ticket per covered fault): any increase
  // of `two_bot_voice_live_self_heal_total` between two valid samples means a
  // live view was left not ready on a connected gateway and was republished
  // from the gateway cache. The first valid sample only stores the baseline
  // and never fires, so lifetime counts predating the deploy stay silent; a
  // counter that went backwards means the process restarted: no window. A
  // missing or malformed family (older containers never emitted it) is no
  // window either: it keeps the previous baseline, never fires on its own,
  // and holds a firing ticket instead of resolving it.
  // `??` covers DO storage written before these fields existed.
  const SELF_HEAL_RULE = "voice_live_self_heal";
  const selfHealRows = gauge("two_bot_voice_live_self_heal_total");
  const selfHeal = selfHealRows.length > 0 ? exactSeries(selfHealRows, {}) : undefined;
  const prevSelfHeal = prev.voiceLiveSelfHeal ?? 0;
  const selfHealSeen = prev.voiceLiveSelfHealSeen ?? false;
  let voiceLiveSelfHeal = prevSelfHeal;
  let voiceLiveSelfHealSeen = selfHealSeen;
  if (selfHeal !== undefined) {
    voiceLiveSelfHeal = selfHeal;
    voiceLiveSelfHealSeen = true;
    if (selfHealSeen && selfHeal > prevSelfHeal) firing.push(SELF_HEAL_RULE);
  } else if (prev.firing.includes(SELF_HEAL_RULE)) {
    firing.push(SELF_HEAL_RULE);
  }

  // Live-view not-ready age: `two_bot_voice_live_not_ready_seconds` is the
  // maximum seconds a guild's live view has been not ready while the gateway
  // is connected, republished every actor tick. While above 0 that guild's
  // creator joins are dropped without an accept or refusal line; past
  // VOICE_LIVE_NOT_READY_THRESHOLD_SECONDS the runbook calls for a restart
  // and a bug report. The age itself is the sustainment: a single valid
  // reading past the bound proves that many seconds of continuous not-ready,
  // so the rule pages without a streak and a valid reading at or below the
  // bound clears at once. The gauge is an optional scrape input, so older
  // containers hold the previous firing state instead of resolving it. A hung
  // dispatch does not freeze the published age: the watchdog sibling
  // reads the shared live view directly, so a hang still pages past the
  // bound.
  const NOT_READY_RULE = "voice_live_not_ready";
  const notReady = exactSeries(gauge("two_bot_voice_live_not_ready_seconds"), {});
  if (notReady !== undefined) {
    if (notReady > VOICE_LIVE_NOT_READY_THRESHOLD_SECONDS) firing.push(NOT_READY_RULE);
  } else if (prev.firing.includes(NOT_READY_RULE)) {
    firing.push(NOT_READY_RULE);
  }

  // Queue stall: `two_bot_voice_queue_oldest_age_seconds` is the maximum age
  // of the oldest queued (not in-flight) voice action across all current
  // guild actors. Past VOICE_QUEUE_STALL_THRESHOLD_SECONDS a guild's queue
  // is draining unacceptably slowly. Ticket, not page: a slow-but-healthy
  // drain can legitimately age past the bound under burst load, and
  // member-visible join degradation already pages through the join-budget
  // burn rules. A valid reading at or below the bound clears at once; the
  // gauge is an optional scrape input, so older containers hold the previous
  // firing state. A hung dispatch does not freeze publication: the watchdog
  // sibling recomputes the age from the snapshot's first-enqueue stamp on
  // its own clock, so a hang that starts below the bound still tickets past
  // it.
  // Queue depth (`two_bot_voice_queue_pending{lane}`) is deliberately
  // unconsumed: depth without age is not a failure signal (see the catalog).
  const QUEUE_STALL_RULE = "voice_queue_stall";
  const queueOldest = exactSeries(gauge("two_bot_voice_queue_oldest_age_seconds"), {});
  if (queueOldest !== undefined) {
    if (queueOldest > VOICE_QUEUE_STALL_THRESHOLD_SECONDS) firing.push(QUEUE_STALL_RULE);
  } else if (prev.firing.includes(QUEUE_STALL_RULE)) {
    firing.push(QUEUE_STALL_RULE);
  }

  // Gateway missed events (sequence gaps inside one session): any increase
  // between two samples fails the zero-missed-events acceptance. The first
  // sample only stores the baseline and never fires; a counter that went
  // backwards means the process restarted: no window.
  // `??` covers DO storage written before these fields existed.
  let gatewayMissed = 0;
  for (const s of gauge("two_bot_gateway_missed_events_total")) gatewayMissed += s.value;
  const prevGatewayMissed = prev.gatewayMissed ?? 0;
  const gatewaySeen = prev.gatewayMissedSeen ?? false;
  const gatewayReset = gatewayMissed < prevGatewayMissed;
  if (gatewaySeen && !gatewayReset && gatewayMissed > prevGatewayMissed) firing.push("gateway_missed_events");

  const unpaired = pairGatewayDisconnects(samples, prev);
  if (unpaired.firing) firing.push(GATEWAY_UNPAIRED_RULE);

  // Gateway stuck not-ready with no disconnect edge: the scraped /readyz
  // gateway component stayed non-ready (`starting` while Armed, `down` while
  // Unconfigured or Draining in crates/bot/src/gateway.rs) for
  // GATEWAY_NOT_READY_SAMPLES consecutive valid samples. The component
  // breakdown lives only in the /readyz JSON body
  // (crates/core/src/health.rs), never in the /metrics exposition, so the
  // keepalive passes the parsed component in as `gatewayReady` instead of a
  // Sample series: true is Connected, false is starting/down, undefined is an
  // invalid sample (probe failed or body unparsable) that changes nothing and
  // holds the current state. `gateway_failure.failed_step` cannot serve here:
  // it is absent until a failure is recorded, so a shard stuck starting with
  // no recorded failure has no step to read. The first valid sample only
  // stores the baseline and never fires; a process restart (gateway counter
  // decrease or validated incarnation change above) re-baselines the streak even
  // when this sample's readiness is invalid, and holds a firing non-ready or
  // invalid alert rather than clearing it; any ready sample clears at once,
  // even across a restart. A failed /metrics scrape never reaches this
  // evaluation: the caller
  // preserves the whole state, so the rule also needs consecutive ticks where
  // both the probe parsed and the scrape succeeded.
  const GATEWAY_NOT_READY_RULE = "gateway_not_ready";
  const notReadyWasFiring = prev.firing.includes(GATEWAY_NOT_READY_RULE);
  const notReadySeen = prev.gatewayNotReadySeen ?? false;
  const notReadyStreakPrev = prev.gatewayNotReadyStreak ?? 0;
  let gatewayNotReadySeen = notReadySeen;
  let gatewayNotReadyStreak = notReadyStreakPrev;
  let gatewayNotReadyFiring = notReadyWasFiring;
  if (gatewayReady !== undefined && !notReadySeen) {
    gatewayNotReadySeen = true;
    gatewayNotReadyStreak = gatewayReady ? 0 : 1;
    gatewayNotReadyFiring = notReadyWasFiring;
  } else if (gatewayReady === true) {
    // A valid ready sample clears at once, even across a restart: a restart
    // that lands on ready means the new process came up connected.
    gatewayNotReadyStreak = 0;
    gatewayNotReadyFiring = false;
  } else if (unpaired.restarted && notReadySeen) {
    // A process restart re-baselines the streak even when this sample's
    // readiness is invalid: the pre-restart streak is stale, so the next valid
    // non-ready sample recounts from 1 instead of firing off it. Firing still
    // holds for non-ready or invalid samples (a restart never resolves those),
    // matching the sibling unpaired rule.
    gatewayNotReadyStreak = gatewayReady === false ? 1 : 0;
    gatewayNotReadyFiring = notReadyWasFiring;
  } else if (gatewayReady === false) {
    gatewayNotReadyStreak = notReadyStreakPrev + 1;
    gatewayNotReadyFiring = gatewayNotReadyStreak >= GATEWAY_NOT_READY_SAMPLES || notReadyWasFiring;
  }
  if (gatewayNotReadyFiring) firing.push(GATEWAY_NOT_READY_RULE);

  // Gateway checkpoint commit failures (every failure stops the dispatch
  // worker, so there is no benign singleton): any increase between two
  // samples pages. The first sample only stores the baseline and never
  // fires. A runtime failure lingers 75 s (`shutdown::RUNTIME_FAILURE_LINGER`,
  // pinned against the keepalive tick plus probe timeouts in
  // checkpoint-linger.test.ts) past one full keepalive tick, so the increase
  // is normally scraped before the exit resets the counter; the post-restart
  // reset sample resolves rather than firing. `??` covers DO storage written
  // before these fields existed.
  let checkpointFailures = 0;
  for (const s of gauge("two_bot_gateway_checkpoint_failures_total")) checkpointFailures += s.value;
  const prevCheckpointFailures = prev.checkpointFailures ?? 0;
  const checkpointSeen = prev.checkpointFailuresSeen ?? false;
  const checkpointReset = checkpointFailures < prevCheckpointFailures;
  if (checkpointSeen && !checkpointReset && checkpointFailures > prevCheckpointFailures) firing.push("gateway_checkpoint_failures");

  // Website-action receiver refusals by family: refused
  // `two_bot_internal_actions_total` outcomes (every outcome other than
  // `executed`) must rise in RECEIVER_REFUSAL_SAMPLES consecutive windows
  // before a family pages, so a receiver-abuse or refusal storm names its
  // family while one forged pre-auth probe (always family `other`) stays
  // silent. The first sample only stores the baseline and never fires; a
  // per-family counter that went backwards means the process restarted and
  // clears that family's streak. `??` covers DO storage written before
  // these fields existed.
  const receiverRefusals: Record<string, number> = {};
  for (const s of gauge("two_bot_internal_actions_total")) {
    if (s.labels["outcome"] !== "executed") {
      const family = s.labels["family"] ?? "other";
      receiverRefusals[family] = (receiverRefusals[family] ?? 0) + s.value;
    }
  }
  const prevReceiverRefusals = prev.receiverRefusals ?? {};
  const receiverSeen = prev.receiverRefusalsSeen ?? false;
  const prevReceiverStreaks = prev.receiverRefusalStreaks ?? {};
  const receiverRefusalStreaks: Record<string, number> = {};
  if (receiverSeen) {
    for (const [family, count] of Object.entries(receiverRefusals)) {
      const prevCount = prevReceiverRefusals[family] ?? 0;
      // A counter that went backwards means the process restarted: no window.
      // Sustained refusal, not one probe: only windows with new refusals
      // extend the streak, so idle windows and restarts clear it.
      const streak = count < prevCount || count === prevCount ? 0 : (prevReceiverStreaks[family] ?? 0) + 1;
      receiverRefusalStreaks[family] = streak;
      if (streak >= RECEIVER_REFUSAL_SAMPLES) firing.push(`receiver_refusals:${family}`);
    }
  } else {
    for (const family of Object.keys(receiverRefusals)) receiverRefusalStreaks[family] = 0;
  }

  // EventSub tampered-signature surge: `two_bot_eventsub_classified_total`
  // with exactly `kind="tampered"` (only `BadSignature`) must rise in
  // EVENTSUB_TAMPERED_SAMPLES consecutive windows before paging, so an active
  // forgery attempt or a broken secret rotation pages while one forged probe
  // stays quiet. Only the fixed kind label counts: any extra label, any other
  // kind, a duplicated row, or a non-integer value invalidates the sample.
  // The first valid sample only stores the baseline and never fires; a
  // backwards counter means the process restarted and rebaselines without
  // firing; missing/invalid scrapes preserve a firing alert and never
  // manufacture recovery; a flat valid window resolves. `??` covers DO
  // storage written before these fields existed.
  const TAMPERED_RULE = "eventsub_tampered";
  const tamperedValue = exactSeries(gauge("two_bot_eventsub_classified_total"), { kind: "tampered" });
  const prevTampered = prev.eventsubTampered ?? 0;
  const tamperedSeen = prev.eventsubTamperedSeen ?? false;
  const prevTamperedStreak = prev.eventsubTamperedStreak ?? 0;
  const wasTamperedFiring = prev.firing.includes(TAMPERED_RULE);
  let eventsubTampered = prevTampered;
  let eventsubTamperedSeen = tamperedSeen;
  let eventsubTamperedStreak = prevTamperedStreak;
  if (tamperedValue === undefined) {
    if (wasTamperedFiring) firing.push(TAMPERED_RULE);
  } else if (!tamperedSeen) {
    eventsubTampered = tamperedValue;
    eventsubTamperedSeen = true;
    eventsubTamperedStreak = 0;
  } else if (tamperedValue < prevTampered) {
    eventsubTampered = tamperedValue;
    eventsubTamperedSeen = true;
    eventsubTamperedStreak = 0;
  } else if (tamperedValue > prevTampered) {
    const streak = prevTamperedStreak + 1;
    eventsubTampered = tamperedValue;
    eventsubTamperedSeen = true;
    eventsubTamperedStreak = streak;
    if (streak >= EVENTSUB_TAMPERED_SAMPLES) firing.push(TAMPERED_RULE);
  } else {
    eventsubTampered = tamperedValue;
    eventsubTamperedSeen = true;
    eventsubTamperedStreak = 0;
  }

  // Dispatch saturation is a ticket, not evidence of gateway sequence loss.
  // Iterate only the fixed lanes: neither storage nor firing keys can acquire
  // member/channel labels from a malformed exposition. Each lane needs one
  // nonnegative, safely represented integer with only its lane label.
  const dispatchDrops: Record<string, number> = {};
  const dispatchDropStreaks: Record<string, number> = {};
  const dropSamples = gauge("two_bot_dispatch_drops_total");
  for (const lane of DISPATCH_LANES) {
    const rows = dropSamples.filter((s) => s.labels["lane"] === lane);
    const sample = rows[0];
    if (rows.length === 1 && sample && Object.keys(sample.labels).length === 1
      && Number.isSafeInteger(sample.value) && sample.value >= 0) {
      dispatchDrops[lane] = sample.value;
    }
  }
  // All lanes belong to one process. A reset in any observed lane invalidates
  // the whole window, including lanes whose new count overtook their old one.
  const dispatchReset = DISPATCH_LANES.some((lane) => {
    const count = dispatchDrops[lane];
    const previous = prev.dispatchDrops?.[lane];
    return count !== undefined && previous !== undefined && count < previous;
  });
  for (const lane of DISPATCH_LANES) {
    const key = `dispatch_drops:${lane}`;
    const wasFiring = prev.firing.includes(key);
    const count = dispatchDrops[lane];
    const previous = prev.dispatchDrops?.[lane];
    if (count === undefined) {
      // Missing/invalid data breaks consecutive growth and drops the baseline,
      // but does not provide recovery evidence for a currently firing lane.
      if (wasFiring) firing.push(key);
      continue;
    }
    if (dispatchReset) {
      dispatchDropStreaks[lane] = 0;
      continue;
    }
    if (previous === undefined) {
      dispatchDropStreaks[lane] = 0;
      // The first valid sample (including after a gap) is only a baseline.
      if (wasFiring) firing.push(key);
      continue;
    }
    // Flat samples resolve; positive deltas after a gap retain an already
    // firing ticket until a valid quiet window.
    const grew = count > previous;
    const streak = grew ? Math.min((prev.dispatchDropStreaks?.[lane] ?? 0) + 1, DISPATCH_DROP_SAMPLES) : 0;
    dispatchDropStreaks[lane] = streak;
    if (grew && (wasFiring || streak >= DISPATCH_DROP_SAMPLES)) firing.push(key);
  }

  // Sustained community-facts drain loss is a ticket. Only the fixed reasons
  // count: unknown/hostile reason rows never become subjects or stored state.
  // Growth is summed over every fixed reason, so a large single burst stays
  // silent until three consecutive completed windows each add failures. A flat
  // or reset window is the valid recovery; missing/invalid evidence breaks
  // growth but preserves an active ticket.
  const COMMUNITY_FACTS_DRAIN_KEY = "community_facts_drain_failures";
  const drainSamples = gauge("two_bot_community_facts_drain_failures_total");
  const drainCounts: Record<string, number> = {};
  let drainValid = true;
  for (const reason of COMMUNITY_FACTS_DRAIN_REASONS) {
    const rows = drainSamples.filter((s) => s.labels["reason"] === reason);
    const sample = rows[0];
    if (rows.length !== 1 || !sample || Object.keys(sample.labels).length !== 1
      || !Number.isSafeInteger(sample.value) || sample.value < 0) {
      drainValid = false;
      break;
    }
    drainCounts[reason] = sample.value;
  }
  const wasDrainFiring = prev.firing.includes(COMMUNITY_FACTS_DRAIN_KEY);
  let communityFactsDrainFailures: Record<string, number> = {};
  let communityFactsDrainStreak = 0;
  if (!drainValid) {
    // Missing/invalid evidence drops the baseline, never the active ticket.
    if (wasDrainFiring) firing.push(COMMUNITY_FACTS_DRAIN_KEY);
  } else {
    const prevDrain = prev.communityFactsDrainFailures;
    const hasBaseline = prevDrain !== undefined
      && COMMUNITY_FACTS_DRAIN_REASONS.every((reason) => {
        const count = prevDrain[reason];
        return typeof count === "number" && Number.isSafeInteger(count) && count >= 0;
      });
    if (!hasBaseline) {
      // The first valid sample (including after a gap or from legacy state)
      // is only a baseline; it never resolves an active ticket on its own.
      communityFactsDrainFailures = { ...drainCounts };
      if (wasDrainFiring) firing.push(COMMUNITY_FACTS_DRAIN_KEY);
    } else {
      // Any backwards reason means the process restarted: re-baseline with a
      // clear streak, even when another reason's new count overtook its old
      // one. A reset window is valid recovery evidence, so it resolves.
      const reset = COMMUNITY_FACTS_DRAIN_REASONS.some((reason) => drainCounts[reason]! < (prevDrain[reason] ?? 0));
      communityFactsDrainFailures = { ...drainCounts };
      if (reset) {
        communityFactsDrainStreak = 0;
      } else {
        let delta = 0;
        for (const reason of COMMUNITY_FACTS_DRAIN_REASONS) delta += drainCounts[reason]! - (prevDrain[reason] ?? 0);
        if (delta <= 0) {
          communityFactsDrainStreak = 0;
        } else {
          communityFactsDrainStreak = Math.min((prev.communityFactsDrainStreak ?? 0) + 1, COMMUNITY_FACTS_DRAIN_SAMPLES);
          if (wasDrainFiring || communityFactsDrainStreak >= COMMUNITY_FACTS_DRAIN_SAMPLES) firing.push(COMMUNITY_FACTS_DRAIN_KEY);
        }
      }
    }
  }

  // Event-store consumer lag is a ticket, not a page. The label-free duration
  // gauge reads 0 when unobserved; only sustained samples above
  // EVENT_STORE_LAG_THRESHOLD_SECONDS (fractional counts) across
  // EVENT_STORE_LAG_SAMPLES consecutive valid samples fire. A valid sample
  // at or below the threshold resets the streak and resolves; missing/invalid
  // samples (absent, duplicate, labeled, non-finite, negative) break the
  // streak but preserve a firing ticket, and never let label values reach
  // state or keys. A process restart reads 0, so restarts rebaseline the
  // same way. Failed scrapes reset the streak through interruptDispatchDrops.
  const EVENT_STORE_LAG_RULE = "event_store_consumer_lag";
  const wasLagFiring = prev.firing.includes(EVENT_STORE_LAG_RULE);
  const lagRows = gauge("two_bot_event_store_consumer_lag_seconds");
  let lagValue: number | undefined;
  if (lagRows.length === 1) {
    const row = lagRows[0]!;
    if (Object.keys(row.labels).length === 0 && Number.isFinite(row.value) && row.value >= 0) {
      lagValue = row.value;
    }
  }
  const prevLagStreak = prev.eventStoreLagStreak ?? 0;
  let eventStoreLagStreak: number;
  if (lagValue === undefined) {
    eventStoreLagStreak = 0;
    if (wasLagFiring) firing.push(EVENT_STORE_LAG_RULE);
  } else if (lagValue <= EVENT_STORE_LAG_THRESHOLD_SECONDS) {
    eventStoreLagStreak = 0;
  } else {
    eventStoreLagStreak = Math.min(prevLagStreak + 1, EVENT_STORE_LAG_SAMPLES);
    if (wasLagFiring || eventStoreLagStreak >= EVENT_STORE_LAG_SAMPLES) firing.push(EVENT_STORE_LAG_RULE);
  }

  // Failed EventSub reconciliation passes (one ticket per failed pass). The
  // Rust process records exactly one outcome per completed pass that reaches
  // Helix (`two_bot_eventsub_reconcile_passes_total{outcome}` with the fixed
  // `healthy`/`pending`/`failed` outcomes); parked or refused startup passes
  // never reach Helix and record no sample. Only the pass outcome is an alert
  // input here: the detailed per-site error counters stay exposition-only so
  // one failed pass raises one ticket however many error sites moved (the
  // prior-failed rule below only fires on windows where no failed pass grew,
  // so it never multiplies this ticket). The first
  // valid sample only stores a baseline. A later positive failed-pass delta
  // fires (even beside a healthy delta); healthy-only or pending-only growth
  // never fires. A flat failed counter never recovers: only a healthy-pass
  // increase with no failed-pass increase resolves, once. Any backwards
  // counter means the process restarted: that sample re-baselines, firing
  // when the new process has already counted a failed pass (the startup pass
  // runs before the first scrape, so a reset sample with failed > 0 is a
  // post-restart failure, not a fabricated delta) and otherwise holding a
  // firing ticket instead of fabricating a RESOLVED. Missing or
  // invalid samples (absent family on older containers, malformed values,
  // extra labels, duplicates) hold the firing state and keep the previous
  // baseline; `??` covers DO storage written before this rule existed.
  const RECONCILE_FAILED_RULE = "eventsub_reconcile_failed";
  const wasReconcileFiring = prev.firing.includes(RECONCILE_FAILED_RULE);
  const passSamples = gauge("two_bot_eventsub_reconcile_passes_total");
  const reconcileHealthyNow = exactSeries(passSamples, { outcome: "healthy" });
  const reconcilePendingNow = exactSeries(passSamples, { outcome: "pending" });
  const reconcileFailedNow = exactSeries(passSamples, { outcome: "failed" });
  let reconcileHealthy = prev.reconcileHealthy ?? 0;
  let reconcilePending = prev.reconcilePending ?? 0;
  let reconcileFailed = prev.reconcileFailed ?? 0;
  let reconcileSeen = prev.reconcileSeen ?? false;
  if (reconcileHealthyNow === undefined || reconcilePendingNow === undefined || reconcileFailedNow === undefined) {
    if (wasReconcileFiring) firing.push(RECONCILE_FAILED_RULE);
  } else if (!reconcileSeen) {
    reconcileHealthy = reconcileHealthyNow;
    reconcilePending = reconcilePendingNow;
    reconcileFailed = reconcileFailedNow;
    reconcileSeen = true;
  } else if (
    reconcileHealthyNow < reconcileHealthy
    || reconcilePendingNow < reconcilePending
    || reconcileFailedNow < reconcileFailed
  ) {
    reconcileHealthy = reconcileHealthyNow;
    reconcilePending = reconcilePendingNow;
    reconcileFailed = reconcileFailedNow;
    reconcileSeen = true;
    if (reconcileFailedNow > 0 || wasReconcileFiring) firing.push(RECONCILE_FAILED_RULE);
  } else {
    const failedGrew = reconcileFailedNow > reconcileFailed;
    const healthyGrew = reconcileHealthyNow > reconcileHealthy;
    reconcileHealthy = reconcileHealthyNow;
    reconcilePending = reconcilePendingNow;
    reconcileFailed = reconcileFailedNow;
    reconcileSeen = true;
    if (failedGrew) firing.push(RECONCILE_FAILED_RULE);
    else if (!healthyGrew && wasReconcileFiring) firing.push(RECONCILE_FAILED_RULE);
  }

  // Retention purge skips needing Operator replay (one ticket per skipped
  // purge): any increase of `two_bot_retention_replay_needed_total` between
  // two valid samples means a real guild removal's purge-queue write hit the
  // schema gap and the removed guild's rows linger until the Operator replays
  // the insert (`docs/privacy.md#retention-and-explicit-exceptions`). Ticket,
  // not page: no member impact and no data loss, but every skip is operator
  // work with a bounded procedure. The first valid sample only stores the
  // baseline and never fires, so lifetime counts predating the deploy stay
  // silent; a counter that went backwards means the process restarted and
  // re-baselines without firing; a missing or malformed family (older
  // containers never emitted it) is no window: it keeps the previous baseline
  // and never fires on its own. `??` covers DO storage written before these
  // fields existed.
  const RETENTION_REPLAY_RULE = "retention_replay_needed";
  const retentionNow = exactSeries(gauge("two_bot_retention_replay_needed_total"), {});
  const prevRetention = prev.retentionReplayNeeded ?? 0;
  const retentionSeen = prev.retentionReplayNeededSeen ?? false;
  let retentionReplayNeeded = prevRetention;
  let retentionReplayNeededSeen = retentionSeen;
  if (retentionNow !== undefined) {
    retentionReplayNeeded = retentionNow;
    retentionReplayNeededSeen = true;
    if (retentionSeen && retentionNow > prevRetention) firing.push(RETENTION_REPLAY_RULE);
  }

  // Auxiliary XP skipped on a missing table/grant: ticket on growth, like
  // retention. First sample/reset baselines; missing or invalid is no window.
  const drainSkipsNow = exactSeries(gauge("two_bot_gateway_drain_skips_total"), {});
  const prevDrainSkips = prev.gatewayDrainSkips ?? 0;
  const drainSkipsSeen = prev.gatewayDrainSkipsSeen ?? false;
  let gatewayDrainSkips = prevDrainSkips;
  let gatewayDrainSkipsSeen = drainSkipsSeen;
  if (drainSkipsNow !== undefined) {
    gatewayDrainSkips = drainSkipsNow;
    gatewayDrainSkipsSeen = true;
    if (drainSkipsSeen && drainSkipsNow > prevDrainSkips) firing.push("gateway_drain_skips");
  }

  // Voice dispatch turns past the observe bound (one ticket per slow turn):
  // any increase of `two_bot_voice_dispatch_timeouts_total` between two valid
  // samples means a dispatch turn ran past the 60 s observe bound. The turn
  // ran to completion and the queue keeps its retries, so one overrun is
  // slowness, not loss — ticket, never page — but an over-budget turn means a
  // constituent await has no cap, and repeats mean overload. The warn log line
  // (`voice dispatch turn exceeded observe bound`) carries the guild and
  // elapsed seconds. Baseline, restart and gap semantics mirror the retention
  // rule above: first valid sample baselines, restarts re-baseline, missing
  // or malformed families are no window.
  const DISPATCH_TIMEOUTS_RULE = "voice_dispatch_timeouts";
  const dispatchTimeoutsNow = exactSeries(gauge("two_bot_voice_dispatch_timeouts_total"), {});
  const prevDispatchTimeouts = prev.voiceDispatchTimeouts ?? 0;
  const dispatchTimeoutsSeen = prev.voiceDispatchTimeoutsSeen ?? false;
  let voiceDispatchTimeouts = prevDispatchTimeouts;
  let voiceDispatchTimeoutsSeen = dispatchTimeoutsSeen;
  if (dispatchTimeoutsNow !== undefined) {
    voiceDispatchTimeouts = dispatchTimeoutsNow;
    voiceDispatchTimeoutsSeen = true;
    if (dispatchTimeoutsSeen && dispatchTimeoutsNow > prevDispatchTimeouts) firing.push(DISPATCH_TIMEOUTS_RULE);
  }

  // Voice watchdog stuck windows (one ticket per stuck window): any increase
  // of `two_bot_voice_watchdog_stucks_total` between two valid samples means
  // queued actions saw no completed dispatch turn for the full 120 s window.
  // Ticket, not page: one stuck window can self-clear on the next window, and
  // member-visible join degradation already pages through the join-budget
  // burn rules. The error log line (`voice worker stuck: queued actions with
  // no progress`, at most one per window) names the stall. Baseline, restart
  // and gap semantics mirror the retention rule above.
  const WATCHDOG_STUCKS_RULE = "voice_watchdog_stucks";
  const watchdogStucksNow = exactSeries(gauge("two_bot_voice_watchdog_stucks_total"), {});
  const prevWatchdogStucks = prev.voiceWatchdogStucks ?? 0;
  const watchdogStucksSeen = prev.voiceWatchdogStucksSeen ?? false;
  let voiceWatchdogStucks = prevWatchdogStucks;
  let voiceWatchdogStucksSeen = watchdogStucksSeen;
  if (watchdogStucksNow !== undefined) {
    voiceWatchdogStucks = watchdogStucksNow;
    voiceWatchdogStucksSeen = true;
    if (watchdogStucksSeen && watchdogStucksNow > prevWatchdogStucks) firing.push(WATCHDOG_STUCKS_RULE);
  }

  // Voice halt-latch recoveries (one ticket per covered halt): any increase of
  // `two_bot_voice_halt_recoveries_total` between two valid samples means a
  // store-CredentialRefused halt cleared via the 60 s credential re-probe
  // (Discord-401 halts never auto-recover, so this counter never counts a
  // token recovery). Ticket, not page: the fault already passed, but the halt
  // refused member joins while latched and the store refusal is worth a look
  // — including brief halt/recover cycles the sampled halt latch never shows.
  // The info log line (`voice worker halt cleared: store credentials
  // healthy`) marks each recovery. Baseline, restart and gap semantics mirror
  // the retention rule above. Breaker closes stay deliberately unconsumed (see
  // the catalog): a close cools an episode the opens ticket already named.
  const HALT_RECOVERIES_RULE = "voice_halt_recoveries";
  const haltRecoveriesNow = exactSeries(gauge("two_bot_voice_halt_recoveries_total"), {});
  const prevHaltRecoveries = prev.voiceHaltRecoveries ?? 0;
  const haltRecoveriesSeen = prev.voiceHaltRecoveriesSeen ?? false;
  let voiceHaltRecoveries = prevHaltRecoveries;
  let voiceHaltRecoveriesSeen = haltRecoveriesSeen;
  if (haltRecoveriesNow !== undefined) {
    voiceHaltRecoveries = haltRecoveriesNow;
    voiceHaltRecoveriesSeen = true;
    if (haltRecoveriesSeen && haltRecoveriesNow > prevHaltRecoveries) firing.push(HALT_RECOVERIES_RULE);
  }

  // Invalid-request breaker opens (one ticket per trip): any increase of
  // `two_bot_rest_guard_breaker_opens_total` between two valid samples means
  // the 600 s invalid-request window reached threshold and the guard started
  // refusing non-essential REST sends locally. Ticket, like its sustained-open
  // sibling: a trip on a bad deploy is routine, and the breaker closes on its
  // own when the window cools (closes are resolution evidence, not a rule).
  // The warn log line (`discord_breaker_open`) carries the invalid count and
  // threshold. Baseline, restart and gap semantics mirror the retention rule
  // above.
  const BREAKER_OPENS_RULE = "rest_breaker_opens";
  const breakerOpensNow = exactSeries(gauge("two_bot_rest_guard_breaker_opens_total"), {});
  const prevBreakerOpens = prev.restBreakerOpens ?? 0;
  const breakerOpensSeen = prev.restBreakerOpensSeen ?? false;
  let restBreakerOpens = prevBreakerOpens;
  let restBreakerOpensSeen = breakerOpensSeen;
  if (breakerOpensNow !== undefined) {
    restBreakerOpens = breakerOpensNow;
    restBreakerOpensSeen = true;
    if (breakerOpensSeen && breakerOpensNow > prevBreakerOpens) firing.push(BREAKER_OPENS_RULE);
  }

  // Discord global-pause episodes (one ticket per new episode): any increase
  // of `two_bot_rest_guard_global_pauses_total` between two valid samples
  // means a new global REST pause began (extensions and overlapping pending
  // responses never increment it). Ticket, never page: the pause self-clears
  // at its deadline and no operator action shortens Discord-side backpressure,
  // but a storm of episodes points at our own request bug. The warn log line
  // (`discord_global_pause`) carries the wait. Baseline, restart and gap
  // semantics mirror the retention rule above.
  const GLOBAL_PAUSES_RULE = "rest_global_pauses";
  const globalPausesNow = exactSeries(gauge("two_bot_rest_guard_global_pauses_total"), {});
  const prevGlobalPauses = prev.restGlobalPauses ?? 0;
  const globalPausesSeen = prev.restGlobalPausesSeen ?? false;
  let restGlobalPauses = prevGlobalPauses;
  let restGlobalPausesSeen = globalPausesSeen;
  if (globalPausesNow !== undefined) {
    restGlobalPauses = globalPausesNow;
    restGlobalPausesSeen = true;
    if (globalPausesSeen && globalPausesNow > prevGlobalPauses) firing.push(GLOBAL_PAUSES_RULE);
  }

  // Dead gateway (no heartbeat-ACK evidence): the label-free latency gauge
  // reads NaN until the first ACK lands on the current connection
  // (metrics.rs resets it to None on every reconnect), so NaN or a finite
  // value unchanged since the previous valid sample both mean no fresh ACK
  // arrived this window. Healthy connections ACK every heartbeat interval
  // (~41 s), well inside the ~60 s scrape cadence, so three consecutive
  // evidenceless samples (~2-3 min, the sibling gateway bound) page. Any
  // fresh evidence (a finite change, or a finite value after unknown)
  // clears at once. A clean boot (Connected within ~1 min, first ACK within
  // another interval) never reaches the third sample; a boot or outage
  // already past three samples pages here alongside gateway_not_ready,
  // which owns the never-connected case. A missing row holds like a
  // malformed one (duplicated, labeled, negative, infinite): missing input
  // never manufactures a page, matching every sibling rule, and a missing
  // required series in a real scrape fails the whole scrape anyway (the
  // metrics_scrape_lost path). The first valid sample only stores the
  // baseline and never fires. A validated incarnation change (process
  // restart, `unpaired.restarted` from the #1409 fence) re-baselines like
  // the first sample and holds a firing page: the new process has no
  // same-process history to judge.
  const GATEWAY_LATENCY_DEAD_RULE = "gateway_latency_dead";
  const latencyWasFiring = prev.firing.includes(GATEWAY_LATENCY_DEAD_RULE);
  const latencyRows = gauge("two_bot_gateway_latency_seconds");
  let latencyReading: number | "unknown" | "invalid";
  if (latencyRows.length === 0) {
    latencyReading = "invalid";
  } else if (latencyRows.length !== 1 || Object.keys(latencyRows[0]!.labels).length !== 0) {
    latencyReading = "invalid";
  } else {
    const latencyValue = latencyRows[0]!.value;
    latencyReading = Number.isNaN(latencyValue) ? "unknown"
      : (Number.isFinite(latencyValue) && latencyValue >= 0 ? latencyValue : "invalid");
  }
  const prevLatencySeen = prev.gatewayLatencySeen ?? false;
  const prevLatencyLast = prev.gatewayLatencyLast;
  const prevLatencyStreak = prev.gatewayLatencyDeadStreak ?? 0;
  let gatewayLatencyLast = prevLatencyLast;
  let gatewayLatencyDeadStreak = prevLatencyStreak;
  let gatewayLatencySeen = prevLatencySeen;
  if (latencyReading === "invalid") {
    if (latencyWasFiring) firing.push(GATEWAY_LATENCY_DEAD_RULE);
  } else if (!prevLatencySeen || unpaired.restarted) {
    // The first valid sample (including after a gap or a validated process
    // restart) is only a baseline. A new baseline is not a quiet window,
    // so a held page stays held.
    gatewayLatencySeen = true;
    if (latencyReading === "unknown") {
      gatewayLatencyDeadStreak = 1;
      gatewayLatencyLast = undefined;
    } else {
      gatewayLatencyDeadStreak = 0;
      gatewayLatencyLast = latencyReading;
    }
    if (latencyWasFiring) firing.push(GATEWAY_LATENCY_DEAD_RULE);
  } else if (latencyReading === "unknown" || (prevLatencyLast !== undefined && latencyReading === prevLatencyLast)) {
    // Unknown clears the last finite value: the next finite reading is
    // always fresh evidence, even when its micros repeat the stale one.
    if (latencyReading === "unknown") gatewayLatencyLast = undefined;
    gatewayLatencyDeadStreak = Math.min(prevLatencyStreak + 1, GATEWAY_LATENCY_DEAD_SAMPLES);
    if (latencyWasFiring || gatewayLatencyDeadStreak >= GATEWAY_LATENCY_DEAD_SAMPLES) firing.push(GATEWAY_LATENCY_DEAD_RULE);
  } else {
    gatewayLatencyDeadStreak = 0;
    gatewayLatencyLast = latencyReading;
  }

  // Gateway reconnect flapping: `two_bot_gateway_reconnects_total` counts
  // successful transport reconnections (new HELLOs after the first), so one
  // blip is a clean recovery and only a sustained rise means the session
  // cannot stay up. Three consecutive rising windows page (the sibling
  // gateway bound); a flat window resolves, a backwards counter (process
  // restart) rebaselines, the first valid sample only stores the baseline,
  // and missing/invalid samples hold a firing page without manufacturing
  // evidence. A validated incarnation change (`unpaired.restarted`) also
  // re-baselines like the first sample and holds: a new process whose first
  // counter already overtook the old baseline is not a same-process rise.
  // Any sequence gap the flapping causes still pages immediately
  // via gateway_missed_events.
  const GATEWAY_RECONNECT_FLAP_RULE = "gateway_reconnect_flapping";
  const flapValue = exactSeries(gauge("two_bot_gateway_reconnects_total"), {});
  const prevFlap = prev.gatewayReconnects ?? 0;
  const flapSeen = prev.gatewayReconnectsSeen ?? false;
  const prevFlapStreak = prev.gatewayReconnectStreak ?? 0;
  const wasFlapFiring = prev.firing.includes(GATEWAY_RECONNECT_FLAP_RULE);
  let gatewayReconnects = prevFlap;
  let gatewayReconnectsSeen = flapSeen;
  let gatewayReconnectStreak = prevFlapStreak;
  if (flapValue === undefined) {
    if (wasFlapFiring) firing.push(GATEWAY_RECONNECT_FLAP_RULE);
  } else if (!flapSeen || unpaired.restarted) {
    // The first valid sample (including after a gap or a validated process
    // restart) is only a baseline. A new baseline is not a quiet window,
    // so a held page stays held.
    gatewayReconnects = flapValue;
    gatewayReconnectsSeen = true;
    gatewayReconnectStreak = 0;
    if (wasFlapFiring) firing.push(GATEWAY_RECONNECT_FLAP_RULE);
  } else if (flapValue < prevFlap) {
    // A backwards counter means the process restarted: re-baseline and resolve.
    gatewayReconnects = flapValue;
    gatewayReconnectsSeen = true;
    gatewayReconnectStreak = 0;
  } else if (flapValue > prevFlap) {
    gatewayReconnects = flapValue;
    gatewayReconnectsSeen = true;
    gatewayReconnectStreak = Math.min(prevFlapStreak + 1, GATEWAY_RECONNECT_FLAP_SAMPLES);
    if (wasFlapFiring || gatewayReconnectStreak >= GATEWAY_RECONNECT_FLAP_SAMPLES) firing.push(GATEWAY_RECONNECT_FLAP_RULE);
  } else {
    // A flat valid sample proves recovery and resolves.
    gatewayReconnects = flapValue;
    gatewayReconnectsSeen = true;
    gatewayReconnectStreak = 0;
  }

  // Handler saturation is a ticket, not a page: the dispatch pipeline still
  // works, but its tail crossed into seconds. A window is saturated when its
  // p99 crosses HANDLER_SATURATED_P99_SECONDS, read exactly from the bucket
  // deltas: the share of completed dispatches past the 1 s bucket exceeds
  // 1%. The window also needs HANDLER_SATURATED_MIN_OBS completions (quiet
  // windows stay silent) and HANDLER_SATURATED_MIN_SLOW slow ones (one slow
  // dispatch in a quiet window stays silent). Three consecutive saturated
  // windows ticket; any judged-quiet window or counter reset (process
  // restart) clears at once. The first valid sample only stores the
  // baseline; missing/invalid samples drop the baseline and hold a firing
  // ticket without manufacturing evidence. A validated incarnation change
  // (`unpaired.restarted`) re-baselines like the first sample and holds:
  // bucket deltas against a dead process baseline are not a same-process
  // window. Routine quantile movement stays log-only in the query pack;
  // this rule is the saturation backstop.
  const HANDLER_SATURATED_RULE = "handler_duration_saturated";
  const handlerSamples = gauge("two_bot_handler_duration_seconds_bucket");
  const handlerCounts: Record<string, number> = {};
  let handlerValid = true;
  for (const le of HANDLER_DURATION_BUCKET_LE) {
    const rows = handlerSamples.filter((s) => s.labels["le"] === le);
    const sample = rows[0];
    if (rows.length !== 1 || !sample || Object.keys(sample.labels).length !== 1
      || !Number.isSafeInteger(sample.value) || sample.value < 0) {
      handlerValid = false;
      break;
    }
    handlerCounts[le] = sample.value;
  }
  const wasHandlerFiring = prev.firing.includes(HANDLER_SATURATED_RULE);
  let handlerBuckets: Record<string, number> = {};
  let handlerStreak = 0;
  if (!handlerValid) {
    // Missing/invalid evidence drops the baseline, never the active ticket.
    if (wasHandlerFiring) firing.push(HANDLER_SATURATED_RULE);
  } else {
    const prevHandler = prev.handlerBuckets;
    const hasHandlerBaseline = prevHandler !== undefined
      && HANDLER_DURATION_BUCKET_LE.every((le) => {
        const count = prevHandler[le];
        return typeof count === "number" && Number.isSafeInteger(count) && count >= 0;
      });
    if (!hasHandlerBaseline || unpaired.restarted) {
      // The first valid sample (including after a gap, from legacy state,
      // or a validated process restart) is only a baseline; it never
      // resolves an active ticket on its own.
      handlerBuckets = { ...handlerCounts };
      if (wasHandlerFiring) firing.push(HANDLER_SATURATED_RULE);
    } else {
      // Any backwards bucket means the process restarted: re-baseline with a
      // clear streak, even when another bucket's new count overtook its old
      // one. A reset window is valid recovery evidence, so it resolves.
      const handlerReset = HANDLER_DURATION_BUCKET_LE.some((le) => handlerCounts[le]! < (prevHandler[le] ?? 0));
      handlerBuckets = { ...handlerCounts };
      if (handlerReset) {
        handlerStreak = 0;
      } else {
        // Window counts come from cumulative-bucket deltas; the `+Inf`
        // bucket delta is the completed-dispatch count, no `_count` row
        // needed. Past-bound observations are the count the 1 s bucket did
        // not absorb.
        const obs = handlerCounts["+Inf"]! - (prevHandler["+Inf"] ?? 0);
        const slow = obs - (handlerCounts["1"]! - (prevHandler["1"] ?? 0));
        if (obs >= HANDLER_SATURATED_MIN_OBS && slow >= HANDLER_SATURATED_MIN_SLOW
          && slow / obs > HANDLER_SATURATED_SHARE) {
          handlerStreak = Math.min((prev.handlerStreak ?? 0) + 1, HANDLER_SATURATED_SAMPLES);
          if (wasHandlerFiring || handlerStreak >= HANDLER_SATURATED_SAMPLES) firing.push(HANDLER_SATURATED_RULE);
        } else {
          handlerStreak = 0;
        }
      }
    }
  }

  // Intermittent job failures ticket per job: `job_consecutive_failures`
  // pages when one job fails three times in a row, but a job failing every
  // other run never trips it. Each job accumulates completed runs across
  // consecutive valid samples until the window holds
  // JOB_FAILURE_SHARE_MIN_RUNS runs; the judged window tickets when failures
  // exceed JOB_FAILURE_SHARE_RATIO of runs with at least
  // JOB_FAILURE_SHARE_MIN_FAILURES failures (mirroring
  // `eventsub_post_failed_share`). Only a judged window (floor met) at or
  // below the line resolves; flat or sub-floor windows hold an active
  // ticket, missing/invalid samples preserve firing and baseline, and any
  // backwards counter (restart) rebaselines. A validated incarnation
  // change (`unpaired.restarted`) also re-baselines like the first sample
  // and holds: run deltas against a dead process baseline are not a
  // same-process window. The first valid sample only stores the baseline
  // and never fires. Unknown job labels are ignored,
  // never subjects; a Rust job addition fails loud via the catalog job-label
  // gate until the fixed list names it. A job dead enough to page
  // `job_consecutive_failures` tickets here too: co-firing means continuous
  // outage, this ticket alone means intermittent sickness.
  const JOB_SHARE_RULE = "job_failure_share";
  const jobRunsRows = gauge("two_bot_job_runs_total");
  const jobRunsNow: Record<string, { failure: number; total: number }> | undefined = (() => {
    const acc: Record<string, { failure: number; total: number }> = Object.create(null);
    for (const job of JOB_RUNS_JOBS) {
      const success = exactSeries(jobRunsRows, { job, outcome: "success" });
      const failure = exactSeries(jobRunsRows, { job, outcome: "failure" });
      if (success === undefined || failure === undefined || !Number.isSafeInteger(success + failure)) return undefined;
      acc[job] = { failure, total: success + failure };
    }
    return acc;
  })();
  const prevJobShareBase = prev.jobShareBase;
  const prevJobShareSeen = prev.jobShareSeen ?? false;
  const jobShareBase: Record<string, { failure: number; total: number }> = Object.create(null);
  let jobShareSeen = prevJobShareSeen;
  if (jobRunsNow === undefined) {
    if (prevJobShareBase !== undefined) {
      for (const [job, base] of Object.entries(prevJobShareBase)) jobShareBase[job] = { ...base };
    }
    for (const key of prev.firing) {
      if (key === JOB_SHARE_RULE || key.startsWith(`${JOB_SHARE_RULE}:`)) firing.push(key);
    }
  } else if (!prevJobShareSeen || prevJobShareBase === undefined) {
    for (const [job, counts] of Object.entries(jobRunsNow)) jobShareBase[job] = { ...counts };
    jobShareSeen = true;
  } else if (unpaired.restarted) {
    // A validated process restart re-baselines like the first sample and
    // holds any firing ticket: run deltas against a dead process baseline
    // are never judged, resolved, or fired from this sample.
    for (const [job, counts] of Object.entries(jobRunsNow)) jobShareBase[job] = { ...counts };
    jobShareSeen = true;
    for (const key of prev.firing) {
      if (key === JOB_SHARE_RULE || key.startsWith(`${JOB_SHARE_RULE}:`)) firing.push(key);
    }
  } else {
    jobShareSeen = true;
    for (const job of JOB_RUNS_JOBS) {
      const now = jobRunsNow[job]!;
      const base = prevJobShareBase[job];
      const key = `${JOB_SHARE_RULE}:${job}`;
      const wasShareFiring = prev.firing.includes(key);
      if (base === undefined || typeof base.failure !== "number" || typeof base.total !== "number"
        || !Number.isSafeInteger(base.failure) || !Number.isSafeInteger(base.total)) {
        jobShareBase[job] = { ...now };
        if (wasShareFiring) firing.push(key);
        continue;
      }
      if (now.failure < base.failure || now.total < base.total) {
        // A backwards counter means the process restarted: re-baseline and resolve.
        jobShareBase[job] = { ...now };
        continue;
      }
      const dFail = now.failure - base.failure;
      const dTotal = now.total - base.total;
      if (dTotal >= JOB_FAILURE_SHARE_MIN_RUNS) {
        if (dFail >= JOB_FAILURE_SHARE_MIN_FAILURES && dFail / dTotal > JOB_FAILURE_SHARE_RATIO) {
          firing.push(key);
        }
        jobShareBase[job] = { ...now };
      } else {
        // Sub-floor windows stay quiet and hold the baseline so runs
        // accumulate toward the floor across consecutive valid samples.
        // An active ticket survives them: only a judged window clears it,
        // otherwise a sick job flaps ALERT/RESOLVED every window.
        jobShareBase[job] = { ...base };
        if (wasShareFiring) firing.push(key);
      }
    }
  }

  // Failed feed fetches (one ticket per window with new failures): any
  // increase of `two_bot_feed_fetch_errors_total` summed over the twelve
  // fixed error classes between two valid samples means a feed poll failed.
  // Ticket, not page: one dead source (or one transient transport blip)
  // never pages, while a poller failing every window keeps the ticket open
  // without re-notifying. The family records failures only, so even `other`
  // counts: an unknown class is a failed fetch with a lost reason, not healthy volume.
  // The sibling attempts family stays unconsumed (see the catalog): every
  // non-success attempt pairs 1:1 with an error increment
  // (`record_feed_fetch` in `crates/core/src/feeds_connector.rs`), so this
  // sum carries the same signal with finer reason codes. The first valid
  // sample only stores the baseline and never fires; a backwards sum means
  // the process restarted and re-baselines silently; a missing or malformed
  // family (older containers never emitted it, or a class row short) is no
  // window: it keeps the previous baseline and never fires on its own. `??`
  // covers DO storage written before these fields existed.
  const FEED_FETCH_FAILURES_RULE = "feed_fetch_failures";
  const feedErrorRows = gauge("two_bot_feed_fetch_errors_total");
  let feedErrorsNow: number | undefined;
  {
    let sum = 0;
    let complete = true;
    for (const errorClass of FEED_FETCH_ERROR_CLASSES) {
      const value = exactSeries(feedErrorRows, { error_class: errorClass });
      if (value === undefined) {
        complete = false;
        break;
      }
      sum += value;
    }
    if (complete) feedErrorsNow = sum;
  }
  const prevFeedErrors = prev.feedFetchErrors ?? 0;
  const feedErrorsSeen = prev.feedFetchErrorsSeen ?? false;
  let feedFetchErrors = prevFeedErrors;
  let feedFetchErrorsSeen = feedErrorsSeen;
  if (feedErrorsNow !== undefined) {
    feedFetchErrors = feedErrorsNow;
    feedFetchErrorsSeen = true;
    if (feedErrorsSeen && feedErrorsNow > prevFeedErrors) firing.push(FEED_FETCH_FAILURES_RULE);
  }

  // EventSub receive/classify gap (deliveries accepted but never classified):
  // the receiver increments `two_bot_eventsub_received_total` at body intake
  // and exactly one `two_bot_eventsub_classified_total{kind}` row before
  // answering (`receive` in `crates/bot/src/eventsub_http.rs`), with no
  // await between the two, so a healthy scrape shows received equal to the
  // summed kinds. A positive gap persisting across
  // EVENTSUB_RECEIVE_GAP_SAMPLES consecutive valid samples tickets (never
  // pages): one gapped scrape can land mid-delivery, but a stuck gap means
  // intake outruns classification. A joint reset resolves: the wedge died
  // with the old process. A negative gap cannot happen in one process (every
  // classification was received first), so a corrupt sample is no window.
  // Missing or malformed families (older containers without either row, or a
  // kind row short) break the streak but preserve a firing ticket, and never
  // let label values reach state or keys. Failed scrapes reset the streak
  // through interruptDispatchDrops.
  const RECEIVE_GAP_RULE = "eventsub_receive_gap";
  const wasGapFiring = prev.firing.includes(RECEIVE_GAP_RULE);
  const receivedNow = exactSeries(gauge("two_bot_eventsub_received_total"), {});
  const classifiedRows = gauge("two_bot_eventsub_classified_total");
  let classifiedNow: number | undefined;
  {
    let sum = 0;
    let complete = true;
    for (const kind of EVENTSUB_CLASSIFIED_KINDS) {
      const value = exactSeries(classifiedRows, { kind });
      if (value === undefined) {
        complete = false;
        break;
      }
      sum += value;
    }
    if (complete) classifiedNow = sum;
  }
  let gapValue: number | undefined;
  if (receivedNow !== undefined && classifiedNow !== undefined && receivedNow >= classifiedNow) {
    gapValue = receivedNow - classifiedNow;
  }
  const prevGapStreak = prev.eventsubReceiveGapStreak ?? 0;
  let eventsubReceiveGapStreak: number;
  if (gapValue === undefined) {
    eventsubReceiveGapStreak = 0;
    if (wasGapFiring) firing.push(RECEIVE_GAP_RULE);
  } else if (gapValue === 0) {
    eventsubReceiveGapStreak = 0;
  } else {
    eventsubReceiveGapStreak = Math.min(prevGapStreak + 1, EVENTSUB_RECEIVE_GAP_SAMPLES);
    if (wasGapFiring || eventsubReceiveGapStreak >= EVENTSUB_RECEIVE_GAP_SAMPLES) firing.push(RECEIVE_GAP_RULE);
  }

  // EventSub reconcile prior-failed findings: ticket on finding growth only
  // once a healthy/pending pass completes, with no failed-pass growth in the
  // window. Rust records the finding before awaiting repairs and verification,
  // so a mid-pass scrape must retain all baselines rather than consume the
  // delta or ticket a repair that has not landed. A failed completion consumes
  // the finding silently: the failed-pass sibling already names the incident.
  // The other error sites pair with a failed pass (see the catalog).
  // Require the finding and all three pass outcomes. Keep separate companion
  // baselines because the failed-pass rule may advance while the finding row
  // is missing. Any backwards counter re-baselines the whole window silently;
  // the first complete sample (including legacy storage) also only baselines.
  const PRIOR_FAILED_RULE = "eventsub_reconcile_prior_failed";
  const priorFailedNow = exactSeries(gauge("two_bot_eventsub_reconcile_errors_total"), { error_class: "reconcile_prior_failed" });
  const prevPriorFailed = prev.reconcilePriorFailed ?? 0;
  const prevPriorHealthy = prev.reconcilePriorFailedPassHealthy ?? 0;
  const prevPriorPending = prev.reconcilePriorFailedPassPending ?? 0;
  const prevPriorFailedPasses = prev.reconcilePriorFailedPassFailed ?? 0;
  const priorFailedSeen = (prev.reconcilePriorFailedSeen ?? false)
    && prev.reconcilePriorFailedPassHealthy !== undefined
    && prev.reconcilePriorFailedPassPending !== undefined;
  let reconcilePriorFailed = prevPriorFailed;
  let reconcilePriorFailedPassHealthy = prevPriorHealthy;
  let reconcilePriorFailedPassPending = prevPriorPending;
  let reconcilePriorFailedPassFailed = prevPriorFailedPasses;
  let reconcilePriorFailedSeen = priorFailedSeen;
  if (priorFailedNow !== undefined && reconcileHealthyNow !== undefined
    && reconcilePendingNow !== undefined && reconcileFailedNow !== undefined) {
    const reset = priorFailedNow < prevPriorFailed || reconcileHealthyNow < prevPriorHealthy
      || reconcilePendingNow < prevPriorPending || reconcileFailedNow < prevPriorFailedPasses;
    const nonFailedGrew = reconcileHealthyNow > prevPriorHealthy || reconcilePendingNow > prevPriorPending;
    const failedGrew = reconcileFailedNow > prevPriorFailedPasses;
    if (!priorFailedSeen || reset || nonFailedGrew || failedGrew) {
      reconcilePriorFailed = priorFailedNow;
      reconcilePriorFailedPassHealthy = reconcileHealthyNow;
      reconcilePriorFailedPassPending = reconcilePendingNow;
      reconcilePriorFailedPassFailed = reconcileFailedNow;
      reconcilePriorFailedSeen = true;
      if (priorFailedSeen && !reset && nonFailedGrew && !failedGrew && priorFailedNow > prevPriorFailed) {
        firing.push(PRIOR_FAILED_RULE);
      }
    }
  }

  // Automod sanction refusals (one ticket per window with new refusals): any
  // increase of `two_bot_automod_decisions_total{outcome="sanction_refused"}`
  // between two valid samples means a delete-adjacent sanction could not run.
  // Ticket, not page: the content decision still recorded, but the ladder
  // step never landed. The other seven outcomes stay quiet (see the catalog):
  // clean/exempt/dry-run verdicts and landed ladders are policy working as
  // designed, and counted retries are normal resilience. Unknown outcomes
  // collapse to `sanction_refused` in Rust, so a ticket can also mean a new
  // outcome needs classification. Baseline, restart and gap semantics mirror
  // the feed rule above.
  const SANCTION_REFUSED_RULE = "automod_sanction_refused";
  const sanctionRefusedNow = exactSeries(gauge("two_bot_automod_decisions_total"), { outcome: "sanction_refused" });
  const prevSanctionRefused = prev.automodSanctionRefused ?? 0;
  const sanctionRefusedSeen = prev.automodSanctionRefusedSeen ?? false;
  let automodSanctionRefused = prevSanctionRefused;
  let automodSanctionRefusedSeen = sanctionRefusedSeen;
  if (sanctionRefusedNow !== undefined) {
    automodSanctionRefused = sanctionRefusedNow;
    automodSanctionRefusedSeen = true;
    if (sanctionRefusedSeen && sanctionRefusedNow > prevSanctionRefused) firing.push(SANCTION_REFUSED_RULE);
  }

  // Moderation enforcement failures (one ticket per window with new failures):
  // any increase of `two_bot_moderation_enforcements_total` summed over every
  // verb and the `discord_error`/`storage_error` outcomes between two valid
  // samples means an enforcement never completed (uncertain Discord failure
  // or persistence failure). Ticket, not page: one rate-limited kick never
  // pages, while a Discord or store outage re-tickets every window. `success`
  // landed, `refused` is policy proving no mutation (like the `refused` voice
  // operations the `voice_failures` rule skips), and `other` is an unknown
  // collapse that is not evidence of failure, so all three stay quiet (see
  // the catalog). Baseline, restart and gap semantics mirror the feed rule
  // above.
  const MODERATION_FAILURES_RULE = "moderation_enforcement_failures";
  const enforcementRows = gauge("two_bot_moderation_enforcements_total");
  let moderationNow: number | undefined;
  {
    let sum = 0;
    let complete = true;
    for (const verb of MODERATION_VERBS) {
      for (const outcome of MODERATION_FAILURE_OUTCOMES) {
        const value = exactSeries(enforcementRows, { verb, outcome });
        if (value === undefined) {
          complete = false;
          break;
        }
        sum += value;
      }
      if (!complete) break;
    }
    if (complete) moderationNow = sum;
  }
  const prevModeration = prev.moderationEnforcementFailures ?? 0;
  const moderationSeen = prev.moderationEnforcementFailuresSeen ?? false;
  let moderationEnforcementFailures = prevModeration;
  let moderationEnforcementFailuresSeen = moderationSeen;
  if (moderationNow !== undefined) {
    moderationEnforcementFailures = moderationNow;
    moderationEnforcementFailuresSeen = true;
    if (moderationSeen && moderationNow > prevModeration) firing.push(MODERATION_FAILURES_RULE);
  }

  // Stuck voice REST attempts: `two_bot_voice_rest_in_flight_age_seconds`
  // above VOICE_REST_STUCK_SECONDS across VOICE_REST_STUCK_SAMPLES consecutive
  // valid samples tickets (never pages). Every voice REST send wraps its wire
  // attempt in a 10 s timeout with an in-flight guard that drops on timeout,
  // so a healthy gauge never reads past ~10 s; a sustained high age means an
  // attempt neither completed nor dropped its slot. A valid sample at or
  // below the bound resolves. Missing or malformed samples (absent family on
  // older containers, labeled, duplicated, fractional or negative rows) break
  // the streak but preserve a firing ticket. Failed scrapes reset the streak
  // through interruptDispatchDrops.
  const VOICE_REST_STUCK_RULE = "voice_rest_stuck";
  const wasVoiceStuckFiring = prev.firing.includes(VOICE_REST_STUCK_RULE);
  const stuckNow = exactSeries(gauge("two_bot_voice_rest_in_flight_age_seconds"), {});
  const prevVoiceStuckStreak = prev.voiceRestStuckStreak ?? 0;
  let voiceRestStuckStreak: number;
  if (stuckNow === undefined) {
    voiceRestStuckStreak = 0;
    if (wasVoiceStuckFiring) firing.push(VOICE_REST_STUCK_RULE);
  } else if (stuckNow <= VOICE_REST_STUCK_SECONDS) {
    voiceRestStuckStreak = 0;
  } else {
    voiceRestStuckStreak = Math.min(prevVoiceStuckStreak + 1, VOICE_REST_STUCK_SAMPLES);
    if (wasVoiceStuckFiring || voiceRestStuckStreak >= VOICE_REST_STUCK_SAMPLES) firing.push(VOICE_REST_STUCK_RULE);
  }

  // Voice global-backoff caps (one ticket per window with new caps): any
  // increase of `two_bot_voice_global_backoff_capped_total` between two valid
  // samples means a voice action hit a global rate-limit wait past
  // GLOBAL_BACKOFF_MAX (30 s) and shed to tick-driven queue backoff instead
  // of parking the guild actor. Ticket, never page: the shed is the designed
  // backpressure shape and no operator action shortens Discord-side limits,
  // but a storm of caps points at our own request pattern. Baseline, restart
  // and gap semantics mirror the feed rule above.
  const BACKOFF_CAPPED_RULE = "voice_global_backoff_capped";
  const backoffCappedNow = exactSeries(gauge("two_bot_voice_global_backoff_capped_total"), {});
  const prevBackoffCapped = prev.voiceBackoffCapped ?? 0;
  const backoffCappedSeen = prev.voiceBackoffCappedSeen ?? false;
  let voiceBackoffCapped = prevBackoffCapped;
  let voiceBackoffCappedSeen = backoffCappedSeen;
  if (backoffCappedNow !== undefined) {
    voiceBackoffCapped = backoffCappedNow;
    voiceBackoffCappedSeen = true;
    if (backoffCappedSeen && backoffCappedNow > prevBackoffCapped) firing.push(BACKOFF_CAPPED_RULE);
  }

  return { firing, state: { firing, jobErrorClasses, rest429, rest5xx, restTotal, poolStreak, dbErrors, sendBlocked, sendBlockedStreak, prefixVerdictRefused: nextPrefixVerdict, prefixVerdictStreak: nextPrefixVerdictStreak, prefixVerdictSeen: nextPrefixVerdictSeen, breakerOpenStreak, restRefusalInvalid, restRefusalRejected, restRefusalSeen, restPendingStuckStreak, voiceOps, voiceFailures, voiceDeadLetters, voiceOrphans, voiceVoteKickFailures: voiceVoteKickFailures ?? prevVoiceVoteKickFailures, voiceVoteKickFailuresSeen: voiceVoteKickFailures !== undefined ? true : prevVoteKickSeen, voiceVoteKickFiring, gatewayMissed, gatewayMissedSeen: true, voiceLiveSelfHeal, voiceLiveSelfHealSeen, checkpointFailures, checkpointFailuresSeen: true, receiverRefusals, receiverRefusalsSeen: true, receiverRefusalStreaks, ...unpaired.fields, gatewayNotReadySeen, gatewayNotReadyStreak, dispatchDrops, dispatchDropStreaks, communityFactsDrainFailures, communityFactsDrainStreak, auditHaltStreak, eventStoreLagStreak, reconcileHealthy, reconcilePending, reconcileFailed, reconcileSeen, retentionReplayNeeded, retentionReplayNeededSeen, gatewayDrainSkips, gatewayDrainSkipsSeen, voiceDispatchTimeouts, voiceDispatchTimeoutsSeen, voiceWatchdogStucks, voiceWatchdogStucksSeen, voiceHaltRecoveries, voiceHaltRecoveriesSeen, restBreakerOpens, restBreakerOpensSeen, restGlobalPauses, restGlobalPausesSeen, feedFetchErrors, feedFetchErrorsSeen, eventsubReceiveGapStreak, reconcilePriorFailed, reconcilePriorFailedPassHealthy, reconcilePriorFailedPassPending, reconcilePriorFailedPassFailed, reconcilePriorFailedSeen, automodSanctionRefused, automodSanctionRefusedSeen, moderationEnforcementFailures, moderationEnforcementFailuresSeen, voiceRestStuckStreak, voiceBackoffCapped, voiceBackoffCappedSeen, joinBurn: joinBurn.ring, eventsubPosts, eventsubPostsSeen, errorBudgetBurn: errorBudgetBurn.ring, eventsubTampered, eventsubTamperedSeen, eventsubTamperedStreak, gatewayLatencyLast, gatewayLatencyDeadStreak, gatewayLatencySeen, gatewayReconnects, gatewayReconnectsSeen, gatewayReconnectStreak, handlerBuckets, handlerStreak, jobShareBase, jobShareSeen }, joinHeld: joinBurn.held, joinSlowCovered: joinBurn.slowCovered };
}

export function ruleFor(key: string): RuleDef | undefined {
  const id = key.split(":")[0];
  return RULES.find((r) => r.id === id);
}

/** Promotion-stage verdict: `fail` true means the stage must fail so auto-rollback runs. */
export interface PromotionGateVerdict {
  fail: boolean;
  /** Gate rule ids present in the firing list, in gate order. */
  reasons: string[];
}

/**
 * Canary error-rate gate over an `evaluateMetrics` firing list. Fails when
 * any `PROMOTION_ERROR_GATE_RULES` entry fires (exact match: this gate's
 * rules carry no `:subject` suffix). A 429-only burst passes: backpressure
 * is not a rollback trigger. Callers wire the failure into their per-stage
 * health gate (staging verify, production gate, or the shared
 * gradual-promotion smoke wrapper) so the existing auto-rollback restores
 * the recorded pre-promote version.
 */
export function evaluatePromotionGate(firing: string[]): PromotionGateVerdict {
  const reasons = PROMOTION_ERROR_GATE_RULES.filter((rule) => firing.includes(rule));
  return { fail: reasons.length > 0, reasons };
}

/**
 * Fired-packet filename carrying the producer identity (TOG-12100):
 * `evidence-{ruleId}-{window}.json`. The rule id is the single shared
 * spelling also pinned in Rust (`ALERT_RULE_IDS` in
 * `crates/core/src/evidence.rs`) and documented in `docs/metrics.md`, so the
 * QA evidence table can attribute packets when several rules fire in one soak
 * window. Returns `undefined` for unknown keys rather than a misleading name.
 */
export function packetFilename(key: string, window: string): string | undefined {
  const rule = ruleFor(key);
  if (!rule) return undefined;
  const safe = (part: string) => part.replace(/[^A-Za-z0-9._-]/g, "-");
  return `evidence-${safe(rule.id)}-${safe(window)}.json`;
}

function jobErrorClassForAlert(
  key: string,
  classes: Record<string, JobErrorClass> | undefined,
): JobErrorClass | undefined {
  const prefix = "job_consecutive_failures:";
  if (!key.startsWith(prefix)) return undefined;
  const errorClass = classes?.[key.slice(prefix.length)];
  return errorClass !== undefined && JOB_ERROR_CLASS_SET.has(errorClass) ? errorClass : undefined;
}

/** Alert-message lines for transitions; no mentions, no secrets. */
export function transitionMessages(
  before: string[],
  after: string[],
  beforeJobErrorClasses?: Record<string, JobErrorClass>,
  afterJobErrorClasses?: Record<string, JobErrorClass>,
  context?: AlertContext,
): string[] {
  const out: string[] = [];
  const suffix = alertContextSuffix(context);
  for (const key of after.filter((k) => !before.includes(k))) {
    const rule = ruleFor(key);
    const runbook = rule ? runbookUrl(rule) : `${RUNBOOK_BASE_URL}runbook.md`;
    const errorClass = jobErrorClassForAlert(key, afterJobErrorClasses);
    const classSuffix = errorClass ? ` (last error class: ${errorClass})` : "";
    const severity = rule?.severity ? ` (${rule.severity})` : "";
    out.push(`two-bot-next ALERT ${key}${classSuffix}${severity}: ${rule?.summary ?? key}. Runbook: ${runbook}.${suffix}`);
  }
  for (const key of before.filter((k) => !after.includes(k))) {
    const errorClass = jobErrorClassForAlert(key, beforeJobErrorClasses);
    const classSuffix = errorClass ? ` (last error class: ${errorClass})` : "";
    out.push(`two-bot-next RESOLVED ${key}${classSuffix}.${suffix}`);
  }
  return out;
}

/**
 * Sustained loss of the metrics scrape (the observer going blind).
 *
 * This is a synthetic Worker-side incident, not a metric rule: it never enters
 * `evaluateMetrics` or `RULES` (those stay pinned to the Rust exposition
 * contract), and its streak lives under its own storage key so failed scrapes
 * can neither resolve metric incidents nor disturb their baselines. Three
 * consecutive failed scheduled scrapes ticket once; the first complete valid
 * scrape afterwards resolves once. Timeout, non-OK, oversized and invalid
 * exposition all count as failure; bodies never reach logs or notifications.
 */
export const SCRAPE_LOSS_THRESHOLD = 3;
/** Synthetic firing key for sustained scrape loss; never a `RULES` id. */
export const SCRAPE_LOSS_RULE_ID = "metrics_scrape_lost";
/** Runbook anchor for the scrape-loss ticket (`docs/runbook.md` heading slug). */
export const SCRAPE_LOSS_RUNBOOK = "runbook.md#alert-metrics-scrape-lost";
/** Summary line for the scrape-loss ticket. */
export const SCRAPE_LOSS_SUMMARY =
  `metrics scrape lost for ${SCRAPE_LOSS_THRESHOLD} consecutive scheduled scrapes`;

/** Observer-loss state, stored independently of metric-rule state. */
export interface ScrapeLossState {
  /** Consecutive failed scheduled scrapes, capped at `SCRAPE_LOSS_THRESHOLD`. */
  streak: number;
  /** Whether the sustained-loss incident is currently firing. */
  firing: boolean;
}

export const EMPTY_SCRAPE_LOSS: ScrapeLossState = { streak: 0, firing: false };

/** Coerce stored observer-loss state; legacy or corrupt storage stays quiet. */
export function sanitizeScrapeLossState(stored: unknown): ScrapeLossState {
  if (typeof stored !== "object" || stored === null) return { ...EMPTY_SCRAPE_LOSS };
  const record = stored as Record<string, unknown>;
  const streak = typeof record["streak"] === "number"
    && Number.isSafeInteger(record["streak"])
    && (record["streak"] as number) >= 0
    ? Math.min(record["streak"] as number, SCRAPE_LOSS_THRESHOLD)
    : 0;
  return { streak, firing: record["firing"] === true };
}

/** Firing line for the scrape-loss ticket; no mentions, no secrets, no bodies. */
export function scrapeLossFiringMessage(context?: AlertContext): string {
  return `two-bot-next ALERT ${SCRAPE_LOSS_RULE_ID} (ticket): ${SCRAPE_LOSS_SUMMARY}. Runbook: ${RUNBOOK_BASE_URL}${SCRAPE_LOSS_RUNBOOK}.${alertContextSuffix(context)}`;
}

/** Recovery line for the scrape-loss ticket; no mentions, no secrets, no bodies. */
export function scrapeLossResolvedMessage(context?: AlertContext): string {
  return `two-bot-next RESOLVED ${SCRAPE_LOSS_RULE_ID}.${alertContextSuffix(context)}`;
}
