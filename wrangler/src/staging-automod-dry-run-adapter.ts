import { boundRead } from "./internal-actions.ts";
import type { ProtectedAutomodEffects } from "./staging-automod-caller.ts";
import { sameLease } from "./staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "./staging-use-window.ts";

type Adapter = Pick<ProtectedAutomodEffects, "dryRun" | "readRuntime" | "fenceAndDrain" | "stop">;
type Start = NonNullable<Adapter["dryRun"]>["start"];
type Lease = Parameters<Start>[0];
type RuntimePolicy = NonNullable<Adapter["dryRun"]>["policy"];
type Runtime = ReturnType<Adapter["readRuntime"]>;
type Drain = ReturnType<Adapter["fenceAndDrain"]>;

const ISOLATED_POLICY = Object.freeze({
  memberIntake: false, enforcement: false, resume: false, registrySync: false,
  interactionDispatch: false, dedicatedIntents: true,
} satisfies Parameters<Start>[1]);
const POLICY_KEYS = Object.keys(ISOLATED_POLICY) as (keyof RuntimePolicy)[];
const RUNTIME_READ_TIMEOUT_MS = 8_000;
const refused = () => new Error("staging_automod_runtime_unverified");

interface Transport {
  plannedPolicy?: RuntimePolicy;
  // The remote launcher must enforce the policy, lease and deadline at boot.
  // Resolve true only for an acknowledged launch. Recovery owns uncertain starts.
  launch(...args: [...Parameters<Start>, signal: AbortSignal]): Promise<true | undefined>;
  // Query effective runtime state independently of launch and stop replies. The
  // runtime must identify its own lease. A local container flag is insufficient.
  readRuntime(lease: Lease, signal: AbortSignal): Runtime;
  // Fence remote writers for this lease. Return affirmative evidence only once
  // prior publication and startup can no longer land, even after caller restart.
  fenceAndDrainRemote(lease: Lease, before: number, signal: AbortSignal): Drain;
  stop(lease: Lease, before: number, signal: AbortSignal): Promise<boolean>;
}

function isolated(policy: RuntimePolicy | undefined): boolean {
  return policy != null && typeof policy === "object" && Reflect.ownKeys(policy).length === POLICY_KEYS.length
    && POLICY_KEYS.every(key => Object.hasOwn(policy, key) && policy[key] === ISOLATED_POLICY[key]);
}

function matchesLease(expected: Lease, actual: Lease | undefined): boolean {
  return actual?.purpose === AUTOMOD_STAGING_PURPOSE && sameLease(expected, actual);
}

function issuedLease(lease: Lease): Lease {
  if (!matchesLease(lease, lease)) throw refused();
  const snapshot = Object.freeze({ ...lease });
  if (!matchesLease(lease, snapshot)) throw refused();
  return snapshot;
}

/** Effects for the protected caller, which owns authorization and the durable
 * hold. A failed effect leaves that hold available for protected recovery. */
export function createStagingAutomodDryRunAdapter(transport: Transport, now = Date.now): Adapter {
  function timeLeft(lease: Lease, before: number): number {
    const at = now();
    if (!Number.isSafeInteger(before) || !Number.isSafeInteger(at) || at < lease.acquiredAt || at >= before) throw refused();
    return before - at;
  }

  async function bounded<T>(lease: Lease, before: number, work: (signal: AbortSignal) => Promise<T>): Promise<T> {
    const signal = AbortSignal.timeout(Math.min(timeLeft(lease, before), 2_147_483_647));
    const result = await boundRead(work(signal), signal);
    signal.throwIfAborted();
    timeLeft(lease, before);
    return result;
  }

  async function readRuntime(lease: Lease, signal: AbortSignal): Runtime {
    const observed = await transport.readRuntime(lease, signal);
    const running = observed?.running;
    if (!observed || !matchesLease(lease, observed.lease) || typeof running !== "boolean") return;
    return { lease, running, ...(isolated(observed.policy) ? { policy: ISOLATED_POLICY } : {}) };
  }

  const start: Start = async (expected, policy, before) => {
    try {
      const lease = issuedLease(expected);
      if (!isolated(transport.plannedPolicy) || !isolated(policy) || before >= lease.expiresAt) throw refused();
      const acknowledged = await bounded(lease, before, signal => transport.launch(lease, ISOLATED_POLICY, before, signal));
      if (acknowledged !== true) throw refused();
      const observed = await bounded(lease, before, signal => readRuntime(lease, signal));
      if (observed?.running !== true || observed.policy !== ISOLATED_POLICY) throw refused();
    } catch { throw refused(); }
  };

  return {
    ...(isolated(transport.plannedPolicy) ? { dryRun: { policy: ISOLATED_POLICY, start } } : {}),
    readRuntime: async expected => {
      try {
        const signal = AbortSignal.timeout(RUNTIME_READ_TIMEOUT_MS);
        return await boundRead(readRuntime(issuedLease(expected), signal), signal);
      } catch { return undefined; }
    },
    fenceAndDrain: async (expected, before) => {
      try {
        const lease = issuedLease(expected);
        const proof = await bounded(lease, before, signal => transport.fenceAndDrainRemote(lease, before, signal));
        if (proof?.publicationDrained !== true || proof.startupDrained !== true || !matchesLease(lease, proof.lease)) return;
        return { lease, publicationDrained: true, startupDrained: true };
      } catch { return undefined; }
    },
    stop: async (expected, before) => {
      try {
        const lease = issuedLease(expected);
        const acknowledged = await bounded(lease, before, signal => transport.stop(lease, before, signal));
        if (acknowledged !== true) return false;
        const observed = await bounded(lease, before, signal => readRuntime(lease, signal));
        return observed?.running === false;
      } catch { return false; }
    },
  };
}
