/** Watch-signal query contract: the ticker-staleness PromQL in
 * `docs/watch-signal-queries.md` must exclude never-succeeded series, matching
 * the Worker `ticker_stale` rule (`s.value > 0` in `wrangler/src/alert-rules.ts`).
 * Without the `> 0` filter a boot/parked ticker (timestamp zero) reads as
 * billions of seconds stale while the rule correctly stays silent.
 * Reads the doc as text; never scrapes anything.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const doc = readFileSync(new URL("../../docs/watch-signal-queries.md", import.meta.url), "utf8");

test("the ticker-staleness query drops zero success timestamps before subtraction", () => {
  const block = doc.match(/```promql\n([\s\S]*?two_bot_job_last_success_timestamp_seconds[\s\S]*?)```/);
  assert.ok(block, "ticker-staleness promql block with two_bot_job_last_success_timestamp_seconds not found");
  const query = block[1]!;
  assert.ok(query.includes("scheduled_messages"), "query must cover the scheduled_messages ticker");
  assert.ok(query.includes("settings"), "query must cover the settings ticker");
  assert.ok(/>\s*0/.test(query), "query must filter `> 0` so boot/parked (zero-timestamp) series are absent, not stale");
});
