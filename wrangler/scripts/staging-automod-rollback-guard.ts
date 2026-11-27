import { types } from "node:util";
import { isSnowflake } from "../src/container-env.ts";
import { STAGING_AUTOMOD_TARGET, type CommandRevision } from "../src/staging-automod-recovery.ts";
import type { StagingUse } from "../src/staging-lifecycle.ts";
import { AUTOMOD_STAGING_PURPOSE, MAX_STAGING_USE_MS } from "../src/staging-use-window.ts";

type Target = typeof STAGING_AUTOMOD_TARGET;
type Scope = Readonly<Pick<StagingUse, "runId" | "runAttempt" | "deploymentId" | "epoch">>;
type Pin = Scope & Readonly<{ target: Target; sha: string; command: CommandRevision }>;
type VerifiedPin = Omit<Pin, "target">;

export type StagingAutomodRollbackGuardInput = Readonly<{
  enabled?: boolean;
  now: number;
  pin: Pin;
  promotion: Readonly<{ observedAt: number; PRODUCTION_AUTO_PROMOTE_DISABLED: boolean }>;
  ownership: Readonly<{
    observedAt: number;
    owner: Pick<Scope, "deploymentId" | "epoch">;
    requestedAttempt: Pick<Scope, "runId" | "runAttempt">;
    sha: string;
    use: Readonly<StagingUse> | null;
    runtimeState: "stopped" | "starting" | "running" | "unknown";
    stopVerified: boolean;
    cleanupPending: boolean;
  }>;
  publication: Scope & Readonly<{
    recordedAt: number;
    disposition: "not_published" | "reverted";
    target: Target;
    command: CommandRevision;
  }>;
}>;

const common = { schema: "two-bot-staging-automod-rollback-guard.v1", environment: "staging" } as const;
const disabled = Object.freeze({ ...common, verdict: "disabled" } as const);
const passed = Object.freeze({ ...common, verdict: "passed" } as const);
const MAX_AGE_MS = 60_000;
const MAX_TIME_MS = 8_640_000_000_000_000;
const PUBLICATION_KEYS = ["recordedAt", "disposition", "target", "command", "runId", "runAttempt", "deploymentId", "epoch"];

type FailureCode = "invalid_clock" | "staging_pin_invalid" | "production_auto_promotion_unconfirmed"
  | "staging_ownership_unconfirmed" | "staging_lease_unconfirmed" | "staging_publication_unconfirmed";

function failed(code: FailureCode) {
  return Object.freeze({ ...common, verdict: "failed", code } as const);
}

function inspectable(value: unknown): value is object {
  return value !== null && typeof value === "object" && !types.isProxy(value) && !types.isModuleNamespaceObject(value);
}

function record(value: unknown): value is object {
  if (!inspectable(value) || Array.isArray(value)) return false;
  const prototype: unknown = Object.getPrototypeOf(value);
  return prototype === null || prototype === Object.prototype;
}

function ownData(value: unknown, key: string): unknown {
  if (!inspectable(value)) return;
  const own = Object.getOwnPropertyDescriptor(value, key);
  if (own && Object.hasOwn(own, "value")) return own.value;
}

function field(value: unknown, key: string): unknown {
  return record(value) ? ownData(value, key) : undefined;
}

function exactRecord(value: unknown, keys: readonly string[]): boolean {
  return record(value) && Reflect.ownKeys(value).length === keys.length
    && keys.every(key => Object.hasOwn(value, key));
}

function time(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 && value <= MAX_TIME_MS;
}

function fresh(value: unknown, now: number): value is number {
  return time(value) && value <= now && now - value <= MAX_AGE_MS;
}

function positiveInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value > 0;
}

function targetMatches(value: unknown): boolean {
  const { applicationId, guildId, name } = STAGING_AUTOMOD_TARGET;
  return exactRecord(value, ["applicationId", "guildId", "name"]) && field(value, "applicationId") === applicationId
    && field(value, "guildId") === guildId && field(value, "name") === name;
}

function snowflake(value: unknown): value is string {
  return typeof value === "string" && /^[1-9][0-9]{16,19}$/.test(value) && isSnowflake(value);
}

function readPin(value: unknown): VerifiedPin | undefined {
  const runId = field(value, "runId");
  const runAttempt = field(value, "runAttempt");
  const deploymentId = field(value, "deploymentId");
  const epoch = field(value, "epoch");
  const sha = field(value, "sha");
  const command = field(value, "command");
  const id = field(command, "id");
  const version = field(command, "version");
  if (typeof runId !== "string" || !/^[1-9][0-9]{0,19}$/.test(runId) || !positiveInteger(runAttempt)
    || typeof deploymentId !== "string" || !/^[a-zA-Z0-9_-]{1,128}$/.test(deploymentId) || !positiveInteger(epoch)
    || typeof sha !== "string" || !/^[0-9a-f]{40}$/.test(sha) || !targetMatches(field(value, "target"))
    || !exactRecord(command, ["id", "version"]) || !snowflake(id) || !snowflake(version)) return;
  return { runId, runAttempt, deploymentId, epoch, sha, command: { id, version } };
}

function sameAttempt(value: unknown, pin: Scope): boolean {
  return field(value, "runId") === pin.runId && field(value, "runAttempt") === pin.runAttempt;
}

function sameDeployment(value: unknown, pin: Scope): boolean {
  return field(value, "deploymentId") === pin.deploymentId && field(value, "epoch") === pin.epoch;
}

function expiredLease(value: unknown, pin: VerifiedPin, observedAt: number): boolean {
  const acquiredAt = field(value, "acquiredAt");
  const expiresAt = field(value, "expiresAt");
  const phase = field(value, "phase");
  return record(value) && !Object.hasOwn(value, "voiceMove") && field(value, "environment") === "staging"
    && field(value, "purpose") === AUTOMOD_STAGING_PURPOSE && (phase === "active" || phase === "released")
    && sameAttempt(value, pin) && sameDeployment(value, pin) && field(value, "sha") === pin.sha
    && time(acquiredAt) && time(expiresAt) && acquiredAt < expiresAt && expiresAt <= observedAt
    && expiresAt - acquiredAt <= MAX_STAGING_USE_MS;
}

function publicationMatches(value: unknown, pin: VerifiedPin, now: number): boolean {
  if (!exactRecord(value, PUBLICATION_KEYS)) return false;
  const disposition = field(value, "disposition");
  const command = field(value, "command");
  return (disposition === "not_published" || disposition === "reverted")
    && fresh(field(value, "recordedAt"), now) && sameAttempt(value, pin) && sameDeployment(value, pin)
    && targetMatches(field(value, "target")) && exactRecord(command, ["id", "version"])
    && field(command, "id") === pin.command.id && field(command, "version") === pin.command.version;
}

/** The caller authenticates the pin and supplies coherent readbacks from under
 * the existing staging writer fence. Publication is a complete scoped
 * disposition for the pinned command revision, including when absent. */
export function evaluateStagingAutomodRollbackGuard(input?: unknown) {
  if (ownData(input, "enabled") !== true) return disabled;

  const now = field(input, "now");
  if (!time(now)) return failed("invalid_clock");
  const pin = readPin(field(input, "pin"));
  if (!pin) return failed("staging_pin_invalid");

  const promotion = field(input, "promotion");
  if (field(promotion, "PRODUCTION_AUTO_PROMOTE_DISABLED") !== true || !fresh(field(promotion, "observedAt"), now)) {
    return failed("production_auto_promotion_unconfirmed");
  }

  const ownership = field(input, "ownership");
  const observedAt = field(ownership, "observedAt");
  if (!fresh(observedAt, now) || !sameDeployment(field(ownership, "owner"), pin)
    || !sameAttempt(field(ownership, "requestedAttempt"), pin) || field(ownership, "sha") !== pin.sha) {
    return failed("staging_ownership_unconfirmed");
  }
  const use = field(ownership, "use");
  if (field(ownership, "stopVerified") !== true || field(ownership, "cleanupPending") !== false
    || field(ownership, "runtimeState") !== "stopped" || (use !== null && !expiredLease(use, pin, observedAt))) {
    return failed("staging_lease_unconfirmed");
  }

  return publicationMatches(field(input, "publication"), pin, now) ? passed : failed("staging_publication_unconfirmed");
}
