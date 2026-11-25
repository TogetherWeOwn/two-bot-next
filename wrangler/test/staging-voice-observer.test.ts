import assert from "node:assert/strict";
import { test } from "node:test";
import { runVoiceMoveAcceptance } from "../scripts/staging-voice-move-acceptance.mjs";
import { mockVoiceMoveWindow } from "./fixtures/staging-voice-move.ts";
import { mockVoiceObservation } from "./fixtures/staging-voice-observer.ts";

type MockInput = Parameters<typeof mockVoiceObservation>[0];

function observerWindow(alter?: (checkpoint: string, input: MockInput) => MockInput) {
  const window = mockVoiceMoveWindow();
  const { args } = window;
  let now = args.now();
  const readings: [string, ReturnType<typeof mockVoiceObservation>][] = [];
  const observe: typeof args.observe = async (request) => {
    const { checkpoint, channelId } = request;
    now += 1;
    const notBefore = now;
    const reading = await args.observe(request);
    const input = {
      fixture: args.fixture, channelId, notBefore, now,
      readableChannelIds: args.fixture.channelIds,
      reading: { ...reading, channelType: 2, observedAt: new Date(now).toISOString() },
    };
    const observation = mockVoiceObservation(alter ? alter(checkpoint, input) : input);
    readings.push([checkpoint, observation]);
    return observation;
  };
  return { ...window, readings, input: { ...args, observe, now: () => now } };
}

test("mock observer preserves the pinned room order and the member's effective Connect", async () => {
  const { input, readings } = observerWindow();
  const { guildId, discordId, channelIds: [source, destination] } = input.fixture;
  let captured = input.now();
  const receipt = await runVoiceMoveAcceptance(input);
  assert.equal(receipt.verdict, "passed");
  assert.equal(receipt.mode, "mock");
  const read = (channelId: string, connectedChannelId: string | null, canConnect: boolean) => ({
    guildId, discordId, channelId, connectedChannelId, canConnect, channelType: 2,
    observedAt: new Date(++captured).toISOString(),
  });
  assert.deepEqual(readings, [
    ["before:moved", read(destination, source, true)],
    ["after:moved", read(destination, destination, true)],
    ["before:not_permitted", read(source, destination, false)],
    ["after:not_permitted", read(source, destination, false)],
    ["before:not_connected", read(destination, null, true)],
    ["after:not_connected", read(destination, null, true)],
  ]);
});

test("mock observer binds a readChannels projection to its room and the 60-second limit", () => {
  const { args } = mockVoiceMoveWindow();
  const { fixture } = args;
  const [source, destination] = fixture.channelIds;
  const now = args.now();
  const reading = {
    guildId: fixture.guildId, discordId: fixture.discordId, channelId: source,
    connectedChannelId: source, canConnect: false, channelType: 2,
    observedAt: new Date(now - 60_000).toISOString(),
  };
  const input = { fixture, channelId: source, reading, readableChannelIds: [source], notBefore: now - 61_000, now };
  assert.deepEqual(mockVoiceObservation(input), reading);
  for (const change of [
    { channelId: destination },
    { reading: { ...reading, discordId: "100000000000000099" } },
    { reading: { ...reading, observedAt: new Date(now - 60_001).toISOString() } },
    { reading: { ...reading, observedAt: reading.observedAt.replace(".000Z", "Z") } },
  ]) {
    assert.throws(() => mockVoiceObservation({ ...input, ...change }), /^Error: mock_observer_unconfirmed$/);
  }
});

test("mock observer refuses non-voice and out-of-window checkpoint readings", async () => {
  for (const fault of ["stage channel", "before checkpoint", "in the future"]) {
    const change = (now: number) => {
      if (fault === "stage channel") return { channelType: 13 };
      return { observedAt: new Date(now + (fault === "before checkpoint" ? -1 : 1)).toISOString() };
    };
    const { args } = mockVoiceMoveWindow();
    const baseline = await runVoiceMoveAcceptance({
      ...args,
      // Only the producer can reject these reads. The runner checks the normalized core fields.
      observe: async (request) => ({ ...await args.observe(request), ...change(args.now()) }),
    });
    assert.equal(baseline.verdict, "passed", fault);

    const { input, signed, calls } = observerWindow((checkpoint, observed) => checkpoint === "before:moved"
      ? { ...observed, reading: { ...observed.reading, ...change(observed.now) } }
      : observed);
    const receipt = await runVoiceMoveAcceptance(input);
    assert.equal(receipt.verdict, "failed", fault);
    assert.equal(receipt.code, "probe_failed", fault);
    assert.deepEqual(signed, [], fault);
    assert.deepEqual(calls, ["status", "release", "status"], fault);
    assert.deepEqual(receipt.cleanup, { memberDisconnected: null, off: true, stopped: true }, fault);
  }
});

test("mock observer refuses a denial readback replayed at the next checkpoint", async () => {
  const pair: { earlier?: MockInput["reading"]; current?: MockInput["reading"] } = {};
  const { input, readings, signed } = observerWindow((checkpoint, observed) => {
    if (checkpoint === "before:not_permitted") pair.earlier = observed.reading;
    if (checkpoint === "after:not_permitted") {
      pair.current = observed.reading;
      return { ...observed, reading: pair.earlier ?? null };
    }
    return observed;
  });
  const receipt = await runVoiceMoveAcceptance(input);
  assert.ok(pair.earlier?.observedAt && pair.current?.observedAt);
  assert.ok(pair.earlier.observedAt < pair.current.observedAt);
  assert.deepEqual(pair.current, { ...pair.earlier, observedAt: pair.current.observedAt });
  assert.equal(readings.at(-1)?.[0], "before:not_permitted");
  assert.equal(receipt.verdict, "failed");
  assert.equal(receipt.code, "probe_failed");
  assert.deepEqual(receipt.cases.map(({ outcome }) => outcome), ["moved"]);
  assert.equal(signed.length, 2);
  assert.deepEqual(receipt.cleanup, { memberDisconnected: null, off: true, stopped: true });
});

test("mock observer refuses an unreadable occupied room even when the target is visible", async () => {
  const { input, signed } = observerWindow((checkpoint, observed) => checkpoint === "before:moved"
    ? { ...observed, readableChannelIds: [observed.channelId] }
    : observed);
  const receipt = await runVoiceMoveAcceptance(input);
  assert.equal(receipt.verdict, "failed");
  assert.equal(receipt.code, "probe_failed");
  assert.deepEqual(signed, []);
  assert.deepEqual(receipt.cleanup, { memberDisconnected: null, off: true, stopped: true });
});

test("mock observer refuses unknown occupancy in the disconnect case", async () => {
  for (const fault of ["missing readback", "missing occupancy", "outside pinned rooms", "unreadable room"]) {
    const { input, signed, calls } = observerWindow((checkpoint, observed) => {
      if (checkpoint !== "before:not_connected") return observed;
      if (fault === "missing readback") return { ...observed, reading: null };
      if (fault === "unreadable room") return { ...observed, readableChannelIds: [observed.channelId] };
      const reading = { ...observed.reading };
      if (fault === "missing occupancy") {
        delete reading.connectedChannelId;
        return { ...observed, reading };
      }
      const outside = "100000000000000099";
      return { ...observed, reading: { ...reading, connectedChannelId: outside },
        readableChannelIds: [...observed.readableChannelIds, outside] };
    });
    const receipt = await runVoiceMoveAcceptance(input);
    assert.equal(receipt.verdict, "failed", fault);
    assert.equal(receipt.code, "probe_failed", fault);
    assert.deepEqual(receipt.cases.map(({ outcome }) => outcome), ["moved", "not_permitted"], fault);
    assert.deepEqual(signed.map(({ channel_id }) => channel_id), [...input.fixture.channelIds].reverse(), fault);
    assert.deepEqual(calls, ["assert", "assert", "status", "release", "status"], fault);
    assert.deepEqual(receipt.cleanup, { memberDisconnected: false, off: true, stopped: true }, fault);
  }
});
