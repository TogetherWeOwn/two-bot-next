import { types } from "node:util";
import { sameLease, STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";
import type { StagingUse } from "../src/staging-lifecycle.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";

const base = { schema: "two-bot-staging-automod-authority-pin.v1", environment: "staging" } as const;
const disabled = Object.freeze({ ...base, verdict: "disabled" } as const);
const passed = Object.freeze({ ...base, verdict: "passed" } as const);
const MAX_CLOCK_MS = 8_640_000_000_000_000;
const leaseKeys = ["environment", "phase", "purpose", "deploymentId", "sha", "epoch", "runId", "runAttempt",
  "acquiredAt", "expiresAt"] as const;

type Failure = "invalid_input" | "invalid_clock" | "invalid_max_age" | "plan_refused" | "scope_mismatch"
  | "lease_unconfirmed" | "publish_approval_unconfirmed" | "cleanup_approval_unconfirmed" | "promotion_unconfirmed";

function fail(code: Failure) {
  return Object.freeze({ ...base, verdict: "failed", code } as const);
}

function dataObject(value: unknown): value is object {
  // A proxy can fabricate a data descriptor, even when its traps never throw.
  return value !== null && typeof value === "object" && !types.isProxy(value);
}

function plainRecord(value: unknown): value is object {
  if (!dataObject(value) || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function dataProperty(value: object, key: string): { value: unknown } | undefined {
  const descriptor = Object.getOwnPropertyDescriptor(value, key);
  if (descriptor && Object.hasOwn(descriptor, "value")) return { value: descriptor.value };
}

function snapshot<Key extends string>(value: unknown, keys: readonly Key[]): Record<Key, unknown> | undefined {
  if (!plainRecord(value)) return;
  const result: Record<Key, unknown> = Object.create(null);
  for (const key of keys) {
    const descriptor = dataProperty(value, key);
    if (!descriptor) return;
    result[key] = descriptor.value;
  }
  return result;
}

function nonnegativeInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function fresh(at: unknown, now: number, maxAgeMs: number): boolean {
  return nonnegativeInteger(at) && at <= now && now - at <= maxAgeMs;
}

function fixedTarget(value: unknown): boolean {
  const target = snapshot(value, ["applicationId", "guildId", "name"]);
  return target?.applicationId === STAGING_AUTOMOD_TARGET.applicationId
    && target.guildId === STAGING_AUTOMOD_TARGET.guildId && target.name === STAGING_AUTOMOD_TARGET.name;
}

function snapshotLease(value: unknown): Readonly<StagingUse> | undefined {
  if (!dataObject(value) || Object.hasOwn(value, "voiceMove")) return;
  const lease = snapshot(value, leaseKeys);
  if (lease?.purpose !== AUTOMOD_STAGING_PURPOSE || lease.runAttempt !== 1) return;
  // Callers validate the remaining fields with sameLease before using them.
  return lease as Readonly<StagingUse>;
}

function approved(value: unknown, action: "publish" | "cleanup", lease: Readonly<StagingUse>,
  now: number, maxAgeMs: number): boolean {
  const approval = snapshot(value, ["action", "issuedLease", "target", "approvedAt"]);
  return approval?.action === action && fresh(approval.approvedAt, now, maxAgeMs) && fixedTarget(approval.target)
    && sameLease(lease, snapshotLease(approval.issuedLease));
}

/** Check pins supplied by an authenticated protected caller. The caller owns
 * the clock, freshness policy and provenance of the run, lease and readbacks.
 * The verdict is evidence for this instant. It performs no staging operation.
 */
export function verifyStagingAutomodAuthorityPins(input?: unknown) {
  if (!dataObject(input) || dataProperty(input, "enabled")?.value !== true) return disabled;

  const pins = snapshot(input, ["now", "maxAgeMs", "plan", "target", "environment", "purpose", "runAttempt",
    "runId", "deploymentId", "sha", "epoch", "issuedLease", "approvals", "promotion"]);
  if (!pins) return fail("invalid_input");
  const { now, maxAgeMs } = pins;
  if (!nonnegativeInteger(now) || now > MAX_CLOCK_MS) return fail("invalid_clock");
  if (!nonnegativeInteger(maxAgeMs)) return fail("invalid_max_age");

  const plan = snapshot(pins.plan, ["enabled", "allowStart", "notAfter", "cleanupMs", "target"]);
  if (plan?.enabled !== true || plan.allowStart !== true || !nonnegativeInteger(plan.notAfter) || plan.notAfter <= now
    || !nonnegativeInteger(plan.cleanupMs) || plan.cleanupMs === 0 || !fixedTarget(plan.target)) return fail("plan_refused");

  if (pins.environment !== "staging" || pins.purpose !== AUTOMOD_STAGING_PURPOSE || pins.runAttempt !== 1
    || !fixedTarget(pins.target)) return fail("scope_mismatch");
  const lease = snapshotLease(pins.issuedLease);
  if (!lease || !sameLease(lease, lease) || lease.acquiredAt > now || lease.expiresAt <= now || lease.runId !== pins.runId
    || lease.deploymentId !== pins.deploymentId || lease.sha !== pins.sha || lease.epoch !== pins.epoch) {
    return fail("lease_unconfirmed");
  }

  if (!plainRecord(pins.approvals)) return fail("publish_approval_unconfirmed");
  for (const action of ["publish", "cleanup"] as const) {
    if (!approved(dataProperty(pins.approvals, action)?.value, action, lease, now, maxAgeMs)) {
      return fail(`${action}_approval_unconfirmed`);
    }
  }

  const promotion = snapshot(pins.promotion, ["PRODUCTION_AUTO_PROMOTE_DISABLED", "observedAt"]);
  if (promotion?.PRODUCTION_AUTO_PROMOTE_DISABLED !== true || !fresh(promotion.observedAt, now, maxAgeMs)) {
    return fail("promotion_unconfirmed");
  }
  return passed;
}
