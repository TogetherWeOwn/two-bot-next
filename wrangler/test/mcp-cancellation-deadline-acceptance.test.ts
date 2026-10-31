/**
 * Protected failing-first acceptance: MCP refusal settlement owns its deadline;
 * stream cleanup does not. Synthetic Worker boundary only, no live services.
 *
 * Contract for the combined implementation:
 * - Intake retains its 64 KiB byte cap and whole-body 6000 ms budget. Pre-abort,
 *   cap+1, read error, expiration and client abort use the fixed parse refusal,
 *   never a container fetch, even if cancel never settles or rejects later.
 * - Container headers/body retain their separate 6000 ms outbound budget and
 *   64 KiB cap. Rejected status, streamed overflow, read error and
 *   abort/expiration keep the fixed tool failure. Intake time is NOT deducted
 *   from the outbound budget; neither budget is renewed by cleanup or chunks.
 * - Cancel is best-effort: attempt it, consume rejection, release reader locks
 *   where legal and remove timers/listeners without awaiting uncooperative
 *   cleanup again in finally. Never echo body/error/cancel/bearer sentinels.
 * - Preserve authentication-before-body, notifications, valid exact-cap input,
 *   healthy output and bearer non-forwarding. No tool/readiness surface changes.
 *
 * The regression caught here is awaiting reader/body.cancel() after a refusal
 * is decided. Real ReadableStreams have controlled underlying cancel promises;
 * only the external container boundary is stubbed. Errored native streams do
 * not call their underlying cancel hook: reader.cancel() rejects with the read
 * error instead, which must also be consumed without changing the refusal.
 * Fake clocks plus settlement snapshots (never an unbounded await) keep RED
 * runs bounded. Late-rejection fixtures are rejected only AFTER the settlement
 * assertion, including on RED via test teardown; node:test detects unhandled
 * rejections, and the explicit listener also records any observed event.
 * Run from wrangler: node --import ./test/cloudflare-loader.mjs --test test/mcp-cancellation-deadline-acceptance.test.ts
 */
import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";
import { getEventListeners } from "node:events";
import { setImmediate } from "node:timers/promises";
import worker, { type Env } from "../src/index.ts";

const CAP = 64 * 1024;
const INTAKE_MS = 6_000;
const OUTBOUND_MS = 6_000;
const TOKEN = "synthetic-mcp-cancellation-bearer-fixture";
const SENTINEL = "synthetic-private-cancellation-detail";
const PARSE_FAILURE = {
  jsonrpc: "2.0", id: null, error: { code: -32700, message: "parse error" },
};
const TOOL_FAILURE = {
  jsonrpc: "2.0", id: 17, error: { code: -32000, message: "container request failed" },
};
const TOOL_LIMIT_FAILURE = {
  jsonrpc: "2.0", id: 17, error: { code: -32000, message: "container body exceeds limit" },
};
const encode = (text: string) => new TextEncoder().encode(text);
const call = JSON.stringify({
  jsonrpc: "2.0", id: 17, method: "tools/call", params: { name: "get_status", arguments: {} },
});
let caller = 0;

function request(body: BodyInit = call, signal?: AbortSignal, auth = true): Request {
  const init: RequestInit & { duplex?: "half" } = {
    method: "POST",
    headers: {
      ...(auth ? { authorization: `Bearer ${TOKEN}` } : {}),
      "content-type": "application/json",
      "cf-connecting-ip": `192.0.2.${++caller}`,
    },
    body,
    ...(signal ? { signal } : {}),
  };
  if (body instanceof ReadableStream) init.duplex = "half";
  return new Request("https://fixture.invalid/mcp", init);
}

function harness(respond: (req: Request) => Response | Promise<Response> = () => Response.json({ ok: true })) {
  const requests: Request[] = [];
  const names: string[] = [];
  const env = {
    REDIRECT_MAPPINGS_JSON: "[]",
    CF_VERSION_METADATA: { id: "synthetic-cancellation-deployment" },
    MCP_TOKEN: TOKEN,
    TWO_BOT: {
      getByName(name: string) {
        names.push(name);
        return { fetch(req: Request) { requests.push(req); return respond(req); } };
      },
    },
  } as unknown as Env;
  const ctx = { waitUntil() {} } as unknown as ExecutionContext;
  return { requests, names, fetch: (req: Request) => worker.fetch(req, env, ctx) };
}

/** Each test drives a real stream, including an explicitly uncooperative cancel. */
function controlledBody(t: TestContext, mode: "never" | "reject-late") {
  let controller!: ReadableStreamDefaultController<Uint8Array>;
  let rejectCancel!: (error: Error) => void;
  const cancellation = new Promise<void>((_, reject) => { rejectCancel = reject; });
  const state = { pulls: 0, cancels: 0 };
  const stream = new ReadableStream<Uint8Array>({
    start(c) { controller = c; },
    pull() { state.pulls++; },
    cancel() { state.cancels++; return cancellation; },
  }, { highWaterMark: 0 });
  // Deliberately do not attach a catch here: production must consume rejection.
  // If cancel was never invoked, rejecting this unused promise would be a test
  // artifact, not a production rejection, so only reject an attempted cancel.
  if (mode === "reject-late") t.after(async () => {
    if (state.cancels > 0) rejectCancel(new Error(SENTINEL));
    await flush();
  });
  return {
    stream, state,
    enqueue: (bytes: Uint8Array) => controller.enqueue(bytes),
    close: () => controller.close(),
    error: () => controller.error(new Error(SENTINEL)),
    rejectCancel: () => rejectCancel(new Error(SENTINEL)),
  };
}

function track<T>(promise: Promise<T>) {
  const state: { settled: boolean; value?: T; error?: unknown } = { settled: false };
  void promise.then(
    (value) => { state.settled = true; state.value = value; },
    (error) => { state.settled = true; state.error = error; },
  );
  return state;
}

/** Drain stream/promise reactions only after the real auth boundary is observed. */
async function flush() {
  for (let i = 0; i < 4; i++) await setImmediate();
}

/**
 * WebCrypto completion is not bounded by a fixed number of event-loop turns.
 * Observe intake/dispatch (or a terminal response) before driving fake time.
 * The real-time bound fails a stuck fixture; it never advances either 6000 ms
 * application budget or waits for cancellation/refusal settlement after a tick.
 */
async function waitForBoundary(label: string, ready: () => boolean) {
  const deadline = performance.now() + 5_000;
  while (!ready()) {
    assert.ok(performance.now() < deadline, `timed out awaiting ${label}`);
    await setImmediate();
  }
}

/** Track live timers, not clear-call counts (a fired timer is no longer live). */
function clock(t: TestContext) {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const active = new Set<ReturnType<typeof setTimeout>>();
  const originalSet = globalThis.setTimeout;
  const originalClear = globalThis.clearTimeout;
  // Native AbortSignal.timeout uses an internal clock, not global setTimeout.
  // Model that platform signal with the same fake clock, separately from the
  // application-owned timers whose cleanup we assert. Native signal timers
  // cannot be explicitly cleared by their consumer.
  t.mock.method(AbortSignal, "timeout", (ms: number) => {
    const controller = new AbortController();
    originalSet(() => controller.abort(new DOMException("Timed out", "TimeoutError")), ms);
    return controller.signal;
  });
  t.mock.method(globalThis, "setTimeout", (fn: (...args: unknown[]) => void, ms?: number, ...args: unknown[]) => {
    const handle = originalSet(() => { active.delete(handle); fn(...args); }, ms);
    active.add(handle);
    return handle;
  });
  t.mock.method(globalThis, "clearTimeout", (handle: ReturnType<typeof setTimeout>) => {
    active.delete(handle);
    return originalClear(handle);
  });
  const unhandled: unknown[] = [];
  const onUnhandled = (error: unknown) => { unhandled.push(error); };
  process.on("unhandledRejection", onUnhandled);
  t.after(async () => {
    await flush();
    process.off("unhandledRejection", onUnhandled);
    assert.deepEqual(unhandled, [], "cleanup must not produce unhandled rejections");
  });
  return {
    tick: async (ms: number) => { t.mock.timers.tick(ms); await flush(); },
    assertClean(req: Request, streams: ReadableStream<Uint8Array>[]) {
      assert.equal(active.size, 0, "no deadline/cleanup timer remains after settlement");
      assert.equal(getEventListeners(req.signal, "abort").length, 0, "intake abort listener removed");
      for (const stream of streams) assert.equal(stream.locked, false, "reader lock released where legal");
    },
  };
}

async function refusal(state: ReturnType<typeof track<Response>>, expected: typeof PARSE_FAILURE) {
  assert.equal(state.settled, true, "fixed refusal must settle without waiting for cancellation");
  assert.equal(state.error, undefined, "cleanup must not replace refusal with an exception");
  assert.ok(state.value instanceof Response);
  assert.equal(state.value.status, 200);
  const text = await state.value.text();
  assert.ok(!text.includes(SENTINEL), "no stream/read/cancel detail echoed");
  assert.ok(!text.includes(TOKEN), "no bearer echoed");
  assert.deepEqual(JSON.parse(text), expected);
}

const intakeCases = ["pre-abort", "overflow", "read-error", "expiration", "client-abort"] as const;
for (const mode of ["never", "reject-late"] as const) {
  for (const scenario of intakeCases) {
    test(`MCP intake ${scenario}: ${mode} cancellation cannot hold the parse refusal`, async (t) => {
      const c = clock(t);
      const body = controlledBody(t, mode);
      const client = new AbortController();
      const req = request(body.stream, client.signal);
      if (scenario === "pre-abort") client.abort(new Error(SENTINEL));
      const h = harness();
      const state = track(h.fetch(req));
      await waitForBoundary("authenticated intake", () =>
        body.state.pulls > 0 || body.state.cancels > 0 || state.settled);
      if (scenario === "overflow") {
        // A complete, valid JSON call at cap+1 must be refused before parsing.
        const value = JSON.stringify({ jsonrpc: "2.0", id: 17, method: "tools/call",
          params: { name: "get_status", arguments: {}, pad: "" } });
        const bytes = encode(value.replace('"pad":""', `"pad":"${"x".repeat(CAP + 1 - value.length)}"`));
        assert.equal(bytes.byteLength, CAP + 1);
        body.enqueue(bytes.subarray(0, CAP));
        await flush();
        assert.equal(state.settled, false, "cap bytes without EOF are not themselves overflow");
        body.enqueue(bytes.subarray(CAP));
      } else if (scenario === "read-error") {
        body.enqueue(encode(SENTINEL));
        await flush();
        body.error();
      } else if (scenario === "expiration" || scenario === "client-abort") {
        // Valid JSON is not EOF: cancellation must not turn this into a forward.
        body.enqueue(encode(call));
        await flush();
        await c.tick(INTAKE_MS - 1);
        assert.equal(state.settled, false, "intake keeps the existing full deadline");
        if (scenario === "client-abort") client.abort(new Error(SENTINEL));
      }
      await flush();
      // This is an upper bound, not permission to start another cleanup budget.
      await c.tick(scenario === "expiration" || scenario === "client-abort" ? 1 : INTAKE_MS);
      assert.deepEqual(h.names, [], "intake refusal must not resolve a container");
      assert.equal(h.requests.length, 0, "intake refusal must not fetch the container");
      if (scenario !== "read-error") assert.equal(body.state.cancels, 1, "attempt best-effort cancel once");
      await refusal(state, PARSE_FAILURE);
      c.assertClean(req, [body.stream]);
      const cancels = body.state.cancels;
      if (mode === "reject-late" && cancels > 0) { body.rejectCancel(); await flush(); }
      await c.tick(INTAKE_MS * 2);
      assert.equal(body.state.cancels, cancels, "no finally/timer retry of uncooperative cleanup");
    });
  }
}

const outboundCases = ["rejected-status", "streamed-overflow", "read-error", "expiration", "aborted-headers"] as const;
for (const mode of ["never", "reject-late"] as const) {
  for (const scenario of outboundCases) {
    test(`MCP outbound ${scenario}: ${mode} cancellation cannot hold the tool failure`, async (t) => {
      const c = clock(t);
      const body = controlledBody(t, mode);
      const req = request();
      const response = () => new Response(body.stream, {
        status: scenario === "rejected-status" ? 500 : 200,
      });
      const h = harness((outgoing) => {
        if (scenario === "aborted-headers") return new Promise<Response>((resolve) => {
          outgoing.signal.addEventListener("abort", () => resolve(response()), { once: true });
        });
        return response();
      });
      const state = track(h.fetch(req));
      await waitForBoundary("authenticated tool dispatch", () => h.requests.length > 0 || state.settled);
      assert.equal(h.requests.length, 1, "one tool call makes one container request");
      assert.equal(h.requests[0]!.headers.has("authorization"), false, "bearer never forwarded");
      if (scenario === "streamed-overflow") {
        body.enqueue(encode(SENTINEL + "x".repeat(CAP + 1 - SENTINEL.length)));
      } else if (scenario === "read-error") {
        body.enqueue(encode(SENTINEL));
        await flush();
        body.error();
      } else if (scenario === "expiration" || scenario === "aborted-headers") {
        if (scenario === "expiration") body.enqueue(encode('{"detail":"' + SENTINEL));
        await c.tick(OUTBOUND_MS - 1);
        assert.equal(state.settled, false, "outbound retains its existing deadline");
      }
      await flush();
      await c.tick(scenario === "expiration" || scenario === "aborted-headers" ? 1 : OUTBOUND_MS);
      await refusal(state, scenario === "streamed-overflow" ? TOOL_LIMIT_FAILURE : TOOL_FAILURE);
      if (scenario !== "read-error") assert.equal(body.state.cancels, 1, "attempt best-effort cancel once");
      if (scenario === "rejected-status") {
        assert.equal(body.state.pulls, 0, "known refusal cancels without draining private upstream bytes");
      }
      c.assertClean(req, [body.stream]);
      const cancels = body.state.cancels;
      if (mode === "reject-late" && cancels > 0) { body.rejectCancel(); await flush(); }
      await c.tick(OUTBOUND_MS * 2);
      assert.equal(body.state.cancels, cancels, "no late cleanup retry");
    });
  }
}

test("MCP outbound late rejected status cannot start a second cleanup budget", async (t) => {
  const c = clock(t);
  const body = controlledBody(t, "never");
  let headers!: (response: Response) => void;
  const h = harness(() => new Promise<Response>((resolve) => { headers = resolve; }));
  const req = request();
  const state = track(h.fetch(req));
  await waitForBoundary("authenticated tool dispatch", () => h.requests.length > 0 || state.settled);
  await c.tick(OUTBOUND_MS - 1);
  headers(new Response(body.stream, { status: 500 }));
  await flush();
  await c.tick(1);
  await refusal(state, TOOL_FAILURE);
  assert.equal(body.state.cancels, 1);
  c.assertClean(req, [body.stream]);
});

test("MCP valid control: intake and outbound use distinct whole-body budgets", async (t) => {
  const c = clock(t);
  const intake = controlledBody(t, "never");
  const output = controlledBody(t, "never");
  const h = harness(() => new Response(output.stream));
  const req = request(intake.stream);
  const state = track(h.fetch(req));
  await waitForBoundary("authenticated intake", () => intake.state.pulls > 0 || state.settled);
  intake.enqueue(encode(call));
  await c.tick(INTAKE_MS - 1);
  assert.equal(state.settled, false);
  assert.equal(h.requests.length, 0, "no tool fetch until intake EOF");
  intake.close();
  await waitForBoundary("tool dispatch after intake EOF", () => h.requests.length > 0 || state.settled);
  assert.equal(h.requests.length, 1);
  await c.tick(OUTBOUND_MS - 1);
  assert.equal(state.settled, false, "outbound has its own budget after slow valid intake");
  output.enqueue(encode('{"ok":true}'));
  output.close();
  await flush();
  assert.equal(state.settled, true, "healthy output succeeds within its separate budget");
  assert.equal(state.error, undefined);
  assert.ok(state.value);
  const payload = await state.value.json() as { id: number; error?: unknown; result: { structuredContent: unknown } };
  assert.equal(payload.id, 17);
  assert.equal(payload.error, undefined);
  assert.deepEqual(payload.result.structuredContent, { ok: true });
  assert.equal(h.requests[0]!.headers.has("authorization"), false);
  assert.equal(intake.state.cancels + output.state.cancels, 0, "healthy bodies are not cancelled");
  c.assertClean(req, [intake.stream, output.stream]);
  await c.tick(INTAKE_MS + OUTBOUND_MS);
  assert.equal(intake.state.cancels + output.state.cancels, 0, "no leaked timer cancels healthy bodies");
});

test("MCP valid control: exact-cap intake and exact-cap healthy output remain accepted", async (t) => {
  const c = clock(t);
  const intake = controlledBody(t, "never");
  const output = controlledBody(t, "never");
  const padded = (text: string) => encode(text + " ".repeat(CAP - encode(text).byteLength));
  const h = harness(() => new Response(output.stream));
  const req = request(intake.stream);
  const state = track(h.fetch(req));
  intake.enqueue(padded(call));
  intake.close();
  await waitForBoundary("authenticated exact-cap dispatch", () => h.requests.length > 0 || state.settled);
  assert.equal(h.requests.length, 1, "exact-cap request is accepted");
  output.enqueue(padded('{"ok":true}'));
  output.close();
  await flush();
  assert.equal(state.settled, true);
  assert.equal(state.error, undefined);
  assert.ok(state.value);
  const payload = await state.value.json() as { error?: unknown; result: { structuredContent: unknown } };
  assert.equal(payload.error, undefined);
  assert.deepEqual(payload.result.structuredContent, { ok: true });
  c.assertClean(req, [intake.stream, output.stream]);
});

test("MCP auth control: missing bearer refuses without pulling a hostile body", async (t) => {
  const c = clock(t);
  const body = controlledBody(t, "never");
  const h = harness();
  const req = request(body.stream, undefined, false);
  const state = track(h.fetch(req));
  await waitForBoundary("authentication refusal", () => state.settled);
  assert.equal(state.settled, true);
  assert.equal(state.error, undefined);
  assert.ok(state.value);
  assert.equal(state.value.status, 401);
  assert.match(state.value.headers.get("www-authenticate") ?? "", /Bearer/);
  assert.equal(body.state.pulls, 0, "authentication precedes body reads");
  assert.equal(h.requests.length, 0);
  assert.deepEqual(h.names, []);
  c.assertClean(req, [body.stream]);
});

test("MCP notification control: valid notification keeps empty 202 acknowledgement", async (t) => {
  const c = clock(t);
  const h = harness();
  const req = request(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }));
  const state = track(h.fetch(req));
  await waitForBoundary("authenticated notification acknowledgement", () => state.settled);
  assert.equal(state.settled, true);
  assert.equal(state.error, undefined);
  assert.ok(state.value);
  assert.equal(state.value.status, 202);
  assert.equal(await state.value.text(), "");
  assert.deepEqual(h.names, []);
  assert.equal(h.requests.length, 0);
  c.assertClean(req, []);
});
