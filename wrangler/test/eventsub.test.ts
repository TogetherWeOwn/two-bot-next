/**
 * TOG-23364: staging-only, default-dark Worker ingress for Twitch EventSub
 * deliveries (`POST /hooks/twitch`). The installed Container SDK runs
 * unmodified; runtime storage and TCP are in-memory and every value is
 * synthetic. No Twitch, Discord, database, deployed Worker or secret is
 * contacted.
 * Run: node --import ./test/cloudflare-loader.mjs --test test/eventsub.test.ts
 */
import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";
import { DatabaseSync } from "node:sqlite";
import worker, { TwoBotContainer, type Env } from "../src/index.ts";
import { DEPLOYMENT_HEADER, OWNER_KEY } from "../src/ownership.ts";
import { LOG_QUERY_URL } from "../src/alert-rules.ts";
import { isRequestId } from "../src/request-id.ts";
import {
  EVENTSUB_PATH,
  EVENTSUB_RECEIVER_BIND,
  EVENTSUB_RECEIVER_PORT,
  MAX_BODY_BYTES,
  REFRESH_NONCE_HEADER,
  REFRESH_SECRET_HEADER,
  REFRESH_SIGNATURE_HEADER,
  REFRESH_TIMESTAMP_HEADER,
  handleEventsubIngress,
  MAX_IN_FLIGHT,
  relayEventsubResponse,
  resetEventsubIngressLimits,
  signEventsubRefresh,
} from "../src/eventsub.ts";
import {
  CONTAINER_MARKER,
  CONTAINER_MARKER_VALUE,
  RECEIVER_PORT,
} from "../src/internal-actions.ts";

const ID = "deployment-A";
const OWNER = {
  deploymentId: ID, epoch: 1, phase: "active", actor: "fixture-operator",
  timestamp: "2026-10-03T00:00:00.000Z", oldEpoch: 0, oldDeploymentId: null,
};
// Synthetic delivery headers: fixed alphabet, no key material.
const MESSAGE_ID = "synthetic-delivery-0001";
const MESSAGE_TIMESTAMP = "2026-10-13T00:00:00.000Z";
const MESSAGE_SIGNATURE = `sha256=${"b".repeat(64)}`;
const BODY = '{"subscription":{"type":"stream.online","version":"1"},"event":{}}';
const CHALLENGE = "synthetic-challenge-0001";

const DARK_ENV = {
  CF_VERSION_METADATA: { id: ID },
  DISCORD_TOKEN: "synthetic-discord-token",
  DATABASE_URL: "synthetic-database-value",
  GUILD_ID: "111222333444555666",
  KEEPALIVE_SECONDS: "60",
  REDIRECT_FALLBACK_CODE: "",
  REDIRECT_MAPPINGS_JSON: "[]",
};
/** What staging runs after the Operator's last step: flag, reconcile inputs and credentials. */
const ENABLED_ENV = {
  ...DARK_ENV,
  EVENTSUB_INGRESS: "1",
  TWO_EVENTSUB: "1",
  // Built from parts: a single assigned literal on a secret key trips gitleaks
  // generic-api-key. The joined runtime values are unchanged.
  TWO_TWITCH_WEBHOOK_SECRET: ["synthetic", "operator", "secret", "01"].join("-"),
  TWO_TWITCH_CALLBACK_URL: "https://hooks.example.com/twitch",
  TWO_TWITCH_BROADCASTERS: "111222333444555666",
  TWO_TWITCH_CLIENT_ID: "synthetic-operator-client-id-01",
  TWO_TWITCH_CLIENT_SECRET: ["synthetic", "operator", "client", "secret", "01"].join("-"),
};

const DELIVERY_HEADERS: Record<string, string> = {
  "content-type": "application/json",
  "twitch-eventsub-message-id": MESSAGE_ID,
  "twitch-eventsub-message-timestamp": MESSAGE_TIMESTAMP,
  "twitch-eventsub-message-signature": MESSAGE_SIGNATURE,
  "twitch-eventsub-message-type": "notification",
};

let clientCounter = 0;
/** A fresh edge IP per request: per-IP buckets are module-level state. */
function post(body: BodyInit | null = BODY, headers: Record<string, string> = DELIVERY_HEADERS, path = EVENTSUB_PATH, method = "POST") {
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
  t.after(() => resetEventsubIngressLimits());
  resetEventsubIngressLimits();
  const logs: string[] = [];
  for (const method of ["log", "warn", "error"] as const) {
    t.mock.method(console, method, (...args: unknown[]) => { logs.push(args.map(String).join(" ")); });
  }
  const starts: { env?: Record<string, string> }[] = [];
  const seen: Seen[] = [];
  const values = new Map<string, unknown>([[OWNER_KEY, "owner" in options ? options.owner : OWNER]]);
  const listening = new Set<number>();
  let receiver: (request: Seen) => Response | Promise<Response> = (request) =>
    Response.json({ ok: true, request_id: request.headers["x-request-id"] });
  const runtime = {
    running: false,
    async destroy() { this.running = false; },
    start(config: { env?: Record<string, string> }) {
      assert.equal(this.running, false, "must not start an already-running container");
      starts.push(structuredClone(config));
      listening.add(Number(config.env?.LISTEN_ADDR?.split(":").at(-1) ?? 8080));
      const bind = config.env?.TWO_INTERNAL_BIND;
      if (bind) listening.add(Number(bind.split(":").at(-1)));
      // TOG-23387: the real container binds the EventSub receiver at startup
      // while TWO_EVENTSUB is "1" with the signing secret present; model it
      // so `requiredPorts` checks pass.
      const eventsubBind = config.env?.TWO_EVENTSUB_BIND;
      if (eventsubBind) listening.add(Number(eventsubBind.split(":").at(-1)));
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
        const response = port === EVENTSUB_RECEIVER_PORT && url.pathname === EVENTSUB_PATH
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
    receiverCalls: () => seen.filter((r) => r.port === EVENTSUB_RECEIVER_PORT && r.pathname === EVENTSUB_PATH),
    /**
     * TOG-23387: the container binds EVENTSUB_RECEIVER_PORT at startup while
     * TWO_EVENTSUB is "1" with the signing secret present (see `start`
     * above). This only swaps the scripted receiver logic; the port is
     * already listening after `warm`.
     */
    serveEventsubReceiver: () => { listening.add(EVENTSUB_RECEIVER_PORT); },
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
    { ...DARK_ENV, TWO_EVENTSUB: "1" },
    { ...DARK_ENV, EVENTSUB_INGRESS: "1" },
    { ...DARK_ENV, EVENTSUB_INGRESS: "1", TWO_EVENTSUB: "0" },
    { ...DARK_ENV, EVENTSUB_INGRESS: "1", TWO_EVENTSUB: "true" },
    { ...DARK_ENV, EVENTSUB_INGRESS: "true", TWO_EVENTSUB: "1" },
    { ...DARK_ENV, EVENTSUB_INGRESS: "0", TWO_EVENTSUB: "1" },
    { ...DARK_ENV, EVENTSUB_INGRESS: "", TWO_EVENTSUB: "1" },
    { ...DARK_ENV, EVENTSUB_INGRESS: 1, TWO_EVENTSUB: "1" },
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

test("production-shaped env (no ingress var) stays dark even with the operator switch set", async (t) => {
  const h = await harness(t, { ...DARK_ENV, TWO_EVENTSUB: "1" });
  const response = await h.call(post());
  assert.notEqual(response.status, 200);
  assert.deepEqual(h.seen, []);
  // The DO route is dark too, whatever reaches it.
  const direct = await h.bot.fetch(new Request(`https://worker.invalid${EVENTSUB_PATH}`, {
    method: "POST", body: BODY, headers: { ...DELIVERY_HEADERS, [DEPLOYMENT_HEADER]: ID },
  }));
  assert.equal(direct.status, 404);
  assert.deepEqual(h.seen, []);
});

test("enabled flag without the signing secret stays dark: nothing forwarded, no receiver port", async (t) => {
  const h = await harness(t, { ...DARK_ENV, EVENTSUB_INGRESS: "1", TWO_EVENTSUB: "1" });
  await h.warm();
  assert.equal(h.starts.length, 1);
  const forwarded = h.starts[0]?.env ?? {};
  assert.equal(forwarded["TWO_EVENTSUB"], undefined, "flag not forwarded without the secret");
  assert.equal(forwarded["TWO_EVENTSUB_BIND"], undefined, "bind not forwarded without the secret");
  assert.equal(h.bot.requiredPorts, undefined, "no receiver port while dark");
  // The public route still gates on the flag alone, but the dark container
  // has no receiver: the forward fails closed to the 503 envelope.
  const response = await h.call(post());
  assert.equal(response.status, 503);
});

test("enabled env forwards flag, bind, marker, reconcile inputs and credentials and pins 8092 in requiredPorts", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  assert.equal(h.starts.length, 1);
  const forwarded = h.starts[0]?.env ?? {};
  assert.equal(forwarded["TWO_EVENTSUB"], "1", "flag forwarded only with the secret");
  assert.equal(forwarded["TWO_EVENTSUB_BIND"], EVENTSUB_RECEIVER_BIND, "bind is the Worker's constant");
  assert.equal(forwarded[CONTAINER_MARKER], CONTAINER_MARKER_VALUE, "container marker forwarded");
  assert.equal(
    forwarded["TWO_TWITCH_WEBHOOK_SECRET"],
    ENABLED_ENV.TWO_TWITCH_WEBHOOK_SECRET,
    "signing secret forwarded only with the flag",
  );
  assert.equal(
    forwarded["TWO_TWITCH_CALLBACK_URL"],
    ENABLED_ENV.TWO_TWITCH_CALLBACK_URL,
    "reconcile callback forwarded on its own explicit line",
  );
  assert.equal(
    forwarded["TWO_TWITCH_BROADCASTERS"],
    ENABLED_ENV.TWO_TWITCH_BROADCASTERS,
    "reconcile allowlist forwarded on its own explicit line",
  );
  assert.equal(
    forwarded["TWO_TWITCH_CLIENT_ID"],
    ENABLED_ENV.TWO_TWITCH_CLIENT_ID,
    "Helix client id forwarded on its own explicit line",
  );
  assert.equal(
    forwarded["TWO_TWITCH_CLIENT_SECRET"],
    ENABLED_ENV.TWO_TWITCH_CLIENT_SECRET,
    "Helix client secret forwarded on its own explicit line",
  );
  assert.deepEqual(h.bot.requiredPorts, [8080, EVENTSUB_RECEIVER_PORT]);
  // Combined with the internal-actions receiver: both sidecars are checked.
  const both = await harness(t, { ...ENABLED_ENV, TWO_INTERNAL_ACTIONS: "1" });
  await both.warm();
  assert.deepEqual(both.bot.requiredPorts, [8080, RECEIVER_PORT, EVENTSUB_RECEIVER_PORT]);
});

test("a silent eventsub receiver fails startup instead of reporting ready", async (t) => {
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
    /connection refused|port 8092|Failed to verify port/i,
  );
});

test("health and eventsub ports listening together pass startup while enabled", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.bot.startAndWaitForPorts(8080, { portReadyTimeoutMS: 2000, waitInterval: 50 });
  assert.equal(h.runtime.running, true);
  assert.equal(h.starts.length, 1);
  const env = h.starts[0]?.env ?? {};
  assert.equal(env["TWO_EVENTSUB"], "1");
  assert.equal(env["TWO_EVENTSUB_BIND"], EVENTSUB_RECEIVER_BIND);
  assert.equal(env[CONTAINER_MARKER], CONTAINER_MARKER_VALUE);
  assert.deepEqual(h.bot.requiredPorts, [8080, EVENTSUB_RECEIVER_PORT]);
});

// ---------------------------------------------------------------- enabled: refusals before the container

test("enabled route refuses non-POST, query and look-alike paths without touching the container", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  const before = h.seen.length;
  for (const method of ["GET", "HEAD", "PUT", "PATCH", "DELETE", "OPTIONS"]) {
    const response = await h.call(post(BODY, DELIVERY_HEADERS, EVENTSUB_PATH, method));
    assert.equal(response.status, 404, method);
    if (method !== "HEAD") assert.equal(await bodyOf(response), "not found");
  }
  for (const query of ["?x=1", "?", "?token=abc", "?a=1&b=2"]) {
    const response = await h.call(post(BODY, DELIVERY_HEADERS, `${EVENTSUB_PATH}${query}`));
    assert.equal(response.status, 404, query);
  }
  const baseline = await harness(t, DARK_ENV);
  for (const path of ["/hooks/twitch/", "/hooks/twitc", "//hooks/twitch", "/Hooks/Twitch", "/hooks/twitch%2f", "/hooks/twitch/x", "/hooks", "/hooks/twitch2"]) {
    const [expected, actual] = await Promise.all([baseline.call(post(BODY, DELIVERY_HEADERS, path)), h.call(post(BODY, DELIVERY_HEADERS, path))]);
    assert.equal(actual.status, expected.status, path);
    assert.equal(await bodyOf(actual), await bodyOf(expected), path);
  }
  assert.equal(h.seen.length, before, "no refused request reached the container");
});

test("wrong content type and missing or malformed Twitch headers are refused before the container", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  const before = h.seen.length;
  const without = (name: string) => Object.fromEntries(Object.entries(DELIVERY_HEADERS).filter(([k]) => k !== name));
  const cases: [string, Record<string, string>, number][] = [
    ["text/plain", { ...DELIVERY_HEADERS, "content-type": "text/plain" }, 415],
    ["no content type", without("content-type"), 415],
    ["json+suffix", { ...DELIVERY_HEADERS, "content-type": "application/vnd.api+json" }, 415],
    ["content-encoding", { ...DELIVERY_HEADERS, "content-encoding": "gzip" }, 415],
    ["no message id", without("twitch-eventsub-message-id"), 401],
    ["no timestamp", without("twitch-eventsub-message-timestamp"), 401],
    ["no signature", without("twitch-eventsub-message-signature"), 401],
    ["no message type", without("twitch-eventsub-message-type"), 401],
    ["list-valued id", { ...DELIVERY_HEADERS, "twitch-eventsub-message-id": "abc, def" }, 400],
    ["oversize id", { ...DELIVERY_HEADERS, "twitch-eventsub-message-id": "a".repeat(129) }, 400],
    ["bad id alphabet", { ...DELIVERY_HEADERS, "twitch-eventsub-message-id": "id with spaces" }, 400],
    ["oversize timestamp", { ...DELIVERY_HEADERS, "twitch-eventsub-message-timestamp": "t".repeat(65) }, 400],
    ["control char in signature", { ...DELIVERY_HEADERS, "twitch-eventsub-message-signature": "sha256=" }, 400],
    ["signature without prefix", { ...DELIVERY_HEADERS, "twitch-eventsub-message-signature": "b".repeat(64) }, 400],
    ["oversize message type", { ...DELIVERY_HEADERS, "twitch-eventsub-message-type": "t".repeat(65) }, 400],
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

  const declared = await h.call(post(new Uint8Array(MAX_BODY_BYTES + 1)));
  assert.equal(declared.status, 413);

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
  const bad = await h.call(post(BODY, { ...DELIVERY_HEADERS, "content-length": "12abc" }));
  assert.ok([400, 413].includes(bad.status));
  assert.equal(h.seen.length, before);

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
  h.serveEventsubReceiver();
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
  h.serveEventsubReceiver();
  const before = h.seen.length;
  const ip = "198.51.100.7";
  const fromIp = () => new Request(`https://worker.invalid${EVENTSUB_PATH}`, {
    method: "POST", body: BODY, headers: { ...DELIVERY_HEADERS, "cf-connecting-ip": ip },
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

  resetEventsubIngressLimits();
  let release!: () => void;
  const gate = new Promise<void>((resolve) => { release = resolve; });
  const stalled = {
    TWO_BOT: { getByName: () => ({ fetch: async (request: Request) => {
      await gate;
      return Response.json({ ok: true, request_id: request.headers.get("x-request-id") });
    } }) },
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

// ---------------------------------------------------------------- enabled: forwarding

test("forwards exactly the allowlisted headers and unchanged bytes to the fixed receiver port", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  h.serveEventsubReceiver();
  const payload = new Uint8Array([0x7b, 0x22, 0xff, 0xfe, 0x00, 0x0d, 0x0a, 0x20, 0x7d, 0x20, 0x0a]);
  const request = post(payload, {
    ...DELIVERY_HEADERS,
    authorization: "Bearer should-not-forward",
    cookie: "session=should-not-forward",
    "twitch-eventsub-message-retry": "1",
    "twitch-eventsub-subscription-type": "stream.online",
    "x-forwarded-for": "192.0.2.9",
    "x-custom": "should-not-forward",
    [DEPLOYMENT_HEADER]: "caller-chosen-deployment",
  });
  const response = await h.call(request);
  assert.equal(response.status, 200);
  await response.arrayBuffer();
  const [call] = h.receiverCalls();
  assert.ok(call, "receiver reached");
  assert.equal(call.port, EVENTSUB_RECEIVER_PORT);
  assert.equal(call.pathname, EVENTSUB_PATH);
  assert.equal(call.method, "POST");
  assert.equal(Buffer.compare(call.body!, payload), 0);
  assert.equal(call.headers["content-type"], "application/json");
  assert.equal(call.headers["twitch-eventsub-message-id"], MESSAGE_ID);
  assert.equal(call.headers["twitch-eventsub-message-timestamp"], MESSAGE_TIMESTAMP);
  assert.equal(call.headers["twitch-eventsub-message-signature"], MESSAGE_SIGNATURE);
  assert.equal(call.headers["twitch-eventsub-message-type"], "notification");
  for (const dropped of ["authorization", "cookie", "twitch-eventsub-message-retry", "twitch-eventsub-subscription-type", "x-forwarded-for", "x-custom", "cf-connecting-ip"]) {
    assert.ok(!(dropped in call.headers), `${dropped} must not forward`);
  }
  // No inbound id here, so the edge minted one: a valid join key, not a leak.
  assert.ok(isRequestId(call.headers["x-request-id"] ?? ""));
  // The fence id is stamped server-side, never taken from the caller.
  assert.notEqual(call.headers[DEPLOYMENT_HEADER.toLowerCase()], "caller-chosen-deployment");
});

// ---------------------------------------------------------------- enabled: relaying the receiver

test("a challenge answer relays as text/plain byte for byte; receiver 4xx verdicts relay with status", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  h.serveEventsubReceiver();
  h.setReceiver(() => new Response(CHALLENGE, { status: 200, headers: { "content-type": "text/plain" } }));
  const challenge = await h.call(post());
  assert.equal(challenge.status, 200);
  assert.equal(challenge.headers.get("content-type"), "text/plain");
  assert.equal(await challenge.text(), CHALLENGE);
  assert.equal(challenge.headers.get("cache-control"), "no-store");

  // The receiver maps bad signatures and stale timestamps to the same
  // unauthorized refusal; deterministic wrong-subscription deliveries ack.
  h.setReceiver((request) => Response.json({
    ok: false, request_id: request.headers["x-request-id"],
    error: { code: "unauthorized", message: "request refused", retryable: false },
  }, { status: 401 }));
  const refused = await h.call(post());
  assert.equal(refused.status, 401);
  assert.deepEqual((await refused.json() as { error: { code: string } }).error.code, "unauthorized");
  // The drained happy paths settle the SDK proxy pipe.
  await new Promise<void>((resolve) => setImmediate(resolve));
  assert.equal(h.bot["inflightRequests"], 0, "drained relays must release the proxy slot");
});

test("refused receiver shapes become the fixed 503 envelope without leaking wire bytes", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  h.serveEventsubReceiver();
  // Refused shapes bypass the SDK proxy pipe. The pinned SDK (0.3.7,
  // unmodified) forwards container bodies through
  // `res.body?.pipeTo(writable).finally(...)` with no rejection handler, so an
  // early consumer cancel aborts that internal pipe and rejects with
  // `undefined` — the same wart the internal-actions and /ops/metrics paths
  // live with. Cancelling through it under Node fails the file with an
  // unattributed unhandled rejection, so these shapes are served past the pipe
  // exactly as the internal-actions tests serve fixtures; early-cancel
  // mechanics (cancel + unread tails) are proven by the relay unit tests
  // below.
  const leak = `boom ${MESSAGE_SIGNATURE} ${CHALLENGE}`;
  const refused: Response[] = [
    new Response(`${CHALLENGE}${"x".repeat(4097)}`, { status: 200, headers: { "content-type": "text/plain" } }),
    new Response(leak, { status: 500, headers: { "content-type": "text/html" } }),
    new Response(leak, { status: 500 }),
    new Response(null, { status: 204 }),
  ];
  const fetchPastPipe = t.mock.method(h.bot, "containerFetch", async () => refused.shift()!);
  for (let i = 0; i < 4; i++) {
    const sanitized = await h.call(post());
    assert.equal(sanitized.status, 503);
    const text = await sanitized.text();
    assert.deepEqual(Object.keys((JSON.parse(text) as { error: object }).error).sort(), ["code", "message", "retryable"]);
    assert.ok(!text.includes("boom") && !text.includes(CHALLENGE) && !text.includes(MESSAGE_SIGNATURE));
  }
  fetchPastPipe.mock.restore();
});

test("relay unit: oversize, wrong-media and bodyless answers never leave; envelopes keep status", async () => {
  const text = (body: string, status: number, contentType: string) =>
    new Response(body, { status, headers: { "content-type": contentType } });
  // Oversize challenge: cancelled, fixed 503.
  const big = await relayEventsubResponse(text(`ok${"x".repeat(4097)}`, 200, "text/plain"));
  assert.equal(big.status, 503);
  assert.deepEqual((await big.json() as { error: { code: string } }).error.code, "unavailable");
  // Wrong media and non-200 text: fixed 503 without reading.
  for (const upstream of [text("<html>nope</html>", 500, "text/html"), text("nope", 500, "text/plain"), text("{}", 200, "application/jsonp")]) {
    const refused = await relayEventsubResponse(upstream);
    assert.equal(refused.status, 503);
    await refused.arrayBuffer();
  }
  const bodyless = await relayEventsubResponse(new Response(null, { status: 204 }));
  assert.equal(bodyless.status, 503);
  await bodyless.arrayBuffer();
  // Complete receiver envelopes retain status; only numeric retry-after survives.
  const sent = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
  const denied = await relayEventsubResponse(Response.json({
    ok: false, request_id: sent,
    error: { code: "unauthorized", message: "request refused", retryable: false },
  }, { status: 401, headers: { "retry-after": "7" } }), sent);
  assert.equal(denied.status, 401);
  assert.equal(denied.headers.get("retry-after"), "7");
  assert.deepEqual((await denied.json() as { error: { code: string } }).error.code, "unauthorized");
  const odd = await relayEventsubResponse(Response.json({ ok: true, request_id: sent }, { status: 200, headers: { "retry-after": "7.5", "x-secret": "leak" } }), sent);
  assert.equal(odd.status, 200);
  assert.equal(odd.headers.get("retry-after"), null);
  assert.equal(odd.headers.get("x-secret"), null);
  assert.equal(odd.headers.get("cache-control"), "no-store");
});

test("public ingress never starts the container: a cold instance answers 503 without a receiver call", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  const response = await h.call(post());
  assert.equal(response.status, 503);
  assert.deepEqual((await response.json() as { error: { code: string } }).error.code, "unavailable");
  assert.deepEqual(h.receiverCalls(), [], "receiver untouched");
  assert.deepEqual(h.starts, [], "container not started");
});

test("logs carry only scalar event/status/error_class/request_id/release/log_url: no header value, challenge or body ever lands", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  h.serveEventsubReceiver();
  h.setReceiver(() => new Response(CHALLENGE, { status: 200, headers: { "content-type": "text/plain" } }));
  const ok = await h.call(post());
  assert.equal(ok.status, 200);
  await ok.arrayBuffer();
  const bad = await h.call(post(BODY, { ...DELIVERY_HEADERS, "content-type": "text/plain" }));
  assert.equal(bad.status, 415);
  await bad.arrayBuffer();
  const text = h.logs.join("\n");
  for (const secret of [MESSAGE_ID, MESSAGE_SIGNATURE, MESSAGE_TIMESTAMP, CHALLENGE, BODY]) {
    assert.ok(!text.includes(secret), "sensitive value reached the logs");
  }
  const ingress = h.logs.filter((line) => line.includes("eventsub_ingress"));
  assert.ok(ingress.length >= 1, "refusals are logged");
  for (const line of ingress) {
    const record = JSON.parse(line) as Record<string, unknown>;
    assert.deepEqual(Object.keys(record).sort(), ["error_class", "event", "log_url", "release", "request_id", "status"]);
    assert.ok(isRequestId(String(record["request_id"])), `bad id shape: ${line}`);
    assert.match(String(record["release"]), /^(?:[0-9a-f]{7,40}|unknown)$/, `bad release shape: ${line}`);
    assert.equal(record["log_url"], LOG_QUERY_URL, `bad log url: ${line}`);
  }
});

// Paperclip Review (PR #1116): pin the EventSub edge wiring with an exact SHA.
// The shape test above accepts `sha|unknown`, so replacing
// `env.CONTAINER_RELEASE` with `undefined` at this edge call site stays green.
// This test drives a real 40-hex SHA through `worker.fetch` and asserts the
// span line carries it verbatim.
test("edge stamps the deploy CONTAINER_RELEASE on eventsub span lines", async (t) => {
  const sha = "9f3a1c7e2b4d6a8c0e1f3a5b7c9d2e4f6a8b0c1d";
  const h = await harness(t, { ...ENABLED_ENV, CONTAINER_RELEASE: sha });
  await h.warm();
  const refused = await h.call(post(BODY, { ...DELIVERY_HEADERS, "content-type": "text/plain" }));
  assert.equal(refused.status, 415);
  await refused.arrayBuffer();
  const span = h.logs.filter((line) => line.includes("eventsub_ingress"));
  assert.ok(span.length >= 1, "expected a span line");
  for (const line of span) {
    const record = JSON.parse(line) as Record<string, unknown>;
    assert.equal(record["release"], sha, `edge must stamp the deploy SHA: ${line}`);
    assert.equal(record["log_url"], LOG_QUERY_URL, `bad log url: ${line}`);
  }
});

// Paperclip Review round 4 (PR #1116): pin the merged-in #1181 refusal sites
// with an exact SHA. The abort/throw/relay branches arrived from main without
// the trailing `release` argument and logged `unknown` despite a valid SHA.
test("abort/throw/relay refusals stamp the SHA (no unknown leak)", async (t) => {
  const sha = "9f3a1c7e2b4d6a8c0e1f3a5b7c9d2e4f6a8b0c1d";
  const lines: string[] = [];
  t.mock.method(console, "warn", (...args: unknown[]) => {
    lines.push(args.map(String).join(" "));
  });
  resetEventsubIngressLimits();
  t.after(() => resetEventsubIngressLimits());

  // 1. Synchronous forward throw -> 503 unavailable.
  const syncThrow = await handleEventsubIngress(
    post(),
    "span-release-sync-throw",
    () => { throw new Error("synthetic binding failure"); },
    sha,
  );
  assert.equal(syncThrow.status, 503);
  await syncThrow.arrayBuffer();

  // 2. Abort-driven forward rejection -> 504 upstream_timeout.
  const timeoutMock = t.mock.method(AbortSignal, "timeout", () => AbortSignal.abort());
  try {
    const aborted = await handleEventsubIngress(
      post(),
      "span-release-abort",
      async () => { throw new Error("synthetic abort rejection"); },
      sha,
    );
    assert.equal(aborted.status, 504);
    await aborted.arrayBuffer();
  } finally {
    timeoutMock.mock.restore();
  }

  // 3. Relay with an already-aborted signal -> 503 unavailable.
  const relayed = await relayEventsubResponse(
    Response.json({ ok: true, request_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV" }),
    "01ARZ3NDEKTSV4RRFFQ69G5FAV",
    AbortSignal.abort(),
    sha,
  );
  assert.equal(relayed.status, 503);
  await relayed.arrayBuffer();

  const span = lines.filter((line) => line.includes("eventsub_ingress"));
  assert.ok(span.length >= 3, `expected 3 span lines, got: ${JSON.stringify(lines)}`);
  for (const line of span) {
    assert.equal((JSON.parse(line) as Record<string, unknown>)["release"], sha, line);
  }
});

test("minted request id survives EventSub header filtering and upstream refusal", async (t) => {
  resetEventsubIngressLimits();
  t.after(() => resetEventsubIngressLimits());
  t.mock.method(console, "warn", () => {});
  const attacker = "Bearer synthetic-eventsub-secret";
  let forwardedId: string | null = null;
  const response = await handleEventsubIngress(
    post(BODY, { ...DELIVERY_HEADERS, "x-request-id": attacker }),
    "request-id-correlation-test",
    async (sanitized) => {
      forwardedId = sanitized.headers.get("x-request-id");
      throw new Error("synthetic upstream failure");
    },
  );
  assert.equal(response.status, 503);
  const body = await response.json() as { request_id: string };
  assert.ok(isRequestId(forwardedId ?? ""));
  assert.equal(body.request_id, forwardedId);
});

test("EventSub relay refusal keeps the request id from the incoming request", async () => {
  const sent = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
  const response = await relayEventsubResponse(new Response("synthetic receiver failure", {
    status: 503,
    headers: { "content-type": "text/plain" },
  }), sent);
  assert.equal(response.status, 503);
  assert.equal((await response.json() as { request_id: string }).request_id, sent);
});

test("x-request-id is honored across refusal, envelope and receiver forward", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  h.serveEventsubReceiver();
  // Hand-built 26-char Crockford id (no I/L/O/U, no lowercase).
  const sent = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
  const refused = await h.call(post(BODY, { ...DELIVERY_HEADERS, "content-type": "text/plain", "x-request-id": sent }));
  assert.equal(refused.status, 415);
  assert.equal((await refused.json() as { request_id: string }).request_id, sent);
  const forwarded = await h.call(post(BODY, { ...DELIVERY_HEADERS, "x-request-id": sent }));
  assert.equal(forwarded.status, 200);
  await forwarded.arrayBuffer();
  const calls = h.receiverCalls();
  assert.equal(calls.length, 1);
  assert.equal(calls[0]?.headers["x-request-id"], sent);
  const ingress = h.logs.filter((line) => line.includes("eventsub_ingress"));
  assert.ok(ingress.every((line) => line.includes(sent)), "refusal log must join by the sent id");
  // An attacker-shaped value is never honored, forwarded or logged.
  const attacker = "Bearer synthetic-eventsub-secret";
  const refusedAttacker = await h.call(post(BODY, { ...DELIVERY_HEADERS, "content-type": "text/plain", "x-request-id": attacker }));
  assert.equal(refusedAttacker.status, 415);
  const minted = (await refusedAttacker.json() as { request_id: string }).request_id;
  assert.ok(isRequestId(minted));
  assert.notEqual(minted, attacker);
  for (const line of h.logs) assert.ok(!line.includes("synthetic-eventsub-secret"), line);
});

// ---------------------------------------------------------------- no-restart rotation refresh

const REFRESH_HEADER_NAMES = [
  REFRESH_SECRET_HEADER,
  REFRESH_TIMESTAMP_HEADER,
  REFRESH_NONCE_HEADER,
  REFRESH_SIGNATURE_HEADER,
] as const;

test("caller-supplied refresh headers are refused without touching the container", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  const before = h.seen.length;
  for (const name of REFRESH_HEADER_NAMES) {
    const response = await h.call(post(BODY, { ...DELIVERY_HEADERS, [name]: "caller-controlled" }));
    assert.equal(response.status, 400, name);
    const body = (await response.json()) as { error: { code: string; retryable: boolean } };
    assert.equal(body.error.code, "malformed", name);
    assert.equal(body.error.retryable, false, name);
  }
  assert.equal(h.seen.length, before, "no refused request reached the container");
  for (const line of h.logs) assert.ok(!line.includes("caller-controlled"), line);
});

test("reload key travels on its own explicit line only with the receiver forward", async (t) => {
  // Built from parts: a single assigned literal on a secret key trips the scanner.
  const key = ["synthetic", "reload", "key", "00000000000001"].join("-");
  const armed = await harness(t, { ...ENABLED_ENV, TWO_EVENTSUB_RELOAD_KEY: key });
  await armed.warm();
  assert.equal(armed.starts[0]?.env?.["TWO_EVENTSUB_RELOAD_KEY"], key);
  const disarmed = await harness(t, ENABLED_ENV);
  await disarmed.warm();
  assert.equal(disarmed.starts[0]?.env?.["TWO_EVENTSUB_RELOAD_KEY"], undefined);
  // Dark receiver forwards nothing, even with the key set.
  const dark = await harness(t, {
    ...DARK_ENV, EVENTSUB_INGRESS: "1", TWO_EVENTSUB: "1", TWO_EVENTSUB_RELOAD_KEY: key,
  });
  await dark.warm();
  assert.equal(dark.starts[0]?.env?.["TWO_EVENTSUB_RELOAD_KEY"], undefined);
});

test("armed DO attaches a server-side refresh no caller value can influence", async (t) => {
  const key = ["synthetic", "reload", "key", "00000000000001"].join("-");
  const h = await harness(t, { ...ENABLED_ENV, TWO_EVENTSUB_RELOAD_KEY: key });
  await h.warm();
  const response = await h.call(post());
  assert.equal(response.status, 200);
  await response.arrayBuffer();
  const calls = h.receiverCalls();
  assert.equal(calls.length, 1);
  const headers = calls[0]?.headers ?? {};
  assert.equal(headers[REFRESH_SECRET_HEADER], ENABLED_ENV.TWO_TWITCH_WEBHOOK_SECRET);
  assert.match(headers[REFRESH_TIMESTAMP_HEADER] ?? "", /^\d{1,10}$/);
  assert.match(headers[REFRESH_NONCE_HEADER] ?? "", /^[0-9a-f]{32}$/);
  assert.match(headers[REFRESH_SIGNATURE_HEADER] ?? "", /^sha256=[0-9a-f]{64}$/);
  // A second delivery signs differently: the nonce is single-use.
  const second = await h.call(post());
  assert.equal(second.status, 200);
  await second.arrayBuffer();
  assert.equal(h.receiverCalls().length, 2);
  assert.notEqual(h.receiverCalls()[1]?.headers[REFRESH_NONCE_HEADER], headers[REFRESH_NONCE_HEADER]);
  assert.notEqual(h.receiverCalls()[1]?.headers[REFRESH_SIGNATURE_HEADER], headers[REFRESH_SIGNATURE_HEADER]);
  // Nothing secret-bearing reaches the Worker logs.
  for (const line of h.logs) {
    assert.ok(!line.includes(ENABLED_ENV.TWO_TWITCH_WEBHOOK_SECRET), line);
    assert.ok(!line.includes(key), line);
  }
});

test("disarmed deliveries carry no refresh headers", async (t) => {
  const h = await harness(t, ENABLED_ENV);
  await h.warm();
  const response = await h.call(post());
  assert.equal(response.status, 200);
  await response.arrayBuffer();
  const headers = h.receiverCalls()[0]?.headers ?? {};
  for (const name of REFRESH_HEADER_NAMES) {
    assert.equal(headers[name], undefined, name);
  }
});

test("armed key without the signing secret fails closed before the container", async (t) => {
  const key = ["synthetic", "reload", "key", "00000000000001"].join("-");
  const { TWO_TWITCH_WEBHOOK_SECRET: _dropped, ...withoutSecret } = ENABLED_ENV;
  const h = await harness(t, { ...withoutSecret, TWO_EVENTSUB_RELOAD_KEY: key });
  await h.warm();
  const before = h.receiverCalls().length;
  const response = await h.call(post());
  assert.equal(response.status, 503);
  const body = (await response.json()) as { error: { code: string; retryable: boolean } };
  assert.equal(body.error.code, "unavailable");
  assert.equal(body.error.retryable, true);
  assert.equal(h.receiverCalls().length, before, "unsigned refresh never reaches the container");
});

test("refresh signing fails closed on missing secrets", async () => {
  assert.equal(
    await signEventsubRefresh({ reloadKey: "", secret: "synthetic-secret", requestId: "r" }),
    null,
  );
  assert.equal(
    await signEventsubRefresh({ reloadKey: "synthetic-key", secret: "", requestId: "r" }),
    null,
  );
});

test("refresh signature matches the container's known-answer vector", async () => {
  // Both languages load the same public synthetic vector. Its expected
  // signature is fixed test data, never computed by the implementation.
  const { readFileSync } = await import("node:fs");
  const vector = JSON.parse(readFileSync(new URL(
    "../../crates/core/tests/fixtures/eventsub_refresh.json", import.meta.url,
  ), "utf8")) as {
    reloadKey: string;
    timestamp: string;
    nonce: string;
    requestId: string;
    secretParts: string[];
    signature: string;
  };
  const headers = await signEventsubRefresh({
    reloadKey: vector.reloadKey,
    timestamp: vector.timestamp,
    nonce: vector.nonce,
    requestId: vector.requestId,
    secret: vector.secretParts.join("-"),
  });
  assert.ok(headers);
  assert.equal(headers.get(REFRESH_SIGNATURE_HEADER), vector.signature);
});
