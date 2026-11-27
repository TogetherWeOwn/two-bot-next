import { createHash } from "node:crypto";
import { sameLease } from "../src/staging-automod-recovery.ts";
import { assembleStagingAutomodEvidence } from "./staging-automod-evidence.ts";
import { verifyStagingAutomodReceipt, type StagingAutomodQualificationPin } from "./staging-automod-receipt-verifier.ts";

type Lease = StagingAutomodQualificationPin["lease"];
const REJECTIONS = [
  "invalid_input", "promotion_not_disabled", "lease_mismatch", "job_evidence_failed", "receipt_invalid",
] as const;
type Rejection = typeof REJECTIONS[number];
type Summary = Readonly<
  | { verdict: "qualified"; reasons: readonly []; runId: string; deploymentId: string; sha: string; epoch: number }
  | { verdict: "rejected"; reasons: readonly [Rejection]; runId: null; deploymentId: null; sha: null; epoch: null }
>;
type Archive = Summary & Readonly<{ archiveDigest: string }>;

const LEASE_KEYS = [
  "environment", "purpose", "phase", "runId", "runAttempt", "deploymentId", "sha", "epoch", "acquiredAt", "expiresAt",
] as const;
const SERVING_KEYS = [
  "workerDeploymentId", "workerVersionId", "sourceSha", "containerApplicationId", "containerNamespaceId",
  "containerRolloutId", "containerImageDigest", "ownerEpoch",
] as const;
const ARCHIVE_KEYS = ["archiveDigest", "deploymentId", "epoch", "reasons", "runId", "sha", "verdict"] as const;

function hasKeys<K extends string>(value: unknown, keys: readonly K[]): value is Record<K, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    && Object.keys(value).length === keys.length && keys.every(key => Object.hasOwn(value, key));
}

// Check and copy decoded data before handing it to the merged verifiers.
// Bound the walk so malformed receipt trees cannot drive unbounded recursion.
function snapshot(input: unknown): unknown {
  let remaining = 256;
  function visit(value: unknown, depth: number): unknown {
    if (--remaining < 0 || depth > 8) throw new Error("invalid archive data");
    if (value === null || typeof value === "boolean" || typeof value === "number") return value;
    if (typeof value === "string" && value.length <= 128) return value;
    if (typeof value !== "object") throw new Error("invalid archive data");

    const array = Array.isArray(value);
    const keys = Reflect.ownKeys(value);
    if (Object.getPrototypeOf(value) !== (array ? Array.prototype : Object.prototype) || keys.length > 16) {
      throw new Error("invalid archive data");
    }
    function read(key: string): unknown {
      const field = Object.getOwnPropertyDescriptor(value, key);
      if (!field?.enumerable || !Object.hasOwn(field, "value")) throw new Error("invalid archive data");
      return visit(field.value, depth + 1);
    }
    if (array) {
      const length = keys.length - 1; // A dense array also owns its length.
      if (Object.getOwnPropertyDescriptor(value, "length")?.value !== length) throw new Error("invalid archive data");
      return Array.from({ length }, (_, index) => read(String(index)));
    }
    return Object.fromEntries(keys.map(key => {
      if (typeof key !== "string" || key.length > 64) throw new Error("invalid archive data");
      return [key, read(key)];
    }));
  }
  return visit(input, 0);
}

function issuedLease(value: unknown): value is Lease {
  return hasKeys(value, LEASE_KEYS) && value.purpose === "automod-preview" && value.runAttempt === 1
    && sameLease(value as Lease, value);
}

function digest({ deploymentId, epoch, reasons, runId, sha, verdict }: Summary): string {
  // The fixed schema contains only primitives and a bounded reason array.
  // Write the summary keys in lexicographic order for canonical JSON.
  return createHash("sha256").update(JSON.stringify({ deploymentId, epoch, reasons, runId, sha, verdict })).digest("hex");
}

function seal(summary: Summary): Archive {
  Object.freeze(summary.reasons);
  return Object.freeze({ ...summary, archiveDigest: digest(summary) });
}

function reject(reason: Rejection): Archive {
  return seal({ verdict: "rejected", reasons: [reason], runId: null, deploymentId: null, sha: null, epoch: null });
}

/** Seal a summary from independently retained evidence and a trusted UTC clock.
 * Rejections carry only a fixed reason and omit all input identifiers.
 */
export function sealStagingAutomodQualificationArchive(input: unknown, now: number): Archive {
  try {
    const data = snapshot(input);
    if (!hasKeys(data, ["pin", "receipt", "evidenceInput", "promotionDisabled"])
      || !Number.isSafeInteger(now) || now < 0) return reject("invalid_input");

    const { pin, receipt, evidenceInput, promotionDisabled } = data;
    if (!hasKeys(pin, ["runId", "lease", "serving", "command"]) || !issuedLease(pin.lease)
      || !hasKeys(pin.serving, SERVING_KEYS) || !hasKeys(pin.command, ["id", "version"])
      || !hasKeys(evidenceInput, ["expectedLease", "attempt"]) || !hasKeys(evidenceInput.attempt, ["lease", "result"])
      || !issuedLease(evidenceInput.expectedLease) || !issuedLease(evidenceInput.attempt.lease)) return reject("invalid_input");

    const { attempt, expectedLease } = evidenceInput;
    const { result } = attempt;
    if (!hasKeys(result, ["verdict", "start", "recover"]) &&
      !(hasKeys(result, ["verdict", "code"]) && result.verdict === "failed" && result.code === "lease_unconfirmed")) {
      return reject("invalid_input");
    }
    if (promotionDisabled !== true) return reject("promotion_not_disabled");
    if (!sameLease(pin.lease, expectedLease) || !sameLease(pin.lease, attempt.lease)) return reject("lease_mismatch");
    if (assembleStagingAutomodEvidence(evidenceInput).verdict !== "passed") return reject("job_evidence_failed");
    // The merged receipt verifier checks all pin values after the shape check above.
    if (!verifyStagingAutomodReceipt(receipt, pin as StagingAutomodQualificationPin, now)) return reject("receipt_invalid");

    const { runId, deploymentId, sha, epoch } = pin.lease;
    return seal({ verdict: "qualified", reasons: [], runId, deploymentId, sha, epoch });
  } catch {
    return reject("invalid_input");
  }
}

/** Check the serialized envelope's schema and checksum. Provenance still
 * depends on the trusted acquisition and retention of the archive.
 */
export function verifyStagingAutomodQualificationArchive(archive: unknown): boolean {
  try {
    const data = snapshot(archive);
    if (!hasKeys(data, ARCHIVE_KEYS)) return false;
    const { verdict, reasons, runId, deploymentId, sha, epoch, archiveDigest } = data;
    if (!Array.isArray(reasons) || typeof archiveDigest !== "string" || !/^[0-9a-f]{64}$/.test(archiveDigest)) return false;
    if (verdict === "qualified") {
      if (reasons.length !== 0 || typeof runId !== "string" || !/^[1-9][0-9]{0,19}$/.test(runId)
        || typeof deploymentId !== "string" || !/^[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}$/.test(deploymentId)
        || typeof sha !== "string" || !/^[0-9a-f]{40}$/.test(sha)
        || typeof epoch !== "number" || !Number.isSafeInteger(epoch) || epoch <= 0) return false;
      return archiveDigest === digest({ verdict, reasons: [], runId, deploymentId, sha, epoch });
    }
    const reason = REJECTIONS.find(code => code === reasons[0]);
    return verdict === "rejected" && reasons.length === 1 && reason !== undefined
      && runId === null && deploymentId === null && sha === null && epoch === null
      && archiveDigest === digest({ verdict, reasons: [reason], runId, deploymentId, sha, epoch });
  } catch {
    return false;
  }
}
