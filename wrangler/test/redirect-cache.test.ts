/** Local workerd acceptance with a mocked mapping store; no deployed services. */
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import { Miniflare, convertV4MiniflareOptions } from "miniflare";
import { RedirectMissCache, handleRedirect, type Campaign } from "../src/redirect.ts";

const handler = stripTypeScriptTypes(
  readFileSync(new URL("../src/redirect.ts", import.meta.url), "utf8"),
);

// Only this fixture accepts a synthetic clock header. Both the handler and
// cache are the production implementation, running inside one workerd isolate.
const fixture = `${handler}
let now = 0;
const misses = new RedirectMissCache({ ttlMs: 5000, maxEntries: 2 }, () => now);
export default {
  async fetch(request, env) {
    now = Number(request.headers.get("test-now") ?? 0);
    const url = new URL(request.url);
    const result = await handleRedirect(request.method, url.pathname, "fixture", {
      guildId: "synthetic-guild",
      missCache: misses,
      lookup: async (slug) => {
        const response = await env.STORE.fetch("http://store/" + slug);
        if (!response.ok) throw new Error("fixture store unavailable");
        return response.json();
      },
      recordClick: async () => {},
    });
    return new Response(result.body, { status: result.status, headers: result.headers });
  },
};`;

test("Miniflare: canonical misses share one lookup, expire, and expose a new slug", async (t) => {
  const rows = new Map<string, Campaign>();
  const lookups: string[] = [];
  const mf = new Miniflare(convertV4MiniflareOptions({
    modules: true,
    script: fixture,
    compatibilityDate: "2026-09-01",
    serviceBindings: {
      STORE: async (request: Request) => {
        const slug = new URL(request.url).pathname.slice(1);
        lookups.push(slug);
        return Response.json(rows.get(slug) ?? null);
      },
    },
  }));
  t.after(() => mf.dispose());
  const fetch = (path: string, now = 0, method = "GET") =>
    mf.dispatchFetch(`http://worker${path}`, {
      method, redirect: "manual", headers: { "test-now": String(now) },
    });

  for (const path of ["/new-link", "/NEW-LINK/", "/%6eew-link?ignored=1"]) {
    const response = await fetch(path);
    assert.equal(response.status, 404);
    assert.equal(response.headers.get("location"), null);
  }
  assert.deepEqual(lookups, ["new-link"]);

  rows.set("new-link", { slug: "new-link", inviteCode: "freshCode" });
  assert.equal((await fetch("/new-link", 4_999, "HEAD")).status, 404);
  assert.equal(lookups.length, 1, "cache hit must not extend TTL");
  const created = await fetch("/new-link", 5_000);
  assert.equal(created.status, 302);
  assert.equal(created.headers.get("location"), "https://discord.gg/freshCode");
  assert.equal(lookups.length, 2);

  rows.set("new-link", { slug: "new-link", inviteCode: "changedCode" });
  const changed = await fetch("/new-link", 5_001);
  assert.equal(changed.headers.get("location"), "https://discord.gg/changedCode");
  assert.equal(lookups.length, 3, "positive mappings must never be cached");

  for (const path of ["/missing-one", "/missing-two", "/missing-three", "/missing-one"]) {
    assert.equal((await fetch(path, 5_001)).status, 404);
  }
  assert.deepEqual(lookups.slice(3), ["missing-one", "missing-two", "missing-three", "missing-one"],
    "capacity eviction must keep an unbounded slug spray out of isolate memory");

  for (const path of ["/metrics", "/%6detrics/extra", "/favicon.ico", "/a", "/bad%00slug"]) {
    assert.equal((await fetch(path, 5_001)).status, 404);
  }
  assert.equal((await fetch("/healthz", 5_001)).status, 200);
  assert.equal((await fetch("/missing-one", 5_001, "POST")).status, 405);
  assert.equal(lookups.length, 7, "reserved/invalid paths and methods must skip lookup");
});

test("outages and invalid codes are not cached; rate limiting precedes cache hits", async () => {
  const misses = new RedirectMissCache();
  let calls = 0;
  let result: "outage" | "bad-code" | "missing" | "live" = "outage";
  const deps = {
    guildId: "synthetic-guild",
    missCache: misses,
    lookup: async () => {
      calls++;
      if (result === "outage") throw new Error("fixture outage");
      if (result === "missing") return null;
      return { slug: "new-link", inviteCode: result === "bad-code" ? "bad code" : "freshCode" };
    },
    recordClick: async () => {},
  };
  assert.equal((await handleRedirect("GET", "/new-link", "caller", deps)).status, 503);
  result = "bad-code";
  assert.equal((await handleRedirect("GET", "/new-link", "caller", deps)).status, 500);
  result = "live";
  assert.equal((await handleRedirect("GET", "/new-link", "caller", deps)).status, 302);
  result = "missing";
  assert.equal((await handleRedirect("GET", "/missing", "caller", deps)).status, 404);
  assert.equal(calls, 4);
  assert.equal((await handleRedirect("GET", "/missing", "caller", {
    ...deps, isThrottled: () => true,
  })).status, 429);
  assert.equal(calls, 4);
});

for (const spec of [{ ttlMs: 0, maxEntries: 2 }, { ttlMs: 5_000, maxEntries: 0 }]) {
  test(`disabled cache does not retain misses (${JSON.stringify(spec)})`, () => {
    const misses = new RedirectMissCache(spec);
    misses.add("new-link");
    assert.equal(misses.has("new-link"), false);
  });
}
