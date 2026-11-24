import { isSnowflake } from "./container-env.ts";
import { MAX_STAGING_USE_MS, type StagingUse } from "./staging-lifecycle.ts";
import { readPreviewAbsent, stagingPreviewScope, type PreviewEnv } from "./staging-preview.ts";

type Window = Readonly<StagingUse & { voiceMove?: true }>;
type BootEnv = PreviewEnv & {
  TWO_DEPLOY_ENV?: string;
  STAGING_RUNTIME_POLICY?: string;
  TWO_AUTOMOD?: string;
  TWO_MODERATION?: string;
  TWO_OWEN_USER_ID?: string;
  TWO_MODERATION_PROTECTED_ROLE_IDS?: string;
};

type Admission = {
  origin: string;
  purposes: readonly string[];
  lease: Window;
  verifiedRecoveryPair: boolean;
  verifiedRuntimeDatabase: boolean;
  verifiedGatewayIntents: boolean;
  cleanupReserveMs: number;
};

type Observation = { lease: Window; running: boolean };
type DiscordGet = (url: string, token: string, signal: AbortSignal) => Promise<unknown>;

function moderationId(value: string): boolean {
  return value === value.trim() && /^[1-9][0-9]{16,19}$/.test(value) && isSnowflake(value);
}

function sameLease(a: Window, b: Window, now: number): boolean {
  return a.environment === "staging" && b.environment === a.environment
    && a.phase === "active" && b.phase === a.phase && a.voiceMove === undefined && b.voiceMove === undefined
    && a.deploymentId === b.deploymentId && a.sha === b.sha && a.epoch === b.epoch
    && a.runId === b.runId && a.runAttempt === b.runAttempt
    && a.acquiredAt === b.acquiredAt && a.expiresAt === b.expiresAt
    && Number.isSafeInteger(a.acquiredAt) && Number.isSafeInteger(a.expiresAt)
    && a.acquiredAt >= 0 && a.acquiredAt <= now && now < a.expiresAt
    && a.expiresAt - a.acquiredAt <= MAX_STAGING_USE_MS;
}

/** Dormant preflight. The protected DO must hold its ownership gate and supply
 * current authoritative lease/stopped reads and its bounded Discord GET adapter.
 * This result can be persisted as baseline evidence; it grants no boot or write. */
export async function admitStagingAutomod(
  input: Admission, env: BootEnv, observe: () => Promise<Observation>, get: DiscordGet,
  now = Date.now,
): Promise<{ lease: Window; baselineAbsent: true; publishBefore: number }> {
  const lease = Object.freeze({ ...input.lease });
  const { cleanupReserveMs } = input;
  const publishBefore = lease.expiresAt - cleanupReserveMs;
  const refuse = () => new Error("staging_automod_admission_refused");
  const roles = env.TWO_MODERATION_PROTECTED_ROLE_IDS ?? "";
  if (input.origin !== input.origin.trim() || !/^https:\/\/two-bot-next-staging\.[a-z0-9-]+\.workers\.dev$/.test(input.origin)
    || env.TWO_DEPLOY_ENV !== "staging" || env.STAGING_RUNTIME_POLICY !== "bounded"
    || !stagingPreviewScope(env)
    || input.purposes.length !== 1 || input.purposes[0] !== "automod-preview"
    || env.TWO_AUTOMOD === "1" || env.TWO_MODERATION !== "1" || !moderationId(env.TWO_OWEN_USER_ID ?? "")
    || (roles !== "" && !roles.split(",").every(moderationId))
    || input.verifiedRecoveryPair !== true || input.verifiedRuntimeDatabase !== true
    || input.verifiedGatewayIntents !== true || !Number.isSafeInteger(cleanupReserveMs)
    || cleanupReserveMs <= 0) throw refuse();

  const requireStopped = async () => {
    const observed = await observe();
    const at = now();
    if (observed.running !== false || !sameLease(lease, observed.lease, at) || at >= publishBefore) throw refuse();
    return at;
  };
  const at = await requireStopped();
  const signal = AbortSignal.timeout(Math.min(publishBefore - at, 8_000));
  try {
    if (await readPreviewAbsent(env, (url, token) => {
      signal.throwIfAborted();
      return get(url, token, signal);
    }) >= 100 || signal.aborted) throw refuse();
  } catch {
    throw refuse();
  }
  await requireStopped();
  return { lease, baselineAbsent: true, publishBefore };
}
