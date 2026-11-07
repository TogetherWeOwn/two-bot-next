import { test } from "node:test";
import assert from "node:assert/strict";
import { soakWindow } from "../scripts/staging-soak-window.mjs";
import { pipelineUse } from "../scripts/staging-use-pipeline.mjs";
import { StagingUseError } from "../scripts/staging-use.mjs";

const NOW = 1_700_000_000_000;
const MINUTE = 60_000;
const SOAK = 1_800_000;
const SHA = "a".repeat(40);
const URL_ = "https://two-bot-next-staging.example.workers.dev/";
const TOKEN = "t".repeat(40);
const RUN = "12345";

const code = (expected: string) => (error: any) => {
  assert.ok(error instanceof StagingUseError, "expected a StagingUseError");
  assert.equal(error.code, expected);
  return true;
};

const args = (over: Record<string, unknown> = {}) => ({
  automatic: true, url: URL_, token: TOKEN, runId: RUN, runAttempt: 1, sha: SHA,
  deploymentId: "dep-1", expiresAt: NOW + 40 * MINUTE, ...over,
});
const consumeArgs = (over: Record<string, unknown> = {}) => {
  const { automatic, ...rest } = args(over);
  return { ...rest, action: "consume" };
};

// Deterministic harness: one wall clock and one monotonic clock that only move
// when the fake pause or a fake consume moves them. Nothing here sleeps.
function harness(opts: {
  failOnConsume?: number; firstConsumeCost?: number; pauseAdvance?: (ms: number) => number;
  wallJumpOnPause?: number; monotonicOverride?: (real: number, pauses: number) => number;
} = {}) {
  const clk = { wall: NOW, mono: 5_000 };
  const log: string[] = [];
  const consumes: any[] = [];
  const pauses: number[] = [];
  const events: any[] = [];
  const consume = async (a: any, invoke?: unknown, now?: unknown, emit?: unknown) => {
    consumes.push({ args: a, invoke, now, emit });
    log.push("consume");
    if (consumes.length === 1 && opts.firstConsumeCost) {
      clk.wall += opts.firstConsumeCost; clk.mono += opts.firstConsumeCost;
    }
    if (opts.failOnConsume === consumes.length) throw new StagingUseError("staging_ready_refused");
    return { action: "ready" };
  };
  const pause = async (ms: number) => {
    pauses.push(ms);
    log.push(`pause:${ms}`);
    const advance = opts.pauseAdvance ? opts.pauseAdvance(ms) : ms;
    clk.wall += advance; clk.mono += advance;
    if (opts.wallJumpOnPause === pauses.length) clk.wall += 60 * MINUTE;
  };
  const now = () => clk.wall;
  const monotonic = () => opts.monotonicOverride ? opts.monotonicOverride(clk.mono, pauses.length) : clk.mono;
  const emit = (value: any) => { log.push(`emit:${value.action}`); events.push(value); };
  const run = (a: any) => soakWindow(a, consume, pause, now, monotonic, emit);
  return { run, consumes, pauses, events, log, clk };
}

const actionsOf = (events: any[]) => events.map((event) => event.action);

test("automatic soak measures a fresh full 30 minutes after the first successful consume", async () => {
  const h = harness({ firstConsumeCost: 7_000 });
  await h.run(args({ expiresAt: NOW + SOAK + 60 * MINUTE }));
  assert.equal(h.pauses.reduce((sum, ms) => sum + ms, 0), SOAK);
  assert.ok(h.pauses.every((ms) => ms > 0 && ms <= MINUTE));
  assert.equal(h.pauses.length, 30);
  // Start is sampled after the first consume returned, never before it.
  assert.equal(h.log[0], "consume");
  assert.equal(h.log[1], "emit:soak-start");
  assert.equal(h.log.at(-1), "emit:soak-complete");
  assert.equal(h.clk.mono - 5_000, 7_000 + SOAK);
  assert.deepEqual(actionsOf(h.events), ["soak-start", "soak-complete"]);
  assert.equal(h.events[0].durationMs, SOAK);
});

test("each consume is bound to the exact original arguments and nothing else", async () => {
  const h = harness();
  await h.run(args({ expiresAt: NOW + 40 * MINUTE }));
  // Initial, one per interval, plus the final boundary check.
  assert.equal(h.consumes.length, 1 + 30 + 1);
  for (const call of h.consumes) {
    assert.deepEqual(call.args, consumeArgs());
    assert.equal(call.args.action, "consume");
    assert.equal("automatic" in call.args, false);
    assert.equal(call.invoke, undefined);
  }
  for (const action of ["begin", "rebind", "acquire", "release", "finish", "restore-check"]) {
    assert.equal(h.consumes.some((call) => call.args.action === action), false, action);
  }
});

test("soak consumes through the real pipeline only ever issue ready with the original binding", async () => {
  const invoked: any[] = [];
  const invoke = async (a: any) => {
    invoked.push(a);
    return { schema: "two-bot-staging-use.v1", action: a.action, environment: "staging" };
  };
  const h = harness();
  const consume = (a: any, _unused: unknown, now: any, emit: any) => pipelineUse(a, invoke as any, now, emit);
  await soakWindow(args({ automatic: false }), consume as any, async () => {}, () => NOW, () => 1, h.events.push.bind(h.events) as any);
  assert.equal(invoked.length, 2);
  for (const call of invoked) {
    assert.deepEqual(call, { url: URL_, token: TOKEN, runId: RUN, runAttempt: 1, action: "ready",
      sha: SHA, deploymentId: "dep-1", expiresAt: NOW + 40 * MINUTE });
  }
});

test("interval readiness is rechecked at most every 60 seconds with a final boundary check", async () => {
  const h = harness();
  await h.run(args());
  assert.ok(h.pauses.every((ms) => ms <= MINUTE));
  // Strict alternation: consume, then (pause, consume) per interval.
  const expected = ["consume"];
  for (const ms of h.pauses) expected.push(`pause:${ms}`, "consume");
  expected.push("consume");
  assert.deepEqual(h.log.filter((entry) => !entry.startsWith("emit:")), expected);
  assert.equal(h.log.at(-1), "emit:soak-complete");
  assert.equal(h.log.at(-2), "consume");
});

test("a pause that overshoots still ends with the final readiness and never exceeds 60 seconds", async () => {
  const h = harness({ pauseAdvance: () => 70_000 });
  await h.run(args({ expiresAt: NOW + 60 * MINUTE }));
  assert.ok(h.pauses.every((ms) => ms <= MINUTE));
  assert.equal(h.consumes.length, h.pauses.length + 2);
  assert.deepEqual(actionsOf(h.events), ["soak-start", "soak-complete"]);
});

test("manual mode has zero delay but demands readiness at both boundaries", async () => {
  const h = harness();
  await h.run(args({ automatic: false, expiresAt: NOW + 1 }));
  assert.deepEqual(h.pauses, []);
  assert.equal(h.consumes.length, 2);
  assert.deepEqual(h.consumes.map((call) => call.args), [consumeArgs({ expiresAt: NOW + 1 }), consumeArgs({ expiresAt: NOW + 1 })]);
  assert.deepEqual(actionsOf(h.events), ["soak-start", "soak-complete"]);
  assert.equal(h.events[0].durationMs, 0);

  for (const failOnConsume of [1, 2]) {
    const bad = harness({ failOnConsume });
    await assert.rejects(bad.run(args({ automatic: false })), code("staging_ready_refused"));
    assert.equal(bad.consumes.length, failOnConsume);
    assert.equal(bad.events.some((event) => event.action === "soak-complete"), false);
    assert.equal(bad.pauses.length, 0);
  }
});

test("insufficient budget or an already-expired window is refused before any consume", async () => {
  const cases: [Record<string, unknown>, string][] = [
    [{ expiresAt: NOW + SOAK }, "staging_window_invalid"],
    [{ expiresAt: NOW + SOAK - 1 }, "staging_window_invalid"],
    [{ expiresAt: NOW }, "staging_window_invalid"],
    [{ expiresAt: NOW - 1 }, "staging_window_invalid"],
    [{ expiresAt: Number.NaN }, "staging_window_invalid"],
    [{ expiresAt: undefined }, "staging_window_invalid"],
    [{ expiresAt: NOW + 0.5 + SOAK + 1 }, "staging_window_invalid"],
    [{ automatic: undefined }, "staging_pipeline_mode_invalid"],
    [{ automatic: "true" }, "staging_pipeline_mode_invalid"],
    [{ automatic: 1 }, "staging_pipeline_mode_invalid"],
    [{ automatic: false, expiresAt: NOW }, "staging_window_invalid"],
  ];
  for (const [over, expected] of cases) {
    const h = harness();
    await assert.rejects(h.run(args(over)), code(expected), JSON.stringify(over));
    assert.equal(h.consumes.length, 0, JSON.stringify(over));
    assert.deepEqual(h.events, [], JSON.stringify(over));
  }
});

test("the smallest sufficient budget passes and the exact boundary after consume cost expires", async () => {
  const ok = harness();
  await ok.run(args({ expiresAt: NOW + SOAK + 1 }));
  assert.equal(actionsOf(ok.events).at(-1), "soak-complete");

  // The first consume burns 7s of the budget, so NOW + SOAK + 7000 is not enough.
  const edge = harness({ firstConsumeCost: 7_000 });
  await assert.rejects(edge.run(args({ expiresAt: NOW + SOAK + 7_000 })), code("staging_window_expired"));
  assert.equal(edge.consumes.length, 1);
  assert.equal(edge.pauses.length, 0);
  assert.equal(edge.events.some((event) => event.action === "soak-complete"), false);

  const fits = harness({ firstConsumeCost: 7_000 });
  await fits.run(args({ expiresAt: NOW + SOAK + 7_001 }));
  assert.equal(actionsOf(fits.events).at(-1), "soak-complete");
});

test("expiry reached during the soak refuses the next consume and never completes", async () => {
  const h = harness({ wallJumpOnPause: 3 });
  await assert.rejects(h.run(args({ expiresAt: NOW + SOAK + 5 * MINUTE })), code("staging_window_expired"));
  assert.equal(h.pauses.length, 3);
  assert.equal(h.consumes.length, 3);
  assert.deepEqual(actionsOf(h.events), ["soak-start"]);
});

test("a monotonic clock that goes backwards or is not finite fails closed", async () => {
  for (const [name, override] of [
    ["backwards", (real: number, pauses: number) => (pauses >= 2 ? 1_000 : real)],
    ["nan", (real: number, pauses: number) => (pauses >= 2 ? Number.NaN : real)],
    ["infinity", (real: number, pauses: number) => (pauses >= 2 ? Number.NEGATIVE_INFINITY : real)],
  ] as const) {
    const h = harness({ monotonicOverride: override });
    await assert.rejects(h.run(args()), code("staging_window_invalid"), name);
    assert.deepEqual(actionsOf(h.events), ["soak-start"], name);
    assert.equal(h.pauses.length, 2, name);
  }
});

test("a forward monotonic jump past the duration still requires final readiness", async () => {
  const h = harness({ monotonicOverride: (real, pauses) => (pauses >= 2 ? real + SOAK : real) });
  await h.run(args());
  assert.equal(h.pauses.length, 2);
  // Initial, two interval checks, final.
  assert.equal(h.consumes.length, 4);
  assert.equal(h.log.at(-2), "consume");
  assert.equal(h.log.at(-1), "emit:soak-complete");

  const refused = harness({ monotonicOverride: (real, pauses) => (pauses >= 2 ? real + SOAK : real), failOnConsume: 4 });
  await assert.rejects(refused.run(args()), code("staging_ready_refused"));
  assert.equal(actionsOf(refused.events).includes("soak-complete"), false);
});

test("readiness refusal at any consume aborts without soak-complete", async () => {
  for (const failOnConsume of [1, 2, 15, 31, 32]) {
    const h = harness({ failOnConsume });
    await assert.rejects(h.run(args()), code("staging_ready_refused"), String(failOnConsume));
    assert.equal(h.consumes.length, failOnConsume, String(failOnConsume));
    assert.equal(actionsOf(h.events).includes("soak-complete"), false, String(failOnConsume));
    assert.equal(h.pauses.length, Math.min(30, failOnConsume - 1), String(failOnConsume));
    assert.deepEqual(actionsOf(h.events), failOnConsume === 1 ? [] : ["soak-start"], String(failOnConsume));
  }
});

test("expiry that elapses inside a consume is rejected after that consume", async () => {
  let wall = NOW;
  const consume = async () => { wall = NOW + 40 * MINUTE; };
  const events: any[] = [];
  await assert.rejects(soakWindow(args(), consume as any, async () => {}, () => wall, () => 1, (e: any) => events.push(e)),
    code("staging_window_expired"));
  assert.deepEqual(events, []);
});

test("emitted soak events carry only fixed fields and never the token or URL", async () => {
  const h = harness();
  await h.run(args());
  assert.equal(h.events.length, 2);
  for (const event of h.events) {
    assert.deepEqual(Object.keys(event).sort(), ["action", "deploymentId", "durationMs", "environment", "expiresAt",
      "observedAt", "runAttempt", "runId", "schema", "sha"]);
    assert.equal(event.schema, "two-bot-staging-soak.v1");
    assert.equal(event.environment, "staging");
    assert.equal(event.runId, RUN);
    assert.equal(event.sha, SHA);
    assert.equal(event.deploymentId, "dep-1");
    assert.equal(event.expiresAt, NOW + 40 * MINUTE);
  }
  const serialized = JSON.stringify(h.events);
  assert.equal(serialized.includes(TOKEN), false);
  assert.equal(serialized.includes(URL_), false);
  assert.equal(serialized.includes("token"), false);

  const failed = harness({ failOnConsume: 2 });
  await assert.rejects(failed.run(args()));
  assert.equal(JSON.stringify(failed.events).includes(TOKEN), false);
});
