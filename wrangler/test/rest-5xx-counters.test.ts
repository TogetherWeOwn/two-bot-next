import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseArgs, runCli } from "../scripts/rest-5xx-counters.mjs";
import { metricsLines } from "./fixtures/metrics.ts";

const ORIGIN = "https://two-bot-next-staging.example.workers.dev";
const TOKEN = "t".repeat(32);

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

test("arguments are strict", () => {
  assert.throws(() => parseArgs(["--metrics-url", ORIGIN]), Error);
  assert.throws(() => parseArgs(["--out", "x", "--unknown", "y"]), Error);
  assert.throws(() => parseArgs(["--out", "x", "--user-agent", ""]), Error);
  assert.equal(parseArgs(["--out", "x", "--metrics-url", ORIGIN]).metricsUrl, ORIGIN);
});
