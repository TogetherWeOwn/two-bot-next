import { boundRead } from "./internal-actions.ts";

const READ_TIMEOUT_MS = 2_000;

/** Wrap authenticated, read-only provider callbacks for readStagingAutomodJobInputs.
 * Each read gets its own deadline. The issuer still validates every returned record
 * and treats a failed lookup as unavailable for that read alone.
 */
export function boundStagingAutomodAuthority<Context, Lease, Action, Approval>(
  provider: Readonly<{
    readIssuedLease: (context: Context, signal: AbortSignal) => Promise<Lease | undefined>;
    readApproval: (action: Action, lease: Lease, signal: AbortSignal) => Promise<Approval | undefined>;
  }>,
  callerSignal?: AbortSignal,
) {
  async function read<T>(lookup: (signal: AbortSignal) => Promise<T>): Promise<T> {
    callerSignal?.throwIfAborted();
    const timeout = AbortSignal.timeout(READ_TIMEOUT_MS);
    const signal = callerSignal ? AbortSignal.any([callerSignal, timeout]) : timeout;
    const record = await boundRead(lookup(signal), signal);
    signal.throwIfAborted();
    return record;
  }

  return {
    readIssuedLease: (context: Context) => read(signal => provider.readIssuedLease(context, signal)),
    readApproval: (action: Action, lease: Lease) => read(signal => provider.readApproval(action, lease, signal)),
  };
}
