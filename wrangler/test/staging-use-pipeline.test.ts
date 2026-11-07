import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { pipelineUse } from "../scripts/staging-use-pipeline.mjs";
import { StagingUseError } from "../scripts/staging-use.mjs";

const NOW = 1_700_000_000_000;
const MINUTE = 60_000;
const SHA = "a".repeat(40);
const SHA2 = "b".repeat(40);
const URL_ = "https://two-bot-next-staging.example.workers.dev/";
const TOKEN = "t".repeat(40);
const RUN = "12345";

type Use = {
  environment: "staging"; runId: string; runAttempt: number; expiresAt: number; acquiredAt: number;
  deploymentId: string; sha: string; epoch: number; phase: "preparing" | "active" | "released";
};

const grant = (over: Partial<Use> = {}): Use => ({
  environment: "staging", runId: RUN, runAttempt: 1, expiresAt: NOW + 10 * MINUTE, acquiredAt: NOW - MINUTE,
  deploymentId: "dep-1", sha: SHA, epoch: 7, phase: "active", ...over,
});

// A stateful fake of the low-level client. It returns the projected client
// receipt shape (never raw HTTP bodies) and records every call and emit in one
// ordered log so mutation-versus-receipt ordering can be asserted.
function fixture(initial: { use?: Use | null; deploymentId?: string; sha?: string; running?: boolean } = {}, failOn?: string) {
  const state = {
    deploymentId: initial.deploymentId ?? "dep-1", sha: initial.sha ?? SHA, epoch: 7,
    running: initial.running ?? true, use: initial.use === undefined ? null : initial.use,
  };
  const log: string[] = [];
  const calls: any[] = [];
  const emitted: any[] = [];
  const receipt = (action: string) => ({
    schema: "two-bot-staging-use.v1", action, environment: "staging",
    observedAt: new Date(NOW).toISOString(), deploymentId: state.deploymentId, sha: state.sha,
    epoch: state.epoch, running: state.running, use: state.use ? { ...state.use } : null,
  });
  const invoke = async (args: any) => {
    log.push(`call:${args.action}`);
    calls.push(args);
    if (failOn === args.action) throw new StagingUseError(`injected_${args.action}_failure`);
    if (args.action === "acquire") {
      state.use = grant({ runId: args.runId, runAttempt: args.runAttempt, expiresAt: args.expiresAt,
        acquiredAt: NOW, deploymentId: args.deploymentId, sha: args.sha, phase: "active" });
    } else if (args.action === "release") {
      state.use = { ...state.use!, phase: "released" };
      state.running = false;
    }
    return receipt(args.action);
  };
  const emit = (value: any) => { log.push(`emit:${value.action}`); emitted.push(value); };
  const names = (prefix: string) => log.filter((entry) => entry.startsWith(prefix)).map((entry) => entry.slice(prefix.length));
  return { invoke, emit, log, calls, emitted, state, called: () => names("call:") };
}

const base = { url: URL_, token: TOKEN, runId: RUN, runAttempt: 1 };
const clock = () => NOW;
const code = (expected: string) => (error: any) => {
  assert.ok(error instanceof StagingUseError, "expected a StagingUseError");
  assert.equal(error.code, expected);
  return true;
};
const binding = (over: Record<string, unknown> = {}) => ({
  ...base, sha: SHA, deploymentId: "dep-1", expiresAt: NOW + 10 * MINUTE, ...over,
});

test("rollback preflight asserts only the current retained grant without readiness or mutation", async () => {
  const use = grant();
  const f = fixture({ use });
  const result = await pipelineUse({ ...base, action: "restore-check", expiresAt: use.expiresAt },
    f.invoke, clock, f.emit);
  assert.deepEqual(f.called(), ["status", "assert"]);
  assert.deepEqual(f.calls[1], { ...binding(), action: "assert" });
  assert.equal(result.action, "assert");
  assert.deepEqual(f.state.use, use);
  assert.equal(f.emitted.some((r) => r.action === "window"), false);
  for (const invalid of [null, grant({ runId: "999" }), grant({ runAttempt: 2 }),
    grant({ expiresAt: NOW + MINUTE }), grant({ phase: "released" }), grant({ phase: "preparing" })]) {
    const bad = fixture({ use: invalid });
    await assert.rejects(pipelineUse({ ...base, action: "restore-check", expiresAt: use.expiresAt },
      bad.invoke, clock, bad.emit), code("staging_window_mismatch"));
    assert.deepEqual(bad.called(), ["status"]);
  }
  const denied = fixture({ use }, "assert");
  await assert.rejects(pipelineUse({ ...base, action: "restore-check", expiresAt: use.expiresAt },
    denied.invoke, clock, denied.emit), code("injected_assert_failure"));
  assert.deepEqual(denied.called(), ["status", "assert"]);
});

test("begin persists attempt and chosen expiry before acquire, then acquires and readies the same window", async () => {
  const f = fixture();
  const expiresAt = NOW + 30 * MINUTE;
  const result = await pipelineUse({ ...base, action: "begin", minutes: 30 }, f.invoke, clock, f.emit);
  assert.deepEqual(f.log, [
    "emit:attempt", "call:status", "emit:status", "emit:window",
    "call:acquire", "emit:acquire", "call:ready", "emit:ready",
  ]);
  assert.deepEqual(f.emitted[0], {
    schema: "two-bot-staging-pipeline.v1", action: "attempt", operation: "begin", environment: "staging",
    runId: RUN, runAttempt: 1, observedAt: new Date(NOW).toISOString(),
  });
  assert.deepEqual(f.emitted.find((value) => value.action === "window"), {
    schema: "two-bot-staging-pipeline.v1", action: "window", environment: "staging",
    sha: SHA, deploymentId: "dep-1", expiresAt, runId: RUN, runAttempt: 1, observedAt: new Date(NOW).toISOString(),
  });
  assert.deepEqual(f.calls, [
    { ...base, action: "status" },
    { ...base, action: "acquire", sha: SHA, deploymentId: "dep-1", expiresAt },
    { ...base, action: "ready", sha: SHA, deploymentId: "dep-1", expiresAt },
  ]);
  assert.equal(result.action, "ready");
  assert.equal(result.use.expiresAt, expiresAt);
  assert.equal(result.use.phase, "active");
});

test("deploy begin pins the independent receipt before any mutation", async () => {
  for (const over of [{ sha: SHA2 }, { deploymentId: "dep-2" }]) {
    const f = fixture();
    await assert.rejects(pipelineUse({ ...base, action: "begin", minutes: 30,
      sha: SHA, deploymentId: "dep-1", ...over }, f.invoke, clock, f.emit), code("staging_identity_mismatch"));
    assert.deepEqual(f.called(), ["status"]);
    assert.equal(f.emitted.some((value) => value.action === "window"), false);
    assert.equal(f.state.use, null);
  }
  for (const over of [{ sha: SHA }, { deploymentId: "dep-1" }, { sha: "bad", deploymentId: "dep-1" }]) {
    const f = fixture();
    await assert.rejects(pipelineUse({ ...base, action: "begin", minutes: 30, ...over }, f.invoke, clock, f.emit),
      code("staging_identity_invalid"));
    assert.deepEqual(f.calls, []);
  }
});

test("deploy acquire-only preserves the existing rollout gate and fault-injection verdict", async () => {
  const f = fixture({}, "ready");
  const result = await pipelineUse({ ...base, action: "begin", minutes: 30,
    sha: SHA, deploymentId: "dep-1", acquireOnly: true }, f.invoke, clock, f.emit);
  assert.deepEqual(f.called(), ["status", "acquire"]);
  assert.equal(result.action, "acquire");
  assert.equal(result.use.phase, "active");
  assert.equal(result.use.expiresAt, NOW + 30 * MINUTE);
  assert.ok(f.log.indexOf("emit:window") < f.log.indexOf("call:acquire"));
  const failing = fixture({}, "acquire");
  await assert.rejects(pipelineUse({ ...base, action: "begin", minutes: 30, acquireOnly: true },
    failing.invoke, clock, failing.emit), code("injected_acquire_failure"));
  assert.deepEqual(failing.called(), ["status", "acquire"]);
});

test("begin accepts the 360 minute maximum", async () => {
  const f = fixture();
  await pipelineUse({ ...base, action: "begin", minutes: 360 }, f.invoke, clock, f.emit);
  assert.equal(f.calls[1].expiresAt, NOW + 360 * MINUTE);
});

test("begin retry retains the immutable expiry of the same attempt, including a preparing grant", async () => {
  for (const phase of ["preparing", "active"] as const) {
    const original = NOW + 5 * MINUTE;
    const f = fixture({ use: grant({ phase, expiresAt: original }) });
    await pipelineUse({ ...base, action: "begin", minutes: 360 }, f.invoke, clock, f.emit);
    assert.deepEqual(f.called(), ["status", "acquire", "ready"], phase);
    assert.equal(f.calls[1].expiresAt, original, phase);
    assert.equal(f.calls[2].expiresAt, original, phase);
    assert.equal(f.emitted.find((value) => value.action === "window").expiresAt, original, phase);
    assert.equal(f.state.use!.expiresAt, original, phase);
  }
});

test("begin refuses a released or expired same attempt before any receipt window or mutation", async () => {
  const cases = [
    grant({ phase: "released" }),
    grant({ phase: "active", expiresAt: NOW }),
    grant({ phase: "preparing", expiresAt: NOW - 1 }),
  ];
  for (const use of cases) {
    const f = fixture({ use });
    await assert.rejects(pipelineUse({ ...base, action: "begin", minutes: 30 }, f.invoke, clock, f.emit),
      code("staging_use_ended"));
    assert.deepEqual(f.called(), ["status"]);
    assert.equal(f.emitted.some((value) => value.action === "window"), false);
    assert.deepEqual(f.state.use, use);
  }
});

test("begin conflicts with any other live window and leaves it untouched", async () => {
  const cases = [
    grant({ runId: "99999" }),
    grant({ runId: "99999", phase: "preparing" }),
    grant({ runAttempt: 2 }),
  ];
  for (const use of cases) {
    const f = fixture({ use });
    await assert.rejects(pipelineUse({ ...base, action: "begin", minutes: 30 }, f.invoke, clock, f.emit),
      code("staging_use_conflict"));
    assert.deepEqual(f.called(), ["status"]);
    assert.equal(f.emitted.some((value) => value.action === "window"), false);
    assert.deepEqual(f.state.use, use);
  }
});

test("begin may replace another run's released or expired window with a fresh one", async () => {
  const cases = [
    grant({ runId: "99999", phase: "released" }),
    grant({ runId: "99999", expiresAt: NOW }),
    grant({ runAttempt: 2, expiresAt: NOW - 1, phase: "preparing" }),
  ];
  for (const use of cases) {
    const f = fixture({ use });
    await pipelineUse({ ...base, action: "begin", minutes: 15 }, f.invoke, clock, f.emit);
    assert.deepEqual(f.called(), ["status", "acquire", "ready"]);
    assert.equal(f.calls[1].expiresAt, NOW + 15 * MINUTE);
  }
});

test("begin failures propagate and the window receipt is already persisted before acquire", async () => {
  const status = fixture({}, "status");
  await assert.rejects(pipelineUse({ ...base, action: "begin", minutes: 30 }, status.invoke, clock, status.emit),
    code("injected_status_failure"));
  assert.deepEqual(status.log, ["emit:attempt", "call:status"]);

  const acquire = fixture({}, "acquire");
  await assert.rejects(pipelineUse({ ...base, action: "begin", minutes: 30 }, acquire.invoke, clock, acquire.emit),
    code("injected_acquire_failure"));
  assert.deepEqual(acquire.log, ["emit:attempt", "call:status", "emit:status", "emit:window", "call:acquire"]);
  assert.equal(acquire.state.use, null);

  const ready = fixture({}, "ready");
  await assert.rejects(pipelineUse({ ...base, action: "begin", minutes: 30 }, ready.invoke, clock, ready.emit),
    code("injected_ready_failure"));
  assert.deepEqual(ready.called(), ["status", "acquire", "ready"]);
  assert.equal(ready.emitted.at(-1).action, "acquire");
});

test("a receipt that cannot be persisted stops the pipeline before the next call", async () => {
  const attempt = fixture();
  const failAttempt = (value: any) => { if (value.action === "attempt") throw new StagingUseError("receipt_failed"); attempt.emit(value); };
  await assert.rejects(pipelineUse({ ...base, action: "begin", minutes: 30 }, attempt.invoke, clock, failAttempt),
    code("receipt_failed"));
  assert.deepEqual(attempt.calls, []);

  const window = fixture();
  const failWindow = (value: any) => { if (value.action === "window") throw new StagingUseError("receipt_failed"); window.emit(value); };
  await assert.rejects(pipelineUse({ ...base, action: "begin", minutes: 30 }, window.invoke, clock, failWindow),
    code("receipt_failed"));
  assert.deepEqual(window.called(), ["status"]);
  assert.equal(window.state.use, null);
});

test("invalid arguments refuse before any invoke or receipt", async () => {
  const cases: [Record<string, unknown>, string][] = [
    [{ action: "stop", minutes: 30 }, "staging_pipeline_action_invalid"],
    [{ action: undefined }, "staging_pipeline_action_invalid"],
    [{ action: "status" }, "staging_pipeline_action_invalid"],
    [{ action: "begin", minutes: 30, runId: "0" }, "staging_run_invalid"],
    [{ action: "begin", minutes: 30, runId: "abc" }, "staging_run_invalid"],
    [{ action: "begin", minutes: 30, runId: 12345 }, "staging_run_invalid"],
    [{ action: "begin", minutes: 30, runId: undefined }, "staging_run_invalid"],
    [{ action: "begin", minutes: 30, runAttempt: 0 }, "staging_run_invalid"],
    [{ action: "begin", minutes: 30, runAttempt: 1.5 }, "staging_run_invalid"],
    [{ action: "begin", minutes: 30, runAttempt: Number.NaN }, "staging_run_invalid"],
    [{ action: "begin", minutes: 0 }, "staging_window_invalid"],
    [{ action: "begin", minutes: 361 }, "staging_window_invalid"],
    [{ action: "begin", minutes: 30, acquireOnly: "true" }, "staging_pipeline_mode_invalid"],
    [{ action: "begin", minutes: 1.5 }, "staging_window_invalid"],
    [{ action: "begin", minutes: Number.NaN }, "staging_window_invalid"],
    [{ action: "begin", minutes: undefined }, "staging_window_invalid"],
    [{ action: "begin", minutes: 30, expiresAt: NOW + MINUTE }, "staging_pipeline_begin_inherited"],
    [{ action: "finish" }, "staging_window_invalid"],
    [{ action: "finish", expiresAt: 0 }, "staging_window_invalid"],
    [{ action: "consume", sha: SHA, deploymentId: "dep-1" }, "staging_window_invalid"],
    [{ action: "consume", expiresAt: NOW + MINUTE, sha: "A".repeat(40), deploymentId: "dep-1" }, "staging_identity_invalid"],
    [{ action: "consume", expiresAt: NOW + MINUTE, sha: "a".repeat(39), deploymentId: "dep-1" }, "staging_identity_invalid"],
    [{ action: "consume", expiresAt: NOW + MINUTE, sha: SHA, deploymentId: "bad id" }, "staging_identity_invalid"],
    [{ action: "consume", expiresAt: NOW + MINUTE, sha: SHA, deploymentId: "x".repeat(129) }, "staging_identity_invalid"],
    [{ action: "consume", expiresAt: NOW + MINUTE, sha: SHA }, "staging_identity_invalid"],
    [{ action: "rebind", sha: SHA, deploymentId: "dep-1" }, "staging_window_invalid"],
    [{ action: "rebind", expiresAt: NOW + MINUTE, deploymentId: "dep-1" }, "staging_identity_invalid"],
    [{ action: "rebind", expiresAt: NOW + MINUTE, sha: SHA, deploymentId: "" }, "staging_identity_invalid"],
  ];
  for (const [over, expected] of cases) {
    const f = fixture({ use: grant() });
    await assert.rejects(pipelineUse({ ...base, ...over }, f.invoke, clock, f.emit), code(expected), JSON.stringify(over));
    assert.deepEqual(f.calls, [], JSON.stringify(over));
    assert.deepEqual(f.emitted, [], JSON.stringify(over));
    assert.equal(f.state.use!.phase, "active");
  }
});

test("consume issues exactly one ready with the caller's exact identity and window", async () => {
  const f = fixture({ use: grant() });
  const before = structuredClone(f.state);
  const result = await pipelineUse({ ...base, action: "consume", sha: SHA, deploymentId: "dep-1",
    expiresAt: NOW + 10 * MINUTE }, f.invoke, clock, f.emit);
  assert.deepEqual(f.calls, [{ ...base, action: "ready", sha: SHA, deploymentId: "dep-1", expiresAt: NOW + 10 * MINUTE }]);
  assert.deepEqual(f.log, ["emit:attempt", "call:ready", "emit:ready"]);
  assert.equal(f.emitted[0].operation, "consume");
  assert.equal(result.action, "ready");
  assert.deepEqual(f.state, before);
});

test("consume never acquires, renews or releases even when readiness is refused", async () => {
  const f = fixture({ use: grant() }, "ready");
  await assert.rejects(pipelineUse({ ...base, action: "consume", sha: SHA2, deploymentId: "dep-2",
    expiresAt: NOW + 10 * MINUTE }, f.invoke, clock, f.emit), code("injected_ready_failure"));
  assert.deepEqual(f.calls, [{ ...base, action: "ready", sha: SHA2, deploymentId: "dep-2", expiresAt: NOW + 10 * MINUTE }]);
  assert.equal(f.state.use!.phase, "active");
});

test("rebind keeps the prior window and moves only to the new expected deployment", async () => {
  const original = NOW + 4 * MINUTE;
  const f = fixture({ use: grant({ expiresAt: original, phase: "active" }), deploymentId: "dep-2", sha: SHA2 });
  const result = await pipelineUse({ ...base, action: "rebind", sha: SHA2, deploymentId: "dep-2",
    expiresAt: original }, f.invoke, clock, f.emit);
  assert.deepEqual(f.log, [
    "emit:attempt", "call:status", "emit:status", "emit:window",
    "call:acquire", "emit:acquire", "call:ready", "emit:ready",
  ]);
  assert.deepEqual(f.calls, [
    { ...base, action: "status" },
    { ...base, action: "acquire", sha: SHA2, deploymentId: "dep-2", expiresAt: original },
    { ...base, action: "ready", sha: SHA2, deploymentId: "dep-2", expiresAt: original },
  ]);
  assert.equal(f.emitted[0].operation, "rebind");
  assert.equal(result.use.expiresAt, original);
  assert.equal(result.use.deploymentId, "dep-2");
  assert.equal(result.use.sha, SHA2);
});

test("rebind of a preparing window also keeps the prior expiry", async () => {
  const original = NOW + 4 * MINUTE;
  const f = fixture({ use: grant({ expiresAt: original, phase: "preparing" }), deploymentId: "dep-2", sha: SHA2 });
  await pipelineUse({ ...base, action: "rebind", sha: SHA2, deploymentId: "dep-2", expiresAt: original },
    f.invoke, clock, f.emit);
  assert.equal(f.calls[1].expiresAt, original);
  assert.equal(f.state.use!.expiresAt, original);
});

test("rebind refuses without a matching unreleased prior window and never mints a fresh grant", async () => {
  const original = NOW + 4 * MINUTE;
  const cases: [Use | null, Record<string, unknown>, string][] = [
    [null, {}, "staging_window_mismatch"],
    [grant({ runId: "99999", expiresAt: original }), {}, "staging_window_mismatch"],
    [grant({ runAttempt: 2, expiresAt: original }), {}, "staging_window_mismatch"],
    [grant({ expiresAt: original + 1 }), {}, "staging_window_mismatch"],
    [grant({ expiresAt: original, phase: "released" }), {}, "staging_window_mismatch"],
    [grant({ expiresAt: original }), { deploymentId: "dep-3" }, "staging_identity_mismatch"],
    [grant({ expiresAt: original }), { sha: "c".repeat(40) }, "staging_identity_mismatch"],
  ];
  for (const [use, over, expected] of cases) {
    const f = fixture({ use, deploymentId: "dep-2", sha: SHA2 });
    await assert.rejects(pipelineUse({ ...base, action: "rebind", sha: SHA2, deploymentId: "dep-2",
      expiresAt: original, ...over }, f.invoke, clock, f.emit), code(expected), JSON.stringify(over));
    assert.deepEqual(f.called(), ["status"]);
    assert.equal(f.emitted.some((value) => value.action === "window"), false);
    assert.deepEqual(f.state.use, use);
  }
});

test("finish releases the same window using the current identity, even when expired", async () => {
  const expiresAt = NOW - 5 * MINUTE;
  const f = fixture({ use: grant({ expiresAt, acquiredAt: NOW - 10 * MINUTE }) });
  const result = await pipelineUse({ ...base, action: "finish", expiresAt }, f.invoke, clock, f.emit);
  assert.deepEqual(f.calls, [
    { ...base, action: "status" },
    { ...base, action: "release", sha: SHA, deploymentId: "dep-1", expiresAt },
  ]);
  assert.deepEqual(f.log, ["emit:attempt", "call:status", "emit:status", "call:release", "emit:release"]);
  assert.equal(result.action, "release");
  assert.equal(result.use.phase, "released");
  assert.equal(result.running, false);
});

test("finish after a rebind releases with the rebound deployment and original expiry", async () => {
  const expiresAt = NOW + 3 * MINUTE;
  const f = fixture({ use: grant({ expiresAt, deploymentId: "dep-2", sha: SHA2 }), deploymentId: "dep-2", sha: SHA2 });
  await pipelineUse({ ...base, action: "finish", expiresAt }, f.invoke, clock, f.emit);
  assert.deepEqual(f.calls[1], { ...base, action: "release", sha: SHA2, deploymentId: "dep-2", expiresAt });
  assert.equal(f.state.use!.phase, "released");
  assert.equal(f.calls.length, 2);
});

test("finish is a recorded no-op that never stops another run or attempt", async () => {
  const expiresAt = NOW + 3 * MINUTE;
  const others: (Use | null)[] = [
    null,
    grant({ runId: "99999", expiresAt }),
    grant({ runAttempt: 2, expiresAt }),
    grant({ runId: "99999", runAttempt: 2, expiresAt, phase: "preparing" }),
  ];
  for (const use of others) {
    const f = fixture({ use });
    const result = await pipelineUse({ ...base, action: "finish", expiresAt }, f.invoke, clock, f.emit);
    assert.deepEqual(f.called(), ["status"]);
    assert.deepEqual(f.log, ["emit:attempt", "call:status", "emit:status", "emit:not-owned"]);
    assert.deepEqual(result, {
      schema: "two-bot-staging-pipeline.v1", action: "not-owned", environment: "staging",
      runId: RUN, runAttempt: 1, observedAt: new Date(NOW).toISOString(),
    });
    assert.deepEqual(f.state.use, use);
    assert.equal(f.state.running, true);
  }
});

test("finish refuses a same-attempt grant with a different expiry without releasing", async () => {
  const f = fixture({ use: grant({ expiresAt: NOW + 3 * MINUTE }) });
  await assert.rejects(pipelineUse({ ...base, action: "finish", expiresAt: NOW + 4 * MINUTE },
    f.invoke, clock, f.emit), code("staging_window_mismatch"));
  assert.deepEqual(f.called(), ["status"]);
  assert.equal(f.state.use!.phase, "active");
  assert.equal(f.state.running, true);
});

test("finish failures propagate after the attempt receipt", async () => {
  const status = fixture({ use: grant() }, "status");
  await assert.rejects(pipelineUse({ ...base, action: "finish", expiresAt: NOW + 10 * MINUTE },
    status.invoke, clock, status.emit), code("injected_status_failure"));
  assert.deepEqual(status.log, ["emit:attempt", "call:status"]);

  const release = fixture({ use: grant() }, "release");
  await assert.rejects(pipelineUse({ ...base, action: "finish", expiresAt: NOW + 10 * MINUTE },
    release.invoke, clock, release.emit), code("injected_release_failure"));
  assert.deepEqual(release.called(), ["status", "release"]);
  assert.deepEqual(release.calls[1], { ...base, action: "release", sha: SHA, deploymentId: "dep-1", expiresAt: NOW + 10 * MINUTE });
  assert.equal(release.state.use!.phase, "active");
});

test("finish repeats native stop confirmation for an already released same window", async () => {
  const f = fixture({ use: grant({ phase: "released" }), running: false });
  const result = await pipelineUse({ ...base, action: "finish", expiresAt: NOW + 10 * MINUTE },
    f.invoke, clock, f.emit);
  assert.deepEqual(f.called(), ["status", "release"]);
  assert.equal(result.action, "release");
  assert.equal(result.use.phase, "released");
  assert.equal(result.running, false);
});

test("pipeline CLI failure receipts retain only validated run correlation and fixed codes", () => {
  const dir = mkdtempSync(join(tmpdir(), "staging-pipeline-test-"));
  try {
    const receipt = join(dir, "receipt.ndjson");
    const output = join(dir, "output");
    const cli = fileURLToPath(new URL("../scripts/staging-use-pipeline.mjs", import.meta.url));
    for (const runId of [RUN, "untrusted-" + TOKEN]) {
      const result = spawnSync(process.execPath, [cli, "invalid-" + TOKEN], {
        env: { GITHUB_RUN_ID: runId, GITHUB_RUN_ATTEMPT: "1", STAGING_USE_RECEIPT: receipt,
          GITHUB_OUTPUT: output, OWNERSHIP_CONTROL_TOKEN: TOKEN, STAGING_WORKER_URL: URL_ },
        encoding: "utf8", timeout: 5000,
      });
      assert.equal(result.status, 1);
      assert.equal(result.stderr.trim(), "staging_pipeline_action_invalid");
      assert.equal(result.stdout.includes(TOKEN), false);
      const line = JSON.parse(result.stdout);
      assert.equal(line.runId, runId === RUN ? RUN : null);
      assert.equal(line.runAttempt, 1);
      assert.equal(line.action, "failed");
      assert.equal(line.code, "staging_pipeline_action_invalid");
      assert.equal(line.environment, "staging");
    }
    const rows = readFileSync(receipt, "utf8").trim().split("\n").map((line) => JSON.parse(line));
    assert.equal(rows.length, 2);
    assert.equal(rows[0].runId, RUN);
    assert.equal(rows[1].runId, null);
    assert.equal(JSON.stringify(rows).includes(TOKEN), false);
  } finally { rmSync(dir, { recursive: true, force: true }); }
});

test("every action passes only the injected invoke the run identity and no extra fields", async () => {
  const f = fixture({ use: grant() });
  await pipelineUse({ ...binding(), action: "consume", minutes: 99, extra: "ignored" }, f.invoke, clock, f.emit);
  assert.deepEqual(Object.keys(f.calls[0]).sort(),
    ["action", "deploymentId", "expiresAt", "runAttempt", "runId", "sha", "token", "url"]);
});
