import { test } from "node:test";
import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import worker, { type Env } from "../src/index.ts";
import { handleAdminGateway, adminAuthMetrics } from "../src/admin-gateway.ts";
import { relayReceiverResponse } from "../src/internal-actions.ts";

const env = {
  ADMIN_GATEWAY_SERVICE_KEY_ID: "fixture-admin-service",
  ADMIN_GATEWAY_SERVICE_KEY: randomBytes(32).toString("hex"),
};
const bearer = `adm_0123456789abcdef.${randomBytes(32).toString("hex")}`;
let caller = 0;
function request(auth = `Bearer ${bearer}`): Request {
  return new Request("https://worker.invalid/admin/v1/settings.get/123", {
    headers: { authorization: auth, "cf-connecting-ip": `fixture-admin-${++caller}` },
  });
}
function refusal(req: Request, code = "action_not_allowed", challenge?: string): Response {
  return Response.json({ ok: false, request_id: req.headers.get("x-request-id"),
    error: { code, retryable: false } }, {
    status: code === "action_not_allowed" ? 403 : 401,
    headers: challenge ? { "www-authenticate": challenge } : {},
  });
}
function count(name: string, text = adminAuthMetrics()): number {
  const value = new RegExp(`^two_bot_admin_gateway_${name}_total (\\d+)$`, "m").exec(text);
  assert.ok(value, `${name} counter must be exposed`);
  return Number(value[1]);
}

test("closed gateway refuses even a valid correlated receiver success", async () => {
  const response = await handleAdminGateway(request(), env, async (req) =>
    Response.json({ ok: true, result: { unexpected: "never expose" }, request_id: req.headers.get("x-request-id") }));
  assert.equal(response.status, 403);
  const body = await response.json() as { error: { code: string }; result?: unknown };
  assert.equal(body.error.code, "action_not_allowed");
  assert.equal(body.result, undefined);
  assert.ok(!JSON.stringify(body).includes("never expose"));
});

test("service authentication failures are unavailable, not bearer guesses, across both relays", async () => {
  for (const [code, challenge, expected] of [
    ["unauthorized", undefined, 503], // missing ring entry or invalid service MAC
    ["stale_request", undefined, 503],
    ["stale_request", "Bearer", 503], // a challenge alone is not sufficient
    ["unauthorized", "Basic", 503],
    ["unauthorized", "Bearer injected", 503],
    ["unauthorized", "Bearer", 401], // only the registry adds this fixed challenge
  ] as const) {
    const before = count("unauthorized");
    const response = await handleAdminGateway(request(), env, async (req) =>
      relayReceiverResponse(refusal(req, code, challenge), req.headers.get("x-request-id")!));
    assert.equal(response.status, expected, `${code}/${challenge}`);
    assert.equal(response.headers.get("www-authenticate"), expected === 401 ? "Bearer" : null);
    assert.equal(count("unauthorized") - before, expected === 401 ? 1 : 0);
    if (expected === 503) {
      assert.equal((await response.json() as { error: { code: string } }).error.code, "unavailable");
    } else assert.equal(await response.text(), "unauthorized");
  }
});

test("undeclared streamed intake cancels immediately at 64 KiB, before consuming the tail", async () => {
  let pulls = 0;
  let cancelled = false;
  let forwards = 0;
  const stream = new ReadableStream<Uint8Array>({
    pull(controller) {
      pulls++;
      if (pulls === 1) controller.enqueue(new TextEncoder().encode(" ".repeat(65537)));
      else { controller.enqueue(new TextEncoder().encode('{"guild_id":"123"}')); controller.close(); }
    },
    cancel() { cancelled = true; },
  }, { highWaterMark: 0 });
  const req = new Request("https://worker.invalid/admin/v1/settings.get", {
    method: "POST", headers: { authorization: `Bearer ${bearer}`, "content-type": "application/json", "cf-connecting-ip": `fixture-admin-${++caller}` },
    body: stream, duplex: "half",
  } as RequestInit);
  assert.equal(req.headers.get("content-length"), null);
  const response = await handleAdminGateway(req, env, async (forwarded) => { forwards++; return refusal(forwarded); });
  assert.equal(response.status, 400);
  assert.equal(forwards, 0);
  assert.equal(pulls, 1, "must not collect or parse the oversized tail");
  assert.equal(cancelled, true);
});

test("eight hanging forwards exhaust admission; six-second deadlines retain slots until settlement", async (t) => {
  const deadline = new AbortController();
  const timeouts: number[] = [];
  t.mock.method(AbortSignal, "timeout", (ms: number) => { timeouts.push(ms); return deadline.signal; });
  const pending: { req: Request; resolve: (response: Response) => void }[] = [];
  let allStarted!: () => void;
  const started = new Promise<void>((resolve) => { allStarted = resolve; });
  const forward = (req: Request) => {
    // A cap mutant must fail, not hang while awaiting its ninth forward.
    if (pending.length >= 8) return Promise.resolve(refusal(req));
    return new Promise<Response>((resolve) => {
    pending.push({ req, resolve });
    if (pending.length === 8) allStarted();
    });
  };
  const calls = Array.from({ length: 8 }, () => handleAdminGateway(request(), env, forward));
  try {
    await started;
    assert.deepEqual(timeouts, Array(8).fill(6000));
    const ninth = await handleAdminGateway(request(), env, forward);
    assert.equal(ninth.status, 429);
    assert.equal(ninth.headers.get("retry-after"), "1");
    assert.equal(pending.length, 8, "no ninth forward or retry");
    deadline.abort();
    for (const response of await Promise.all(calls)) {
      assert.equal(response.status, 504);
      assert.equal((await response.json() as { error: { code: string } }).error.code, "upstream_timeout");
    }
    assert.equal((await handleAdminGateway(request(), env, forward)).status, 429, "abort is not proof that the forward stopped");
    assert.equal(pending.length, 8);
  } finally {
    deadline.abort();
    for (const { req, resolve } of pending) resolve(refusal(req));
    await Promise.all(calls);
    await new Promise<void>((resolve) => setImmediate(resolve));
    t.mock.restoreAll();
  }
  assert.equal((await handleAdminGateway(request(), env, async (req) => refusal(req))).status, 403, "settled forwards release admission");
});

test("forward rejection returns a fixed 503 without retry or bearer counter", async () => {
  const before = count("unauthorized");
  let attempts = 0;
  const response = await handleAdminGateway(request(), env, async () => { attempts++; throw new Error("synthetic-private-error"); });
  assert.equal(response.status, 503);
  assert.equal(attempts, 1);
  assert.ok(!(await response.text()).includes("synthetic-private-error"));
  assert.equal(count("unauthorized"), before);
});

test("authenticated metrics append refusal counters only when the gateway is configured, with bounded intake", async () => {
  const scrapeToken = randomBytes(32).toString("hex");
  let body = "# TYPE fixture counter\nfixture 1\n";
  const bindings = {
    ...env, CF_VERSION_METADATA: { id: "fixture-deployment" },
    METRICS_SCRAPE_TOKEN: scrapeToken, REDIRECT_MAPPINGS_JSON: "[]", REDIRECT_FALLBACK_CODE: "",
    TWO_BOT: { getByName: () => ({ fetch: async () => new Response(body, { headers: { "content-type": "text/plain", "content-length": String(body.length) } }) }) },
  } as unknown as Env;
  const scrape = (configured = bindings) => worker.fetch(new Request("https://worker.invalid/ops/metrics", {
    headers: { authorization: `Bearer ${scrapeToken}`, "cf-connecting-ip": `fixture-scrape-${++caller}` },
  }), configured, {} as ExecutionContext);
  const initial = await scrape();
  assert.equal(initial.status, 200);
  assert.equal(initial.headers.get("content-length"), null, "appended body must not retain upstream length");
  const before = await initial.text();
  const unauthenticated = request("");
  const guessingIp = unauthenticated.headers.get("cf-connecting-ip")!;
  assert.equal((await handleAdminGateway(unauthenticated, env, async () => { throw new Error("must not forward"); })).status, 401);
  let throttled = false;
  for (let i = 0; i < 30; i++) {
    const req = request("");
    req.headers.set("cf-connecting-ip", guessingIp);
    const response = await handleAdminGateway(req, env, async () => { throw new Error("must not forward"); });
    if (response.status === 429) { throttled = true; break; }
    assert.equal(response.status, 401);
  }
  assert.ok(throttled);
  const after = await (await scrape()).text();
  assert.ok(count("unauthorized", after) > count("unauthorized", before));
  assert.ok(count("throttled", after) > count("throttled", before));
  const dark = { ...bindings, ADMIN_GATEWAY_SERVICE_KEY: undefined };
  assert.equal(await (await scrape(dark)).text(), body);
  body = "x".repeat(65537);
  const oversize = await scrape();
  assert.equal(oversize.status, 503);
  assert.equal(await oversize.text(), "metrics unavailable\n");
});
