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
 * evidence receipt. `held` means the join series were unusable and the
 * previous firing state holds: the caller must treat the verdict as unknown,
 * never as clear. `slowCovered` reports whether both slow windows have
 * baseline history; silence without it is unknown too, so only a covered
 * non-firing verdict may resolve the card. A malformed ring starts empty
 * (windows stay inactive until history covers them, never a false fire).
 */

import { readFileSync } from "node:fs";
import {
  EMPTY_STATE,
  evaluateMetrics,
  parseExposition,
} from "../src/alert-rules.ts";

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
  try {
    raw = readFileSync(path, "utf8");
  } catch {
    return [];
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

const args = process.argv.slice(2);
const expositionPath = argValue(args, "--exposition");
const ringPath = argValue(args, "--ring");
const nowRaw = argValue(args, "--now");
const now = nowRaw === undefined ? NaN : Number(nowRaw);
if (!expositionPath || !ringPath || !Number.isFinite(now)) {
  usage();
  process.exit(2);
}

const exposition = readFileSync(expositionPath, "utf8");
const samples = parseExposition(exposition);
const ring = loadRing(ringPath);
const previous = { ...EMPTY_STATE, joinBurn: ring };
const { firing, state, joinHeld, joinSlowCovered } = evaluateMetrics(samples, previous, now);
const verdict = {
  slow: firing.includes("voice_join_slow_burn"),
  fast: firing.includes("voice_join_fast_burn"),
  held: joinHeld === true,
  slowCovered: joinSlowCovered === true,
  ring: state.joinBurn ?? [],
};
process.stdout.write(`${JSON.stringify(verdict)}\n`);
