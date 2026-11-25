import type { runProtectedAutomodJob } from "./staging-automod-job.ts";
import { sameLease, STAGING_AUTOMOD_TARGET } from "./staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "./staging-use-window.ts";

type Approval = NonNullable<Parameters<typeof runProtectedAutomodJob>[1]>;
type Lease = Approval["issuedLease"];
type Action = Approval["action"];
type Context = Readonly<Pick<Lease, "environment" | "deploymentId" | "sha" | "runId" | "runAttempt">> &
  Readonly<{ target: Approval["target"] }>;

type ProtectedAuthority = Readonly<{
  readIssuedLease: (context: Context) => Promise<Lease | undefined>;
  readApproval: (action: Action, issuedLease: Lease) => Promise<Approval | undefined>;
}>;

function fixedTarget(target: Approval["target"]): boolean {
  return target?.applicationId === STAGING_AUTOMOD_TARGET.applicationId
    && target.guildId === STAGING_AUTOMOD_TARGET.guildId;
}

async function readApproval(authority: ProtectedAuthority, action: Action, issuedLease: Lease) {
  try {
    const record = await authority.readApproval(action, issuedLease);
    if (record?.action === action && fixedTarget(record.target)
      && record.issuedLease?.purpose === AUTOMOD_STAGING_PURPOSE && sameLease(issuedLease, record.issuedLease)) {
      return Object.freeze({ action, target: STAGING_AUTOMOD_TARGET, issuedLease });
    }
  } catch {
    // An unavailable approval denies only its own action.
  }
}

/** The caller supplies authenticated job context and protected readbacks.
 * Read the original lease issued by staging lifecycle and each action's own
 * approval. Provider authentication and lease issuance precede this adapter.
 */
export async function readStagingAutomodJobInputs(expected: Context | undefined, authority: ProtectedAuthority) {
  if (!expected) return;
  const { environment, deploymentId, sha, runId, runAttempt, target } = expected;
  if (environment !== "staging" || runAttempt !== 1 || !fixedTarget(target)) return;
  const context = Object.freeze({ environment, deploymentId, sha, runId, runAttempt, target: STAGING_AUTOMOD_TARGET });

  let lease: Lease | undefined;
  try { lease = await authority.readIssuedLease(context); }
  catch { return; }
  if (lease?.purpose !== AUTOMOD_STAGING_PURPOSE || !sameLease(lease, lease)
    || lease.deploymentId !== deploymentId || lease.sha !== sha
    || lease.runId !== runId || lease.runAttempt !== runAttempt) return;

  const { epoch, acquiredAt, expiresAt } = lease;
  const issuedLease = Object.freeze({ environment, purpose: AUTOMOD_STAGING_PURPOSE, phase: "active" as const,
    deploymentId, sha, runId, runAttempt, epoch, acquiredAt, expiresAt });
  const [start, recover] = await Promise.all([
    readApproval(authority, "start", issuedLease),
    readApproval(authority, "recover", issuedLease),
  ]);
  const approvals = Object.freeze({ ...(start && { start }), ...(recover && { recover }) });
  return Object.freeze({ context, issuedLease, approvals });
}
