import { readFileSync } from "node:fs";

const rust = readFileSync(new URL("../../../crates/core/src/metrics.rs", import.meta.url), "utf8");

export function rustLabels(name: string | readonly string[]): string[] {
  // Fixed wire dimensions can be specified literally, independent of a Rust constant name.
  if (typeof name !== "string") return [...name];
  // Accept both `&[&str] = &[...]` and `[&str; N] = [...]` const shapes.
  const list = new RegExp(`pub const ${name}:[^=]*=\\s*(?:&)?\\[([\\s\\S]*?)\\];`).exec(rust);
  if (!list) throw new Error(`missing Rust metric dimension ${name}`);
  const quoted = [...list[1]!.matchAll(/"([^"]+)"/g)].map((m) => m[1]!);
  if (quoted.length > 0) return quoted;
  // Enum-variant dimensions (e.g. JOB_ERROR_CLASSES) carry no quoted
  // strings; derive snake_case labels from the variant names instead.
  const variants = [...list[1]!.matchAll(/JobErrorClass::([A-Za-z]+)/g)].map((m) => m[1]!);
  if (variants.length > 0) {
    return variants.map((v) => v.replace(/([a-z])([A-Z])/g, "$1_$2").toLowerCase());
  }
  throw new Error(`no parseable labels in Rust metric dimension ${name}`);
}

export const RUST_ALERT_AXES: Record<string, Record<string, string | readonly string[]>> = {
  two_bot_process_incarnation: { part: ["0", "1", "2", "3"] },
  two_bot_job_last_success_timestamp_seconds: { job: "JOBS" },
  two_bot_job_consecutive_failures: { job: "JOBS" },
  two_bot_job_last_error_class: { job: "JOBS", class: "JOB_ERROR_CLASSES" },
  two_bot_rest_requests_total: { route: "REST_ROUTES", result: "RESULTS" },
  two_bot_db_pool_configured: {},
  two_bot_db_pool_connections: {},
  two_bot_db_pool_idle_connections: {},
  two_bot_db_pool_max_connections: {},
  two_bot_db_errors_total: { op: "DB_ERROR_OPS" },
  two_bot_send_admissions_total: { outcome: "SEND_ADMISSION_OUTCOMES" },
  two_bot_gateway_prefix_trigger_refused_total: { reason: "PREFIX_TRIGGER_REFUSED_REASONS" },
  two_bot_voice_operations_total: { op: "VOICE_OPERATIONS", outcome: "VOICE_OUTCOMES" },
  two_bot_voice_dead_letters_total: { action: "VOICE_DEAD_ACTIONS" },
  two_bot_voice_orphans_total: {},
  two_bot_voice_vote_kick_total: { outcome: "VOICE_VOTE_KICK_OUTCOMES" },
  two_bot_voice_joins_received_total: {},
  two_bot_voice_join_refused_total: { reason: "VOICE_JOIN_REFUSAL_REASONS" },
  two_bot_voice_join_to_move_seconds_bucket: { le: "VOICE_JOIN_TO_MOVE_BUCKET_LE" },
  two_bot_voice_creator_map_generation: {},
  two_bot_voice_creator_map_age_seconds: {},
  two_bot_voice_worker_halted: {},
  two_bot_voice_live_self_heal_total: {},
  two_bot_voice_live_not_ready_seconds: {},
  two_bot_voice_queue_oldest_age_seconds: {},
  two_bot_audit_delivery_halt: {},
  two_bot_gateway_missed_events_total: {},
  two_bot_gateway_disconnects_total: {},
  two_bot_gateway_resumes_total: {},
  two_bot_gateway_events_total: { event: "EVENTS" },
  two_bot_gateway_checkpoint_failures_total: { stage: "CHECKPOINT_FAILURE_STAGES" },
  two_bot_dispatch_drops_total: { lane: "DISPATCH_LANES" },
  two_bot_internal_actions_total: { family: "INTERNAL_ACTION_FAMILIES", outcome: "INTERNAL_ACTION_OUTCOMES" },
  two_bot_rest_guard_breaker_open: {},
  two_bot_rest_guard_token_invalid: {},
  two_bot_rest_guard_invalid_requests_total: {},
  two_bot_rest_guard_rejected_requests_total: {},
  two_bot_rest_guard_pending_global_responses: {},
  two_bot_community_facts_drain_failures_total: { reason: "COMMUNITY_FACTS_DRAIN_REASONS" },
  two_bot_eventsub_posts_total: { outcome: "EVENTSUB_POST_OUTCOMES" },
  two_bot_eventsub_reconcile_passes_total: { outcome: "EVENTSUB_RECONCILE_OUTCOMES" },
  two_bot_eventsub_classified_total: { kind: "EVENTSUB_DELIVERY_KINDS" },
  two_bot_event_store_consumer_lag_seconds: {},
  two_bot_retention_replay_needed_total: {},
  two_bot_gateway_drain_skips_total: {},
  two_bot_voice_dispatch_timeouts_total: {},
  two_bot_voice_watchdog_stucks_total: {},
  two_bot_voice_halt_recoveries_total: {},
  two_bot_rest_guard_breaker_opens_total: {},
  two_bot_rest_guard_global_pauses_total: {},
  two_bot_gateway_latency_seconds: {},
  two_bot_gateway_reconnects_total: {},
  // Handler buckets render from the private BUCKETS_MICROS (no pub label
  // const like VOICE_JOIN_TO_MOVE_BUCKET_LE), so the wire `le` spellings are
  // literal here, pinned against the validator and the rule below.
  two_bot_handler_duration_seconds_bucket: { le: ["0.001", "0.005", "0.01", "0.05", "0.1", "0.5", "1", "5", "+Inf"] },
  two_bot_job_runs_total: { job: "JOBS", outcome: "JOB_OUTCOMES" },
  two_bot_feed_fetch_errors_total: { error_class: "FEED_FETCH_ERROR_CLASSES" },
  two_bot_eventsub_received_total: {},
  two_bot_eventsub_reconcile_errors_total: { error_class: "EVENTSUB_RECONCILE_ERROR_CLASSES" },
  two_bot_automod_decisions_total: { outcome: "AUTOMOD_DECISION_OUTCOMES" },
  two_bot_moderation_enforcements_total: { verb: "MODERATION_VERBS", outcome: "MODERATION_ENFORCEMENT_OUTCOMES" },
  two_bot_voice_rest_in_flight_age_seconds: {},
  two_bot_voice_global_backoff_capped_total: {},
};

/** Complete consumed-input fixture generated from the Rust label contract, not the validator. */
export function metricsLines(rankFailures = 0): string[] {
  const lines: string[] = [];
  for (const [name, axes] of Object.entries(RUST_ALERT_AXES)) {
    let rows: Record<string, string>[] = [{}];
    for (const [label, dimension] of Object.entries(axes)) {
      rows = rows.flatMap((row) => rustLabels(dimension).map((value) => ({ ...row, [label]: value })));
    }
    for (const labels of rows) {
      const fields = Object.entries(labels).map(([k, v]) => `${k}="${v}"`).join(",");
      const value = name === "two_bot_process_incarnation" && labels.part === "3" ? 1
        : name === "two_bot_job_consecutive_failures" && labels.job === "rank" ? rankFailures : 0;
      lines.push(`${name}${fields ? `{${fields}}` : ""} ${value}`);
    }
  }
  return lines;
}

export function metricsBody(rankFailures = 0): string {
  return [...metricsLines(rankFailures), ""].join("\n");
}
