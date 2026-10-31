/**
 * Protected acceptance: EventSub receiver response admission and projection.
 *
 * Fixtures come from crates/bot/src/eventsub_http.rs: envelope/refuse/ack and
 * their status/code/retryable call sites, not the older abbreviated Worker
 * fixtures. Challenges follow twitch_eventsub::classify: 1..256 ASCII-graphic
 * bytes. Request ids follow the actual 26-character Crockford contract (not a
 * stricter generic ULID parser) and must join the request sent to the receiver.
 *
 * Breaks caught: raw JSON reflection, arbitrary receiver ids/error vocabulary,
 * invented success statuses, relaxed challenge validation, drain-before-cap,
 * renewed body deadlines, blocking cancellation and lost response hardening.
 * These are schema/outflow tests, NOT proof of upstream authenticity.
 *
 * Only containerFetch is replaced in the route tests, past SDK 0.3.7's proxy
 * pipe (whose early-cancel rejection is unhandled under Node). Ownership, DO
 * routing, ingress validation, correlation and the relay remain real. No live
 * Worker, TCP, Discord, Twitch, database service or credentials are used.
 * Run from repo root:
 * node --import ./wrangler/test/cloudflare-loader.mjs --test wrangler/test/eventsub-response-contract-acceptance.test.ts
 */
import assert from "node:assert/strict";
import { test, type TestContext } from "node:test";
import { DatabaseSync } from "node:sqlite";
import { format } from "node:util";
import worker, { TwoBotContainer, type Env } from "../src/index.ts";
import { DEPLOYMENT_HEADER, OWNER_KEY } from "../src/ownership.ts";
import { relayEventsubResponse, resetEventsubIngressLimits } from "../src/eventsub.ts";

const REQUEST_ID = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER_ID = "01ARZ3NDEKTSV4RRFFQ69G5FAW";
const PRIVATE = "synthetic-private-receiver-outflow";
const ACK = { ok: true, request_id: REQUEST_ID };
const UNAVAILABLE = {
  ok: false, request_id: REQUEST_ID,
  error: { code: "unavailable", message: "request refused", retryable: true },
};
const encoder = new TextEncoder();

// Exact combinations emitted by the receiver, including retryable 408 (not
// just 4xx=false / 5xx=true). Bad-signature/stale-timestamp map to unauthorized
// there; they are NOT receiver wire error codes.
const REFUSALS = [
  { status: 400, code: "malformed", retryable: false },
  { status: 401, code: "unauthorized", retryable: false },
  { status: 404, code: "not_found", retryable: false },
  { status: 405, code: "method_not_allowed", retryable: false },
  { status: 408, code: "request_timeout", retryable: true },
  { status: 413, code: "payload_too_large", retryable: false },
  { status: 415, code: "unsupported_media_type", retryable: false },
  { status: 503, code: "busy", retryable: true },
  { status: 503, code: "upstream_timeout", retryable: true },
  { status: 503, code: "unavailable", retryable: true },
] as const;

function refusal(code: string, retryable: boolean, requestId = REQUEST_ID) {
  return { ok: false, request_id: requestId, error: { code, message: "request refused", retryable } };
}

function captureLogs(t: TestContext): string[] {
  const logs: string[] = [];
  for (const method of ["log", "info", "warn", "error", "debug"] as const) {
    t.mock.method(console, method, (...args: unknown[]) => { logs.push(format(...args)); });
  }
  return logs;
}

async function observed(response: Response, logs: string[], forbidden = [PRIVATE]) {
  const body = await response.text();
  const headers = JSON.stringify(Object.fromEntries(response.headers));
  for (const sentinel of forbidden) {
    assert.ok(!body.includes(sentinel), "upstream private bytes must not reach the response body");
    assert.ok(!headers.includes(sentinel), "upstream private bytes must not reach response headers");
    assert.ok(!logs.join("\n").includes(sentinel), "upstream private bytes must not reach logs");
  }
  assert.equal(response.headers.get("cache-control"), "no-store");
  return body;
}

async function assertUnavailable(response: Response, logs: string[], forbidden = [PRIVATE]) {
  const body = await observed(response, logs, forbidden);
  assert.equal(response.status, 503, "unsupported receiver content must fail closed, not acknowledge");
  assert.equal(response.headers.get("content-type")?.split(";")[0], "application/json");
  assert.deepEqual(JSON.parse(body), UNAVAILABLE, "only the fixed refusal with the local request id may leave");
}

function json(value: unknown, status = 200, headers: HeadersInit = {}) {
  return Response.json(value, { status, headers });
}

async function within<T>(work: Promise<T>): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const guard = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error("relay held the caller beyond the 1000ms fixture guard")), 1000);
  });
  try { return await Promise.race([work, guard]); }
  finally { clearTimeout(timer); }
}

test("receiver acknowledgement retains its exact meaning and request correlation", async (t) => {
  const logs = captureLogs(t);
  const response = await relayEventsubResponse(json(ACK), REQUEST_ID);
  assert.equal(response.status, 200);
  assert.deepEqual(JSON.parse(await observed(response, logs)), ACK);
});

test("acknowledgement projects away private extra fields instead of relaying raw JSON", async (t) => {
  const logs = captureLogs(t);
  const response = await relayEventsubResponse(json({ ...ACK, private: PRIVATE, result: { detail: PRIVATE } }), REQUEST_ID);
  assert.equal(response.status, 200);
  assert.deepEqual(JSON.parse(await observed(response, logs)), ACK);
});

for (const fixture of REFUSALS) {
  test(`receiver ${fixture.status} ${fixture.code} retains status, fixed message and retry policy`, async (t) => {
    const logs = captureLogs(t);
    const expected = refusal(fixture.code, fixture.retryable);
    const response = await relayEventsubResponse(json(expected, fixture.status), REQUEST_ID);
    assert.equal(response.status, fixture.status);
    assert.deepEqual(JSON.parse(await observed(response, logs)), expected);
  });

  test(`receiver ${fixture.code} projects hostile text and extra fields without changing its verdict`, async (t) => {
    const logs = captureLogs(t);
    const response = await relayEventsubResponse(json({
      ...refusal(fixture.code, fixture.retryable), private: PRIVATE,
      error: { code: fixture.code, retryable: fixture.retryable, message: PRIVATE, detail: PRIVATE },
    }, fixture.status), REQUEST_ID);
    assert.equal(response.status, fixture.status);
    assert.deepEqual(JSON.parse(await observed(response, logs)), refusal(fixture.code, fixture.retryable));
  });
}

const INVALID_JSON = [
  { name: "empty bytes", body: "" },
  { name: "whitespace only", body: " \r\n\t" },
  { name: "truncated JSON", body: '{"ok":true,' },
  { name: "trailing private text", body: `${JSON.stringify(ACK)}${PRIVATE}` },
  { name: "null", body: "null" },
  { name: "boolean", body: "true" },
  { name: "number", body: "200" },
  { name: "private string", body: JSON.stringify(PRIVATE) },
  { name: "array", body: JSON.stringify([ACK]) },
  { name: "empty object", body: "{}" },
];
for (const fixture of INVALID_JSON) {
  test(`JSON ${fixture.name} is not a receiver envelope`, async (t) => {
    const logs = captureLogs(t);
    await assertUnavailable(await relayEventsubResponse(new Response(fixture.body, {
      status: 200, headers: { "content-type": "application/json" },
    }), REQUEST_ID), logs);
  });
}

test("JSON media with no body is not an empty successful acknowledgement", async (t) => {
  const logs = captureLogs(t);
  await assertUnavailable(await relayEventsubResponse(new Response(null, {
    status: 200, headers: { "content-type": "application/json" },
  }), REQUEST_ID), logs);
});

const BAD_ENVELOPES: { name: string; status: number; value: unknown }[] = [
  { name: "missing ok", status: 200, value: { request_id: REQUEST_ID } },
  { name: "string ok", status: 200, value: { ...ACK, ok: "true" } },
  { name: "numeric ok", status: 200, value: { ...ACK, ok: 1 } },
  { name: "success plus error", status: 200, value: { ...ACK, error: UNAVAILABLE.error } },
  { name: "success plus null error", status: 200, value: { ...ACK, error: null } },
  { name: "success in 201", status: 201, value: ACK },
  { name: "success in 202", status: 202, value: ACK },
  { name: "success in 401", status: 401, value: ACK },
  { name: "success in 503", status: 503, value: ACK },
  { name: "refusal in 200", status: 200, value: refusal("unauthorized", false) },
  { name: "refusal in 302", status: 302, value: refusal("unauthorized", false) },
  { name: "refusal in 500", status: 500, value: UNAVAILABLE },
  { name: "wrong status for known code", status: 403, value: refusal("unauthorized", false) },
  { name: "missing error", status: 401, value: { ok: false, request_id: REQUEST_ID } },
  { name: "string error", status: 401, value: { ok: false, request_id: REQUEST_ID, error: PRIVATE } },
  { name: "missing code", status: 401, value: { ok: false, request_id: REQUEST_ID, error: { message: "request refused", retryable: false } } },
  { name: "unknown private code", status: 503, value: refusal(PRIVATE, true) },
  { name: "domain-only bad_signature code", status: 401, value: refusal("bad_signature", false) },
  { name: "domain-only stale_timestamp code", status: 403, value: refusal("stale_timestamp", false) },
  { name: "inherited-name code", status: 503, value: refusal("toString", true) },
  { name: "overlong code", status: 503, value: refusal("unavailable".repeat(100), true) },
  { name: "false retry policy for busy", status: 503, value: refusal("busy", false) },
  { name: "true retry policy for unauthorized", status: 401, value: refusal("unauthorized", true) },
  { name: "false retry policy for request_timeout", status: 408, value: refusal("request_timeout", false) },
  { name: "missing retry policy", status: 503, value: { ok: false, request_id: REQUEST_ID, error: { code: "busy", message: "request refused" } } },
  { name: "string retry policy", status: 503, value: { ...UNAVAILABLE, error: { ...UNAVAILABLE.error, retryable: "true" } } },
];
for (const fixture of BAD_ENVELOPES) {
  test(`${fixture.name} fails closed instead of inventing receiver semantics`, async (t) => {
    const logs = captureLogs(t);
    await assertUnavailable(await relayEventsubResponse(json(fixture.value, fixture.status), REQUEST_ID), logs);
  });
}

const BAD_IDS: { name: string; value: unknown }[] = [
  { name: "absent", value: undefined },
  { name: "null", value: null },
  { name: "numeric", value: 123 },
  { name: "empty", value: "" },
  { name: "short", value: REQUEST_ID.slice(1) },
  { name: "long", value: `${REQUEST_ID}0` },
  { name: "lowercase", value: REQUEST_ID.toLowerCase() },
  { name: "excluded Crockford character", value: `${REQUEST_ID.slice(0, -1)}I` },
  { name: "control character", value: `${REQUEST_ID.slice(0, -1)}\n` },
  { name: "non-ASCII character", value: `${REQUEST_ID.slice(0, -1)}é` },
  { name: "private text", value: PRIVATE },
  { name: "valid but mismatched", value: OTHER_ID },
];
for (const fixture of BAD_IDS) {
  for (const ok of [true, false]) {
    test(`${ok ? "ack" : "refusal"} with ${fixture.name} receiver id cannot echo it`, async (t) => {
      const logs = captureLogs(t);
      const value = { ...(ok ? ACK : refusal("unauthorized", false)), request_id: fixture.value };
      await assertUnavailable(await relayEventsubResponse(json(value, ok ? 200 : 401), REQUEST_ID), logs,
        fixture.value === OTHER_ID ? [PRIVATE, OTHER_ID] : [PRIVATE]);
    });
  }
}

test("correlation accepts the actual Crockford contract, not a stricter ULID first-character rule", async (t) => {
  const logs = captureLogs(t);
  const id = "Z".repeat(26);
  const response = await relayEventsubResponse(json({ ok: true, request_id: id }), id);
  assert.equal(response.status, 200);
  assert.deepEqual(JSON.parse(await observed(response, logs)), { ok: true, request_id: id });
});

for (const challenge of ["!", "~", "synthetic-challenge-0001", "!~".repeat(128)]) {
  test(`ASCII-graphic challenge of ${challenge.length} bytes starting at 0x${challenge.charCodeAt(0).toString(16)} is echoed byte-exact`, async (t) => {
    const logs = captureLogs(t);
    const bytes = encoder.encode(challenge);
    const response = await relayEventsubResponse(new Response(bytes, {
      status: 200, headers: { "content-type": "text/plain" },
    }), REQUEST_ID);
    assert.equal(response.status, 200);
    assert.equal(response.headers.get("content-type"), "text/plain");
    assert.equal(response.headers.get("cache-control"), "no-store");
    assert.deepEqual(new Uint8Array(await response.arrayBuffer()), bytes);
    assert.ok(!logs.join("\n").includes(challenge), "even valid challenge bytes must not be logged");
  });
}

const BAD_CHALLENGES = [
  { name: "empty", bytes: new Uint8Array() },
  { name: "257 ASCII bytes", bytes: encoder.encode("x".repeat(257)) },
  { name: "space", bytes: encoder.encode(`${PRIVATE} `) },
  { name: "tab", bytes: encoder.encode(`${PRIVATE}\t`) },
  { name: "newline", bytes: encoder.encode(`${PRIVATE}\n`) },
  { name: "carriage return", bytes: encoder.encode(`${PRIVATE}\r`) },
  { name: "NUL", bytes: encoder.encode(`${PRIVATE}\0`) },
  { name: "DEL", bytes: encoder.encode(`${PRIVATE}\x7f`) },
  { name: "non-ASCII UTF-8", bytes: encoder.encode(`${PRIVATE}é`) },
  { name: "invalid UTF-8 octet", bytes: new Uint8Array([...encoder.encode(PRIVATE), 0xff]) },
];
for (const fixture of BAD_CHALLENGES) {
  test(`${fixture.name} challenge is a fixed refusal without reflection`, async (t) => {
    const logs = captureLogs(t);
    await assertUnavailable(await relayEventsubResponse(new Response(fixture.bytes, {
      status: 200, headers: { "content-type": "text/plain" },
    }), REQUEST_ID), logs);
  });
}

for (const media of ["application/json", "text/plain"]) {
  test(`${media} keeps only safe headers and locally stamps security headers`, async (t) => {
    const logs = captureLogs(t);
    const response = await relayEventsubResponse(new Response(media === "application/json" ? JSON.stringify(ACK) : "safe-challenge", {
      status: 200, headers: {
        "content-type": media, "cache-control": "public, max-age=3600", "retry-after": "7",
        "set-cookie": `private=${PRIVATE}`, location: `https://example.invalid/${PRIVATE}`,
        "x-private": PRIVATE, "content-length": "9999",
        "access-control-allow-origin": "*", "content-encoding": "gzip",
        "x-content-type-options": PRIVATE, "referrer-policy": PRIVATE, "x-frame-options": PRIVATE,
      },
    }), REQUEST_ID);
    await observed(response, logs);
    assert.equal(response.status, 200);
    assert.equal(response.headers.get("retry-after"), "7");
    for (const name of ["set-cookie", "location", "x-private", "content-length", "access-control-allow-origin", "content-encoding"]) {
      assert.equal(response.headers.get(name), null, `${name} is not allowlisted`);
    }
    assert.equal(response.headers.get("x-content-type-options"), "nosniff");
    assert.equal(response.headers.get("referrer-policy"), "no-referrer");
    assert.equal(response.headers.get("x-frame-options"), "DENY");
  });
}

test("fixed response-boundary refusal retains locally chosen security headers", async (t) => {
  const logs = captureLogs(t);
  const response = await relayEventsubResponse(new Response(PRIVATE, {
    status: 500, headers: { "content-type": "text/html", "x-frame-options": PRIVATE },
  }), REQUEST_ID);
  await assertUnavailable(response, logs);
  assert.equal(response.headers.get("x-content-type-options"), "nosniff");
  assert.equal(response.headers.get("referrer-policy"), "no-referrer");
  assert.equal(response.headers.get("x-frame-options"), "DENY");
});

for (const retry of ["7.5", "-1", "1234567", "Wed, 21 Oct 2026 07:28:00 GMT", PRIVATE]) {
  test(`retry-after ${retry} is not reflected`, async (t) => {
    const logs = captureLogs(t);
    const response = await relayEventsubResponse(json(refusal("busy", true), 503, { "retry-after": retry }), REQUEST_ID);
    assert.equal(response.status, 503);
    assert.equal(response.headers.get("retry-after"), null);
    assert.deepEqual(JSON.parse(await observed(response, logs)), refusal("busy", true));
  });
}

function chunked(chunks: Uint8Array[], onCancel: () => unknown = () => {}) {
  let pulls = 0;
  const stream = new ReadableStream<Uint8Array>({
    pull(controller) {
      const chunk = chunks[pulls++];
      if (chunk) controller.enqueue(chunk);
      else controller.close();
    },
    cancel: onCancel,
  }, { highWaterMark: 0 });
  return { stream, pulls: () => pulls };
}

test("valid JSON split across chunks is projected only after the full object arrives", async (t) => {
  const logs = captureLogs(t);
  const input = chunked([encoder.encode('{"ok":true,"request_'), encoder.encode(`id":"${REQUEST_ID}"}`)]);
  const response = await relayEventsubResponse(new Response(input.stream, {
    headers: { "content-type": "application/json" },
  }), REQUEST_ID);
  assert.equal(response.status, 200);
  assert.deepEqual(JSON.parse(await observed(response, logs)), ACK);
});

test("JSON response at the existing 65536-byte cap is still admitted when its envelope is valid", async (t) => {
  const logs = captureLogs(t);
  const body = JSON.stringify(ACK);
  const response = await relayEventsubResponse(new Response(body + " ".repeat(65536 - body.length), {
    headers: { "content-type": "application/json" },
  }), REQUEST_ID);
  assert.equal(response.status, 200);
  assert.deepEqual(JSON.parse(await observed(response, logs)), ACK);
});

for (const fixture of [
  { media: "text/plain", first: encoder.encode("x".repeat(256)) },
  { media: "application/json", first: encoder.encode(JSON.stringify(ACK).padEnd(65536, " ")) },
]) {
  test(`${fixture.media} cancels incrementally on the first over-cap chunk without draining its tail`, async (t) => {
    const logs = captureLogs(t);
    let cancelled = 0;
    const input = chunked([fixture.first, encoder.encode("x"), encoder.encode(PRIVATE)], () => {
      cancelled++;
      return new Promise<void>(() => {});
    });
    const response = await within(relayEventsubResponse(new Response(input.stream, {
      headers: { "content-type": fixture.media },
    }), REQUEST_ID));
    await assertUnavailable(response, logs);
    assert.equal(input.pulls(), 2, "no tail read after crossing the byte cap");
    assert.equal(cancelled, 1, "cancellation is attempted but its never-settling promise is not awaited");
  });
}

test("wrong media cancels without reading even when cancellation never settles", async (t) => {
  const logs = captureLogs(t);
  let cancelled = 0;
  const input = chunked([encoder.encode(PRIVATE)], () => { cancelled++; return new Promise<void>(() => {}); });
  const response = await within(relayEventsubResponse(new Response(input.stream, {
    status: 500, headers: { "content-type": "text/html" },
  }), REQUEST_ID));
  await assertUnavailable(response, logs);
  assert.equal(input.pulls(), 0);
  assert.equal(cancelled, 1);
});

for (const failure of ["throw", "reject"] as const) {
  test(`cancel ${failure} never escapes as private text or an unhandled rejection`, async (t) => {
    const logs = captureLogs(t);
    const input = chunked([encoder.encode(PRIVATE)], () => {
      if (failure === "throw") throw new Error(PRIVATE);
      return Promise.reject(new Error(PRIVATE));
    });
    await assertUnavailable(await within(relayEventsubResponse(new Response(input.stream, {
      status: 500, headers: { "content-type": "text/html" },
    }), REQUEST_ID)), logs);
    assert.equal(input.pulls(), 0);
    await new Promise<void>((resolve) => setImmediate(resolve));
    assert.ok(!logs.join("\n").includes(PRIVATE));
  });
}

test("a stream error discards a valid-looking prefix and its private exception", async (t) => {
  const logs = captureLogs(t);
  let pulls = 0;
  const stream = new ReadableStream<Uint8Array>({
    pull(controller) {
      if (pulls++ === 0) controller.enqueue(encoder.encode(JSON.stringify(ACK)));
      else controller.error(new Error(PRIVATE));
    },
  }, { highWaterMark: 0 });
  await assertUnavailable(await relayEventsubResponse(new Response(stream, {
    headers: { "content-type": "application/json" },
  }), REQUEST_ID), logs);
});

// Route-level proof: changing only an exported relay helper must not leave the
// actual DO or public ingress on the old raw-byte path.
async function routeHarness(t: TestContext) {
  const logs = captureLogs(t);
  resetEventsubIngressLimits();
  t.after(() => resetEventsubIngressLimits());
  const database = new DatabaseSync(":memory:");
  t.after(() => database.close());
  const id = "synthetic-response-contract-deployment";
  const values = new Map<string, unknown>([[OWNER_KEY, {
    deploymentId: id, epoch: 1, phase: "active", actor: "fixture-operator",
    timestamp: "2026-10-01T00:00:00.000Z", oldEpoch: 0, oldDeploymentId: null,
  }]]);
  const gates: Promise<unknown>[] = [];
  const ctx = {
    exports: { ContainerProxy: () => ({ fetch() { throw new Error("unexpected outbound request"); } }) },
    id: { toString: () => "synthetic-response-contract-do" },
    container: {
      running: true, start() {}, async destroy() {}, monitor: () => new Promise<void>(() => {}),
      getTcpPort() { throw new Error("fixture must not contact TCP"); },
    },
    storage: {
      get: async (key: string) => values.get(key),
      put: async (key: string | Record<string, unknown>, value?: unknown) => {
        if (typeof key === "string") values.set(key, value);
        else for (const [k, v] of Object.entries(key)) values.set(k, v);
      },
      delete: async (key: string) => values.delete(key),
      kv: { get: (key: string) => values.get(key), put: (key: string, value: unknown) => { values.set(key, value); }, delete: (key: string) => values.delete(key) },
      sql: { exec: (query: string, ...bindings: (string | number | null)[]) => database.prepare(query).all(...bindings) },
      setAlarm: async () => {}, deleteAlarm: async () => {}, sync: async () => {},
    },
    blockConcurrencyWhile<T>(callback: () => Promise<T>) { const promise = callback(); gates.push(promise); return promise; },
  };
  const env = { CF_VERSION_METADATA: { id }, EVENTSUB_INGRESS: "1", TWO_EVENTSUB: "1" } as unknown as Env;
  const bot = new TwoBotContainer(ctx as unknown as DurableObjectState<{}>, env);
  await Promise.all(gates);
  env.TWO_BOT = { getByName: () => bot } as unknown as Env["TWO_BOT"];
  const request = () => new Request("https://worker.invalid/hooks/twitch", {
    method: "POST", body: '{"subscription":{"type":"stream.online","version":"1"},"event":{}}',
    headers: {
      [DEPLOYMENT_HEADER]: id, "x-request-id": REQUEST_ID, "cf-connecting-ip": "192.0.2.170",
      "content-type": "application/json", "twitch-eventsub-message-id": "synthetic-response-delivery",
      "twitch-eventsub-message-timestamp": "2026-10-13T00:00:00.000Z",
      "twitch-eventsub-message-signature": `sha256=${"b".repeat(64)}`,
      "twitch-eventsub-message-type": "notification",
    },
  });
  return { bot, logs, request, edge: () => worker.fetch(request(), env, {} as ExecutionContext) };
}

for (const route of ["DO", "edge"] as const) {
  test(`${route} route projects the receiver acknowledgement, not only the standalone helper`, async (t) => {
    const h = await routeHarness(t);
    t.mock.method(h.bot, "containerFetch", async () => json({ ...ACK, private: PRIVATE }));
    const response = await (route === "DO" ? h.bot.fetch(h.request()) : h.edge());
    assert.equal(response.status, 200);
    assert.deepEqual(JSON.parse(await observed(response, h.logs)), ACK);
  });
}

for (const route of ["DO", "edge"] as const) {
  test(`${route} route keeps a meaningful receiver 408 and its retry policy`, async (t) => {
    const h = await routeHarness(t);
    t.mock.method(h.bot, "containerFetch", async () => json(refusal("request_timeout", true), 408));
    const response = await (route === "DO" ? h.bot.fetch(h.request()) : h.edge());
    assert.equal(response.status, 408);
    assert.deepEqual(JSON.parse(await observed(response, h.logs)), refusal("request_timeout", true));
  });

  test(`${route} route refuses a foreign receiver correlation instead of echoing it`, async (t) => {
    const h = await routeHarness(t);
    t.mock.method(h.bot, "containerFetch", async () => json({ ok: true, request_id: OTHER_ID }));
    const response = await (route === "DO" ? h.bot.fetch(h.request()) : h.edge());
    await assertUnavailable(response, h.logs, [PRIVATE, OTHER_ID]);
  });
}

test("DO uses the one existing 20000ms deadline through body admission, then accepts the next delivery", async (t) => {
  const h = await routeHarness(t);
  const deadline = new AbortController();
  let deadlineCalls = 0;
  t.mock.method(AbortSignal, "timeout", (ms: number) => {
    assert.equal(ms, 20000, "do not enlarge or replace the receiver deadline");
    deadlineCalls++;
    return deadlineCalls === 1 ? deadline.signal : new AbortController().signal;
  });
  let calls = 0;
  let cancelled = 0;
  let forwarded: Request | undefined;
  t.mock.method(h.bot, "containerFetch", async (input: Request) => {
    forwarded = input;
    assert.equal(input.headers.get("x-request-id"), REQUEST_ID);
    if (++calls > 1) return json(ACK);
    let pulls = 0;
    return new Response(new ReadableStream<Uint8Array>({
      pull(controller) {
        if (pulls++ === 0) { controller.enqueue(encoder.encode(`{"private":"${PRIVATE}",`)); return; }
        deadline.abort();
        return new Promise<void>(() => {});
      },
      cancel() { cancelled++; return new Promise<void>(() => {}); },
    }, { highWaterMark: 0 }), { headers: { "content-type": "application/json" } });
  });
  await assertUnavailable(await within(h.bot.fetch(h.request())), h.logs);
  assert.equal(calls, 1, "deadline never retries a delivery");
  assert.equal(deadlineCalls, 1, "body admission must not renew the deadline");
  assert.equal(forwarded?.signal.aborted, true, "the forwarded fetch and relay share the expired signal");
  assert.equal(cancelled, 1, "best-effort cleanup does not wait on its promise");
  const next = await within(h.bot.fetch(h.request()));
  assert.equal(next.status, 200);
  assert.deepEqual(JSON.parse(await observed(next, h.logs)), ACK);
  assert.equal(calls, 2, "a completed timeout must not wedge the next receiver request");
});
