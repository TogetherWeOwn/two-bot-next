/**
 * Acceptance tests: a current fetch failure must replace stale readiness.
 *
 * Contract pinned here, driven only through the served page's public
 * surface (Connect/Disconnect clicks and the polling interval):
 *
 * - When the current poll's `fetch` rejects (network failure, abort of the
 *   live request, Worker unreachable) after a valid READY or NOT READY
 *   report rendered, the previous report must be replaced with an explicit
 *   unavailable/failure summary. The stale heading, component states, job
 *   values and build stamp must not stay on screen as if current; only the
 *   existing `fetch failed` status line changing is not enough.
 * - A rejected obsolete poll (older sequence in the same connection, or a
 *   previous generation after Disconnect/reconnect) must not clear a
 *   newer successful report or overwrite a disconnected state.
 * - A following valid poll recovers on the same connection (no reconnect
 *   click, still exactly one interval); valid 200/503 reports, HTML
 *   escaping, and the existing HTTP-refusal / invalid-report failure
 *   states are unchanged (passing controls).
 *
 * The failure summary's exact wording is the implementer's choice: tests
 * require a non-empty summary matching /fail|unavailable/i with every
 * stale marker gone, so any explicit failure rendering satisfies them.
 * The fake fetch honours `AbortSignal`, so an implementation that aborts
 * superseded polls sees the same rejection a browser would deliver, and
 * snapshot-equality assertions keep the obsolete-poll cases independent
 * of how the staleness guard is implemented.
 *
 * Run: node --import ./test/cloudflare-loader.mjs --test test/ops-dashboard-fetch-failure.test.ts
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import worker, { type Env } from "../src/index.ts";
import { OPS_DASHBOARD_PATH, OPS_SUMMARY_PATH } from "../src/ops-dashboard.ts";

const CLIENT_ID = "synthetic-dashboard-client-id-0123456789";
const CLIENT_SECRET = "synthetic-dashboard-client-secret-0123456789abcdef";
const ID_1 = "test-id-one";
const SECRET_1 = "test-secret-one";
const ID_2 = "test-id-two";
const SECRET_2 = "test-secret-two";

async function dashboardScript(): Promise<string> {
  const env = {
    CF_VERSION_METADATA: { id: "synthetic-fetch-failure-deployment" },
    OPS_DASHBOARD_CLIENT_ID: CLIENT_ID,
    OPS_DASHBOARD_CLIENT_SECRET: CLIENT_SECRET,
    REDIRECT_MAPPINGS_JSON: "[]",
    TWO_BOT: {
      getByName: () => ({
        fetch: async () => new Response("{}", { status: 503 }),
      }),
    },
  } as unknown as Env;
  const res = await worker.fetch(
    new Request(`https://w.invalid${OPS_DASHBOARD_PATH}`),
    env,
    {} as ExecutionContext,
  );
  assert.equal(res.status, 200, "dashboard page must serve");
  const html = await res.text();
  const match = html.match(/<script>([\s\S]*?)<\/script>/i);
  assert.ok(match, "page must contain one inline script");
  return match![1]!;
}

interface StubResponse {
  status: number;
  headers: { get: (name: string) => string | null };
  json: () => Promise<unknown>;
}

interface FetchInitLike {
  method?: string;
  headers?: Record<string, string>;
  signal?: AbortSignal;
}

interface PendingFetch {
  url: string;
  init?: FetchInitLike;
  resolve: (res: StubResponse) => void;
  reject: (err: unknown) => void;
}

function abortError(): Error {
  const err = new Error("The operation was aborted");
  err.name = "AbortError";
  return err;
}

interface El {
  value: string;
  textContent: string;
  innerHTML: string;
  addEventListener?: (event: string, fn: () => unknown) => void;
}

const flush = async () => {
  for (let i = 0; i < 10; i++) await new Promise<void>((r) => setImmediate(r));
};

function makePage(script: string) {
  const listeners = new Map<string, () => unknown>();
  const el = (value = ""): El => ({ value, textContent: "", innerHTML: "" });
  const elements: Record<string, El> = {
    summary: el(),
    status: el(),
    clientId: el(ID_1),
    clientSecret: el(SECRET_1),
    connect: el(),
    disconnect: el(),
  };
  for (const id of ["connect", "disconnect"]) {
    elements[id]!.addEventListener = (event: string, fn: () => unknown) => {
      listeners.set(`${id}:${event}`, fn);
    };
  }

  const fetches: PendingFetch[] = [];
  const fetchImpl = (url: string, init?: FetchInitLike): Promise<StubResponse> =>
    new Promise<StubResponse>((resolvePromise, rejectPromise) => {
      let settled = false;
      const pending: PendingFetch = {
        url,
        init,
        resolve: (res) => {
          if (!settled) {
            settled = true;
            resolvePromise(res);
          }
        },
        reject: (err) => {
          if (!settled) {
            settled = true;
            rejectPromise(err);
          }
        },
      };
      fetches.push(pending);
      const signal = init?.signal;
      if (signal) {
        if (signal.aborted) pending.reject(abortError());
        else {
          signal.addEventListener("abort", () => pending.reject(abortError()), {
            once: true,
          });
        }
      }
    });

  const intervals = new Map<number, () => unknown>();
  let nextIntervalId = 1;

  const logs: string[] = [];
  const record = (...args: unknown[]) => {
    logs.push(args.map((a) => String(a)).join(" "));
  };
  const storageWrites: string[] = [];
  const storage = {
    getItem: () => null,
    setItem: (key: string, value: string) => {
      storageWrites.push(`${key}=${value}`);
    },
    removeItem: () => {},
    clear: () => {},
  };

  const sandbox: Record<string, unknown> = {
    document: {
      getElementById: (id: string) => elements[id] ?? null,
    },
    fetch: fetchImpl,
    setInterval: (fn: () => unknown) => {
      const id = nextIntervalId++;
      intervals.set(id, fn);
      return id;
    },
    clearInterval: (id: number) => {
      intervals.delete(id);
    },
    setTimeout: (fn: () => void, ms: number) => setTimeout(fn, ms).unref(),
    clearTimeout,
    AbortController,
    localStorage: storage,
    sessionStorage: storage,
    console: { log: record, warn: record, error: record, info: record },
  };
  vm.createContext(sandbox);
  vm.runInContext(script, sandbox);

  const invoke = (fn: () => unknown) => {
    const out: unknown = fn();
    if (out instanceof Promise) out.catch(() => {});
  };

  const click = async (id: "connect" | "disconnect") => {
    const fn = listeners.get(`${id}:click`);
    assert.ok(fn, `page must wire the ${id} button`);
    invoke(fn);
    await flush();
  };

  return {
    elements,
    fetches,
    intervals,
    logs,
    storageWrites,
    clickConnect: () => click("connect"),
    clickDisconnect: () => click("disconnect"),
    tick: async () => {
      assert.equal(intervals.size, 1, "exactly one live interval must tick");
      const [fn] = [...intervals.values()];
      invoke(fn!);
      await flush();
    },
    snapshot: () => ({
      status: elements["status"]!.textContent,
      summary: elements["summary"]!.innerHTML,
    }),
    setCredentials: (id: string, secret: string) => {
      elements["clientId"]!.value = id;
      elements["clientSecret"]!.value = secret;
    },
  };
}

function jsonResponse(status: number, data: unknown): StubResponse {
  return {
    status,
    headers: {
      get: (name: string) =>
        name.toLowerCase() === "content-type" ? "application/json" : null,
    },
    json: async () => data,
  };
}

function refusalResponse(status: number): StubResponse {
  return {
    status,
    headers: {
      get: (name: string) =>
        name.toLowerCase() === "content-type" ? "text/plain" : null,
    },
    json: async () => {
      throw new Error("no json body");
    },
  };
}

/** A READY report whose every rendered field carries a unique marker. */
const readyBody = (marker: string) => ({
  components: [[`comp-${marker}`, "ready"]],
  jobs: {
    [`job-${marker}`]: { last_success: `success-${marker}`, consecutive_failures: 0 },
  },
  build_revision: `build-${marker}`,
  build_id: "b",
});

/** A NOT READY report whose every rendered field carries a unique marker. */
const notReadyBody = (marker: string) => ({
  components: [
    [`comp-${marker}`, "ready"],
    [`down-${marker}`, "down"],
  ],
  jobs: {
    [`job-${marker}`]: { last_success: `success-${marker}`, consecutive_failures: 3 },
  },
  gateway_failure: { phase: `phase-${marker}`, class: `class-${marker}` },
  build_revision: `build-${marker}`,
  build_id: "b",
});

type Page = ReturnType<typeof makePage>;

/**
 * The summary after a current fetch failure: an explicit failure state,
 * not the previous report. Every marker from the stale report is gone and
 * neither readiness heading survives.
 */
function assertFailureSummary(page: Page, staleMarker: string, context: string) {
  const { status, summary } = page.snapshot();
  assert.equal(status, "fetch failed", `${context}: status line must stay "fetch failed"`);
  assert.ok(summary.length > 0, `${context}: summary must show an explicit failure state`);
  assert.match(
    summary,
    /fail|unavailable/i,
    `${context}: summary must read as a failure/unavailable state, got: ${summary}`,
  );
  for (const stale of [
    staleMarker,
    `comp-${staleMarker}`,
    `down-${staleMarker}`,
    `job-${staleMarker}`,
    `success-${staleMarker}`,
    `build-${staleMarker}`,
    `phase-${staleMarker}`,
    `class-${staleMarker}`,
    ">READY<",
    "NOT READY",
  ]) {
    assert.ok(
      !summary.includes(stale),
      `${context}: stale value ${JSON.stringify(stale)} must not be presented as current`,
    );
  }
}

/** Connect and land one valid report, asserting the control render. */
async function pageWithReport(
  script: string,
  status: number,
  body: ReturnType<typeof readyBody> | ReturnType<typeof notReadyBody>,
  marker: string,
): Promise<Page> {
  const page = makePage(script);
  await page.clickConnect();
  assert.equal(page.fetches.length, 1, "connect must start a poll");
  page.fetches[0]!.resolve(jsonResponse(status, body));
  await flush();
  const snap = page.snapshot();
  assert.ok(snap.summary.includes(marker), "control: the valid report renders first");
  assert.ok(snap.status.startsWith("updated "), "control: valid poll stamps updated");
  return page;
}

test("current fetch rejection after READY replaces the stale report", async () => {
  const page = await pageWithReport(await dashboardScript(), 200, readyBody("alpha"), "alpha");
  assert.ok(page.snapshot().summary.includes(">READY<"), "control: READY renders");
  await page.tick();
  assert.equal(page.fetches.length, 2, "interval must start the failing poll");
  page.fetches[1]!.reject(new Error("synthetic network failure"));
  await flush();
  assertFailureSummary(page, "alpha", "READY then current rejection");
});

test("current fetch rejection after NOT READY replaces the stale report", async () => {
  const page = await pageWithReport(await dashboardScript(), 503, notReadyBody("bravo"), "bravo");
  assert.ok(page.snapshot().summary.includes("NOT READY"), "control: NOT READY renders");
  await page.tick();
  page.fetches[1]!.reject(new TypeError("synthetic fetch failure"));
  await flush();
  assertFailureSummary(page, "bravo", "NOT READY then current rejection");
});

test("current fetch rejection on the first poll shows a failure summary", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  page.fetches[0]!.reject(new Error("synthetic network failure"));
  await flush();
  const { status, summary } = page.snapshot();
  assert.equal(status, "fetch failed");
  assert.ok(summary.length > 0, "first-poll failure must not leave the summary blank");
  assert.match(summary, /fail|unavailable/i);
  assert.ok(!summary.includes(">READY<") && !summary.includes("NOT READY"));
});

test("consecutive current rejections keep the failure summary, never resurrect stale data", async () => {
  const page = await pageWithReport(await dashboardScript(), 200, readyBody("charlie"), "charlie");
  await page.tick();
  page.fetches[1]!.reject(new Error("synthetic network failure"));
  await flush();
  assertFailureSummary(page, "charlie", "first current rejection");
  await page.tick();
  assert.equal(page.fetches.length, 3, "polling must continue after a fetch failure");
  page.fetches[2]!.reject(new Error("synthetic network failure"));
  await flush();
  assertFailureSummary(page, "charlie", "second current rejection");
});

test("a valid poll after a current fetch failure recovers without reconnecting", async () => {
  const page = await pageWithReport(await dashboardScript(), 200, readyBody("delta"), "delta");
  await page.tick();
  page.fetches[1]!.reject(new Error("synthetic network failure"));
  await flush();
  assertFailureSummary(page, "delta", "current rejection before recovery");
  // No Connect click: the same live interval drives the recovery poll.
  await page.tick();
  assert.equal(page.fetches.length, 3);
  assert.equal(page.intervals.size, 1, "recovery keeps the one live interval");
  page.fetches[2]!.resolve(jsonResponse(200, readyBody("echo")));
  await flush();
  const snap = page.snapshot();
  assert.ok(snap.summary.includes(">READY<"), "recovery poll must render READY again");
  assert.ok(snap.summary.includes("echo"), "recovery must show the new report");
  assert.ok(!snap.summary.includes("delta"), "recovery must not mix in the stale report");
  assert.ok(snap.status.startsWith("updated "), "recovery must stamp updated");
});

test("a 503 poll after a current fetch failure renders NOT READY without reconnecting", async () => {
  const page = await pageWithReport(await dashboardScript(), 200, readyBody("foxtrot"), "foxtrot");
  await page.tick();
  page.fetches[1]!.reject(new Error("synthetic network failure"));
  await flush();
  assertFailureSummary(page, "foxtrot", "current rejection before 503 recovery");
  await page.tick();
  page.fetches[2]!.resolve(jsonResponse(503, notReadyBody("golf")));
  await flush();
  const snap = page.snapshot();
  assert.ok(snap.summary.includes("NOT READY"), "503 recovery must render NOT READY");
  assert.ok(snap.summary.includes("golf"));
  assert.ok(snap.status.startsWith("updated "));
});

test("control: an older rejection cannot clear a newer successful report", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  await page.tick();
  assert.equal(page.fetches.length, 2);
  page.fetches[1]!.resolve(jsonResponse(200, readyBody("hotel")));
  await flush();
  const beforeStale = page.snapshot();
  assert.ok(beforeStale.summary.includes("hotel"), "control: newer poll renders");
  page.fetches[0]!.reject(new Error("synthetic network failure"));
  await flush();
  assert.deepEqual(page.snapshot(), beforeStale);
});

test("control: a rejection landing after Disconnect cannot overwrite the disconnected state", async () => {
  const page = await pageWithReport(await dashboardScript(), 200, readyBody("india"), "india");
  await page.tick();
  assert.equal(page.fetches.length, 2);
  await page.clickDisconnect();
  const beforeStale = page.snapshot();
  assert.equal(beforeStale.status, "disconnected");
  page.fetches[1]!.reject(new Error("synthetic network failure"));
  await flush();
  assert.deepEqual(page.snapshot(), beforeStale);
});

test("control: an obsolete-generation rejection cannot clear the reconnected report", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  await page.clickDisconnect();
  page.setCredentials(ID_2, SECRET_2);
  await page.clickConnect();
  assert.equal(page.fetches.length, 2, "reconnect must start a fresh poll");
  page.fetches[1]!.resolve(jsonResponse(200, readyBody("juliett")));
  await flush();
  const beforeStale = page.snapshot();
  assert.ok(beforeStale.summary.includes("juliett"), "control: new connection renders");
  page.fetches[0]!.reject(new Error("synthetic network failure"));
  await flush();
  assert.deepEqual(page.snapshot(), beforeStale);
});

test("an obsolete-generation rejection cannot pre-empt a visibly nonfresh reconnect poll", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  await page.clickDisconnect();
  page.setCredentials(ID_2, SECRET_2);
  await page.clickConnect();
  const beforeStale = page.snapshot();
  assert.ok(!beforeStale.status.startsWith("updated "), "reconnect has no fresh report yet");
  assert.match(beforeStale.summary, /connecting|pending|waiting|unavailable|stale/i);
  assert.ok(!beforeStale.summary.includes(">READY<"), "pending reconnect is not fresh readiness");
  assert.ok(!beforeStale.summary.includes('class="ok"'), "pending reconnect is not success");
  assert.equal(page.fetches[0]!.init?.signal?.aborted, true, "old connection work is cancelled");
  assert.equal(page.fetches[1]!.init?.signal?.aborted, false, "reconnect poll remains active");
  page.fetches[0]!.reject(new Error("synthetic network failure"));
  await flush();
  assert.deepEqual(page.snapshot(), beforeStale);
  page.fetches[1]!.resolve(jsonResponse(200, readyBody("kilo")));
  await flush();
  assert.ok(page.snapshot().summary.includes("kilo"), "the pending current poll still renders");
});

test("control: a current HTTP refusal still replaces a prior READY report", async () => {
  const page = await pageWithReport(await dashboardScript(), 200, readyBody("lima"), "lima");
  await page.tick();
  page.fetches[1]!.resolve(refusalResponse(401));
  await flush();
  const snap = page.snapshot();
  assert.equal(snap.status, "summary 401");
  assert.ok(snap.summary.includes("summary request failed: 401"));
  assert.ok(!snap.summary.includes("lima"), "refusal must not keep the stale report");
});

test("control: a current invalid report still replaces a prior READY report", async () => {
  const page = await pageWithReport(await dashboardScript(), 200, readyBody("mike"), "mike");
  await page.tick();
  page.fetches[1]!.resolve(jsonResponse(200, { components: [["only-name"]] }));
  await flush();
  const snap = page.snapshot();
  assert.equal(snap.status, "summary 200");
  assert.ok(snap.summary.includes("summary report invalid: 200"));
  assert.ok(!snap.summary.includes("mike"), "invalid report must not keep the stale report");
});

test("control: valid 200/503 rendering and HTML escaping are unchanged", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  page.fetches[0]!.resolve(
    jsonResponse(503, {
      components: [["gate<img src=x>", "do&wn"]],
      jobs: {},
      build_revision: "r",
      build_id: "b",
    }),
  );
  await flush();
  const { summary } = page.snapshot();
  assert.ok(summary.includes("NOT READY"), "503 must render NOT READY");
  assert.ok(!summary.includes("<img"), "raw markup must not reach the DOM");
  assert.ok(summary.includes("&lt;img"), "angle brackets must be escaped");
  assert.ok(summary.includes("do&amp;wn"), "ampersands must be escaped");
});

test("control: polls stay GET-only on the summary path across a fetch failure", async () => {
  const page = await pageWithReport(await dashboardScript(), 200, readyBody("november"), "november");
  await page.tick();
  page.fetches[1]!.reject(new Error("synthetic network failure"));
  await flush();
  await page.tick();
  assert.equal(page.fetches.length, 3, "polling continues after the failure");
  for (const call of page.fetches) {
    assert.equal(call.url, OPS_SUMMARY_PATH);
    assert.equal(call.init?.method ?? "GET", "GET", "poll must not use a mutating method");
    assert.equal(call.init?.headers?.["cf-access-client-id"], ID_1);
    assert.equal(call.init?.headers?.["cf-access-client-secret"], SECRET_1);
  }
});

test("no credential is persisted, logged or rendered on the fetch-failure path", async () => {
  const page = await pageWithReport(await dashboardScript(), 200, readyBody("oscar"), "oscar");
  await page.tick();
  page.fetches[1]!.reject(new Error("synthetic network failure"));
  await flush();
  const rendered = `${page.snapshot().status}\n${page.snapshot().summary}`;
  for (const secret of [SECRET_1, ID_1]) {
    assert.ok(!rendered.includes(secret), "credentials must never render");
    assert.ok(
      !page.logs.some((line) => line.includes(secret)),
      "credentials must never be logged",
    );
    assert.ok(
      !page.storageWrites.some((line) => line.includes(secret)),
      "credentials must never be persisted",
    );
  }
});
