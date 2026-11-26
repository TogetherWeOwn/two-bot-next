import { boundRead, releaseUpstream } from "../src/internal-actions.ts";
import type { ProtectedAutomodCall } from "../src/staging-automod-caller.ts";
import { sameLease } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-lifecycle.ts";
import { boundStagingAutomodInventoryRead } from "./staging-automod-inventory-verifier.ts";

type Lease = ProtectedAutomodCall["issuedLease"];
type Job = Readonly<Pick<Lease, "runId" | "runAttempt" | "sha">>;
type Invoke = (request: ProtectedAutomodCall, signal: AbortSignal) => Promise<Response>;
const LEASE_KEYS = ["environment", "purpose", "phase", "runId", "runAttempt", "deploymentId",
  "sha", "epoch", "acquiredAt", "expiresAt"] as const;
const RECEIPT_KEYS = ["schema", "environment", "observedAt", "action", "lease", "verdict"] as const;
const MAX_RECEIPT_BYTES = 1024;
const REQUEST_MS = 15_000;

function hasKeys<K extends string>(value: unknown, keys: readonly K[]): value is Record<K, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    && Object.keys(value).length === keys.length && keys.every(key => Object.hasOwn(value, key));
}

async function readReceipt(response: Response, signal: AbortSignal): Promise<unknown> {
  const reader = response.body?.getReader();
  if (!reader) return;
  const decoder = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });
  let text = "";
  let bytes = 0;
  try {
    for (;;) {
      const { done, value } = await boundRead(reader.read(), signal);
      if (done) return JSON.parse(text + decoder.decode());
      bytes += value.byteLength;
      if (bytes > MAX_RECEIPT_BYTES) return;
      text += decoder.decode(value, { stream: true });
    }
  } finally {
    void reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}

async function confirmed(action: ProtectedAutomodCall["action"], issuedLease: Lease, invoke: Invoke): Promise<boolean> {
  const controller = new AbortController();
  const { signal } = controller;
  const timer = setTimeout(() => controller.abort(), REQUEST_MS);
  try {
    const response = await boundStagingAutomodInventoryRead(invoke(Object.freeze({ action, issuedLease }), signal).then(reply => {
      if (signal.aborted) releaseUpstream(reply);
      return reply;
    }), signal);
    if (!response.ok) {
      releaseUpstream(response);
      return false;
    }
    const receipt = await readReceipt(response, signal);
    if (!hasKeys(receipt, RECEIPT_KEYS) || receipt.schema !== "two-bot-staging-automod-job.v1"
      || receipt.environment !== "staging" || receipt.action !== action || receipt.verdict !== "passed"
      || !hasKeys(receipt.lease, LEASE_KEYS) || receipt.lease.purpose !== AUTOMOD_STAGING_PURPOSE
      || !sameLease(issuedLease, receipt.lease) || typeof receipt.observedAt !== "string") return false;
    const at = Date.parse(receipt.observedAt);
    return Number.isFinite(at) && at >= issuedLease.acquiredAt && new Date(at).toISOString() === receipt.observedAt;
  } catch {
    return false;
  } finally {
    clearTimeout(timer);
  }
}

/** The caller supplies the protected run/build pin and the independently issued
 * lease. The invoker must authenticate each action separately. A local timeout
 * only ends the wait; the Worker must confirm drain, stop and same-lease retirement.
 */
export async function runStagingAutomodDryRun(job: Job, issuedLease: Lease, invoke: Invoke) {
  if (!sameLease(issuedLease, issuedLease) || !hasKeys(issuedLease, LEASE_KEYS)
    || issuedLease.purpose !== AUTOMOD_STAGING_PURPOSE || issuedLease.runId !== job.runId
    || issuedLease.runAttempt !== job.runAttempt || job.runAttempt !== 1 || issuedLease.sha !== job.sha) {
    return { verdict: "failed", code: "lease_unconfirmed" } as const;
  }
  const lease = Object.freeze({ ...issuedLease, purpose: AUTOMOD_STAGING_PURPOSE });

  let start: boolean;
  let recover: boolean;
  try {
    start = await confirmed("start", lease, invoke);
  } finally {
    recover = await confirmed("recover", lease, invoke);
  }
  return { verdict: start && recover ? "passed" : "failed", start, recover } as const;
}
