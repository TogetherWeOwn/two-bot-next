import { test } from "node:test";
import assert from "node:assert/strict";
import {
  EMPTY_STATE, evaluateMetrics, parseExposition,
  type MetricsAlertState,
} from "../src/alert-rules.ts";

// Regression tests for the Paperclip Review 3/5 findings on the vote-kick
// `voice_failures` arm: the missing-family hold must not latch
// room-operation alerts, and the first valid sample must baseline instead of
// paging on lifetime counts. This file is implementer-owned; the acceptance
// file `voice-enforcement-failures-alert.test.ts` is untouched.

const NOW = 1_000_000;
function body(extra: string[]): string {
  return ["# HELP x y", "# TYPE x gauge", ...extra, ""].join("\n");
}
const ev = (lines: string[], prev: MetricsAlertState = EMPTY_STATE) =>
  evaluateMetrics(parseExposition(body(lines)), prev, NOW);

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

function vk(overrides: Partial<Record<string, number>> = {}): string[] {
  return VOTE_KICK_OUTCOMES.map(
    (outcome) => `two_bot_voice_vote_kick_total{outcome="${outcome}"} ${overrides[outcome] ?? 0}`,
  );
}

test("a dead-letter alert with no vote-kick family resolves on the next quiet window", () => {
  const quiet = ev([]);
  const fired = ev([`two_bot_voice_dead_letters_total{action="create"} 1`], quiet.state);
  assert.deepEqual(fired.firing, ["voice_failures"]);
  // No vote-kick family on the next quiet window: the room-operation alert
  // must resolve, not latch via the vote-kick hold.
  assert.deepEqual(ev([], fired.state).firing, []);
  // A ratio-breach alert behaves the same way.
  const ratioFired = ev([
    `two_bot_voice_operations_total{op="create",outcome="success"} 3`,
    `two_bot_voice_operations_total{op="create",outcome="discord"} 7`,
  ]);
  assert.deepEqual(ratioFired.firing, ["voice_failures"]);
  assert.deepEqual(ev([], ratioFired.state).firing, []);
});

test("a missing family still holds a vote-kick-caused alert", () => {
  const baseline = ev(vk());
  const fired = ev(vk({ gave_up: 1 }), baseline.state);
  assert.deepEqual(fired.firing, ["voice_failures"]);
  assert.deepEqual(ev([], fired.state).firing, ["voice_failures"]);
});

test("the first valid sample baselines lifetime failures instead of paging", () => {
  // Fresh state with lifetime counts: baseline only.
  const first = ev(vk({ discord_error: 3 }));
  assert.deepEqual(first.firing, []);
  assert.deepEqual(ev(vk({ discord_error: 4 }), first.state).firing, ["voice_failures"]);
  // Legacy stored state without the new keys behaves the same way.
  const legacy = { ...ev(vk({ cooldown: 3 })).state } as Record<string, unknown>;
  for (const key of Object.keys(legacy)) {
    if (/kick|enforc|vk/i.test(key)) delete legacy[key];
  }
  const legacyFirst = ev(vk({ discord_error: 3 }), legacy as MetricsAlertState);
  assert.deepEqual(legacyFirst.firing, []);
  assert.deepEqual(
    ev(vk({ discord_error: 4 }), legacyFirst.state).firing,
    ["voice_failures"],
  );
});
