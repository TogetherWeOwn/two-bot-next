import { types } from "node:util";
import { isSnowflake } from "../src/container-env.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";

const HEADER = {
  schema: "two-bot-staging-automod-readback-consistency.v1", environment: "staging",
} as const;
const DISABLED = Object.freeze({ ...HEADER, verdict: "disabled" } as const);
const VERIFIED = Object.freeze({ ...HEADER, verdict: "verified" } as const);
const PINS = ["runId", "deploymentId", "sourceSha", "ownerEpoch", "commandId", "commandVersion", "commandHash"] as const;
const READBACK_FIELDS = [
  "sourceId", "observedAt", "target", ...PINS, "scope", "extraCommandCount", "unpinnedCommandCount",
] as const;
const UUID = /^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$/;

type FailureCode =
  | "invalid_input" | "invalid_window" | "invalid_readback" | "unauthenticated_source"
  | "duplicate_source" | "readback_not_fresh" | "readback_skew" | "target_mismatch"
  | "invalid_pins" | "command_scope_unconfirmed" | "readback_mismatch"
  | "production_auto_promotion_unconfirmed";

export type StagingAutomodReadbackContext = Readonly<{
  now: number;
  maxAgeMs: number;
  maxSkewMs: number;
  /** Private to the collector. Bind each frozen snapshot to its independently authenticated source ID. */
  sources: WeakMap<object, string>;
  /** Fresh repository variable readback from the trusted collector. */
  promotion: unknown;
}>;

function failed(code: FailureCode) {
  return Object.freeze({ ...HEADER, verdict: "failed", code } as const);
}

function inspectable(value: unknown): value is object {
  return value !== null && (typeof value === "object" || typeof value === "function")
    && !types.isProxy(value) && !types.isModuleNamespaceObject(value);
}

function ownData(value: object, key: PropertyKey): PropertyDescriptor | undefined {
  const descriptor = Object.getOwnPropertyDescriptor(value, key);
  if (descriptor && Object.hasOwn(descriptor, "value")) return descriptor;
}

function record<K extends string>(value: unknown, keys: readonly K[]): Record<K, unknown> | undefined {
  if (!inspectable(value) || typeof value !== "object" || Array.isArray(value)) return;
  const prototype: unknown = Object.getPrototypeOf(value);
  if ((prototype !== Object.prototype && prototype !== null) || Reflect.ownKeys(value).length !== keys.length) return;
  const copy = Object.create(null) as Record<K, unknown>;
  for (const key of keys) {
    const descriptor = ownData(value, key);
    if (!descriptor) return;
    copy[key] = descriptor.value;
  }
  return copy;
}

function time(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function fresh(at: unknown, now: number, maxAgeMs: number): at is number {
  return time(at) && at <= now && now - at <= maxAgeMs;
}

function matches(value: unknown, limit: number, format: RegExp): value is string {
  return typeof value === "string" && value.length <= limit && format.test(value);
}

function snowflake(value: unknown): boolean {
  return matches(value, 20, /^[1-9][0-9]{16,19}$/) && isSnowflake(value);
}

function validPins(pins: Record<typeof PINS[number], unknown>): boolean {
  return matches(pins.runId, 16, /^[1-9][0-9]{0,15}$/) && Number.isSafeInteger(Number(pins.runId))
    && matches(pins.deploymentId, 36, UUID) // The lifecycle deployment pin is the Worker version ID.
    && matches(pins.sourceSha, 40, /^[0-9a-f]{40}$/)
    && time(pins.ownerEpoch) && pins.ownerEpoch > 0
    && snowflake(pins.commandId) && snowflake(pins.commandVersion)
    && matches(pins.commandHash, 64, /^[0-9a-f]{64}$/);
}

function fixedTarget(value: unknown): boolean {
  if (!inspectable(value)) return false;
  const target = record(value, ["applicationId", "guildId", "name"]);
  return target !== undefined && Object.isFrozen(value)
    && target.applicationId === STAGING_AUTOMOD_TARGET.applicationId
    && target.guildId === STAGING_AUTOMOD_TARGET.guildId && target.name === STAGING_AUTOMOD_TARGET.name;
}

function guildOnly(readback: Record<typeof READBACK_FIELDS[number], unknown>): boolean {
  return readback.scope === "guild" && readback.extraCommandCount === 0 && readback.unpinnedCommandCount === 0;
}

/** Check the complete guild registry projections from two authenticated collectors.
 * The trusted caller supplies the clock and policy, binds the exact source objects
 * after authenticating them, and independently reads the repository variable.
 * Freeze each source and its target before binding so the authenticated values
 * cannot change. Source IDs are canonical collector identities, never credentials.
 */
export function verifyStagingAutomodReadbackConsistency(input?: unknown, context?: StagingAutomodReadbackContext) {
  if (!inspectable(input) || ownData(input, "enabled")?.value !== true) return DISABLED;

  const request = record(input, ["enabled", "first", "second"]);
  if (!request) return failed("invalid_input");
  const policy = record(context, ["now", "maxAgeMs", "maxSkewMs", "sources", "promotion"]);
  if (!policy || !time(policy.now) || !time(policy.maxAgeMs) || !time(policy.maxSkewMs)) {
    return failed("invalid_window");
  }
  const { now, maxAgeMs, maxSkewMs, sources, promotion } = policy;
  if (!types.isWeakMap(sources)) return failed("unauthenticated_source");

  const { first, second } = request;
  if (!inspectable(first) || !inspectable(second)) return failed("invalid_readback");
  const a = record(first, READBACK_FIELDS);
  const b = record(second, READBACK_FIELDS);
  if (!a || !b || !Object.isFrozen(first) || !Object.isFrozen(second)) return failed("invalid_readback");
  if (!matches(a.sourceId, 64, /^[A-Za-z0-9][A-Za-z0-9._-]*$/)
    || !matches(b.sourceId, 64, /^[A-Za-z0-9][A-Za-z0-9._-]*$/)
    || WeakMap.prototype.get.call(sources, first) !== a.sourceId
    || WeakMap.prototype.get.call(sources, second) !== b.sourceId) return failed("unauthenticated_source");
  if (a.sourceId === b.sourceId) return failed("duplicate_source");
  if (!fresh(a.observedAt, now, maxAgeMs) || !fresh(b.observedAt, now, maxAgeMs)) return failed("readback_not_fresh");
  if (Math.abs(a.observedAt - b.observedAt) > maxSkewMs) return failed("readback_skew");
  if (!fixedTarget(a.target) || !fixedTarget(b.target)) return failed("target_mismatch");
  if (!validPins(a) || !validPins(b)) return failed("invalid_pins");
  if (!guildOnly(a) || !guildOnly(b)) return failed("command_scope_unconfirmed");
  if (PINS.some(key => a[key] !== b[key])) return failed("readback_mismatch");

  const flag = record(promotion, ["observedAt", "PRODUCTION_AUTO_PROMOTE_DISABLED"]);
  if (flag?.PRODUCTION_AUTO_PROMOTE_DISABLED !== true || !fresh(flag.observedAt, now, maxAgeMs)) {
    return failed("production_auto_promotion_unconfirmed");
  }
  return VERIFIED;
}
