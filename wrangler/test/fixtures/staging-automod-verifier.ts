import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import type { bindStagingAutomodInventoryVerifier } from "../../scripts/staging-automod-inventory-verifier.ts";

type Launch = NonNullable<Parameters<typeof bindStagingAutomodInventoryVerifier>[0]>;

export function syntheticVerifier(program: string, observe: (child: ReturnType<Launch>) => void = () => {}): Launch {
  return (file, [supervisor], options) => {
    assert.ok(supervisor);
    const child = spawn(file, [supervisor, process.execPath, "-e", program], options);
    observe(child);
    return child;
  };
}

export function verifierWithDescendant(output: string, { exitCode, holdStdout = false }: {
  exitCode?: number; holdStdout?: boolean;
} = {}): string {
  const descendant = 'process.send(process.pid); setTimeout(() => {}, 7_000);';
  return `
    const { spawn } = require("node:child_process");
    const child = spawn(process.execPath, ["-e", ${JSON.stringify(descendant)}],
      { stdio: ["ignore", ${JSON.stringify(holdStdout ? "inherit" : "ignore")}, "ignore", "ipc"] });
    child.once("message", pid => {
      child.disconnect();
      child.unref();
      process.stdout.write(${JSON.stringify(output)}, () => process.stderr.write(String(pid) + "\\n"));
      ${exitCode === undefined ? "setTimeout(() => {}, 7_000);" : `process.exitCode = ${exitCode};`}
    });
  `;
}
