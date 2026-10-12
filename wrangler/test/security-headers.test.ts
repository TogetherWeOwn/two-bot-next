/**
 * Worker security headers (TOG-22701): every Worker-constructed response
 * carries MIME-sniffing / clickjacking hardening.
 *
 * Decided set: `x-content-type-options: nosniff`,
 * `referrer-policy: no-referrer` (matches the existing 302), and
 * `x-frame-options: DENY` (probe/metrics/control/redirect bodies are
 * machine-consumed text or JSON, never framed HTML). No `frame-ancestors`
 * CSP: X-Frame-Options DENY already states the posture with fewer bytes.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { SECURITY_HEADERS } from "../src/security-headers.ts";
import { handleRedirect } from "../src/redirect.ts";
import worker, { type Env } from "../src/index.ts";

const EXPECTED: Record<string, string> = {
  "x-content-type-options": "nosniff",
  "referrer-policy": "no-referrer",
  "x-frame-options": "DENY",
};

function assertHardened(headers: Headers, where: string): void {
  for (const [name, value] of Object.entries(EXPECTED)) {
    assert.equal(headers.get(name), value, `${where}: ${name} must be ${value}`);
  }
}

test("canonical helper carries the decided set", () => {
  assert.deepEqual({ ...SECURITY_HEADERS }, EXPECTED);
});

test("redirect text() responses carry the decided set", async () => {
  const deps = {
    guildId: "synthetic-guild",
    fallbackInviteCode: null,
    lookup: async () => null,
    recordClick: async () => {},
  };
  for (const [method, path] of [
    ["GET", "/never-created"],
    ["GET", "/healthz"],
    ["POST", "/reddit"],
    ["GET", "/favicon.ico"],
  ] as const) {
    const res = await handleRedirect(method, path, "caller", deps);
    for (const [name, value] of Object.entries(EXPECTED)) {
      assert.equal(res.headers[name], value, `${method} ${path}: ${name}`);
    }
  }
  const outage = await handleRedirect("GET", "/reddit", "caller", {
    ...deps,
    lookup: async () => { throw new Error("fixture outage"); },
  });
  assert.equal(outage.status, 503);
  for (const [name, value] of Object.entries(EXPECTED)) {
    assert.equal(outage.headers[name], value, `outage 503: ${name}`);
  }
});

test("redirect 302 keeps location/cache-control and gains hardening", async () => {
  const deps = {
    guildId: "synthetic-guild",
    fallbackInviteCode: "fallbackCode",
    lookup: async () => ({ slug: "reddit", inviteCode: "freshCode" }),
    recordClick: async () => {},
  };
  const res = await handleRedirect("GET", "/reddit", "caller", deps);
  assert.equal(res.status, 302);
  assert.equal(res.headers["location"], "https://discord.gg/freshCode");
  assert.match(res.headers["cache-control"] ?? "", /no-store/);
  assert.equal(res.headers["referrer-policy"], "no-referrer");
  assert.equal(res.headers["x-content-type-options"], "nosniff");
  assert.equal(res.headers["x-frame-options"], "DENY");
});

test("Worker-constructed responses carry the decided set", async () => {
  const env = {
    REDIRECT_MAPPINGS_JSON: "[]",
    REDIRECT_FALLBACK_CODE: "fallbackCode",
    CF_VERSION_METADATA: { id: "synthetic-headers-deployment" },
    TWO_BOT: {
      getByName: () => ({
        fetch: async () => new Response("ok"),
      }),
    },
  } as unknown as Env;
  const ctx = { waitUntil() {} } as unknown as ExecutionContext;
  const get = (path: string, init?: RequestInit) =>
    worker.fetch(new Request(`https://worker.invalid${path}`, init), env, ctx);

  // Probe method gate (405), probe throttle (429), metrics auth (401),
  // unconfigured metrics (404), reserved slug (404).
  assertHardened((await get("/health", { method: "POST" })).headers, "probe 405");
  const burst = (ip: string) => get("/health", { headers: { "cf-connecting-ip": ip } });
  for (let i = 0; i < 60; i++) await (await burst("192.0.2.50")).text();
  assertHardened((await burst("192.0.2.50")).headers, "probe 429");

  const token = "synthetic-headers-scrape-token-0123456789";
  const authed = { ...env, METRICS_SCRAPE_TOKEN: token } as Env;
  const metrics = (auth?: string) =>
    worker.fetch(
      new Request("https://worker.invalid/ops/metrics", {
        headers: { ...(auth ? { authorization: auth } : {}), "cf-connecting-ip": "192.0.2.51" },
      }),
      authed,
      ctx,
    );
  assertHardened((await metrics()).headers, "metrics 401");
  assertHardened(
    (await worker.fetch(new Request("https://worker.invalid/ops/metrics"), env, ctx)).headers,
    "metrics 404",
  );
  assertHardened((await get("/metrics")).headers, "reserved 404");

  // Control path without a token (401) and misconfigured redirect (503).
  assertHardened(
    (await get("/internal/ownership", { method: "GET" })).headers,
    "control 401",
  );
  const misconfigured = { ...env, REDIRECT_FALLBACK_CODE: "has space" } as Env;
  const bad = await worker.fetch(
    new Request("https://worker.invalid/reddit"),
    misconfigured,
    ctx,
  );
  assert.equal(bad.status, 503);
  assertHardened(bad.headers, "redirect 503");

  // Container-unavailable 500: a non-bot upstream becomes the fixed 500.
  const failing = {
    ...env,
    TWO_BOT: { getByName: () => ({ fetch: async () => new Response("boom", { status: 500 }) }) },
  } as unknown as Env;
  const unavailable = await worker.fetch(
    new Request("https://worker.invalid/health", { headers: { "cf-connecting-ip": "192.0.2.52" } }),
    failing,
    ctx,
  );
  assert.equal(unavailable.status, 500);
  assertHardened(unavailable.headers, "ready 500");
});
