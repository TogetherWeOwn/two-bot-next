#!/usr/bin/env node
// Staging promotion error-rate gate. Invokes evaluatePromotionGate() from
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
// - live (default): scrapes staging /ops/metrics twice with
//   METRICS_SCRAPE_TOKEN, evaluates the delta through evaluateMetrics, then
//   gates. The scrape bearer floor (32 chars) mirrors wrangler/src/index.ts
//   and docs/runbook.md; a missing or short token is a controlled skip,
//   never a silent pass and never a failure: the gate enforces only once
//   the Operator provisions the secret (see docs/runbook-observability.md).
// Query or parse failures in live mode exit non-zero (fail closed): a gate
// that cannot read metrics must not certify the build.
import { realpathSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { parseAlertScrape } from "../src/metrics-scrape.ts";
import {
  EMPTY_STATE,
  evaluateMetrics,
  evaluatePromotionGate,
} from "../src/alert-rules.ts";

const USER_AGENT = "two-bot-next-staging-rollout/1.0";
// Floor mirrors the Worker scrape gate (wrangler/src/index.ts) and
// docs/runbook.md: anything shorter leaves the route dark.
const MIN_SCRAPE_TOKEN_LENGTH = 32;
const DEFAULT_SAMPLE_INTERVAL_SECONDS = 75;
const DEFAULT_SCRAPE_TIMEOUT_SECONDS = 15;
const SCRAPE_ATTEMPTS = 3;
const SCRAPE_RETRY_SECONDS = 10;

class UsageError extends Error {}
class ScrapeError extends Error {}

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
    throw new UsageError("rehearsal firing is not valid JSON");
  }
  if (!Array.isArray(parsed) || !parsed.every((entry) => typeof entry === "string")) {
    throw new UsageError("rehearsal firing must be a JSON array of strings");
  }
  return parsed;
}

/** Pick the gate mode. Rehearsal wins; live needs a full-length token. */
export function decideMode({ rehearsalFiring, token }) {
  if (rehearsalFiring !== undefined) return { mode: "rehearsal" };
  if (typeof token === "string" && token.length >= MIN_SCRAPE_TOKEN_LENGTH) return { mode: "live" };
  return { mode: "skip", reason: "metrics scrape token absent: gate not yet enforcing (see docs/runbook-observability.md)" };
}

async function fetchExposition({ metricsUrl, token, timeoutSeconds, fetchImpl, retrySleepSeconds = SCRAPE_RETRY_SECONDS }) {
  const url = `${metricsUrl.replace(/\/$/, "")}/ops/metrics`;
  let lastError;
  for (let attempt = 1; attempt <= SCRAPE_ATTEMPTS; attempt += 1) {
    try {
      const response = await fetchImpl(url, {
        headers: { authorization: `Bearer ${token}`, "user-agent": USER_AGENT },
        signal: AbortSignal.timeout(timeoutSeconds * 1000),
      });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const text = await response.text();
      const samples = parseAlertScrape(text);
      if (samples === null) throw new Error("unparseable exposition");
      return { text, samples };
    } catch (error) {
      lastError = error;
      if (attempt < SCRAPE_ATTEMPTS) await new Promise((resolve) => setTimeout(resolve, retrySleepSeconds * 1000));
    }
  }
  throw new ScrapeError(`metrics scrape failed after ${SCRAPE_ATTEMPTS} attempts: ${lastError instanceof Error ? lastError.message : lastError}`);
}

/**
 * Live gate over two scrapes (delta mirrors the keepalive evaluation).
 * Deps are injectable so tests prove verdicts without the network.
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
}) {
  const scrape = () => fetchExposition({ metricsUrl, token, timeoutSeconds, fetchImpl, retrySleepSeconds });
  const first = await scrape();
  const baseline = evaluateMetrics(first.samples, EMPTY_STATE, nowSeconds());
  await sleep(sampleIntervalSeconds);
  const second = await scrape();
  const evaluation = evaluateMetrics(second.samples, baseline.state, nowSeconds());
  return {
    firing: evaluation.firing,
    sampleCount: second.samples.length,
    restTotal: evaluation.state.restTotal,
  };
}

function parseArgs(argv) {
  const config = {
    metricsUrl: "",
    evidence: "",
    sha: "unknown",
    rehearsalFiring: undefined,
    sampleIntervalSeconds: DEFAULT_SAMPLE_INTERVAL_SECONDS,
    timeoutSeconds: DEFAULT_SCRAPE_TIMEOUT_SECONDS,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i];
    const value = argv[i + 1];
    if (value === undefined) throw new UsageError(`flag ${flag} needs a value`);
    if (flag === "--metrics-url") config.metricsUrl = value;
    else if (flag === "--evidence") config.evidence = value;
    else if (flag === "--sha") config.sha = value;
    else if (flag === "--rehearsal-firing") config.rehearsalFiring = value;
    else if (flag === "--sample-interval-seconds") config.sampleIntervalSeconds = Number(value);
    else if (flag === "--scrape-timeout-seconds") config.timeoutSeconds = Number(value);
    else throw new UsageError(`unknown flag ${flag}`);
    i += 1;
  }
  if (!config.metricsUrl) throw new UsageError("--metrics-url is required");
  if (!config.evidence) throw new UsageError("--evidence is required");
  if (!Number.isFinite(config.sampleIntervalSeconds) || config.sampleIntervalSeconds <= 0) {
    throw new UsageError("--sample-interval-seconds must be a positive number");
  }
  return config;
}

function writeEvidence(path, evidence) {
  writeFileSync(path, `${JSON.stringify(evidence)}\n`, "utf8");
}

async function main(argv, env) {
  const config = parseArgs(argv);
  const { mode, reason } = decideMode({ rehearsalFiring: config.rehearsalFiring, token: env.METRICS_SCRAPE_TOKEN });
  if (mode === "skip") {
    const evidence = { mode, sha: config.sha, firing: [], verdict: "skip", reasons: [], detail: reason };
    writeEvidence(config.evidence, evidence);
    console.log(`::warning::promotion error-rate gate skipped: ${reason}`);
    console.log(JSON.stringify(evidence));
    return 0;
  }
  if (mode === "rehearsal") {
    const firing = parseRehearsalFiring(config.rehearsalFiring);
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
  const live = await runLiveGate({
    metricsUrl: config.metricsUrl,
    token: env.METRICS_SCRAPE_TOKEN,
    sampleIntervalSeconds: config.sampleIntervalSeconds,
    timeoutSeconds: config.timeoutSeconds,
  });
  const { fail, reasons } = judgeFiring(live.firing);
  const evidence = {
    mode,
    sha: config.sha,
    firing: live.firing,
    verdict: fail ? "fail" : "pass",
    reasons,
    detail: `live delta over ${config.sampleIntervalSeconds}s (samples=${live.sampleCount})`,
  };
  writeEvidence(config.evidence, evidence);
  console.log(JSON.stringify(evidence));
  if (fail) {
    console.log(`::error::promotion error-rate gate failed: ${reasons.join(",")}`);
    return 1;
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
  main(process.argv.slice(2), process.env).then(
    (code) => process.exit(code),
    (error) => {
      if (error instanceof UsageError) {
        console.log(`::error::promotion error-rate gate misconfigured: ${error.message}`);
        process.exit(2);
      }
      console.log(`::error::promotion error-rate gate failed closed: ${error instanceof Error ? error.message : error}`);
      process.exit(1);
    },
  );
}
