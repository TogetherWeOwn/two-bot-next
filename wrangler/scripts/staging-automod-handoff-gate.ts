import type { composeStagingAutomodQualificationBundle } from "./staging-automod-qualification-bundle.ts";
import { STAGING_AUTOMOD_PLAN } from "../src/staging-automod-plan.ts";
import { isSnowflake } from "../src/container-env.ts";

type PassedBundle = Extract<ReturnType<typeof composeStagingAutomodQualificationBundle>, { verdict: "passed" }>;
type QualificationPin = Omit<PassedBundle, "schema" | "environment" | "verdict" | "observedAt">;

export type StagingAutomodHandoffGateInput = Readonly<{
  enabled?: boolean;
  /** Trusted UTC epoch milliseconds. Required when enabled. */
  now?: number;
  bundle?: unknown;
  pin?: QualificationPin;
  expectedBaselineFingerprint?: string;
  restoration?: Readonly<{
    /** UTC epoch milliseconds for the coherent post-rehearsal readback. */
    observedAt: number;
    command: QualificationPin["command"] | null;
    baselineFingerprint: string;
    activeLease: object | null;
    stagingUseReleased: boolean;
  }>;
  productionAutoPromoteDisabled?: boolean;
}>;

export type StagingAutomodHandoffFailureCode =
  | "qualification_unconfirmed"
  | "qualification_pin_mismatch"
  | "qualification_time_invalid"
  | "restoration_time_invalid"
  | "command_absence_unconfirmed"
  | "baseline_unconfirmed"
  | "lease_clearance_unconfirmed"
  | "staging_use_release_unconfirmed"
  | "production_auto_promote_not_disabled"
  | "staging_automod_plan_enabled";

export type StagingAutomodHandoffVerdict =
  | Readonly<{ verdict: "disabled" }>
  | Readonly<{ verdict: "passed" }>
  | Readonly<{ verdict: "failed"; code: StagingAutomodHandoffFailureCode }>;

// The handoff schema is pinned independently of the composer at this boundary.
const SCHEMA = "two-bot.staging-automod-qualification.v1";
const MAX_AGE_MS = 60_000;
const MAX_UTC_MS = 8_640_000_000_000_000;
const UUID = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/;
const FINGERPRINT = /^[0-9a-f]{64}$/;
const PIN_FORMATS = {
  runId: /^[1-9][0-9]{0,19}$/,
  sha: /^[0-9a-f]{40}$/,
  workerDeploymentId: UUID,
  workerVersionId: UUID,
  containerApplicationId: UUID,
  containerNamespaceId: /^[0-9a-f]{32}$/,
  containerRolloutId: UUID,
  containerImageDigest: /^sha256:[0-9a-f]{64}$/,
} satisfies Record<Exclude<keyof QualificationPin, "ownerEpoch" | "command">, RegExp>;

function field(value: unknown, key: string): unknown {
  if (value === null || typeof value !== "object") return;
  try {
    if (Array.isArray(value)) return;
    const own = Object.getOwnPropertyDescriptor(value, key);
    if (own && Object.hasOwn(own, "value")) return own.value;
  } catch {
    // An unreadable field cannot establish evidence.
  }
}

function matches(value: unknown, format: RegExp): value is string {
  return typeof value === "string" && value.length <= 71 && format.test(value);
}

function commandMatches(command: unknown, pin: unknown): boolean {
  return ["id", "version"].every(key => {
    const expected = field(pin, key);
    return matches(expected, /^[1-9][0-9]{16,19}$/) && isSnowflake(expected) && field(command, key) === expected;
  });
}

function pinMatches(bundle: unknown, pin: unknown): boolean {
  const epoch = field(pin, "ownerEpoch");
  return Object.entries(PIN_FORMATS).every(([key, format]) => {
    const expected = field(pin, key);
    return matches(expected, format) && field(bundle, key) === expected;
  }) && typeof epoch === "number" && Number.isSafeInteger(epoch) && epoch > 0 && field(bundle, "ownerEpoch") === epoch
    && commandMatches(field(bundle, "command"), field(pin, "command"));
}

function utcTime(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 && value <= MAX_UTC_MS;
}

function qualificationTime(value: unknown): number | undefined {
  if (typeof value !== "string" || value.length !== 24) return;
  const time = Date.parse(value);
  if (utcTime(time) && new Date(time).toISOString() === value) return time;
}

function failed(code: StagingAutomodHandoffFailureCode): StagingAutomodHandoffVerdict {
  return Object.freeze({ verdict: "failed", code });
}

/** Evaluate caller-authenticated snapshots offline. Supply the run, identity,
 * command and pre-rehearsal fingerprint independently of the bundle. The
 * caller must bind the restoration readback to that same rehearsal and scope.
 */
export function evaluateStagingAutomodHandoffGate(input: StagingAutomodHandoffGateInput = {}): StagingAutomodHandoffVerdict {
  if (field(input, "enabled") !== true) return Object.freeze({ verdict: "disabled" });

  const bundle = field(input, "bundle");
  if (field(bundle, "schema") !== SCHEMA || field(bundle, "environment") !== "staging"
    || field(bundle, "verdict") !== "passed") return failed("qualification_unconfirmed");
  if (!pinMatches(bundle, field(input, "pin"))) return failed("qualification_pin_mismatch");

  const now = field(input, "now");
  const observedAt = qualificationTime(field(bundle, "observedAt"));
  if (!utcTime(now) || observedAt === undefined || observedAt > now || now - observedAt > MAX_AGE_MS) {
    return failed("qualification_time_invalid");
  }

  const restoration = field(input, "restoration");
  const restoredAt = field(restoration, "observedAt");
  if (!utcTime(restoredAt) || restoredAt < observedAt || restoredAt > now) return failed("restoration_time_invalid");
  if (field(restoration, "command") !== null) return failed("command_absence_unconfirmed");
  const baseline = field(input, "expectedBaselineFingerprint");
  if (!matches(baseline, FINGERPRINT) || field(restoration, "baselineFingerprint") !== baseline) {
    return failed("baseline_unconfirmed");
  }
  if (field(restoration, "activeLease") !== null) return failed("lease_clearance_unconfirmed");
  if (field(restoration, "stagingUseReleased") !== true) return failed("staging_use_release_unconfirmed");
  if (field(input, "productionAutoPromoteDisabled") !== true) return failed("production_auto_promote_not_disabled");
  if (STAGING_AUTOMOD_PLAN.enabled !== false) return failed("staging_automod_plan_enabled");
  return Object.freeze({ verdict: "passed" });
}
