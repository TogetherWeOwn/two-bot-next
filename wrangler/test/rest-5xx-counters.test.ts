import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { countersFrom, parseArgs, runCli } from "../scripts/rest-5xx-counters.mjs";
import { metricsLines } from "./fixtures/metrics.ts";

const ORIGIN = "https://two-bot-next-staging.example.workers.dev";
const TOKEN = "t".repeat(32);

/** Full valid exposition with the `other` REST route counters overridden. */
function restBody(twoXx: number | string, fiveXx: number | string): string {
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

function out(): string {
  return join(mkdtempSync(join(tmpdir(), "rest-5xx-counters-")), "counters.json");
}

async function run(env: Record<string, string>, fetchImpl: unknown, argv?: string[]) {
  const path = out();
  const lines: string[] = [];
  const log = console.log;
  console.log = (line: string) => lines.push(line);
  try {
    const code = await runCli(argv ?? ["--metrics-url", ORIGIN, "--out", path], env, {
      fetchImpl,
      retrySleepSeconds: 0,
    });
    return { code, receipt: JSON.parse(readFileSync(path, "utf8")), lines };
  } finally {
    console.log = log;
  }
}

test("one scrape records the cumulative REST total and 5xx counters", async () => {
  const calls: unknown[][] = [];
  const fetchImpl = async (...args: unknown[]) => {
    calls.push(args);
    return { ok: true, status: 200, text: async () => restBody(180, 20) };
  };
  const { code, receipt, lines } = await run(
    { STAGING_WORKER_URL: ORIGIN, METRICS_SCRAPE_TOKEN: TOKEN },
    fetchImpl,
  );
  assert.equal(code, 0);
  assert.equal(calls.length, 1);
  assert.equal(calls[0]![0], `${ORIGIN}/ops/metrics`);
  assert.equal(receipt.available, true);
  assert.equal(receipt.rest5xx, 20);
  assert.equal(receipt.restTotal, 200);
  assert.equal(receipt.incarnation, "0".repeat(24) + "00000001");
  assert.ok(!lines.join("\n").includes(TOKEN));
});

test("a scrape without the identity metric records counters with no incarnation", async () => {
  const calls: unknown[][] = [];
  const fetchImpl = async (...args: unknown[]) => {
    calls.push(args);
    const body = restBody(180, 20).split("\n")
      .filter((line) => !line.startsWith("two_bot_process_incarnation")).join("\n");
    return { ok: true, status: 200, text: async () => body };
  };
  const { code, receipt } = await run(
    { STAGING_WORKER_URL: ORIGIN, METRICS_SCRAPE_TOKEN: TOKEN },
    fetchImpl,
  );
  assert.equal(code, 0);
  assert.equal(receipt.available, true);
  assert.ok(!("incarnation" in receipt));
});

test("a missing or short bearer records no data, never a pass", async () => {
  for (const token of ["", "short"]) {
    const { code, receipt } = await run({ STAGING_WORKER_URL: ORIGIN, METRICS_SCRAPE_TOKEN: token }, async () => {
      throw new Error("must not fetch");
    });
    assert.equal(code, 0);
    assert.deepEqual(receipt, { available: false, code: "token_absent" });
  }
});

test("an unconfigured origin is refused before any request", async () => {
  const { code, receipt } = await run({ METRICS_SCRAPE_TOKEN: TOKEN }, async () => {
    throw new Error("must not fetch");
  });
  assert.equal(code, 2);
  assert.deepEqual(receipt, { available: false, code: "invalid_metrics_origin" });
});

test("scrape failures fail closed with a fixed code", async () => {
  const refused = async () => ({ ok: false, status: 401, text: async () => "" });
  const auth = await run({ STAGING_WORKER_URL: ORIGIN, METRICS_SCRAPE_TOKEN: TOKEN }, refused);
  assert.equal(auth.code, 1);
  assert.deepEqual(auth.receipt, { available: false, code: "auth_failed" });
  const garbage = async () => ({ ok: true, status: 200, text: async () => "not metrics" });
  const decode = await run({ STAGING_WORKER_URL: ORIGIN, METRICS_SCRAPE_TOKEN: TOKEN }, garbage);
  assert.equal(decode.code, 1);
  assert.deepEqual(decode.receipt, { available: false, code: "decode_failed" });
});

test("REST counter receipts reject invalid integers and unsafe sums", () => {
  const sample = (value: number, result = "2xx") => ({
    name: "two_bot_rest_requests_total", labels: { route: "other", result }, value,
  });
  for (const value of [-1, 0.5, NaN, Infinity, Number.MAX_SAFE_INTEGER + 1]) {
    assert.throws(() => countersFrom([sample(value)]));
  }
  assert.throws(() => countersFrom([sample(Number.MAX_SAFE_INTEGER), sample(1, "5xx")]));
});

test("an invalid REST counter produces an unavailable receipt", async () => {
  const fetchImpl = async () => ({ ok: true, status: 200, text: async () => restBody(Number.MAX_SAFE_INTEGER + 1, 0) });
  const { code, receipt } = await run(
    { STAGING_WORKER_URL: ORIGIN, METRICS_SCRAPE_TOKEN: TOKEN }, fetchImpl,
  );
  assert.equal(code, 1);
  assert.deepEqual(receipt, { available: false, code: "invalid_rest_counters" });
});

test("raw REST counter tokens cannot round or underflow into valid receipts", async () => {
  for (const token of ["1.0000000000000001", "9007199254740990.9", "-1e-400", "1e-400", "-0.0001"]) {
    const fetchImpl = async () => ({ ok: true, status: 200, text: async () => restBody(0, token) });
    const { code, receipt } = await run(
      { STAGING_WORKER_URL: ORIGIN, METRICS_SCRAPE_TOKEN: TOKEN }, fetchImpl,
    );
    assert.equal(code, 1, token);
    assert.deepEqual(receipt, { available: false, code: "decode_failed" }, token);
  }
});

test("valid decimal and exponent REST counter tokens remain accepted", async () => {
  for (const token of ["1.0", "10e-1", ".1e1", "+1e0", "-0e-400"]) {
    const fetchImpl = async () => ({ ok: true, status: 200, text: async () => restBody(0, token) });
    const { code, receipt } = await run(
      { STAGING_WORKER_URL: ORIGIN, METRICS_SCRAPE_TOKEN: TOKEN }, fetchImpl,
    );
    assert.equal(code, 0, token);
    assert.equal(receipt.available, true, token);
    assert.equal(receipt.rest5xx, token === "-0e-400" ? 0 : 1, token);
  }
});

test("long padded REST tokens preserve integer validation", async () => {
  const padding = "0".repeat(12_000);
  for (const [token, valid] of [
    [padding + "1", true],
    ["1" + padding + "e-12000", true],
    ["1" + padding + "1e-12001", false],
    ["1e-12001", false],
  ] as const) {
    const fetchImpl = async () => ({ ok: true, status: 200, text: async () => restBody(0, token) });
    const { code, receipt } = await run(
      { STAGING_WORKER_URL: ORIGIN, METRICS_SCRAPE_TOKEN: TOKEN }, fetchImpl,
    );
    assert.equal(code, valid ? 0 : 1);
    if (valid) {
      assert.equal(receipt.rest5xx, 1);
    } else {
      assert.deepEqual(receipt, { available: false, code: "decode_failed" });
    }
  }
});

test("arguments are strict", () => {
  assert.throws(() => parseArgs(["--metrics-url", ORIGIN]), Error);
  assert.throws(() => parseArgs(["--out", "x", "--unknown", "y"]), Error);
  assert.throws(() => parseArgs(["--out", "x", "--user-agent", ""]), Error);
  assert.equal(parseArgs(["--out", "x", "--metrics-url", ORIGIN]).metricsUrl, ORIGIN);
});
