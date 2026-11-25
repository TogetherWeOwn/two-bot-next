import { OwnershipRefused } from "./ownership.ts";
import { StagingAutomodLifecycle } from "./staging-automod-lifecycle.ts";
import { sameLease, StagingAutomodRecovery } from "./staging-automod-recovery.ts";
import {
  AUTOMOD_STAGING_PURPOSE, type StagingIdentity, type StagingLifecycle, type StagingUse,
} from "./staging-lifecycle.ts";

type Lease = Readonly<StagingUse>;
type Lifecycle = ConstructorParameters<typeof StagingAutomodLifecycle>[0];
type RuntimePolicy = NonNullable<Awaited<ReturnType<Lifecycle["observe"]>>>["policy"];

export type ProtectedAutomodCall = Readonly<{ action: "start" | "recover"; issuedLease: Lease }>;

/** Trusted adapters supplied by the eventual protected workflow caller.
 * Runtime and cleanup readbacks each identify their own issued lease. */
export type ProtectedAutomodEffects = Pick<Lifecycle, "enabled" | "dryRun" | "fenceAndDrain" | "stop" | "request"> & {
  readRuntime(lease: Lease): Promise<{ lease: Lease; running: boolean; policy?: RuntimePolicy } | undefined>;
  readCleanupGrant(lease: Lease): Promise<{ lease: Lease; before: number } | undefined>;
};

type Authority = Pick<Lifecycle, "storage" | "env" | "exclusive"> & {
  staging: StagingLifecycle;
  identity(): Promise<StagingIdentity>;
};

function refused(): OwnershipRefused {
  return new OwnershipRefused("staging_use_conflict");
}

function boundTo(expected: Lease, observed: Lease | undefined): boolean {
  return observed?.purpose === AUTOMOD_STAGING_PURPOSE && sameLease(expected, observed);
}

export async function runProtectedAutomod(call: ProtectedAutomodCall, authority: Authority,
  effects: ProtectedAutomodEffects): Promise<void> {
  const { storage, env, staging } = authority;
  const { action } = call;
  const { dryRun } = effects;
  const enabled = effects.enabled === true;
  if (env.TWO_DEPLOY_ENV !== "staging" || env.STAGING_RUNTIME_POLICY !== "bounded"
    || (action !== "start" && action !== "recover")
    || (action === "start" && !enabled)) throw refused();

  const issued = Object.freeze({ ...call.issuedLease });
  const journal = new StagingAutomodRecovery(storage);
  const binding = async (expected: Lease) => {
    const bound = await staging.automodBinding(issued, await authority.identity(), Date.now());
    if (!sameLease(bound.lease, expected)) throw refused();
    return bound;
  };
  const lifecycle = new StagingAutomodLifecycle({
    storage, env, enabled,
    ...(dryRun === undefined ? {} : { dryRun: {
      policy: dryRun.policy,
      start: async (expected, policy, before) => {
        const { lease, active } = await binding(expected);
        if (!active || !(Date.now() < before)) throw refused();
        await dryRun.start(lease, policy, before);
      },
    } }),
    exclusive: operation => authority.exclusive(async () => {
      const bound = await binding(issued);
      if (action === "start" && !bound.active) throw refused();
      const result = await operation();
      if (action === "recover") {
        // A missing journal is not successful cleanup. The same-lease
        // completion receipt also covers retry after an interrupted release.
        if (!await journal.completedFor(issued)) throw refused();
        await staging.releaseAutomod(issued, await authority.identity());
      }
      return result;
    }),
    observe: async expected => {
      const { lease, active } = await binding(expected);
      const cleanup = action === "recover" ? await effects.readCleanupGrant(lease) : undefined;
      if (action === "recover" && (!cleanup || !boundTo(lease, cleanup.lease)
        || !Number.isSafeInteger(cleanup.before) || !(Date.now() < cleanup.before))) throw refused();
      const runtime = await effects.readRuntime(lease);
      if (!runtime || !boundTo(lease, runtime.lease) || typeof runtime.running !== "boolean") throw refused();
      return { lease, purpose: AUTOMOD_STAGING_PURPOSE, active, running: runtime.running,
        ...(runtime.policy === undefined ? {} : { policy: runtime.policy }),
        ...(cleanup === undefined ? {} : { cleanupBefore: cleanup.before }) };
    },
    fenceAndDrain: async (expected, before) => {
      const { lease } = await binding(expected);
      if (!(Date.now() < before)) throw refused();
      const drained = await effects.fenceAndDrain(lease, before);
      return drained && boundTo(lease, drained.lease) ? drained : undefined;
    },
    stop: async (expected, before) => {
      const { lease } = await binding(expected);
      if (!(Date.now() < before)) throw refused();
      return effects.stop(lease, before);
    },
    // This caller handles only startup and cleanup. It has no publication path.
    get: async () => { throw refused(); },
    request: (url, init) => {
      if (action !== "recover" || (init.method !== "GET" && init.method !== "DELETE")) throw refused();
      return effects.request(url, init);
    },
  });
  if (action === "start") await lifecycle.startDryRun(issued);
  else await lifecycle.recover(issued);
}
