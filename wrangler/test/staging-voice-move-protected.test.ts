import assert from "node:assert/strict";
import { test } from "node:test";
import { runProtectedVoiceMoveAcceptance } from "../scripts/staging-voice-move-acceptance.mjs";
import { mockVoiceMoveWindow } from "./fixtures/staging-voice-move.ts";

function protectedWindow(elapsedMs?: number) {
  const { args, signed, calls, moves } = mockVoiceMoveWindow(elapsedMs);
  const { evidence, caller, ...window } = args;
  const reads: string[] = [];
  return {
    signed, calls, moves, reads,
    input: {
      ...window,
      previousReady: evidence.previousReady,
      previousOff: evidence.previousOff,
      session: {
        ...caller,
        async readCurrent({ signal }: { signal: AbortSignal }) {
          signal.throwIfAborted();
          reads.push("session");
          return evidence.session;
        },
      },
      async readChannels({ signal }: { signal: AbortSignal }) {
        signal.throwIfAborted();
        reads.push("channels");
        return evidence.readback;
      },
    },
  };
}

test("protected voice adapter defaults off without touching readers or the lease", async () => {
  const { input, reads, signed, calls } = protectedWindow();
  for (const enabled of [undefined, false, "true"]) {
    const result = await runProtectedVoiceMoveAcceptance({ ...input, enabled });
    assert.equal(result.verdict, "disabled");
  }
  assert.deepEqual({ reads, signed, calls }, { reads: [], signed: [], calls: [] });
});

test("protected voice adapter proves both no-effect cases and releases the exact active window", async () => {
  const { input, reads, signed, calls, moves } = protectedWindow();
  const releases = [];
  const result = await runProtectedVoiceMoveAcceptance({
    ...input,
    async control(request) {
      if (request.action === "release") releases.push(request);
      return input.control(request);
    },
  });
  const [source, destination] = input.fixture.channelIds;
  const { runId, runAttempt, expiresAt, sha, deploymentId } = input.lease;
  assert.equal(result.verdict, "passed");
  assert.equal(result.mode, "mock");
  assert.deepEqual(reads, ["session", "channels"]);
  assert.deepEqual(signed.map(({ channel_id }) => channel_id), [destination, source, destination]);
  assert.deepEqual(moves, [{ discordId: input.fixture.discordId, channelId: destination }]);
  assert.deepEqual(result.cases.map(({ outcome }) => outcome), ["moved", "not_permitted", "not_connected"]);
  assert.deepEqual(calls, ["assert", "assert", "assert", "status", "release", "status"]);
  assert.deepEqual(releases.map(({ action, runId, runAttempt, expiresAt, sha, deploymentId }) =>
    ({ action, runId, runAttempt, expiresAt, sha, deploymentId })),
  [{ action: "release", runId, runAttempt, expiresAt, sha, deploymentId }]);
  assert.deepEqual(result.cleanup, { memberDisconnected: true, off: true, stopped: true });
});

test("protected voice adapter refuses an unrelated ready receipt and cleans up only its own window", async () => {
  const { input, reads, calls, signed } = protectedWindow();
  const result = await runProtectedVoiceMoveAcceptance({ ...input, ready: input.previousReady });
  assert.equal(result.verdict, "failed");
  assert.equal(result.code, "lease_unconfirmed");
  assert.deepEqual({ reads, signed }, { reads: [], signed: [] });
  assert.deepEqual(calls, ["status", "release", "status"]);
  assert.deepEqual(result.cleanup, { memberDisconnected: null, off: true, stopped: true });
});

test("protected voice adapter rejects an unbound caller before collecting readbacks", async () => {
  for (const sessionPresent of [false, true]) {
    const { input, reads, calls, signed } = protectedWindow();
    const session = sessionPresent ? { ...input.session, discordId: input.fixture.channelIds[0] } : undefined;
    const result = await runProtectedVoiceMoveAcceptance({ ...input, session });
    assert.equal(result.verdict, "failed");
    assert.equal(result.code, "caller_unconfirmed");
    assert.deepEqual({ reads, signed }, { reads: [], signed: [] });
    assert.deepEqual(calls, ["status", "release", "status"]);
    assert.deepEqual(result.cleanup, { memberDisconnected: null, off: true, stopped: true });
  }
});

test("protected voice adapter finishes the window on mismatched or failed readbacks", async () => {
  const sentinel = "private diagnostic from the fixture reader";
  for (const fault of ["session", "observer", "prior window"]) {
    const { input, calls, signed } = protectedWindow();
    const result = await runProtectedVoiceMoveAcceptance({
      ...input,
      previousOff: fault === "prior window" ? undefined : input.previousOff,
      session: {
        ...input.session,
        async readCurrent(request) {
          const session = await input.session.readCurrent(request);
          return fault === "session" ? { authenticated: true, private: sentinel } : session;
        },
      },
      async readChannels(request) {
        if (fault === "observer") throw new Error(sentinel);
        return input.readChannels(request);
      },
    });
    assert.equal(result.verdict, "failed");
    assert.equal(result.code, {
      session: "session_unconfirmed", observer: "probe_failed", "prior window": "prior_cleanup_unconfirmed",
    }[fault]);
    assert.deepEqual(signed, []);
    assert.deepEqual(result.cases, []);
    assert.deepEqual(calls, ["status", "release", "status"]);
    assert.deepEqual(result.cleanup, { memberDisconnected: null, off: true, stopped: true });
    assert.equal(JSON.stringify(result).includes(sentinel), false);
  }
});

test("protected voice adapter closes the lease when a scoped readback stalls", async () => {
  const { input, calls, signed, reads } = protectedWindow(15 * 60_000 - 1010);
  const lateSession = Promise.withResolvers<object>();
  let signal: AbortSignal | undefined;
  const result = await runProtectedVoiceMoveAcceptance({
    ...input,
    session: {
      ...input.session,
      readCurrent(request) {
        signal = request.signal;
        return lateSession.promise;
      },
    },
    readChannels(request) {
      reads.push("channels invoked");
      return input.readChannels(request);
    },
  });
  assert.equal(result.verdict, "failed");
  assert.equal(result.code, "probe_timeout");
  assert.equal(signal?.aborted, true);
  lateSession.resolve({});
  await new Promise<void>((resolve) => setImmediate(resolve));
  assert.deepEqual({ signed, reads }, { signed: [], reads: [] });
  assert.deepEqual(calls, ["status", "release", "status"]);
  assert.deepEqual(result.cleanup, { memberDisconnected: null, off: true, stopped: true });
});

test("protected voice adapter stops on an inconsistent denial and still finishes the window", async () => {
  const { input, calls, signed } = protectedWindow();
  const result = await runProtectedVoiceMoveAcceptance({
    ...input,
    async observe(request) {
      const state = await input.observe(request);
      if (request.checkpoint === "after:not_permitted") state.connectedChannelId = null;
      return state;
    },
  });
  assert.equal(result.verdict, "failed");
  assert.equal(result.code, "after_unconfirmed");
  assert.deepEqual(result.cases.map(({ outcome }) => outcome), ["moved"]);
  assert.equal(signed.length, 2);
  assert.deepEqual(calls, ["assert", "assert", "status", "release", "status"]);
  assert.deepEqual(result.cleanup, { memberDisconnected: null, off: true, stopped: true });
});

test("protected voice adapter fails when final status cannot affirm OFF and stopped", async () => {
  for (const field of ["voiceMoveEnabled", "running"]) {
    const { input, calls } = protectedWindow();
    const result = await runProtectedVoiceMoveAcceptance({
      ...input,
      async control(request) {
        const state = await input.control(request);
        if (request.action === "status" && state.use.phase === "released") state[field] = true;
        return state;
      },
    });
    assert.equal(result.verdict, "failed");
    assert.equal(result.code, "cleanup_unconfirmed");
    assert.deepEqual(calls, ["assert", "assert", "assert", "status", "release", "status"]);
    assert.deepEqual(result.cleanup, { memberDisconnected: true, off: null, stopped: null });
  }
});
