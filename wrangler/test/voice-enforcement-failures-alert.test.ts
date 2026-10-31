import { test } from "node:test";
import assert from "node:assert/strict";
import {
  EMPTY_STATE, RULES, evaluateMetrics, parseExposition, ruleFor,
  type MetricsAlertState,
} from "../src/alert-rules.ts";

// Acceptance tests: vote-kick enforcement failures must trip the existing
// `voice_failures` rule. Today the evaluator sums only
// `two_bot_voice_operations_total`, dead-letters and orphans, so a vote-kick
// path that always fails enforcement (`discord_error`, `gave_up`,
// `permission_missing` on `two_bot_voice_vote_kick_total{outcome}`) never
// pages. Benign vote-kick outcomes — refusals such as `cooldown` and
// already-terminal enforcements such as `skipped_room_gone` — must stay
// quiet. These fixtures use only the public evaluator surface so the
// builder may choose any internal state representation.

const NOW = 1_000_000;
function body(extra: string[]): string {
  return ["# HELP x y", "# TYPE x gauge", ...extra, ""].join("\n");
}
const ev = (lines: string[], prev: MetricsAlertState = EMPTY_STATE) =>
  evaluateMetrics(parseExposition(body(lines)), prev, NOW);

/** Every `outcome` value the bot renders for the vote-kick family. */
const VOTE_KICK_OUTCOMES = [
  "started", "evidence_unavailable", "not_a_room", "initiator_not_occupant",
  "target_not_occupant", "self_target", "protected_target",
  "privileged_target", "authority_unavailable", "active_vote_exists",
  "cooldown", "initiator_limited", "reused_vote_id", "unknown_vote",
  "wrong_vote_boundary", "ineligible_voter", "repeated_vote", "invalid_time",
  "connect_denied_and_disconnected", "connect_denied_target_absent",
  "skipped_room_gone", "skipped_target_protected", "permission_missing",
  "discord_error", "gave_up", "other",
] as const;

/**
 * The full vote-kick family exactly as the bot renders it: every outcome
 * present, zero unless overridden. Overrides accept raw strings so invalid
 * samples can be expressed.
 */
function vk(overrides: Partial<Record<string, number | string>> = {}): string[] {
  return VOTE_KICK_OUTCOMES.map(
    (outcome) => `two_bot_voice_vote_kick_total{outcome="${outcome}"} ${overrides[outcome] ?? 0}`,
  );
}

/** The three terminal enforcement failures that must page. */
const ENFORCEMENT_FAILURES = ["discord_error", "gave_up", "permission_missing"] as const;

/** Refusals and already-terminal/successful enforcements: never a page. */
const BENIGN: Record<string, number> = {
  started: 40,
  cooldown: 12,
  active_vote_exists: 9,
  initiator_limited: 7,
  protected_target: 5,
  connect_denied_and_disconnected: 30,
  connect_denied_target_absent: 4,
  skipped_room_gone: 6,
  skipped_target_protected: 3,
};

for (const outcome of ENFORCEMENT_FAILURES) {
  test(`one new ${outcome} vote-kick enforcement failure fires voice_failures`, () => {
    const baseline = ev(vk());
    assert.deepEqual(baseline.firing, []);
    assert.deepEqual(ev(vk({ [outcome]: 1 }), baseline.state).firing, ["voice_failures"]);
  });
}

test("benign vote-kick refusals and terminal skips never fire voice_failures", () => {
  const baseline = ev(vk());
  const benignOnly = ev(vk(BENIGN), baseline.state);
  assert.deepEqual(benignOnly.firing, []);
  // A busier second benign window stays just as quiet.
  const busier = Object.fromEntries(
    Object.entries(BENIGN).map(([outcome, n]) => [outcome, n + 25]),
  );
  assert.deepEqual(ev(vk(busier), benignOnly.state).firing, []);
});

test("an enforcement failure fires alongside benign volume and healthy room operations", () => {
  const baseline = ev([
    `two_bot_voice_operations_total{op="create",outcome="success"} 100`,
    ...vk(BENIGN),
  ]);
  // Room operations stay 100% healthy and benign vote-kick outcomes keep
  // rising: neither may mask or dilute the single new enforcement failure.
  const mixed = ev([
    `two_bot_voice_operations_total{op="create",outcome="success"} 220`,
    ...vk({ ...BENIGN, started: 90, gave_up: 1 }),
  ], baseline.state);
  assert.deepEqual(mixed.firing, ["voice_failures"]);
});

test("a flat enforcement counter resolves instead of firing on its absolute value", () => {
  const established = ev(vk({ discord_error: 5 })).state;
  const fired = ev(vk({ discord_error: 6 }), established);
  assert.deepEqual(fired.firing, ["voice_failures"]);
  // No new enforcement failure in the next window: the alert recovers.
  assert.deepEqual(ev(vk({ discord_error: 6 }), fired.state).firing, []);
});

test("a counter reset rebaselines vote-kick enforcement failures", () => {
  const established = ev(vk({ permission_missing: 10 })).state;
  // The process restarted and its counters went backwards: no window, no page.
  const reset = ev(vk({ permission_missing: 1 }), established);
  assert.deepEqual(reset.firing, []);
  assert.deepEqual(ev(vk({ permission_missing: 1 }), reset.state).firing, []);
  // A genuinely new failure after the reset fires again.
  assert.deepEqual(ev(vk({ permission_missing: 2 }), reset.state).firing, ["voice_failures"]);
});

test("a missing vote-kick family preserves a firing voice_failures alert", () => {
  const baseline = ev(vk());
  const fired = ev(vk({ gave_up: 1 }), baseline.state);
  assert.deepEqual(fired.firing, ["voice_failures"]);
  // A scrape with healthy room operations but no vote-kick family (older
  // container or partial exposition) must hold the alert, not resolve it.
  // A counter reset is different: it rebaselines and resolves (see the
  // reset test above and the restart case in alert-rules.test.ts), so the
  // hold applies only while the room-operation counters have not reset.
  const missing = ev(
    [`two_bot_voice_operations_total{op="create",outcome="success"} 10`],
    fired.state,
  );
  assert.deepEqual(missing.firing, ["voice_failures"]);
  // When the family returns with no new failures, the alert recovers.
  assert.deepEqual(
    ev([
      `two_bot_voice_operations_total{op="create",outcome="success"} 10`,
      ...vk({ gave_up: 1 }),
    ], missing.state).firing,
    [],
  );
  // A wholly empty sample holds the alert too, and an absent family never
  // fires on its own.
  assert.deepEqual(ev([], fired.state).firing, ["voice_failures"]);
  assert.deepEqual(ev([]).firing, []);
});

test("an invalid vote-kick sample preserves state and never fires on its own", () => {
  assert.deepEqual(ev(vk({ discord_error: "not_a_number" })).firing, []);
  const baseline = ev(vk());
  const fired = ev(vk({ discord_error: 1 }), baseline.state);
  assert.deepEqual(fired.firing, ["voice_failures"]);
  assert.deepEqual(
    ev(vk({ discord_error: "not_a_number" }), fired.state).firing,
    ["voice_failures"],
  );
});

test("legacy alert state without vote-kick tracking stays safe and rebaselines", () => {
  // State persisted before the evaluator tracked vote-kick enforcements:
  // drop any vote-kick/enforcement bookkeeping keys, whatever they are named.
  const legacy = { ...ev(vk({ cooldown: 3 })).state } as Record<string, unknown>;
  for (const key of Object.keys(legacy)) {
    if (/kick|enforc|vk/i.test(key)) delete legacy[key];
  }
  const legacyState = legacy as MetricsAlertState;
  const benign = ev(vk({ cooldown: 9 }), legacyState);
  assert.deepEqual(benign.firing, []);
  assert.deepEqual(
    ev(vk({ cooldown: 9, discord_error: 1 }), benign.state).firing,
    ["voice_failures"],
  );
});

test("existing voice_failures arms are unchanged and never double-fire", () => {
  // The room-operation ratio arm fires with no vote-kick family present.
  assert.deepEqual(
    ev([
      `two_bot_voice_operations_total{op="create",outcome="success"} 3`,
      `two_bot_voice_operations_total{op="create",outcome="discord"} 7`,
    ]).firing,
    ["voice_failures"],
  );
  // A lone new dead-letter still fires below the ratio window.
  const quiet = ev([]);
  assert.deepEqual(
    ev([`two_bot_voice_dead_letters_total{action="create"} 1`], quiet.state).firing,
    ["voice_failures"],
  );
  // Both arms tripping in one window still report the rule exactly once.
  const baseline = ev(vk());
  assert.deepEqual(
    ev([
      `two_bot_voice_operations_total{op="create",outcome="success"} 3`,
      `two_bot_voice_operations_total{op="create",outcome="discord"} 7`,
      ...vk({ discord_error: 2 }),
    ], baseline.state).firing,
    ["voice_failures"],
  );
});

test("voice_failures keeps its rule identity and runbook anchor", () => {
  assert.equal(RULES.filter((rule) => rule.id === "voice_failures").length, 1);
  assert.equal(ruleFor("voice_failures")?.runbook, "runbook.md#alert-voice-failures");
});
