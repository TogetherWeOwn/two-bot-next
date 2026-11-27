import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, type TestContext } from "node:test";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const runner = fileURLToPath(new URL("./fixtures/staging-voice-move-entry-output.ts", import.meta.url));
const runChild = promisify(execFile);

async function assertEntryOutput(t: TestContext, scenario: string, verdict: string, runId?: string) {
  const dir = mkdtempSync(join(tmpdir(), "staging-voice-move-entry-output-"));
  t.after(() => rmSync(dir, { recursive: true }));
  const outputPath = join(dir, "output");
  const summaryPath = join(dir, "summary");
  const originalOutput = Buffer.from("existing_output=synthetic\n");
  const originalSummary = Buffer.from("Existing synthetic summary\n");
  writeFileSync(outputPath, originalOutput);
  writeFileSync(summaryPath, originalSummary);

  const result = await runChild(process.execPath, runId ? [runner, scenario, runId] : [runner, scenario], {
    env: {
      GITHUB_OUTPUT: outputPath,
      GITHUB_STEP_SUMMARY: summaryPath,
      STAGING_USE_RECEIPT: join(dir, "ready.jsonl"),
      ...(scenario === "disabled" ? {} : { STAGING_VOICE_MOVE_PROOF: "true" }),
    },
    encoding: "utf8",
    timeout: 15_000,
  });

  assert.equal(result.stdout, `${verdict}\n`);
  assert.equal(result.stderr, "");
  assert.deepEqual(readFileSync(outputPath), originalOutput);
  assert.deepEqual(readFileSync(summaryPath), originalSummary);
}

test("default-off workflow entry leaves GitHub output and summary untouched",
  (t) => assertEntryOutput(t, "disabled", "disabled"));

test("workflow entry keeps provider refusal details out of GitHub output and summary",
  (t) => assertEntryOutput(t, "provider-refused", "failed"));

test("workflow entry reports failure when the final OFF readback is unconfirmed",
  (t) => assertEntryOutput(t, "cleanup-uncertain", "failed"));

test("coherent synthetic workflow entry returns only the bounded mock verdict",
  (t) => assertEntryOutput(t, "coherent", "passed"));

test("failed restart readiness stays redacted alongside an independent voice runner completion", async (t) => {
  const results = await Promise.allSettled([
    assertEntryOutput(t, "restart-failed", "failed", "12345"),
    assertEntryOutput(t, "restart-ready", "passed", "12346"),
  ]);
  const failures = results.flatMap((result) => result.status === "rejected" ? [result.reason] : []);
  if (failures.length) throw new AggregateError(failures, "restart runner assertions failed");
});
