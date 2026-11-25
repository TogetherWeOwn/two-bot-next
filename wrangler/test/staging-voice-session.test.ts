import assert from "node:assert/strict";
import { test } from "node:test";
import { runProtectedVoiceMoveAcceptance } from "../scripts/staging-voice-move-acceptance.mjs";
import { bindStagingVoiceSession } from "../scripts/staging-voice-session.mjs";
import { mockVoiceMoveWindow } from "./fixtures/staging-voice-move.ts";

const origin = "https://next.togetherweown.com";
const receiptUrl = `${origin}/__probe/voice-move/session`;
const discordId = "100000000000000701";
const channels = ["100000000000000703", "100000000000000704"];
const revision = { version_id: "12345678-1234-4321-8123-123456789abc", commit: "a".repeat(40) };
const observedAt = "2030-06-15T12:34:56.123Z";
const body = { matched: true, observed_at: observedAt, revision };
const refusal = (error: unknown) => error instanceof Error
  && error.message === "staging_voice_session_unconfirmed" && !("cause" in error);
type MoveArgs = { channelId: string; signal: AbortSignal };

function response(content: BodyInit | null = JSON.stringify(body), init: ResponseInit = {}, url = receiptUrl) {
  return Object.defineProperty(new Response(content, {
    headers: { "content-type": "application/json; charset=UTF-8", "cache-control": "no-store, private", ...init.headers },
    status: init.status,
  }), "url", { value: url });
}

function setup(request: (url: string, init: RequestInit) => Response | Promise<Response> | undefined = () => response()) {
  return {
    fixture: { environment: "staging", discordId, channelIds: [...channels] },
    expectedRevision: { ...revision },
    session: { environment: "staging", discordId, origin, request, async move(_args: MoveArgs) {} },
  };
}

test("reads the exact staged Web receipt through the existing scoped session and preserves time", async () => {
  const signal = new AbortController().signal;
  const calls = [];
  const input = setup(function (url, init) {
    assert.equal(this, input.session);
    assert.equal(url, receiptUrl);
    assert.deepEqual(init, {
      method: "POST",
      headers: { origin, accept: "application/json", "content-type": "application/json" },
      body: JSON.stringify({ discord_id: discordId, channel_ids: channels }),
      redirect: "error", cache: "no-store", signal,
    });
    calls.push("read");
    // JSON field order and formatting are immaterial. Read the body as bytes.
    return response(JSON.stringify({ revision, observed_at: observedAt, matched: true }, null, 1));
  });
  input.session.move = async function (args) {
    assert.equal(this, input.session);
    calls.push(args);
  };
  const session = bindStagingVoiceSession(input);
  input.fixture.channelIds.reverse();
  input.expectedRevision.commit = "b".repeat(40);
  input.session.move = async () => assert.fail("bound move replaced");
  assert.deepEqual({ environment: session.environment, discordId: session.discordId }, { environment: "staging", discordId });
  const projected = await session.readCurrent({ signal });
  assert.deepEqual(projected, {
    environment: "staging", action: "voice.move_member", authenticated: true, discordId, channelIds: channels, observedAt,
  });
  projected.channelIds.reverse();
  assert.deepEqual((await session.readCurrent({ signal })).channelIds, channels);
  const args = { channelId: channels[1], signal };
  await session.move(args);
  assert.deepEqual(calls, ["read", "read", args]);
});

test("refuses missing or unapproved local pins before touching transport", () => {
  let calls = 0;
  const input = setup(() => { calls++; return response(); });
  for (const bad of [
    { session: undefined },
    { session: { ...input.session, request: undefined } },
    { session: { ...input.session, move: undefined } },
    { session: { ...input.session, environment: "production" } },
    { session: { ...input.session, discordId: channels[0] } },
    ...[undefined, `${origin}/`, `${origin}:443`, "https://togetherweown.com", `${origin}.example`,
      `${origin}/?alternate`, "http://next.togetherweown.com"].map((origin) => ({ session: { ...input.session, origin } })),
    { fixture: undefined },
    { fixture: { ...input.fixture, environment: "production" } },
    { fixture: { ...input.fixture, discordId: "007" } },
    ...[[], [channels[0], channels[0]], [discordId, channels[1]], [channels[0], "18446744073709551616"],
      [channels[0], `${channels[1]}\n`], [...channels, discordId]].map((channelIds) => ({ fixture: { ...input.fixture, channelIds } })),
    { expectedRevision: undefined },
    { expectedRevision: {} },
    ...[revision.version_id.slice(0, 8), revision.version_id.toUpperCase(), `${revision.version_id}\n`]
      .map((version_id) => ({ expectedRevision: { ...revision, version_id } })),
    ...[revision.commit.slice(0, 12), revision.commit.toUpperCase(), `${revision.commit}\n`]
      .map((commit) => ({ expectedRevision: { ...revision, commit } })),
  ]) assert.throws(() => bindStagingVoiceSession({ ...input, ...bad }), refusal);
  assert.equal(calls, 0);
});

test("rejects status, redirect, media, cache and session mutation failures", async () => {
  for (const [label, reply] of Object.entries({
    "transport omitted response": undefined,
    "no body": response(null),
    "HTTP refusal": response(undefined, { status: 403 }),
    "other success status": response(undefined, { status: 201 }),
    "redirect response": response(undefined, { status: 302 }),
    "already followed": Object.defineProperty(response(), "redirected", { value: true }),
    "unexpected URL": response(undefined, {}, `${receiptUrl}?query=1`),
    "missing final URL": response(undefined, {}, ""),
    "other origin": response(undefined, {}, "https://togetherweown.com/__probe/voice-move/session"),
    "location header": response(undefined, { headers: { location: receiptUrl } }),
    "rotating session response": response(undefined, { headers: { "set-cookie": "synthetic=unexpected" } }),
    "non-JSON": response(undefined, { headers: { "content-type": "application/jsonx" } }),
    "cacheable receipt": response(undefined, { headers: { "cache-control": "private, max-age=60" } }),
    "quoted cache extension": response(undefined, { headers: { "cache-control": 'foo="a, private, no-store, b"' } }),
    "declared oversize": response(undefined, { headers: { "content-length": "1025" } }),
  })) await assertReceiptRefused(() => reply, label);
});

async function assertReceiptRefused(request: Parameters<typeof setup>[0], label: string) {
  await assert.rejects(bindStagingVoiceSession(setup(request)).readCurrent({ signal: new AbortController().signal }), refusal, label);
}

test("requires an exact receipt, full matching Web revision and canonical observation time", async () => {
  const json = JSON.stringify(body);
  for (const [label, content] of Object.entries({
    "malformed": "{",
    "missing match": JSON.stringify({ observed_at: observedAt, revision }),
    "false match": JSON.stringify({ ...body, matched: false }),
    "string match": JSON.stringify({ ...body, matched: "true" }),
    "asserted identity echoed": JSON.stringify({ ...body, discord_id: discordId }),
    "duplicate match": json.replace('"matched":true', '"matched":false,"matched":true'),
    "duplicate escaped key": json.replace('"matched":true', '"mat\\u0063hed":false,"matched":true'),
    "missing revision": JSON.stringify({ matched: true, observed_at: observedAt }),
    "extra revision key": JSON.stringify({ ...body, revision: { ...revision, tag: "other" } }),
    "duplicate revision key": json.replace('"commit":', '"commit":"other","commit":'),
    "wrong Worker": JSON.stringify({ ...body, revision: { ...revision, version_id: "12345678-1234-4321-8123-123456789abd" } }),
    "wrong commit": JSON.stringify({ ...body, revision: { ...revision, commit: "b".repeat(40) } }),
    "unattributed commit": JSON.stringify({ ...body, revision: { ...revision, commit: null } }),
    "invalid calendar date": JSON.stringify({ ...body, observed_at: "2030-02-30T12:34:56.123Z" }),
    "not UTC canonical": JSON.stringify({ ...body, observed_at: "2030-06-15T12:34:56.123+00:00" }),
    "missing milliseconds": JSON.stringify({ ...body, observed_at: "2030-06-15T12:34:56Z" }),
    "BOM": `\u{FEFF}${json}`,
    "invalid UTF-8": Buffer.concat([Buffer.from(json), Uint8Array.of(0xff)]),
  })) await assertReceiptRefused(() => response(content), label);
});

test("counts streamed bytes and cancels at the cap without draining the response", async () => {
  const json = JSON.stringify(body);
  const encoder = new TextEncoder();
  const exact = bindStagingVoiceSession(setup(() => response(json.padEnd(1024, " "))));
  assert.equal((await exact.readCurrent({ signal: new AbortController().signal })).observedAt, observedAt);

  let reads = 0;
  let cancelled = false;
  const stream = new ReadableStream({
    pull(controller) {
      reads++;
      controller.enqueue(encoder.encode(reads === 1 ? json : " ".repeat(1024)));
    },
    cancel() { cancelled = true; },
  }, { highWaterMark: 0 });
  const session = bindStagingVoiceSession(setup(() => response(stream)));
  await assert.rejects(session.readCurrent({ signal: new AbortController().signal }), refusal);
  assert.deepEqual({ reads, cancelled }, { reads: 2, cancelled: true });
});

test("redacts exceptions and refuses cancellation during transport or stalled body reads", async () => {
  const sentinel = "synthetic sensitive transport detail";
  const controller = new AbortController();
  const failed = bindStagingVoiceSession(setup(() => { throw new Error(sentinel); }));
  await assert.rejects(failed.readCurrent({ signal: controller.signal }), refusal);
  let calls = 0;
  const late = Promise.withResolvers<Response>();
  const session = bindStagingVoiceSession(setup(() => { calls++; return late.promise; }));
  await assert.rejects(session.readCurrent(), refusal);
  await assert.rejects(session.readCurrent({ signal: AbortSignal.abort(sentinel) }), refusal);
  assert.equal(calls, 0);
  const pending = session.readCurrent({ signal: controller.signal });
  controller.abort(sentinel);
  await assert.rejects(pending, refusal);
  const lateCancelled = Promise.withResolvers<void>();
  late.resolve(response(new ReadableStream({ cancel: () => lateCancelled.resolve() })));
  await lateCancelled.promise;
  assert.equal(calls, 1);

  const stalled = new AbortController();
  const reading = Promise.withResolvers<void>();
  let cancelled = false;
  const stream = new ReadableStream({
    pull() { reading.resolve(); return new Promise(() => {}); },
    cancel() { cancelled = true; return new Promise(() => {}); },
  }, { highWaterMark: 0 });
  const scoped = bindStagingVoiceSession(setup(() => response(stream)));
  const readingBody = scoped.readCurrent({ signal: stalled.signal });
  await reading.promise;
  stalled.abort(sentinel);
  await assert.rejects(readingBody, refusal);
  assert.equal(cancelled, true);
});

type ReceiptCase = "valid" | "wrong build" | "swapped pair" | "before acquisition" | "future" | "stale";
async function runWithReceipt(scenario: ReceiptCase) {
  const { args, signed, moves, calls } = mockVoiceMoveWindow(scenario === "stale" ? 61_001 : 1_000);
  const { caller, evidence, now, fixture, ...input } = args;
  const reads: string[] = [];
  const scenarioTimes: Partial<Record<ReceiptCase, number>> = {
    "before acquisition": input.ready.use.acquiredAt - 1,
    future: now() + 1,
    stale: input.ready.use.acquiredAt, // The receipt fails on age alone.
  };
  const sampledAt = scenarioTimes[scenario] ?? now();
  const scope = {
    ...caller,
    origin,
    move(args: MoveArgs) {
      assert.deepEqual(reads, ["session", "discord"], "both evidence reads precede the move");
      return caller.move(args);
    },
    request(_target: string, init: RequestInit & { signal: AbortSignal }) {
      init.signal.throwIfAborted();
      reads.push("session");
      const expectedChannels = scenario === "swapped pair" ? [...fixture.channelIds].reverse() : fixture.channelIds;
      const matches = init.body === JSON.stringify({ discord_id: fixture.discordId, channel_ids: expectedChannels });
      if (!matches) return response(JSON.stringify({ error: "forbidden" }), { status: 403 });
      const servedRevision = scenario === "wrong build" ? { ...revision, commit: "c".repeat(40) } : revision;
      return response(JSON.stringify({ matched: true, observed_at: new Date(sampledAt).toISOString(), revision: servedRevision }));
    },
  };
  const session = bindStagingVoiceSession({ session: scope, fixture, expectedRevision: revision });
  const { runId, runAttempt, expiresAt, sha, deploymentId } = input.lease;
  const result = await runProtectedVoiceMoveAcceptance({
    ...input, now, fixture, session,
    previousReady: evidence.previousReady, previousOff: evidence.previousOff,
    async readChannels({ signal }) {
      signal.throwIfAborted();
      reads.push("discord");
      return evidence.readback;
    },
    async control(request) {
      if (request.action === "release") {
        assert.deepEqual({ runId: request.runId, runAttempt: request.runAttempt, expiresAt: request.expiresAt,
          sha: request.sha, deploymentId: request.deploymentId }, { runId, runAttempt, expiresAt, sha, deploymentId });
      }
      return input.control(request);
    },
  });
  return { result, signed, moves, calls, reads };
}

test("the protected runner completes the mock cases with a verified receipt", async () => {
  const { result, signed, moves, calls } = await runWithReceipt("valid");
  assert.deepEqual({ verdict: result.verdict, mode: result.mode }, { verdict: "passed", mode: "mock" });
  assert.deepEqual(result.cases.map(({ outcome }) => outcome), ["moved", "not_permitted", "not_connected"]);
  assert.deepEqual(calls, ["assert", "assert", "assert", "status", "release", "status"]);
  assert.deepEqual(result.cleanup, { memberDisconnected: true, off: true, stopped: true });
  assert.deepEqual({ signed: signed.length, moves: moves.length }, { signed: 3, moves: 1 });
});

test("the protected runner refuses wrong-build and stale receipts and finishes the lease", async () => {
  const cases = ["wrong build", "swapped pair", "before acquisition", "future", "stale"] as const;
  for (const scenario of cases) {
    const { result, signed, moves, calls, reads } = await runWithReceipt(scenario);
    const readerRefused = scenario === "wrong build" || scenario === "swapped pair";
    assert.deepEqual({ verdict: result.verdict, mode: result.mode }, { verdict: "failed", mode: "mock" }, scenario);
    assert.equal(result.code, readerRefused ? "probe_failed" : "session_unconfirmed", scenario);
    assert.deepEqual(reads, readerRefused ? ["session"] : ["session", "discord"], scenario);
    assert.deepEqual({ cases: result.cases, signed, moves }, { cases: [], signed: [], moves: [] }, scenario);
    assert.deepEqual(result.cleanup, { memberDisconnected: null, off: true, stopped: true }, scenario);
    assert.deepEqual(calls, ["status", "release", "status"], scenario);
  }
});
