/**
 * TOG-12980: staging-only, default-dark Worker ingress for the private
 * internal-actions receiver (CISO conditions C1-C10, TOG-12979). The installed
 * Container SDK runs unmodified; runtime storage and TCP are in-memory and every
 * value is synthetic. No Discord, database, deployed Worker or secret is
 * contacted.
 * Run: node --import ./test/cloudflare-loader.mjs --test test/internal-actions.test.ts
 */
import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { DatabaseSync } from "node:sqlite";
import worker, { TwoBotContainer, type Env } from "../src/index.ts";
import { OWNER_KEY, DEPLOYMENT_HEADER } from "../src/ownership.ts";
import {
  ACTIONS_PATH,
  CONTAINER_MARKER,
  CONTAINER_MARKER_VALUE,
  MAX_BODY_BYTES,
  MAX_IN_FLIGHT,
  RECEIVER_BIND,
  RECEIVER_PORT,
  resetIngressLimits,
} from "../src/internal-actions.ts";

const ID = "deployment-A";
const OWNER = {
  deploymentId: ID, epoch: 1, phase: "active", actor: "fixture-operator",
  timestamp: "2026-10-03T00:00:00.000Z", oldEpoch: 0, oldDeploymentId: null,
};
const SIGNATURE = `sha256=${"a".repeat(64)}`;
const NONCE = "synthetic-nonce-0001";
const BODY = '{"action":"announcement.post","channel":"smoke-throwaway"}';
// Generated per run so no literal signing key exists in source.
const KEYS = `web-staging:${randomBytes(32).toString("hex")}`;
const CALLER_SECRET = `Bearer ${randomBytes(8).toString("hex")}`;
// Runtime-generated too: a literal "idempotency-key" value trips gitleaks' generic-api-key rule.
const INTENT_ID = `intent-${randomBytes(6).toString("hex")}`;

const DARK_ENV = {
  CF_VERSION_METADATA: { id: ID },
  DISCORD_TOKEN: "synthetic-discord-token",
  DATABASE_URL: "synthetic-database-value",
  GUILD_ID: "111222333444555666",
  KEEPALIVE_SECONDS: "60",
  REDIRECT_FALLBACK_CODE: "",
  REDIRECT_MAPPINGS_JSON: "[]",
};
const RECEIVER_SECRETS = {
  TWO_INTERNAL_ACTIONS: "1",
  TWO_INTERNAL_CALLERS: "web-staging:website-staging",
  TWO_INTERNAL_CHANNEL_KEYS: "smoke-throwaway:222333444555666777",
  TWO_INTERNAL_KEYS: KEYS,
};
/** What staging runs after the Operator's last step. */
const ENABLED_ENV = { ...DARK_ENV, INTERNAL_ACTIONS_INGRESS: "1", ...RECEIVER_SECRETS };

const SIGNED_HEADERS: Record<string, string> = {
  "content-type": "application/json",
  "x-two-key-id": "web-staging",
  "x-two-timestamp": "1790000000",
  "x-two-nonce": NONCE,
  "x-two-signature": SIGNATURE,
  "idempotency-key": INTENT_ID,
};

let clientCounter = 0;
/** A fresh edge IP per request: per-IP buckets are module-level state. */
function post(body: BodyInit | null = BODY, headers: Record<string, string> = SIGNED_HEADERS, path = ACTIONS_PATH, method = "POST") {
  const init: RequestInit & { duplex?: "half" } = {
    method,
    headers: { ...headers, "cf-connecting-ip": `203.0.113.${++clientCounter % 250}.${Math.floor(clientCounter / 250)}` },
  };
  if (method !== "GET" && method !== "HEAD" && body !== null) {
    init.body = body;
    if (typeof body === "object" && "getReader" in body) init.duplex = "half";
  }
  return new Request(`https://worker.invalid${path}`, init);
}

interface Seen {
  port: number;
  pathname: string;
  method: string;
  headers: Record<string, string>;
  body: Uint8Array | null;
}

async function harness(t: TestContext, env: Record<string, unknown>, options: { owner?: unknown } = {}) {
  const database = new DatabaseSync(":memory:");
  t.after(() => database.close());
  t.after(() => resetIngressLimits());
  resetIngressLimits();
  const logs: string[] = [];
  for (const method of ["log", "warn", "error"] as const) {
    t.mock.method(console, method, (...args: unknown[]) => { logs.push(args.map(String).join(" ")); });
  }
  const starts: { env?: Record<string, string> }[] = [];
  const seen: Seen[] = [];
  const values = new Map<string, unknown>([[OWNER_KEY, "owner" in options ? options.owner : OWNER]]);
  const listening = new Set<number>();
  let receiver: (request: Seen) => Response | Promise<Response> = () => Response.json({ ok: true, result: { message_id: "1" } });
  const runtime = {
    running: false,
    async destroy() { this.running = false; },
    start(config: { env?: Record<string, string> }) {
      assert.equal(this.running, false, "must not start an already-running container");
      starts.push(structuredClone(config));
      listening.add(Number(config.env?.LISTEN_ADDR?.split(":").at(-1) ?? 8080));
      const bind = config.env?.TWO_INTERNAL_BIND;
      if (bind) listening.add(Number(bind.split(":").at(-1)));
      this.running = true;
    },
    monitor: () => new Promise<void>(() => {}),
    getTcpPort: (port: number) => ({
      async fetch(input: string | Request, init?: RequestInit | Request) {
        const url = new URL(typeof input === "string" ? input : input.url);
        if (!listening.has(port)) throw new Error(`connection refused: nothing listens on ${port}`);
        const record: Seen = {
          port,
          pathname: url.pathname,
          method: init?.method ?? "GET",
          headers: Object.fromEntries(new Headers(init?.headers)),
          body: init instanceof Request && init.body ? new Uint8Array(await init.arrayBuffer()) : null,
        };
        seen.push(record);
        const response = port === RECEIVER_PORT && url.pathname === ACTIONS_PATH
          ? await receiver(record)
          : Response.json({ ready: true, status: "ok" });
        Object.defineProperty(response, "webSocket", { value: null });
        return response;
      },
    }),
  };
  const gates: Promise<unknown>[] = [];
  const ctx = {
    exports: { ContainerProxy: () => ({ fetch: () => { throw new Error("unexpected outbound request"); } }) },
    id: { toString: () => "synthetic-do-id" },
    container: runtime,
    storage: {
      get: async (key: string) => values.get(key),
      put: async (key: string | Record<string, unknown>, value?: unknown) => {
        if (typeof key === "string") values.set(key, value);
        else for (const [k, v] of Object.entries(key)) values.set(k, v);
      },
      delete: async (key: string) => values.delete(key),
      kv: {
        get: (key: string) => values.get(key),
        put: (key: string, value: unknown) => { values.set(key, value); },
        delete: (key: string) => values.delete(key),
      },
      sql: {
        exec(query: string, ...bindings: (string | number | null)[]) {
          return database.prepare(query).all(...bindings);
        },
      },
      setAlarm: async () => {},
      deleteAlarm: async () => {},
      sync: async () => {},
    },
    blockConcurrencyWhile<T>(callback: () => Promise<T>) {
      const promise = callback();
      gates.push(promise);
      return promise;
    },
  };
  const workerEnv = { ...env } as Record<string, unknown>;
  const bot = new TwoBotContainer(ctx as unknown as DurableObjectState<{}>, workerEnv as unknown as Env);
  await Promise.all(gates);
  workerEnv["TWO_BOT"] = { getByName: () => bot };
  const call = (request: Request) =>
    worker.fetch(request, workerEnv as unknown as Env, {} as ExecutionContext);
  /** Probes (not public ingress) start the container, as in production. */
  const warm = async () => {
    const response = await bot.fetch(new Request("https://worker.invalid/health", { headers: { [DEPLOYMENT_HEADER]: ID } }));
    assert.equal(response.status, 200);
    await response.arrayBuffer();
  };
  return {
    bot, runtime, starts, seen, logs, values, call, warm,
    setReceiver: (fn: typeof receiver) => { receiver = fn; },
    receiverCalls: () => seen.filter((r) => r.port === RECEIVER_PORT && r.pathname === ACTIONS_PATH),
  };
}

async function bodyOf(response: Response): Promise<string> {
  return response.text();
}

// ---------------------------------------------------------------- dark by default

test("dark states are indistinguishable from today's behavior and never touch the container", async (t) => {
  const baseline = await harness(t, DARK_ENV);
  const reference = await baseline.call(post());
  const referenceBody = await bodyOf(reference);
  assert.notEqual(reference.status, 200, "no route exists today");
  const states: Record<string, unknown>[] = [
    DARK_ENV,
    { ...DARK_ENV, ...RECEIVER_SECRETS },
    { ...DARK_ENV, INTERNAL_ACTIONS_INGRESS: "1" },
    { ...DARK_ENV, INTERNAL_ACTIONS_INGRESS: "1", TWO_INTERNAL_ACTIONS: "0" },
    { ...DARK_ENV, INTERNAL_ACTIONS_INGRESS: "1", TWO_INTERNAL_ACTIONS: "true" },
    { ...DARK_ENV, INTERNAL_ACTIONS_INGRESS: "true", ...RECEIVER_SECRETS },
    { ...DARK_ENV, INTERNAL_ACTIONS_INGRESS: "0", ...RECEIVER_SECRETS },
    { ...DARK_ENV, INTERNAL_ACTIONS_INGRESS: "", ...RECEIVER_SECRETS },
    { ...DARK_ENV, INTERNAL_ACTIONS_INGRESS: 1, ...RECEIVER_SECRETS },
  ];
  for (const env of states) {
    const h = await harness(t, env);
    const response = await h.call(post());
    assert.equal(response.status, reference.status, JSON.stringify(env));
    assert.equal(await bodyOf(response), referenceBody);
    assert.deepEqual(h.seen, [], "container untouched");
    assert.deepEqual(h.starts, [], "container not started");
  }
});

test("production-shaped env (no ingress var) stays dark even with receiver secrets set", async (t) => {
  const h = await harness(t, { ...DARK_ENV, ...RECEIVER_SECRETS });
  const response = await h.call(post());
  assert.notEqual(response.status, 200);
  assert.deepEqual(h.seen, []);
  // The DO route is dark too, whatever reaches it.
  const direct = await h.bot.fetch(new Request(`https://worker.invalid${ACTIONS_PATH}`, {
    method: "POST", body: BODY, headers: { ...SIGNED_HEADERS, [DEPLOYMENT_HEADER]: ID },
  }));
  assert.equal(direct.status, 404);
  assert.deepEqual(h.seen, []);
});

// ---------------------------------------------------------------- enabled: refusals before the container

test("enabled route refuses non-POST, query and unsigned shapes without touching the container", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  const before = h.seen.length;
  for (const method of ["GET", "HEAD", "PUT", "PATCH", "DELETE", "OPTIONS"]) {
    const response = await h.call(post(BODY, SIGNED_HEADERS, ACTIONS_PATH, method));
    assert.equal(response.status, 404, method);
    if (method !== "HEAD") assert.equal(await bodyOf(response), "not found");
  }
  for (const query of ["?x=1", "?", "?token=abc", "?a=1&b=2"]) {
    const response = await h.call(post(BODY, SIGNED_HEADERS, `${ACTIONS_PATH}${query}`));
    assert.equal(response.status, 404, query);
  }
  assert.equal(h.seen.length, before, "no refused request reached the container");
});

test("enabled route only matches the exact path; look-alikes fall through to today's behavior", async (t) => {
  const baseline = await harness(t, DARK_ENV);
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  const before = h.seen.length;
  for (const path of ["/internal/actions/", "/internal/action", "//internal/actions", "/Internal/Actions", "/internal/actions%2f", "/internal/actions/x", "/internal"]) {
    const [expected, actual] = await Promise.all([baseline.call(post(BODY, SIGNED_HEADERS, path)), h.call(post(BODY, SIGNED_HEADERS, path))]);
    assert.equal(actual.status, expected.status, path);
    assert.equal(await bodyOf(actual), await bodyOf(expected), path);
  }
  assert.equal(h.seen.length, before);
});

test("wrong content type, content encoding and missing signing headers are refused before the container", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  const before = h.seen.length;
  const without = (name: string) => Object.fromEntries(Object.entries(SIGNED_HEADERS).filter(([k]) => k !== name));
  const cases: [string, Record<string, string>, number][] = [
    ["text/plain", { ...SIGNED_HEADERS, "content-type": "text/plain" }, 415],
    ["no content type", without("content-type"), 415],
    ["wrong charset", { ...SIGNED_HEADERS, "content-type": "application/json; charset=latin1" }, 415],
    ["json+suffix", { ...SIGNED_HEADERS, "content-type": "application/vnd.api+json" }, 415],
    ["content-encoding", { ...SIGNED_HEADERS, "content-encoding": "gzip" }, 415],
    ["no key id", without("x-two-key-id"), 401],
    ["no timestamp", without("x-two-timestamp"), 401],
    ["no nonce", without("x-two-nonce"), 401],
    ["no signature", without("x-two-signature"), 401],
    ["list-valued key id", { ...SIGNED_HEADERS, "x-two-key-id": "web-staging, other" }, 400],
    ["oversize nonce", { ...SIGNED_HEADERS, "x-two-nonce": "n".repeat(33) }, 400],
    ["oversize idempotency key", { ...SIGNED_HEADERS, "idempotency-key": "k".repeat(201) }, 400],
    ["control char in signature", { ...SIGNED_HEADERS, "x-two-signature": "sha256=\u0001" }, 400],
  ];
  for (const [name, headers, status] of cases) {
    let response: Response;
    try {
      response = await h.call(post(BODY, headers));
    } catch {
      // The runtime itself rejects some header values before the Worker runs.
      continue;
    }
    assert.equal(response.status, status, name);
    const body = await response.json() as { ok: boolean; error: { code: string; message: string } };
    assert.equal(body.ok, false);
    assert.equal(response.headers.get("cache-control"), "no-store");
  }
  assert.equal(h.seen.length, before);
});

test("oversize, empty and slow bodies are refused before the container", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  const before = h.seen.length;

  // Declared too large: refused without reading a byte.
  const declared = await h.call(post(new Uint8Array(MAX_BODY_BYTES + 1)));
  assert.equal(declared.status, 413);

  // Undeclared (streamed) too large.
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(new Uint8Array(MAX_BODY_BYTES));
      controller.enqueue(new Uint8Array(1));
      controller.close();
    },
  });
  const streamed = await h.call(post(stream));
  assert.equal(streamed.status, 413);

  const empty = await h.call(post(new Uint8Array()));
  assert.equal(empty.status, 400);
  const bad = await h.call(post(BODY, { ...SIGNED_HEADERS, "content-length": "12abc" }));
  assert.ok([400, 413].includes(bad.status));
  assert.equal(h.seen.length, before);

  // A body that never ends is cut at the body timeout.
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const stalled = new ReadableStream<Uint8Array>({ pull: () => new Promise(() => {}) });
  const pending = h.call(post(stalled));
  await new Promise<void>((resolve) => setImmediate(resolve));
  t.mock.timers.tick(5_000);
  const timedOut = await pending;
  assert.equal(timedOut.status, 408);
  assert.equal(h.seen.length, before);
});

test("exactly the size cap is accepted, byte for byte", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  const payload = new Uint8Array(MAX_BODY_BYTES).map((_, i) => (i * 31 + 7) & 0xff);
  const response = await h.call(post(payload));
  assert.equal(response.status, 200);
  const [call] = h.receiverCalls();
  assert.ok(call?.body);
  assert.equal(Buffer.compare(call.body, payload), 0);
});

test("per-IP bucket returns 429 before the container; the in-flight cap bounds concurrency", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  const before = h.seen.length;
  const ip = "198.51.100.7";
  const fromIp = () => new Request(`https://worker.invalid${ACTIONS_PATH}`, {
    method: "POST", body: BODY, headers: { ...SIGNED_HEADERS, "cf-connecting-ip": ip },
  });
  const statuses: number[] = [];
  let retryAfter: string | null = null;
  for (let i = 0; i < 25; i++) {
    const response = await h.call(fromIp());
    statuses.push(response.status);
    if (response.status === 429) retryAfter = response.headers.get("retry-after");
    await response.arrayBuffer();
  }
  assert.ok(statuses.includes(429), "burst must be capped");
  assert.equal(statuses[0], 200);
  assert.match(retryAfter ?? "", /^\d+$/);
  assert.ok(h.seen.length - before < 25, "refused requests never reached the container");

  // Concurrency: stall the DO and exceed the per-isolate cap from distinct IPs.
  resetIngressLimits();
  let release!: () => void;
  const gate = new Promise<void>((resolve) => { release = resolve; });
  const stalled = {
    TWO_BOT: { getByName: () => ({ fetch: async () => { await gate; return Response.json({ ok: true }); } }) },
  };
  const env = { ...ENABLED_ENV, ...stalled } as unknown as Env;
  const held = Array.from({ length: MAX_IN_FLIGHT }, () =>
    worker.fetch(post(), env, {} as ExecutionContext));
  await new Promise<void>((resolve) => setImmediate(resolve));
  const over = await worker.fetch(post(), env, {} as ExecutionContext);
  assert.equal(over.status, 429);
  assert.equal(over.headers.get("retry-after"), "1");
  release();
  for (const response of await Promise.all(held)) assert.equal(response.status, 200);
  const after = await worker.fetch(post(), env, {} as ExecutionContext);
  assert.equal(after.status, 200, "capacity returns once requests settle");
});

test("a stalled upstream is cut at the request deadline with a fixed envelope", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  const env = {
    ...ENABLED_ENV,
    TWO_BOT: { getByName: () => ({ fetch: () => new Promise<Response>(() => {}) }) },
  } as unknown as Env;
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const pending = worker.fetch(post(), env, {} as ExecutionContext);
  await new Promise<void>((resolve) => setImmediate(resolve));
  await new Promise<void>((resolve) => setImmediate(resolve));
  t.mock.timers.tick(20_000);
  const response = await pending;
  assert.equal(response.status, 504);
  assert.deepEqual((await response.json() as { error: { code: string } }).error.code, "upstream_timeout");
  assert.deepEqual(h.seen, []);
});

// ---------------------------------------------------------------- enabled: forwarding

test("forwards exactly the allowlisted headers and unchanged bytes to the fixed receiver port", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  // Not valid UTF-8, CRLFs, trailing whitespace and NUL: forwarded verbatim.
  const payload = new Uint8Array([0x7b, 0x22, 0xff, 0xfe, 0x00, 0x0d, 0x0a, 0x20, 0x7d, 0x20, 0x0a]);
  const request = post(payload, {
    ...SIGNED_HEADERS,
    authorization: CALLER_SECRET,
    cookie: "session=should-not-forward",
    "x-forwarded-for": "192.0.2.9",
    "x-custom": "should-not-forward",
    [DEPLOYMENT_HEADER]: "caller-chosen-deployment",
  });
  const response = await h.call(request);
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { ok: true, result: { message_id: "1" } });
  const [call, ...rest] = h.receiverCalls();
  assert.equal(rest.length, 0);
  assert.ok(call?.body);
  assert.equal(call.port, RECEIVER_PORT);
  assert.equal(call.method, "POST");
  assert.equal(Buffer.compare(call.body, payload), 0, "body bytes unchanged");
  assert.deepEqual(Object.keys(call.headers).sort(), [
    "content-type", "idempotency-key",
    "x-two-key-id", "x-two-nonce", "x-two-signature", "x-two-timestamp",
  ]);
  assert.equal(call.headers["x-two-signature"], SIGNATURE);
  assert.equal(call.headers["x-two-nonce"], NONCE);
  assert.equal(call.headers["content-type"], "application/json");
  assert.equal(h.starts.length, 1, "ingress never starts a second container");
});

test("idempotency key is optional at the Worker; the receiver owns that rule", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  const { "idempotency-key": _drop, ...headers } = SIGNED_HEADERS;
  const response = await h.call(post(BODY, headers));
  assert.equal(response.status, 200);
  assert.equal(h.receiverCalls()[0]?.headers["idempotency-key"], undefined);
});

test("receiver answers relay with allowlisted headers only; container error text never leaves", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();

  h.setReceiver(() => new Response(JSON.stringify({ ok: false, request_id: "r1", error: { code: "unauthorized", message: "unauthorized", retryable: false } }), {
    status: 401,
    headers: {
      "content-type": "application/json",
      "idempotent-replay": "true",
      "retry-after": "7",
      "set-cookie": "session=leak",
      "x-secret": "leak",
      "cache-control": "public, max-age=999",
    },
  }));
  const relayed = await h.call(post());
  assert.equal(relayed.status, 401);
  assert.equal(relayed.headers.get("idempotent-replay"), "true");
  assert.equal(relayed.headers.get("retry-after"), "7");
  assert.equal(relayed.headers.get("cache-control"), "no-store");
  assert.equal(relayed.headers.get("set-cookie"), null);
  assert.equal(relayed.headers.get("x-secret"), null);
  assert.equal((await relayed.json() as { ok: boolean }).ok, false);

  const leak = `boom ${ENABLED_ENV.DISCORD_TOKEN} ${ENABLED_ENV.DATABASE_URL} ${KEYS}`;
  for (const response of [
    () => new Response(leak, { status: 500 }),
    () => new Response(leak, { status: 429 }),
    () => new Response(leak, { status: 503, headers: { "content-type": "text/html" } }),
    () => new Response(leak, { status: 200, headers: { "content-type": "application/jsonp" } }),
    () => new Response(new Uint8Array(70 * 1024), { status: 200, headers: { "content-type": "application/json" } }),
    () => new Response(null, { status: 204, headers: { "content-type": "application/json" } }),
  ]) {
    h.setReceiver(response);
    const sanitized = await h.call(post());
    assert.equal(sanitized.status, 503);
    const text = await sanitized.text();
    assert.deepEqual(Object.keys((JSON.parse(text) as { error: object }).error).sort(), ["code", "message", "retryable"]);
    assert.ok(!text.includes("boom") && !text.includes(KEYS) && !text.includes(ENABLED_ENV.DISCORD_TOKEN));
  }

  // The SDK turns a failed proxy into a text 500 carrying the raw message.
  h.setReceiver(() => { throw new Error(`connection reset ${leak}`); });
  const thrown = await h.call(post());
  assert.equal(thrown.status, 503);
  assert.ok(!(await thrown.text()).includes("boom"));
  assert.ok(
    h.logs.filter((line) => line.includes("boom")).every((line) => line.startsWith("Error proxying request to container")),
    "only the SDK's own lifecycle log may echo the platform message; this Worker logs scalars only",
  );
  h.logs.length = 0;
  await h.call(post(BODY, { ...SIGNED_HEADERS, "content-type": "text/plain" }));
  assert.ok(h.logs.every((line) => !line.includes(KEYS)));
});

test("public ingress never cold-starts the container", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  const response = await h.call(post());
  assert.equal(response.status, 503);
  assert.equal((await response.json() as { error: { code: string } }).error.code, "unavailable");
  assert.deepEqual(h.starts, []);
  assert.deepEqual(h.seen, []);
});

test("the ownership fence refuses ingress; a stale or spoofed deployment never forwards", async (t) => {
  for (const owner of [
    { ...OWNER, deploymentId: "deployment-B" },
    { ...OWNER, phase: "fenced", deploymentId: null },
    undefined,
  ]) {
    const h = await harness(t, ENABLED_ENV, { owner });
    const response = await h.call(post());
    assert.equal(response.status, 503, JSON.stringify(owner));
    const body = await response.text();
    assert.equal((JSON.parse(body) as { error: { code: string } }).error.code, "unavailable");
    assert.ok(!body.includes("deployment"), "closed reason stays in logs");
    assert.deepEqual(h.seen, []);
  }

  // Straight to the DO with a wrong id: the fence refuses even a well-formed request.
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  const before = h.seen.length;
  const direct = await h.bot.fetch(new Request(`https://worker.invalid${ACTIONS_PATH}`, {
    method: "POST", body: BODY, headers: { ...SIGNED_HEADERS, [DEPLOYMENT_HEADER]: "deployment-B" },
  }));
  assert.equal(direct.status, 503);
  const missing = await h.bot.fetch(new Request(`https://worker.invalid${ACTIONS_PATH}`, {
    method: "POST", body: BODY, headers: SIGNED_HEADERS,
  }));
  assert.equal(missing.status, 503);
  assert.equal(h.seen.length, before);
});

test("logs carry only scalar event, status and error class", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  const secrets = [SIGNATURE, NONCE, BODY, KEYS, "203.0.113", INTENT_ID];
  await h.call(post(BODY, { ...SIGNED_HEADERS, "content-type": "text/plain" }));
  await h.call(post(new Uint8Array(MAX_BODY_BYTES + 1)));
  await h.call(post(BODY, SIGNED_HEADERS, `${ACTIONS_PATH}?sig=${SIGNATURE}`));
  const { "x-two-nonce": _n, ...noNonce } = SIGNED_HEADERS;
  await h.call(post(BODY, noNonce));
  const ingress = h.logs.filter((line) => line.includes("internal_actions_ingress"));
  assert.ok(ingress.length >= 3, "refusals are logged");
  for (const line of ingress) {
    const record = JSON.parse(line) as Record<string, unknown>;
    assert.deepEqual(Object.keys(record).sort(), ["error_class", "event", "status"]);
    assert.match(String(record["error_class"]), /^[a-z_]+$/);
  }
  for (const line of h.logs) for (const secret of secrets) assert.ok(!line.includes(secret), secret);
});

// ---------------------------------------------------------------- container wiring

test("receiver env reaches the Container only while TWO_INTERNAL_ACTIONS is exactly 1", async (t) => {
  const dark = await harness(t, { ...DARK_ENV, INTERNAL_ACTIONS_INGRESS: "1", TWO_INTERNAL_KEYS: KEYS, TWO_INTERNAL_CALLERS: "x:y" });
  await dark.warm();
  assert.deepEqual(Object.keys(dark.starts[0]?.env ?? {}).filter((k) => k.startsWith("TWO_INTERNAL_")), []);

  for (const value of ["0", "true", "", " 1", "1 "]) {
    const h = await harness(t, { ...DARK_ENV, ...RECEIVER_SECRETS, TWO_INTERNAL_ACTIONS: value });
    await h.warm();
    assert.deepEqual(Object.keys(h.starts[0]?.env ?? {}).filter((k) => k.startsWith("TWO_INTERNAL_")), [], JSON.stringify(value));
  }

  const on = await harness(t, {
    ...ENABLED_ENV,
    TWO_INTERNAL_BIND: "0.0.0.0:9999",
    TWO_INTERNAL_ALLOW_MODERATION: "1",
    TWO_INTERNAL_ROLE_KEYS: "synthetic",
  });
  await on.warm();
  const env = on.starts[0]?.env ?? {};
  assert.deepEqual(
    Object.fromEntries(Object.entries(env).filter(([k]) => k.startsWith("TWO_INTERNAL_"))),
    {
      TWO_INTERNAL_ACTIONS: "1",
      TWO_INTERNAL_BIND: RECEIVER_BIND,
      [CONTAINER_MARKER]: CONTAINER_MARKER_VALUE,
      TWO_INTERNAL_CALLERS: RECEIVER_SECRETS.TWO_INTERNAL_CALLERS,
      TWO_INTERNAL_CHANNEL_KEYS: RECEIVER_SECRETS.TWO_INTERNAL_CHANNEL_KEYS,
      TWO_INTERNAL_KEYS: KEYS,
    },
    "the bind and container marker are the Worker's constants; other TWO_INTERNAL_* names never pass",
  );
  assert.equal(RECEIVER_BIND, "0.0.0.0:8091");
  assert.equal(CONTAINER_MARKER, "TWO_INTERNAL_CONTAINER");
  assert.equal(CONTAINER_MARKER_VALUE, "1");
  assert.equal(env["LISTEN_ADDR"], "0.0.0.0:8080", "the public health bind is unchanged");
  assert.ok(on.logs.every((line) => !line.includes(KEYS)), "the signing key is never logged");
});

test("BOT_PORT may not collide with the receiver port while it is enabled", async (t) => {
  await assert.rejects(() => harness(t, { ...ENABLED_ENV, BOT_PORT: String(RECEIVER_PORT) }), /BOT_PORT must differ/);
  const dark = await harness(t, { ...DARK_ENV, BOT_PORT: String(RECEIVER_PORT) });
  assert.ok(dark.bot);
});

test("the receiver port joins startup port checks only while enabled", async (t) => {
  const dark = await harness(t, { ...DARK_ENV, INTERNAL_ACTIONS_INGRESS: "1" });
  await dark.warm();
  assert.deepEqual([...new Set(dark.seen.map((r) => r.port))], [8080]);

  const on = await harness(t, ENABLED_ENV);
  await on.warm();
  assert.deepEqual([...new Set(on.seen.map((r) => r.port))].sort(), [8080, RECEIVER_PORT]);
  assert.deepEqual(on.bot.requiredPorts, [8080, RECEIVER_PORT]);
  // Dark leaves the SDK default (a field declared without initializer, so
  // undefined) untouched: the port set it resolves is the one base main resolves.
  assert.equal(dark.bot.requiredPorts, undefined);
  const resolved = (bot: object) => (bot as { getPortsToCheck(ports?: number): Promise<number[]> }).getPortsToCheck();
  assert.deepEqual(await resolved(dark.bot), [8080]);
  assert.deepEqual(await resolved(on.bot), [8080, RECEIVER_PORT]);
});

test("a receiver that never listens fails startup instead of reporting ready", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  t.mock.method(h.runtime, "start", (config: { env?: Record<string, string> }) => {
    h.starts.push(structuredClone(config));
    h.runtime.running = true;
    // Only the health port ever listens; the sidecar cannot reach the receiver.
  });
  t.mock.method(h.runtime, "getTcpPort", (port: number) => ({
    fetch: async () => {
      if (port === 8080) return Object.assign(Response.json({ ready: true }), { webSocket: null });
      throw new Error("connection refused");
    },
  }));
  await assert.rejects(
    h.bot.startAndWaitForPorts(8080, { portReadyTimeoutMS: 400, waitInterval: 100 }),
    /connection refused|port 8091|Failed to verify port/i,
  );
});

// TOG-16851: with the receiver on and listening on the Worker's wildcard bind,
// the startup port check for both ports succeeds and the container is healthy.
test("health and receiver ports listening together pass startup while enabled", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.bot.startAndWaitForPorts(8080, { portReadyTimeoutMS: 2000, waitInterval: 50 });
  assert.equal(h.runtime.running, true);
  assert.equal(h.starts.length, 1);
  const env = h.starts[0]?.env ?? {};
  assert.equal(env["TWO_INTERNAL_BIND"], RECEIVER_BIND);
  assert.equal(env[CONTAINER_MARKER], CONTAINER_MARKER_VALUE);
  assert.deepEqual(h.bot.requiredPorts, [8080, RECEIVER_PORT]);
});
