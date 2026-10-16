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
/** Pool at max with zero idle for this many consecutive samples. */
export const POOL_SATURATED_SAMPLES = 3;
/** DB errors must reach this many between two samples... */
export const DB_ERROR_MIN_ERRORS = 3;
/** ...and send-admission refusals must appear in this many consecutive samples. */
export const SEND_BLOCKED_SAMPLES = 3;
/** Website-action receiver refusals must appear in this many consecutive samples. */
export const RECEIVER_REFUSAL_SAMPLES = 3;
/** Consecutive unpaired gateway samples before the page; bound justified in the runbook. */
export const GATEWAY_UNPAIRED_SAMPLES = 3;
/** Voice failures must exceed this share of room operations between two samples... */
export const VOICE_FAILURE_RATIO = 0.05;
/** ...and the window must hold at least this many operations. */
export const VOICE_FAILURE_MIN_OPS = 10;
/** Dispatch drops must grow in this many consecutive completed sample windows. */
export const DISPATCH_DROP_SAMPLES = 3;
/** Mirrors the fixed DISPATCH_LANES allowlist in crates/core/src/metrics.rs. */
export const DISPATCH_LANES: readonly string[] = ["messages", "interactions", "registry", "privileged", "busy", "reactions"];

export const RULES: readonly RuleDef[] = [
  { id: "job_stale", summary: `scheduled job has no success for more than ${STALE_INTERVALS} intervals`, runbook: "runbook.md#alert-job-stale" },
  { id: "job_consecutive_failures", summary: `scheduled job failed ${FAILURE_THRESHOLD}+ times in a row`, runbook: "runbook.md#alert-job-failures" },
  { id: "rest_429_rate", summary: `Discord REST 429s exceed ${REST_429_RATIO * 100}% of requests`, runbook: "runbook.md#alert-rest-429" },
  { id: "rest_5xx_rate", summary: `Discord REST 5xx responses exceed ${REST_5XX_RATIO * 100}% of requests`, runbook: "runbook.md#alert-rest-5xx" },
  { id: "db_pool_saturated", summary: `database pool exhausted for ${POOL_SATURATED_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-db-pool" },
  { id: "db_errors", summary: `database errors reached ${DB_ERROR_MIN_ERRORS}+ between samples`, runbook: "runbook.md#alert-db-errors" },
  { id: "send_admission_blocked", summary: `Discord sends refused admission for ${SEND_BLOCKED_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-send-admission-blocked" },
  { id: "voice_failures", summary: `voice room lifecycle failures exceed ${VOICE_FAILURE_RATIO * 100}% of operations (min ${VOICE_FAILURE_MIN_OPS} ops), or new dead-letters/orphans`, runbook: "runbook.md#alert-voice-failures" },
  { id: "gateway_missed_events", summary: `gateway missed events increased between samples`, runbook: "runbook.md#alert-gateway-missed-events" },
  { id: "gateway_unpaired_disconnect", summary: `gateway disconnect with no RESUMED or fresh READY for ${GATEWAY_UNPAIRED_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-gateway-unpaired-disconnect" },
  { id: "ticker_stale", summary: `15 s ticker has no success for more than ${TICKER_STALE_SECONDS / 60} minutes`, runbook: "runbook.md#alert-ticker-stale" },
  { id: "receiver_refusals", summary: `website-action receiver refusals for ${RECEIVER_REFUSAL_SAMPLES} consecutive samples`, runbook: "runbook.md#alert-receiver-refusals" },
  { id: "dispatch_drops", summary: `dispatch-lane drops grew for ${DISPATCH_DROP_SAMPLES} consecutive sample windows (reactions include fairness refusals; not proof of gateway packet loss)`, runbook: "runbook.md#alert-dispatch-drops", severity: "ticket" },
  { id: "job_unknown_stale", summary: `job without a cadence entry has no success for more than ${UNKNOWN_JOB_STALE_SECONDS / 3600} hours`, runbook: "runbook.md#alert-job-unknown-stale", severity: "ticket" },
  { id: "gateway_checkpoint_failures", summary: `gateway checkpoint commit failures increased between samples`, runbook: "runbook.md#alert-checkpoint-failures" },
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
  checkpointFailures: number;
  /** False until the first evaluation stores a baseline: the first sample never fires. */
  checkpointFailuresSeen: boolean;
  /** Refused `two_bot_internal_actions_total` outcomes summed by family. */
  receiverRefusals: Record<string, number>;
  /** False until the first evaluation stores a baseline: the first sample never fires. */
  receiverRefusalsSeen: boolean;
  /** Per-family consecutive windows with new refusals (sustained surge, not one probe). */
  receiverRefusalStreaks: Record<string, number>;
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
  /** Per-lane baseline; absent after a missing/invalid sample or in legacy storage. */
  dispatchDrops?: Record<string, number>;
  /** Consecutive positive deltas, capped at DISPATCH_DROP_SAMPLES. */
  dispatchDropStreaks?: Record<string, number>;
}

export const EMPTY_STATE: MetricsAlertState = { firing: [], jobErrorClasses: {}, rest429: 0, rest5xx: 0, restTotal: 0, poolStreak: 0, dbErrors: 0, sendBlocked: 0, sendBlockedStreak: 0, voiceOps: 0, voiceFailures: 0, voiceDeadLetters: 0, voiceOrphans: 0, gatewayMissed: 0, gatewayMissedSeen: false, checkpointFailures: 0, checkpointFailuresSeen: false, receiverRefusals: {}, receiverRefusalsSeen: false, receiverRefusalStreaks: {}, gatewayDisconnects: 0, gatewayRecoveries: 0, gatewayUnpairedSeen: false, gatewayUnpaired: 0, gatewayUnpairedStreak: 0, dispatchDrops: {}, dispatchDropStreaks: {} };

/** An unsuccessful scrape breaks the dispatch streak, never an existing alert. */
export function interruptDispatchDrops(state: MetricsAlertState): MetricsAlertState {
  return { ...state, dispatchDrops: {}, dispatchDropStreaks: {} };
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

type GatewayPairingFields = Pick<
  MetricsAlertState,
  "gatewayDisconnects" | "gatewayRecoveries" | "gatewayUnpairedSeen" | "gatewayUnpaired" | "gatewayUnpairedStreak"
>;

/**
 * One counter series value, or undefined when absent, duplicated, or not a
 * count. Takes the per-series pre-filter from the gauge helper below, so the
 * scrape-validation guard ("New consumed families cannot silently bypass
 * validation") covers these series: a Rust rename or malformed value fails
 * the scrape instead of silently holding the rule off. The
 * finite/nonnegative/single-row checks stay as a backstop for direct
 * `evaluateMetrics` callers.
 */
function gatewayCounter(matches: Sample[], event?: string): number | undefined {
  const scoped = event === undefined ? matches : matches.filter((s) => s.labels["event"] === event);
  if (scoped.length !== 1) return undefined;
  const value = scoped[0]!.value;
  return Number.isFinite(value) && value >= 0 ? value : undefined;
}

/** Pairs disconnects with RESUMED plus fresh READY; invalid samples hold, restarts re-baseline. */
function pairGatewayDisconnects(samples: Sample[], prev: MetricsAlertState): { firing: boolean; fields: GatewayPairingFields } {
  const carried: GatewayPairingFields = {
    gatewayDisconnects: prev.gatewayDisconnects ?? 0,
    gatewayRecoveries: prev.gatewayRecoveries ?? 0,
    gatewayUnpairedSeen: prev.gatewayUnpairedSeen ?? false,
    gatewayUnpaired: prev.gatewayUnpaired ?? 0,
    gatewayUnpairedStreak: prev.gatewayUnpairedStreak ?? 0,
  };
  const wasFiring = prev.firing.includes(GATEWAY_UNPAIRED_RULE);
  const gauge = (name: string) => samples.filter((s) => s.name === name);
  const disconnects = gatewayCounter(gauge("two_bot_gateway_disconnects_total"));
  const resumes = gatewayCounter(gauge("two_bot_gateway_resumes_total"));
  const ready = gatewayCounter(gauge("two_bot_gateway_events_total"), "READY");
  if (disconnects === undefined || resumes === undefined || ready === undefined) {
    return { firing: wasFiring, fields: carried };
  }
  const recoveries = resumes + ready;
  const baseline: GatewayPairingFields = {
    gatewayDisconnects: disconnects,
    gatewayRecoveries: recoveries,
    gatewayUnpairedSeen: true,
    gatewayUnpaired: 0,
    gatewayUnpairedStreak: 0,
  };
  if (!carried.gatewayUnpairedSeen) return { firing: false, fields: baseline };
  if (disconnects < carried.gatewayDisconnects || recoveries < carried.gatewayRecoveries) {
    return { firing: wasFiring, fields: baseline };
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
      fields: {
        gatewayDisconnects: disconnects,
        gatewayRecoveries: recoveries,
        gatewayUnpairedSeen: true,
        gatewayUnpaired: 0,
        gatewayUnpairedStreak: 0,
      },
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
    },
  };
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

  const unpaired = pairGatewayDisconnects(samples, prev);
  if (unpaired.firing) firing.push(GATEWAY_UNPAIRED_RULE);

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

  return { firing, state: { firing, jobErrorClasses, rest429, rest5xx, restTotal, poolStreak, dbErrors, sendBlocked, sendBlockedStreak, voiceOps, voiceFailures, voiceDeadLetters, voiceOrphans, gatewayMissed, gatewayMissedSeen: true, checkpointFailures, checkpointFailuresSeen: true, receiverRefusals, receiverRefusalsSeen: true, receiverRefusalStreaks, ...unpaired.fields, dispatchDrops, dispatchDropStreaks } };
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
