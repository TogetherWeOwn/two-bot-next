/** Protected test utility, shared by VM acceptance and the offline browser fixture.
 * Only browser boundaries are replaced: time, interval delivery, and fetch.
 * Intervals are delivered explicitly; advance() runs one-shot deadlines in order.
 * This lets expiry be checked without a new interval hiding the old observation.
 */
import worker, { type Env } from "../src/index.ts";

export async function servedLifecyclePage() {
  const response = await worker.fetch(new Request("https://w.invalid/ops/dashboard"), {
    REDIRECT_MAPPINGS_JSON: "[]",
    CF_VERSION_METADATA: { id: "synthetic-lifecycle-deployment" },
    TWO_BOT: { getByName: () => { throw new Error("static dashboard reached the container"); } },
  } as unknown as Env, {} as ExecutionContext);
  if (response.status !== 200) throw new Error(`dashboard returned ${response.status}`);
  return { html: await response.text(), csp: response.headers.get("content-security-policy") };
}

export function installLifecycleHarness(host: Record<string, unknown>, honourAbort = true) {
  const epoch = Date.parse("2026-01-02T03:04:05.000Z");
  let elapsed = 0;
  let sequence = 0;
  const timeouts = new Map<number, { at: number; fn: () => unknown }>();
  const intervals = new Map<number, () => unknown>();
  const flush = async () => {
    for (let i = 0; i < 24; i++) await Promise.resolve();
  };
  const invoke = (fn: () => unknown) => {
    const result = fn() as Promise<unknown> | undefined;
    if (result && typeof result.catch === "function") result.catch(() => {});
  };
  const RealDate = host.Date as DateConstructor;
  host.Date = class extends RealDate {
    constructor(value?: string | number) { super(value ?? epoch + elapsed); }
    static now() { return epoch + elapsed; }
  };
  Object.defineProperty(host, "performance", { configurable: true, value: { now: () => elapsed } });
  host.setTimeout = (fn: () => unknown, delay = 0) => {
    const id = ++sequence;
    timeouts.set(id, { at: elapsed + Math.max(1, Number(delay)), fn });
    return id;
  };
  host.clearTimeout = (id: number) => timeouts.delete(id);
  host.setInterval = (fn: () => unknown) => {
    const id = ++sequence;
    intervals.set(id, fn);
    return id;
  };
  host.clearInterval = (id: number) => intervals.delete(id);

  function deferred<T>() {
    let resolve!: (value: T) => void;
    let reject!: (reason: unknown) => void;
    const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
    // An abort before response headers also rejects the unread body promise.
    promise.catch(() => {});
    return { promise, resolve, reject };
  }
  interface Reply {
    status: number;
    headers: { get: (name: string) => string | null };
    json: () => Promise<unknown>;
  }
  const requests: {
    url: string;
    init: RequestInit;
    done: boolean;
    headers: (status?: number) => void;
    body: (value: unknown) => void;
    reply: (value: unknown, status?: number) => void;
    fail: (reason: unknown) => void;
  }[] = [];
  host.fetch = (url: string, init: RequestInit = {}) => {
    const headers = deferred<Reply>();
    const body = deferred<unknown>();
    const signal = init.signal;
    const finish = () => {
      request.done = true;
      signal?.removeEventListener("abort", abort);
    };
    const abort = () => {
      if (honourAbort) {
        finish();
        const error = new Error("synthetic request aborted");
        error.name = "AbortError";
        headers.reject(error);
        body.reject(error);
      }
    };
    const request = {
      url, init, done: false,
      headers: (status = 200) => headers.resolve({
        status,
        headers: { get: (name: string) => name.toLowerCase() === "content-type" ? "application/json" : null },
        json: () => body.promise,
      }),
      body: (value: unknown) => { finish(); body.resolve(value); },
      reply: (value: unknown, status = 200) => { request.headers(status); request.body(value); },
      fail: (reason: unknown) => { finish(); headers.reject(reason); body.reject(reason); },
    };
    requests.push(request);
    if (signal?.aborted) abort();
    else signal?.addEventListener("abort", abort, { once: true });
    return headers.promise;
  };
  const logs: string[] = [];
  const storageWrites: string[] = [];
  const record = (...args: unknown[]) => logs.push(args.map(String).join(" "));
  host.console = { log: record, warn: record, error: record, info: record, debug: record };
  const storage = {
    getItem: () => null,
    setItem: (key: string, value: string) => storageWrites.push(`${key}=${value}`),
    removeItem: () => {}, clear: () => {},
  };
  Object.defineProperty(host, "localStorage", { configurable: true, value: storage });
  Object.defineProperty(host, "sessionStorage", { configurable: true, value: storage });
  return {
    requests, timeouts, intervals, logs, storageWrites, flush,
    now: () => elapsed,
    outstanding: () => requests.filter((r) => !r.done && !r.init.signal?.aborted).length,
    tick: async () => { for (const fn of [...intervals.values()]) invoke(fn); await flush(); },
    advance: async (ms: number) => {
      const until = elapsed + ms;
      let turns = 0;
      while (true) {
        const next = [...timeouts.entries()].filter(([, t]) => t.at <= until)
          .sort((a, b) => a[1].at - b[1].at)[0];
        if (!next) break;
        if (++turns > 1000) throw new Error("unbounded one-shot timer loop");
        elapsed = next[1].at;
        timeouts.delete(next[0]);
        invoke(next[1].fn);
        await flush();
      }
      elapsed = until;
      await flush();
    },
  };
}
