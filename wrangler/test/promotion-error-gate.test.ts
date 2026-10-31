import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { join, relative } from "node:path";
import {
  decideLiveVerdict,
  decideMode,
  isDirectInvocation,
  judgeFiring,
  parseArgs,
  parseRehearsalFiring,
  runCli,
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

// Review-round pins: timer-range timeouts, 4xx handling with an auth code,
// body-read retry, workflow-spelled allowlist env, redirect-throw
// classification, and flag-position --help. Offline fixtures only.
const REVIEW_STAGING = "https://two-bot-next-staging.5150.workers.dev";
const REVIEW_PRODUCTION = "https://two-bot-next-production.fixture.workers.dev";
const REVIEW_TOKEN = "sentinel-scrape-bearer-222222222222222222";
const REVIEW_HOSTILE = "HOSTILE-REVIEW-4e8a-must-not-echo";

function reviewEnv(vars: Record<string, string | undefined>): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = {};
  for (const [key, value] of Object.entries(vars)) {
    if (value !== undefined) env[key] = value;
  }
  return env;
}

function reviewDeps(fetchImpl: unknown) {
  return { fetchImpl, sleep: async () => {}, nowSeconds: () => 1_000_000, retrySleepSeconds: 0 };
}

function silenceReviewConsole<T>(fn: () => Promise<T>): Promise<{ result: T; output: string }> {
  const lines: string[] = [];
  const origLog = console.log;
  console.log = (...args: unknown[]) => void lines.push(args.map(String).join(" "));
  return fn().then(
    (result) => {
      console.log = origLog;
      return { result, output: lines.join("\n") };
    },
    (error: unknown) => {
      console.log = origLog;
      throw error;
    },
  );
}

function reviewEvidencePath(): string {
  return join(mkdtempSync(join(tmpdir(), "gate-review-")), "evidence.json");
}

test("timeouts outside the supported timer range are refused before any fetch", async () => {
  for (const value of ["3000000", "99999999999"]) {
    let error: unknown;
    try {
      parseArgs(["--metrics-url", REVIEW_STAGING, "--evidence", "/tmp/e.json", "--scrape-timeout-seconds", value]);
    } catch (thrown: unknown) {
      error = thrown;
    }
    assert.ok(error instanceof Error, `expected a timeout refusal for ${value}`);
    assert.equal((error as { code?: unknown }).code, "invalid_timeout", `stable code for ${value}`);
  }
  // Fractional seconds stay valid: the gate rounds to a whole-millisecond delay.
  assert.equal(
    parseArgs(["--metrics-url", REVIEW_STAGING, "--evidence", "/tmp/e.json", "--scrape-timeout-seconds", "1.1"])
      .timeoutSeconds,
    1.1,
  );
  let calls = 0;
  const fetchImpl = async () => {
    calls += 1;
    return { ok: true, status: 200, text: async () => restBody(100, 0) };
  };
  await assert.rejects(
    runLiveGate({
      metricsUrl: REVIEW_STAGING,
      token: REVIEW_TOKEN,
      timeoutSeconds: 1e12,
      allowedMetricsOrigins: [REVIEW_STAGING],
      fetchImpl,
      sleep: async () => {},
      nowSeconds: () => 1_000_000,
    }),
    (error: unknown) => {
      assert.ok(error instanceof Error);
      assert.equal((error as { code?: unknown }).code, "invalid_timeout");
      return true;
    },
  );
  assert.equal(calls, 0, "an out-of-range timeout must cause zero fetches");
});

test("client errors fail at once with an auth code for 401/403, 5xx keeps retrying", async () => {
  const failing = async (status: number) => {
    let calls = 0;
    const fetchImpl = async () => {
      calls += 1;
      return { ok: false, status, text: async () => REVIEW_HOSTILE };
    };
    let error: unknown;
    try {
      await runLiveGate({
        metricsUrl: REVIEW_STAGING,
        token: REVIEW_TOKEN,
        allowedMetricsOrigins: [REVIEW_STAGING],
        fetchImpl,
        sleep: async () => {},
        nowSeconds: () => 1_000_000,
        retrySleepSeconds: 0,
      });
    } catch (thrown: unknown) {
      error = thrown;
    }
    assert.ok(error instanceof Error, `expected a failure for ${status}`);
    return { error: error as Error & { code?: unknown }, calls };
  };
  for (const status of [401, 403]) {
    const { error, calls } = await failing(status);
    assert.equal(error.code, "auth_failed", `auth code for ${status}`);
    assert.equal(calls, 1, `no retry for ${status}: the bearer must not ride a refused request`);
    assert.ok(!error.message.includes(REVIEW_HOSTILE), "auth text is fixed");
  }
  const badRequest = await failing(400);
  assert.equal(badRequest.error.code, "transport_failed");
  assert.equal(badRequest.calls, 1, "a retry cannot fix a client error");
  const serverError = await failing(500);
  assert.equal(serverError.error.code, "transport_failed");
  assert.equal(serverError.calls, 3, "5xx keeps the bounded retry");
});

test("a transient 429 is retried and recovers, a stuck 429 fails bounded", async () => {
  // /ops/metrics sheds an exhausted per-caller scrape bucket with 429 plus
  // retry-after; the gate's bounded retry must recover a transient throttle
  // instead of failing the deploy stage after a single attempt.
  let recoveredCalls = 0;
  const transient = async () => {
    recoveredCalls += 1;
    if (recoveredCalls === 1) return { ok: false, status: 429, text: async () => REVIEW_HOSTILE };
    return { ok: true, status: 200, text: async () => restBody(100, 0) };
  };
  const recovered = await runLiveGate({
    metricsUrl: REVIEW_STAGING,
    token: REVIEW_TOKEN,
    allowedMetricsOrigins: [REVIEW_STAGING],
    fetchImpl: transient,
    sleep: async () => {},
    nowSeconds: () => 1_000_000,
    retrySleepSeconds: 0,
  });
  // First scrape retries the 429 once and recovers; the second scrape is clean.
  assert.equal(recoveredCalls, 3, "a transient 429 is retried, not fatal");
  assert.deepEqual(recovered.firing, []);
  // An endpoint stuck at 429 still fails closed after the 3-attempt bound.
  let stuckCalls = 0;
  const stuck = async () => {
    stuckCalls += 1;
    return { ok: false, status: 429, text: async () => REVIEW_HOSTILE };
  };
  await assert.rejects(
    runLiveGate({
      metricsUrl: REVIEW_STAGING,
      token: REVIEW_TOKEN,
      allowedMetricsOrigins: [REVIEW_STAGING],
      fetchImpl: stuck,
      sleep: async () => {},
      nowSeconds: () => 1_000_000,
      retrySleepSeconds: 0,
    }),
    (error: unknown) => {
      assert.ok(error instanceof Error);
      assert.equal((error as { code?: unknown }).code, "transport_failed");
      assert.ok(!error.message.includes(REVIEW_HOSTILE), "refusal text is fixed");
      return true;
    },
  );
  assert.equal(stuckCalls, 3, "a stuck 429 keeps the 3-attempt bound");
});

test("an auth failure is a fail-closed receipt with exit 1", async () => {
  const fetchImpl = async () => ({ ok: false, status: 401, text: async () => REVIEW_HOSTILE });
  const evidencePath = reviewEvidencePath();
  const { result: code, output } = await silenceReviewConsole(() =>
    runCli(
      ["--metrics-url", REVIEW_STAGING, "--evidence", evidencePath, "--sha", "test-sha"],
      reviewEnv({ METRICS_SCRAPE_TOKEN: REVIEW_TOKEN, STAGING_WORKER_URL: REVIEW_STAGING }),
      reviewDeps(fetchImpl),
    ),
  );
  assert.equal(code, 1);
  const evidence = JSON.parse(readFileSync(evidencePath, "utf8")) as Record<string, unknown>;
  assert.equal(evidence.verdict, "error");
  assert.equal(evidence.code, "auth_failed");
  assert.ok(output.includes("auth_failed"), "the console names the stable code");
  assert.ok(!JSON.stringify(evidence).includes(REVIEW_TOKEN), "the receipt never carries the bearer");
});

test("body-read failures retry like connect failures and never echo", async () => {
  let calls = 0;
  const fetchImpl = async () => {
    calls += 1;
    return {
      ok: true,
      status: 200,
      text: async () => {
        throw new Error(REVIEW_HOSTILE);
      },
    };
  };
  let error: unknown;
  try {
    await runLiveGate({
      metricsUrl: REVIEW_STAGING,
      token: REVIEW_TOKEN,
      allowedMetricsOrigins: [REVIEW_STAGING],
      fetchImpl,
      sleep: async () => {},
      nowSeconds: () => 1_000_000,
      retrySleepSeconds: 0,
    });
  } catch (thrown: unknown) {
    error = thrown;
  }
  assert.ok(error instanceof Error, "expected a transport failure");
  assert.equal((error as { code?: unknown }).code, "transport_failed");
  assert.equal(calls, 3, "body-phase transients keep the 3-attempt bound");
  assert.ok(!(error as Error).message.includes(REVIEW_HOSTILE), "body-read text is fixed");
});

test("unparseable exposition still fails once as decode_failed", async () => {
  let calls = 0;
  const fetchImpl = async () => {
    calls += 1;
    return { ok: true, status: 200, text: async () => REVIEW_HOSTILE };
  };
  await assert.rejects(
    runLiveGate({
      metricsUrl: REVIEW_STAGING,
      token: REVIEW_TOKEN,
      allowedMetricsOrigins: [REVIEW_STAGING],
      fetchImpl,
      sleep: async () => {},
      nowSeconds: () => 1_000_000,
      retrySleepSeconds: 0,
    }),
    (error: unknown) => {
      assert.ok(error instanceof Error);
      assert.equal((error as { code?: unknown }).code, "decode_failed");
      return true;
    },
  );
  assert.equal(calls, 1, "a decodable fetch with garbage content is not transient");
});

test("an over-cap exposition is refused at once as decode_failed", async () => {
  // Merge pin with the 64 KiB intake cap: a body the bounded reader
  // refuses is deterministic, so it fails on the first attempt (no bearer
  // re-send) with the fixed decode_failed code, never remote bytes.
  const base = `${restBody(100, 0)}\n`;
  const pad = 64 * 1024 + 1 - Buffer.byteLength(base, "utf8") - 2;
  const over = `${base}#${"x".repeat(pad)}\n`;
  assert.equal(Buffer.byteLength(over, "utf8"), 64 * 1024 + 1);
  let calls = 0;
  const fetchImpl = async () => {
    calls += 1;
    return new Response(over, { status: 200 });
  };
  await assert.rejects(
    runLiveGate({
      metricsUrl: REVIEW_STAGING,
      token: REVIEW_TOKEN,
      allowedMetricsOrigins: [REVIEW_STAGING],
      fetchImpl,
      sleep: async () => {},
      nowSeconds: () => 1_000_000,
      retrySleepSeconds: 0,
    }),
    (error: unknown) => {
      assert.ok(error instanceof Error);
      assert.equal((error as { code?: unknown }).code, "decode_failed");
      assert.equal((error as Error).message, "metrics scrape failed: exposition could not be decoded");
      return true;
    },
  );
  assert.equal(calls, 1, "an over-cap body will not shrink on retry");
});

test("the allowlist honors the workflow-spelled origin env names", async () => {
  // The deploy workflows expose only STAGING_URL / PRODUCTION_URL on the
  // gate step: deleting either spelling must break this test, not production.
  for (const [origin, vars] of [
    [REVIEW_STAGING, { STAGING_URL: REVIEW_STAGING }],
    [REVIEW_PRODUCTION, { PRODUCTION_URL: REVIEW_PRODUCTION }],
  ] as const) {
    const { fetchImpl, calls } = stubFetch([restBody(100, 0), restBody(180, 0)]);
    const evidencePath = reviewEvidencePath();
    const { result: code } = await silenceReviewConsole(() =>
      runCli(
        ["--metrics-url", origin, "--evidence", evidencePath, "--sha", "test-sha"],
        reviewEnv({ METRICS_SCRAPE_TOKEN: REVIEW_TOKEN, ...vars }),
        reviewDeps(fetchImpl),
      ),
    );
    assert.equal(code, 0, `workflow-spelled env authorizes ${origin}`);
    assert.ok(calls() > 0, "an allowlisted origin must be scraped");
    const evidence = JSON.parse(readFileSync(evidencePath, "utf8")) as Record<string, unknown>;
    assert.equal(evidence.verdict, "pass");
  }
});

test("a redirect-shaped throw is one fixed refusal with a single attempt", async () => {
  let calls = 0;
  // Real fetch with redirect: "error" never returns a 3xx response; it throws.
  const fetchImpl = async () => {
    calls += 1;
    throw new TypeError("fetch failed", { cause: new Error("unexpected redirect") });
  };
  await assert.rejects(
    runLiveGate({
      metricsUrl: REVIEW_STAGING,
      token: REVIEW_TOKEN,
      allowedMetricsOrigins: [REVIEW_STAGING],
      fetchImpl,
      sleep: async () => {},
      nowSeconds: () => 1_000_000,
      retrySleepSeconds: 0,
    }),
    (error: unknown) => {
      assert.ok(error instanceof Error);
      assert.equal((error as { code?: unknown }).code, "redirect_refused");
      return true;
    },
  );
  assert.equal(calls, 1, "a redirect is refused once, never retried");
});

test("a help token in a value slot is data, flag-position --help still prints", async () => {
  // --user-agent --help from a templated variable: the scrape proceeds with
  // the token as data instead of exiting 0 with no evidence.
  const { fetchImpl, calls } = stubFetch([restBody(100, 0), restBody(180, 0)]);
  const livePath = reviewEvidencePath();
  const live = await silenceReviewConsole(() =>
    runCli(
      ["--metrics-url", REVIEW_STAGING, "--evidence", livePath, "--sha", "test-sha", "--user-agent", "--help"],
      reviewEnv({ METRICS_SCRAPE_TOKEN: REVIEW_TOKEN, STAGING_WORKER_URL: REVIEW_STAGING }),
      reviewDeps(fetchImpl),
    ),
  );
  assert.equal(live.result, 0);
  assert.ok(calls() > 0, "a value-position --help must not suppress the scrape");
  assert.ok(!live.output.includes("Failure codes"), "no help text for a value token");
  const liveEvidence = JSON.parse(readFileSync(livePath, "utf8")) as Record<string, unknown>;
  assert.equal(liveEvidence.mode, "live");
  // --sha -h with no token: a recorded skip carrying the value, not bare help.
  const skipPath = reviewEvidencePath();
  const skip = await silenceReviewConsole(() =>
    runCli(
      ["--metrics-url", REVIEW_STAGING, "--evidence", skipPath, "--sha", "-h"],
      reviewEnv({}),
      reviewDeps(fetchImpl),
    ),
  );
  assert.equal(skip.result, 0);
  const skipEvidence = JSON.parse(readFileSync(skipPath, "utf8")) as Record<string, unknown>;
  assert.equal(skipEvidence.mode, "skip");
  assert.equal(skipEvidence.sha, "-h");
  // Flag position keeps working.
  const help = await silenceReviewConsole(() => runCli(["--help"], reviewEnv({}), reviewDeps(fetchImpl)));
  assert.equal(help.result, 0);
  assert.ok(help.output.includes("Failure codes"), "flag-position --help prints the code map");
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
