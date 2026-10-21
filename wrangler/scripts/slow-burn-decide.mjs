#!/usr/bin/env node
/**
 * Slow-burn firing verdict for the creator-join SLO tracker.
 *
 * Reads one `/ops/metrics` exposition plus the tracker's persisted burn ring
 * (cumulative `two_bot_voice_joins_received_total`, benign `duplicate`
 * refusals and `two_bot_voice_join_to_move_seconds_bucket{le="5"}` snapshots)
 * and reports whether `voice_join_slow_burn` fires, using the checked-in
 * evaluator from `../src/alert-rules.ts` with zero edits to the rule file.
 * The caller owns the ring: it restores the previous ring, appends the fresh
 * snapshot and prunes entries older than the slow window plus margin, exactly
 * like the Worker does. This driver only answers the firing question and
 * returns the next ring.
 *
 * Usage:
 *   node wrangler/scripts/slow-burn-decide.mjs \
 *     --exposition <metrics.prom> --ring <ring.json> --now <epoch-seconds>
 *
 * Prints one JSON object to stdout:
 *   {"slow": bool, "fast": bool, "held": bool, "slowCovered": bool, "ring": JoinBurnSnapshot[]}
 * `slow` is the tracker's only filing signal; `fast` is reported for the
 * evidence receipt. `held` means the Worker refused the scrape or the join
 * series were unusable. The caller must treat the verdict as unknown,
 * never as clear. `slowCovered` reports whether both slow windows have
 * baseline history; silence without it is unknown too, so only a covered
 * non-firing verdict may resolve the card. A malformed ring starts empty
 * (windows stay inactive until history covers them, never a false fire).
 */

import { closeSync, openSync, readSync } from "node:fs";
import { EMPTY_STATE, evaluateMetrics } from "../src/alert-rules.ts";
import { parseAlertScrape } from "../src/metrics-scrape.ts";

const MAX_CACHE_BYTES = 1024 * 1024;
// Exposition cap mirrors the Worker scrape (MAX_METRICS_BODY_BYTES in
// wrangler/src/index.ts): every sibling reader refuses bodies over 64 KiB.
const MAX_EXPOSITION_BYTES = 64 * 1024;

function usage() {
  console.error(
    "usage: slow-burn-decide.mjs --exposition <path> --ring <path> --now <epoch-seconds>",
  );
}

function argValue(args, name) {
  const index = args.indexOf(name);
  if (index === -1 || index + 1 >= args.length) return undefined;
  return args[index + 1];
}

function loadRing(path) {
  let raw;
  let fd;
  try {
    fd = openSync(path, "r");
    const buffer = Buffer.alloc(MAX_CACHE_BYTES + 1);
    let size = 0;
    while (size < buffer.length) {
      const count = readSync(fd, buffer, size, buffer.length - size, null);
      if (count === 0) break;
      size += count;
    }
    if (size > MAX_CACHE_BYTES) return [];
    raw = buffer.subarray(0, size).toString("utf8");
  } catch {
    return [];
  } finally {
    if (fd !== undefined) closeSync(fd);
  }
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return [];
  }
  if (!Array.isArray(parsed)) return [];
  const ring = [];
  for (const entry of parsed) {
    if (
      entry === null ||
      typeof entry !== "object" ||
      !Number.isFinite(entry.t) ||
      !Number.isSafeInteger(entry.received) ||
      !Number.isSafeInteger(entry.duplicate) ||
      !Number.isSafeInteger(entry.fastMoves) ||
      entry.received < 0 ||
      entry.duplicate < 0 ||
      entry.fastMoves < 0
    ) {
      return [];
    }
    ring.push({
      t: entry.t,
      received: entry.received,
      duplicate: entry.duplicate,
      fastMoves: entry.fastMoves,
    });
  }
  return ring;
}

/**
 * Bounded exposition read matching loadRing's capped pattern. Returns
 * null when the file exceeds MAX_EXPOSITION_BYTES; the caller holds the
 * verdict exactly like a Worker-refused scrape. IO errors propagate so a
 * missing file still crashes instead of reading as a refusal.
 */
function loadExposition(path) {
  let fd;
  try {
    fd = openSync(path, "r");
    const buffer = Buffer.alloc(MAX_EXPOSITION_BYTES + 1);
    let size = 0;
    while (size < buffer.length) {
      const count = readSync(fd, buffer, size, buffer.length - size, null);
      if (count === 0) break;
      size += count;
    }
    if (size > MAX_EXPOSITION_BYTES) return null;
    return buffer.subarray(0, size).toString("utf8");
  } finally {
    if (fd !== undefined) closeSync(fd);
  }
}

const args = process.argv.slice(2);
const expositionPath = argValue(args, "--exposition");
const ringPath = argValue(args, "--ring");
const nowRaw = argValue(args, "--now");
const now = nowRaw === undefined ? NaN : Number(nowRaw);
if (!expositionPath || !ringPath || !Number.isFinite(now)) {
  usage();
  process.exit(2);
}

const exposition = loadExposition(expositionPath);
const samples = exposition === null ? null : parseAlertScrape(exposition);
const ring = loadRing(ringPath);
const previous = { ...EMPTY_STATE, joinBurn: ring };
let verdict = { slow: false, fast: false, held: true, slowCovered: false, ring };
if (samples !== null) {
  const { firing, state, joinHeld, joinSlowCovered } = evaluateMetrics(samples, previous, now);
  verdict = {
    slow: firing.includes("voice_join_slow_burn"),
    fast: firing.includes("voice_join_fast_burn"),
    held: joinHeld === true,
    slowCovered: joinSlowCovered === true,
    ring: state.joinBurn ?? [],
  };
}
process.stdout.write(`${JSON.stringify(verdict)}\n`);
