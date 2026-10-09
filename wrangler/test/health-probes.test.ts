/**
 * TOG-12245 (threat-model F7): public /health and /readyz are bounded
 * GET/HEAD probes at the Worker. Synthetic bindings only — the stub DO
 * records what the Worker forwards; no endpoint, database or secret is
 * contacted. The Container's own alarm probe bypasses the Worker entirely,
 * so it is covered by the keepalive tests in container.test.ts (full suite).
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { type Env } from "../src/index.ts";

interface Forwarded {
  method: string;
  url: string;
  headers: Record<string, string>;
  body: string;
}

function harness() {
  const forwarded: Forwarded[] = [];
  const names: string[] = [];
  const env = {
    REDIRECT_MAPPINGS_JSON: "[]",
    REDIRECT_FALLBACK_CODE: "fallbackCode",
    // The ownership fence refuses forwards without a deployment id.
    CF_VERSION_METADATA: { id: "synthetic-probe-deployment" },
    TWO_BOT: {
      getByName: (name: string) => {
        names.push(name);
        return {
          fetch: async (r: Request) => {
            const headers: Record<string, string> = {};
            r.headers.forEach((value, key) => {
              headers[key] = value;
            });
            forwarded.push({ method: r.method, url: r.url, headers, body: await r.text() });
            return new Response("ok");
          },
        };
      },
    },
  } as unknown as Env;
  const ctx = { waitUntil() {} } as unknown as ExecutionContext;
  return { env, ctx, forwarded, names };
}

const probe = (path: string, init?: RequestInit) =>
  new Request(`https://probe.invalid${path}`, init);

for (const path of ["/health", "/readyz"]) {
  test(`${path}: POST/PUT/DELETE/PATCH/OPTIONS return 405 and never touch the container`, async () => {
    const h = harness();
    for (const method of ["POST", "PUT", "DELETE", "PATCH", "OPTIONS"]) {
      const res = await worker.fetch(
        probe(path, {
          method,
          headers: { authorization: "Bearer caller-secret", "x-custom": "1" },
          body: "caller-payload",
        }),
        h.env,
        h.ctx,
      );
      assert.equal(res.status, 405);
      assert.equal(res.headers.get("allow"), "GET, HEAD");
      await res.text();
    }
    assert.deepEqual(h.names, [], "refused methods must not resolve the DO");
    assert.deepEqual(h.forwarded, [], "refused bodies must never be forwarded");
  });

  test(`${path}: GET forwards a fresh probe with no query, caller headers or body`, async () => {
    const h = harness();
    const res = await worker.fetch(
      probe(`${path}?token=caller-secret&visitor=1`, {
        headers: {
          authorization: "Bearer caller-secret",
          cookie: "session=caller-secret",
          "x-custom": "caller",
          "cf-connecting-ip": "10.1.2.3",
        },
      }),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 200);
    assert.equal(await res.text(), "ok");
    assert.equal(h.forwarded.length, 1);
    const [fwd] = h.forwarded;
    assert.equal(fwd!.method, "GET");
    assert.equal(fwd!.url, `https://probe.invalid${path}`);
    const fwdUrl = new URL(fwd!.url);
    assert.equal(fwdUrl.search, "", "query must be stripped");
    assert.equal(fwdUrl.origin, "https://probe.invalid", "origin must be preserved");
    assert.equal(fwdUrl.pathname, path, "path must reach the container unchanged");
    for (const leaked of ["authorization", "cookie", "x-custom", "cf-connecting-ip"]) {
      assert.equal(fwd!.headers[leaked], undefined, `${leaked} must not be forwarded`);
    }
    assert.deepEqual(
      Object.keys(fwd!.headers),
      ["x-two-bot-deployment-id"],
      "only the Worker-stamped deployment id reaches the fenced DO",
    );
    assert.equal(fwd!.headers["x-two-bot-deployment-id"], "synthetic-probe-deployment");
    assert.equal(fwd!.body, "", "no caller body may be forwarded");
  });

  test(`${path}: HEAD forwards as HEAD`, async () => {
    const h = harness();
    const res = await worker.fetch(
      probe(path, { method: "HEAD", headers: { "cf-connecting-ip": "10.1.2.4" } }),
      h.env,
      h.ctx,
    );
    assert.equal(res.status, 200);
    assert.equal(h.forwarded.length, 1);
    assert.equal(h.forwarded[0]!.method, "HEAD");
    assert.equal(h.forwarded[0]!.url, `https://probe.invalid${path}`);
  });
}

test("a burst over budget gets 429 with retry-after; other callers and 405s are unaffected", async () => {
  const h = harness();
  const get = (ip: string) =>
    worker.fetch(probe("/health", { headers: { "cf-connecting-ip": ip } }), h.env, h.ctx);
  // Default bucket: 60 burst. Sequential probes refill negligibly.
  for (let i = 0; i < 60; i++) {
    const res = await get("10.9.9.9");
    assert.equal(res.status, 200);
    await res.text();
  }
  const over = await get("10.9.9.9");
  assert.equal(over.status, 429);
  assert.ok(Number(over.headers.get("retry-after")) >= 1);
  assert.equal(h.forwarded.length, 60, "throttled probes must not reach the container");
  // A different caller still has budget, and the method gate precedes the throttle.
  const other = await get("10.9.9.10");
  assert.equal(other.status, 200);
  await other.text();
  const refused = await worker.fetch(
    probe("/health", { method: "POST", headers: { "cf-connecting-ip": "10.9.9.9" } }),
    h.env,
    h.ctx,
  );
  assert.equal(refused.status, 405);
  assert.equal(h.forwarded.length, 61);
});

test("/ops/metrics gate behaves as before: 404 without token, 401 without/with wrong bearer", async () => {
  const h = harness();
  const token = "synthetic-metrics-scrape-token-0123456789";
  const env = (t?: string) => ({ ...h.env, METRICS_SCRAPE_TOKEN: t }) as Env;
  const get = (e: Env, auth?: string) =>
    worker.fetch(
      new Request("https://probe.invalid/ops/metrics", { headers: auth ? { authorization: auth } : {} }),
      e,
      h.ctx,
    );
  assert.equal((await get(env(), "Bearer anything")).status, 404);
  assert.equal((await get(env(token))).status, 401);
  assert.equal((await get(env(token), "Bearer nope")).status, 401);
  assert.equal(h.forwarded.length, 0, "unauthenticated scrapes never reach the container");
  const ok = await get(env(token), `Bearer ${token}`);
  assert.equal(ok.status, 200);
  assert.equal(h.forwarded.length, 1);
});

test("/ops/metrics: short token 404s as not configured with one redacted log line", async () => {
  const h = harness();
  const short = "short-token";
  const errors: string[] = [];
  const orig = console.error;
  console.error = (...args: unknown[]) => { errors.push(args.map(String).join(" ")); };
  try {
    const res = await worker.fetch(
      new Request("https://probe.invalid/ops/metrics", { headers: { authorization: `Bearer ${short}` } }),
      { ...h.env, METRICS_SCRAPE_TOKEN: short } as Env,
      h.ctx,
    );
    assert.equal(res.status, 404);
    assert.equal(await res.text(), "not found");
    assert.equal(h.forwarded.length, 0, "short-token scrapes never reach the container");
  } finally {
    console.error = orig;
  }
  assert.equal(errors.length, 1, "exactly one misconfiguration line");
  assert.ok(errors[0]!.includes("metrics_scrape_token_misconfigured"), "line names the event");
  assert.ok(errors[0]!.includes("32"), "line names the 32-character requirement");
  assert.ok(!errors[0]!.includes(short), "line never carries the token value");
});

test("/ops/metrics: repeated wrong bearers throttle per caller before comparison; other callers unaffected", async () => {
  const h = harness();
  const token = "synthetic-metrics-throttle-token-0123456789";
  const e = { ...h.env, METRICS_SCRAPE_TOKEN: token } as Env;
  const get = (ip: string, auth?: string) =>
    worker.fetch(
      new Request("https://probe.invalid/ops/metrics", {
        headers: { ...(auth ? { authorization: auth } : {}), "cf-connecting-ip": ip },
      }),
      e,
      h.ctx,
    );
  // Bucket: 10 burst. Ten failures are plain 401s.
  for (let i = 0; i < 10; i++) {
    const res = await get("10.7.7.7", "Bearer wrong-bearer-value");
    assert.equal(res.status, 401, `failure ${i} must stay 401`);
    await res.text();
  }
  const throttled = await get("10.7.7.7", "Bearer wrong-bearer-value");
  assert.equal(throttled.status, 429);
  assert.ok(Number(throttled.headers.get("retry-after")) >= 1);
  assert.equal(await throttled.text(), "slow down\n");
  assert.equal(h.forwarded.length, 0, "failed scrapes never reach the container");
  // A throttled caller is refused before the secret comparison: even the
  // correct bearer from that address is 429 while the bucket is exhausted,
  // so guessing cannot confirm a bearer. Buckets are per caller, so another
  // caller's failures cannot throttle a correct bearer elsewhere.
  const guessedRight = await get("10.7.7.7", `Bearer ${token}`);
  assert.equal(guessedRight.status, 429, "throttled caller gets 429 without a comparison");
  assert.ok(Number(guessedRight.headers.get("retry-after")) >= 1);
  await guessedRight.text();
  const other = await get("10.7.7.8", "Bearer wrong-bearer-value");
  assert.equal(other.status, 401);
  await other.text();
  const ok = await get("10.7.7.8", `Bearer ${token}`);
  assert.equal(ok.status, 200);
  assert.equal(h.forwarded.length, 1);
  // No token value in any response body seen above.
});

test("/ops/metrics: concurrent burst cannot share one token across comparisons", async () => {
  const h = harness();
  const token = "synthetic-metrics-concurrency-token-0123456789";
  const e = { ...h.env, METRICS_SCRAPE_TOKEN: token } as Env;
  const get = (ip: string, auth?: string) =>
    worker.fetch(
      new Request("https://probe.invalid/ops/metrics", {
        headers: { ...(auth ? { authorization: auth } : {}), "cf-connecting-ip": ip },
      }),
      e,
      h.ctx,
    );
  // Budget is taken synchronously before the awaited digest comparison, so 30
  // concurrent wrong guesses plus the correct bearer last fit only the
  // 10-burst: everything else — including the correct bearer — is refused
  // without a comparison.
  const attempts = Array.from({ length: 30 }, () => get("10.8.8.8", "Bearer wrong-bearer-value"));
  attempts.push(get("10.8.8.8", `Bearer ${token}`));
  const results = await Promise.all(attempts);
  let compared = 0;
  let refused = 0;
  for (const r of results) {
    if (r.status === 401) compared += 1;
    else if (r.status === 429) {
      refused += 1;
      assert.ok(Number(r.headers.get("retry-after")) >= 1);
    } else assert.fail(`unexpected concurrent status ${r.status}`);
    await r.text();
  }
  assert.equal(compared, 10, "only the 10-burst is compared");
  assert.equal(refused, 21, "the rest is refused");
  assert.equal(results[30]!.status, 429, "the concurrent correct bearer is refused without a comparison");
  assert.equal(h.forwarded.length, 0, "no concurrent attempt reaches the container");
});

test("/ops/metrics: wrong bearer and throttle responses never carry the token", async () => {
  const h = harness();
  const token = "synthetic-metrics-secrecy-token-0123456789";
  const e = { ...h.env, METRICS_SCRAPE_TOKEN: token } as Env;
  for (let i = 0; i < 11; i++) {
    const res = await worker.fetch(
      new Request("https://probe.invalid/ops/metrics", {
        headers: { authorization: "Bearer wrong", "cf-connecting-ip": "10.7.7.9" },
      }),
      e,
      h.ctx,
    );
    const body = await res.text();
    assert.ok(!body.includes(token), `response ${i} must not carry the token`);
    assert.ok(!(res.headers.get("www-authenticate") ?? "").includes(token));
  }
});
