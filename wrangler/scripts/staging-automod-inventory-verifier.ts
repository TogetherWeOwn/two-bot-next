import { spawn, type ChildProcessByStdio } from "node:child_process";
import type { Readable, Writable } from "node:stream";
import { fileURLToPath, URL } from "node:url";
import { boundRead } from "../src/internal-actions.ts";
import type { bindStagingAutomodInventoryReader } from "./staging-automod-inventory-reader.ts";

type Launch = (
  file: string,
  args: string[],
  options: { env: { GH_TOKEN?: string }; stdio: "pipe"; detached: true },
) => ChildProcessByStdio<Writable, Readable, Readable>;

type VerifierOptions = {
  /** Caller-supplied credential scoped to repository and Actions reads. */
  githubReadToken: string;
};

const SUPERVISOR = fileURLToPath(new URL("../../scripts/staging_automod_verifier_supervisor.py", import.meta.url));
const VERIFIER = fileURLToPath(new URL("../../scripts/staging_automod_inventory_reader.py", import.meta.url));
const MAX_GITHUB_TOKEN_BYTES = 4_096;
const MAX_OUTPUT_BYTES = 4_096;
const active = new WeakMap<AbortSignal, Set<Promise<string>>>();

/** Keep deadline races for provider calls and join any managed verifier on that action's signal. */
export async function boundStagingAutomodInventoryRead<T>(work: Promise<T>, signal: AbortSignal): Promise<T> {
  try {
    return await boundRead(work, signal);
  } finally {
    if (signal.aborted) await Promise.allSettled(active.get(signal) ?? []);
  }
}

/** The trusted launcher supplies piped stdio. The reader supplies validated pins. */
export function bindStagingAutomodInventoryVerifier(
  launch: Launch = spawn,
  options?: VerifierOptions,
): Parameters<typeof bindStagingAutomodInventoryReader>[0] {
  return (workflowSha, runId, signal) => {
    const work = new Promise<string>((resolve, reject) => {
      const refusal = new Error("Staging inventory verifier failed");
      if (signal.aborted) return reject(refusal);
      let child: ReturnType<Launch>;
      try {
        const env: { GH_TOKEN?: string } = {};
        if (options !== undefined) {
          const { githubReadToken } = options;
          // Accept only visible ASCII, so the length bound also caps bytes.
          if (typeof githubReadToken !== "string" || githubReadToken.length === 0
            || githubReadToken.length > MAX_GITHUB_TOKEN_BYTES || /[^!-~]/.test(githubReadToken)) {
            return reject(refusal);
          }
          env.GH_TOKEN = githubReadToken;
        }
        // Node otherwise copies this parent variable even with an explicit env.
        if (process.env.NODE_V8_COVERAGE) {
          Object.defineProperty(env, "NODE_V8_COVERAGE", { value: undefined });
        }
        child = launch("python3", [SUPERVISOR, "python3", VERIFIER, workflowSha, runId],
          { env, stdio: "pipe", detached: true });
      } catch {
        return reject(refusal);
      }
      if (child.pid === undefined) {
        // A descriptor-limit error can arrive without pipes or a close event.
        const rejectLaunch = () => reject(refusal);
        child.once("error", rejectLaunch);
        child.once("close", rejectLaunch);
        return;
      }

      const stdout = Buffer.alloc(MAX_OUTPUT_BYTES);
      let length = 0;
      let stderrLength = 0;
      let refused = false;
      const cancel = () => {
        if (refused) return;
        refused = true;
        // EOF wakes the supervisor even when cancellation precedes its startup.
        child.stdin.end();
      };
      child.stdin.on("error", cancel);
      child.on("error", cancel);
      child.stdout.on("error", cancel);
      child.stderr.on("error", cancel);
      child.stdout.on("data", (chunk: Buffer) => {
        if (refused) return;
        if (length + chunk.length > MAX_OUTPUT_BYTES) return cancel();
        length += chunk.copy(stdout, length);
      });
      child.stderr.on("data", (chunk: Buffer) => {
        if (refused) return;
        stderrLength += chunk.length;
        if (stderrLength > MAX_OUTPUT_BYTES) cancel();
      });
      child.once("close", code => {
        signal.removeEventListener("abort", cancel);
        if (code !== 0 || refused || signal.aborted) return reject(refusal);
        resolve(stdout.toString("utf8", 0, length));
      });
      signal.addEventListener("abort", cancel, { once: true });
      if (signal.aborted) cancel();
    });

    const pending = active.get(signal) ?? new Set<Promise<string>>();
    active.set(signal, pending);
    pending.add(work);
    const forget = () => pending.delete(work);
    void work.then(forget, forget);
    return work;
  };
}
