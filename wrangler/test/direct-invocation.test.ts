import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { copyFileSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { isDirectInvocation } from "../scripts/is-direct-invocation.mjs";

test("direct invocation compares canonical paths and decodes file URLs", (t) => {
  const root = mkdtempSync(join(tmpdir(), "direct invocation % # "));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const script = join(root, "entry % #.mjs");
  const link = join(root, "link.mjs");
  writeFileSync(script, "");
  symlinkSync(script, link);
  const url = pathToFileURL(script).href;
  assert.equal(isDirectInvocation(script, url), true);
  assert.equal(isDirectInvocation(relative(process.cwd(), script), url), true);
  assert.equal(isDirectInvocation(link, url), true);
  assert.equal(isDirectInvocation(script, pathToFileURL(link).href), true);
  assert.equal(isDirectInvocation(fileURLToPath(import.meta.url), url), false);
  for (const argv of [undefined, null, "", join(root, "absent.mjs")]) {
    assert.equal(isDirectInvocation(argv, url), false);
  }
  assert.equal(isDirectInvocation(script, "not a file URL"), false);
  assert.equal(isDirectInvocation(script, pathToFileURL(join(root, "absent.mjs")).href), false);
});

// Real files (not only aliases) make import.meta.url contain escaped spaces,
// percent and hash characters. Copy the local dependency closure so each CLI
// exercises its own guard and import path without a network or configuration.
const REFUSALS = {
  "check-readyz.mjs": "Readiness gate failed: expected the bot's component breakdown, never an ownership refusal\n",
  "ownership-control.mjs": "Ownership control failed; stop and report without substituting credentials\n",
  "production-ownership-control.mjs": "PRODUCTION_WORKER_URL is not a valid URL; stop before deployment\n",
  "staging-ownership-status.mjs": "Ownership status is not configured; stop without substituting credentials\n",
};

for (const [file, refusal] of Object.entries(REFUSALS)) {
  test(`${file}: executes and imports from a real percent-escaped path`, (t) => {
    const root = mkdtempSync(join(tmpdir(), "safety CLI % # "));
    t.after(() => rmSync(root, { recursive: true, force: true }));
    for (const dependency of [...Object.keys(REFUSALS), "is-direct-invocation.mjs", "ownership-control-response.mjs"]) {
      copyFileSync(new URL(`../scripts/${dependency}`, import.meta.url), join(root, dependency));
    }
    const script = join(root, file);
    const executed = spawnSync(process.execPath, [script], {
      env: {}, encoding: "utf8", timeout: 15_000,
    });
    assert.equal(executed.error, undefined);
    assert.equal(executed.signal, null);
    assert.equal(executed.status, 1);
    assert.equal(executed.stdout, "");
    assert.equal(executed.stderr, refusal);

    const imported = spawnSync(process.execPath, [
      "--input-type=module", "--eval", `await import(${JSON.stringify(pathToFileURL(script).href)})`,
    ], { env: {}, encoding: "utf8", timeout: 15_000 });
    assert.equal(imported.error, undefined);
    assert.equal(imported.signal, null);
    assert.equal(imported.status, 0);
    assert.equal(imported.stdout, "");
    assert.equal(imported.stderr, "");
  });
}
