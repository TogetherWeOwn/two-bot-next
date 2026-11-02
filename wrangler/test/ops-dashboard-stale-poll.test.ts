/**
 * Acceptance tests: the ops dashboard must ignore obsolete poll results.
 *
 * Contract pinned here, driven only through the page's public surface
 * (Connect/Disconnect clicks and the polling interval):
 *
 * - A poll that started under a previous connection (Disconnect, or
 *   Disconnect + reconnect) must never write the DOM when it completes,
 *   however it completes: success, HTTP refusal, fetch rejection, JSON
 *   rejection, or a `json()` body that only arrives late.
 * - Within one connection, an older poll must never overwrite a newer
 *   completed result, in either readiness direction and for error
 *   completions too.
 * - Connect/reconnect keeps exactly one live polling interval; Disconnect
 *   clears the interval and the credential inputs and leaves the
 *   disconnected status authoritative.
 * - Rendering, escaping, GET-only access to `/ops/summary`, and the
 *   in-order happy path are unchanged (passing controls).
 *
 * Obsolete completions are checked by snapshot-equality on the rendered
 * status + summary. Explicit cancellation checks also require superseded
 * work to abort and disconnected/pending states to remain visibly nonfresh.
 * The fake fetch honours `AbortSignal` so an aborting implementation sees
 * the same rejection a browser would deliver.
 *
 * Run: node --import ./test/cloudflare-loader.mjs --test test/ops-dashboard-stale-poll.test.ts
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
    CF_VERSION_METADATA: { id: "synthetic-stale-poll-deployment" },
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

  // Invoke without awaiting the returned value: a handler/tick may return
  // the poll promise itself, which stays pending until the test settles
  // the fake fetch. The catch guard keeps a returned rejection observed.
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

function deferredJsonResponse(status: number) {
  let resolveJson!: (data: unknown) => void;
  let rejectJson!: (err: unknown) => void;
  const jsonPromise = new Promise<unknown>((res, rej) => {
    resolveJson = res;
    rejectJson = rej;
  });
  const response: StubResponse = {
    status,
    headers: {
      get: (name: string) =>
        name.toLowerCase() === "content-type" ? "application/json" : null,
    },
    json: () => jsonPromise,
  };
  return { response, resolveJson, rejectJson };
}

const readyBody = (marker: string) => ({
  components: [["process", "ready"]],
  jobs: {},
  build_revision: marker,
  build_id: "b",
});

const notReadyBody = (marker: string) => ({
  components: [
    ["process", "ready"],
    ["gateway", "down"],
  ],
  jobs: {},
  build_revision: marker,
  build_id: "b",
});

type Page = ReturnType<typeof makePage>;

/** Connect, land one READY render, then disconnect with a poll pending. */
async function pageDisconnectedWithPendingPoll(script: string): Promise<Page> {
  const page = makePage(script);
  await page.clickConnect();
  assert.equal(page.fetches.length, 1, "connect must start a poll");
  page.fetches[0]!.resolve(jsonResponse(200, readyBody("first-render")));
  await flush();
  assert.ok(
    page.snapshot().summary.includes("first-render"),
    "control: the live poll renders before disconnect",
  );
  await page.tick();
  assert.equal(page.fetches.length, 2, "interval must start a second poll");
  await page.clickDisconnect();
  assert.equal(page.snapshot().status, "disconnected");
  return page;
}

test("control: a live poll renders READY with an updated stamp", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  page.fetches[0]!.resolve(jsonResponse(200, readyBody("live-marker")));
  await flush();
  const snap = page.snapshot();
  assert.ok(snap.summary.includes(">READY<"), "200 must render READY");
  assert.ok(snap.summary.includes("live-marker"));
  assert.ok(snap.status.startsWith("updated "), "200 must stamp updated");
});

test("stale success after Disconnect cannot overwrite the disconnected state", async () => {
  const page = await pageDisconnectedWithPendingPoll(await dashboardScript());
  const before = page.snapshot();
  page.fetches[1]!.resolve(jsonResponse(200, readyBody("stale-marker")));
  await flush();
  assert.deepEqual(page.snapshot(), before);
  assert.ok(!page.snapshot().summary.includes("stale-marker"));
});

test("stale success after Disconnect cannot populate a never-rendered page", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  await page.clickDisconnect();
  const before = page.snapshot();
  assert.equal(before.status, "disconnected");
  assert.match(before.summary, /disconnected|unavailable|stale/i, "disconnect must be visible");
  assert.ok(!before.summary.includes(">READY<"), "disconnect is not fresh readiness");
  assert.ok(!before.summary.includes('class="ok"'), "disconnect must remove success styling");
  assert.equal(page.fetches[0]!.init?.signal?.aborted, true, "disconnect cancels the pending poll");
  page.fetches[0]!.resolve(jsonResponse(200, readyBody("stale-marker")));
  await flush();
  assert.deepEqual(page.snapshot(), before);
});

test("stale HTTP refusal after Disconnect cannot overwrite the disconnected state", async () => {
  const page = await pageDisconnectedWithPendingPoll(await dashboardScript());
  const before = page.snapshot();
  page.fetches[1]!.resolve(refusalResponse(401));
  await flush();
  assert.deepEqual(page.snapshot(), before);
});

test("stale fetch rejection after Disconnect cannot overwrite the disconnected state", async () => {
  const page = await pageDisconnectedWithPendingPoll(await dashboardScript());
  const before = page.snapshot();
  page.fetches[1]!.reject(new Error("synthetic network failure"));
  await flush();
  assert.deepEqual(page.snapshot(), before);
});

test("stale JSON rejection after Disconnect cannot overwrite the disconnected state", async () => {
  const page = await pageDisconnectedWithPendingPoll(await dashboardScript());
  const before = page.snapshot();
  page.fetches[1]!.resolve({
    status: 200,
    headers: {
      get: (name: string) =>
        name.toLowerCase() === "content-type" ? "application/json" : null,
    },
    json: async () => {
      throw new Error("synthetic json failure");
    },
  });
  await flush();
  assert.deepEqual(page.snapshot(), before);
});

test("a json() body arriving after Disconnect cannot overwrite the disconnected state", async () => {
  const page = await pageDisconnectedWithPendingPoll(await dashboardScript());
  const { response, resolveJson } = deferredJsonResponse(200);
  const before = page.snapshot();
  page.fetches[1]!.resolve(response);
  await flush();
  resolveJson(readyBody("late-json-marker"));
  await flush();
  assert.deepEqual(page.snapshot(), before);
});

test("obsolete-generation success after reconnect cannot pre-empt the new connection", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  await page.clickDisconnect();
  page.setCredentials(ID_2, SECRET_2);
  await page.clickConnect();
  assert.equal(page.fetches.length, 2, "reconnect must start a fresh poll");
  const beforeStale = page.snapshot();
  page.fetches[0]!.resolve(jsonResponse(200, readyBody("obsolete-marker")));
  await flush();
  assert.deepEqual(page.snapshot(), beforeStale);
  page.fetches[1]!.resolve(jsonResponse(200, readyBody("current-marker")));
  await flush();
  assert.ok(page.snapshot().summary.includes("current-marker"));
  assert.ok(!page.snapshot().summary.includes("obsolete-marker"));
});

test("obsolete-generation rejection after reconnect cannot overwrite the new result", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  await page.clickDisconnect();
  page.setCredentials(ID_2, SECRET_2);
  await page.clickConnect();
  page.fetches[1]!.resolve(jsonResponse(200, readyBody("current-marker")));
  await flush();
  const beforeStale = page.snapshot();
  assert.ok(beforeStale.summary.includes("current-marker"), "control: new poll renders");
  page.fetches[0]!.reject(new Error("synthetic network failure"));
  await flush();
  assert.deepEqual(page.snapshot(), beforeStale);
});

test("older READY cannot overwrite a newer completed NOT READY", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  await page.tick();
  assert.equal(page.fetches.length, 2);
  page.fetches[1]!.resolve(jsonResponse(503, notReadyBody("newer-marker")));
  await flush();
  const beforeStale = page.snapshot();
  assert.ok(beforeStale.summary.includes("NOT READY"), "control: 503 renders NOT READY");
  assert.ok(beforeStale.summary.includes("newer-marker"));
  page.fetches[0]!.resolve(jsonResponse(200, readyBody("older-marker")));
  await flush();
  assert.deepEqual(page.snapshot(), beforeStale);
});

test("older NOT READY cannot overwrite a newer completed READY", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  await page.tick();
  page.fetches[1]!.resolve(jsonResponse(200, readyBody("newer-marker")));
  await flush();
  const beforeStale = page.snapshot();
  assert.ok(beforeStale.summary.includes(">READY<"), "control: 200 renders READY");
  page.fetches[0]!.resolve(jsonResponse(503, notReadyBody("older-marker")));
  await flush();
  assert.deepEqual(page.snapshot(), beforeStale);
});

test("older rejection cannot overwrite a newer completed result", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  await page.tick();
  page.fetches[1]!.resolve(jsonResponse(200, readyBody("newer-marker")));
  await flush();
  const beforeStale = page.snapshot();
  page.fetches[0]!.reject(new Error("synthetic network failure"));
  await flush();
  assert.deepEqual(page.snapshot(), beforeStale);
});

test("older READY cannot overwrite a newer fetch rejection", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  await page.tick();
  assert.equal(page.fetches.length, 2);
  page.fetches[1]!.reject(new Error("synthetic network failure"));
  await flush();
  const beforeStale = page.snapshot();
  assert.equal(beforeStale.status, "fetch failed");
  assert.ok(
    beforeStale.summary.includes("summary unavailable: fetch failed"),
    "control: the newer rejection replaces the summary",
  );
  page.fetches[0]!.resolve(jsonResponse(200, readyBody("older-marker")));
  await flush();
  assert.deepEqual(page.snapshot(), beforeStale);
});

test("older HTTP refusal cannot overwrite a newer completed result", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  await page.tick();
  page.fetches[1]!.resolve(jsonResponse(200, readyBody("newer-marker")));
  await flush();
  const beforeStale = page.snapshot();
  page.fetches[0]!.resolve(refusalResponse(504));
  await flush();
  assert.deepEqual(page.snapshot(), beforeStale);
});

test("older late json() cannot overwrite a newer completed result", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  const { response, resolveJson } = deferredJsonResponse(200);
  page.fetches[0]!.resolve(response);
  await flush();
  await page.tick();
  page.fetches[1]!.resolve(jsonResponse(503, notReadyBody("newer-marker")));
  await flush();
  const beforeStale = page.snapshot();
  assert.ok(beforeStale.summary.includes("NOT READY"), "control: newer poll renders");
  resolveJson(readyBody("older-marker"));
  await flush();
  assert.deepEqual(page.snapshot(), beforeStale);
});

test("a superseded poll cannot render while its replacement is pending", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  await page.tick();
  assert.equal(page.fetches.length, 2, "interval must start the replacement poll");
  const beforeStale = page.snapshot();
  assert.equal(page.fetches[0]!.init?.signal?.aborted, true, "supersession cancels old work");
  assert.equal(page.fetches[1]!.init?.signal?.aborted, false, "replacement remains active");
  page.fetches[0]!.resolve(jsonResponse(200, readyBody("older-marker")));
  await flush();
  assert.deepEqual(page.snapshot(), beforeStale, "older success cannot pre-empt its replacement");
  assert.ok(!page.snapshot().summary.includes("older-marker"));
  page.fetches[1]!.resolve(jsonResponse(503, notReadyBody("newer-marker")));
  await flush();
  const snap = page.snapshot();
  assert.ok(snap.summary.includes("NOT READY"));
  assert.ok(snap.summary.includes("newer-marker"));
  assert.ok(snap.status.startsWith("updated "));
});

test("control: sequential completed polls still render each result in order", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  page.fetches[0]!.resolve(jsonResponse(200, readyBody("older-marker")));
  await flush();
  assert.ok(page.snapshot().summary.includes("older-marker"));
  assert.ok(page.snapshot().summary.includes(">READY<"));
  assert.ok(page.snapshot().status.startsWith("updated "));
  await page.tick();
  assert.equal(page.fetches.length, 2);
  page.fetches[1]!.resolve(jsonResponse(503, notReadyBody("newer-marker")));
  await flush();
  const snap = page.snapshot();
  assert.ok(snap.summary.includes("NOT READY"));
  assert.ok(snap.summary.includes("newer-marker"));
  assert.ok(!snap.summary.includes("older-marker"));
  assert.ok(snap.status.startsWith("updated "));
});

test("control: a live HTTP refusal still renders its failure state", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  page.fetches[0]!.resolve(refusalResponse(401));
  await flush();
  const snap = page.snapshot();
  assert.equal(snap.status, "summary 401");
  assert.ok(snap.summary.includes("summary request failed: 401"));
});

test("connect/reconnect keeps one interval; Disconnect clears it and the credentials", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  assert.equal(page.intervals.size, 1, "connect starts one interval");
  await page.clickConnect();
  assert.equal(page.intervals.size, 1, "a second connect keeps one interval");
  await page.clickDisconnect();
  assert.equal(page.intervals.size, 0, "disconnect clears the interval");
  assert.equal(page.elements["clientId"]!.value, "", "disconnect clears the id");
  assert.equal(page.elements["clientSecret"]!.value, "", "disconnect clears the secret");
  assert.equal(page.snapshot().status, "disconnected");
  page.setCredentials(ID_2, SECRET_2);
  await page.clickConnect();
  assert.equal(page.intervals.size, 1, "reconnect restarts exactly one interval");
});

test("polls stay GET-only on the summary path, one credential pair per connection", async () => {
  const page = makePage(await dashboardScript());
  await page.clickConnect();
  await page.clickDisconnect();
  page.setCredentials(ID_2, SECRET_2);
  await page.clickConnect();
  assert.equal(page.fetches.length, 2);
  for (const call of page.fetches) {
    assert.equal(call.url, OPS_SUMMARY_PATH);
    assert.equal(call.init?.method ?? "GET", "GET", "poll must not use a mutating method");
  }
  assert.equal(page.fetches[0]!.init?.headers?.["cf-access-client-id"], ID_1);
  assert.equal(page.fetches[0]!.init?.headers?.["cf-access-client-secret"], SECRET_1);
  assert.equal(page.fetches[1]!.init?.headers?.["cf-access-client-id"], ID_2);
  assert.equal(page.fetches[1]!.init?.headers?.["cf-access-client-secret"], SECRET_2);
});

test("no credential is persisted, logged or rendered across the stale-poll lifecycle", async () => {
  const page = await pageDisconnectedWithPendingPoll(await dashboardScript());
  page.fetches[1]!.resolve(jsonResponse(200, readyBody("stale-marker")));
  await flush();
  page.setCredentials(ID_2, SECRET_2);
  await page.clickConnect();
  page.fetches[2]!.reject(new Error("synthetic network failure"));
  await flush();
  const rendered = `${page.snapshot().status}\n${page.snapshot().summary}`;
  for (const secret of [SECRET_1, SECRET_2, ID_1, ID_2]) {
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

test("control: component names and states stay HTML-escaped", async () => {
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
  assert.ok(!summary.includes("<img"), "raw markup must not reach the DOM");
  assert.ok(summary.includes("&lt;img"), "angle brackets must be escaped");
  assert.ok(summary.includes("do&amp;wn"), "ampersands must be escaped");
});
