import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { measure, verdict } from "./run.mjs";

if (process.env.GITHUB_ACTIONS !== "true") throw new Error("mutation fixtures run only in GitHub Actions");
const root = mkdtempSync(join(process.env.RUNNER_TEMP, "worker-mutation-fixture-"));
const file = "wrangler/src/rule.ts";
const source = "export const legacy = (n: number) => n > 99;\nexport const eligible = (n: number) => (n) /* input */ >=\n  3;\n";
try {
  mkdirSync(join(root, "wrangler/src"), { recursive: true });
  writeFileSync(join(root, file), source);
  for (const [label, body, status, expected] of [
    ["strong", "test('boundary', () => assert.equal(eligible(3), true));", "killed", 0],
    ["weak", "test('legacy', () => assert.equal(legacy(100), true));", "survived", 1],
    ["broken", "test('bad baseline', () => assert.equal(eligible(3), false));", "infrastructure_error", 2],
    ["empty", "", "unsupported", 0],
  ]) {
    writeFileSync(join(root, "wrangler/rule.test.mjs"),
      `import { test } from 'node:test';\nimport assert from 'node:assert/strict';\nimport { eligible, legacy } from './src/rule.ts';\n${body}\n`);
    const results = await measure(root, { [file]: [2] }, ["--test", "rule.test.mjs"]);
    assert.equal(readFileSync(join(root, file), "utf8"), source);
    assert.deepEqual(results.map((entry) => entry.status), [status]);
    if (status === "survived" || status === "killed") assert.equal(results[0].line, 2);
    assert.equal(verdict(results, "warn"), 0);
    assert.equal(verdict(results, "hard"), expected);
    console.log(`Worker fixture ${label}: ${status}, HARD exit ${expected}`);
  }
  const operandOnly = await measure(root, { [file]: [3] }, ["--test", "rule.test.mjs"]);
  assert.deepEqual(operandOnly.map((entry) => entry.status), ["unsupported"]);

  const adjacent = "export const total = (n: number) => n+-2;\n";
  writeFileSync(join(root, file), adjacent);
  writeFileSync(join(root, "wrangler/rule.test.mjs"),
    "import { test } from 'node:test';\nimport assert from 'node:assert/strict';\nimport { total } from './src/rule.ts';\ntest('adjacent operators', () => assert.equal(total(3), 1));\n");
  const arithmetic = await measure(root, { [file]: [1] }, ["--test", "rule.test.mjs"]);
  assert.deepEqual(arithmetic.map((entry) => entry.status), ["killed"]);
  assert.equal(readFileSync(join(root, file), "utf8"), adjacent);
  console.log("Worker fixture: adjacent unary operator remained valid; mutant killed");
} finally {
  rmSync(root, { recursive: true });
}
