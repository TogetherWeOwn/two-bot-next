import assert from "node:assert/strict";
import { test } from "node:test";
import { runStagingVoiceMoveProof } from "../scripts/staging-voice-move-acceptance.mjs";
import { bindStagingVoiceSession } from "../scripts/staging-voice-session.mjs";
import { mockVoiceMoveWindow } from "./fixtures/staging-voice-move.ts";
import { mockVoiceObservation } from "./fixtures/staging-voice-observer.ts";

const origin = "https://next.togetherweown.com";
const receiptUrl = `${origin}/__probe/voice-move/session`;
const revision = { version_id: "12345678-1234-4321-8123-123456789abc", commit: "b".repeat(40) };

function proofWindow() {
  const { args, signed, moves, calls } = mockVoiceMoveWindow();
  const { fixture, evidence, caller, ready } = args;
  let now = args.now();
  const webReads: string[] = [];
  const discordReads: string[] = [];
  const releases = [];
  const statuses = [];
  const webReceipt = { matched: true, revision: { ...revision }, observed_at: new Date(now).toISOString() };
  const webSession = {
    environment: caller.environment,
    discordId: caller.discordId,
    origin,
    async request(url: string, init: RequestInit) {
      assert.equal(this, webSession);
      assert.equal(url, receiptUrl);
      assert.equal(init.method, "POST");
      assert.equal(new Headers(init.headers).get("origin"), origin);
      assert.equal(init.redirect, "error");
      init.signal?.throwIfAborted();
      assert.deepEqual(JSON.parse(String(init.body)), { discord_id: fixture.discordId, channel_ids: fixture.channelIds });
      webReads.push("session");
      return Object.defineProperty(new Response(JSON.stringify(webReceipt), {
        headers: { "content-type": "application/json", "cache-control": "private, no-store" },
      }), "url", { value: url });
    },
    async move(request: Parameters<typeof caller.move>[0]) {
      assert.equal(this, webSession);
      assert.deepEqual(Object.keys(request).sort(), ["channelId", "signal"]);
      assert.deepEqual(webReads, ["session"]);
      return caller.move(request);
    },
  };
  const project = (checkpoint: string, channelId: string, reading: typeof evidence.readback.source) => {
    discordReads.push(checkpoint);
    return mockVoiceObservation({
      fixture, channelId, reading, now, notBefore: ready.use.acquiredAt, readableChannelIds: fixture.channelIds,
    });
  };
  const observer = {
    async readChannels({ signal }: { signal: AbortSignal }) {
      signal.throwIfAborted();
      assert.equal(this, observer);
      const [source, destination] = fixture.channelIds;
      return {
        source: project("source", source, evidence.readback.source),
        destination: project("destination", destination, evidence.readback.destination),
      };
    },
    async observe(request: Parameters<typeof args.observe>[0]) {
      assert.equal(this, observer);
      const reading = await args.observe(request);
      return project(request.checkpoint, request.channelId, {
        ...reading, channelType: 2, observedAt: new Date(++now).toISOString(),
      });
    },
  };
  const input = {
    enabled: true, lease: args.lease, ready, fixture, previousReady: evidence.previousReady, previousOff: evidence.previousOff,
    bindSession: bindStagingVoiceSession, webSession, expectedRevision: revision, observer, now: () => now,
    async control(request) {
      const state = await args.control(request);
      if (request.action === "release") releases.push(request);
      if (request.action === "status") statuses.push(state);
      return state;
    },
  };
  return { input, webReceipt, webReads, discordReads, signed, moves, calls, releases, statuses };
}

function assertExactFinish({ input, releases, statuses }: ReturnType<typeof proofWindow>) {
  const { runId, runAttempt, expiresAt, sha, deploymentId } = input.lease;
  assert.deepEqual(releases.map((release) => ({
    action: release.action, runId: release.runId, runAttempt: release.runAttempt,
    expiresAt: release.expiresAt, sha: release.sha, deploymentId: release.deploymentId,
  })), [{ action: "release", runId, runAttempt, expiresAt, sha, deploymentId }]);
  assert.equal(statuses.length, 2);
  const [before, after] = statuses;
  assert.deepEqual(before.use, input.ready.use);
  assert.deepEqual(after.use, { ...input.ready.use, phase: "released" });
  assert.equal(after.voiceMoveEnabled, false);
  assert.equal(after.running, false);
}

test("protected proof defaults off before touching adapters or control", async () => {
  const { input, calls, webReads, discordReads, signed } = proofWindow();
  const result = await runStagingVoiceMoveProof({
    ...input, enabled: undefined, bindSession: () => assert.fail("bound the Web session while disabled"),
  });
  assert.equal(result.verdict, "disabled");
  assert.deepEqual({ calls, webReads, discordReads, signed }, { calls: [], webReads: [], discordReads: [], signed: [] });
});

test("protected proof joins the scoped Web reader and independent Discord checkpoints", async () => {
  const window = proofWindow();
  const { input, webReads, discordReads, signed, moves, calls } = window;
  const receipt = await runStagingVoiceMoveProof(input);
  const [source, destination] = input.fixture.channelIds;
  assert.equal(receipt.verdict, "passed");
  assert.equal(receipt.mode, "mock");
  assert.deepEqual(receipt.lease, input.lease);
  assert.deepEqual(receipt.cases.map(({ outcome }) => outcome), ["moved", "not_permitted", "not_connected"]);
  assert.deepEqual(webReads, ["session"]);
  assert.deepEqual(discordReads, ["source", "destination", "before:moved", "after:moved",
    "before:not_permitted", "after:not_permitted", "before:not_connected", "after:not_connected"]);
  assert.deepEqual(signed.map(({ channel_id }) => channel_id), [destination, source, destination]);
  assert.deepEqual(moves, [{ discordId: input.fixture.discordId, channelId: destination }]);
  assert.deepEqual(calls, ["assert", "assert", "assert", "status", "release", "status"]);
  assert.deepEqual(receipt.cleanup, { memberDisconnected: true, off: true, stopped: true });
  assertExactFinish(window);
});

test("protected proof refuses wrong or stale preflight evidence and finishes its exact window", async () => {
  for (const fault of ["Web identity", "Web revision", "stale Web receipt", "Discord identity", "stale Discord receipt"]) {
    const window = proofWindow();
    const { input, webReceipt, webReads, discordReads, calls, signed, moves } = window;
    const readChannels = input.observer.readChannels.bind(input.observer);
    const stale = new Date(input.ready.use.acquiredAt - 1).toISOString();
    if (fault === "Web identity") input.webSession.discordId = input.fixture.channelIds[0];
    if (fault === "Web revision") webReceipt.revision.commit = "c".repeat(40);
    if (fault === "stale Web receipt") webReceipt.observed_at = stale;
    if (fault === "Discord identity" || fault === "stale Discord receipt") {
      input.observer.readChannels = async (request) => {
        const readback = await readChannels(request);
        if (fault === "Discord identity") readback.source.discordId = input.fixture.channelIds[0];
        else readback.destination.observedAt = stale;
        return readback;
      };
    }
    const receipt = await runStagingVoiceMoveProof(input);
    assert.equal(receipt.verdict, "failed", fault);
    assert.equal(receipt.code, {
      "Web identity": "probe_failed", "Web revision": "probe_failed", "stale Web receipt": "session_unconfirmed",
      "Discord identity": "discord_unconfirmed", "stale Discord receipt": "discord_unconfirmed",
    }[fault], fault);
    assert.deepEqual(webReads, fault === "Web identity" ? [] : ["session"], fault);
    assert.deepEqual(discordReads, ["Web identity", "Web revision"].includes(fault) ? [] : ["source", "destination"], fault);
    assert.deepEqual({ signed, moves, cases: receipt.cases }, { signed: [], moves: [], cases: [] }, fault);
    assert.deepEqual(calls, ["status", "release", "status"], fault);
    assert.deepEqual(receipt.cleanup, { memberDisconnected: null, off: true, stopped: true }, fault);
    assertExactFinish(window);
  }
});

test("protected proof refuses a stale or wrong-room-type checkpoint before a move", async () => {
  for (const fault of ["stale time", "room type"]) {
    const window = proofWindow();
    const { input, calls, signed } = window;
    const observe = input.observer.observe.bind(input.observer);
    input.observer.observe = async (request) => {
      const state = await observe(request);
      return fault === "stale time"
        ? { ...state, observedAt: new Date(input.now() - 2).toISOString() }
        : { ...state, channelType: 13 };
    };
    const receipt = await runStagingVoiceMoveProof(input);
    assert.equal(receipt.verdict, "failed", fault);
    assert.equal(receipt.code, "discord_unconfirmed", fault);
    assert.deepEqual(signed, [], fault);
    assert.deepEqual(calls, ["status", "release", "status"], fault);
    assert.deepEqual(receipt.cleanup, { memberDisconnected: null, off: true, stopped: true }, fault);
    assertExactFinish(window);
  }
});

test("protected proof cannot accept OFF and stopped from another lease epoch", async () => {
  const window = proofWindow();
  const { input, calls } = window;
  const result = await runStagingVoiceMoveProof({
    ...input,
    async control(request) {
      const state = await input.control(request);
      if (request.action !== "status" || state.use.phase !== "released") return state;
      const epoch = state.epoch + 1;
      return { ...state, epoch, use: { ...state.use, epoch } };
    },
  });
  assert.equal(result.verdict, "failed");
  assert.equal(result.code, "cleanup_unconfirmed");
  assert.equal(result.cases.length, 3);
  assert.deepEqual(calls, ["assert", "assert", "assert", "status", "release", "status"]);
  assert.deepEqual(result.cleanup, { memberDisconnected: true, off: null, stopped: null });
  assertExactFinish(window);
});
