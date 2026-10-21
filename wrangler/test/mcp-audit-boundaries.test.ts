import { test } from "node:test";
import assert from "node:assert/strict";
import { handleMcp, type McpEnv } from "../src/mcp.ts";

const row = {
  entry_id: "synthetic-entry", kind: "voice_join", guild_id: "424242",
  occurred_at: "2026-10-19T00:00:00.000Z", actor_id: null, target_id: "111111",
  source_channel_id: null, destination_channel_id: "222222", message_id: null,
  action: null, delivery_state: "none",
};
const excerpt = { available: true, rows: [row], truncated: false };

async function call(respond: () => Response, signal?: AbortSignal) {
  const env = {
    CF_VERSION_METADATA: { id: "synthetic-version" },
    TWO_BOT: { getByName: () => ({ fetch: async () => respond() }) },
  } as unknown as McpEnv;
  return handleMcp(new Request("https://probe.invalid/mcp", {
    method: "POST", signal,
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "get_audit" } }),
  }), env);
}

test("audit proxy rejects schema drift, private columns and invalid field types", async () => {
  const marker = "synthetic-private-marker";
  for (const data of [
    { ...excerpt, metadata: marker },
    { ...excerpt, rows: [{ ...row, token: marker }] },
    { ...excerpt, rows: [{ ...row, action: { message: marker } }] },
    { ...excerpt, rows: [{ ...row, entry_id: null }] },
    { ...excerpt, rows: [{ ...row, occurred_at: marker.repeat(20) }] },
    { ...excerpt, rows: [{ ...row, kind: "line\nbreak" }] },
    { ...excerpt, rows: Array.from({ length: 51 }, () => row) },
    { ...excerpt, available: false },
    { available: false, rows: [], truncated: true },
    { rows: [], truncated: false },
    { ...excerpt, truncated: "false" },
  ]) {
    const response = await call(() => Response.json(data));
    const text = await response.text();
    const body = JSON.parse(text);
    assert.equal(body.error?.code, -32000);
    assert.equal(body.result, undefined);
    assert.ok(!text.includes(marker));
  }
});

test("audit proxy pins the Rust producer's millisecond UTC timestamp format", async () => {
  for (const occurred_at of ["2026-10-19T00:00:02.000Z", "2026-10-19T00:00:01.123Z"]) {
    const data = { ...excerpt, rows: [{ ...row, occurred_at }] };
    const body = await (await call(() => Response.json(data))).json() as any;
    assert.deepEqual(body.result?.structuredContent, data);
    assert.equal(body.error, undefined);
  }
  for (const occurred_at of [
    "2026-10-19T00:00:02Z",
    "2026-10-19T00:00:01.12Z",
    "2026-10-19T00:00:01.123+00:00",
    "2026-10-19T00:00:01.123",
  ]) {
    const data = { ...excerpt, rows: [{ ...row, occurred_at }] };
    const body = await (await call(() => Response.json(data))).json() as any;
    assert.equal(body.error?.code, -32000);
    assert.equal(body.result, undefined);
  }
});

test("audit proxy requires JSON on an exact 200, including unavailable answers", async () => {
  for (const response of [
    Response.json(excerpt, { status: 503 }),
    Response.json(excerpt, { status: 201 }),
    new Response(JSON.stringify(excerpt), { headers: { "content-type": "text/plain" } }),
    new Response("{bad json", { headers: { "content-type": "application/json" } }),
  ]) {
    const body = await (await call(() => response)).json() as any;
    assert.equal(body.error?.code, -32000);
    assert.equal(body.result, undefined);
  }
});

test("audit proxy cancels an oversize stream regardless of Content-Length", async () => {
  let cancelled = false;
  const stream = new ReadableStream<Uint8Array>({
    start(controller) { controller.enqueue(new Uint8Array(64 * 1024 + 1)); },
    cancel() { cancelled = true; },
  });
  const body = await (await call(() => new Response(stream, {
    headers: { "content-type": "application/json", "content-length": "2" },
  }))).json() as any;
  assert.equal(body.error?.code, -32000);
  assert.equal(body.result, undefined);
  assert.equal(cancelled, true);
});

test("audit proxy fails closed and cancels on a client abort after valid JSON bytes", async () => {
  const controller = new AbortController();
  let cancelled = false;
  const response = call(() => {
    const stream = new ReadableStream<Uint8Array>({
      start(writer) {
        writer.enqueue(new TextEncoder().encode(JSON.stringify(excerpt)));
        setTimeout(() => controller.abort(), 20);
      },
      cancel() { cancelled = true; },
    });
    return new Response(stream, { headers: { "content-type": "application/json" } });
  }, controller.signal);
  const body = await (await response).json() as any;
  assert.equal(body.error?.code, -32000);
  assert.equal(body.result, undefined);
  assert.equal(cancelled, true);
});

test("audit proxy deadline covers a stalled body and never waits for hanging cleanup", { timeout: 1000 }, async (t) => {
  const controller = new AbortController();
  t.mock.method(AbortSignal, "timeout", (milliseconds: number) => {
    assert.equal(milliseconds, 6000);
    setTimeout(() => controller.abort(), 30);
    return controller.signal;
  });
  let cancelled = false;
  const response = await call(() => new Response(new ReadableStream({
    start(writer) { writer.enqueue(new TextEncoder().encode(JSON.stringify(excerpt))); },
    cancel() { cancelled = true; return new Promise<void>(() => {}); },
  }), { headers: { "content-type": "application/json" } }));
  const body = await response.json() as any;
  assert.equal(body.error?.code, -32000);
  assert.equal(body.result, undefined);
  assert.equal(cancelled, true);
});

test("audit proxy fails closed on a body read error without echoing its cause", async () => {
  const response = await call(() => new Response(new ReadableStream({
    start(writer) { writer.error(new Error("synthetic-private-marker")); },
  }), { headers: { "content-type": "application/json" } }));
  const text = await response.text();
  assert.ok(!text.includes("synthetic-private-marker"));
  assert.equal(JSON.parse(text).error?.code, -32000);
});
