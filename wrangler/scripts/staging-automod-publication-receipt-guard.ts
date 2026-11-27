import { types } from "node:util";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";
import { MAX_STAGING_USE_MS } from "../src/staging-use-window.ts";

const MAX_TIMESTAMP = 8_640_000_000_000_000;
const MAX_PUBLICATION_AGE_MS = 60_000;
const bindingKeys = [
  "runId", "runAttempt", "deploymentId", "workerVersionId", "sourceSha",
  "ownerEpoch", "ruleId", "previewReceiptId", "actionDigest",
] as const;
const pinKeys = [...bindingKeys, "stagingGuildId", "approvedMode", "leaseAcquiredAt", "leaseExpiresAt"] as const;
const receiptKeys = [
  ...bindingKeys, "environment", "productionAutoPromoteDisabled", "guildId", "mode", "publishedAt", "observedAt",
] as const;
const approvalKeys = ["approvalRunId", "approvalSha"] as const;

type FailureCode = "invalid_clock" | "invalid_pin" | "invalid_receipt" | "environment_mismatch"
  | "promotion_not_disabled" | "guild_mismatch" | "binding_mismatch" | "mode_mismatch"
  | "approval_unconfirmed" | "deadline_outside_window" | "receipt_stale";

function object(value: unknown): value is object {
  return value !== null && typeof value === "object" && !types.isProxy(value) && !Array.isArray(value);
}

function ownData(value: object, key: PropertyKey) {
  const descriptor = Object.getOwnPropertyDescriptor(value, key);
  return descriptor && Object.hasOwn(descriptor, "value") ? descriptor : undefined;
}

function snapshot<Key extends string, Optional extends string = never>(
  value: unknown, required: readonly Key[], optional: readonly Optional[] = [],
): (Record<Key, unknown> & Partial<Record<Optional, unknown>>) | undefined {
  if (!object(value)) return;
  const prototype: unknown = Object.getPrototypeOf(value);
  if (prototype !== null && prototype !== Object.prototype) return;

  const keys = Reflect.ownKeys(value);
  const allowed: ReadonlySet<PropertyKey> = new Set([...required, ...optional]);
  if (keys.length > allowed.size || !required.every(key => Object.hasOwn(value, key))) return;
  const copy: Record<PropertyKey, unknown> = Object.create(null);
  for (const key of keys) {
    if (!allowed.has(key)) return;
    const descriptor = ownData(value, key);
    if (!descriptor) return;
    copy[key] = descriptor.value;
  }
  return copy as Record<Key, unknown> & Partial<Record<Optional, unknown>>;
}

function timestamp(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 && value <= MAX_TIMESTAMP;
}

function positiveInteger(value: unknown): boolean {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0;
}

function matches(value: unknown, maxLength: number, pattern: RegExp): boolean {
  return typeof value === "string" && value.length <= maxLength && pattern.test(value);
}

function identifier(value: unknown): boolean {
  return typeof value === "string" && value.length <= 128 && value.trim().length > 0;
}

function validBinding(pin: Record<(typeof bindingKeys)[number], unknown>): boolean {
  return matches(pin.runId, 20, /^[1-9][0-9]{0,19}$/)
    && positiveInteger(pin.runAttempt) && positiveInteger(pin.ownerEpoch)
    && matches(pin.deploymentId, 128, /^[a-zA-Z0-9_-]{1,128}$/) && identifier(pin.workerVersionId)
    && identifier(pin.ruleId) && identifier(pin.previewReceiptId)
    && matches(pin.sourceSha, 40, /^[0-9a-f]{40}$/) && matches(pin.actionDigest, 64, /^[0-9a-f]{64}$/);
}

/** Check decoded staging evidence against independently authenticated pins and a
 * trusted UTC clock. A pass attests to the supplied evidence at that instant.
 * Receipt timestamps are UTC epoch milliseconds. The verdict exposes no input IDs.
 */
export function verifyStagingAutomodPublicationReceipt(input: unknown, now = Date.now()) {
  const observedAt = timestamp(now) ? new Date(now).toISOString() : null;
  const common = {
    schema: "two-bot-staging-automod-publication-receipt-guard.v1", environment: "staging", observedAt,
  } as const;
  if (!object(input) || ownData(input, "enabled")?.value !== true) {
    return Object.freeze({ ...common, verdict: "disabled" } as const);
  }

  const fail = (code: FailureCode) => Object.freeze({ ...common, verdict: "failed", code } as const);
  if (observedAt === null) return fail("invalid_clock");
  const pin = snapshot(ownData(input, "pin")?.value, pinKeys);
  if (!pin || !validBinding(pin)) return fail("invalid_pin");
  const receipt = snapshot(ownData(input, "receipt")?.value, receiptKeys, approvalKeys);
  if (!receipt) return fail("invalid_receipt");
  if (receipt.environment !== "staging") return fail("environment_mismatch");
  if (receipt.productionAutoPromoteDisabled !== true) return fail("promotion_not_disabled");
  // Exact equality to the canonical staging snowflake also rejects a jointly
  // substituted pin and receipt pointing at another guild.
  if (pin.stagingGuildId !== STAGING_AUTOMOD_TARGET.guildId || receipt.guildId !== pin.stagingGuildId) {
    return fail("guild_mismatch");
  }
  if (!bindingKeys.every(key => receipt[key] === pin[key])) return fail("binding_mismatch");
  if ((receipt.mode !== "dry_run" && receipt.mode !== "live_staging") || receipt.mode !== pin.approvedMode) {
    return fail("mode_mismatch");
  }
  if (receipt.mode === "live_staging" && (!identifier(receipt.approvalRunId) || receipt.approvalSha !== pin.sourceSha)) {
    return fail("approval_unconfirmed");
  }

  const { leaseAcquiredAt, leaseExpiresAt } = pin;
  const { publishedAt, observedAt: receiptObservedAt } = receipt;
  if (!timestamp(leaseAcquiredAt) || !timestamp(leaseExpiresAt) || !timestamp(publishedAt) || !timestamp(receiptObservedAt)
    || leaseExpiresAt <= leaseAcquiredAt || leaseExpiresAt - leaseAcquiredAt > MAX_STAGING_USE_MS
    || leaseAcquiredAt > publishedAt || publishedAt > receiptObservedAt || receiptObservedAt > now || now > leaseExpiresAt) {
    return fail("deadline_outside_window");
  }
  // Bound the full age since publication, including delay before observation.
  if (now - publishedAt > MAX_PUBLICATION_AGE_MS) return fail("receipt_stale");
  return Object.freeze({ ...common, verdict: "passed" } as const);
}
