/**
 * Staging-only persisted remediation guard receipts (TOG-30294 slice).
 *
 * Failing-first coverage: the receipt must reflect the actual stored guard
 * (read-back, not in-memory), never claim success on storage failure, carry
 * exact evaluation correlation, stay public-safe, and stay dark in production.
 * One receipt at most per keepalive tick, staging only.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  EMPTY_REMEDIATION_STATE,
  REMEDIATION_COOLDOWN_SECONDS,
  buildRemediationGuardReceipt,
  remediationReceiptDecision,
  remediationTriggerFiring,
  type RemediationState,
} from "../src/remediation.ts";

const NOW = 2_000_000;
const STALE = ["voice_creator_map_stale"];
const UNPAIRED = ["gateway_unpaired_disconnect"];

const CORRELATION = {
  buildRevision: "abc1234",
  release: "abc1234",
  workerVersion: "deployment-A",
};

function receiptInput(overrides: Partial<{
  persisted: unknown;
  expected: RemediationState;
  firing: string[];
  nowSeconds: number;
}> = {}) {
  const expected: RemediationState = overrides.expected ?? EMPTY_REMEDIATION_STATE;
  return {
    persisted: "persisted" in overrides ? overrides.persisted : expected,
    expected,
    firing: overrides.firing ?? [],
    messages: [] as { kind: "recommend" | "ceo-page"; scope: "voice-stall" | "crash-loop"; action: "creator-map-reload" | "same-sha-takeover" | undefined; firingKey: string; text: string }[],
    nowSeconds: overrides.nowSeconds ?? NOW,
    ...CORRELATION,
  };
}

test("trigger firing allowlist keeps only remediation keys", () => {
  assert.deepEqual(remediationTriggerFiring([...STALE, ...UNPAIRED]), [...STALE, ...UNPAIRED]);
  assert.deepEqual(
    remediationTriggerFiring([...STALE, "voice_join_fast_burn", "job_stale:rank"]),
    STALE,
  );
  assert.deepEqual(remediationTriggerFiring([]), []);
  assert.deepEqual(remediationTriggerFiring(["voice_creator_map_stale:extra"]), []);
});

test("idle guard with no firing is the pre-drill baseline decision", () => {
  const { decision } = remediationReceiptDecision([], [], EMPTY_REMEDIATION_STATE, NOW);
  assert.equal(decision, "idle");
});

test("recommend R1 carries its action for correlation", () => {
  const { decision, action } = remediationReceiptDecision(
    [{ kind: "recommend", scope: "voice-stall", action: "creator-map-reload", firingKey: STALE[0]!, text: "x" }],
    STALE,
    EMPTY_REMEDIATION_STATE,
    NOW,
  );
  assert.equal(decision, "recommend");
  assert.equal(action, "creator-map-reload");
});

test("suppressed firing without a message is quiet, not recovery", () => {
  const suppressed: RemediationState = {
    lastRecommendedAtSeconds: NOW,
    scopes: {
      "voice-stall": { phase: "takeover-recommended", consecutiveFailures: 2, suppressed: true, clearSinceSeconds: 0 },
      "voice-live": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
    },
  };
  const { decision } = remediationReceiptDecision([], STALE, suppressed, NOW + REMEDIATION_COOLDOWN_SECONDS);
  assert.equal(decision, "suppressed-quiet");
});

test("firing inside the global cooldown without a message is held", () => {
  const afterR1: RemediationState = {
    lastRecommendedAtSeconds: NOW,
    scopes: {
      "voice-stall": { phase: "reload-recommended", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      "voice-live": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
    },
  };
  const { decision } = remediationReceiptDecision([], STALE, afterR1, NOW + 60);
  assert.equal(decision, "held-cooldown");
});

test("quiet tick on a non-idle guard is the sustained-clear hold-down", () => {
  const holding: RemediationState = {
    lastRecommendedAtSeconds: NOW,
    scopes: {
      "voice-stall": { phase: "takeover-recommended", consecutiveFailures: 1, suppressed: false, clearSinceSeconds: NOW + 60 },
      "voice-live": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
    },
  };
  const { decision } = remediationReceiptDecision([], [], holding, NOW + 120);
  assert.equal(decision, "clear-hold");
});

test("a hold-down on the deaf scope alone is a clear-hold, not an idle reset", () => {
  // Every scope must be checked: a guard where only the newest scope holds a
  // hold-down would otherwise be reported `idle`, claiming a reset that did
  // not happen.
  const liveHold: RemediationState = {
    lastRecommendedAtSeconds: NOW,
    scopes: {
      "voice-stall": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      "voice-live": { phase: "takeover-recommended", consecutiveFailures: 1, suppressed: false, clearSinceSeconds: NOW + 60 },
      "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
    },
  };
  assert.equal(remediationReceiptDecision([], [], liveHold, NOW + 120).decision, "clear-hold");
  // Fully reset still reports idle.
  const allIdle = structuredClone(liveHold) as RemediationState & { scopes: Record<string, unknown> };
  (allIdle.scopes as Record<string, unknown>)["voice-live"] = { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 };
  assert.equal(remediationReceiptDecision([], [], allIdle, NOW + 120).decision, "idle");
});

test("success receipt reflects the persisted read-back with exact correlation", () => {
  const expected: RemediationState = {
    lastRecommendedAtSeconds: NOW,
    scopes: {
      "voice-stall": { phase: "reload-recommended", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      "voice-live": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
    },
  };
  const receipt = buildRemediationGuardReceipt({
    ...receiptInput({ persisted: structuredClone(expected), expected, firing: STALE }),
    messages: [{ kind: "recommend", scope: "voice-stall", action: "creator-map-reload", firingKey: STALE[0]!, text: "x" }],
  });
  assert.equal(receipt.ok, true);
  assert.equal(receipt.deploy_env, "staging");
  assert.equal(receipt.evaluated_at_seconds, NOW);
  assert.deepEqual(receipt.firing, STALE);
  assert.equal(receipt.decision, "recommend");
  assert.deepEqual(receipt.guard, expected);
  assert.equal(receipt.build_revision, CORRELATION.buildRevision);
  assert.equal(receipt.release, CORRELATION.release);
  assert.equal(receipt.worker_version, CORRELATION.workerVersion);
});

test("mismatched read-back is a failure receipt, never a false success", () => {
  const expected: RemediationState = {
    lastRecommendedAtSeconds: NOW,
    scopes: {
      "voice-stall": { phase: "reload-recommended", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      "voice-live": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
      "crash-loop": { phase: "idle", consecutiveFailures: 0, suppressed: false, clearSinceSeconds: 0 },
    },
  };
  const receipt = buildRemediationGuardReceipt({
    ...receiptInput({ persisted: structuredClone(EMPTY_REMEDIATION_STATE), expected, firing: STALE }),
    messages: [],
  });
  assert.equal(receipt.ok, false);
  if (!receipt.ok) assert.equal(receipt.reason, "mismatch");
});

test("corrupt persisted guard is invalid, never a success receipt", () => {
  const expected = EMPTY_REMEDIATION_STATE;
  for (const persisted of [
    null,
    { lastRecommendedAtSeconds: NaN, scopes: expected.scopes },
    { lastRecommendedAtSeconds: NOW, scopes: { "voice-stall": { phase: "idle", consecutiveFailures: -1, suppressed: false, clearSinceSeconds: 0 } } },
  ]) {
    const receipt = buildRemediationGuardReceipt({
      ...receiptInput({ persisted, expected, firing: [] }),
      messages: [],
    });
    assert.equal(receipt.ok, false, JSON.stringify(persisted));
  }
});

test("receipts stay public-safe", () => {
  const receipt = buildRemediationGuardReceipt({
    ...receiptInput({ firing: [...STALE, ...UNPAIRED] }),
    messages: [],
  });
  const text = JSON.stringify(receipt);
  assert.doesNotMatch(text, /TOG-|PAP-|bearer|token|secret|webhook|discord\.com\/api\/webhooks/i);
  assert.ok(!("webhook" in receipt) && !("token" in receipt));
});
