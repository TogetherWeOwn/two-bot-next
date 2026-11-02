#!/usr/bin/env node
// Promotion error-rate gate (staging and production). Invokes evaluatePromotionGate() from
// ../src/alert-rules.ts over a firing list and exits non-zero when the stage
// must fail, so the workflow's existing per-stage auto-rollback restores the
// recorded pre-promote snapshot. Secrets come from env only, never argv,
// and evidence carries allowlisted IDs plus the verdict, never tokens or
// raw exposition bodies.
//
// Modes, in precedence order:
// - rehearsal (--rehearsal-firing JSON): dispatch-only drill input. Judges a
//   rehearsal firing (a 5xx spike for the rollback proof) without touching
//   the network, so the fail-to-rollback chain is provable with no live
//   traffic and no new secret.
// - live (default): scrapes the configured staging or production Worker
//   origin at /ops/metrics twice with METRICS_SCRAPE_TOKEN, evaluates the
//   delta through evaluateMetrics, then gates. The destination is fenced:
//   --metrics-url must be a bare https:// origin matching one of the
//   configured staging/production Worker identities, redirects are refused,
//   and failures carry a stable code plus a JSON error receipt.
//   The scrape bearer floor (32 chars) mirrors wrangler/src/index.ts
//   and docs/runbook.md; a missing or short token is a controlled skip,
//   never a silent pass and never a failure: the gate enforces only once
//   the Operator provisions the secret (see docs/runbook-observability.md).
//   A live window with fewer REST requests than REST_5XX_MIN_REQUESTS records
//   `insufficient_data` (exit 0 with a warning), never `pass`, with the
//   judged volume in `restRequests`: a gate that cannot judge must not
//   certify the build.
// Query or parse failures in live mode exit non-zero (fail closed): a gate
// that cannot read metrics must not certify the build.
import { realpathSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { parseAlertScrape } from "../src/metrics-scrape.ts";
import {
  EMPTY_STATE,
  evaluateMetrics,
  evaluatePromotionGate,
  REST_5XX_MIN_REQUESTS,
} from "../src/alert-rules.ts";

const USER_AGENT = "two-bot-next-staging-rollout/1.0";
// Floor mirrors the Worker scrape gate (wrangler/src/index.ts) and
// docs/runbook.md: anything shorter leaves the route dark.
const MIN_SCRAPE_TOKEN_LENGTH = 32;
const DEFAULT_SAMPLE_INTERVAL_SECONDS = 75;
const DEFAULT_SCRAPE_TIMEOUT_SECONDS = 15;
const SCRAPE_ATTEMPTS = 3;
const SCRAPE_RETRY_SECONDS = 10;
const EXIT_USAGE = 2;
const EXIT_GATE = 1;
// Intake cap mirrors the Worker scrape (MAX_METRICS_BODY_BYTES in
// wrangler/src/index.ts) and the MCP container reads: every sibling
// reader refuses expositions over 64 KiB.
const MAX_EXPOSITION_BYTES = 64 * 1024;
// Closed failure vocabulary. Every refusal carries one of these codes plus
// its fixed next action; receipts and console text never carry remote
// bodies, URL credentials, the bearer or exception messages.
const FAILURE_CODES = [
  "usage_error",
  "invalid_metrics_origin",
  "invalid_timeout",
  "redirect_refused",
  "auth_failed",
  "transport_failed",
  "decode_failed",
];
const NEXT_ACTIONS = {
  usage_error: "Check the gate flags with --help and retry.",
  invalid_metrics_origin: "Set --metrics-url to the configured staging or production Worker origin and retry.",
  invalid_timeout: "Set --scrape-timeout-seconds to a finite positive value within the supported range and retry.",
  redirect_refused: "Verify the Worker serves /ops/metrics directly without redirects and retry.",
  auth_failed: "Verify METRICS_SCRAPE_TOKEN is current and retry.",
  transport_failed: "Verify the Worker is reachable and retry.",
  decode_failed: "Verify the Worker serves valid exposition at /ops/metrics and retry.",
};
const INPUT_REFUSAL_CODES = new Set(["usage_error", "invalid_metrics_origin", "invalid_timeout"]);
const REDIRECT_REFUSED_MESSAGE =
  "metrics scrape failed: redirect refused; the scrape bearer is never followed across redirects";
const AUTH_FAILED_MESSAGE = "metrics scrape failed: the Worker refused the scrape credentials";
const TRANSPORT_FAILED_MESSAGE = `metrics scrape failed after ${SCRAPE_ATTEMPTS} attempts: transport error`;
const DECODE_FAILED_MESSAGE = "metrics scrape failed: exposition could not be decoded";
const ORIGIN_REFUSED_MESSAGE =
  "metrics origin refused: expected a bare https:// Worker origin from the configured staging/production identities";
const TIMEOUT_REFUSED_MESSAGE = "scrape timeout refused: expected a finite positive timeout within the supported range";

class UsageError extends Error {
  constructor(message, code = "usage_error") {
    super(message);
    this.name = "UsageError";
    this.code = code;
  }
}
class ScrapeError extends Error {
  constructor(message, code = "transport_failed") {
    super(message);
    this.name = "ScrapeError";
    this.code = code;
  }
}

/**
 * True when `value` is a bare https:// origin: no userinfo, no explicit
 * port, no path beyond an optional trailing slash, no query or fragment.
 * The check never echoes the input; callers report a fixed refusal.
 */
function isBareHttpsOrigin(value) {
  if (typeof value !== "string" || value === "") return false;
  let url;
  try {
    url = new URL(value);
  } catch {
    return false;
  }
  if (url.protocol !== "https:") return false;
  if (url.username !== "" || url.password !== "") return false;
  if (url.port !== "") return false;
  if (url.pathname !== "/" && url.pathname !== "") return false;
  if (url.search !== "" || url.hash !== "") return false;
  if (!url.hostname) return false;
  return true;
}

/** Canonical origin (`https://host`) for a bare origin, else null. */
export function originOf(value) {
  if (!isBareHttpsOrigin(value)) return null;
  try {
    return new URL(value).origin;
  } catch {
    return null;
  }
}

// Largest delay the timer behind AbortSignal.timeout honors: above 2^31 - 1
// ms Node clamps the timer, so the scrape would abort almost at once instead
// of waiting out the configured timeout. The gate refuses such values as
// invalid_timeout before any signal or network work.
const MAX_TIMER_MS = 2_147_483_647;

/** Whole-millisecond delay for a timeout in seconds; ceiling so sub-ms values still wait. */
function timeoutMs(seconds) {
  return Math.ceil(seconds * 1000);
}

function isSupportedTimeout(value) {
  if (!Number.isFinite(value) || value <= 0) return false;
  const ms = timeoutMs(value);
  return ms >= 1 && ms <= MAX_TIMER_MS;
}

/**
 * Normalize configured Worker identities to a set of canonical origins.
 * Malformed entries are ignored, never fatal: an empty set refuses every
 * live destination, which is the fail-closed posture.
 */
function normalizeAllowedOrigins(entries) {
  const allowed = new Set();
  if (!Array.isArray(entries)) return allowed;
  for (const entry of entries) {
    const origin = originOf(entry);
    if (origin) allowed.add(origin);
  }
  return allowed;
}

/**
 * Worker identities the gate may scrape, from the repository's existing
 * staging/production origin configuration. The workflow passes the same
 * values as STAGING_URL / PRODUCTION_URL for the gate step, so both
 * spellings authorize the identical origin; nothing else does.
 */
export function allowedOriginsFromEnv(env) {
  return normalizeAllowedOrigins([
    env.STAGING_WORKER_URL,
    env.PRODUCTION_WORKER_URL,
    env.STAGING_URL,
    env.PRODUCTION_URL,
  ]);
}

function nextActionFor(code) {
  return NEXT_ACTIONS[code] ?? NEXT_ACTIONS.usage_error;
}

function codeOf(error, fallback = "transport_failed") {
  const code = error instanceof Error ? error.code : undefined;
  return typeof code === "string" && FAILURE_CODES.includes(code) ? code : fallback;
}

function isRedirectFailure(error) {
  const message = error instanceof Error ? error.message : String(error ?? "");
  if (/redirect/i.test(message)) return true;
  const cause = error instanceof Error ? error.cause : undefined;
  if (cause instanceof Error && /redirect/i.test(cause.message)) return true;
  if (typeof cause === "string" && /redirect/i.test(cause)) return true;
  return false;
}

function helpText() {
  return `promotion error-rate gate: fence the scrape destination, judge the 5xx rate, fail closed

Usage:
  node wrangler/scripts/promotion-error-gate.mjs --metrics-url <origin> --evidence <path> [--sha <sha>]
    [--rehearsal-firing <json>] [--sample-interval-seconds <n>] [--scrape-timeout-seconds <n>]
    [--user-agent <name>]
  node wrangler/scripts/promotion-error-gate.mjs --help

Options:
  --metrics-url <origin>          Bare https:// Worker origin from the configured
                                  staging/production identities. No userinfo, port,
                                  path, query or fragment. Anything else is refused
                                  with zero fetches.
  --evidence <path>               JSON evidence receipt to write, including for failures.
  --sha <sha>                     Build under test (default: unknown).
  --rehearsal-firing <json>       Offline drill input: judge a JSON array of rule names
                                  with no network and no token.
  --sample-interval-seconds <n>   Seconds between live scrapes (default: 75).
  --scrape-timeout-seconds <n>    Per-scrape timeout in seconds: finite, positive, and
                                  within the supported timer range (default: 15).
  --user-agent <name>             Scrape user agent (default: ${USER_AGENT}).
  --help                          Print this text and exit 0. No token, no network.

Failure codes (receipt verdict "error", never a pass):
  usage_error             Bad flags. ${NEXT_ACTIONS.usage_error}
  invalid_metrics_origin  Destination refused. ${NEXT_ACTIONS.invalid_metrics_origin}
  invalid_timeout         Timeout refused. ${NEXT_ACTIONS.invalid_timeout}
  redirect_refused        Redirect refused. ${NEXT_ACTIONS.redirect_refused} Exit 1.
  auth_failed             Credentials refused (401/403). ${NEXT_ACTIONS.auth_failed} Exit 1.
  transport_failed        Scrape transport failed after ${SCRAPE_ATTEMPTS} attempts. ${NEXT_ACTIONS.transport_failed} Exit 1.
  decode_failed           Exposition not decodable. ${NEXT_ACTIONS.decode_failed} Exit 1.

Offline rehearsal (no network, no token):
  node wrangler/scripts/promotion-error-gate.mjs --metrics-url https://two-bot-next-staging.5150.workers.dev --evidence /tmp/gate-evidence.json --rehearsal-firing '[]'
`;
}

/**
 * Bounded exposition read: content-length precheck plus an incremental
 * streaming cap, so an over-cap body never buffers fully. Returns null
 * when the body exceeds `limit`. Minimal fetch stubs without a body
 * stream fall back to text() with a post-read byte check.
 */
async function readBoundedExposition(response, limit) {
  const declared = response.headers?.get?.("content-length");
  if (declared !== undefined && declared !== null) {
    const parsed = Number(declared);
    if (Number.isFinite(parsed) && parsed > limit) return null;
  }
  const reader = response.body?.getReader?.();
  if (!reader) {
    const text = await response.text();
    return Buffer.byteLength(text, "utf8") > limit ? null : text;
  }
  const chunks = [];
  let size = 0;
  let complete = false;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        complete = true;
        break;
      }
      size += value.byteLength;
      if (size > limit) return null;
      chunks.push(value);
    }
    const bytes = new Uint8Array(size);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return new TextDecoder().decode(bytes);
  } finally {
    // A hostile cancel hook can itself stall or reject. Do not await it.
    if (!complete) void reader.cancel().catch(() => {});
    try {
      reader.releaseLock();
    } catch {
      // Some runtimes cannot release a lock while a read is pending.
    }
  }
}

/**
 * Firing verdict over an evaluateMetrics firing list. `fail` true means the
 * promotion stage must fail so auto-rollback runs. Thin wrapper so the
 * workflow judges through the same helper the unit tests pin.
 */
export function judgeFiring(firing) {
  const verdict = evaluatePromotionGate(firing);
  return { fail: verdict.fail, reasons: [...verdict.reasons] };
}

/** Parse and validate a rehearsal firing list; malformed input fails closed. */
export function parseRehearsalFiring(raw) {
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch {
    throw new UsageError("rehearsal firing is not valid JSON", "usage_error");
  }
  if (!Array.isArray(parsed) || !parsed.every((entry) => typeof entry === "string")) {
    throw new UsageError("rehearsal firing must be a JSON array of strings", "usage_error");
  }
  return parsed;
}

/** Pick the gate mode. Rehearsal wins; live needs a full-length token. */
export function decideMode({ rehearsalFiring, token }) {
  if (rehearsalFiring !== undefined) return { mode: "rehearsal" };
  if (typeof token === "string" && token.length >= MIN_SCRAPE_TOKEN_LENGTH) return { mode: "live" };
  return { mode: "skip", reason: "metrics scrape token absent: gate not yet enforcing (see docs/runbook-observability.md)" };
}

export async function fetchExposition({ metricsUrl, token, timeoutSeconds, fetchImpl, retrySleepSeconds = SCRAPE_RETRY_SECONDS, userAgent = USER_AGENT }) {
  const url = `${metricsUrl.replace(/\/$/, "")}/ops/metrics`;
  const delayMs = timeoutMs(timeoutSeconds);
  for (let attempt = 1; attempt <= SCRAPE_ATTEMPTS; attempt += 1) {
    let response;
    try {
      response = await fetchImpl(url, {
        headers: { authorization: `Bearer ${token}`, "user-agent": userAgent },
        signal: AbortSignal.timeout(delayMs),
        redirect: "error",
      });
    } catch (error) {
      if (isRedirectFailure(error)) throw new ScrapeError(REDIRECT_REFUSED_MESSAGE, "redirect_refused");
      if (attempt < SCRAPE_ATTEMPTS) {
        await new Promise((resolve) => setTimeout(resolve, retrySleepSeconds * 1000));
        continue;
      }
      throw new ScrapeError(TRANSPORT_FAILED_MESSAGE, "transport_failed");
    }
    if (response.status >= 300 && response.status < 400) {
      throw new ScrapeError(REDIRECT_REFUSED_MESSAGE, "redirect_refused");
    }
    if (response.status === 401 || response.status === 403) {
      throw new ScrapeError(AUTH_FAILED_MESSAGE, "auth_failed");
    }
    if (!response.ok) {
      // Only 5xx and 429 are transient. 429 is the endpoint's own designed
      // refusal: /ops/metrics sheds a momentarily exhausted per-caller scrape
      // bucket with `slow down` plus retry-after, expecting a correct bearer
      // to wait it out (wrangler/src/index.ts). The fixed retry sleep already
      // exceeds the refill, so the bounded retry recovers it. Every other 4xx
      // is refused at once: a retry cannot fix a client error, and must not
      // re-send the bearer against a request the Worker already refused.
      if ((response.status === 429 || response.status >= 500) && attempt < SCRAPE_ATTEMPTS) {
        await new Promise((resolve) => setTimeout(resolve, retrySleepSeconds * 1000));
        continue;
      }
      throw new ScrapeError(TRANSPORT_FAILED_MESSAGE, "transport_failed");
    }
    let text;
    try {
      text = await readBoundedExposition(response, MAX_EXPOSITION_BYTES);
    } catch {
      // Body-phase transients (stalls, resets, the timeout firing mid-read)
      // keep the same bounded retry as connect-phase ones.
      if (attempt < SCRAPE_ATTEMPTS) {
        await new Promise((resolve) => setTimeout(resolve, retrySleepSeconds * 1000));
        continue;
      }
      throw new ScrapeError(TRANSPORT_FAILED_MESSAGE, "transport_failed");
    }
    // Over-cap is refused at once, like any other deterministic refusal: a
    // retry re-sends the bearer against a body that will not shrink, so it
    // must not loop. The code stays decode_failed with fixed text: the
    // exposition could not be admitted for decoding.
    if (text === null) throw new ScrapeError(DECODE_FAILED_MESSAGE, "decode_failed");
    const samples = parseAlertScrape(text);
    if (samples === null) throw new ScrapeError(DECODE_FAILED_MESSAGE, "decode_failed");
    return { text, samples };
  }
  throw new ScrapeError(TRANSPORT_FAILED_MESSAGE, "transport_failed");
}

/**
 * Live gate over two scrapes (delta mirrors the keepalive evaluation).
 * Deps are injectable so tests prove verdicts without the network.
 *
 * `restDelta` is the judged REST request volume between the two scrapes
 * (second minus baseline counters). A negative delta means the counters
 * reset between scrapes (process restart): the window cannot judge, same
 * as a too-quiet one.
 */
export async function runLiveGate({
  metricsUrl,
  token,
  sampleIntervalSeconds = DEFAULT_SAMPLE_INTERVAL_SECONDS,
  timeoutSeconds = DEFAULT_SCRAPE_TIMEOUT_SECONDS,
  fetchImpl = fetch,
  sleep = (s) => new Promise((resolve) => setTimeout(resolve, s * 1000)),
  nowSeconds = () => Date.now() / 1000,
  retrySleepSeconds = SCRAPE_RETRY_SECONDS,
  userAgent = USER_AGENT,
  allowedMetricsOrigins,
}) {
  if (!isSupportedTimeout(timeoutSeconds)) throw new UsageError(TIMEOUT_REFUSED_MESSAGE, "invalid_timeout");
  if (!isBareHttpsOrigin(metricsUrl)) throw new UsageError(ORIGIN_REFUSED_MESSAGE, "invalid_metrics_origin");
  if (allowedMetricsOrigins !== undefined) {
    const allowed = normalizeAllowedOrigins(allowedMetricsOrigins);
    const actual = originOf(metricsUrl);
    if (!actual || !allowed.has(actual)) throw new UsageError(ORIGIN_REFUSED_MESSAGE, "invalid_metrics_origin");
  }
  const scrape = () => fetchExposition({ metricsUrl, token, timeoutSeconds, fetchImpl, retrySleepSeconds, userAgent });
  const first = await scrape();
  const baseline = evaluateMetrics(first.samples, EMPTY_STATE, nowSeconds());
  await sleep(sampleIntervalSeconds);
  const second = await scrape();
  const evaluation = evaluateMetrics(second.samples, baseline.state, nowSeconds());
  return {
    firing: evaluation.firing,
    sampleCount: second.samples.length,
    restTotal: evaluation.state.restTotal,
    restDelta: evaluation.state.restTotal - baseline.state.restTotal,
  };
}

/**
 * Map a live firing list plus judged request volume to a recorded verdict.
 * A window with fewer REST requests than REST_5XX_MIN_REQUESTS cannot judge
 * the 5xx share, so it records `insufficient_data` (exit 0 with a warning),
 * never `pass`: a too-quiet window must not read as a healthy one, and the
 * runbook accepts only a `pass` with enough requests as enforcement proof.
 */
export function decideLiveVerdict({ firing, restDelta }) {
  const { fail, reasons } = judgeFiring(firing);
  if (fail) return { fail: true, verdict: "fail", reasons };
  if (restDelta < REST_5XX_MIN_REQUESTS) return { fail: false, verdict: "insufficient_data", reasons: [] };
  return { fail: false, verdict: "pass", reasons: [] };
}

export function parseArgs(argv) {
  const config = {
    metricsUrl: "",
    evidence: "",
    sha: "unknown",
    rehearsalFiring: undefined,
    sampleIntervalSeconds: DEFAULT_SAMPLE_INTERVAL_SECONDS,
    timeoutSeconds: DEFAULT_SCRAPE_TIMEOUT_SECONDS,
    userAgent: USER_AGENT,
  };
  let hasMetricsUrl = false;
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i];
    const value = argv[i + 1];
    if (value === undefined) throw new UsageError(`flag ${flag} needs a value`, "usage_error");
    if (flag === "--metrics-url") {
      config.metricsUrl = value;
      hasMetricsUrl = true;
    } else if (flag === "--evidence") config.evidence = value;
    else if (flag === "--sha") config.sha = value;
    else if (flag === "--rehearsal-firing") config.rehearsalFiring = value;
    else if (flag === "--sample-interval-seconds") config.sampleIntervalSeconds = Number(value);
    else if (flag === "--scrape-timeout-seconds") config.timeoutSeconds = Number(value);
    else if (flag === "--user-agent") config.userAgent = value;
    else throw new UsageError(`unknown flag ${flag}`, "usage_error");
    i += 1;
  }
  if (!hasMetricsUrl) throw new UsageError("--metrics-url is required", "usage_error");
  if (!isBareHttpsOrigin(config.metricsUrl)) throw new UsageError(ORIGIN_REFUSED_MESSAGE, "invalid_metrics_origin");
  if (!config.evidence) throw new UsageError("--evidence is required", "usage_error");
  if (!Number.isFinite(config.sampleIntervalSeconds) || config.sampleIntervalSeconds <= 0) {
    throw new UsageError("--sample-interval-seconds must be a positive number", "usage_error");
  }
  if (!isSupportedTimeout(config.timeoutSeconds)) {
    throw new UsageError(TIMEOUT_REFUSED_MESSAGE, "invalid_timeout");
  }
  if (!config.userAgent) {
    throw new UsageError("--user-agent must not be empty", "usage_error");
  }
  return config;
}

function writeEvidence(path, evidence) {
  writeFileSync(path, `${JSON.stringify(evidence)}\n`, "utf8");
}

function tryWriteEvidence(path, evidence) {
  if (typeof path !== "string" || path === "") return false;
  try {
    writeEvidence(path, evidence);
    return true;
  } catch {
    return false;
  }
}

function prescanFlag(argv, flag) {
  const index = argv.indexOf(flag);
  if (index < 0 || index + 1 >= argv.length) return undefined;
  return argv[index + 1];
}

function reportFailure(code) {
  console.log(`::error::promotion error-rate gate ${code}: ${nextActionFor(code)}`);
}

/**
 * True when --help/-h appears in flag position. Every gate flag takes a
 * value, so flags sit at even indices; a help token in a value slot (for
 * example `--user-agent --help` from a templated variable) is data, not a
 * help request, and must never exit 0 with no evidence.
 */
function wantsHelp(argv) {
  for (let i = 0; i < argv.length; i += 2) {
    if (argv[i] === "--help" || argv[i] === "-h") return true;
  }
  return false;
}

/**
 * CLI entry with injectable deps for offline tests. Returns the process
 * exit code: 0 pass/skip, 1 gate or scrape failure, 2 usage/input refusal.
 * Failures write a deterministic fail-closed error receipt when the
 * evidence path is usable; the console always carries the stable code.
 */
export async function runCli(argv, env, deps = {}) {
  if (wantsHelp(argv)) {
    console.log(helpText());
    return 0;
  }
  const {
    fetchImpl = fetch,
    sleep = (s) => new Promise((resolve) => setTimeout(resolve, s * 1000)),
    nowSeconds = () => Date.now() / 1000,
    retrySleepSeconds = SCRAPE_RETRY_SECONDS,
  } = deps;
  let config;
  try {
    config = parseArgs(argv);
  } catch (error) {
    const code = codeOf(error, "usage_error");
    const receipt = {
      mode: "live",
      sha: prescanFlag(argv, "--sha") ?? "unknown",
      firing: [],
      verdict: "error",
      code,
      nextAction: nextActionFor(code),
    };
    tryWriteEvidence(prescanFlag(argv, "--evidence"), receipt);
    reportFailure(code);
    return EXIT_USAGE;
  }
  const { mode, reason } = decideMode({ rehearsalFiring: config.rehearsalFiring, token: env.METRICS_SCRAPE_TOKEN });
  if (mode === "skip") {
    const evidence = { mode, sha: config.sha, firing: [], verdict: "skip", reasons: [], detail: reason };
    writeEvidence(config.evidence, evidence);
    console.log(`::warning::promotion error-rate gate skipped: ${reason}`);
    console.log(JSON.stringify(evidence));
    return 0;
  }
  if (mode === "rehearsal") {
    let firing;
    try {
      firing = parseRehearsalFiring(config.rehearsalFiring);
    } catch (error) {
      const code = codeOf(error, "usage_error");
      tryWriteEvidence(config.evidence, {
        mode,
        sha: config.sha,
        firing: [],
        verdict: "error",
        code,
        nextAction: nextActionFor(code),
      });
      reportFailure(code);
      return EXIT_USAGE;
    }
    const { fail, reasons } = judgeFiring(firing);
    const evidence = {
      mode,
      sha: config.sha,
      firing,
      verdict: fail ? "fail" : "pass",
      reasons,
      detail: "rehearsal firing from the dispatch-only drill input, not live metrics",
    };
    writeEvidence(config.evidence, evidence);
    console.log(JSON.stringify(evidence));
    if (fail) {
      console.log(`::error::promotion error-rate gate failed (rehearsal): ${reasons.join(",")}`);
      return 1;
    }
    return 0;
  }
  let live;
  try {
    live = await runLiveGate({
      metricsUrl: config.metricsUrl,
      token: env.METRICS_SCRAPE_TOKEN,
      sampleIntervalSeconds: config.sampleIntervalSeconds,
      timeoutSeconds: config.timeoutSeconds,
      userAgent: config.userAgent,
      fetchImpl,
      sleep,
      nowSeconds,
      retrySleepSeconds,
      allowedMetricsOrigins: [...allowedOriginsFromEnv(env)],
    });
  } catch (error) {
    const code = codeOf(error);
    tryWriteEvidence(config.evidence, {
      mode,
      sha: config.sha,
      firing: [],
      verdict: "error",
      code,
      nextAction: nextActionFor(code),
    });
    reportFailure(code);
    return INPUT_REFUSAL_CODES.has(code) ? EXIT_USAGE : EXIT_GATE;
  }
  const { fail, verdict, reasons } = decideLiveVerdict({ firing: live.firing, restDelta: live.restDelta });
  const evidence = {
    mode,
    sha: config.sha,
    firing: live.firing,
    verdict,
    reasons,
    restRequests: live.restDelta,
    detail: `live delta over ${config.sampleIntervalSeconds}s (rest_requests=${live.restDelta}, samples=${live.sampleCount})`,
  };
  writeEvidence(config.evidence, evidence);
  console.log(JSON.stringify(evidence));
  if (fail) {
    console.log(`::error::promotion error-rate gate failed: ${reasons.join(",")}`);
    return 1;
  }
  if (verdict === "insufficient_data") {
    console.log(
      `::warning::promotion error-rate gate could not judge: only ${live.restDelta} REST requests in the window (minimum ${REST_5XX_MIN_REQUESTS}); not a pass`,
    );
  }
  return 0;
}

/**
 * True when this module is the executed script rather than an import.
 * Compares canonical paths (never argv string shape): `process.argv[1]` is
 * absolute on some Node versions and cwd-relative on others, so a string
 * comparison silently no-ops the gate on one of them.
 */
export function isDirectInvocation(argv1, moduleUrl) {
  try {
    if (typeof argv1 !== "string" || argv1 === "") return false;
    return realpathSync(argv1) === realpathSync(fileURLToPath(moduleUrl));
  } catch {
    return false;
  }
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  runCli(process.argv.slice(2), process.env).then(
    (code) => process.exit(code),
    () => {
      console.log("::error::promotion error-rate gate failed closed: unexpected error");
      process.exit(1);
    },
  );
}
