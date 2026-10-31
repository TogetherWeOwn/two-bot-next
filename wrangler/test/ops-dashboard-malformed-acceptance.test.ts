/**
 * Protected acceptance suite: malformed ops-summary reports.
 *
 * Drives the actually served dashboard page script (fetched through the
 * Worker, executed in a Node VM) with queued summary responses. The
 * contract under test:
 *
 * - A malformed readiness JSON shape (null component entries, null job
 *   values, non-pair component entries, a non-object report) settles the
 *   poll without an unhandled rejection.
 * - A malformed report replaces any previously rendered readiness with an
 *   explicit failure state: the summary carries the page's existing
 *   failure styling (`class="bad"`) plus unavailable/invalid/failed text,
 *   and shows neither the READY nor the NOT READY heading.
 * - The `updated <timestamp>` status is written only after a report has
 *   been successfully validated and rendered, so a malformed report never
 *   leaves the previous READY result displayed as current.
 * - The next valid poll recovers in the same page session, with no
 *   reconnect, and every poll keeps sending both service-token headers.
 * - Valid 200 READY, valid 503 NOT READY, escaped dynamic text and the
 *   existing non-JSON failure line are unchanged (the control tests).
 *
 * Synthetic stubs only — no endpoint, database, credential or deployment
 * is contacted.
 *
 * Run: node --import ./test/cloudflare-loader.mjs --test test/ops-dashboard-malformed-acceptance.test.ts
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import worker, { type Env } from "../src/index.ts";
import {
  DASHBOARD_CLIENT_ID_HEADER,
  DASHBOARD_CLIENT_SECRET_HEADER,
  OPS_DASHBOARD_PATH,
} from "../src/ops-dashboard.ts";

const DEPLOYMENT = "synthetic-malformed-acceptance-deployment";
const CLIENT_ID = "synthetic-malformed-client-id-0123456789";
const CLIENT_SECRET = "synthetic-malformed-client-secret-0123456789abcdef";

function workerEnv(): Env {
  return {
    CF_VERSION_METADATA: { id: DEPLOYMENT },
    OPS_DASHBOARD_CLIENT_ID: CLIENT_ID,
    OPS_DASHBOARD_CLIENT_SECRET: CLIENT_SECRET,
    REDIRECT_MAPPINGS_JSON: "[]",
    TWO_BOT: {
      getByName: () => ({
        fetch: async () =>
          new Response("unreachable in page-script acceptance tests", { status: 500 }),
      }),
    },
  } as unknown as Env;
}

const ctx = {} as ExecutionContext;
let callerSeq = 0;

async function servedPageScript(): Promise<string> {
  const headers = new Headers({ "cf-connecting-ip": `acceptance-caller-${++callerSeq}` });
  const res = await worker.fetch(
    new Request(`https://w.invalid${OPS_DASHBOARD_PATH}`, { headers }),
    workerEnv(),
    ctx,
  );
  assert.equal(res.status, 200, "dashboard page must be served");
  const html = await res.text();
  const match = html.match(/<script>([\s\S]*?)<\/script>/i);
  assert.ok(match, "page must contain one inline script");
  return match![1]!;
}

type QueuedResponse = {
  status: number;
  contentType: string;
  /** Value returned by `res.json()`; omit to make `res.json()` throw. */
  data?: unknown;
};

const json = (status: number, data: unknown): QueuedResponse => ({
  status,
  contentType: "application/json",
  data,
});

const VALID_READY = {
  components: [["process", "ready"]],
  jobs: { sweep: { last_success: "2026-10-18T00:00:00Z", consecutive_failures: 0 } },
  build_revision: "r1",
  build_id: "b1",
};

function runPageScript(script: string, queue: QueuedResponse[]) {
  const listeners = new Map<string, () => void>();
  const fetchCalls: { url: string; headers: Record<string, string> }[] = [];
  const elements: Record<
    string,
    {
      value?: string;
      textContent?: string;
      innerHTML?: string;
      addEventListener?: (event: string, fn: () => void) => void;
    }
  > = {
    summary: { innerHTML: "" },
    status: { textContent: "disconnected" },
    clientId: { value: "test-id" },
    clientSecret: { value: "test-secret" },
    connect: {},
    disconnect: {},
  };
  for (const id of ["connect", "disconnect"]) {
    elements[id]!.addEventListener = (event: string, fn: () => void) => {
      listeners.set(`${id}:${event}`, fn);
    };
  }
  const pending = [...queue];
  const fetchImpl = async (url: string, init?: { headers?: Record<string, string> }) => {
    fetchCalls.push({ url, headers: init?.headers ?? {} });
    const next = pending.shift();
    assert.ok(next, "page polled more times than the test queued responses");
    return {
      status: next.status,
      headers: {
        get: (name: string) =>
          name.toLowerCase() === "content-type" ? next.contentType : null,
      },
      json: async () => {
        if (!("data" in next)) throw new Error("no json body");
        return next.data;
      },
    };
  };
  const sandbox: Record<string, unknown> = {
    document: {
      getElementById: (id: string) => elements[id] ?? null,
    },
    fetch: fetchImpl,
    setInterval: () => 0,
    clearInterval: () => {},
    setTimeout: (fn: () => void, ms: number) => setTimeout(fn, ms).unref(),
    clearTimeout,
    AbortController,
    console,
  };
  vm.createContext(sandbox);
  // Expose poll for the drive below; the served-script parse surface is
  // covered by the existing dashboard suite, so appending this hook cannot
  // hide a SyntaxError in the raw script.
  vm.runInContext(`${script}\n;globalThis.__hook = { poll };`, sandbox);
  const hook = vm.runInContext("__hook", sandbox) as { poll: () => Promise<void> };
  return { elements, listeners, hook, fetchCalls };
}

function assertReady(elements: ReturnType<typeof runPageScript>["elements"], label: string) {
  const html = elements["summary"]!.innerHTML!;
  assert.ok(html.includes(">READY<"), `${label}: must show READY`);
  assert.ok(!html.includes("NOT READY"), `${label}: must not show NOT READY`);
  assert.ok(
    elements["status"]!.textContent!.startsWith("updated "),
    `${label}: status must stamp the update time`,
  );
}

function assertExplicitFailureState(
  elements: ReturnType<typeof runPageScript>["elements"],
  label: string,
) {
  const html = elements["summary"]!.innerHTML!;
  assert.ok(!html.includes(">READY<"), `${label}: stale READY heading must be replaced`);
  assert.ok(!html.includes("NOT READY"), `${label}: malformed is not a NOT READY report`);
  assert.ok(
    html.includes('class="bad"'),
    `${label}: failure state must use the page's bad styling, got: ${html}`,
  );
  assert.ok(
    /unavailable|invalid|failed/i.test(html),
    `${label}: failure state must say the report is unavailable/invalid/failed, got: ${html}`,
  );
  assert.ok(
    !elements["status"]!.textContent!.startsWith("updated "),
    `${label}: status must not claim a fresh update for a malformed report, got: ${elements["status"]!.textContent}`,
  );
}

function assertServiceTokenHeaders(
  fetchCalls: ReturnType<typeof runPageScript>["fetchCalls"],
  label: string,
) {
  assert.ok(fetchCalls.length > 0, `${label}: poll must fetch the summary`);
  for (const call of fetchCalls) {
    assert.equal(call.url, "/ops/summary", `${label}: poll target`);
    assert.equal(
      call.headers[DASHBOARD_CLIENT_ID_HEADER],
      "test-id",
      `${label}: every poll sends the client id header`,
    );
    assert.equal(
      call.headers[DASHBOARD_CLIENT_SECRET_HEADER],
      "test-secret",
      `${label}: every poll sends the client secret header`,
    );
  }
}

// --- Controls: behavior that must stay unchanged (green before the fix) ---

test("control: valid 200 renders READY and stamps updated", async () => {
  const script = await servedPageScript();
  const { elements, hook, fetchCalls } = runPageScript(script, [json(200, VALID_READY)]);
  await assert.doesNotReject(hook.poll());
  assertReady(elements, "valid 200");
  assert.ok(elements["summary"]!.innerHTML!.includes("process"), "lists the component");
  assert.ok(elements["summary"]!.innerHTML!.includes("sweep"), "lists the job");
  assertServiceTokenHeaders(fetchCalls, "valid 200");
});

test("control: valid 503 renders NOT READY with the down component", async () => {
  const script = await servedPageScript();
  const { elements, hook } = runPageScript(script, [
    json(503, {
      components: [["process", "ready"], ["gateway", "down"]],
      jobs: {},
      gateway_failure: { phase: "connect", class: "timeout" },
      build_revision: "r1",
      build_id: "b1",
    }),
  ]);
  await assert.doesNotReject(hook.poll());
  const html = elements["summary"]!.innerHTML!;
  assert.ok(html.includes("NOT READY"), "503 must show NOT READY");
  assert.ok(html.includes("gateway") && html.includes("down"), "lists the down component");
  assert.ok(
    elements["status"]!.textContent!.startsWith("updated "),
    "a valid 503 report is still a successful update",
  );
});

test("control: dynamic text in a valid report is escaped", async () => {
  const script = await servedPageScript();
  const { elements, hook } = runPageScript(script, [
    json(200, {
      components: [['<b>&"x', "ready"]],
      jobs: { "<job>": { last_success: "<t>", consecutive_failures: 0 } },
      build_revision: "<r>",
      build_id: "b1",
    }),
  ]);
  await assert.doesNotReject(hook.poll());
  const html = elements["summary"]!.innerHTML!;
  assert.ok(html.includes("&lt;b&gt;&amp;&quot;x"), "component name must be escaped");
  assert.ok(html.includes("&lt;job&gt;"), "job name must be escaped");
  assert.ok(html.includes("&lt;t&gt;"), "job timestamp must be escaped");
  assert.ok(html.includes("&lt;r&gt;"), "build revision must be escaped");
  assert.ok(!html.includes('<b>&"x'), "raw component name must not reach the DOM");
});

test("control: non-JSON 401 keeps the existing failure line", async () => {
  const script = await servedPageScript();
  const { elements, hook } = runPageScript(script, [
    { status: 401, contentType: "text/plain" },
  ]);
  await assert.doesNotReject(hook.poll());
  assert.equal(elements["status"]!.textContent, "summary 401");
  assert.ok(
    elements["summary"]!.innerHTML!.includes("summary request failed: 401"),
    "existing refusal rendering is unchanged",
  );
});

// --- Malformed shapes from a clean slate ---

const MALFORMED: { name: string; data: unknown }[] = [
  {
    name: "null component entry",
    data: { components: [null], jobs: {}, build_revision: "r1", build_id: "b1" },
  },
  {
    name: "null job value",
    data: {
      components: [["process", "ready"]],
      jobs: { demo: null },
      build_revision: "r1",
      build_id: "b1",
    },
  },
  {
    name: "null component entry after a valid entry",
    data: {
      components: [["process", "ready"], null],
      jobs: {},
      build_revision: "r1",
      build_id: "b1",
    },
  },
  {
    name: "non-pair component entry",
    data: { components: [42], jobs: {}, build_revision: "r1", build_id: "b1" },
  },
  { name: "top-level null report", data: null },
  { name: "top-level array report", data: [] },
];

for (const { name, data } of MALFORMED) {
  test(`malformed 200 (${name}) settles poll with an explicit failure state`, async () => {
    const script = await servedPageScript();
    const { elements, hook, fetchCalls } = runPageScript(script, [json(200, data)]);
    await assert.doesNotReject(
      hook.poll(),
      `${name}: poll must not reject on a malformed report`,
    );
    assertExplicitFailureState(elements, name);
    assertServiceTokenHeaders(fetchCalls, name);
  });
}

test("malformed 503 (null component entry) is a failure state, not NOT READY", async () => {
  const script = await servedPageScript();
  const { elements, hook } = runPageScript(script, [
    json(503, { components: [null], jobs: {}, build_revision: "r1", build_id: "b1" }),
  ]);
  await assert.doesNotReject(hook.poll(), "malformed 503 must settle the poll");
  assertExplicitFailureState(elements, "malformed 503");
});

// --- Stale-readiness replacement and next-poll recovery ---

for (const { name, data } of MALFORMED.slice(0, 2)) {
  test(`stale READY is replaced when the next poll is malformed (${name})`, async () => {
    const script = await servedPageScript();
    const { elements, hook } = runPageScript(script, [
      json(200, VALID_READY),
      json(200, data),
    ]);
    await assert.doesNotReject(hook.poll());
    assertReady(elements, "initial valid poll");
    const readyHtml = elements["summary"]!.innerHTML!;

    await assert.doesNotReject(
      hook.poll(),
      `${name}: the malformed poll must settle, not reject`,
    );
    assert.notEqual(
      elements["summary"]!.innerHTML,
      readyHtml,
      `${name}: the previous READY report must not stay on screen`,
    );
    assertExplicitFailureState(elements, name);
  });
}

test("next valid poll recovers after a malformed report without reconnecting", async () => {
  const script = await servedPageScript();
  const recovered = {
    components: [["gateway", "ready"]],
    jobs: {},
    build_revision: "r2",
    build_id: "b2",
  };
  const { elements, hook, fetchCalls } = runPageScript(script, [
    json(200, VALID_READY),
    json(200, { components: [null], jobs: {}, build_revision: "r1", build_id: "b1" }),
    json(200, recovered),
  ]);

  await assert.doesNotReject(hook.poll());
  assertReady(elements, "initial valid poll");

  await assert.doesNotReject(hook.poll(), "malformed poll must settle");
  assertExplicitFailureState(elements, "malformed poll");

  // Same page session, same poll function: no Connect click, no reload.
  await assert.doesNotReject(hook.poll());
  assertReady(elements, "recovery poll");
  assert.ok(
    elements["summary"]!.innerHTML!.includes("gateway"),
    "recovery renders the new report, not the stale one",
  );
  assert.ok(elements["summary"]!.innerHTML!.includes("r2"), "recovery shows the new build");
  assert.equal(fetchCalls.length, 3, "all three polls fetched the summary");
  assertServiceTokenHeaders(fetchCalls, "recovery sequence");
});
