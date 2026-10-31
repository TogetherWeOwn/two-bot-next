/**
 * Protected acceptance: expire observations and bound the entire browser poll.
 * Drive the exact Worker-served script, without appending private entrypoints.
 * No implementation diff was consulted. Existing acceptance files stay intact.
 *
 * Operational outer bounds are deliberately loose: a hung poll and an old
 * observation must become explicitly unavailable within 60 seconds (four
 * nominal 15-second poll periods). The implementation chooses the shorter
 * deadline/TTL and copy. A request's budget must NOT restart at headers.
 *
 * Breaks caught: unbounded fetch/body waits, per-tick fan-out, uncancelled
 * reconnect/disconnect work, retained green READY, timer leaks, stale writes
 * after abort/timeout, and an old expiry timer invalidating recovered status.
 * Node's test glob registers this file. See the sibling manifest for handoff.
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import vm from "node:vm";
import { installLifecycleHarness, servedLifecyclePage } from "./ops-dashboard-lifecycle-harness.ts";

const OUTER_BOUND_MS = 60_000;
const ID = "synthetic-browser-id-one";
const SECRET = "synthetic-browser-secret-one";
const ID_TWO = "synthetic-browser-id-two";
const SECRET_TWO = "synthetic-browser-secret-two";
const ready = (marker: string) => ({
  components: [["process", "ready"]], jobs: {}, build_revision: marker, build_id: "fixture",
});
const notReady = (marker: string) => ({
  components: [["process", "ready"], ["gateway", "down"]],
  jobs: {}, build_revision: marker, build_id: "fixture",
});

async function page(honourAbort = true) {
  const { html } = await servedLifecyclePage();
  const blocks = [...html.matchAll(/<script>([\s\S]*?)<\/script>/gi)];
  assert.equal(blocks.length, 1, "exercise the one script actually served to staff");
  const listeners = new Map<string, () => unknown>();
  const elements = Object.fromEntries(
    ["summary", "status", "clientId", "clientSecret", "connect", "disconnect"].map((id) => [id, {
      value: "", textContent: "", innerHTML: "", className: "",
      addEventListener: (event: string, fn: () => unknown) => listeners.set(`${id}:${event}`, fn),
    }]),
  );
  elements.clientId!.value = ID;
  elements.clientSecret!.value = SECRET;
  const sandbox: Record<string, unknown> = {
    Date, AbortController, AbortSignal, Headers, DOMException,
    document: { getElementById: (id: string) => elements[id] ?? null },
  };
  const clock = installLifecycleHarness(sandbox, honourAbort);
  vm.createContext(sandbox);
  vm.runInContext(blocks[0]![1]!, sandbox);
  const click = async (id: "connect" | "disconnect") => {
    const fn = listeners.get(`${id}:click`);
    assert.ok(fn, `${id} button must be wired`);
    const result = fn() as Promise<unknown> | undefined;
    if (result && typeof result.catch === "function") result.catch(() => {});
    await clock.flush();
  };
  return {
    ...clock, elements,
    connect: () => click("connect"), disconnect: () => click("disconnect"),
    snapshot: () => ({ status: elements.status!.textContent, summary: elements.summary!.innerHTML }),
    credentials: (id: string, secret: string) => {
      elements.clientId!.value = id; elements.clientSecret!.value = secret;
    },
  };
}
type Page = Awaited<ReturnType<typeof page>>;
function noCurrentReady(p: Page) {
  const { summary } = p.snapshot();
  assert.doesNotMatch(summary, />\s*READY\s*</i, "bare previous READY must not look current");
  assert.doesNotMatch(summary, /class=["'][^"']*\bok\b[^"']*["']/i,
    "expired readiness must not retain its success-color class");
}
function unavailable(p: Page) {
  assert.match(p.snapshot().summary, /unavailable|stale|expired|disconnected|timed?\s*out|timeout/i,
    "the main panel, not only a tiny status line, must explain lost freshness");
  assert.match(`${p.snapshot().status} ${p.snapshot().summary}`, /retry|reconnect|connect|refresh|check|wait|try again/i,
    "show short actionable recovery copy, not a silent old report");
  noCurrentReady(p);
}
async function landReady(p: Page, marker = "first-observation") {
  await p.connect();
  assert.equal(p.requests.length, 1);
  p.requests[0]!.reply(ready(marker));
  await p.flush();
  assert.match(p.snapshot().summary, />READY</, "control: fixture must render real READY first");
  assert.ok(p.snapshot().summary.includes(marker));
}
function nextDeadline(p: Page) {
  const delays = [...p.timeouts.values()].map((t) => t.at - p.now());
  const delay = Math.min(...delays, OUTER_BOUND_MS);
  assert.ok(delay > 1, "deadline must allow an ordinary response");
  return delay;
}
function priorStamp(p: Page) {
  const stamp = p.snapshot().status.replace(/^updated\s+/i, "").trim();
  assert.match(stamp, /\d.*\d/, "successful observation has a human-readable time");
  return stamp;
}
function retainsTime(p: Page, stamp: string) {
  const text = `${p.snapshot().status} ${p.snapshot().summary}`;
  assert.ok(text.includes(stamp), "retain the last observed time while declaring it stale");
}

test("control: valid reports render fresh READY and NOT READY with observation time", async () => {
  const p = await page();
  await landReady(p);
  priorStamp(p);
  await p.tick();
  p.requests.at(-1)!.reply(notReady("parked-report"), 503);
  await p.flush();
  assert.match(p.snapshot().summary, /NOT READY/);
  assert.match(p.snapshot().summary, /parked-report/);
  assert.match(p.snapshot().status, /^updated /);
  await p.disconnect();
});

test("hung response headers become explicitly unavailable and abort within the outer bound", async () => {
  const p = await page();
  await p.connect();
  await p.advance(OUTER_BOUND_MS);
  unavailable(p);
  assert.equal(p.requests[0]!.init.signal?.aborted, true, "timeout cancels transport work");
  assert.equal(p.outstanding(), 0);
});

test("hung json body becomes unavailable within the same whole-poll budget", async () => {
  const p = await page();
  await p.connect();
  const deadline = nextDeadline(p);
  await p.advance(deadline - 1);
  p.requests[0]!.headers();
  await p.flush();
  await p.advance(2);
  unavailable(p);
  assert.equal(p.requests[0]!.init.signal?.aborted, true, "headers must not retire or restart the deadline");
});

for (const phase of ["headers", "body"] as const) {
  test(`a previous READY expires visibly when the following ${phase} never settle`, async () => {
    const p = await page();
    await landReady(p);
    const stamp = priorStamp(p);
    await p.tick();
    if (phase === "body") { p.requests.at(-1)!.headers(); await p.flush(); }
    await p.advance(OUTER_BOUND_MS);
    unavailable(p);
    retainsTime(p, stamp);
    assert.equal(p.requests.at(-1)!.init.signal?.aborted, true);
  });

  test(`repeated interval ticks bound outstanding ${phase} work instead of accumulating requests`, async () => {
    const p = await page();
    await p.connect();
    for (let i = 0; i < 100; i++) {
      if (phase === "body") { p.requests.at(-1)!.headers(); await p.flush(); }
      await p.tick();
      assert.ok(p.outstanding() <= 2, `tick ${i + 1}: at most two un-aborted polls may own transport`);
      assert.ok(p.timeouts.size <= 3, "superseding work must not accumulate deadline timers");
    }
    assert.equal(p.intervals.size, 1, "one polling lifecycle");
    await p.disconnect();
    assert.equal(p.outstanding(), 0, "disconnect cancels the last outstanding request");
    assert.equal(p.timeouts.size, 0, "disconnect clears request and freshness timers");
    assert.equal(p.intervals.size, 0);
  });

  test(`a late ${phase} success cannot resurrect READY after its timeout`, async () => {
    // Deliberately non-cooperative promises: cancellation is not a fencing substitute.
    const p = await page(false);
    await landReady(p);
    await p.tick();
    const old = p.requests.at(-1)!;
    if (phase === "body") { old.headers(); await p.flush(); }
    await p.advance(OUTER_BOUND_MS);
    unavailable(p);
    const timedOut = p.snapshot();
    old.reply(ready("forbidden-late-success"));
    await p.flush();
    assert.deepEqual(p.snapshot(), timedOut, "the timed-out attempt has permanently lost DOM authority");
  });

  test(`Disconnect aborts pending ${phase}, clears timers/credentials, and fences late completion`, async () => {
    const p = await page(false);
    await landReady(p);
    await p.tick();
    const old = p.requests.at(-1)!;
    if (phase === "body") { old.headers(); await p.flush(); }
    await p.disconnect();
    assert.equal(old.init.signal?.aborted, true, "disconnect must actively cancel, not only ignore writes");
    assert.equal(p.intervals.size, 0);
    assert.equal(p.timeouts.size, 0);
    assert.equal(p.elements.clientId!.value, "");
    assert.equal(p.elements.clientSecret!.value, "");
    const disconnected = p.snapshot();
    old.reply(ready("disconnected-late-success"));
    await p.flush();
    await p.advance(OUTER_BOUND_MS * 2);
    assert.deepEqual(p.snapshot(), disconnected);
  });
}

test("Disconnect removes success styling and labels the old observation with its last time", async () => {
  const p = await page();
  await landReady(p);
  const stamp = priorStamp(p);
  await p.disconnect();
  assert.match(p.snapshot().status, /disconnected/i);
  unavailable(p);
  retainsTime(p, stamp);
});

test("Connect while already connected cancels old work and does not present prior READY as fresh", async () => {
  const p = await page(false);
  await landReady(p);
  const stamp = priorStamp(p);
  await p.tick();
  const old = p.requests.at(-1)!;
  p.credentials(ID_TWO, SECRET_TWO);
  await p.connect();
  assert.equal(old.init.signal?.aborted, true, "a new connection supersedes the pending request");
  noCurrentReady(p);
  assert.match(`${p.snapshot().status} ${p.snapshot().summary}`, /connect|refresh|stale|pending/i);
  retainsTime(p, stamp);
  assert.equal(p.intervals.size, 1);
  const connecting = p.snapshot();
  old.reply(notReady("obsolete-connection"), 503);
  await p.flush();
  assert.deepEqual(p.snapshot(), connecting);
  p.requests.at(-1)!.reply(ready("new-connection"));
  await p.flush();
  assert.match(p.snapshot().summary, />READY</);
  assert.match(p.snapshot().summary, /new-connection/);
});

test("freshness expiry labels the panel even without delivery of another interval tick", async () => {
  const p = await page();
  await landReady(p);
  const stamp = priorStamp(p);
  await p.advance(OUTER_BOUND_MS);
  assert.equal(p.requests.length, 1, "isolate freshness from request timeout and polling errors");
  unavailable(p);
  retainsTime(p, stamp);
});

test("recovery after a timed-out body becomes fresh and ignores obsolete rejection", async () => {
  const p = await page(false);
  await landReady(p);
  await p.tick();
  const old = p.requests.at(-1)!;
  old.headers();
  await p.flush();
  await p.advance(OUTER_BOUND_MS);
  unavailable(p);
  await p.tick();
  p.requests.at(-1)!.reply(ready("recovered-observation"));
  await p.flush();
  assert.match(p.snapshot().summary, />READY</);
  assert.match(p.snapshot().summary, /recovered-observation/);
  assert.match(p.snapshot().status, /^updated /);
  const recovered = p.snapshot();
  old.fail(new Error("obsolete-body-error"));
  await p.flush();
  assert.deepEqual(p.snapshot(), recovered);
  await p.disconnect();
  assert.equal(p.timeouts.size, 0);
});

test("an older observation's expiry cannot invalidate a more recent successful observation", async () => {
  const p = await page();
  await landReady(p);
  const firstExpiry = Math.min(...[...p.timeouts.values()].map((t) => t.at));
  assert.ok(Number.isFinite(firstExpiry) && firstExpiry <= OUTER_BOUND_MS,
    "a fresh report needs a finite expiry independent of the next request");
  await p.advance(firstExpiry - 1);
  await p.tick();
  p.requests.at(-1)!.reply(ready("newer-observation"));
  await p.flush();
  const newer = p.snapshot();
  await p.advance(2);
  assert.deepEqual(p.snapshot(), newer, "the previous observation's timer must lose authority");
  await p.advance(OUTER_BOUND_MS);
  unavailable(p);
});

test("control: lifecycle transport is GET-only, credentials rotate only in memory, and unsafe report strings escape", async () => {
  const p = await page();
  await landReady(p);
  await p.tick();
  p.requests.at(-1)!.fail(new Error(`network ${SECRET}`));
  await p.flush();
  await p.disconnect();
  p.credentials(ID_TWO, SECRET_TWO);
  await p.connect();
  p.requests.at(-1)!.reply({
    components: [["gate<img src=x>", "do&wn"]], jobs: {}, build_revision: "r", build_id: "b",
  }, 503);
  await p.flush();
  assert.match(p.snapshot().summary, /&lt;img/);
  assert.match(p.snapshot().summary, /do&amp;wn/);
  assert.doesNotMatch(p.snapshot().summary, /<img/);
  for (const [i, request] of p.requests.entries()) {
    assert.equal(request.url, "/ops/summary");
    assert.equal(request.init.method ?? "GET", "GET");
    assert.equal(request.init.body, undefined);
    const headers = new Headers(request.init.headers);
    assert.equal(headers.get("cf-access-client-id"), i === p.requests.length - 1 ? ID_TWO : ID);
    assert.equal(headers.get("cf-access-client-secret"), i === p.requests.length - 1 ? SECRET_TWO : SECRET);
  }
  await p.disconnect();
  const publicText = JSON.stringify([p.snapshot(), p.logs, p.storageWrites]);
  for (const value of [ID, SECRET, ID_TWO, SECRET_TWO]) assert.ok(!publicText.includes(value));
  assert.deepEqual(p.storageWrites, []);
});

test("control: a malformed report after READY replaces success instead of renewing freshness", async () => {
  const p = await page();
  await landReady(p);
  await p.tick();
  p.requests.at(-1)!.reply({ components: "not-a-component-list", jobs: {} });
  await p.flush();
  noCurrentReady(p);
  assert.match(p.snapshot().summary, /invalid|malformed|unavailable|fail/i);
  await p.disconnect();
});

test("control: exact served inline bytes retain strict hash-based CSP", async () => {
  const { html, csp } = await servedLifecyclePage();
  assert.ok(csp);
  const actual = Object.fromEntries(csp.split(";").map((d) => d.trim().split(/\s+/)).filter(([k]) => k)
    .map(([key, ...values]) => [key, values]));
  const expected: Record<string, string[]> = {
    "default-src": ["'none'"], "connect-src": ["'self'"], "base-uri": ["'none'"],
    "form-action": ["'none'"], "frame-ancestors": ["'none'"],
  };
  for (const tag of ["script", "style"]) {
    const blocks = [...html.matchAll(new RegExp(`<${tag}>([\\s\\S]*?)<\\/${tag}>`, "gi"))];
    assert.equal(blocks.length, 1);
    expected[`${tag}-src`] = [`'sha256-${createHash("sha256").update(blocks[0]![1]!).digest("base64")}'`];
  }
  assert.deepEqual(actual, expected);
  assert.doesNotMatch(csp, /unsafe-inline|unsafe-eval/);
  assert.doesNotMatch(html, /\s(?:on[\w:-]+|style)\s*=/i);
});
