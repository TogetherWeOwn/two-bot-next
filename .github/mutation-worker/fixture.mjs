import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { edits, measure, verdict } from "./run.mjs";

if (process.env.GITHUB_ACTIONS !== "true") throw new Error("mutation fixtures run only in GitHub Actions");
const root = mkdtempSync(join(process.env.RUNNER_TEMP, "worker-mutation-fixture-"));
const file = "wrangler/src/rule.ts";
const source = "export const legacy = (n: number) => n > 99;\nexport const eligible = (n: number) => (n) /* input */ >=\n  3;\n";
try {
  mkdirSync(join(root, "wrangler/src"), { recursive: true });
  const git = (...args) => execFileSync("git", args, { cwd: root, encoding: "utf8", timeout: 30_000 }).trim();
  function commit() {
    git("add", "--", file);
    git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture");
    return git("rev-parse", "HEAD");
  }
  git("init", "-q");
  writeFileSync(join(root, file), source.replace(">=", ">"));
  const base = commit();
  writeFileSync(join(root, file), source);
  const head = commit();
  execFileSync("python3", ["-c", "from pathlib import Path; from mutation_scope import main; main(Path.cwd())"], {
    cwd: root, timeout: 30_000,
    env: { ...process.env, PYTHONPATH: fileURLToPath(new URL("../scripts", import.meta.url)),
      BASE_SHA: base, HEAD_SHA: head, RUNNER_TEMP: root, GITHUB_OUTPUT: join(root, "outputs") },
  });
  const changed = JSON.parse(readFileSync(join(root, "worker-mutation-plan.json"), "utf8"));
  assert.deepEqual(changed, { [file]: [2] });
  assert.match(readFileSync(join(root, "outputs"), "utf8"), /^worker=true$/m);
  const receipt = join(process.env.RUNNER_TEMP, "worker-mutation.json");
  const fallback = readFileSync(receipt, "utf8");
  assert.equal(JSON.parse(fallback).results[0].status, "infrastructure_error");
  const auditCalls = join(root, "audit-calls");
  writeFileSync(join(root, "npm"),
    '#!/bin/sh\nprintf "%s\\n" "$*" >> "$WORKER_MUTATION_AUDIT_FIXTURE_CALLS"\nexit 1\n', { mode: 0o700 });
  const auditWarning = execFileSync("bash", [fileURLToPath(new URL("./measure.sh", import.meta.url))], {
    cwd: root, encoding: "utf8", timeout: 30_000,
    env: { ...process.env, PATH: `${root}:${process.env.PATH}`, WORKER_MUTATION_AUDIT_FIXTURE_CALLS: auditCalls },
  });
  assert.match(auditWarning, /::warning::Worker mutation measurement blocked by the dependency audit/);
  assert.equal(readFileSync(auditCalls, "utf8"), "audit --prefix wrangler --audit-level=high --include=dev\n");
  assert.equal(readFileSync(receipt, "utf8"), fallback);
  console.log("Worker fixture: refused dependency audit preserved the infrastructure receipt");
  writeFileSync(join(root, "wrangler/initialization.test.mjs"),
    "import { eligible } from './src/rule.ts';\nif (!eligible(3)) throw new Error('fixture module load failed');\n");
  for (const [label, body, status, expected] of [
    ["strong", "test('boundary', () => assert.equal(eligible(3), true));", "killed", 0],
    ["weak", "test('legacy', () => assert.equal(legacy(100), true));", "survived", 1],
    ["broken", "test('bad baseline', () => assert.equal(eligible(3), false));", "infrastructure_error", 2],
    ["cancelled", "test('pending mutant test', () => eligible(3) ? undefined : new Promise(() => {}));", "infrastructure_error", 2],
    ["wrapper-failed", "test('boundary', () => assert.equal(eligible(3), true));", "infrastructure_error", 2],
    ["skipped", "test('skipped', (t) => t.skip(''));", "unsupported", 0],
    ["todo", "test('todo', (t) => t.todo(''));", "unsupported", 0],
    ["empty", "", "unsupported", 0],
  ]) {
    writeFileSync(join(root, "wrangler/rule.test.mjs"),
      `import { test } from 'node:test';\nimport assert from 'node:assert/strict';\nimport { eligible, legacy } from './src/rule.ts';\n${body}\n`);
    const tests = ["--test", "rule.test.mjs"];
    if (label === "wrapper-failed") tests.push("initialization.test.mjs");
    const results = await measure(root, changed, tests);
    assert.equal(readFileSync(join(root, file), "utf8"), source);
    assert.deepEqual(results.map((entry) => entry.status), [status]);
    const attempted = ["strong", "weak", "cancelled", "wrapper-failed"].includes(label);
    assert.equal(results[0].line, attempted ? 2 : undefined);
    assert.equal(verdict(results, "warn"), 0);
    assert.equal(verdict(results, "hard"), expected);
    console.log(`Worker fixture ${label}: ${status}, HARD exit ${expected}`);
  }
  const operandOnly = await measure(root, { [file]: [3] }, ["--test", "rule.test.mjs"]);
  assert.deepEqual(operandOnly.map((entry) => entry.status), ["unsupported"]);
  assert.deepEqual(edits("const result = a && b && c;", new Set([1])), []);
  assert.deepEqual(edits("const result = a && b;", new Set([1])).map((edit) => edit.replacement), [" || "]);

  const adjacent = "export const total = (n: number) => n+-2;\n";
  assert.deepEqual(edits(adjacent, new Set([1])).map((edit) => edit.replacement), [" - "]);
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
