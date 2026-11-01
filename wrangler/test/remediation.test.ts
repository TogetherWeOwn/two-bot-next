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
  interruptRemediationHoldDown,
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

test("recovery needs a sustained clear: one quiet tick holds the guard", () => {
  let state = EMPTY_REMEDIATION_STATE;
  state = evaluateRemediation(STALE, state, NOW).state;
  state = evaluateRemediation(STALE, state, NOW + REMEDIATION_COOLDOWN_SECONDS).state;
  const paged = evaluateRemediation(STALE, state, NOW + 2 * REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(paged.state.scopes["voice-stall"].suppressed, true);
  // A single quiet tick (e.g. the restart after a recommended takeover
  // re-baselines counters) starts the hold-down but keeps the failures.
  const quietOnce = evaluateRemediation([], paged.state, NOW + 2 * REMEDIATION_COOLDOWN_SECONDS + 60);
  assert.deepEqual(quietOnce.messages, []);
  assert.equal(quietOnce.state.scopes["voice-stall"].suppressed, true);
  assert.equal(quietOnce.state.scopes["voice-stall"].consecutiveFailures, 2);
  // A full cooldown window of silence resets to idle.
  const quietLong = evaluateRemediation(
    [],
    quietOnce.state,
    NOW + 2 * REMEDIATION_COOLDOWN_SECONDS + 60 + REMEDIATION_COOLDOWN_SECONDS,
  );
  assert.deepEqual(quietLong.messages, []);
  assert.deepEqual(quietLong.state.scopes["voice-stall"], {
    phase: "idle",
    consecutiveFailures: 0,
    suppressed: false,
    clearSinceSeconds: 0,
  });
  const refire = evaluateRemediation(STALE, quietLong.state, quietLong.state.lastRecommendedAtSeconds + REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(refire.messages[0]!.action, "creator-map-reload");
});

test("a transient resolve after a takeover does not reset the failure count", () => {
  let state = EMPTY_REMEDIATION_STATE;
  state = evaluateRemediation(STALE, state, NOW).state;
  state = evaluateRemediation(STALE, state, NOW + REMEDIATION_COOLDOWN_SECONDS).state;
  assert.equal(state.scopes["voice-stall"].consecutiveFailures, 1);
  // Takeover restarts the process: the next tick goes quiet (generation 0 /
  // re-baselined counters) even though the fault survives.
  const quiet = evaluateRemediation([], state, NOW + REMEDIATION_COOLDOWN_SECONDS + 60);
  assert.deepEqual(quiet.messages, []);
  assert.equal(quiet.state.scopes["voice-stall"].consecutiveFailures, 1);
  assert.equal(quiet.state.scopes["voice-stall"].phase, "takeover-recommended");
  // The alert refires before the hold-down elapses: this still counts toward
  // the two-failure stop instead of restarting at zero.
  const refire = evaluateRemediation(
    STALE,
    quiet.state,
    NOW + 2 * REMEDIATION_COOLDOWN_SECONDS,
  );
  assert.equal(refire.messages.length, 1);
  assert.equal(refire.messages[0]!.kind, "ceo-page");
  assert.equal(refire.state.scopes["voice-stall"].suppressed, true);
});

test("an alert that flaps between firing and resolved still reaches the CEO page", () => {
  let state = EMPTY_REMEDIATION_STATE;
  state = evaluateRemediation(STALE, state, NOW).state;
  const quiet = evaluateRemediation([], state, NOW + 60);
  assert.deepEqual(quiet.messages, []);
  const refire = evaluateRemediation(STALE, quiet.state, NOW + REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(refire.messages[0]!.action, "same-sha-takeover");
  const quietAgain = evaluateRemediation([], refire.state, NOW + REMEDIATION_COOLDOWN_SECONDS + 60);
  assert.deepEqual(quietAgain.messages, []);
  const paged = evaluateRemediation(
    STALE,
    quietAgain.state,
    NOW + 2 * REMEDIATION_COOLDOWN_SECONDS,
  );
  assert.equal(paged.messages[0]!.kind, "ceo-page");
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
      "voice-stall": {
        phase: "takeover-recommended",
        consecutiveFailures: 2,
        suppressed: true,
        clearSinceSeconds: 0,
      },
      "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
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
      scopes: {
        "voice-stall": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      },
    },
    {
      lastRecommendedAtSeconds: NOW,
      scopes: {
        "voice-stall": {
          phase: "takeover-recommended",
          consecutiveFailures: NaN,
          suppressed: false,
          clearSinceSeconds: 0,
        },
        "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      },
    },
    // Counts this code never writes restart the guard: a negative or
    // fractional count would otherwise skew the two-failure stop rule.
    ...[-100, 1.5, Number.MAX_SAFE_INTEGER + 1].map((consecutiveFailures) => ({
      lastRecommendedAtSeconds: NOW,
      scopes: {
        "voice-stall": { phase: "idle", consecutiveFailures, suppressed: false, clearSinceSeconds: 0 },
        "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      },
    })),
    // A hold-down timestamp this code never writes also restarts the guard.
    ...[-1, 1.5, NaN, Number.MAX_SAFE_INTEGER + 1].map((clearSinceSeconds) => ({
      lastRecommendedAtSeconds: NOW,
      scopes: {
        "voice-stall": {
          phase: "takeover-recommended",
          consecutiveFailures: 1,
          suppressed: false,
          clearSinceSeconds,
        },
        "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      },
    })),
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
      "voice-stall": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      "voice-live": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
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

test("the first recommendation files no host-action card: only a takeover line does", () => {
  const r1 = operatorInstruction("creator-map-reload");
  assert.match(r1, /No host action/);
  assert.doesNotMatch(r1, /File a host-action card only if this alert is still firing past the next tick/);
  assert.match(r1, /file a host-action card only from that line/);
  const { messages } = evaluateRemediation(STALE, EMPTY_REMEDIATION_STATE, NOW);
  assert.match(messages[0]!.text, /No host action/);
  assert.match(operatorInstruction("same-sha-takeover"), /file one host-action card/);
});

test("a valid guard record passes sanitizing untouched", () => {
  const state = evaluateRemediation(STALE, EMPTY_REMEDIATION_STATE, NOW).state;
  assert.deepEqual(sanitizeRemediationState(state, NOW), state);
  assert.deepEqual(sanitizeRemediationState(EMPTY_REMEDIATION_STATE, NOW), EMPTY_REMEDIATION_STATE);
});

test("a fractional production clock survives the storage round-trip and still reaches the CEO page", () => {
  // Production passes `Date.now() / 1000` (fractional). The guard must floor
  // on write so the next tick's sanitize keeps the cooldown and failure count.
  const t0 = NOW + 0.13;
  const first = evaluateRemediation(STALE, EMPTY_REMEDIATION_STATE, t0);
  assert.equal(first.messages[0]!.action, "creator-map-reload");
  assert.ok(Number.isSafeInteger(first.state.lastRecommendedAtSeconds));
  let state = sanitizeRemediationState(structuredClone(first.state), t0);
  assert.deepEqual(state, first.state);

  const t1 = t0 + REMEDIATION_COOLDOWN_SECONDS + 0.42;
  const second = evaluateRemediation(STALE, state, t1);
  assert.equal(second.messages[0]!.action, "same-sha-takeover");
  assert.equal(second.state.scopes["voice-stall"].consecutiveFailures, 1);
  state = sanitizeRemediationState(structuredClone(second.state), t1);
  assert.deepEqual(state, second.state);

  // Takeover restarts the process: the next tick goes quiet even though the
  // fault survives. The hold-down timestamp must be an integer so sanitize
  // on the following tick does not wipe the guard.
  const quietAt = t1 + 60.77;
  const quiet = evaluateRemediation([], state, quietAt);
  assert.deepEqual(quiet.messages, []);
  assert.equal(quiet.state.scopes["voice-stall"].consecutiveFailures, 1);
  assert.ok(Number.isSafeInteger(quiet.state.scopes["voice-stall"].clearSinceSeconds));
  const survived = sanitizeRemediationState(structuredClone(quiet.state), quietAt + 60.11);
  assert.deepEqual(survived, quiet.state);
  assert.notEqual(survived.lastRecommendedAtSeconds, 0);

  // Refire inside the hold-down keeps the failure count and reaches the stop.
  const refire = evaluateRemediation(STALE, survived, t0 + 2 * REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(refire.messages.length, 1);
  assert.equal(refire.messages[0]!.kind, "ceo-page");
  assert.equal(refire.state.scopes["voice-stall"].suppressed, true);
});

// The deaf voice actor (`two_bot_voice_live_not_ready_seconds` past its bound)
// pages but no remediation existed for it, so the signal the host voice-stall
// timer used to act on had no in-contract recovery. It gets its own scope:
// `voice_live_not_ready` and `voice_creator_map_stale` are different faults,
// and sharing one guard would let either burn the other's cooldown window and
// failure count. Its alert bound is already the in-process repair window — the
// bot's dispatch republishes a not-ready view from the gateway cache every
// voice frame while the age climbs — so the first recommendation is the
// takeover itself, not a text-only step that would spend another window.
const DEAF = ["voice_live_not_ready"];

test("a deaf voice actor owns its own scope, so the creator-map trigger cannot consume its window", () => {
  const { messages, state } = evaluateRemediation(DEAF, EMPTY_REMEDIATION_STATE, NOW);
  assert.equal(messages.length, 1);
  assert.equal(messages[0]!.kind, "recommend");
  assert.equal(messages[0]!.scope, "voice-live");
  // The expired self-heal window counts as attempt one: the first
  // recommendation is already the one-card same-SHA takeover.
  assert.equal(messages[0]!.action, "same-sha-takeover");
  assert.match(messages[0]!.text, /REMEDIATION voice-live: same-sha-takeover/);
  assert.match(messages[0]!.text, /Operator: file one host-action card/);
  // The sibling scope stays untouched: a deaf actor must not spend or count
  // against the creator-map guard, and vice versa.
  assert.deepEqual(state.scopes["voice-stall"], EMPTY_REMEDIATION_STATE.scopes["voice-stall"]);
  assert.equal(state.scopes["voice-live"].phase, "takeover-recommended");
  assert.equal(state.scopes["voice-live"].consecutiveFailures, 0);
});

test("the deaf scope repeats the takeover line once, then stops with the CEO page", () => {
  const first = evaluateRemediation(DEAF, EMPTY_REMEDIATION_STATE, NOW);
  assert.equal(first.messages[0]!.action, "same-sha-takeover");
  const retry = evaluateRemediation(DEAF, first.state, NOW + REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(retry.messages.length, 1);
  assert.equal(retry.messages[0]!.action, "same-sha-takeover");
  assert.match(retry.messages[0]!.text, /Operator: file one host-action card/);
  assert.equal(retry.state.scopes["voice-live"].consecutiveFailures, 1);
  const paged = evaluateRemediation(DEAF, retry.state, NOW + 2 * REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(paged.messages[0]!.kind, "ceo-page");
  assert.equal(paged.state.scopes["voice-live"].suppressed, true);
});

test("both voice scopes firing at once each keep their own guard and the cooldown is global", () => {
  const first = evaluateRemediation([...STALE, ...DEAF], EMPTY_REMEDIATION_STATE, NOW);
  // Table order decides who spends the global window: the deaf scope's
  // takeover goes first and the stall scope's no-host-action line waits.
  assert.equal(first.messages.length, 1);
  assert.equal(first.messages[0]!.scope, "voice-live");
  assert.equal(first.messages[0]!.action, "same-sha-takeover");
  assert.deepEqual(first.state.scopes["voice-stall"], EMPTY_REMEDIATION_STATE.scopes["voice-stall"]);
  // Clearing the deaf alert hands the next window to the stall scope, which
  // still has its own idle guard rather than the deaf scope's spend.
  const next = evaluateRemediation(STALE, first.state, NOW + REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(next.messages.length, 1);
  assert.equal(next.messages[0]!.scope, "voice-stall");
  assert.equal(next.messages[0]!.action, "creator-map-reload");
  assert.equal(next.state.scopes["voice-stall"].consecutiveFailures, 0);
  assert.equal(next.state.scopes["voice-live"].consecutiveFailures, 0);
  // Every window still holds exactly one recommendation across the table.
  const again = evaluateRemediation([...STALE, ...DEAF], next.state, NOW + 2 * REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(again.messages.length, 1);
});

test("a deaf takeover is never queued behind a no-host-action line", () => {
  // Every trigger whose first step is a takeover sorts ahead of the
  // in-process-first stall scope, so a concurrent stall never delays one.
  const order = REMEDIATION_TRIGGERS.map((trigger) => trigger.scope);
  assert.deepEqual(order, ["voice-live", "voice-stall", "crash-loop"]);
  const { messages } = evaluateRemediation([...STALE, ...DEAF, ...UNPAIRED], EMPTY_REMEDIATION_STATE, NOW);
  assert.equal(messages.length, 1);
  assert.equal(messages[0]!.action, "same-sha-takeover");
});

test("a guard persisted before the deaf scope existed still sanitizes, and the new scope starts idle", () => {
  // Older production DO storage carries only the two original scopes. Adding
  // a scope must not wipe the persisted guard: the stored guard survives and
  // only the missing one is filled with an idle default.
  const legacy = structuredClone(EMPTY_REMEDIATION_STATE) as RemediationState & {
    scopes: Record<string, unknown>;
  };
  const staleGuard = legacy.scopes["voice-stall"]!;
  (staleGuard as { phase: string }).phase = "reload-recommended";
  delete (legacy.scopes as Record<string, unknown>)["voice-live"];
  const sanitized = sanitizeRemediationState(legacy, NOW);
  assert.deepEqual(sanitized.scopes["voice-stall"], staleGuard);
  assert.deepEqual(sanitized.scopes["voice-live"], EMPTY_REMEDIATION_STATE.scopes["voice-live"]);
  assert.deepEqual(sanitized.scopes["crash-loop"], EMPTY_REMEDIATION_STATE.scopes["crash-loop"]);
});

// A `voice_live_not_ready` key in `firing` can be *retained* rather than
// measured: when the age gauge is missing or malformed in a scrape (older
// containers never emitted it), the alert holds its previous firing state.
// That retained alert must not advance the live scope — no new recommendation,
// no failure increment, no hold-down reset — while the held alert itself is
// untouched. The freeze is pinned below so a refire-by-default never spends a
// takeover card on evidence nobody saw.
test("a retained deaf alert without a current age sample freezes the scope instead of escalating it", () => {
  const first = evaluateRemediation(DEAF, EMPTY_REMEDIATION_STATE, NOW);
  assert.equal(first.messages[0]!.action, "same-sha-takeover");
  // The next scrape is missing the gauge: the key stays in `firing` (the
  // alert is still held), but the guard must not move — no retry line, no
  // failure count, and the global timestamp is not re-spent.
  const frozen = evaluateRemediation(DEAF, first.state, NOW + REMEDIATION_COOLDOWN_SECONDS, DEAF);
  assert.deepEqual(frozen.messages, []);
  assert.deepEqual(frozen.state, first.state);
  // Freeze is not recovery either: a third frozen tick still cannot page.
  const stillFrozen = evaluateRemediation(
    DEAF,
    frozen.state,
    NOW + 2 * REMEDIATION_COOLDOWN_SECONDS,
    DEAF,
  );
  assert.deepEqual(stillFrozen.messages, []);
  assert.deepEqual(stillFrozen.state, first.state);
  // When a current sample returns and the alert is still firing, progression
  // resumes from the untouched guard — the second failure stop still applies.
  const resumed = evaluateRemediation(DEAF, stillFrozen.state, NOW + 3 * REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(resumed.messages.length, 1);
  assert.equal(resumed.messages[0]!.action, "same-sha-takeover");
  assert.equal(resumed.state.scopes["voice-live"].consecutiveFailures, 1);
});

test("a frozen deaf scope neither consumes the global window nor blocks a sibling's", () => {
  const first = evaluateRemediation(DEAF, EMPTY_REMEDIATION_STATE, NOW);
  // Both keys fire; only the deaf one is retained. The stall scope has fresh
  // evidence, so it takes the window while the frozen scope sits.
  const both = evaluateRemediation(
    [...STALE, ...DEAF],
    first.state,
    NOW + REMEDIATION_COOLDOWN_SECONDS,
    DEAF,
  );
  assert.equal(both.messages.length, 1);
  assert.equal(both.messages[0]!.scope, "voice-stall");
  assert.equal(both.messages[0]!.action, "creator-map-reload");
  assert.deepEqual(both.state.scopes["voice-live"], first.state.scopes["voice-live"]);
});

test("a retained key for a scope that never recommended leaves the guard fully idle", () => {
  const { messages, state } = evaluateRemediation(DEAF, EMPTY_REMEDIATION_STATE, NOW, DEAF);
  assert.deepEqual(messages, []);
  assert.deepEqual(state, EMPTY_REMEDIATION_STATE);
});

// The live scope's persisted guard must round-trip exactly like the older
// two: an in-flight or stopped live guard survives `storage.sync` + read-back
// untouched, while a malformed live guard is repaired to idle without wiping
// its valid siblings' cooldown and failure counts.
test("an active live-scope guard survives the storage round-trip untouched", () => {
  const state = evaluateRemediation(DEAF, EMPTY_REMEDIATION_STATE, NOW).state;
  assert.equal(state.scopes["voice-live"].phase, "takeover-recommended");
  const persisted = sanitizeRemediationState(structuredClone(state), NOW);
  assert.deepEqual(persisted, state);
});

test("a suppressed live-scope guard survives the storage round-trip untouched", () => {
  const suppressed: RemediationState = {
    lastRecommendedAtSeconds: NOW - REMEDIATION_COOLDOWN_SECONDS,
    scopes: {
      "voice-stall": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      "voice-live": {
        phase: "takeover-recommended",
        consecutiveFailures: REMEDIATION_MAX_CONSECUTIVE_FAILURES,
        suppressed: true,
        clearSinceSeconds: 0,
      },
      "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
    },
  };
  const persisted = sanitizeRemediationState(structuredClone(suppressed), NOW);
  assert.deepEqual(persisted, suppressed);
});

test("a malformed live-scope guard restarts that scope only and preserves valid siblings", () => {
  const stallGuard = {
    phase: "takeover-recommended" as const,
    consecutiveFailures: 1,
    suppressed: false,
    clearSinceSeconds: NOW - 60,
  };
  const stored = {
    lastRecommendedAtSeconds: NOW - REMEDIATION_COOLDOWN_SECONDS,
    scopes: {
      "voice-stall": stallGuard,
      // Shapes this code never writes: NaN counts and unknown phases must not
      // spread, and must not wipe the live cooldown or the sibling failure.
      "voice-live": { phase: "bogus", consecutiveFailures: NaN, suppressed: "yes", clearSinceSeconds: -3 },
      "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
    },
  };
  const sanitized = sanitizeRemediationState(stored, NOW);
  assert.notDeepEqual(sanitized, EMPTY_REMEDIATION_STATE);
  assert.deepEqual(sanitized.lastRecommendedAtSeconds, stored.lastRecommendedAtSeconds);
  assert.deepEqual(sanitized.scopes["voice-stall"], stallGuard);
  assert.deepEqual(sanitized.scopes["crash-loop"], stored.scopes["crash-loop"]);
  assert.deepEqual(sanitized.scopes["voice-live"], EMPTY_REMEDIATION_STATE.scopes["voice-live"]);
});

// Unobserved wall time is never recovery. A quiet tick with no current age
// sample is not an observed clear, so it must not run down the hold-down: a
// stopped live scope would otherwise reset to idle across an observation gap
// and authorize a fresh takeover with zero failures.
test("an observation gap during the clear hold-down keeps the live stop latched", () => {
  const stopped: RemediationState = {
    lastRecommendedAtSeconds: NOW,
    scopes: {
      "voice-stall": { ...EMPTY_REMEDIATION_STATE.scopes["voice-stall"] },
      "voice-live": {
        phase: "takeover-recommended",
        consecutiveFailures: REMEDIATION_MAX_CONSECUTIVE_FAILURES,
        suppressed: true,
        clearSinceSeconds: 0,
      },
      "crash-loop": { ...EMPTY_REMEDIATION_STATE.scopes["crash-loop"] },
    },
  };
  // One current age-0 sample starts the clear clock.
  const clear = evaluateRemediation([], stopped, NOW + 60);
  assert.equal(clear.state.scopes["voice-live"].clearSinceSeconds, NOW + 60);
  // Then a full window of scrapes with no age sample: the stop stays latched
  // and the hold-down restarts instead of completing.
  const gap = evaluateRemediation([], clear.state, NOW + 60 + REMEDIATION_COOLDOWN_SECONDS, DEAF);
  assert.deepEqual(gap.messages, []);
  assert.deepEqual(gap.state.scopes["voice-live"], { ...stopped.scopes["voice-live"], clearSinceSeconds: 0 });
  // A fresh firing sample after the gap is still suppressed: no new takeover.
  const refire = evaluateRemediation(DEAF, gap.state, NOW + 120 + REMEDIATION_COOLDOWN_SECONDS);
  assert.deepEqual(refire.messages, []);
  assert.equal(refire.state.scopes["voice-live"].suppressed, true);
  assert.equal(refire.state.scopes["voice-live"].consecutiveFailures, REMEDIATION_MAX_CONSECUTIVE_FAILURES);
});

test("after a gap, recovery needs a full window of observed clear from the next sample", () => {
  const holding: RemediationState = {
    lastRecommendedAtSeconds: NOW,
    scopes: {
      "voice-stall": { ...EMPTY_REMEDIATION_STATE.scopes["voice-stall"] },
      "voice-live": { phase: "takeover-recommended", consecutiveFailures: 1, suppressed: false, clearSinceSeconds: NOW + 60 },
      "crash-loop": { ...EMPTY_REMEDIATION_STATE.scopes["crash-loop"] },
    },
  };
  const gap = evaluateRemediation([], holding, NOW + 600, DEAF);
  assert.equal(gap.state.scopes["voice-live"].clearSinceSeconds, 0);
  const observed = NOW + 900;
  const restarted = evaluateRemediation([], gap.state, observed);
  assert.equal(restarted.state.scopes["voice-live"].clearSinceSeconds, observed);
  // The original clock would have completed here; the restarted one has not.
  const early = evaluateRemediation([], restarted.state, NOW + 60 + REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(early.state.scopes["voice-live"].consecutiveFailures, 1);
  const done = evaluateRemediation([], early.state, observed + REMEDIATION_COOLDOWN_SECONDS);
  assert.deepEqual(done.state.scopes["voice-live"], EMPTY_REMEDIATION_STATE.scopes["voice-live"]);
});

test("an unobserved quiet key leaves an idle scope idle", () => {
  const { messages, state } = evaluateRemediation([], EMPTY_REMEDIATION_STATE, NOW, DEAF);
  assert.deepEqual(messages, []);
  assert.deepEqual(state, EMPTY_REMEDIATION_STATE);
});

test("a failed scrape restarts every hold-down and latches everything else", () => {
  const holding: RemediationState = {
    lastRecommendedAtSeconds: NOW,
    scopes: {
      "voice-stall": { phase: "reload-recommended", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: NOW + 60 },
      "voice-live": { phase: "takeover-recommended", consecutiveFailures: 2, suppressed: true, clearSinceSeconds: NOW + 60 },
      "crash-loop": { ...EMPTY_REMEDIATION_STATE.scopes["crash-loop"] },
    },
  };
  const interrupted = interruptRemediationHoldDown(structuredClone(holding));
  assert.equal(interrupted.lastRecommendedAtSeconds, NOW);
  assert.deepEqual(interrupted.scopes["voice-stall"], { ...holding.scopes["voice-stall"], clearSinceSeconds: 0 });
  assert.deepEqual(interrupted.scopes["voice-live"], { ...holding.scopes["voice-live"], clearSinceSeconds: 0 });
  assert.deepEqual(interrupted.scopes["crash-loop"], holding.scopes["crash-loop"]);
  // Nothing holding down: same reference back, so the caller skips the write.
  assert.equal(interruptRemediationHoldDown(EMPTY_REMEDIATION_STATE), EMPTY_REMEDIATION_STATE);
});
