import { admitStagingAutomod } from "./staging-automod.ts";
import {
  inspectStagingAutomod, publishStagingAutomod, removeOwnedStagingAutomod,
} from "./staging-automod-publication.ts";
import { sameLease, StagingAutomodRecovery, type AutomodIntent } from "./staging-automod-recovery.ts";

type Lease = AutomodIntent["lease"];
const DRY_RUN_POLICY = Object.freeze({
  memberIntake: false, enforcement: false, resume: false, registrySync: false, interactionDispatch: false,
  dedicatedIntents: true,
});
type RuntimePolicy = { readonly [K in keyof typeof DRY_RUN_POLICY]: boolean };
type Observation = {
  lease: Lease;
  purpose: "automod-preview";
  running: boolean;
  policy?: RuntimePolicy;
  // Separate bounded authority for cleanup, including after lease expiry.
  cleanupBefore?: number;
};
type DrainEvidence = { lease: Lease; publicationDrained: boolean; startupDrained: boolean };

interface Dependencies {
  enabled?: boolean;
  storage: ConstructorParameters<typeof StagingAutomodRecovery>[0];
  env: Readonly<Parameters<typeof admitStagingAutomod>[1]>;
  get: Parameters<typeof admitStagingAutomod>[3];
  request: Parameters<typeof publishStagingAutomod>[1]["request"];
  // One shared gate across all callers and registry/runtime writers. Every
  // competing use enters through otherUse, including on a fresh instance.
  exclusive<T>(operation: () => Promise<T>): Promise<T>;
  // Read authoritative ownership and sole purpose for the exact original
  // lease. Recovery can return that binding after expiry, but never rebind it.
  observe(lease: Lease): Promise<Observation | undefined>;
  // Fence both kinds of work on this lease and prove earlier remote effects cannot land.
  // Client abort, timeout and local task completion alone cannot prove this.
  fenceAndDrain(lease: Lease, before: number): Promise<DrainEvidence | undefined>;
  stop(lease: Lease, before: number): Promise<boolean>;
  dryRun?: {
    // Effective launch configuration derived by the protected runtime adapter.
    policy: RuntimePolicy;
    // Enforce this policy at launch and refuse after the deadline. An uncertain
    // remote start must remain fenced by recovery even after caller restart.
    start(lease: Lease, policy: typeof DRY_RUN_POLICY, before: number): Promise<void>;
  };
  now?: () => number;
}

const refused = () => new Error("staging_automod_lifecycle_refused");

function isolated(policy: RuntimePolicy | undefined): boolean {
  return policy?.memberIntake === false && policy.enforcement === false && policy.resume === false
    && policy.registrySync === false && policy.interactionDispatch === false && policy.dedicatedIntents === true;
}

/** Dormant coordinator. With the gate held, all work before the journal
 * intent is read-only. Thereafter the durable intent excludes competing use
 * until the recovery journal commits completion. */
export class StagingAutomodLifecycle {
  private readonly deps: Dependencies;
  private readonly journal: StagingAutomodRecovery;
  private readonly now: () => number;

  constructor(deps: Dependencies) {
    this.deps = deps;
    this.journal = new StagingAutomodRecovery(deps.storage);
    this.now = deps.now ?? Date.now;
  }

  otherUse<T>(operation: () => Promise<T>): Promise<T> {
    return this.deps.exclusive(async () => {
      await this.requireIdle();
      return operation();
    });
  }

  publish(input: Parameters<typeof admitStagingAutomod>[0], signal?: AbortSignal) {
    return this.deps.exclusive(async () => {
      if (this.deps.enabled !== true || signal?.aborted) throw refused();
      await this.requireIdle();
      const admission = await admitStagingAutomod(input, this.deps.env,
        () => this.observe(input.lease), this.deps.get, this.now);
      const currentAdmission = async () => {
        const observed = await this.observe(admission.lease);
        return observed.running === false && !signal?.aborted ? admission : undefined;
      };
      const command = await publishStagingAutomod(admission, {
        env: this.deps.env,
        request: (url, init) => {
          if (signal?.aborted) throw refused();
          return this.deps.request(url, init);
        },
        currentAdmission,
        persistIntent: intent => this.journal.persistIntent(intent, currentAdmission, this.now),
        recordCreated: owned => this.journal.recordCreated(owned),
      }, this.now);
      await this.journal.confirmPublication(command);
      return command;
    });
  }

  /** Protected entry within the held window. The ordinary startup path checks
   * otherUse and stays barred until recovery completes. */
  startDryRun(expected: Lease): Promise<void> {
    return this.deps.exclusive(async () => {
      const { dryRun, enabled, env } = this.deps;
      if (enabled !== true || !dryRun || env.TWO_DEPLOY_ENV !== "staging"
        || env.STAGING_RUNTIME_POLICY !== "bounded" || !isolated(dryRun.policy)) throw refused();

      const requireRuntime = async ({ lease, publishBefore }: AutomodIntent, running: boolean) => {
        const observed = await this.observe(lease);
        const at = this.now();
        if (!Number.isSafeInteger(at) || at < lease.acquiredAt || at >= publishBefore || observed.running !== running
          || !isolated(running ? observed.policy : dryRun.policy)) throw refused();
      };
      const intent = await this.journal.claimStartup(expected, intent => requireRuntime(intent, false));
      await requireRuntime(intent, false);
      try { await dryRun.start(intent.lease, DRY_RUN_POLICY, intent.publishBefore); }
      catch { throw refused(); }
      await requireRuntime(intent, true);
    });
  }

  /** Cancellation and expiry supply the exact lease. Restart can resume the
   * pending journal without a live lease. Recovery also works with publishing off. */
  recover(expected?: Lease): Promise<void> {
    return this.deps.exclusive(async () => {
      const pending = await this.journal.pending();
      if (!pending) return;
      const { lease } = pending;
      if (expected !== undefined && !sameLease(lease, expected)) throw refused();
      let prepared: number | undefined;
      await this.journal.recover(lease, {
        inspect: async (intent, command) => {
          if (prepared === undefined) {
            const deadline = await this.cleanupDeadline(lease);
            const drained = await this.deps.fenceAndDrain(lease, deadline).catch(() => { throw refused(); });
            if (drained?.publicationDrained !== true || drained.startupDrained !== true
              || !sameLease(lease, drained.lease) || await this.cleanupDeadline(lease) !== deadline) throw refused();
            if (await this.deps.stop(lease, deadline).catch(() => { throw refused(); }) !== true) throw refused();
            prepared = deadline;
          }
          const deadline = await this.cleanupDeadline(lease, true);
          if (deadline !== prepared) throw refused();
          const registry = await inspectStagingAutomod(intent, command, this.deps, deadline, this.now);
          if (await this.cleanupDeadline(lease, true) !== deadline) throw refused();
          const proof = { intent, authorized: true, stopped: true, publicationDrained: true } as const;
          if (registry === "absent") return { ...proof, registry };
          if (!command) throw refused();
          return { ...proof, registry, command };
        },
        removeOwned: async owned => {
          const removeBefore = await this.cleanupDeadline(lease, true);
          if (removeBefore !== prepared) throw refused();
          const grant = { owned, removeBefore };
          await removeOwnedStagingAutomod(grant, {
            env: this.deps.env, request: (url, init) => this.deps.request(url, init),
            currentCleanupGrant: async () => await this.cleanupDeadline(lease, true) === removeBefore ? grant : undefined,
          }, this.now);
        },
      });
    });
  }

  private async requireIdle(): Promise<void> {
    if (await this.journal.pending()) throw refused();
  }

  private async observe(lease: Lease): Promise<Observation> {
    const observed = await this.deps.observe(lease).catch(() => { throw refused(); });
    if (observed?.purpose !== "automod-preview" || !sameLease(lease, observed.lease)) throw refused();
    return observed;
  }

  private async cleanupDeadline(lease: Lease, requireStopped = false): Promise<number> {
    const observed = await this.observe(lease);
    const deadline = observed.cleanupBefore;
    if ((requireStopped && observed.running !== false) || deadline === undefined
      || !Number.isSafeInteger(deadline) || !(deadline > this.now())) throw refused();
    return deadline;
  }
}
