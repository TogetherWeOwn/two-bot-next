/**
 * Acceptance coverage for PR #1105 (guarded auto-remediation for voice stall
 * and crash loop). The PR's own `remediation.test.ts` covers the pure
 * functions (trigger map, cooldown, escalation, CEO page, reset, sanitize);
 * these three tests pin only the gaps: dual-fire priority and the keepalive
 * tick contract (sanitize → evaluate → persist through DO JSON storage).
 *
 * No behavior changes here: imports only, no `src/` edits.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  EMPTY_REMEDIATION_STATE,
  REMEDIATION_COOLDOWN_SECONDS,
  evaluateRemediation,
  sanitizeRemediationState,
} from "../src/remediation.ts";

const NOW = 3_000_000;
const STALE = ["voice_creator_map_stale"];
const UNPAIRED = ["gateway_unpaired_disconnect"];

test("acceptance: when both scopes fire at once, table order is the priority", () => {
  const { messages, state } = evaluateRemediation(
    [...STALE, ...UNPAIRED],
    EMPTY_REMEDIATION_STATE,
    NOW,
  );
  assert.equal(messages.length, 1);
  assert.equal(messages[0]!.scope, "voice-stall");
  assert.equal(messages[0]!.action, "creator-map-reload");
  // The loser waits for the next window: still silent later in the same window.
  const sameWindow = evaluateRemediation([...STALE, ...UNPAIRED], state, NOW + 60);
  assert.deepEqual(sameWindow.messages, []);
});

test("acceptance: a keepalive tick persists guard state across the cooldown window", () => {
  // Tick 1 from empty DO storage recommends once.
  const tick1 = evaluateRemediation(STALE, sanitizeRemediationState(undefined, NOW), NOW);
  assert.equal(tick1.messages.length, 1);
  // DO storage serializes; the persisted shape must survive the round-trip.
  const persisted = JSON.parse(JSON.stringify(tick1.state));
  // Tick 2 inside the window stays silent and keeps the guard unchanged.
  const tick2 = evaluateRemediation(
    STALE,
    sanitizeRemediationState(persisted, NOW + 60),
    NOW + 60,
  );
  assert.deepEqual(tick2.messages, []);
  assert.deepEqual(tick2.state, tick1.state);
  // Tick 3 past the window escalates with exactly one line.
  const tick3 = evaluateRemediation(
    STALE,
    sanitizeRemediationState(tick2.state, NOW + REMEDIATION_COOLDOWN_SECONDS),
    NOW + REMEDIATION_COOLDOWN_SECONDS,
  );
  assert.equal(tick3.messages.length, 1);
  assert.equal(tick3.messages[0]!.action, "same-sha-takeover");
});

test("acceptance: a corrupt or ms-epoch guard record still recommends on the next tick", () => {
  const wedgedStores: unknown[] = [
    {},
    { lastRecommendedAtSeconds: 1_700_000_000_000, scopes: EMPTY_REMEDIATION_STATE.scopes },
  ];
  for (const stored of wedgedStores) {
    const tick = evaluateRemediation(STALE, sanitizeRemediationState(stored, NOW), NOW);
    assert.equal(tick.messages.length, 1);
    assert.equal(tick.messages[0]!.action, "creator-map-reload");
    // The restarted guard persists as a clean shape, never re-wedging.
    const roundTripped = JSON.parse(JSON.stringify(tick.state));
    assert.deepEqual(sanitizeRemediationState(roundTripped, NOW), tick.state);
  }
});
