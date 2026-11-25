import assert from "node:assert/strict";
import { test } from "node:test";
import { runProtectedStagingVoiceMove } from "../scripts/staging-voice-move-caller.mjs";
import { bindStagingVoiceSession } from "../scripts/staging-voice-session.mjs";
import { mockVoiceMoveWindow } from "./fixtures/staging-voice-move.ts";

function unexpectedCall(): never {
  assert.fail("the caller must only forward the protected inputs");
}

function protectedInputs() {
  const { args } = mockVoiceMoveWindow();
  const { runId, runAttempt, expiresAt, sha, deploymentId } = args.lease;
  return {
    enabled: true,
    window: { runId, runAttempt, expiresAt, sha, deploymentId },
    ready: args.ready,
    previousReady: args.evidence.previousReady,
    previousOff: args.evidence.previousOff,
    fixture: args.fixture,
    webSession: {
      environment: "staging", origin: "https://next.togetherweown.com", discordId: args.fixture.discordId,
      request: unexpectedCall, move: unexpectedCall,
    },
    expectedRevision: { version_id: "11111111-2222-3333-4444-555555555555", commit: "b".repeat(40) },
    observer: { readChannels: unexpectedCall, observe: unexpectedCall },
    control: unexpectedCall,
    now: args.now,
  };
}

test("protected caller forwards one pinned invocation and returns the proof receipt", async () => {
  const options = protectedInputs();
  const receipt = { verdict: "passed", mode: "mock" };
  const calls = [];
  const result = await runProtectedStagingVoiceMove({ ...options, unrelated: true,
    fixture: { ...options.fixture, unrelated: true }, expectedRevision: { ...options.expectedRevision, unrelated: true },
  }, async (request) => {
    calls.push(request);
    return receipt;
  });
  const { window, ...inputs } = options;
  assert.deepEqual(calls, [{ ...inputs, lease: { ...window, epoch: options.ready.epoch }, bindSession: bindStagingVoiceSession }]);
  assert.equal(calls[0].ready, options.ready);
  assert.equal(calls[0].previousReady, options.previousReady);
  assert.equal(calls[0].previousOff, options.previousOff);
  assert.equal(result, receipt);
});

test("protected input setup refuses missing adapters or mismatched pins before proof", async () => {
  const options = protectedInputs();
  const { expectedRevision, previousOff, previousReady, ready, webSession } = options;
  const cases = [
    [{ webSession: { ...webSession, origin: "https://example.invalid" } }, "proof_inputs_incomplete"],
    [{ webSession: { ...webSession, request: undefined } }, "proof_inputs_incomplete"],
    [{ webSession: { ...webSession, move: undefined } }, "proof_inputs_incomplete"],
    [{ expectedRevision: { ...expectedRevision, version_id: "unattributed" } }, "proof_inputs_incomplete"],
    [{ expectedRevision: { ...expectedRevision, commit: "unattributed" } }, "proof_inputs_incomplete"],
    [{ observer: { readChannels: unexpectedCall } }, "proof_inputs_incomplete"],
    [{ observer: { observe: unexpectedCall } }, "proof_inputs_incomplete"],
    [{ control: undefined }, "proof_inputs_incomplete"],
    [{ previousReady: undefined }, "proof_inputs_incomplete"],
    [{ previousOff: { ...previousOff, running: true } }, "prior_cleanup_unconfirmed"],
    [{ previousOff: { ...previousOff, voiceMoveEnabled: true } }, "prior_cleanup_unconfirmed"],
    [{ previousOff: { ...previousOff, use: { ...previousOff.use, runId: ready.use.runId } } }, "prior_cleanup_unconfirmed"],
    [{ previousReady: { ...previousReady, use: { ...previousReady.use, sha: "c".repeat(40) } } }, "prior_cleanup_unconfirmed"],
  ];
  for (const [change, code] of cases) {
    assert.deepEqual(await runProtectedStagingVoiceMove({ ...options, ...change }, unexpectedCall),
      { verdict: "failed", code });
  }
});

test("protected caller returns before invoking the proof when protected inputs are unconfirmed", async () => {
  const options = protectedInputs();
  const { fixture, ready, webSession, window, now } = options;
  const lastSecond = window.expiresAt - 1000;
  assert.deepEqual(await runProtectedStagingVoiceMove(), { verdict: "disabled" });
  assert.deepEqual(await runProtectedStagingVoiceMove({ ...options, enabled: "true" }, unexpectedCall),
    { verdict: "disabled" });

  const cases = [
    [{ fixture: { ...fixture, channelIds: [fixture.channelIds[0]] } }, "fixture_unconfirmed"],
    [{ fixture: { ...fixture, channelIds: [fixture.channelIds[0], fixture.channelIds[0]] } }, "fixture_unconfirmed"],
    [{ webSession: { ...webSession, discordId: fixture.channelIds[0] } }, "fixture_unconfirmed"],
    [{ expectedRevision: undefined }, "proof_inputs_incomplete"],
    [{ previousOff: undefined }, "proof_inputs_incomplete"],
    [{ window: undefined }, "lease_unconfirmed"],
    [{ window: { ...window, runId: "67890" } }, "lease_unconfirmed"],
    [{ window: { ...window, runAttempt: 2 } }, "lease_unconfirmed"],
    [{ window: { ...window, expiresAt: window.expiresAt + 1 } }, "lease_unconfirmed"],
    [{ window: { ...window, sha: "b".repeat(40) } }, "lease_unconfirmed"],
    [{ window: { ...window, deploymentId: "other-deployment" } }, "lease_unconfirmed"],
    [{ ready: { ...ready, use: { ...ready.use, epoch: ready.epoch + 1 } } }, "lease_unconfirmed"],
    [{ now: () => now() + 60_001 }, "lease_unconfirmed"],
    [{ ready: { ...ready, observedAt: new Date(lastSecond - 1).toISOString() }, now: () => lastSecond }, "lease_unconfirmed"],
  ];
  for (const [change, code] of cases) {
    assert.deepEqual(await runProtectedStagingVoiceMove({ ...options, ...change }, unexpectedCall),
      { verdict: "failed", code });
  }
});
