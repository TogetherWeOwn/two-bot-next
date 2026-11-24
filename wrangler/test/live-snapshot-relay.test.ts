import assert from "node:assert/strict";
import { test } from "node:test";
import { MAX_BODY_BYTES, relayReceiverResponse } from "../src/internal-actions.ts";
import {
  handleLiveSnapshotRelay, LIVE_SNAPSHOT_BYTES, LIVE_SNAPSHOT_TIMEOUT_MS,
} from "../src/live-snapshot-relay.ts";
import { DEPLOYMENT_HEADER } from "../src/ownership.ts";
import {
  body, origin, path, receiver, request, scope, signature, snapshot, strippedHeaders, verifyForwarded,
} from "./fixtures/live-snapshot-peer.ts";

test("snapshot verifies untouched signed bytes and returns bounded direct v1 JSON", async () => {
  const inbound = request();
  for (const name of strippedHeaders) inbound.headers.set(name, "synthetic-caller-value");
  const json = JSON.stringify(snapshot);
  const padded = " ".repeat(LIVE_SNAPSHOT_BYTES - new TextEncoder().encode(json).byteLength) + json;
  const peer = receiver(async (forward, port) => {
    await verifyForwarded(inbound, forward, port);
    return new Response(padded, { headers: {
      "content-type": "application/json", "cache-control": "public", "set-cookie": "fixture",
    } });
  });
  const reply = await handleLiveSnapshotRelay(inbound, "fixture-valid", scope, peer);
  assert.equal(reply.status, 200);
  assert.equal(reply.headers.get("cache-control"), "no-store");
  assert.equal(reply.headers.get("set-cookie"), null);
  assert.deepEqual(await reply.json(), snapshot);
});

test("snapshot keeps absent scope, other actions, guilds, credentials and owners closed", async () => {
  let forwarded = 0;
  const peer = receiver(async () => { forwarded++; return Response.json(snapshot); });
  const wrongKey = request();
  wrongKey.headers.set("x-two-key-id", "ordinary-action");
  const duplicateSignature = request();
  duplicateSignature.headers.append("x-two-signature", signature(body));
  const oversized = request();
  oversized.headers.set("content-length", String(MAX_BODY_BYTES + 1));
  const stale = request();
  stale.headers.set(DEPLOYMENT_HEADER, peer.deploymentId);
  const cases = [
    [404, request(), undefined, peer],
    [404, request(body, origin + path + "?"), scope, peer],
    [404, request(body, "http://bot.test" + path), scope, peer],
    [400, request(body.replace("live.snapshot", "announcement.post")), scope, peer],
    [403, request(body.replace(scope.guildId, "4")), scope, peer],
    [401, wrongKey, scope, peer],
    [400, duplicateSignature, scope, peer],
    [413, oversized, scope, peer],
    [503, stale, scope, { ...peer, stampedDeploymentId: "stale" }],
    [503, request(), scope, { ...peer, running: () => false }],
  ] as const;
  for (const [status, inbound, configured, adapter] of cases) {
    assert.equal((await handleLiveSnapshotRelay(inbound, "fixture-refusals", configured, adapter)).status, status);
  }
  assert.equal(forwarded, 0);
});

test("snapshot refuses invalid, oversize and unavailable peers without logging payloads", async (t) => {
  const logs = ["log", "warn", "error"].map((method) => t.mock.method(console, method, () => {}));
  const marker = "synthetic-private-marker";
  let cancelled = false;
  const oversize = new ReadableStream<Uint8Array>({
    start(controller) { controller.enqueue(new Uint8Array(LIVE_SNAPSHOT_BYTES + 1)); },
    cancel() { cancelled = true; },
  });
  for (const serve of [
    () => Response.json({ ...snapshot, guild_id: "4" }),
    () => Response.json({ ...snapshot, seq: Number.MAX_SAFE_INTEGER + 1 }),
    () => Response.json({ ...snapshot, observed_at: "2026-02-30T00:00:00Z" }),
    () => Response.json({ ...snapshot, topics: { ...snapshot.topics, guild: { ...snapshot.topics.guild, extra: marker } } }),
    () => Response.json({ ...snapshot, topics: { ...snapshot.topics, roles: [] } }),
    () => Response.json({ ok: true, result: snapshot }),
    () => new Response(marker, { headers: { "content-type": "application/json" } }),
    () => new Response(marker, { status: 502 }),
    () => new Response(oversize, { headers: { "content-type": "application/json" } }),
    () => { throw new Error(marker); },
  ]) {
    const reply = await handleLiveSnapshotRelay(request(), "fixture-errors", scope, receiver(async () => serve()));
    assert.equal(reply.status, 503);
    assert.equal(reply.headers.get("cache-control"), "no-store");
    assert.deepEqual((await reply.json()).error, { code: "unavailable", message: "request refused", retryable: true });
  }
  assert.equal(cancelled, true);
  assert.equal(logs.reduce((calls, log) => calls + log.mock.callCount(), 0), 0);
});

test("snapshot deadline bounds a stalled reply and ordinary actions still require their envelope", async (t) => {
  const timeout = AbortSignal.timeout;
  t.mock.method(AbortSignal, "timeout", (ms: number) => {
    assert.equal(ms, LIVE_SNAPSHOT_TIMEOUT_MS);
    return timeout(25);
  });
  const stalled = new ReadableStream<Uint8Array>({ pull: () => new Promise(() => {}) });
  // Retain a referenced timer so Node waits for the unref'ed AbortSignal timer.
  const keepAlive = setTimeout(() => {}, 250);
  try {
    const peer = receiver(async () => new Response(stalled, { headers: { "content-type": "application/json" } }));
    assert.equal((await handleLiveSnapshotRelay(request(), "fixture-stall", scope, peer)).status, 503);
  } finally {
    clearTimeout(keepAlive);
  }
  const requestId = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
  assert.equal((await relayReceiverResponse(Response.json(snapshot), requestId)).status, 503);
  const ordinary = { ok: true, request_id: requestId, result: { changed: false } };
  assert.deepEqual(await (await relayReceiverResponse(Response.json(ordinary), requestId)).json(), ordinary);
});
