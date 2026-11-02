/**
 * Acceptance tests (protected): safe transport-failure receipts for the
 * promotion error-rate gate.
 *
 * Contract under test: when the gate cannot judge — usage mistakes,
 * refused destinations, bad timeouts, redirect refusals, transport errors
 * or undecodable exposition — it reports a stable machine-readable code
 * plus a fixed next action, writes a fail-closed JSON evidence receipt when
 * the evidence path is usable, and exits nonzero. The closed code
 * vocabulary is `usage_error`, `invalid_metrics_origin`, `invalid_timeout`,
 * `redirect_refused`, `transport_failed`, `decode_failed`. Receipts and
 * console text never interpolate remote bodies, URL credentials, the bearer
 * or exception messages; a transport failure is verdict `error`, never
 * `pass`, `skip` or `insufficient_data`, and never a silent skip. Usage
 * failures exit 2, transport/decode/redirect failures exit 1. The gate
 * gains a `--help` text and the observability runbook gains the same short
 * code map plus an offline rehearsal command; neither adds workflow wiring.
 *
 * Receipt shape (extra keys allowed, these pinned): `{ mode, sha,
 * firing: [], verdict: "error", code, nextAction }`. Receipts are
 * deterministic: the same failure twice yields identical JSON. `runCli`
 * pre-scans argv for `--evidence <path>` so usage failures still receipt
 * even when full argument parsing fails.
 *
 * These tests were written test-first against a baseline where scrape
 * failures threw with the last raw exception message, wrote no evidence,
 * and `--help` was an unknown flag. The receipt tests are expected to FAIL
 * there and PASS once the codes, receipts, help and runbook land. Offline
 * fixtures only: every fetch is an injected stub, subprocess runs cover
 * only network-free paths (help, usage, rehearsal, skip), and body-cap
 * behavior stays owned elsewhere (no oversized bodies here). Do not edit
 * this file to make an implementation pass; fix the implementation.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import * as gateModule from "../scripts/promotion-error-gate.mjs";
import { runLiveGate } from "../scripts/promotion-error-gate.mjs";
import { metricsBody } from "./fixtures/metrics.ts";

const execFileAsync = promisify(execFile);
const GATE = fileURLToPath(new URL("../scripts/promotion-error-gate.mjs", import.meta.url));
const STAGING = "https://two-bot-next-staging.5150.workers.dev";
const PRODUCTION = "https://two-bot-next-production.fixture.workers.dev";
const TOKEN = "sentinel-scrape-bearer-111111111111111111";
const HOSTILE_EXCEPTION = "HOSTILE-EXCEPTION-7a1e-remote-text-must-not-echo";
const HOSTILE_BODY = "HOSTILE-BODY-9f3c-unparseable-remote-text";
const CODES = [
  "usage_error",
  "invalid_metrics_origin",
  "invalid_timeout",
  "redirect_refused",
  "transport_failed",
  "decode_failed",
];

type GateExports = typeof gateModule & {
  runCli?: (argv: string[], env: NodeJS.ProcessEnv, deps?: Record<string, unknown>) => Promise<number>;
};

function requireRunCli(): NonNullable<GateExports["runCli"]> {
  const runCli = (gateModule as GateExports).runCli;
  assert.equal(
    typeof runCli,
    "function",
    "runCli(argv, env, deps) must be exported so failure receipts are testable offline",
  );
  return runCli;
}

function cliEnv(vars: Record<string, string | undefined>): NodeJS.ProcessEnv {
  const env: NodeJS.ProcessEnv = {};
  for (const [key, value] of Object.entries(vars)) {
    if (value !== undefined) env[key] = value;
  }
  return env;
}

function allowlistedEnv(): NodeJS.ProcessEnv {
  return cliEnv({ METRICS_SCRAPE_TOKEN: TOKEN, STAGING_WORKER_URL: STAGING, PRODUCTION_WORKER_URL: PRODUCTION });
}

function liveDeps(fetchImpl: unknown) {
  return { fetchImpl, sleep: async () => {}, nowSeconds: () => 1_000_000, retrySleepSeconds: 0 };
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

function freshEvidence(): string {
  return join(mkdtempSync(join(tmpdir(), "gate-receipt-")), "evidence.json");
}

function readEvidence(path: string): Record<string, unknown> {
  return JSON.parse(readFileSync(path, "utf8")) as Record<string, unknown>;
}

function assertSafeText(text: string, label: string): void {
  for (const secret of [HOSTILE_EXCEPTION, HOSTILE_BODY, TOKEN, "user:pass"]) {
    assert.ok(!text.includes(secret), `${label} must not leak ${JSON.stringify(secret.slice(0, 24))}`);
  }
}

function assertErrorReceipt(evidence: Record<string, unknown>, code: string, sha = "test-sha"): void {
  assert.equal(evidence.verdict, "error", "a failure receipt never reads as a verdict");
  assert.equal(evidence.code, code);
  assert.deepEqual(evidence.firing, []);
  assert.equal(evidence.sha, sha);
  assert.ok(typeof evidence.mode === "string" && evidence.mode.length > 0);
  assert.ok(typeof evidence.nextAction === "string" && evidence.nextAction.length > 0);
  assertSafeText(JSON.stringify(evidence), "failure receipt");
}

async function runCliCase(argv: string[], env: NodeJS.ProcessEnv, fetchImpl: unknown) {
  const runCli = requireRunCli();
  return silenceConsole(() => runCli(argv, env, liveDeps(fetchImpl)));
}

test("transport failures carry a fixed code and never echo remote text", async () => {
  let calls = 0;
  const fetchImpl = async () => {
    calls += 1;
    throw new Error(HOSTILE_EXCEPTION);
  };
  let error: unknown;
  try {
    await runLiveGate({
      metricsUrl: STAGING,
      token: TOKEN,
      allowedMetricsOrigins: [STAGING],
      fetchImpl,
      sleep: async () => {},
      nowSeconds: () => 1_000_000,
      retrySleepSeconds: 0,
    } as unknown as Parameters<typeof runLiveGate>[0]);
  } catch (thrown: unknown) {
    error = thrown;
  }
  assert.ok(error instanceof Error, "expected a transport failure");
  assert.equal((error as { code?: unknown }).code, "transport_failed");
  assertSafeText(error.message, "transport error");
  assert.equal(calls, 3, "transient failures keep the existing 3-attempt bound");
});

test("HTTP error statuses share one fixed transport message", async () => {
  const messages: string[] = [];
  for (const status of [500, 503]) {
    const fetchImpl = async () => ({ ok: false, status, text: async () => HOSTILE_BODY });
    let error: unknown;
    try {
      await runLiveGate({
        metricsUrl: STAGING,
        token: TOKEN,
        allowedMetricsOrigins: [STAGING],
        fetchImpl,
        sleep: async () => {},
        nowSeconds: () => 1_000_000,
        retrySleepSeconds: 0,
      } as unknown as Parameters<typeof runLiveGate>[0]);
    } catch (thrown: unknown) {
      error = thrown;
    }
    assert.ok(error instanceof Error, `expected a transport failure for ${status}`);
    assert.equal((error as { code?: unknown }).code, "transport_failed");
    assertSafeText(error.message, `HTTP ${status} error`);
    messages.push(error.message);
  }
  assert.equal(messages[0], messages[1], "transport text is fixed across statuses");
});

test("unparseable exposition is a decode failure without remote echo", async () => {
  const fetchImpl = async () => ({ ok: true, status: 200, text: async () => HOSTILE_BODY });
  let error: unknown;
  try {
    await runLiveGate({
      metricsUrl: STAGING,
      token: TOKEN,
      allowedMetricsOrigins: [STAGING],
      fetchImpl,
      sleep: async () => {},
      nowSeconds: () => 1_000_000,
      retrySleepSeconds: 0,
    } as unknown as Parameters<typeof runLiveGate>[0]);
  } catch (thrown: unknown) {
    error = thrown;
  }
  assert.ok(error instanceof Error, "expected a decode failure");
  assert.equal((error as { code?: unknown }).code, "decode_failed");
  assertSafeText(error.message, "decode error");
});

test("redirect statuses share one fixed refusal without reading the body", async () => {
  const messages: string[] = [];
  for (const status of [301, 302, 307, 308]) {
    let textCalls = 0;
    const fetchImpl = async () => ({ ok: false, status, text: async () => void (textCalls += 1) });
    let error: unknown;
    try {
      await runLiveGate({
        metricsUrl: STAGING,
        token: TOKEN,
        allowedMetricsOrigins: [STAGING],
        fetchImpl,
        sleep: async () => {},
        nowSeconds: () => 1_000_000,
        retrySleepSeconds: 0,
      } as unknown as Parameters<typeof runLiveGate>[0]);
    } catch (thrown: unknown) {
      error = thrown;
    }
    assert.ok(error instanceof Error, `expected a redirect refusal for ${status}`);
    assert.equal((error as { code?: unknown }).code, "redirect_refused");
    messages.push(error.message);
    assert.equal(textCalls, 0, `no body read for ${status}`);
  }
  for (const message of messages) {
    assert.equal(message, messages[0], "redirect text is one fixed refusal");
  }
});

test("hostile transport errors produce a safe JSON failure receipt and exit 1", async () => {
  const fetchImpl = async () => {
    throw new Error(HOSTILE_EXCEPTION);
  };
  const evidencePath = freshEvidence();
  const { result: code, output } = await runCliCase(
    ["--metrics-url", STAGING, "--evidence", evidencePath, "--sha", "test-sha"],
    allowlistedEnv(),
    fetchImpl,
  );
  assert.equal(code, 1, "transport failures fail closed with exit 1");
  assertErrorReceipt(readEvidence(evidencePath), "transport_failed");
  assert.ok(output.includes("transport_failed"), "the console names the stable code");
  assertSafeText(output, "console output");
});

test("decode and redirect failures are fail-closed receipts, never passes", async () => {
  const decodeFetch = async () => ({ ok: true, status: 200, text: async () => HOSTILE_BODY });
  const decodePath = freshEvidence();
  const decode = await runCliCase(
    ["--metrics-url", STAGING, "--evidence", decodePath, "--sha", "test-sha"],
    allowlistedEnv(),
    decodeFetch,
  );
  assert.equal(decode.result, 1);
  assertErrorReceipt(readEvidence(decodePath), "decode_failed");
  assertSafeText(decode.output, "decode console output");

  const redirectFetch = async () => ({ ok: false, status: 302, text: async () => HOSTILE_BODY });
  const redirectPath = freshEvidence();
  const redirect = await runCliCase(
    ["--metrics-url", STAGING, "--evidence", redirectPath, "--sha", "test-sha"],
    allowlistedEnv(),
    redirectFetch,
  );
  assert.equal(redirect.result, 1);
  assertErrorReceipt(readEvidence(redirectPath), "redirect_refused");
  assertSafeText(redirect.output, "redirect console output");
});

test("an invalid timeout exits cleanly with an error receipt and zero fetches", async () => {
  let calls = 0;
  const fetchImpl = async () => {
    calls += 1;
    return { ok: true, status: 200, text: async () => metricsBody() };
  };
  const evidencePath = freshEvidence();
  const { result: code, output } = await runCliCase(
    ["--metrics-url", STAGING, "--evidence", evidencePath, "--sha", "test-sha", "--scrape-timeout-seconds", "0"],
    allowlistedEnv(),
    fetchImpl,
  );
  assert.equal(code, 2, "an input refusal exits 2");
  assert.equal(calls, 0, "an invalid timeout must cause zero fetches");
  assertErrorReceipt(readEvidence(evidencePath), "invalid_timeout");
  assert.ok(output.includes("invalid_timeout"));
});

test("usage failures write an error receipt when the evidence path is usable", async () => {
  const fetchImpl = async () => ({ ok: true, status: 200, text: async () => metricsBody() });
  const evidencePath = freshEvidence();
  const { result: code, output } = await runCliCase(
    ["--metrics-url", STAGING, "--evidence", evidencePath, "--sha", "test-sha", "--bogus", "x"],
    allowlistedEnv(),
    fetchImpl,
  );
  assert.equal(code, 2);
  assertErrorReceipt(readEvidence(evidencePath), "usage_error");
  assert.ok(output.includes("usage_error"), "the console names the stable code");
});

test("an unusable evidence path still exits nonzero with the code on the console", async () => {
  const runCli = requireRunCli();
  let calls = 0;
  const fetchImpl = async () => {
    calls += 1;
    return { ok: true, status: 200, text: async () => metricsBody() };
  };
  const missingDir = join(mkdtempSync(join(tmpdir(), "gate-receipt-")), "no-such-dir", "evidence.json");
  const { result: code, output } = await silenceConsole(() =>
    runCli(
      ["--metrics-url", "https://attacker.example", "--evidence", missingDir, "--sha", "test-sha"],
      allowlistedEnv(),
      liveDeps(fetchImpl),
    ),
  );
  assert.notEqual(code, 0, "an unwritable receipt path must not flip the exit to success");
  assert.equal(code, 2);
  assert.equal(calls, 0);
  assert.ok(output.includes("invalid_metrics_origin"), "the console carries the code when no receipt can");
});

test("failure receipts are deterministic across runs", async () => {
  const fetchImpl = async () => {
    throw new Error(HOSTILE_EXCEPTION);
  };
  const firstPath = freshEvidence();
  const secondPath = freshEvidence();
  const first = await runCliCase(
    ["--metrics-url", STAGING, "--evidence", firstPath, "--sha", "test-sha"],
    allowlistedEnv(),
    fetchImpl,
  );
  const second = await runCliCase(
    ["--metrics-url", STAGING, "--evidence", secondPath, "--sha", "test-sha"],
    allowlistedEnv(),
    fetchImpl,
  );
  assert.equal(first.result, 1);
  assert.equal(second.result, 1);
  assert.deepEqual(readEvidence(firstPath), readEvidence(secondPath), "the same failure receipts identically");
});

test("every failure code ships a stable, safe next action", async () => {
  const throwing = async () => {
    throw new Error(HOSTILE_EXCEPTION);
  };
  const okFetch = async () => ({ ok: true, status: 200, text: async () => metricsBody() });
  const redirectFetch = async () => ({ ok: false, status: 308, text: async () => HOSTILE_BODY });
  const decodeFetch = async () => ({ ok: true, status: 200, text: async () => HOSTILE_BODY });
  const cases: { code: string; argv: (evidence: string) => string[]; fetchImpl: unknown; exit: number }[] = [
    {
      code: "usage_error",
      argv: (evidence) => ["--metrics-url", STAGING, "--evidence", evidence, "--sha", "test-sha", "--bogus", "x"],
      fetchImpl: okFetch,
      exit: 2,
    },
    {
      code: "invalid_metrics_origin",
      argv: (evidence) => ["--metrics-url", "https://attacker.example", "--evidence", evidence, "--sha", "test-sha"],
      fetchImpl: okFetch,
      exit: 2,
    },
    {
      code: "invalid_timeout",
      argv: (evidence) => ["--metrics-url", STAGING, "--evidence", evidence, "--sha", "test-sha", "--scrape-timeout-seconds", "-1"],
      fetchImpl: okFetch,
      exit: 2,
    },
    {
      code: "redirect_refused",
      argv: (evidence) => ["--metrics-url", STAGING, "--evidence", evidence, "--sha", "test-sha"],
      fetchImpl: redirectFetch,
      exit: 1,
    },
    {
      code: "transport_failed",
      argv: (evidence) => ["--metrics-url", STAGING, "--evidence", evidence, "--sha", "test-sha"],
      fetchImpl: throwing,
      exit: 1,
    },
    {
      code: "decode_failed",
      argv: (evidence) => ["--metrics-url", STAGING, "--evidence", evidence, "--sha", "test-sha"],
      fetchImpl: decodeFetch,
      exit: 1,
    },
  ];
  for (const { code, argv, fetchImpl, exit } of cases) {
    const firstPath = freshEvidence();
    const secondPath = freshEvidence();
    const first = await runCliCase(argv(firstPath), allowlistedEnv(), fetchImpl);
    const second = await runCliCase(argv(secondPath), allowlistedEnv(), fetchImpl);
    assert.equal(first.result, exit, `${code} exit`);
    assert.equal(second.result, exit, `${code} exit repeats`);
    const firstReceipt = readEvidence(firstPath);
    const secondReceipt = readEvidence(secondPath);
    assertErrorReceipt(firstReceipt, code);
    assert.equal(firstReceipt.nextAction, secondReceipt.nextAction, `${code} next action is stable`);
    assertSafeText(String(firstReceipt.nextAction), `${code} next action`);
  }
});

test("--help exits 0 with the code map and an offline command", async () => {
  let code: number;
  let stdout: string;
  try {
    const result = await execFileAsync(process.execPath, [GATE, "--help"], {
      env: { PATH: process.env.PATH },
      timeout: 30_000,
    });
    code = 0;
    stdout = result.stdout;
  } catch (error: unknown) {
    code = (error as { code?: number }).code ?? 99;
    stdout = String((error as { stdout?: unknown }).stdout ?? "");
  }
  assert.equal(code, 0, "--help must work with no flags, no token and no network");
  for (const expected of CODES) {
    assert.ok(stdout.includes(expected), `--help lists ${expected}`);
  }
  assert.ok(stdout.includes("--metrics-url"), "--help documents the fenced destination flag");
  assert.ok(stdout.includes("--rehearsal-firing"), "--help shows the offline rehearsal command");
});

test("the observability runbook documents the code map and an offline rehearsal command", () => {
  const runbook = readFileSync(new URL("../../docs/runbook-observability.md", import.meta.url), "utf8");
  for (const expected of CODES) {
    assert.ok(runbook.includes(expected), `runbook lists ${expected}`);
  }
  assert.match(
    runbook,
    /node .*promotion-error-gate\.mjs[\s\S]{0,400}?--rehearsal-firing/,
    "runbook shows an offline gate command",
  );
});

async function runGateSubprocess(argv: string[], env: NodeJS.ProcessEnv): Promise<{ code: number; evidence: Record<string, unknown> | null }> {
  const evidencePath = join(mkdtempSync(join(tmpdir(), "gate-receipt-control-")), "evidence.json");
  const fullEnv: NodeJS.ProcessEnv = { ...process.env };
  for (const [key, value] of Object.entries(env)) {
    if (value === undefined) delete fullEnv[key];
    else fullEnv[key] = value;
  }
  let code: number;
  try {
    await execFileAsync(
      process.execPath,
      [GATE, "--metrics-url", STAGING, "--evidence", evidencePath, "--sha", "test-sha", ...argv],
      { env: fullEnv, timeout: 30_000 },
    );
    code = 0;
  } catch (error: unknown) {
    code = (error as { code?: number }).code ?? 99;
  }
  const evidence = existsSync(evidencePath)
    ? (JSON.parse(readFileSync(evidencePath, "utf8")) as Record<string, unknown>)
    : null;
  return { code, evidence };
}

test("control: rehearsal verdicts are unchanged", async () => {
  const failing = await runGateSubprocess(
    ["--rehearsal-firing", '["rest_5xx_rate"]'],
    { METRICS_SCRAPE_TOKEN: TOKEN },
  );
  assert.equal(failing.code, 1);
  assert.equal(failing.evidence?.mode, "rehearsal");
  assert.equal(failing.evidence?.verdict, "fail");

  const quiet = await runGateSubprocess(["--rehearsal-firing", "[]"], { METRICS_SCRAPE_TOKEN: undefined });
  assert.equal(quiet.code, 0);
  assert.equal(quiet.evidence?.mode, "rehearsal");
  assert.equal(quiet.evidence?.verdict, "pass");
});

test("control: an absent or short token is still a recorded skip", async () => {
  for (const token of [undefined, "short"] as const) {
    const result = await runGateSubprocess([], { METRICS_SCRAPE_TOKEN: token });
    assert.equal(result.code, 0);
    assert.equal(result.evidence?.mode, "skip");
    assert.equal(result.evidence?.verdict, "skip");
  }
});
