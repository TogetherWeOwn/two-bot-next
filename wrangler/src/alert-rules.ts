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
}

/** Cadence in seconds of every scheduled job (crates/core/src/*_INTERVAL_MS). */
export const JOB_INTERVAL_SECONDS: Record<string, number> = {
  counter: 60,
  rank: 600,
  scheduled_events: 600,
  presence_probe: 3600,
  community_scorecard: 60,
  inactivity: 3600,
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
export const FAILURE_THRESHOLD = 3;
/** 429s must exceed this share of REST requests between two samples... */
export const REST_429_RATIO = 0.1;
/** ...and the window must hold at least this many requests. */
export const REST_429_MIN_REQUESTS = 10;
/** Pool at max with zero idle for this many consecutive samples. */
export const POOL_SATURATED_SAMPLES = 3;
/** DB errors must reach this many between two samples... */
export const DB_ERROR_MIN_ERRORS = 3;
/** ...and send-admission refusals must appear in this many consecutive samples. */
export const SEND_BLOCKED_SAMPLES = 3;
/** Voice failures must exceed this share of room operations between two samples... */
export const VOICE_FAILURE_RATIO = 0.05;
/** ...and the window must hold at least this many operations. */
export const VOICE_FAILURE_MIN_OPS = 10;

export const RULES: readonly RuleDef[] = [
  { id: "job_stale", summary: `scheduled job has no success for more than ${STALE_INTERVALS} intervals`, runbook: "runbook.md#alert-job-stale" },
  { id: "job_consecutive_failures", summary: `scheduled job failed ${FAILURE_THRESHOLD}+ times in a row`, runbook: "runbook.md#alert-job-failures" },
  { id: "rest_429_rate", summary: `Discord REST 429s exceed ${REST_429_RATIO * 100}% of requests`, runbook: "runbook.md#alert-rest-429" },
  { id: "db_pool_saturated", summary: `database pool exhausted for ${POOL_SATURATED_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-db-pool" },
  { id: "db_errors", summary: `database errors reached ${DB_ERROR_MIN_ERRORS}+ between samples`, runbook: "runbook.md#alert-db-errors" },
  { id: "send_admission_blocked", summary: `Discord sends refused admission for ${SEND_BLOCKED_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-send-admission-blocked" },
  { id: "voice_failures", summary: `voice room lifecycle failures exceed ${VOICE_FAILURE_RATIO * 100}% of operations (min ${VOICE_FAILURE_MIN_OPS} ops), or new dead-letters/orphans`, runbook: "runbook.md#alert-voice-failures" },
  { id: "gateway_missed_events", summary: `gateway missed events increased between samples`, runbook: "runbook.md#alert-gateway-missed-events" },
  { id: "ticker_stale", summary: `15 s ticker has no success for more than ${TICKER_STALE_SECONDS / 60} minutes`, runbook: "runbook.md#alert-ticker-stale" },
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

export interface MetricsAlertState {
  /** Rule ids (with subject) currently firing, e.g. `job_stale:rank`. */
  firing: string[];
  rest429: number;
  restTotal: number;
  poolStreak: number;
  dbErrors: number;
  sendBlocked: number;
  sendBlockedStreak: number;
  voiceOps: number;
  voiceFailures: number;
  voiceDeadLetters: number;
  voiceOrphans: number;
  gatewayMissed: number;
  /** False until the first evaluation stores a baseline: the first sample never fires. */
  gatewayMissedSeen: boolean;
}

export const EMPTY_STATE: MetricsAlertState = { firing: [], rest429: 0, restTotal: 0, poolStreak: 0, dbErrors: 0, sendBlocked: 0, sendBlockedStreak: 0, voiceOps: 0, voiceFailures: 0, voiceDeadLetters: 0, voiceOrphans: 0, gatewayMissed: 0, gatewayMissedSeen: false };

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

export interface Evaluation {
  /** Firing keys `ruleId` or `ruleId:subject`. */
  firing: string[];
  state: MetricsAlertState;
}

export function evaluateMetrics(samples: Sample[], prev: MetricsAlertState, nowSeconds: number): Evaluation {
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

  let rest429 = 0;
  let restTotal = 0;
  for (const s of gauge("two_bot_rest_requests_total")) {
    restTotal += s.value;
    if (s.labels["result"] === "429") rest429 += s.value;
  }
  // A counter that went backwards means the process restarted: no window.
  const reset = rest429 < prev.rest429 || restTotal < prev.restTotal;
  const dTotal = restTotal - prev.restTotal;
  const d429 = rest429 - prev.rest429;
  if (!reset && dTotal >= REST_429_MIN_REQUESTS && d429 / dTotal > REST_429_RATIO) firing.push("rest_429_rate");

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

  // Voice lifecycle failures (T3 room create/move/delete budget in
  // docs/voice-cutover-rollback-triggers.md): any outcome other than
  // `success` (`category_full`, `discord`, `persistence`, `cancelled`)
  // counts as a failure — the join did not place a room, or the move/delete
  // did not complete. A new dead-letter (a queue write that exhausted 10
  // attempts) or orphan (an untracked creator-channel orphan needing manual
  // deletion) fires on its own, so a low-volume stranded-member failure
  // still pages when the operation window is too small for the ratio.
  let voiceOps = 0;
  let voiceFailures = 0;
  for (const s of gauge("two_bot_voice_operations_total")) {
    voiceOps += s.value;
    if (s.labels["outcome"] !== "success") voiceFailures += s.value;
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
  if (!voiceReset && ((dVoiceOps >= VOICE_FAILURE_MIN_OPS && dVoiceFailures / dVoiceOps > VOICE_FAILURE_RATIO) || dVoiceDead >= 1 || dVoiceOrphans >= 1)) {
    firing.push("voice_failures");
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

  return { firing, state: { firing, rest429, restTotal, poolStreak, dbErrors, sendBlocked, sendBlockedStreak, voiceOps, voiceFailures, voiceDeadLetters, voiceOrphans, gatewayMissed, gatewayMissedSeen: true } };
}

export function ruleFor(key: string): RuleDef | undefined {
  const id = key.split(":")[0];
  return RULES.find((r) => r.id === id);
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

/** Alert-message lines for transitions; no mentions, no secrets. */
export function transitionMessages(before: string[], after: string[]): string[] {
  const out: string[] = [];
  for (const key of after.filter((k) => !before.includes(k))) {
    const rule = ruleFor(key);
    const runbook = rule ? runbookUrl(rule) : `${RUNBOOK_BASE_URL}runbook.md`;
    out.push(`two-bot-next ALERT ${key}: ${rule?.summary ?? key}. Runbook: ${runbook}`);
  }
  for (const key of before.filter((k) => !after.includes(k))) {
    out.push(`two-bot-next RESOLVED ${key}.`);
  }
  return out;
}
