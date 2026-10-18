import type { Sample } from "./alert-rules.ts";

/** Required inputs consumed by alert rules; pinned to metrics.rs in tests. */
export const ALERT_INPUT_LABELS: Record<string, Record<string, readonly string[]>> = {
  two_bot_job_last_success_timestamp_seconds: { job: ["invite_snapshot", "session_checkpoint", "counter", "rank", "scheduled_events", "settings", "presence_probe", "community_scorecard", "inactivity", "audit_retry", "scheduled_messages", "feeds", "self_role_recovery", "member_unban_sweep", "automod_expiry", "other"] },
  two_bot_job_consecutive_failures: { job: ["invite_snapshot", "session_checkpoint", "counter", "rank", "scheduled_events", "settings", "presence_probe", "community_scorecard", "inactivity", "audit_retry", "scheduled_messages", "feeds", "self_role_recovery", "member_unban_sweep", "automod_expiry", "other"] },
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
};

/**
 * Optional inputs validated when present but never required. The job error-class
 * gauge postdates older containers: requiring it would turn a valid older
 * exposition into a failed scrape (no page at all) instead of the documented
 * classless page. The creator-map generation/age gauges are optional for the
 * same reason: a rollback container without them must still scrape, with the
 * staleness rule held rather than every rule silenced. The evaluator already
 * tolerates their absence.
 */
export const ALERT_OPTIONAL_INPUT_LABELS: Record<string, Record<string, readonly string[]>> = {
  two_bot_job_last_error_class: { job: ["invite_snapshot", "session_checkpoint", "counter", "rank", "scheduled_events", "settings", "presence_probe", "community_scorecard", "inactivity", "audit_retry", "scheduled_messages", "feeds", "self_role_recovery", "member_unban_sweep", "automod_expiry", "other"], class: ["database", "rest", "configuration", "timeout", "panic", "feed", "recovery_required"] },
  two_bot_voice_creator_map_generation: {},
  two_bot_voice_creator_map_age_seconds: {},
  two_bot_voice_vote_kick_total: { outcome: ["started", "evidence_unavailable", "not_a_room", "initiator_not_occupant", "target_not_occupant", "self_target", "protected_target", "privileged_target", "authority_unavailable", "active_vote_exists", "cooldown", "initiator_limited", "reused_vote_id", "unknown_vote", "wrong_vote_boundary", "ineligible_voter", "repeated_vote", "invalid_time", "connect_denied_and_disconnected", "connect_denied_target_absent", "skipped_room_gone", "skipped_target_protected", "permission_missing", "discord_error", "gave_up", "other"] },
  two_bot_eventsub_classified_total: { kind: ["challenge", "notification", "revocation", "tampered", "ignored", "other"] },
  two_bot_event_store_consumer_lag_seconds: {},
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
      } else if (Object.keys(labels).length !== Object.keys(axes).length
        || Object.keys(axes).some((axis) => !labels[axis])
        || !Number.isFinite(value) || !Number.isInteger(value) || value < 0 || value > 18446744073709551615
        || (name === "two_bot_db_pool_configured" && value !== 0 && value !== 1)) return null;
      // Per-family state uses plain objects in the evaluator. Future families
      // are welcome, but never allow inherited object properties as subjects.
      if (name === "two_bot_internal_actions_total" && labels["family"]! in Object.prototype) return null;
    }
    samples.push({ name, labels, value });
  }
  return [...REQUIRED_INPUTS].every((key) => seen.has(key)) ? samples : null;
}
