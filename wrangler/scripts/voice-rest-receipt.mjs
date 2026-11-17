import { countersFrom } from "./rest-5xx-counters.mjs";

export const SCHEMA = "voice-rest-measurement-v1";
const UNLABELED = "unlabeled";
const LABEL_RE = /^[a-z0-9_]{1,24}$/;
const MAX_LABELS = 16;
const SHA_RE = /^[0-9a-f]{40}$/;
const DEPLOYMENT_RE = /^[A-Za-z0-9._:-]{1,128}$/;
const RUN_RE = /^[1-9][0-9]{0,18}$/;
const ATTEMPT_RE = /^[1-9][0-9]{0,5}$/;
const INCARNATION_RE = /^[0-9a-f]{32}$/;
const UTC_RE = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,3})?Z$/;
export class MeasurementError extends Error {
  constructor(code) {
    super(code);
    this.code = code;
  }
}

function fail(code) {
  throw new MeasurementError(code);
}

/** Canonical UTC instant (ms, Z) for a strict ISO string, else null. */
function utcMs(value) {
  if (typeof value !== "string" || !UTC_RE.test(value)) return null;
  const ms = Date.parse(value);
  return Number.isFinite(ms) ? ms : null;
}

const isCount = (n) => Number.isSafeInteger(n) && n >= 0;

/** Validate the original grant metadata; refuses with a fixed code, never echoes input. */
export function validateMetadata(meta) {
  if (meta === null || typeof meta !== "object") fail("metadata_invalid");
  const { runId, runAttempt, sha, deploymentId, expiresAt } = meta;
  if (typeof runId !== "string" || !RUN_RE.test(runId)) fail("run_id_invalid");
  if (typeof runAttempt !== "string" || !ATTEMPT_RE.test(runAttempt)) fail("run_attempt_invalid");
  if (typeof sha !== "string" || !SHA_RE.test(sha)) fail("sha_invalid");
  if (typeof deploymentId !== "string" || !DEPLOYMENT_RE.test(deploymentId)) fail("deployment_invalid");
  if (utcMs(expiresAt) === null) fail("expiry_invalid");
  return { runId, runAttempt, sha, deploymentId, expiresAt };
}

/**
 * Cumulative count per sanitized `result` label from parsed samples. Samples
 * without a label are kept under `unlabeled` so the labels always sum to the
 * total countersFrom() reports. A label outside [a-z0-9_]{1,24} is refused
 * rather than echoed.
 */
export function resultCountsFrom(samples) {
  const results = Object.create(null);
  let seen = 0;
  for (const s of samples) {
    if (s.name !== "two_bot_rest_requests_total") continue;
    seen += 1;
    const raw = s.labels?.result;
    const label = raw === undefined ? UNLABELED : raw;
    if (typeof label !== "string" || !LABEL_RE.test(label)) fail("result_label_invalid");
    if (!isCount(s.value)) fail("invalid_rest_counters");
    results[label] = (results[label] ?? 0) + s.value;
    if (!isCount(results[label])) fail("invalid_rest_counters");
  }
  if (seen === 0) fail("rest_samples_missing");
  if (Object.keys(results).length > MAX_LABELS) fail("result_label_invalid");
  return Object.fromEntries(Object.entries(results).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)));
}

/** Build a receipt from parsed samples. Throws MeasurementError on any invalid input. */
export function buildReceipt({ samples, metadata, now = () => new Date() }) {
  const meta = validateMetadata(metadata);
  if (!Array.isArray(samples) || samples.length === 0) fail("samples_missing");
  const capturedMs = now().getTime();
  if (!Number.isFinite(capturedMs)) fail("clock_invalid");
  if (capturedMs >= utcMs(meta.expiresAt)) fail("grant_expired");
  const results = resultCountsFrom(samples);
  let counters;
  try {
    counters = countersFrom(samples);
  } catch {
    fail("invalid_rest_counters");
  }
  if (counters.incarnation === undefined) fail("identity_missing");
  const sum = Object.values(results).reduce((a, b) => a + b, 0);
  if (sum !== counters.restTotal || (results["5xx"] ?? 0) !== counters.rest5xx) {
    fail("counter_sum_mismatch");
  }
  return {
    schema: SCHEMA,
    available: true,
    code: "captured",
    ...meta,
    capturedAt: new Date(capturedMs).toISOString(),
    restTotal: counters.restTotal,
    rest5xx: counters.rest5xx,
    incarnation: counters.incarnation,
    results,
  };
}
