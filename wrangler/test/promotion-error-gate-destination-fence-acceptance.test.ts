/**
 * Acceptance tests (protected): fence promotion-gate scrape destinations.
 *
 * Contract under test: the promotion error-rate gate sends an authenticated
 * scrape bearer, so its destination is a security boundary, not a free-form
 * flag. Before any authenticated fetch the gate validates that --metrics-url
 * is a bare `https://` Worker origin (no userinfo, no explicit port, no
 * path/query/fragment, no other scheme) and that its origin is allowlisted
 * against the repository's existing deployment identities: the
 * STAGING_WORKER_URL / PRODUCTION_WORKER_URL origins from the environment.
 * Anything else is a permanent input refusal with zero fetches, zero sleeps
 * and no retry. The scrape timeout is validated as finite and positive
 * before any AbortSignal or network work. Authenticated scrapes never follow
 * redirects (`redirect: "error"`); a redirect is one fixed refusal, never a
 * followed hop. Transient transport failures keep the existing bounded retry
 * policy (3 attempts). No bypass flag and no new credential may be added.
 *
 * New surface the builder provides (all offline, all injectable):
 * - `parseArgs` throws errors carrying a stable `.code`: `usage_error`,
 *   `invalid_metrics_origin`, `invalid_timeout`.
 * - `runLiveGate` accepts `allowedMetricsOrigins: string[]` (exact origin
 *   equality; optional so existing callers keep working) and rejects with
 *   `.code` set (`invalid_metrics_origin`, `invalid_timeout`,
 *   `redirect_refused`, `transport_failed`, `decode_failed`).
 * - `runCli(argv, env, deps)` runs the CLI with injectable `fetchImpl`,
 *   `sleep`, `nowSeconds` and `retrySleepSeconds`, builds the allowlist
 *   from STAGING_WORKER_URL / PRODUCTION_WORKER_URL, writes the failure
 *   receipt, and returns the process exit code.
 *
 * These tests were written test-first against a baseline where the gate
 * appended /ops/metrics to an unchecked --metrics-url, followed redirects
 * with the bearer attached, accepted any timeout value, and embedded raw
 * exception text in its errors. The destination tests are expected to FAIL
 * there and PASS once the fence is implemented. Offline fixtures only: no
 * bearer is read, no live scrape runs, every fetch is an injected stub. Do
 * not edit this file to make an implementation pass; fix the implementation.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, readFileSync as readSource } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import * as gateModule from "../scripts/promotion-error-gate.mjs";
import {
  decideLiveVerdict,
  judgeFiring,
  parseArgs,
  runLiveGate,
} from "../scripts/promotion-error-gate.mjs";
import { metricsLines } from "./fixtures/metrics.ts";

const STAGING = "https://two-bot-next-staging.5150.workers.dev";
const PRODUCTION = "https://two-bot-next-production.fixture.workers.dev";
const TOKEN = "sentinel-scrape-bearer-000000000000000000";

type GateExports = typeof gateModule & {
  runCli?: (argv: string[], env: NodeJS.ProcessEnv, deps?: Record<string, unknown>) => Promise<number>;
};

function requireRunCli(): NonNullable<GateExports["runCli"]> {
  const runCli = (gateModule as GateExports).runCli;
  assert.equal(
    typeof runCli,
    "function",
    "runCli(argv, env, deps) must be exported so the fenced CLI is testable offline",
  );
  return runCli;
}

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

function countingFetch(bodies: string[], seen: { urls: string[]; inits: unknown[] }) {
  let calls = 0;
  const fetchImpl = async (url: unknown, init: unknown) => {
    calls += 1;
    seen.urls.push(String(url));
    seen.inits.push(init);
    const body = bodies[Math.min(calls - 1, bodies.length - 1)]!;
    return { ok: true, status: 200, text: async () => body };
  };
  return { fetchImpl, calls: () => calls };
}

function cliEnv(vars: Record<string, string | undefined>): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = {};
  for (const [key, value] of Object.entries(vars)) {
    if (value !== undefined) env[key] = value;
  }
  return env;
}

function silenceConsole<T>(fn: () => Promise<T>): Promise<{ result: T; output: string }> {
  const lines: string[] = [];
  const origLog = console.log;
  const origError = console.error;
  console.log = (...args: unknown[]) => void lines.push(args.map(String).join(" "));
  console.error = (...args: unknown[]) => void lines.push(args.map(String).join(" "));
  return fn().then(
    (result) => {
      console.log = origLog;
      console.error = origError;
      return { result, output: lines.join("\n") };
    },
    (error: unknown) => {
      console.log = origLog;
      console.error = origError;
      throw error;
    },
  );
}

test("parseArgs refuses non-bare destinations with a stable origin code", () => {
  const bad = [
    "http://two-bot-next-staging.5150.workers.dev",
    "ftp://two-bot-next-staging.5150.workers.dev",
    "https://two-bot-next-staging.5150.workers.dev:8443",
    "https://user:pass@two-bot-next-staging.5150.workers.dev",
    "https://two-bot-next-staging.5150.workers.dev@attacker.example",
    "https://two-bot-next-staging.5150.workers.dev/ops/metrics",
    "https://two-bot-next-staging.5150.workers.dev?x=1",
    "https://two-bot-next-staging.5150.workers.dev#fragment",
    "not-a-url",
    "",
  ];
  for (const metricsUrl of bad) {
    let error: unknown;
    try {
      parseArgs(["--metrics-url", metricsUrl, "--evidence", "/tmp/evidence.json"]);
    } catch (thrown: unknown) {
      error = thrown;
    }
    assert.ok(error instanceof Error, `expected a refusal for ${JSON.stringify(metricsUrl)}`);
    assert.equal(
      (error as { code?: unknown }).code,
      "invalid_metrics_origin",
      `stable code for ${JSON.stringify(metricsUrl)}`,
    );
  }
  // URL credentials must never be echoed back, even in the refusal text.
  assert.throws(
    () => parseArgs(["--metrics-url", "https://user:pass@two-bot-next-staging.5150.workers.dev", "--evidence", "/tmp/e.json"]),
    (error: unknown) => {
      assert.ok(error instanceof Error);
      assert.ok(!error.message.includes("user:pass"), "refusal must not echo URL credentials");
      return true;
    },
  );
});

test("parseArgs accepts a bare https origin with or without trailing slash", () => {
  for (const metricsUrl of [STAGING, `${STAGING}/`]) {
    const config = parseArgs(["--metrics-url", metricsUrl, "--evidence", "/tmp/evidence.json"]);
    assert.equal(config.metricsUrl, metricsUrl);
  }
});

test("parseArgs validates the scrape timeout as finite and positive", () => {
  for (const value of ["0", "-5", "NaN", "Infinity", "-Infinity", "abc", ""]) {
    let error: unknown;
    try {
      parseArgs(["--metrics-url", STAGING, "--evidence", "/tmp/e.json", "--scrape-timeout-seconds", value]);
    } catch (thrown: unknown) {
      error = thrown;
    }
    assert.ok(error instanceof Error, `expected a timeout refusal for ${JSON.stringify(value)}`);
    assert.equal((error as { code?: unknown }).code, "invalid_timeout", `stable code for ${JSON.stringify(value)}`);
  }
  const config = parseArgs(["--metrics-url", STAGING, "--evidence", "/tmp/e.json", "--scrape-timeout-seconds", "5"]);
  assert.equal(config.timeoutSeconds, 5);
});

test("parseArgs keeps its defaults and codes generic usage failures", () => {
  const config = parseArgs(["--metrics-url", STAGING, "--evidence", "/tmp/e.json"]);
  assert.equal(config.sampleIntervalSeconds, 75);
  assert.equal(config.timeoutSeconds, 15);
  assert.equal(config.userAgent, "two-bot-next-staging-rollout/1.0");
  for (const argv of [
    ["--metrics-url", STAGING],
    ["--metrics-url", STAGING, "--evidence", "/tmp/e.json", "--bogus", "x"],
    ["--metrics-url", STAGING, "--evidence", "/tmp/e.json", "--sample-interval-seconds", "0"],
  ]) {
    assert.throws(
      () => parseArgs(argv),
      (error: unknown) => {
        assert.ok(error instanceof Error);
        assert.equal((error as { code?: unknown }).code, "usage_error", `stable code for ${argv.join(" ")}`);
        return true;
      },
    );
  }
});

test("parseArgs offers no arbitrary-host bypass flag", () => {
  for (const flag of ["--allow-any-host", "--allow-insecure-host", "--skip-origin-check", "--insecure"]) {
    assert.throws(
      () => parseArgs(["--metrics-url", STAGING, "--evidence", "/tmp/e.json", flag, "true"]),
      Error,
      `${flag} must stay unknown: no bypass flag may exist`,
    );
  }
});

test("gate source carries no bypass flag and no expanded credential scope", () => {
  const source = readSource(new URL("../scripts/promotion-error-gate.mjs", import.meta.url), "utf8");
  assert.doesNotMatch(source, /allow-any-host|allowAnyHost|allow_any_host|skip-origin|skipOrigin|bypass|insecure|TRUST_|NODE_TLS|SKIP_VERIFY/i);
  // The scrape bearer stays the only token the gate reads; origin env names
  // are identities, never credentials. The length floor const is the one
  // other token-adjacent identifier already present.
  const tokens = [...source.matchAll(/[A-Z][A-Z0-9_]*TOKEN(?:_[A-Z]+)?/g)].map((match) => match[0]);
  assert.ok(tokens.length > 0, "expected the scrape token reference");
  for (const name of tokens) {
    assert.ok(
      name === "METRICS_SCRAPE_TOKEN" || name === "MIN_SCRAPE_TOKEN_LENGTH",
      `no new credential may be introduced: ${name}`,
    );
  }
});

test("runLiveGate refuses a non-allowlisted origin before any fetch or sleep", async () => {
  const seen = { urls: [] as string[], inits: [] as unknown[] };
  const { fetchImpl, calls } = countingFetch([restBody(100, 0)], seen);
  let sleeps = 0;
  let error: unknown;
  try {
    await runLiveGate({
      metricsUrl: "https://attacker.example",
      token: TOKEN,
      allowedMetricsOrigins: [STAGING],
      fetchImpl,
      sleep: async () => void (sleeps += 1),
      nowSeconds: () => 1_000_000,
    } as unknown as Parameters<typeof runLiveGate>[0]);
  } catch (thrown: unknown) {
    error = thrown;
  }
  assert.ok(error instanceof Error, "expected an origin refusal");
  assert.equal((error as { code?: unknown }).code, "invalid_metrics_origin");
  assert.equal(calls(), 0, "a refused origin must cause zero fetches");
  assert.equal(sleeps, 0, "a refused origin must not start the sample window");
});

test("runLiveGate refuses lookalike and unrelated hosts with zero fetches", async () => {
  const hostile = [
    "https://two-bot-next-staging.5150.workers.dev.attacker.example",
    "https://two-bot-next-staging-lookalike.workers.dev",
    "https://metrics.example",
    "https://127.0.0.1",
  ];
  for (const metricsUrl of hostile) {
    const seen = { urls: [] as string[], inits: [] as unknown[] };
    const { fetchImpl, calls } = countingFetch([restBody(100, 0)], seen);
    await assert.rejects(
      runLiveGate({
        metricsUrl,
        token: TOKEN,
        allowedMetricsOrigins: [STAGING, PRODUCTION],
        fetchImpl,
        sleep: async () => {},
        nowSeconds: () => 1_000_000,
      } as unknown as Parameters<typeof runLiveGate>[0]),
      (error: unknown) => {
        assert.ok(error instanceof Error);
        assert.equal((error as { code?: unknown }).code, "invalid_metrics_origin", metricsUrl);
        return true;
      },
    );
    assert.equal(calls(), 0, `zero fetches for ${metricsUrl}`);
  }
});

test("runLiveGate validates the timeout before any fetch", async () => {
  for (const timeoutSeconds of [0, -1, Number.NaN, Number.POSITIVE_INFINITY]) {
    const seen = { urls: [] as string[], inits: [] as unknown[] };
    const { fetchImpl, calls } = countingFetch([restBody(100, 0)], seen);
    await assert.rejects(
      runLiveGate({
        metricsUrl: STAGING,
        token: TOKEN,
        timeoutSeconds,
        allowedMetricsOrigins: [STAGING],
        fetchImpl,
        sleep: async () => {},
        nowSeconds: () => 1_000_000,
      } as unknown as Parameters<typeof runLiveGate>[0]),
      (error: unknown) => {
        assert.ok(error instanceof Error);
        assert.equal((error as { code?: unknown }).code, "invalid_timeout", String(timeoutSeconds));
        return true;
      },
    );
    assert.equal(calls(), 0, `zero fetches for timeout ${String(timeoutSeconds)}`);
  }
});

test("runLiveGate keeps the approved endpoint for allowlisted origins", async () => {
  for (const metricsUrl of [STAGING, `${STAGING}/`]) {
    const seen = { urls: [] as string[], inits: [] as unknown[] };
    const { fetchImpl } = countingFetch([restBody(100, 0), restBody(180, 0)], seen);
    const verdict = await runLiveGate({
      metricsUrl,
      token: TOKEN,
      allowedMetricsOrigins: [STAGING],
      fetchImpl,
      sleep: async () => {},
      nowSeconds: () => 1_000_000,
    } as unknown as Parameters<typeof runLiveGate>[0]);
    assert.deepEqual(verdict.firing, []);
    assert.ok(seen.urls.length > 0);
    for (const url of seen.urls) {
      assert.equal(url, `${STAGING}/ops/metrics`, "authorized origins retain their endpoint");
    }
  }
});

test("runLiveGate disables redirect following on authenticated scrapes", async () => {
  const seen = { urls: [] as string[], inits: [] as unknown[] };
  const { fetchImpl } = countingFetch([restBody(100, 0), restBody(180, 0)], seen);
  await runLiveGate({
    metricsUrl: STAGING,
    token: TOKEN,
    allowedMetricsOrigins: [STAGING],
    fetchImpl,
    sleep: async () => {},
    nowSeconds: () => 1_000_000,
  } as unknown as Parameters<typeof runLiveGate>[0]);
  assert.ok(seen.inits.length > 0);
  for (const init of seen.inits) {
    const options = init as { redirect?: unknown; headers?: Record<string, string> };
    assert.equal(options.redirect, "error", "the bearer must never ride a redirect");
    assert.ok(String(options.headers?.authorization ?? "").startsWith("Bearer "), "scrape stays authenticated");
  }
});

test("runLiveGate classifies a redirect as a fixed refusal without reading the body", async () => {
  let textCalls = 0;
  const fetchImpl = async () => ({ ok: false, status: 302, text: async () => void (textCalls += 1) });
  await assert.rejects(
    runLiveGate({
      metricsUrl: STAGING,
      token: TOKEN,
      allowedMetricsOrigins: [STAGING],
      fetchImpl,
      sleep: async () => {},
      nowSeconds: () => 1_000_000,
      retrySleepSeconds: 0,
    } as unknown as Parameters<typeof runLiveGate>[0]),
    (error: unknown) => {
      assert.ok(error instanceof Error);
      assert.equal((error as { code?: unknown }).code, "redirect_refused");
      return true;
    },
  );
  assert.equal(textCalls, 0, "a redirect refusal must not consume the body");
});

test("runLiveGate retains the bounded retry policy for transient transport failures", async () => {
  let calls = 0;
  const fetchImpl = async () => {
    calls += 1;
    throw new Error("connection reset");
  };
  await assert.rejects(
    runLiveGate({
      metricsUrl: STAGING,
      token: TOKEN,
      allowedMetricsOrigins: [STAGING],
      fetchImpl,
      sleep: async () => {},
      nowSeconds: () => 1_000_000,
      retrySleepSeconds: 0,
    } as unknown as Parameters<typeof runLiveGate>[0]),
  );
  assert.equal(calls, 3, "transient failures keep the existing 3-attempt bound");
});

test("runCli builds the allowlist from the staging/production origin env", async () => {
  const runCli = requireRunCli();
  const seen = { urls: [] as string[], inits: [] as unknown[] };
  const { fetchImpl, calls } = countingFetch([restBody(100, 0), restBody(180, 0)], seen);
  const evidencePath = join(mkdtempSync(join(tmpdir(), "gate-fence-")), "evidence.json");
  const { result: code } = await silenceConsole(() =>
    runCli(
      ["--metrics-url", STAGING, "--evidence", evidencePath, "--sha", "test-sha"],
      cliEnv({ METRICS_SCRAPE_TOKEN: TOKEN, STAGING_WORKER_URL: STAGING, PRODUCTION_WORKER_URL: PRODUCTION }),
      { fetchImpl, sleep: async () => {}, nowSeconds: () => 1_000_000, retrySleepSeconds: 0 },
    ),
  );
  assert.equal(code, 0);
  assert.ok(calls() > 0, "an allowlisted origin must be scraped");
  for (const url of seen.urls) {
    assert.equal(url, `${STAGING}/ops/metrics`);
  }
  const evidence = JSON.parse(readFileSync(evidencePath, "utf8")) as Record<string, unknown>;
  assert.equal(evidence.verdict, "pass");
  assert.equal(evidence.mode, "live");
});

test("runCli refuses a live destination outside the configured origins", async () => {
  const runCli = requireRunCli();
  const seen = { urls: [] as string[], inits: [] as unknown[] };
  const { fetchImpl, calls } = countingFetch([restBody(100, 0)], seen);
  const evidencePath = join(mkdtempSync(join(tmpdir(), "gate-fence-")), "evidence.json");
  const { result: code, output } = await silenceConsole(() =>
    runCli(
      ["--metrics-url", "https://attacker.example", "--evidence", evidencePath, "--sha", "test-sha"],
      cliEnv({ METRICS_SCRAPE_TOKEN: TOKEN, STAGING_WORKER_URL: STAGING, PRODUCTION_WORKER_URL: PRODUCTION }),
      { fetchImpl, sleep: async () => {}, nowSeconds: () => 1_000_000, retrySleepSeconds: 0 },
    ),
  );
  assert.equal(code, 2, "an input refusal exits 2 like other usage failures");
  assert.equal(calls(), 0, "a refused origin must cause zero fetches");
  const evidence = JSON.parse(readFileSync(evidencePath, "utf8")) as Record<string, unknown>;
  assert.equal(evidence.verdict, "error");
  assert.equal(evidence.code, "invalid_metrics_origin");
  assert.deepEqual(evidence.firing, []);
  assert.equal(evidence.sha, "test-sha");
  assert.ok(typeof evidence.nextAction === "string" && evidence.nextAction.length > 0);
  assert.ok(output.includes("invalid_metrics_origin"), "the console names the stable code");
  assert.ok(!JSON.stringify(evidence).includes(TOKEN), "the receipt never carries the bearer");
});

test("runCli fails closed when no origin env is configured", async () => {
  const runCli = requireRunCli();
  const seen = { urls: [] as string[], inits: [] as unknown[] };
  const { fetchImpl, calls } = countingFetch([restBody(100, 0)], seen);
  const evidencePath = join(mkdtempSync(join(tmpdir(), "gate-fence-")), "evidence.json");
  const { result: code } = await silenceConsole(() =>
    runCli(
      ["--metrics-url", STAGING, "--evidence", evidencePath, "--sha", "test-sha"],
      cliEnv({ METRICS_SCRAPE_TOKEN: TOKEN }),
      { fetchImpl, sleep: async () => {}, nowSeconds: () => 1_000_000, retrySleepSeconds: 0 },
    ),
  );
  assert.equal(code, 2);
  assert.equal(calls(), 0, "no configured identity means no authenticated fetch");
  const evidence = JSON.parse(readFileSync(evidencePath, "utf8")) as Record<string, unknown>;
  assert.equal(evidence.code, "invalid_metrics_origin");
});

test("runCli tolerates trailing slashes and ignores malformed origin env", async () => {
  const runCli = requireRunCli();
  const seen = { urls: [] as string[], inits: [] as unknown[] };
  const { fetchImpl } = countingFetch([restBody(100, 0), restBody(180, 0)], seen);
  const evidencePath = join(mkdtempSync(join(tmpdir(), "gate-fence-")), "evidence.json");
  // A trailing slash on the configured identity still authorizes the origin,
  // and a malformed production value is ignored, never fatal.
  const { result: code } = await silenceConsole(() =>
    runCli(
      ["--metrics-url", `${STAGING}/`, "--evidence", evidencePath, "--sha", "test-sha"],
      cliEnv({ METRICS_SCRAPE_TOKEN: TOKEN, STAGING_WORKER_URL: `${STAGING}/`, PRODUCTION_WORKER_URL: "not a url" }),
      { fetchImpl, sleep: async () => {}, nowSeconds: () => 1_000_000, retrySleepSeconds: 0 },
    ),
  );
  assert.equal(code, 0);
  for (const url of seen.urls) {
    assert.equal(url, `${STAGING}/ops/metrics`);
  }
});

test("live verdict controls are unchanged: burst fails, busy-quiet passes, quiet is insufficient", async () => {
  const quiet = countingFetch([restBody(100, 0), restBody(180, 0)], { urls: [], inits: [] });
  const quietVerdict = await runLiveGate({
    metricsUrl: STAGING,
    token: TOKEN,
    fetchImpl: quiet.fetchImpl,
    sleep: async () => {},
    nowSeconds: () => 1_000_000,
  });
  assert.deepEqual(quietVerdict.firing, []);
  assert.equal(quietVerdict.restDelta, 80);
  assert.deepEqual(decideLiveVerdict({ firing: quietVerdict.firing, restDelta: quietVerdict.restDelta }), {
    fail: false,
    verdict: "pass",
    reasons: [],
  });

  const tiny = countingFetch([restBody(0, 0), restBody(5, 2)], { urls: [], inits: [] });
  const tinyVerdict = await runLiveGate({
    metricsUrl: STAGING,
    token: TOKEN,
    fetchImpl: tiny.fetchImpl,
    sleep: async () => {},
    nowSeconds: () => 1_000_000,
  });
  assert.equal(tinyVerdict.restDelta, 7);
  assert.deepEqual(decideLiveVerdict({ firing: tinyVerdict.firing, restDelta: tinyVerdict.restDelta }), {
    fail: false,
    verdict: "insufficient_data",
    reasons: [],
  });

  const burst = countingFetch([restBody(100, 0), restBody(180, 20)], { urls: [], inits: [] });
  const burstVerdict = await runLiveGate({
    metricsUrl: STAGING,
    token: TOKEN,
    fetchImpl: burst.fetchImpl,
    sleep: async () => {},
    nowSeconds: () => 1_000_000,
  });
  assert.deepEqual(burstVerdict.firing, ["rest_5xx_rate"]);
  assert.deepEqual(judgeFiring(burstVerdict.firing), { fail: true, reasons: ["rest_5xx_rate"] });
  assert.deepEqual(decideLiveVerdict({ firing: burstVerdict.firing, restDelta: burstVerdict.restDelta }), {
    fail: true,
    verdict: "fail",
    reasons: ["rest_5xx_rate"],
  });
});
