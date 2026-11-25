import assert from "node:assert/strict";
import { test } from "node:test";
import { runVoiceMoveAcceptance } from "../scripts/staging-voice-move-acceptance.mjs";
import { mockVoiceMoveWindow } from "./fixtures/staging-voice-move.ts";

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
  const changes = [
    (args) => ({ fixture: { ...args.fixture, channelIds: ["300", "300"] } }),
    (args) => ({ fixture: { ...args.fixture, channelIds: ["300", "400", "500"] } }),
    (args) => ({ fixture: { ...args.fixture, channelIds: ["300", "18446744073709551616"] } }),
    (args) => ({ fixture: { ...args.fixture, environment: "production" } }),
    (args) => ({ caller: { ...args.caller, environment: "production" } }),
    (args) => ({ caller: { ...args.caller, discordId: undefined } }),
    (args) => ({ caller: { ...args.caller, discordId: "999" } }),
    (args) => ({ ready: { ...args.ready, voiceMoveEnabled: undefined } }),
    (args) => ({ ready: { ...args.ready, use: { ...args.ready.use, runAttempt: 2 } } }),
  ];
  for (const change of changes) {
    const { args, calls, signed } = mockVoiceMoveWindow();
    const receipt = await runVoiceMoveAcceptance({ ...args, ...change(args) });
    assert.equal(receipt.verdict, "failed");
    assert.ok(receipt.code);
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
  const { args, calls, signed } = mockVoiceMoveWindow();
  let aborted = false;
  const receipt = await runVoiceMoveAcceptance({
    ...args, now: () => args.lease.expiresAt - 1005,
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
