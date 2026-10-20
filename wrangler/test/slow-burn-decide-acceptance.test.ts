import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { parseAlertScrape } from "../src/metrics-scrape.ts";
import { metricsLines } from "./fixtures/metrics.ts";

// Protected acceptance coverage: exercise the CLI, not a second evaluator.
const driver = fileURLToPath(new URL("../scripts/slow-burn-decide.mjs", import.meta.url));
const now = 1_700_000_000;
type Snapshot = { t: number; received: number; duplicate: number; fastMoves: number };
type Verdict = { held: boolean; slow: boolean; fast: boolean; slowCovered: boolean; ring: Snapshot[] };

function inputs(burning: boolean) {
  // 25 joins / 300 s, one slow join when burning: 8x the 0.5% budget,
  // above the existing slow threshold and below the fast threshold.
  const ring: Snapshot[] = Array.from({ length: 84 }, (_, i) => ({
    t: now - (84 - i) * 300,
    received: (i + 1) * 25,
    duplicate: 0,
    fastMoves: (i + 1) * (burning ? 24 : 25),
  }));
  const lines = metricsLines().map((line) => {
    if (line.startsWith("two_bot_voice_joins_received_total ")) return "two_bot_voice_joins_received_total 2125";
    if (line.startsWith("two_bot_voice_join_to_move_seconds_bucket{")) {
      return line.replace(/ 0$/, ` ${85 * (burning ? 24 : 25)}`);
    }
    return line;
  });
  return { ring, text: `${lines.join("\n")}\n` };
}

function decide(text: string, ring: Snapshot[], rawRing?: string): Verdict {
  const dir = mkdtempSync(join(process.env.PAPERCLIP_RUN_SCRATCH_DIR ?? tmpdir(), "slow-burn-decide-test-"));
  try {
    const exposition = join(dir, "metrics.prom");
    const cache = join(dir, "ring.json");
    writeFileSync(exposition, text);
    const previous = rawRing ?? `${JSON.stringify(ring)}\n`;
    writeFileSync(cache, previous);
    const child = spawnSync(process.execPath, [driver, "--exposition", exposition, "--ring", cache, "--now", String(now)], {
      encoding: "utf8", timeout: 10_000, maxBuffer: 1024 * 1024,
      env: { PATH: process.env.PATH ?? "" },
    });
    assert.ifError(child.error);
    assert.equal(child.status, 0, child.stderr);
    assert.equal(child.stderr, "");
    assert.equal(readFileSync(cache, "utf8"), previous, "the driver must not overwrite its caller's cache");
    return JSON.parse(child.stdout) as Verdict;
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

for (const burning of [true, false]) {
  const mode = burning ? "fire" : "clear";
  test(`valid complete scrape preserves covered ${mode} semantics`, () => {
    const { text, ring } = inputs(burning);
    assert.ok(parseAlertScrape(text), "positive fixture must be Worker-valid");
    const verdict = decide(text, ring);
    assert.equal(verdict.held, false);
    assert.equal(verdict.slow, burning);
    assert.equal(verdict.fast, false);
    assert.equal(verdict.slowCovered, true);
    assert.equal(verdict.ring.at(-1)?.t, now);
  });

  const corruptions: Record<string, (text: string) => string> = {
    "malformed unrelated line": (text) => `${text}unrelated malformed sample\n`,
    "hexadecimal unrelated value": (text) => `${text}future_metric 0x10\n`,
    "malformed unrelated labels": (text) => `${text}future_metric{broken} 1\n`,
    "duplicate unrelated series": (text) => `${text}future_metric 1\nfuture_metric 2\n`,
    "duplicate unrelated label keys": (text) => `${text}future_metric{x="a",x="b"} 1\n`,
    "bad unrelated escape": (text) => `${text}future_metric{x="bad\\q"} 1\n`,
    "missing required non-join series": (text) => text.replace("two_bot_gateway_resumes_total 0\n", ""),
    "duplicate required non-join series": (text) => `${text}two_bot_gateway_resumes_total 0\n`,
    "invalid required non-join value": (text) => text.replace("two_bot_gateway_resumes_total 0", "two_bot_gateway_resumes_total NaN"),
    "invalid optional non-join value": (text) => text.replace("two_bot_voice_creator_map_age_seconds 0", "two_bot_voice_creator_map_age_seconds -1"),
  };
  for (const [reason, corrupt] of Object.entries(corruptions)) {
    test(`Worker-refused ${mode} scrape holds: ${reason}`, () => {
      const { text, ring } = inputs(burning);
      const bad = corrupt(text);
      assert.notEqual(bad, text, "corruption must actually alter the fixture");
      assert.equal(parseAlertScrape(bad), null, "Worker must refuse this exact input");
      const verdict = decide(bad, ring);
      assert.equal(verdict.held, true, "join-valid but Worker-invalid exposition must not decide");
      assert.equal(verdict.slow, false);
      assert.equal(verdict.fast, false);
      assert.equal(verdict.slowCovered, false);
      assert.deepEqual(verdict.ring, ring, "a refused scrape must not advance or prune history");
    });
  }

  test(`valid unrelated NaN/Inf and comments do not suppress ${mode}`, () => {
    const { text, ring } = inputs(burning);
    const extended = `${text}# HELP future_metric allowed unrelated series\nfuture_metric NaN\nfuture_histogram_bucket{le="+Inf"} +Inf\n`;
    assert.ok(parseAlertScrape(extended));
    const verdict = decide(extended, ring);
    assert.equal(verdict.held, false);
    assert.equal(verdict.slow, burning);
    assert.equal(verdict.fast, false);
    assert.equal(verdict.slowCovered, true);
  });
}

test("over-cap driver ring rebuilds instead of making a covered decision", () => {
  const { text, ring } = inputs(true);
  const oversized = " ".repeat(1024 * 1024 + 1) + JSON.stringify(ring);
  const verdict = decide(text, ring, oversized);
  assert.equal(verdict.held, false, "a valid scrape can seed fresh history");
  assert.equal(verdict.slow, false);
  assert.equal(verdict.fast, false);
  assert.equal(verdict.slowCovered, false);
  assert.deepEqual(verdict.ring, [{ t: now, received: 2125, duplicate: 0, fastMoves: 2040 }]);
});

test("join-only exposition is incomplete, not a covered decision", () => {
  const { text, ring } = inputs(true);
  const joinOnly = `${text.split("\n").filter((line) => line.startsWith("two_bot_voice_join")).join("\n")}\n`;
  assert.equal(parseAlertScrape(joinOnly), null);
  const verdict = decide(joinOnly, ring);
  assert.equal(verdict.held, true);
  assert.equal(verdict.slow, false);
  assert.equal(verdict.slowCovered, false);
  assert.deepEqual(verdict.ring, ring);
});
