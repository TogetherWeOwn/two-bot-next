/**
 * Poll-deadline render pin: a hanging poll must visibly time out at the
 * deadline, not ride on the previous READY until freshness expiry.
 *
 * Drives the exact Worker-served script with controllable time (via the
 * read-only lifecycle harness import; protected acceptance files untouched).
 * Lands a fresh READY, starts a poll that never settles, advances just past
 * the poll deadline (well before the 30s freshness expiry), and asserts the
 * timeout render owns both the status line and the main panel while keeping
 * the last-observed time. Disabling the deadline render must fail this test.
 *
 * Run: node --import ./test/cloudflare-loader.mjs --test test/ops-dashboard-poll-deadline.test.ts
 */
import { test } from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import { installLifecycleHarness, servedLifecyclePage } from "./ops-dashboard-lifecycle-harness.ts";

const ID = "synthetic-deadline-client-id";
const SECRET = "synthetic-deadline-client-secret";
const ready = (marker: string) => ({
  components: [["process", "ready"]], jobs: {}, build_revision: marker, build_id: "fixture",
});

async function page() {
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
  const clock = installLifecycleHarness(sandbox, true);
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
  };
}

test("a hanging poll renders the timeout state at the deadline, before freshness expiry", async () => {
  const p = await page();
  await p.connect();
  assert.equal(p.requests.length, 1, "connect must start a poll");
  p.requests[0]!.reply(ready("deadline-pin-first"));
  await p.flush();
  assert.match(p.snapshot().summary, />READY</, "control: fixture must render real READY first");
  const stamp = p.snapshot().status.replace(/^updated\s+/i, "").trim();
  assert.match(stamp, /\d.*\d/, "successful observation has a human-readable time");

  // Start a second poll and leave it hanging on headers: no reply, no failure.
  await p.tick();
  assert.equal(p.requests.length, 2, "interval must start the hanging poll");

  // Advance to just past the poll deadline. The pending one-shots are the
  // 30s freshness expiry from the landed observation and the shorter poll
  // deadline, so the minimum delay isolates the deadline render.
  const delays = [...p.timeouts.values()].map((t) => t.at - p.now());
  const deadline = Math.min(...delays);
  assert.ok(deadline > 1, "deadline must allow an ordinary response");
  await p.advance(deadline + 1);

  const snap = p.snapshot();
  assert.equal(snap.status, "summary timed out", "the deadline render must own the status line");
  assert.match(snap.summary, /timed?\s*out|timeout/i, "the main panel must explain the timeout");
  assert.ok(snap.summary.includes(stamp), "retain the last observed time while declaring it stale");
  assert.doesNotMatch(snap.summary, />\s*READY\s*</i, "timed-out readiness must not look current");
  assert.doesNotMatch(snap.summary, /class=["'][^"']*\bok\b[^"']*["']/i,
    "timeout must not retain success styling");
  await p.disconnect();
});
