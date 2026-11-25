import assert from "node:assert/strict";
import { test } from "node:test";
import { preflightVoiceMoveAcceptance, runVoiceMoveAcceptance } from "../scripts/staging-voice-move-acceptance.mjs";
import { mockVoiceMoveWindow } from "./fixtures/staging-voice-move.ts";

test("voice preflight defaults off and returns only a verdict for complete evidence", () => {
  const { args, calls, signed } = mockVoiceMoveWindow();
  assert.deepEqual(preflightVoiceMoveAcceptance(), { verdict: "disabled" });
  assert.deepEqual(preflightVoiceMoveAcceptance({ ...args, enabled: "true" }), { verdict: "disabled" });
  assert.deepEqual(preflightVoiceMoveAcceptance(args), { verdict: "passed" });
  assert.deepEqual(calls, []);
  assert.deepEqual(signed, []);
});

test("voice preflight refuses swapped channels and mismatched actors", () => {
  const { args } = mockVoiceMoveWindow();
  const { session, readback } = args.evidence;
  const cases = [
    [{ session: { ...session, channelIds: [...session.channelIds].reverse() } }, "session_unconfirmed"],
    [{ session: { ...session, discordId: "100000000000000099" } }, "session_unconfirmed"],
    [{ session: { ...session, authenticated: false } }, "session_unconfirmed"],
    [{ readback: { source: readback.destination, destination: readback.source } }, "discord_unconfirmed"],
    [{ readback: { ...readback, source: { ...readback.source, discordId: "100000000000000099" } } }, "discord_unconfirmed"],
    [{ readback: { ...readback, source: { ...readback.source, channelType: 0 } } }, "discord_unconfirmed"],
    [{ readback: { ...readback, source: { ...readback.source, canConnect: true } } }, "discord_unconfirmed"],
    [{ readback: { ...readback, destination: { ...readback.destination, canConnect: false } } }, "discord_unconfirmed"],
    [{ readback: { ...readback, destination: { ...readback.destination, connectedChannelId: null } } }, "discord_unconfirmed"],
  ];
  for (const [change, code] of cases) {
    assert.deepEqual(preflightVoiceMoveAcceptance({ ...args, evidence: { ...args.evidence, ...change } }),
      { verdict: "failed", code });
  }
});

test("voice preflight refuses missing, stale, future or expired evidence", () => {
  const { args } = mockVoiceMoveWindow(70_000);
  const { evidence, ready } = args;
  const observedAt = new Date(args.now() - 60_001).toISOString();
  const cases = [
    [{ lease: undefined }, "lease_invalid"],
    [{ ready: undefined }, "lease_unconfirmed"],
    [{ ready: { ...ready, observedAt } }, "lease_unconfirmed"],
    [{ ready: { ...ready, observedAt: new Date(args.now() + 1).toISOString() } }, "lease_unconfirmed"],
    [{ evidence: undefined }, "session_unconfirmed"],
    [{ evidence: { ...evidence, session: { ...evidence.session, observedAt } } }, "session_unconfirmed"],
    [{ evidence: { ...evidence, readback: { ...evidence.readback, source: undefined } } }, "discord_unconfirmed"],
    [{ evidence: { ...evidence, readback: { ...evidence.readback,
      destination: { ...evidence.readback.destination, observedAt } } } }, "discord_unconfirmed"],
  ];
  for (const [change, code] of cases) {
    assert.deepEqual(preflightVoiceMoveAcceptance({ ...args, ...change }), { verdict: "failed", code });
  }
  const lastSecond = mockVoiceMoveWindow(15 * 60_000 - 1000);
  assert.deepEqual(preflightVoiceMoveAcceptance(lastSecond.args), { verdict: "failed", code: "lease_unconfirmed" });
});

test("voice preflight requires OFF and stopped evidence for the exact prior lease", () => {
  const { args } = mockVoiceMoveWindow();
  const { evidence } = args;
  const { previousReady, previousOff } = evidence;
  const cases = [
    { previousReady: undefined },
    { previousReady: { ...previousReady, voiceMoveEnabled: false } },
    { previousOff: undefined },
    { previousOff: { ...previousOff, running: undefined } },
    { previousOff: { ...previousOff, voiceMoveEnabled: true } },
    { previousOff: { ...previousOff, use: { ...previousOff.use, runAttempt: 2 } } },
    { previousOff: { ...previousOff, use: { ...previousOff.use, acquiredAt: previousOff.use.acquiredAt + 1 } } },
    { previousOff: { ...previousOff, use: { ...previousOff.use, phase: "active" } } },
    { previousOff: { ...previousOff, observedAt: args.ready.observedAt } },
  ];
  for (const pin of [{ runId: args.lease.runId }, { sha: "b".repeat(40) }, { deploymentId: "other" }, { epoch: 3 }]) {
    cases.push({
      previousReady: { ...previousReady, ...pin, use: { ...previousReady.use, ...pin } },
      previousOff: { ...previousOff, ...pin, use: { ...previousOff.use, ...pin } },
    });
  }
  for (const change of cases) {
    assert.deepEqual(preflightVoiceMoveAcceptance({ ...args, evidence: { ...evidence, ...change } }),
      { verdict: "failed", code: "prior_cleanup_unconfirmed" });
  }
  const earlier = (time: number) => time - 15 * 60_000;
  const stale = (receipt: typeof previousReady) => ({
    ...receipt,
    observedAt: new Date(earlier(Date.parse(receipt.observedAt))).toISOString(),
    use: { ...receipt.use, acquiredAt: earlier(receipt.use.acquiredAt), expiresAt: earlier(receipt.use.expiresAt) },
  });
  assert.deepEqual(preflightVoiceMoveAcceptance({
    ...args, evidence: { ...evidence, previousReady: stale(previousReady), previousOff: stale(previousOff) },
  }), { verdict: "failed", code: "prior_cleanup_unconfirmed" });
  const expiredOff = {
    ...previousOff, use: { ...previousOff.use, phase: "active" },
    observedAt: new Date(previousOff.use.expiresAt).toISOString(),
  };
  assert.deepEqual(preflightVoiceMoveAcceptance({ ...args, evidence: { ...evidence, previousOff: expiredOff } }),
    { verdict: "passed" });
});

test("voice acceptance refuses missing preflight input before any move", async () => {
  const { args, calls, signed } = mockVoiceMoveWindow();
  const receipt = await runVoiceMoveAcceptance({ ...args, evidence: undefined });
  assert.equal(receipt.code, "session_unconfirmed");
  assert.equal(receipt.verdict, "failed");
  assert.deepEqual(signed, []);
  assert.deepEqual(calls, ["status", "release", "status"]);
  assert.equal(receipt.cleanup.off, true);
});

test("voice acceptance defaults off and does not touch the lease or caller", async () => {
  const { args, calls, signed } = mockVoiceMoveWindow();
  for (const enabled of [undefined, false, "true", 1]) {
    const receipt = await runVoiceMoveAcceptance({ ...args, enabled });
    assert.equal(receipt.verdict, "disabled");
    assert.equal(receipt.lease, null);
  }
  assert.deepEqual(calls, []);
  assert.deepEqual(signed, []);
});

test("voice acceptance records the three session-bound outcomes and OFF", async () => {
  const { args, signed, moves, calls } = mockVoiceMoveWindow();
  const receipt = await runVoiceMoveAcceptance(args);
  const [source, destination] = args.fixture.channelIds;
  const { discordId } = args.fixture;

  assert.equal(receipt.verdict, "passed");
  assert.equal(receipt.mode, "mock");
  assert.deepEqual(receipt.lease, args.lease);
  assert.deepEqual(receipt.cases.map(({ outcome }) => outcome), ["moved", "not_permitted", "not_connected"]);
  assert.equal(new Set(receipt.cases.map(({ requestId }) => requestId)).size, 3);
  assert.deepEqual(signed, [destination, source, destination].map((channel_id) => ({
    action: "voice.move_member", discord_id: discordId, channel_id,
  })));
  assert.deepEqual(moves, [{ discordId, channelId: destination }]);
  assert.deepEqual(calls, ["assert", "assert", "assert", "status", "release", "status"]);
  assert.deepEqual(receipt.cleanup, { memberDisconnected: true, off: true, stopped: true });
});

test("voice fixture recognizes the occupied source before the Connect denial", async () => {
  const { args, moves } = mockVoiceMoveWindow();
  const [channelId] = args.fixture.channelIds;
  const reply = await args.caller.move({ channelId, signal: new AbortController().signal });
  assert.deepEqual(reply.body.result, { outcome: "already_in_room" });
  assert.deepEqual(moves, []);
});

test("voice acceptance refuses mismatched fixture, caller or ready proof before dispatch", async () => {
  const cases = [
    [(args) => ({ fixture: { ...args.fixture, channelIds: Array(2).fill(args.fixture.channelIds[0]) } }), "fixture_invalid"],
    [(args) => ({ fixture: { ...args.fixture, channelIds: [...args.fixture.channelIds, "100000000000000099"] } }), "fixture_invalid"],
    [(args) => ({ fixture: { ...args.fixture, channelIds: [args.fixture.channelIds[0], "18446744073709551616"] } }), "fixture_invalid"],
    [(args) => ({ fixture: { ...args.fixture, channelIds: [args.fixture.channelIds[0], "300"] } }), "fixture_invalid"],
    [(args) => ({ fixture: { ...args.fixture, environment: "production" } }), "fixture_invalid"],
    [(args) => ({ caller: { ...args.caller, environment: "production" } }), "caller_unconfirmed"],
    [(args) => ({ caller: { ...args.caller, discordId: undefined } }), "caller_unconfirmed"],
    [(args) => ({ caller: { ...args.caller, discordId: "100000000000000099" } }), "caller_unconfirmed"],
    [(args) => ({ ready: { ...args.ready, voiceMoveEnabled: undefined } }), "lease_unconfirmed"],
    [(args) => ({ ready: { ...args.ready, use: { ...args.ready.use, runAttempt: 2 } } }), "lease_unconfirmed"],
  ];
  for (const [change, code] of cases) {
    const { args, calls, signed } = mockVoiceMoveWindow();
    const receipt = await runVoiceMoveAcceptance({ ...args, ...change(args) });
    assert.equal(receipt.verdict, "failed");
    assert.equal(receipt.code, code);
    assert.deepEqual(signed, []);
    assert.deepEqual(calls, ["status", "release", "status"]);
    assert.deepEqual(receipt.cleanup, { memberDisconnected: null, off: true, stopped: true });
  }
});

test("voice acceptance refuses unconfirmed account state, presence or destination permissions", async () => {
  for (const override of [
    { discordId: "999" }, { guildId: "999" }, { channelId: "999" },
    { connectedChannelId: "999" }, { connectedChannelId: undefined }, { canConnect: false },
  ]) {
    const { args, signed } = mockVoiceMoveWindow();
    const receipt = await runVoiceMoveAcceptance({
      ...args, observe: async (check) => ({ ...await args.observe(check), ...override }),
    });
    assert.equal(receipt.code, "before_unconfirmed");
    assert.equal(receipt.verdict, "failed");
    assert.deepEqual(signed, []);
    assert.equal(receipt.cleanup.off, true);
  }
});

test("voice acceptance stops on ambiguous responses and preserves redacted cleanup", async () => {
  const sentinel = "untrusted diagnostic should never be copied";
  const responses = [
    (reply) => ({ ...reply, status: 403 }),
    (reply) => ({ ...reply, body: { ...reply.body, request_id: sentinel } }),
    (reply) => ({ ...reply, body: { ...reply.body, error: { message: sentinel } } }),
    (reply) => ({ ...reply, body: { ...reply.body, result: { outcome: "moved", from_channel_id: "999" } } }),
    () => { throw new Error(sentinel); },
  ];
  for (const response of responses) {
    const { args, signed } = mockVoiceMoveWindow();
    const receipt = await runVoiceMoveAcceptance({
      ...args, caller: { ...args.caller, move: async (request) => response(await args.caller.move(request)) },
    });
    assert.equal(receipt.verdict, "failed");
    assert.match(receipt.code, /^(outcome_unconfirmed|probe_failed)$/);
    assert.deepEqual(receipt.cases, []);
    assert.equal(signed.length, 1);
    assert.deepEqual(receipt.cleanup, { memberDisconnected: null, off: true, stopped: true });
    assert.equal(JSON.stringify(receipt).includes(sentinel), false);
  }
});

test("voice acceptance rejects a replay or an inconsistent refusal", async () => {
  for (const fault of ["replay", "voice_state", "unexpected_field"]) {
    const { args, signed } = mockVoiceMoveWindow();
    const receipt = await runVoiceMoveAcceptance({
      ...args,
      caller: {
        ...args.caller,
        async move(request) {
          const reply = await args.caller.move(request);
          if (fault === "replay") reply.body.request_id = "1".repeat(26);
          if (fault === "unexpected_field" && reply.body.result.outcome === "not_permitted") {
            reply.body.result.from_channel_id = args.fixture.channelIds[1];
          }
          return reply;
        },
      },
      async observe(check) {
        const state = await args.observe(check);
        if (fault === "voice_state" && check.checkpoint === "after:not_permitted") {
          state.connectedChannelId = args.fixture.channelIds[0];
        }
        return state;
      },
    });
    assert.equal(receipt.verdict, "failed");
    assert.equal(receipt.code, fault === "voice_state" ? "after_unconfirmed" : "outcome_unconfirmed");
    assert.equal(receipt.cases.length, 1);
    assert.equal(signed.length, 2);
    assert.equal(receipt.cleanup.off, true);
  }
});

test("voice acceptance refuses to dispatch or release after an ownership epoch change", async () => {
  const { args, signed, calls } = mockVoiceMoveWindow();
  const receipt = await runVoiceMoveAcceptance({
    ...args, control: async (request) => ({ ...await args.control(request), epoch: args.lease.epoch + 1 }),
  });
  assert.equal(receipt.verdict, "failed");
  assert.equal(receipt.code, "lease_unconfirmed");
  assert.deepEqual(signed, []);
  assert.deepEqual(calls, ["assert", "status"]);
  assert.equal(receipt.cleanup.off, null);
});

test("voice acceptance fails after all cases when OFF or stopped readback is missing", async () => {
  for (const field of ["voiceMoveEnabled", "running"]) {
    const { args, signed } = mockVoiceMoveWindow();
    const receipt = await runVoiceMoveAcceptance({
      ...args,
      async control(request) {
        const state = await args.control(request);
        if (request.action === "status" && state.use.phase === "released") state[field] = undefined;
        return state;
      },
    });
    assert.equal(receipt.verdict, "failed");
    assert.equal(receipt.code, "cleanup_unconfirmed");
    assert.equal(receipt.cases.length, 3);
    assert.equal(signed.length, 3);
    assert.deepEqual(receipt.cleanup, { memberDisconnected: true, off: null, stopped: null });
  }
});

test("voice acceptance stops probing at the lease deadline and still invokes finish", async () => {
  const { args, calls, signed } = mockVoiceMoveWindow();
  let at = args.now();
  const receipt = await runVoiceMoveAcceptance({
    ...args, now: () => at,
    async observe(check) {
      const state = await args.observe(check);
      at = args.lease.expiresAt - 1000;
      return state;
    },
  });
  assert.equal(receipt.verdict, "failed");
  assert.equal(receipt.code, "lease_elapsed");
  assert.deepEqual(signed, []);
  assert.deepEqual(calls, ["status", "release", "status"]);
  assert.equal(receipt.cleanup.off, true);
});

test("voice acceptance aborts a stalled probe and still invokes finish", async () => {
  const { args, calls, signed } = mockVoiceMoveWindow(15 * 60_000 - 1005);
  let aborted = false;
  const receipt = await runVoiceMoveAcceptance({
    ...args,
    observe({ signal }) {
      signal.addEventListener("abort", () => { aborted = true; }, { once: true });
      return new Promise(() => {});
    },
  });
  assert.equal(receipt.verdict, "failed");
  assert.equal(receipt.code, "probe_timeout");
  assert.equal(aborted, true);
  assert.deepEqual(signed, []);
  assert.deepEqual(calls, ["status", "release", "status"]);
  assert.equal(receipt.cleanup.off, true);
});
