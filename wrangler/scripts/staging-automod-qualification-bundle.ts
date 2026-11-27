import { verifyStagingAutomodReceipt, type StagingAutomodQualificationPin } from "./staging-automod-receipt-verifier.ts";

type QualificationInput = Readonly<{
  enabled?: boolean;
  pin: StagingAutomodQualificationPin;
  receipt: unknown;
  rehearsal: unknown;
}>;

function field(value: unknown, key: string): unknown {
  if (value === null || typeof value !== "object") return;
  try {
    const own = Object.getOwnPropertyDescriptor(value, key);
    if (own && Object.hasOwn(own, "value")) return own.value;
  } catch {
    // An unreadable field cannot establish evidence.
  }
}

function verifiedPin(input: QualificationInput, now: number): StagingAutomodQualificationPin | undefined {
  try {
    const { runId, lease, serving, command } = input.pin;
    if (lease === null || typeof lease !== "object" || Array.isArray(lease) || "voiceMove" in lease) return;
    // Snapshot the fields used by the verifier so the verdict uses exactly
    // the values it checked, even if reading the receipt changes the input.
    const { environment, purpose, phase, runId: leaseRunId, runAttempt, deploymentId, sha, epoch,
      acquiredAt, expiresAt } = lease;
    const { workerDeploymentId, workerVersionId, sourceSha, containerApplicationId, containerNamespaceId,
      containerRolloutId, containerImageDigest, ownerEpoch } = serving;
    const pin = {
      runId,
      lease: { environment, purpose, phase, runId: leaseRunId, runAttempt, deploymentId, sha, epoch, acquiredAt, expiresAt },
      serving: { workerDeploymentId, workerVersionId, sourceSha, containerApplicationId, containerNamespaceId,
        containerRolloutId, containerImageDigest, ownerEpoch },
      command: { id: command.id, version: command.version },
    };
    if (verifyStagingAutomodReceipt(field(input, "receipt"), pin, now)) return pin;
  } catch {
    // Keep malformed pins and exception details out of the verdict.
  }
}

/** Compose evidence supplied by a trusted caller. Authenticate the receipt and
 * bind the rehearsal result to its protected run, SHA and Worker version before
 * passing them here. An invalid clock yields a null observation time.
 */
export function composeStagingAutomodQualificationBundle(input: QualificationInput, now = Date.now()) {
  const observedAt = Number.isSafeInteger(now) && now >= 0 && now <= 8_640_000_000_000_000
    ? new Date(now).toISOString() : null;
  const common = { schema: "two-bot-staging-automod-qualification.v1", environment: "staging", observedAt } as const;
  if (field(input, "enabled") !== true) return Object.freeze({ ...common, verdict: "disabled" } as const);

  const pin = observedAt === null ? undefined : verifiedPin(input, now);
  if (!pin) return Object.freeze({ ...common, verdict: "failed", code: "receipt_unverified" } as const);

  const rehearsal = field(input, "rehearsal");
  if (field(rehearsal, "verdict") !== "passed" || field(rehearsal, "start") !== true
    || field(rehearsal, "recover") !== true || field(rehearsal, "runId") !== pin.runId
    || field(rehearsal, "sha") !== pin.lease.sha || field(rehearsal, "deploymentId") !== pin.lease.deploymentId) {
    return Object.freeze({ ...common, verdict: "failed", code: "rehearsal_unconfirmed" } as const);
  }

  const { workerDeploymentId, workerVersionId, containerApplicationId, containerNamespaceId,
    containerRolloutId, containerImageDigest, ownerEpoch } = pin.serving;
  return Object.freeze({ ...common, verdict: "passed", runId: pin.runId, sha: pin.lease.sha, workerDeploymentId,
    workerVersionId, containerApplicationId, containerNamespaceId, containerRolloutId, containerImageDigest,
    ownerEpoch, command: Object.freeze(pin.command) } as const);
}
