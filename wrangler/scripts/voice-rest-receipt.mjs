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
const RECEIPT_KEYS = [
  "schema", "available", "code", "runId", "runAttempt", "capturedAt", "sha",
  "deploymentId", "expiresAt", "restTotal", "rest5xx", "incarnation", "results",
];
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
  if (!Number.isFinite(ms)) return null;
  const normalized = value.replace(/(?:\.(\d{1,3}))?Z$/, (_, fraction = "") => `.${fraction.padEnd(3, "0")}Z`);
  return new Date(ms).toISOString() === normalized ? ms : null;
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

/** Re-validate a persisted receipt, refusing unexpected or inconsistent data. */
export function validateReceipt(receipt) {
  if (receipt === null || typeof receipt !== "object" || Array.isArray(receipt)) fail("receipt_invalid");
  const keys = Object.keys(receipt);
  if (keys.length !== RECEIPT_KEYS.length || !RECEIPT_KEYS.every((k) => keys.includes(k))) fail("receipt_invalid");
  if (receipt.schema !== SCHEMA) fail("receipt_invalid");
  if (receipt.available !== true || receipt.code !== "captured") fail("receipt_unavailable");
  validateMetadata(receipt);
  if (utcMs(receipt.capturedAt) === null) fail("receipt_invalid");
  if (!isCount(receipt.restTotal) || !isCount(receipt.rest5xx)) fail("receipt_invalid");
  if (typeof receipt.incarnation !== "string" || !INCARNATION_RE.test(receipt.incarnation)
    || receipt.incarnation === "0".repeat(32)) fail("identity_missing");
  const r = receipt.results;
  if (r === null || typeof r !== "object" || Array.isArray(r)) fail("receipt_invalid");
  const labels = Object.keys(r);
  if (labels.length === 0 || labels.length > MAX_LABELS) fail("receipt_invalid");
  let sum = 0;
  for (const label of labels) {
    if (!LABEL_RE.test(label) || !isCount(r[label])) fail("receipt_invalid");
    sum += r[label];
    if (!isCount(sum)) fail("receipt_invalid");
  }
  if (sum !== receipt.restTotal || (r["5xx"] ?? 0) !== receipt.rest5xx) fail("counter_sum_mismatch");
  return receipt;
}

/** Compare same-grant, same-process receipts without accepting resets or expiry. */
export function compareReceipts(beforeRaw, afterRaw, { now = () => new Date() } = {}) {
  const before = validateReceipt(beforeRaw);
  const after = validateReceipt(afterRaw);
  if (before.runId !== after.runId || before.runAttempt !== after.runAttempt
    || before.sha !== after.sha || before.deploymentId !== after.deploymentId || before.expiresAt !== after.expiresAt) {
    fail("metadata_changed");
  }
  if (before.incarnation !== after.incarnation) fail("incarnation_changed");
  const expiryMs = utcMs(after.expiresAt);
  if (utcMs(before.capturedAt) >= expiryMs || utcMs(after.capturedAt) >= expiryMs) fail("grant_expired");
  const nowMs = now().getTime();
  if (!Number.isFinite(nowMs)) fail("clock_invalid");
  if (nowMs >= expiryMs) fail("grant_expired");
  if (utcMs(after.capturedAt) < utcMs(before.capturedAt) || utcMs(after.capturedAt) > nowMs) fail("time_regressed");
  if (after.restTotal < before.restTotal || after.rest5xx < before.rest5xx) fail("counter_decreased");
  const labels = [...new Set([...Object.keys(before.results), ...Object.keys(after.results)])].sort();
  const deltas = Object.create(null);
  for (const label of labels) {
    const was = Object.hasOwn(before.results, label) ? before.results[label] : 0;
    const is = Object.hasOwn(after.results, label) ? after.results[label] : 0;
    if (is < was) fail("counter_decreased");
    deltas[label] = is - was;
  }
  return {
    schema: SCHEMA, available: true, code: "compared",
    sha: after.sha, deploymentId: after.deploymentId, expiresAt: after.expiresAt,
    incarnation: after.incarnation,
    before: { runId: before.runId, runAttempt: before.runAttempt, capturedAt: before.capturedAt },
    after: { runId: after.runId, runAttempt: after.runAttempt, capturedAt: after.capturedAt },
    restTotalDelta: after.restTotal - before.restTotal,
    rest5xxDelta: after.rest5xx - before.rest5xx,
    resultDeltas: Object.fromEntries(Object.entries(deltas)),
  };
}
