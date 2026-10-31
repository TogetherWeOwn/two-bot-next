/**
 * Acceptance: an ingress forward that outlives the request deadline is an
 * orphan, and orphans must be aborted without freeing their in-flight slot.
 *
 * Both mirrored Worker ingress routes (internal actions and EventSub) race
 * the fenced forward against REQUEST_TIMEOUT_MS and answer a fixed 504 when
 * the deadline passes. The behavior pinned here:
 *
 * - at the deadline the forwarded request's abort signal fires, so a real
 *   downstream fetch is cancelled instead of running untracked;
 * - the per-isolate inFlight slot stays held until the orphaned forward
 *   actually settles, so a burst of stalled upstreams cannot hide behind
 *   deadline refusals and exceed MAX_IN_FLIGHT live forwards;
 * - a late settle (resolve or reject, including the rejection a real fetch
 *   produces when its signal aborts) releases the slot exactly once, keeps
 *   the caller's 504, and never surfaces as an unhandled rejection.
 *
 * The handlers are called directly with synthetic signed fixtures and a
 * stubbed forward; no Container, Discord, Twitch, database or secret is
 * contacted.
 * Run: node --import ./test/cloudflare-loader.mjs --test test/ingress-orphan-acceptance.test.ts
 */
import { test, type TestContext } from "node:test";
import assert from "node:assert/strict";
import {
  ACTIONS_PATH,
  MAX_IN_FLIGHT as ACTIONS_MAX_IN_FLIGHT,
  REQUEST_TIMEOUT_MS as ACTIONS_REQUEST_TIMEOUT_MS,
  handleActionsIngress,
  resetIngressLimits,
} from "../src/internal-actions.ts";
import {
  EVENTSUB_PATH,
  MAX_IN_FLIGHT as EVENTSUB_MAX_IN_FLIGHT,
  REQUEST_TIMEOUT_MS as EVENTSUB_REQUEST_TIMEOUT_MS,
  handleEventsubIngress,
  resetEventsubIngressLimits,
} from "../src/eventsub.ts";

const BODY = '{"synthetic":true}';

// Valid-shaped (never verified here) signing fixtures, mirrored from the
// neighboring suites. Fixed alphabets only: no key material.
function actionsRequest(): Request {
  return new Request(`https://worker.invalid${ACTIONS_PATH}`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "x-two-key-id": "web-staging",
      "x-two-timestamp": "1790000000",
      "x-two-nonce": "synthetic-nonce-0001",
      "x-two-signature": `sha256=${"a".repeat(64)}`,
    },
    body: BODY,
  });
}

function eventsubRequest(): Request {
  return new Request(`https://worker.invalid${EVENTSUB_PATH}`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "twitch-eventsub-message-id": "synthetic-delivery-0001",
      "twitch-eventsub-message-timestamp": "2026-10-13T00:00:00.000Z",
      "twitch-eventsub-message-signature": `sha256=${"b".repeat(64)}`,
      "twitch-eventsub-message-type": "notification",
    },
    body: BODY,
  });
}

type Forward = (sanitized: Request) => Promise<Response>;

interface Route {
  name: string;
  timeoutMs: number;
  maxInFlight: number;
  handle: (request: Request, clientKey: string, forward: Forward) => Promise<Response>;
  reset: () => void;
  makeRequest: () => Request;
}

const ROUTES: Route[] = [
  {
    name: "internal-actions",
    timeoutMs: ACTIONS_REQUEST_TIMEOUT_MS,
    maxInFlight: ACTIONS_MAX_IN_FLIGHT,
    handle: handleActionsIngress,
    reset: resetIngressLimits,
    makeRequest: actionsRequest,
  },
  {
    name: "eventsub",
    timeoutMs: EVENTSUB_REQUEST_TIMEOUT_MS,
    maxInFlight: EVENTSUB_MAX_IN_FLIGHT,
    handle: handleEventsubIngress,
    reset: resetEventsubIngressLimits,
    makeRequest: eventsubRequest,
  },
];

let clientCounter = 0;
/** A fresh bucket key per request: the per-client buckets are module state. */
function clientKey(route: Route): string {
  return `${route.name}-orphan-acceptance-${++clientCounter}`;
}

async function flush(turns = 10): Promise<void> {
  for (let i = 0; i < turns; i++) {
    await new Promise<void>((resolve) => setImmediate(resolve));
  }
}

async function until(condition: () => boolean, what: string): Promise<void> {
  for (let i = 0; i < 200; i++) {
    if (condition()) return;
    await new Promise<void>((resolve) => setImmediate(resolve));
  }
  assert.fail(`timed out waiting for ${what}`);
}

interface Captured {
  signal: AbortSignal;
  abortEvents: number;
  resolve: (response: Response) => void;
  reject: (error: unknown) => void;
}

/** A forward that never settles on its own and ignores its abort signal. */
function stalledForward(captured: Captured[]): Forward {
  return (sanitized) => {
    let resolve!: Captured["resolve"];
    let reject!: Captured["reject"];
    const pending = new Promise<Response>((res, rej) => {
      resolve = res;
      reject = rej;
    });
    const entry: Captured = { signal: sanitized.signal, abortEvents: 0, resolve, reject };
    sanitized.signal.addEventListener("abort", () => {
      entry.abortEvents++;
    });
    captured.push(entry);
    return pending;
  };
}

function ok(): Response {
  return Response.json({ ok: true });
}

async function errorCode(response: Response): Promise<string> {
  return ((await response.json()) as { error: { code: string } }).error.code;
}

function guard(t: TestContext, route: Route): void {
  route.reset();
  t.after(() => route.reset());
  t.mock.method(console, "warn", () => {});
}

for (const route of ROUTES) {
  test(`${route.name}: the deadline is the exported 20s request timeout at an 8-wide cap`, () => {
    assert.equal(route.timeoutMs, 20_000);
    assert.equal(route.maxInFlight, 8);
  });

  test(`${route.name}: a fast upstream relays and frees its slot (control)`, async (t) => {
    guard(t, route);
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const first = await route.handle(route.makeRequest(), clientKey(route), async () => ok());
    assert.equal(first.status, 200);
    assert.deepEqual(await first.json(), { ok: true });
    // Long past the deadline nothing about the completed request changes,
    // and the slot was returned exactly once.
    t.mock.timers.tick(route.timeoutMs * 2);
    await flush();
    const second = await route.handle(route.makeRequest(), clientKey(route), async () => ok());
    assert.equal(second.status, 200);
  });

  test(`${route.name}: a fast upstream rejection answers fixed 503 and frees its slot (control)`, async (t) => {
    guard(t, route);
    const refused = await route.handle(route.makeRequest(), clientKey(route), async () => {
      throw new Error("synthetic upstream failure");
    });
    assert.equal(refused.status, 503);
    assert.equal(await errorCode(refused), "unavailable");
    const after = await route.handle(route.makeRequest(), clientKey(route), async () => ok());
    assert.equal(after.status, 200, "capacity returns once the forward settles");
  });

  test(`${route.name}: held forwards refuse the over-cap request before any deadline (control)`, async (t) => {
    guard(t, route);
    const captured: Captured[] = [];
    const forward = stalledForward(captured);
    const held = Array.from({ length: route.maxInFlight }, () =>
      route.handle(route.makeRequest(), clientKey(route), forward));
    await until(() => captured.length === route.maxInFlight, "held forwards to start");
    let overForwarded = false;
    const over = await route.handle(route.makeRequest(), clientKey(route), async () => {
      overForwarded = true;
      return ok();
    });
    assert.equal(over.status, 429);
    assert.equal(over.headers.get("retry-after"), "1");
    assert.equal(await errorCode(over), "rate_limited");
    assert.equal(overForwarded, false, "an over-cap refusal never reaches the forward");
    for (const entry of captured) entry.resolve(ok());
    for (const response of await Promise.all(held)) assert.equal(response.status, 200);
    const after = await route.handle(route.makeRequest(), clientKey(route), async () => ok());
    assert.equal(after.status, 200, "capacity returns once the held forwards settle");
  });

  test(`${route.name}: at the deadline the caller gets 504 and the orphaned forward is aborted`, async (t) => {
    guard(t, route);
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const captured: Captured[] = [];
    let settled = false;
    const pending = route.handle(route.makeRequest(), clientKey(route), stalledForward(captured));
    void pending.then(() => {
      settled = true;
    });
    await until(() => captured.length === 1, "the forward to start");
    const [entry] = captured;
    assert.ok(entry);

    t.mock.timers.tick(route.timeoutMs - 1);
    await flush();
    assert.equal(settled, false, "the caller must not be answered before the deadline");
    assert.equal(entry.signal.aborted, false, "the forward must not abort before the deadline");
    assert.equal(entry.abortEvents, 0);

    t.mock.timers.tick(1);
    const response = await pending;
    assert.equal(response.status, 504);
    assert.equal(await errorCode(response), "upstream_timeout");
    assert.equal(entry.signal.aborted, true, "the deadline must abort the orphaned forward");
    assert.equal(entry.abortEvents, 1, "the abort fires exactly once");

    entry.resolve(ok());
    await flush();
  });

  test(`${route.name}: timed-out orphans keep their slots and still refuse the 9th request`, async (t) => {
    guard(t, route);
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const captured: Captured[] = [];
    const forward = stalledForward(captured);
    const pending = Array.from({ length: route.maxInFlight }, () =>
      route.handle(route.makeRequest(), clientKey(route), forward));
    await until(() => captured.length === route.maxInFlight, "all forwards to start");
    assert.equal(
      new Set(captured.map((entry) => entry.signal)).size,
      route.maxInFlight,
      "every forward carries its own abort signal",
    );

    t.mock.timers.tick(route.timeoutMs);
    const responses = await Promise.all(pending);
    for (const response of responses) {
      assert.equal(response.status, 504);
      assert.equal(await errorCode(response), "upstream_timeout");
    }
    for (const entry of captured) {
      assert.equal(entry.signal.aborted, true, "every timed-out forward is aborted");
    }

    // The forwards are still pending (the stub ignores the abort), so every
    // slot is still held: a fresh request is refused at the cap, upstream
    // untouched, even though all eight callers were already answered.
    let ninthForwarded = false;
    const ninth = await route.handle(route.makeRequest(), clientKey(route), async () => {
      ninthForwarded = true;
      return ok();
    });
    assert.equal(ninth.status, 429, "a slot held by a pending orphan still counts");
    assert.equal(ninth.headers.get("retry-after"), "1");
    assert.equal(await errorCode(ninth), "rate_limited");
    assert.equal(ninthForwarded, false);

    for (const entry of captured) entry.resolve(ok());
    await flush();
    const after = await route.handle(route.makeRequest(), clientKey(route), async () => ok());
    assert.equal(after.status, 200, "slots return when the orphans finally settle");
  });

  test(`${route.name}: a late response after the deadline frees the slot exactly once`, async (t) => {
    guard(t, route);
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const captured: Captured[] = [];
    const pending = route.handle(route.makeRequest(), clientKey(route), stalledForward(captured));
    await until(() => captured.length === 1, "the forward to start");
    t.mock.timers.tick(route.timeoutMs);
    const response = await pending;
    assert.equal(response.status, 504);
    assert.equal(response.headers.get("content-type"), "application/json");
    const [entry] = captured;
    assert.ok(entry);
    assert.equal(entry.signal.aborted, true);

    // The orphan answers late; the caller keeps its 504 and the slot is
    // returned now, at settle time.
    entry.resolve(ok());
    await flush();
    const after = await route.handle(route.makeRequest(), clientKey(route), async () => ok());
    assert.equal(after.status, 200, "the late settle released the held slot");

    // Exactly once: if the deadline and the late settle both released, the
    // isolate would now admit one forward too many.
    const heldCaptures: Captured[] = [];
    const heldForward = stalledForward(heldCaptures);
    const held = Array.from({ length: route.maxInFlight }, () =>
      route.handle(route.makeRequest(), clientKey(route), heldForward));
    await until(() => heldCaptures.length === route.maxInFlight, "the refill forwards to start");
    const over = await route.handle(route.makeRequest(), clientKey(route), async () => ok());
    assert.equal(over.status, 429, "no phantom slot from a double release");
    for (const heldEntry of heldCaptures) heldEntry.resolve(ok());
    for (const heldResponse of await Promise.all(held)) assert.equal(heldResponse.status, 200);
  });

  test(`${route.name}: a forward rejected by its own abort keeps the 504 and frees the slot`, async (t) => {
    guard(t, route);
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const unhandled: unknown[] = [];
    const onUnhandled = (reason: unknown) => {
      unhandled.push(reason);
    };
    process.on("unhandledRejection", onUnhandled);
    t.after(() => {
      process.removeListener("unhandledRejection", onUnhandled);
    });

    // The shape a real fetch has: the abort rejects the forward.
    let abortSeen = false;
    const forward: Forward = (sanitized) =>
      new Promise<Response>((_resolve, reject) => {
        sanitized.signal.addEventListener("abort", () => {
          abortSeen = true;
          reject(new Error("synthetic abort"));
        });
      });
    const pending = route.handle(route.makeRequest(), clientKey(route), forward);
    await flush();
    t.mock.timers.tick(route.timeoutMs);
    const response = await pending;
    assert.equal(response.status, 504, "an abort-driven rejection never downgrades the deadline answer");
    assert.equal(await errorCode(response), "upstream_timeout");
    assert.equal(abortSeen, true, "the deadline aborted the forward");
    await flush();
    assert.deepEqual(unhandled, [], "the orphan rejection is always handled");
    const after = await route.handle(route.makeRequest(), clientKey(route), async () => ok());
    assert.equal(after.status, 200, "the aborted forward released its slot on settle");
  });

  test(`${route.name}: a late rejection after the deadline is handled and frees the slot`, async (t) => {
    guard(t, route);
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const unhandled: unknown[] = [];
    const onUnhandled = (reason: unknown) => {
      unhandled.push(reason);
    };
    process.on("unhandledRejection", onUnhandled);
    t.after(() => {
      process.removeListener("unhandledRejection", onUnhandled);
    });

    const captured: Captured[] = [];
    const pending = route.handle(route.makeRequest(), clientKey(route), stalledForward(captured));
    await until(() => captured.length === 1, "the forward to start");
    t.mock.timers.tick(route.timeoutMs);
    const response = await pending;
    assert.equal(response.status, 504);
    const [entry] = captured;
    assert.ok(entry);
    assert.equal(entry.signal.aborted, true);

    // The orphan ignores the abort and fails on its own, late.
    entry.reject(new Error("synthetic late failure"));
    await flush();
    assert.deepEqual(unhandled, [], "a late orphan rejection is never unhandled");
    const after = await route.handle(route.makeRequest(), clientKey(route), async () => ok());
    assert.equal(after.status, 200, "the late rejection released the held slot");
  });
}
