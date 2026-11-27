import { types } from "node:util";

type FreshnessInput = Readonly<{
  enabled?: boolean;
  bundle?: unknown;
  lease?: unknown;
  now?: number;
  maxLeaseWindowMs?: number;
}>;

const MAX_DATE_MS = 8_640_000_000_000_000;
const MAX_CLOCK_SKEW_MS = 5_000;
const MAX_OBSERVATION_AGE_MS = 60_000; // Same age limit as the receipt verifier.
const DEFAULT_MAX_LEASE_WINDOW_MS = 300_000;

const DISABLED = Object.freeze({ verdict: "disabled" } as const);
const FAILED = Object.freeze({ verdict: "failed" } as const);
const PASSED = Object.freeze({ verdict: "passed" } as const);

function dataField(value: object, key: PropertyKey): unknown {
  const descriptor = Object.getOwnPropertyDescriptor(value, key);
  return descriptor && Object.hasOwn(descriptor, "value") ? descriptor.value : undefined;
}

function inspectable(value: unknown): value is object {
  return value !== null && typeof value === "object" && !types.isProxy(value);
}

function plainData(value: unknown): value is object {
  if (!inspectable(value) || Array.isArray(value)) return false;
  const prototype: unknown = Object.getPrototypeOf(value);
  return (prototype === Object.prototype || prototype === null)
    && Reflect.ownKeys(value).every(key => {
      const descriptor = Object.getOwnPropertyDescriptor(value, key);
      return descriptor !== undefined && Object.hasOwn(descriptor, "value");
    });
}

function epochMs(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function observationMs(value: unknown): number {
  // Match the composer's canonical UTC format, including extended years.
  if (typeof value !== "string" || (value.length !== 24 && value.length !== 27)) return NaN;
  const parsed = Date.parse(value);
  return epochMs(parsed) && new Date(parsed).toISOString() === value ? parsed : NaN;
}

/** Check the time bounds of a composed bundle. The caller supplies the trusted
 * clock and the independently pinned lease for that bundle.
 */
export function verifyStagingAutomodFreshness(input?: FreshnessInput) {
  if (!inspectable(input) || dataField(input, "enabled") !== true) return DISABLED;
  if (!plainData(input)) return FAILED;

  const bundle = dataField(input, "bundle");
  const lease = dataField(input, "lease");
  const now = dataField(input, "now");
  const configuredWindow = dataField(input, "maxLeaseWindowMs");
  const maxLeaseWindowMs = configuredWindow === undefined ? DEFAULT_MAX_LEASE_WINDOW_MS : configuredWindow;
  if (!epochMs(now) || now > MAX_DATE_MS || !epochMs(maxLeaseWindowMs) || maxLeaseWindowMs === 0
    || !plainData(bundle) || !plainData(lease)) return FAILED;
  if (dataField(bundle, "schema") !== "two-bot-staging-automod-qualification.v1"
    || dataField(bundle, "environment") !== "staging" || dataField(bundle, "verdict") !== "passed") return FAILED;

  const observedAt = observationMs(dataField(bundle, "observedAt"));
  const acquiredAt = dataField(lease, "acquiredAt");
  const expiresAt = dataField(lease, "expiresAt");
  if (!epochMs(observedAt) || !epochMs(acquiredAt) || !epochMs(expiresAt)) return FAILED;
  return observedAt - now <= MAX_CLOCK_SKEW_MS && now - observedAt <= MAX_OBSERVATION_AGE_MS
    && acquiredAt <= observedAt && observedAt < expiresAt && now < expiresAt
    && expiresAt - acquiredAt <= maxLeaseWindowMs ? PASSED : FAILED;
}
