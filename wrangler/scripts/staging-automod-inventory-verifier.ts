import type { ExecFileException, ExecFileOptionsWithBufferEncoding } from "node:child_process";
import { fileURLToPath, URL } from "node:url";
import type { bindStagingAutomodInventoryReader } from "./staging-automod-inventory-reader.ts";

type Execute = (
  file: string,
  args: string[],
  options: ExecFileOptionsWithBufferEncoding,
  callback: (error: ExecFileException | null, stdout: Buffer) => void,
) => void;

const VERIFIER = fileURLToPath(new URL("../../scripts/staging_automod_inventory_reader.py", import.meta.url));
const MAX_OUTPUT_BYTES = 4_096;

/** The injected executor uses execFile semantics. The reader supplies validated pins. */
export function bindStagingAutomodInventoryVerifier(
  execute: Execute,
): Parameters<typeof bindStagingAutomodInventoryReader>[0] {
  return (workflowSha, runId, signal) => new Promise((resolve, reject) => {
    const refusal = new Error("Staging inventory verifier failed");
    if (signal.aborted) return reject(refusal);
    try {
      execute("python3", [VERIFIER, workflowSha, runId], {
        encoding: "buffer",
        env: {},
        maxBuffer: MAX_OUTPUT_BYTES,
        signal,
      }, (error, stdout) => {
        // execFile can supply partial stdout alongside an error.
        if (error || signal.aborted || stdout.length > MAX_OUTPUT_BYTES) return reject(refusal);
        resolve(stdout.toString("utf8"));
      });
    } catch {
      reject(refusal);
    }
  });
}
