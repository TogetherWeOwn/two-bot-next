import { test } from "node:test";
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);
const GATE = fileURLToPath(new URL("../scripts/promotion-error-gate.mjs", import.meta.url));

/** Env for a gate run: hermetic token state, everything else inherited. */
function gateEnv(overrides: Record<string, string | undefined>): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = { ...process.env };
  for (const [key, value] of Object.entries(overrides)) {
    if (value === undefined) delete env[key];
    else env[key] = value;
  }
  return env;
}

async function runRaw(argv: string[], env: NodeJS.ProcessEnv): Promise<{ code: number; stdout: string }> {
  try {
    const { stdout } = await execFileAsync(process.execPath, [GATE, ...argv], { env });
    return { code: 0, stdout };
  } catch (error) {
    assert.equal(typeof (error as { code?: unknown }).code, "number", "expected an exit-code error");
    const code = (error as { code: number }).code;
    const stdout = String((error as { stdout?: unknown }).stdout ?? "");
    return { code, stdout };
  }
}

interface GateRun {
  code: number;
  stdout: string;
  evidence: Record<string, unknown> | null;
}

/**
 * Run the gate exactly the way deploy-staging.yml invokes it: plain `node`
 * with argv flags plus env, then read back the evidence file. Live mode is
 * deliberately never exercised here: it sleeps between scrapes and needs the
 * network (runLiveGate's unit tests pin its verdicts instead).
 */
async function runGate(argv: string[], env: NodeJS.ProcessEnv): Promise<GateRun> {
  const evidencePath = join(mkdtempSync(join(tmpdir(), "error-rate-gate-acceptance-")), "evidence.json");
  const { code, stdout } = await runRaw(
    ["--metrics-url", "https://staging.example", "--evidence", evidencePath, "--sha", "test-sha", ...argv],
    env,
  );
  const evidence = existsSync(evidencePath)
    ? (JSON.parse(readFileSync(evidencePath, "utf8")) as Record<string, unknown>)
    : null;
  return { code, stdout, evidence };
}

test("rehearsal 5xx spike fails the stage with a fail verdict", async () => {
  const token = "sentinel-token-value-0000000000000000";
  const result = await runGate(
    ["--rehearsal-firing", '["rest_5xx_rate"]'],
    gateEnv({ METRICS_SCRAPE_TOKEN: token }),
  );
  // Non-zero exit is what fails the workflow stage so auto-rollback runs.
  assert.equal(result.code, 1);
  assert.equal(result.evidence?.mode, "rehearsal");
  assert.equal(result.evidence?.verdict, "fail");
  assert.deepEqual(result.evidence?.firing, ["rest_5xx_rate"]);
  assert.deepEqual(result.evidence?.reasons, ["rest_5xx_rate"]);
  assert.equal(result.evidence?.sha, "test-sha");
  // Evidence carries allowlisted IDs and the verdict only: the configured
  // scrape token must never leak into it.
  assert.ok(!JSON.stringify(result.evidence).includes(token));
});

test("quiet rehearsal passes the stage", async () => {
  const result = await runGate(["--rehearsal-firing", "[]"], gateEnv({ METRICS_SCRAPE_TOKEN: undefined }));
  assert.equal(result.code, 0);
  assert.equal(result.evidence?.mode, "rehearsal");
  assert.equal(result.evidence?.verdict, "pass");
  assert.deepEqual(result.evidence?.firing, []);
});

test("absent or short token is a recorded skip, never a silent pass", async () => {
  for (const token of [undefined, "short"] as const) {
    const result = await runGate([], gateEnv({ METRICS_SCRAPE_TOKEN: token }));
    assert.equal(result.code, 0);
    assert.equal(result.evidence?.mode, "skip");
    assert.equal(result.evidence?.verdict, "skip");
    assert.deepEqual(result.evidence?.firing, []);
  }
});

test("malformed rehearsal firing fails closed with no pass verdict", async () => {
  const result = await runGate(["--rehearsal-firing", "not-json"], gateEnv({ METRICS_SCRAPE_TOKEN: undefined }));
  assert.notEqual(result.code, 0);
  assert.ok(result.evidence === null || result.evidence.verdict !== "pass");
});

test("misconfigured invocation exits 2 without certifying", async () => {
  const env = gateEnv({ METRICS_SCRAPE_TOKEN: undefined });
  const missing = await runRaw(["--metrics-url", "https://staging.example"], env);
  assert.equal(missing.code, 2);
  const unknown = await runRaw(
    ["--metrics-url", "https://staging.example", "--evidence", join(tmpdir(), "unused.json"), "--bogus", "x"],
    env,
  );
  assert.equal(unknown.code, 2);
});
