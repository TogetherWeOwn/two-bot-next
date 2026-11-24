import assert from "node:assert/strict";
import { test } from "node:test";
import { MAX_BODY_BYTES, relayReceiverResponse } from "../src/internal-actions.ts";
import {
  handleLiveSnapshotRelay, LIVE_SNAPSHOT_BYTES, LIVE_SNAPSHOT_TIMEOUT_MS,
} from "../src/live-snapshot-relay.ts";
import { DEPLOYMENT_HEADER } from "../src/ownership.ts";
import {
  body, invalidSnapshots, origin, path, receiver, request, scope, signature, snapshot,
  stalledBody, stalledUpload, strippedHeaders, verifyForwarded, webSnapshot,
} from "./fixtures/live-snapshot-peer.ts";

test("snapshot verifies untouched signed bytes and returns bounded direct v1 JSON", async () => {
  assert.equal(LIVE_SNAPSHOT_BYTES, webSnapshot.responseBytes);
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
    ...invalidSnapshots.map((value) => () => Response.json(value)),
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

test("snapshot deadline bounds a stalled reply and ordinary actions still require their envelope", { timeout: 1_000 }, async (t) => {
  assert.ok(LIVE_SNAPSHOT_TIMEOUT_MS < webSnapshot.deadlineMs);
  const timeout = AbortSignal.timeout;
  t.mock.method(AbortSignal, "timeout", (ms: number) => {
    assert.equal(ms, LIVE_SNAPSHOT_TIMEOUT_MS);
    return timeout(25);
  });
  // Retain a referenced timer so Node waits for the unref'ed AbortSignal timer.
  const keepAlive = setTimeout(() => {}, 250);
  try {
    const peer = receiver(async () => new Response(stalledBody(), { headers: { "content-type": "application/json" } }));
    assert.equal((await handleLiveSnapshotRelay(stalledUpload(), "fixture-stall", scope, peer)).status, 408);
    assert.equal((await handleLiveSnapshotRelay(request(), "fixture-stall", scope, peer)).status, 503);
  } finally {
    clearTimeout(keepAlive);
  }
  const requestId = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
  assert.equal((await relayReceiverResponse(Response.json(snapshot), requestId)).status, 503);
  const ordinary = { ok: true, request_id: requestId, result: { changed: false } };
  assert.deepEqual(await (await relayReceiverResponse(Response.json(ordinary), requestId)).json(), ordinary);
});
