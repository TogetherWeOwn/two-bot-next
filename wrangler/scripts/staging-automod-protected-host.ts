import { runStagingAutomodOperatorEntry } from "./staging-automod-operator-entry.ts";

type Entry = Parameters<typeof runStagingAutomodOperatorEntry>;
type Providers = Entry[2];
type Inventory = NonNullable<Awaited<ReturnType<Providers["readInventory"]>>>;
type InventorySource = Readonly<{ workflowSha: string; runId: number }>;
type InventoryReadback = Inventory & Readonly<{ workflow_sha: string; workflow_run_id: number }>;
type Options = Entry[0] & Readonly<{ inventorySource?: InventorySource }>;
type Host = Pick<Providers, "authority" | "readWorker"> & Readonly<{
  readRun: Entry[1];
  readAuthenticatedInventory: (source: InventorySource, signal: AbortSignal) => Promise<InventoryReadback | undefined>;
  invokePrivate: Providers["invoke"];
}>;

/** Compose the dormant entry in the protected host. The approved plan pins
 * the serving selection and the staging inventory producer's workflow SHA
 * and run ID. The host authenticates that producer's artifact before returning
 * its v1 readback, and supplies the existing authority and local Worker bindings.
 * invokePrivate must resolve the exact private instance from the transport pin.
 */
export async function runStagingAutomodProtectedHost(options: Options = {}, host?: Host) {
  if (options.enabled !== true) return { verdict: "disabled" } as const;
  const { inventorySource } = options;
  const { workflowSha, runId } = inventorySource ?? {};
  if (!host || typeof workflowSha !== "string" || !/^[0-9a-f]{40}$/.test(workflowSha)
    || typeof runId !== "number" || !Number.isSafeInteger(runId) || runId <= 0) {
    return { verdict: "failed", code: "context_unconfirmed" } as const;
  }
  const source = Object.freeze({ workflowSha, runId });
  return runStagingAutomodOperatorEntry(options, (id, signal) => host.readRun(id, signal), {
    authority: {
      readIssuedLease: (context, signal) => host.authority.readIssuedLease(context, signal),
      readApproval: (action, lease, signal) => host.authority.readApproval(action, lease, signal),
    },
    readWorker: () => host.readWorker(),
    readInventory: async signal => {
      const inventory = await host.readAuthenticatedInventory(source, signal);
      if (inventory?.workflow_sha === source.workflowSha && inventory.workflow_run_id === source.runId) return inventory;
    },
    invoke: (pin, call, signal) => host.invokePrivate(pin, call, signal),
  });
}
