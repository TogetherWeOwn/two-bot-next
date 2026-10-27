import { JOB_RUNS_JOBS, processIncarnation, type Sample } from "./alert-rules.ts";

/** Required inputs consumed by alert rules; pinned to metrics.rs in tests. */
export const ALERT_INPUT_LABELS: Record<string, Record<string, readonly string[]>> = {
  two_bot_job_last_success_timestamp_seconds: { job: JOB_RUNS_JOBS },
  two_bot_job_consecutive_failures: { job: JOB_RUNS_JOBS },
  two_bot_rest_requests_total: {
    route: ["GET /channels/:channel", "GET /channels/:channel/messages", "GET /guilds/:guild", "GET /guilds/:guild/members", "GET /guilds/:guild/scheduled-events", "DELETE /guilds/:guild/bans/:member", "DELETE /channels/:channel/permissions/:overwrite", "PUT /applications/:application/commands", "PUT /applications/:application/guilds/:guild/commands", "POST /interactions/:interaction/:token/callback", "POST /channels/:channel/messages", "DELETE /channels/:channel/messages/:message", "DELETE /guilds/:guild/members/:member", "PUT /guilds/:guild/bans/:member", "PATCH /guilds/:guild/members/:member", "PATCH /channels/:channel", "PUT /channels/:channel/voice-status", "PUT /channels/:channel/permissions/:overwrite", "POST /channels/:channel/messages/bulk-delete", "PUT /guilds/:guild/members/:member/roles/:role", "DELETE /guilds/:guild/members/:member/roles/:role", "POST /guilds/:guild/scheduled-events", "PATCH /guilds/:guild/scheduled-events/:event", "DELETE /guilds/:guild/scheduled-events/:event", "POST /guilds/:guild/channels", "DELETE /channels/:channel", "GET /guilds/:guild/members/:member", "GET /users/@me", "PATCH /webhooks/:application/:token/messages/@original", "other"],
    result: ["2xx", "3xx", "4xx", "429", "5xx", "transport"],
  },
  two_bot_db_pool_configured: {},
  two_bot_db_pool_connections: {},
  two_bot_db_pool_idle_connections: {},
  two_bot_db_pool_max_connections: {},
  two_bot_db_errors_total: { op: ["admission", "other"] },
  two_bot_send_admissions_total: { outcome: ["admitted", "blocked", "storage_error", "other"] },
  two_bot_voice_operations_total: { op: ["create", "move", "delete"], outcome: ["success", "category_full", "discord", "persistence", "refused", "cancelled"] },
  two_bot_voice_dead_letters_total: { action: ["create", "move", "delete", "companion", "ownership", "kick", "rename", "limit", "other"] },
  two_bot_voice_orphans_total: {},
  two_bot_voice_joins_received_total: {},
  two_bot_voice_join_refused_total: { reason: ["halted", "access", "unknown_creator", "stale_ticket", "duplicate", "name_blocked", "other"] },
  two_bot_voice_join_to_move_seconds_bucket: { le: ["0.001", "0.005", "0.01", "0.05", "0.1", "0.5", "1", "5", "+Inf"] },
  two_bot_gateway_missed_events_total: {},
  two_bot_gateway_disconnects_total: {},
  two_bot_gateway_resumes_total: {},
  two_bot_gateway_events_total: { event: ["READY", "RESUMED", "GUILD_CREATE", "GUILD_DELETE", "GUILD_UPDATE", "GUILD_MEMBER_ADD", "GUILD_MEMBER_REMOVE", "GUILD_MEMBER_UPDATE", "MESSAGE_CREATE", "MESSAGE_UPDATE", "MESSAGE_DELETE", "MESSAGE_REACTION_ADD", "MESSAGE_REACTION_REMOVE", "VOICE_STATE_UPDATE", "PRESENCE_UPDATE", "INVITE_CREATE", "INVITE_DELETE", "INTERACTION_CREATE", "HEARTBEAT_ACK", "GATEWAY_CLOSE", "other"] },
  two_bot_gateway_checkpoint_failures_total: { stage: ["pre_commit", "commit"] },
  two_bot_dispatch_drops_total: { lane: ["messages", "interactions", "registry", "privileged", "busy", "reactions"] },
  two_bot_internal_actions_total: {
    family: ["announcement", "event", "settings", "moderation", "membership", "other"],
    outcome: ["executed", "auth_failure", "unknown_key", "clock_skew", "nonce_replay", "rate_limit", "unknown_action", "action_disabled", "malformed_body", "conflict", "upstream", "internal"],
  },
  // Gateway transport and job-run family. Always emitted on the same render
  // path as the gateway/job inputs above (metrics.rs, since #82/#123), so a
  // container that emits the required set cannot lack these: absence fails
  // the scrape instead of silently holding the transport rules off. The
  // handler `_sum`/`_count` rows stay unvalidated passthrough (only the
  // `_bucket` family is an alert input); the rule reads the window count
  // from the `+Inf` bucket delta.
  two_bot_gateway_latency_seconds: {},
  two_bot_gateway_reconnects_total: {},
  two_bot_handler_duration_seconds_bucket: { le: ["0.001", "0.005", "0.01", "0.05", "0.1", "0.5", "1", "5", "+Inf"] },
  two_bot_job_runs_total: { job: JOB_RUNS_JOBS, outcome: ["success", "failure"] },
};

/**
 * Optional inputs validated when present but never required. The job error-class
 * gauge postdates older containers: requiring it would turn a valid older
 * exposition into a failed scrape (no page at all) instead of the documented
 * classless page. The creator-map generation/age gauges and the REST admission
 * breaker-open gauge are optional for the same reason: a rollback container
 * without them must still scrape, with the staleness/breaker rules held
 * rather than every rule silenced. The EventSub reconcile pass outcomes are
 * optional for the same reason: an older container without the family must
 * still scrape, with the failed-pass ticket held rather than every rule
 * silenced. The EventSub delivery-kind counters are optional for the same
 * reason: an older container without them must still scrape, with the
 * tampered-signature page held rather than every rule silenced. The voice
 * stall family below (halt latch, live not-ready age, self-heal counter,
 * queue oldest age) is optional for the same reason: an older container
 * without it must still scrape, with the stall rules held rather than every
 * rule silenced. Queue depth (`two_bot_voice_queue_pending{lane}`) stays
 * unvalidated: no rule consumes it (see the catalog deferral). The guard
 * health inputs (token latch, invalid/refused totals, pending global bodies)
 * are optional for the same reason: an older container without them must
 * still scrape, with the token/surge/stuck rules held rather than every rule
 * silenced. The safety family below (retention purge-skip counter, voice
 * dispatch-timeout, watchdog-stuck and halt-recovery counters, breaker opens
 * and global-pause counters) is optional for the same reason, with the
 * safety-series tickets held. Breaker closes
 * (`two_bot_rest_guard_breaker_closes_total`) stays unvalidated: no rule
 * consumes it (see the catalog deferral). The domain-outcome family below
 * (feed-fetch errors, EventSub received and reconcile errors, automod
 * decisions, moderation enforcements, voice REST in-flight age and
 * global-backoff caps) is optional for the same reason: an older container
 * without it must still scrape, with the domain-outcome tickets held rather
 * than every rule silenced. The evaluator already tolerates their absence.
 */
export const ALERT_OPTIONAL_INPUT_LABELS: Record<string, Record<string, readonly string[]>> = {
  two_bot_gateway_prefix_trigger_refused_total: { reason: ["verdict", "other"] },
  // Rollback containers predate this counter; validate it only when present.
  two_bot_gateway_drain_skips_total: {},
  two_bot_job_last_error_class: { job: JOB_RUNS_JOBS, class: ["database", "rest", "configuration", "timeout", "panic", "feed", "recovery_required"] },
  two_bot_voice_creator_map_generation: {},
  two_bot_voice_creator_map_age_seconds: {},
  two_bot_rest_guard_breaker_open: {},
  two_bot_rest_guard_token_invalid: {},
  two_bot_rest_guard_invalid_requests_total: {},
  two_bot_rest_guard_rejected_requests_total: {},
  two_bot_rest_guard_pending_global_responses: {},
  // Older containers may omit the halt gauge; absence holds its active ticket.
  two_bot_audit_delivery_halt: {},
  two_bot_community_facts_drain_failures_total: { reason: ["error", "timeout", "other"] },
  two_bot_eventsub_posts_total: { outcome: ["attempted", "succeeded", "failed", "other"] },
  two_bot_eventsub_reconcile_passes_total: { outcome: ["healthy", "pending", "failed"] },
  two_bot_voice_vote_kick_total: { outcome: ["started", "evidence_unavailable", "not_a_room", "initiator_not_occupant", "target_not_occupant", "self_target", "protected_target", "privileged_target", "authority_unavailable", "active_vote_exists", "cooldown", "initiator_limited", "reused_vote_id", "unknown_vote", "wrong_vote_boundary", "ineligible_voter", "repeated_vote", "invalid_time", "connect_denied_and_disconnected", "connect_denied_target_absent", "skipped_room_gone", "skipped_target_protected", "permission_missing", "discord_error", "gave_up", "other"] },
  two_bot_eventsub_classified_total: { kind: ["challenge", "notification", "revocation", "tampered", "ignored", "invalid_secret", "invalid_header", "body_too_large", "bad_signature", "invalid_timestamp", "stale_timestamp", "invalid_body", "duplicate_keys", "wrong_subscription", "invalid_challenge", "invalid_callback", "invalid_broadcaster_id", "too_many_broadcasters", "other"] },
  two_bot_event_store_consumer_lag_seconds: {},
  // Voice stall family; old containers may omit the whole family.
  two_bot_voice_worker_halted: {},
  two_bot_voice_live_self_heal_total: {},
  two_bot_voice_live_not_ready_seconds: {},
  two_bot_voice_queue_oldest_age_seconds: {},
  // Safety/liveness family; old containers may omit the whole family.
  two_bot_retention_replay_needed_total: {},
  two_bot_voice_dispatch_timeouts_total: {},
  two_bot_voice_watchdog_stucks_total: {},
  two_bot_voice_halt_recoveries_total: {},
  two_bot_rest_guard_breaker_opens_total: {},
  two_bot_rest_guard_global_pauses_total: {},
  // Domain-outcome and voice-liveness family; old containers may omit the whole family.
  two_bot_feed_fetch_errors_total: { error_class: ["invalid_source", "non_public_address", "redirect_refused", "too_large", "invalid_encoding", "unsupported_content_type", "unsupported_encoding", "resolve", "transport", "deadline", "http_error", "other"] },
  two_bot_eventsub_received_total: {},
  two_bot_eventsub_reconcile_errors_total: { error_class: ["reconcile_action_failed", "reconcile_prior_failed", "reconcile_unhealthy", "reconcile_failed", "other"] },
  two_bot_automod_decisions_total: { outcome: ["accepted", "already_processed", "dry_run", "protected", "deleted", "warned", "timed_out", "sanction_refused"] },
  two_bot_moderation_enforcements_total: {
    verb: ["ban", "tempban", "kick", "timeout", "warn", "purge", "slowmode", "lockdown", "unlock"],
    outcome: ["success", "refused", "discord_error", "storage_error", "other"],
  },
  two_bot_voice_rest_in_flight_age_seconds: {},
  two_bot_voice_global_backoff_capped_total: {},
  // Same-scrape lifecycle evidence; old containers may omit the whole family.
  two_bot_process_incarnation: { part: ["0", "1", "2", "3"] },
};

/** Every series the validator checks: required plus optional-when-present. */
const VALIDATED_INPUT_LABELS: Record<string, Record<string, readonly string[]>> = {
  ...ALERT_INPUT_LABELS,
  ...ALERT_OPTIONAL_INPUT_LABELS,
};

function identity(name: string, labels: Record<string, string>): string {
  return JSON.stringify([name, Object.entries(labels).sort(([a], [b]) => a.localeCompare(b))]);
}

function combinations(axes: Record<string, readonly string[]>): Record<string, string>[] {
  let rows: Record<string, string>[] = [{}];
  for (const [key, values] of Object.entries(axes)) {
    rows = rows.flatMap((row) => values.map((value) => ({ ...row, [key]: value })));
  }
  return rows;
}

const REQUIRED_INPUTS = new Set(Object.entries(ALERT_INPUT_LABELS).flatMap(([name, axes]) =>
  combinations(axes).map((labels) => identity(name, labels))));
// EventVoice was added after the Worker scrape contract. A rollback may omit
// this job entirely, but a present job must still supply the complete set.
const EVENT_VOICE_INPUTS = new Set(Object.entries(ALERT_INPUT_LABELS).flatMap(([name, axes]) =>
  combinations(axes).filter((labels) => labels.job === "event_voice").map((labels) => identity(name, labels))));

// Prometheus numbers, not JavaScript's empty/hexadecimal/Infinity coercions.
const NUMBER = "(?:[+-]?(?:[0-9]+(?:\\.[0-9]*)?|\\.[0-9]+)(?:[eE][+-]?[0-9]+)?|NaN|[+-]?Inf)";
const SAMPLE = new RegExp(`^([a-zA-Z_:][a-zA-Z0-9_:]*)(?:\\{(.*)\\})?[ \\t]+(${NUMBER})(?:[ \\t]+[+-]?[0-9]+)?[ \\t]*$`);
const LABEL = /([a-zA-Z_][a-zA-Z0-9_]*)="((?:[^"\\]|\\[\\"n])*)"(?:,|$)/y;

/** Reject partial or invalid scrapes before any baseline/streak/firing-state write. */
export function parseAlertScrape(text: string): Sample[] | null {
  const samples: Sample[] = [];
  const seen = new Set<string>();
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    const match = SAMPLE.exec(line);
    if (!match) return null;
    const name = match[1]!;
    const labels: Record<string, string> = Object.create(null);
    const labelText = match[2] ?? "";
    let offset = 0;
    while (offset < labelText.length) {
      LABEL.lastIndex = offset;
      const label = LABEL.exec(labelText);
      if (!label || Object.hasOwn(labels, label[1]!)) return null;
      labels[label[1]!] = label[2]!.replace(/\\([\\"n])/g, (_, escaped: string) => escaped === "n" ? "\n" : escaped);
      offset = LABEL.lastIndex;
    }
    const valueText = match[3]!;
    const value = valueText === "+Inf" || valueText === "Inf" ? Infinity : valueText === "-Inf" ? -Infinity : Number(valueText);
    const key = identity(name, labels);
    if (seen.has(key)) return null;
    seen.add(key);
    if (Object.hasOwn(VALIDATED_INPUT_LABELS, name)) {
      const axes = VALIDATED_INPUT_LABELS[name]!;
      if (name === "two_bot_event_store_consumer_lag_seconds") {
        if (Object.keys(labels).length !== 0
          || !Number.isFinite(value) || value < 0 || value > 18446744073709551615) return null;
      } else if (name === "two_bot_gateway_latency_seconds") {
        // Last-ACK latency: NaN until the first heartbeat ACK lands on the
        // current connection (metrics.rs resets to None on every reconnect),
        // otherwise a finite nonnegative second count. Fractional values are
        // valid samples; only the unknown marker is non-finite.
        if (Object.keys(labels).length !== 0
          || !(Number.isNaN(value) || (Number.isFinite(value) && value >= 0 && value <= 18446744073709551615))) return null;
      } else if (Object.keys(labels).length !== Object.keys(axes).length
        || Object.keys(axes).some((axis) => !labels[axis])
        || !Number.isFinite(value) || !Number.isInteger(value) || value < 0 || value > 18446744073709551615
        || ((name === "two_bot_db_pool_configured" || name === "two_bot_audit_delivery_halt" || name === "two_bot_rest_guard_breaker_open" || name === "two_bot_voice_worker_halted" || name === "two_bot_rest_guard_token_invalid") && value !== 0 && value !== 1)) return null;
      // Per-family state uses plain objects in the evaluator. Future families
      // are welcome, but never allow inherited object properties as subjects.
      if (name === "two_bot_internal_actions_total" && labels["family"]! in Object.prototype) return null;
    }
    samples.push({ name, labels, value });
  }
  const incarnation = samples.filter((sample) => sample.name === "two_bot_process_incarnation");
  if (incarnation.length > 0 && processIncarnation(incarnation) === undefined) return null;
  const hasEventVoice = samples.some((sample) => Object.hasOwn(VALIDATED_INPUT_LABELS, sample.name)
    && sample.labels.job === "event_voice");
  return [...REQUIRED_INPUTS].every((key) => seen.has(key)
    || (!hasEventVoice && EVENT_VOICE_INPUTS.has(key))) ? samples : null;
}
