import { test } from "node:test";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import { relative } from "node:path";
import {
  decideLiveVerdict,
  decideMode,
  isDirectInvocation,
  judgeFiring,
  parseArgs,
  parseRehearsalFiring,
  runLiveGate,
} from "../scripts/promotion-error-gate.mjs";
import { metricsLines } from "./fixtures/metrics.ts";

/** Full valid exposition with the `other` REST route counters overridden. */
function restBody(twoXx: number, fiveXx: number): string {
  return metricsLines()
    .map((line) => {
      if (line === `two_bot_rest_requests_total{route="other",result="2xx"} 0`) {
        return `two_bot_rest_requests_total{route="other",result="2xx"} ${twoXx}`;
      }
      if (line === `two_bot_rest_requests_total{route="other",result="5xx"} 0`) {
        return `two_bot_rest_requests_total{route="other",result="5xx"} ${fiveXx}`;
      }
      return line;
    })
    .join("\n");
}

const stubFetch = (bodies: string[]) => {
  let calls = 0;
  const fetchImpl = async () => {
    const body = bodies[calls++] ?? bodies[bodies.length - 1]!;
    return { ok: true, status: 200, text: async () => body };
  };
  return { fetchImpl, calls: () => calls };
};

test("rehearsal judges the injected firing through the promotion helper", () => {
  assert.deepEqual(judgeFiring(parseRehearsalFiring('["rest_5xx_rate"]')), {
    fail: true,
    reasons: ["rest_5xx_rate"],
  });
  // 429 backpressure is not a rollback trigger, even in rehearsal.
  assert.deepEqual(judgeFiring(parseRehearsalFiring('["rest_429_rate"]')), { fail: false, reasons: [] });
  assert.deepEqual(judgeFiring(parseRehearsalFiring("[]")), { fail: false, reasons: [] });
});

test("direct invocation compares canonical paths, never argv shape", () => {
  const moduleUrl = new URL("../scripts/promotion-error-gate.mjs", import.meta.url);
  const absolute = fileURLToPath(moduleUrl);
  // Absolute or cwd-relative: both must detect the executed script, so the
  // gate can never silently no-op on a Node version that spells argv[1]
  // differently.
  assert.equal(isDirectInvocation(absolute, moduleUrl.href), true);
  assert.equal(isDirectInvocation(relative(process.cwd(), absolute), moduleUrl.href), true);
  assert.equal(isDirectInvocation(absolute, import.meta.url), false);
  assert.equal(isDirectInvocation(undefined, moduleUrl.href), false);
  assert.equal(isDirectInvocation("", moduleUrl.href), false);
  assert.equal(isDirectInvocation("/nonexistent/gate.mjs", moduleUrl.href), false);
});

test("malformed rehearsal firing fails closed", () => {
  for (const raw of ["not-json", "42", '{"rule":"rest_5xx_rate"}', '["rest_5xx_rate", 7]']) {
    assert.throws(() => parseRehearsalFiring(raw), Error);
  }
});

test("user agent defaults to the staging rollout name and accepts a production override", () => {
  const base = ["--metrics-url", "https://example.test", "--evidence", "/tmp/evidence.json"];
  assert.equal(parseArgs(base).userAgent, "two-bot-next-staging-rollout/1.0");
  assert.equal(
    parseArgs([...base, "--user-agent", "two-bot-next-production-rollout/1.0"]).userAgent,
    "two-bot-next-production-rollout/1.0",
  );
  assert.throws(() => parseArgs([...base, "--user-agent", ""]), Error);
});

test("mode selection prefers rehearsal, then a full-length token", () => {
  assert.deepEqual(
    decideMode({ rehearsalFiring: '["rest_5xx_rate"]', token: "x".repeat(40) }),
    { mode: "rehearsal" },
  );
  assert.deepEqual(decideMode({ rehearsalFiring: undefined, token: "x".repeat(40) }), { mode: "live" });
  assert.deepEqual(decideMode({ rehearsalFiring: undefined, token: undefined }).mode, "skip");
  assert.deepEqual(decideMode({ rehearsalFiring: undefined, token: "short" }).mode, "skip");
  assert.deepEqual(decideMode({ rehearsalFiring: undefined, token: "x".repeat(31) }).mode, "skip");
});

test("live gate sends the configured user agent on scrapes", async () => {
  const seen: string[] = [];
  const fetchImpl = async (...args: any[]) => {
    seen.push((args[1] as { headers: Record<string, string> }).headers["user-agent"]);
    return { ok: true, status: 200, text: async () => restBody(100, 0) };
  };
  await runLiveGate({
    metricsUrl: "https://example.test",
    token: "x".repeat(40),
    sampleIntervalSeconds: 75,
    fetchImpl,
    sleep: async () => {},
    nowSeconds: () => 1_000_000,
    userAgent: "two-bot-next-production-rollout/1.0",
  });
  assert.ok(seen.length > 0);
  assert.ok(seen.every((userAgent) => userAgent === "two-bot-next-production-rollout/1.0"));
});

test("live gate fails the stage on a 5xx burst between scrapes", async () => {
  const { fetchImpl } = stubFetch([restBody(100, 0), restBody(180, 20)]);
  const verdict = await runLiveGate({
    metricsUrl: "https://staging.example",
    token: "x".repeat(40),
    sampleIntervalSeconds: 75,
    fetchImpl,
    sleep: async () => {},
    nowSeconds: () => 1_000_000,
  });
  assert.deepEqual(verdict.firing, ["rest_5xx_rate"]);
  assert.deepEqual(judgeFiring(verdict.firing), { fail: true, reasons: ["rest_5xx_rate"] });
  assert.equal(verdict.restDelta, 100);
  assert.deepEqual(decideLiveVerdict({ firing: verdict.firing, restDelta: verdict.restDelta }), {
    fail: true,
    verdict: "fail",
    reasons: ["rest_5xx_rate"],
  });
});

test("live gate passes busy quiet windows, flags too-quiet ones", async () => {
  // Enough requests and no 5xx: a genuine pass with the judged volume.
  const quiet = stubFetch([restBody(100, 0), restBody(180, 0)]);
  const quietVerdict = await runLiveGate({
    metricsUrl: "https://staging.example",
    token: "x".repeat(40),
    fetchImpl: quiet.fetchImpl,
    sleep: async () => {},
    nowSeconds: () => 1_000_000,
  });
  assert.deepEqual(quietVerdict.firing, []);
  assert.equal(quietVerdict.restDelta, 80);
  assert.deepEqual(
    decideLiveVerdict({ firing: quietVerdict.firing, restDelta: quietVerdict.restDelta }),
    { fail: false, verdict: "pass", reasons: [] },
  );
  // Below the 10-request minimum the window cannot judge: insufficient_data,
  // never a pass, so a too-quiet window cannot certify the build.
  const tiny = stubFetch([restBody(0, 0), restBody(5, 2)]);
  const tinyVerdict = await runLiveGate({
    metricsUrl: "https://staging.example",
    token: "x".repeat(40),
    fetchImpl: tiny.fetchImpl,
    sleep: async () => {},
    nowSeconds: () => 1_000_000,
  });
  assert.deepEqual(tinyVerdict.firing, []);
  assert.equal(tinyVerdict.restDelta, 7);
  assert.deepEqual(
    decideLiveVerdict({ firing: tinyVerdict.firing, restDelta: tinyVerdict.restDelta }),
    { fail: false, verdict: "insufficient_data", reasons: [] },
  );
});

test("live gate fails closed when metrics are unreadable", async () => {
  const failing = async () => {
    throw new Error("connection reset");
  };
  await assert.rejects(
    runLiveGate({
      metricsUrl: "https://staging.example",
      token: "x".repeat(40),
      fetchImpl: failing,
      sleep: async () => {},
      nowSeconds: () => 1_000_000,
      retrySleepSeconds: 0,
    }),
    /metrics scrape failed/,
  );
  const malformed = stubFetch(["not an exposition"]);
  await assert.rejects(
    runLiveGate({
      metricsUrl: "https://staging.example",
      token: "x".repeat(40),
      fetchImpl: malformed.fetchImpl,
      sleep: async () => {},
      nowSeconds: () => 1_000_000,
      retrySleepSeconds: 0,
    }),
    /metrics scrape failed/,
  );
});
