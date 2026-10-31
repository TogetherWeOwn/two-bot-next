import { realpathSync } from "node:fs";
import { fileURLToPath } from "node:url";

// Match the canonical-path guard in promotion-error-gate.mjs without importing
// its gate or TypeScript dependencies. Decode file URLs before resolving paths
// so symlinks and percent-escaped filenames run the CLI, but imports stay inert.
export function isDirectInvocation(argv1, moduleUrl) {
  try {
    if (typeof argv1 !== "string" || argv1 === "") return false;
    return realpathSync(argv1) === realpathSync(fileURLToPath(moduleUrl));
  } catch {
    return false;
  }
}
