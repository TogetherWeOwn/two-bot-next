import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  EMPTY_STATE, RULES, evaluateMetrics, packetFilename, parseExposition, ruleFor, runbookUrl, transitionMessages,
  type Evaluation, type MetricsAlertState,
} from "../src/alert-rules.ts";

/**
 * Acceptance contract for the EventSub tampered-signature alert.
 *
 * The Rust classifier counts every BadSignature delivery into
 * `two_bot_eventsub_classified_total{kind="tampered"}`. A sustained rise in
 * that counter means an active forgery attempt or a broken secret rotation
 * and must raise exactly one page, while an isolated probe (one rising
 * scrape window, then flat) must stay quiet. These tests pin the outcome,
 * not any particular streak constant: the first rising window never pages,
 * and a run of consecutive rising windows does.
 */

const RULE = "eventsub_tampered";
const KINDS = ["challenge", "notification", "revocation", "tampered", "ignored", "invalid_secret", "invalid_header", "body_too_large", "bad_signature", "invalid_timestamp", "stale_timestamp", "invalid_body", "duplicate_keys", "wrong_subscription", "invalid_challenge", "invalid_callback", "invalid_broadcaster_id", "too_many_broadcasters", "other"] as const;
const TICK_SECONDS = 60;
const T0 = 1_000_000;

function body(extra: string[]): string {
  return ["# HELP x y", "# TYPE x gauge", ...extra, ""].join("\n");
}

/** Full classified family as the Rust exposition emits it: every fixed kind, always. */
function classified(counts: Partial<Record<(typeof KINDS)[number], number>>): string[] {
  return KINDS.map((kind) => `two_bot_eventsub_classified_total{kind="${kind}"} ${counts[kind] ?? 0}`);
}

const evAt = (lines: string[], prev: MetricsAlertState, now: number): Evaluation =>
  evaluateMetrics(parseExposition(body(lines)), prev, now);

/** Feed tampered counter values, one scrape per tick, threading state like the keepalive does. */
function feed(values: number[], prev: MetricsAlertState = EMPTY_STATE, startTick = 0): Evaluation[] {
  const out: Evaluation[] = [];
  let state = prev;
  values.forEach((tampered, i) => {
    const r = evAt(classified({ tampered }), state, T0 + (startTick + i) * TICK_SECONDS);
    out.push(r);
    state = r.state;
  });
  return out;
}

/** Drive baseline + consecutive rising windows until the rule fires (or the budget runs out). */
function surged(): { evals: Evaluation[]; firing: Evaluation; lastValue: number; nextTick: number } {
  const evals = feed([0, 1, 3, 6, 10, 15, 21]);
  const firing = evals.find((r) => r.firing.includes(RULE));
  assert.ok(firing, `sustained surge never fired ${RULE}; firings: ${evals.map((r) => `[${r.firing.join(",")}]`).join(" ")}`);
  const index = evals.indexOf(firing);
  return { evals, firing, lastValue: [0, 1, 3, 6, 10, 15, 21][index]!, nextTick: index + 1 };
}

/** All alert/resolved messages a firing-key sequence would send, in order. */
function allTransitions(evals: Evaluation[]): string[] {
  const messages: string[] = [];
  let before: string[] = [];
  for (const r of evals) {
    messages.push(...transitionMessages(before, r.firing));
    before = r.firing;
  }
  return messages;
}

test("first valid sample is baseline only, even with a nonzero tampered counter", () => {
  const [first, flat] = feed([7, 7]);
  assert.deepEqual(first!.firing, []);
  assert.deepEqual(flat!.firing, []);
});

test("an isolated tampered rise in a single window stays quiet", () => {
  // One forged probe between two scrapes, then nothing: never a page.
  for (const r of feed([0, 1, 1, 1, 1])) assert.deepEqual(r.firing, []);
  // A bigger single-window burst is still a single window.
  for (const r of feed([0, 25, 25, 25])) assert.deepEqual(r.firing, []);
});

test("a sustained tampered surge raises exactly one page transition", () => {
  const { evals } = surged();
  // The baseline and the first rising window stay quiet; only the sustained run pages.
  assert.deepEqual(evals[0]!.firing, []);
  assert.deepEqual(evals[1]!.firing, [], "first rising window must not page on its own");
  // Once firing, the key is exactly the bare rule id: no broadcaster, subscription
  // or other subject suffix ever rides on the fixed kind signal.
  const fired = evals[evals.length - 1]!;
  assert.deepEqual(fired.firing, [RULE]);
  const messages = allTransitions(evals);
  assert.equal(messages.length, 1, `expected one page transition, got: ${messages.join(" | ")}`);
  assert.match(messages[0]!, new RegExp(`^two-bot-next ALERT ${RULE}:`));
  assert.doesNotMatch(messages[0]!, /\(ticket\)/, "tampered surge pages; it is not ticket-severity");
  // Page severity is the absence of the ticket marker on the rule definition.
  assert.equal(ruleFor(RULE)?.severity, undefined, "tampered surge pages");
});

test("continued rises keep firing without re-paging, and a flat valid window resolves once", () => {
  const { firing, lastValue, nextTick } = surged();
  const stillRising = feed([lastValue + 5, lastValue + 9], firing.state, nextTick);
  for (const r of stillRising) assert.deepEqual(r.firing, [RULE]);
  assert.deepEqual(allTransitions([firing, ...stillRising]).filter((m) => m.includes(" ALERT ")).length, 1);

  const flatValue = lastValue + 9;
  const resolved = evAt(classified({ tampered: flatValue }), stillRising[stillRising.length - 1]!.state, T0 + (nextTick + 2) * TICK_SECONDS);
  assert.deepEqual(resolved.firing, []);
  const resolution = transitionMessages([RULE], resolved.firing);
  assert.equal(resolution.length, 1);
  assert.match(resolution[0]!, new RegExp(`^two-bot-next RESOLVED ${RULE}\\.`));
  // Staying flat after recovery stays quiet.
  assert.deepEqual(feed([flatValue, flatValue], resolved.state, nextTick + 3).map((r) => r.firing), [[], []]);
});

test("missing or malformed scrapes preserve a firing alert and never manufacture recovery", () => {
  const { firing, lastValue, nextTick } = surged();
  const at = (lines: string[], tick: number) => evAt(lines, firing.state, T0 + (nextTick + tick) * TICK_SECONDS);
  // Older container / scrape without the family.
  assert.deepEqual(at([], 0).firing, [RULE]);
  assert.deepEqual(at([`two_bot_job_consecutive_failures{job="counter"} 0`], 1).firing, [RULE]);
  // Non-numeric value.
  assert.deepEqual(at([`two_bot_eventsub_classified_total{kind="tampered"} NaN`], 2).firing, [RULE]);
  // Duplicated tampered rows are not a valid sample.
  assert.deepEqual(at([
    `two_bot_eventsub_classified_total{kind="tampered"} ${lastValue}`,
    `two_bot_eventsub_classified_total{kind="tampered"} ${lastValue}`,
  ], 3).firing, [RULE]);
  // None of the bad scrapes corrupted the baseline: the next valid flat sample
  // still resolves, instead of looking like a fresh rise or holding forever.
  let state = firing.state;
  state = at([], 0).state;
  state = evAt([`two_bot_eventsub_classified_total{kind="tampered"} NaN`], state, T0 + (nextTick + 4) * TICK_SECONDS).state;
  assert.deepEqual(evAt(classified({ tampered: lastValue }), state, T0 + (nextTick + 5) * TICK_SECONDS).firing, []);
});

test("malformed scrapes never fire on their own", () => {
  assert.deepEqual(evAt([`two_bot_eventsub_classified_total{kind="tampered"} NaN`], EMPTY_STATE, T0).firing, []);
  const duplicated = evAt([
    `two_bot_eventsub_classified_total{kind="tampered"} 0`,
    `two_bot_eventsub_classified_total{kind="tampered"} 50`,
  ], EMPTY_STATE, T0);
  assert.deepEqual(duplicated.firing, []);
});

test("a counter reset rebaselines without fabricating a surge", () => {
  // Baseline at 10, one quiet rise, then the process restarts (counter drops).
  const before = feed([10, 11]);
  const reset = evAt(classified({ tampered: 1 }), before[before.length - 1]!.state, T0 + 2 * TICK_SECONDS);
  assert.deepEqual(reset.firing, []);
  // A single rise after the reset is again just an isolated window.
  const afterReset = feed([2, 2], reset.state, 3);
  for (const r of afterReset) assert.deepEqual(r.firing, []);
  // A fresh sustained run from the new baseline pages again.
  const again = feed([3, 5, 8, 12, 17, 23], afterReset[afterReset.length - 1]!.state, 5);
  assert.deepEqual(again[again.length - 1]!.firing, [RULE]);
});

test("only the tampered kind feeds the rule; every other kind can surge silently", () => {
  let state = EMPTY_STATE;
  for (let i = 0; i < 6; i++) {
    const r = evAt(
      classified({ tampered: 4, challenge: i * 3, notification: i * 10, revocation: i, ignored: i * 7, other: i * 2 }),
      state,
      T0 + i * TICK_SECONDS,
    );
    assert.deepEqual(r.firing, [], `tick ${i}: non-tampered growth must not fire ${RULE}`);
    state = r.state;
  }
  // An unrecognized kind value is not the tampered signal either.
  const forged = evAt([`two_bot_eventsub_classified_total{kind="forged"} 99`], state, T0 + 6 * TICK_SECONDS);
  assert.ok(!forged.firing.includes(RULE));
});

test("rows carrying labels beyond the fixed kind never drive the alert or leak into messages", () => {
  const hostile = (n: number) =>
    `two_bot_eventsub_classified_total{kind="tampered",broadcaster="12345",note="https://example.invalid/member/123?body=synthetic"} ${n}`;
  let state = EMPTY_STATE;
  const messages: string[] = [];
  let before: string[] = [];
  for (let i = 0; i < 6; i++) {
    const r = evAt([hostile(i * 10)], state, T0 + i * TICK_SECONDS);
    assert.deepEqual(r.firing, [], `tick ${i}: an extra-label row is not a valid tampered sample`);
    messages.push(...transitionMessages(before, r.firing));
    before = r.firing;
    state = r.state;
  }
  for (const m of messages) assert.doesNotMatch(m, /12345|example\.invalid|body=synthetic/);
});

test("older containers without the family and legacy stored state stay quiet and never throw", () => {
  // No family in any scrape: nothing fires, however long it runs.
  let state = EMPTY_STATE;
  for (let i = 0; i < 4; i++) {
    const r = evAt([], state, T0 + i * TICK_SECONDS);
    assert.deepEqual(r.firing, []);
    state = r.state;
  }
  // Durable Object state written before this rule existed has none of its
  // fields; the first family sample against it is a baseline, not a window.
  const legacy = {
    firing: [] as string[], rest429: 0, restTotal: 0, poolStreak: 0, dbErrors: 0, sendBlocked: 0, sendBlockedStreak: 0,
    voiceOps: 0, voiceFailures: 0, voiceDeadLetters: 0, voiceOrphans: 0, gatewayMissed: 0, gatewayMissedSeen: false,
    receiverRefusals: {}, receiverRefusalsSeen: false, receiverRefusalStreaks: {},
  } as unknown as MetricsAlertState;
  const first = evAt(classified({ tampered: 9 }), legacy, T0);
  assert.deepEqual(first.firing, []);
  const sustained = feed([10, 12, 15, 19, 24, 30], first.state, 1);
  assert.deepEqual(sustained[sustained.length - 1]!.firing, [RULE]);
});

test("the rule is catalogued once, pages, and links to an existing runbook heading", () => {
  const matches = RULES.filter((r) => r.id === RULE);
  assert.equal(matches.length, 1, `expected exactly one ${RULE} rule, got ${matches.length}`);
  const rule = matches[0]!;
  assert.equal(rule.severity, undefined, "page severity (no ticket marker)");
  assert.ok(rule.summary.length > 0);
  assert.equal(packetFilename(RULE, "2026-10-09T20-11-06Z"), `evidence-${RULE}-2026-10-09T20-11-06Z.json`);

  const slugs = new Set(
    [...readFileSync(new URL("../../docs/runbook.md", import.meta.url), "utf8").matchAll(/^#+ (.+)$/gm)]
      .map((m) => m[1]!.toLowerCase().replace(/[^a-z0-9 _-]/g, "").replace(/ /g, "-")),
  );
  const anchor = rule.runbook.split("#")[1];
  assert.ok(anchor, `${RULE}: runbook link has no anchor`);
  assert.ok(slugs.has(anchor), `${RULE}: missing runbook heading for #${anchor}`);

  const packet = transitionMessages([], [RULE])[0]!;
  assert.ok(packet.includes(runbookUrl(rule)), `${RULE}: packet lacks its runbook deep link`);
  assert.match(packet, /Build: unknown\. Logs: https:\/\//, `${RULE}: packet lacks build/log context`);
});
