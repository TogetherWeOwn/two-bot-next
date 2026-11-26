import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test, type TestContext } from "node:test";
import { fileURLToPath } from "node:url";

const runner = fileURLToPath(new URL("./fixtures/staging-voice-move-entry-output.ts", import.meta.url));

function assertEntryOutput(t: TestContext, scenario: string, verdict: string) {
  const dir = mkdtempSync(join(tmpdir(), "staging-voice-move-entry-output-"));
  t.after(() => rmSync(dir, { recursive: true }));
  const outputPath = join(dir, "output");
  const summaryPath = join(dir, "summary");
  const originalOutput = Buffer.from("existing_output=synthetic\n");
  const originalSummary = Buffer.from("Existing synthetic summary\n");
  writeFileSync(outputPath, originalOutput);
  writeFileSync(summaryPath, originalSummary);

  const result = spawnSync(process.execPath, [runner, scenario], {
    env: {
      GITHUB_OUTPUT: outputPath,
      GITHUB_STEP_SUMMARY: summaryPath,
      STAGING_USE_RECEIPT: join(dir, "ready.jsonl"),
      ...(scenario === "disabled" ? {} : { STAGING_VOICE_MOVE_PROOF: "true" }),
    },
    encoding: "utf8",
    timeout: 5000,
  });

  assert.ifError(result.error);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.stdout, `${verdict}\n`);
  assert.equal(result.stderr, "");
  assert.deepEqual(readFileSync(outputPath), originalOutput);
  assert.deepEqual(readFileSync(summaryPath), originalSummary);
}

test("default-off workflow entry leaves GitHub output and summary untouched", (t) => {
  assertEntryOutput(t, "disabled", "disabled");
});

test("workflow entry keeps provider refusal details out of GitHub output and summary", (t) => {
  assertEntryOutput(t, "provider-refused", "failed");
});

test("workflow entry reports failure when the final OFF readback is unconfirmed", (t) => {
  assertEntryOutput(t, "cleanup-uncertain", "failed");
});

test("coherent synthetic workflow entry returns only the bounded mock verdict", (t) => {
  assertEntryOutput(t, "coherent", "passed");
});
