/**
 * Guarded auto-remediation recommendations: trigger mapping, 30-minute
 * cooldown, R1-to-R2 escalation, two-failure stop with CEO page, and
 * recovery reset. The Worker never restarts containers; these pure
 * functions decide what the keepalive tick says, not what it does.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  EMPTY_REMEDIATION_STATE,
  REMEDIATION_COOLDOWN_SECONDS,
  REMEDIATION_MAX_CONSECUTIVE_FAILURES,
  REMEDIATION_TRIGGERS,
  evaluateRemediation,
  operatorInstruction,
  remediationRunbookUrl,
  sanitizeRemediationState,
  type RemediationState,
} from "../src/remediation.ts";
import { RUNBOOK_BASE_URL } from "../src/alert-rules.ts";

const NOW = 2_000_000;
const STALE = ["voice_creator_map_stale"];
const UNPAIRED = ["gateway_unpaired_disconnect"];

function anchors(doc: string): Set<string> {
  const text = readFileSync(new URL(`../../docs/${doc}`, import.meta.url), "utf8");
  return new Set([...text.matchAll(/^#+ (.+)$/gm)].map((m) =>
    m[1]!.toLowerCase().replace(/[^a-z0-9 _-]/g, "").replace(/ /g, "-")));
}

test("voice stall recommends the in-process reload first, with no host action", () => {
  const { messages, state } = evaluateRemediation(STALE, EMPTY_REMEDIATION_STATE, NOW);
  assert.equal(messages.length, 1);
  assert.equal(messages[0]!.kind, "recommend");
  assert.equal(messages[0]!.scope, "voice-stall");
  assert.equal(messages[0]!.action, "creator-map-reload");
  assert.match(messages[0]!.text, /REMEDIATION voice-stall: creator-map-reload/);
  assert.match(messages[0]!.text, /No host action/);
  assert.equal(state.scopes["voice-stall"].phase, "reload-recommended");
  assert.equal(state.scopes["voice-stall"].consecutiveFailures, 0);
  assert.equal(state.lastRecommendedAtSeconds, NOW);
});

test("a stall that survives the reload escalates to same-sha-takeover after cooldown", () => {
  const first = evaluateRemediation(STALE, EMPTY_REMEDIATION_STATE, NOW);
  const second = evaluateRemediation(STALE, first.state, NOW + REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(second.messages.length, 1);
  assert.equal(second.messages[0]!.kind, "recommend");
  assert.equal(second.messages[0]!.action, "same-sha-takeover");
  assert.match(second.messages[0]!.text, /Operator: file one host-action card/);
  assert.equal(second.state.scopes["voice-stall"].phase, "takeover-recommended");
  assert.equal(second.state.scopes["voice-stall"].consecutiveFailures, 1);
});

test("two consecutive failures stop the scope with a CEO page, then silence", () => {
  assert.equal(REMEDIATION_MAX_CONSECUTIVE_FAILURES, 2);
  let state = EMPTY_REMEDIATION_STATE;
  state = evaluateRemediation(STALE, state, NOW).state;
  state = evaluateRemediation(STALE, state, NOW + REMEDIATION_COOLDOWN_SECONDS).state;
  const paged = evaluateRemediation(STALE, state, NOW + 2 * REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(paged.messages.length, 1);
  assert.equal(paged.messages[0]!.kind, "ceo-page");
  assert.equal(paged.messages[0]!.action, undefined);
  assert.match(paged.messages[0]!.text, /REMEDIATION STOPPED voice-stall/);
  assert.match(paged.messages[0]!.text, /Paging the CEO card/);
  assert.equal(paged.state.scopes["voice-stall"].suppressed, true);
  const quiet = evaluateRemediation(STALE, paged.state, NOW + 3 * REMEDIATION_COOLDOWN_SECONDS);
  assert.deepEqual(quiet.messages, []);
  assert.equal(quiet.state.scopes["voice-stall"].suppressed, true);
});

test("resolution resets the guard, including suppression", () => {
  let state = EMPTY_REMEDIATION_STATE;
  state = evaluateRemediation(STALE, state, NOW).state;
  state = evaluateRemediation(STALE, state, NOW + REMEDIATION_COOLDOWN_SECONDS).state;
  const paged = evaluateRemediation(STALE, state, NOW + 2 * REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(paged.state.scopes["voice-stall"].suppressed, true);
  const resolved = evaluateRemediation([], paged.state, NOW + 3 * REMEDIATION_COOLDOWN_SECONDS);
  assert.deepEqual(resolved.messages, []);
  assert.deepEqual(resolved.state.scopes["voice-stall"], {
    phase: "idle",
    consecutiveFailures: 0,
    suppressed: false,
  });
  const refire = evaluateRemediation(STALE, resolved.state, NOW + 3 * REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(refire.messages[0]!.action, "creator-map-reload");
});

test("cooldown holds recommendations inside 30 minutes but keeps the guard", () => {
  const first = evaluateRemediation(STALE, EMPTY_REMEDIATION_STATE, NOW);
  const held = evaluateRemediation(STALE, first.state, NOW + REMEDIATION_COOLDOWN_SECONDS - 1);
  assert.deepEqual(held.messages, []);
  assert.deepEqual(held.state, first.state);
});

test("no recommendation without its alert firing alongside", () => {
  assert.deepEqual(evaluateRemediation([], EMPTY_REMEDIATION_STATE, NOW).messages, []);
  assert.deepEqual(
    evaluateRemediation(["voice_join_fast_burn"], EMPTY_REMEDIATION_STATE, NOW).messages,
    [],
  );
  assert.deepEqual(
    evaluateRemediation(["voice_creator_map_stale:extra"], EMPTY_REMEDIATION_STATE, NOW).messages,
    [],
  );
});

test("crash loop goes straight to same-sha-takeover: a reload cannot fix a missing gateway", () => {
  const first = evaluateRemediation(UNPAIRED, EMPTY_REMEDIATION_STATE, NOW);
  assert.equal(first.messages.length, 1);
  assert.equal(first.messages[0]!.scope, "crash-loop");
  assert.equal(first.messages[0]!.action, "same-sha-takeover");
  const retry = evaluateRemediation(UNPAIRED, first.state, NOW + REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(retry.messages[0]!.action, "same-sha-takeover");
  assert.equal(retry.state.scopes["crash-loop"].consecutiveFailures, 1);
  const paged = evaluateRemediation(UNPAIRED, retry.state, NOW + 2 * REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(paged.messages[0]!.kind, "ceo-page");
  assert.equal(paged.state.scopes["crash-loop"].suppressed, true);
});

test("one recommendation per cooldown window across scopes", () => {
  const first = evaluateRemediation(STALE, EMPTY_REMEDIATION_STATE, NOW);
  assert.equal(first.messages.length, 1);
  const both = evaluateRemediation([...STALE, ...UNPAIRED], first.state, NOW + 60);
  assert.deepEqual(both.messages, []);
  const later = evaluateRemediation(
    [...STALE, ...UNPAIRED],
    first.state,
    NOW + REMEDIATION_COOLDOWN_SECONDS,
  );
  assert.equal(later.messages.length, 1);
  assert.equal(later.state.lastRecommendedAtSeconds, NOW + REMEDIATION_COOLDOWN_SECONDS);
});

test("scopes guard independently: a stopped stall does not silence a crash loop", () => {
  let state: RemediationState = {
    ...EMPTY_REMEDIATION_STATE,
    scopes: {
      "voice-stall": { phase: "takeover-recommended", consecutiveFailures: 2, suppressed: true },
      "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false },
    },
  };
  const { messages } = evaluateRemediation([...STALE, ...UNPAIRED], state, NOW);
  assert.equal(messages.length, 1);
  assert.equal(messages[0]!.scope, "crash-loop");
});

test("every trigger links to an existing runbook heading, and so does the response section", () => {
  const slugs = anchors("runbook.md");
  for (const trigger of REMEDIATION_TRIGGERS) {
    const anchor = trigger.runbook.split("#")[1]!;
    assert.ok(slugs.has(anchor), `${trigger.firingKey}: missing runbook heading for #${anchor}`);
    assert.ok(
      remediationRunbookUrl(trigger).startsWith(RUNBOOK_BASE_URL),
      `${trigger.firingKey}: runbook link must stay public-safe`,
    );
  }
  assert.ok(
    slugs.has("auto-remediation-guarded-stall-and-crash-loop-response"),
    "missing auto-remediation response section",
  );
});

test("recommendation lines stay public-safe", () => {
  const { messages } = evaluateRemediation(STALE, EMPTY_REMEDIATION_STATE, NOW);
  const text = messages[0]!.text;
  assert.doesNotMatch(text, /TOG-|PAP-|bearer|token|secret|webhook/i);
  assert.ok(operatorInstruction("creator-map-reload").length > 0);
  assert.ok(operatorInstruction("same-sha-takeover").length > 0);
});

test("a corrupt guard record restarts the guard instead of wedging it", () => {
  for (const corrupt of [
    undefined,
    null,
    7,
    "idle",
    {},
    { lastRecommendedAtSeconds: NOW },
    { lastRecommendedAtSeconds: "now", scopes: EMPTY_REMEDIATION_STATE.scopes },
    { lastRecommendedAtSeconds: NOW, scopes: {} },
    {
      lastRecommendedAtSeconds: NOW,
      scopes: { "voice-stall": { phase: "idle", consecutiveFailures: 0, suppressed: false } },
    },
    {
      lastRecommendedAtSeconds: NOW,
      scopes: {
        "voice-stall": { phase: "takeover-recommended", consecutiveFailures: NaN, suppressed: false },
        "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false },
      },
    },
  ]) {
    assert.deepEqual(sanitizeRemediationState(corrupt, NOW), EMPTY_REMEDIATION_STATE);
    // The restarted guard recommends on a firing trigger: remediation speaks
    // again on the very next tick instead of staying silent.
    const { messages } = evaluateRemediation(STALE, sanitizeRemediationState(corrupt, NOW), NOW);
    assert.equal(messages.length, 1);
    assert.equal(messages[0]!.action, "creator-map-reload");
  }
});

test("a non-finite or far-future guard timestamp restarts the guard", () => {
  const withTimestamp = (lastRecommendedAtSeconds: number): RemediationState => ({
    lastRecommendedAtSeconds,
    scopes: {
      "voice-stall": { phase: "idle", consecutiveFailures: 0, suppressed: false },
      "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false },
    },
  });
  for (const bad of [NaN, Infinity, -Infinity, NOW + 301, 1_700_000_000_000]) {
    assert.deepEqual(sanitizeRemediationState(withTimestamp(bad), NOW), EMPTY_REMEDIATION_STATE);
  }
  // A slightly future timestamp inside clock-skew tolerance passes untouched.
  assert.deepEqual(
    sanitizeRemediationState(withTimestamp(NOW + 60), NOW),
    withTimestamp(NOW + 60),
  );
});

test("a valid guard record passes sanitizing untouched", () => {
  const state = evaluateRemediation(STALE, EMPTY_REMEDIATION_STATE, NOW).state;
  assert.deepEqual(sanitizeRemediationState(state, NOW), state);
  assert.deepEqual(sanitizeRemediationState(EMPTY_REMEDIATION_STATE, NOW), EMPTY_REMEDIATION_STATE);
});
