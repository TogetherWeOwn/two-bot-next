#!/usr/bin/env node
// Gradual-promotion smoke checker for two-bot-next.
//
// The shared central gradual-promote action invokes this file at every stage:
//   node bin/smoke.mjs "https://$DOMAIN" [--allow-indexable] --version-override '<worker-name>="<lowercase-version-uuid>"'
// at 5%, 50%, 100-override (pinned to the NEW version) and apex/live (no
// override) stages, with 6-attempt/10s retry done by the action (fail closed).
// This file does no retry itself: one pass, exit 0 on healthy, 1 on
// unhealthy, 2 on usage/validation errors.
//
// What it proves:
// - The probed origin answers as two-bot-next (GET /health 200 JSON
//   {"status":"ok"} through the Worker proxy).
// - The new build is ready (GET /readyz 200; the container answers 503
//   while any component is not ready, so 200 is the readiness gate).
// - Version attribution: under --version-override the Worker-stamped
//   `x-two-worker-version` header must equal the override UUID, so a canary
//   is tested directly instead of through sampled traffic. A silent fallback
//   to the old version fails instead of passing on the wrong build. On this
//   singleton Worker the pin only passes for the version that already holds
//   ownership (see the staging limit below).
// - Edge hardening: the Worker-owned copy carries `x-content-type-options:
//   nosniff` on both probes.
//
// Indexing posture: two-bot-next serves no indexable HTML (an API/redirect
// Worker: /health, /readyz, /ops/*, redirect links). --allow-indexable is
// accepted for the apex contract and intentionally changes no assertion;
// the flag is logged so apex runs are distinguishable in logs.
//
// Staging-first: the no-override probe runs against staging any time with
//   node bin/smoke.mjs "https://<staging-worker>"
// --version-override is contract-only for the isolated synthetic fixture
// (two-bot-next-gradual-proof): on this singleton Worker an override probe
// of a non-owner version answers 503 ownership_fenced (OwnershipFence.require
// in wrangler/src/ownership.ts), so it cannot pass as a canary. Override mode
// passes only when the candidate already holds ownership. Gradual promotion
// here also needs a Durable Object config without `exports` (this
// wrangler.toml uses exports, which Cloudflare documents as unsupported for
// gradual deployments) or a separate non-DO fixture Worker.
// Production wiring is passive (see docs/gradual-promote-caller.md); never
// claim unwired stages as proof.
import { pathToFileURL } from "node:url";

export const OVERRIDE_HEADER = "Cloudflare-Workers-Version-Overrides";
export const VERSION_HEADER = "x-two-worker-version";
const USER_AGENT = "two-bot-next-smoke/1.0";
const DEFAULT_TIMEOUT_MS = 5_000;

// Worker name: Cloudflare script-name rule (lowercase start, then
// lowercase/digits/dash/underscore). Version: lowercase UUID, the same shape
// the rollback command itself requires.
export const VERSION_OVERRIDE_PATTERN =
  /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}="[0-9a-f-]{36}"$/;
const UUID_PATTERN =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

export function assertVersionOverride(value) {
  if (typeof value !== "string" || !VERSION_OVERRIDE_PATTERN.test(value)) {
    throw new Error(
      'version-override must look like worker-name="12345678-1234-1234-1234-123456789abc"',
    );
  }
  const uuid = value.slice(value.indexOf('="') + 2, -1);
  if (!UUID_PATTERN.test(uuid)) {
    throw new Error(
      'version-override must look like worker-name="12345678-1234-1234-1234-123456789abc"',
    );
  }
  return value;
}

export function parseArgs(argv) {
  const [baseUrl, ...flags] = argv;
  let allowIndexable = false;
  let versionOverride;
  let usage = false;
  for (let i = 0; i < flags.length; i += 1) {
    if (flags[i] === "--allow-indexable") {
      allowIndexable = true;
    } else if (flags[i] === "--version-override") {
      versionOverride = flags[i + 1];
      i += 1;
      if (typeof versionOverride !== "string") usage = true;
    } else {
      usage = true;
    }
  }
  if (typeof baseUrl !== "string" || usage) {
    throw new Error(
      'Usage: node bin/smoke.mjs <base-url> [--allow-indexable] [--version-override worker-name="<version-uuid>"]',
    );
  }
  // Fail fast on the review bug: an unquoted or non-UUID override exits 2
  // before any probe, so a miswired gate can never pass on sampled traffic.
  if (versionOverride !== undefined) assertVersionOverride(versionOverride);
  return { baseUrl, allowIndexable, versionOverride };
}

function check(valid, expected, actual, route, log, state) {
  if (!valid) {
    state.failures += 1;
    state.routeFailures += 1;
    log(`FAIL ${route}: expected ${expected}; actual ${actual}`);
  }
}

async function fetchProbe(url, headers, timeoutMs, fetchImpl) {
  return fetchImpl(url, {
    method: "GET",
    redirect: "manual",
    signal: AbortSignal.timeout(timeoutMs),
    headers: { "user-agent": USER_AGENT, ...headers },
  });
}

export async function smoke(
  baseUrl,
  {
    timeoutMs = DEFAULT_TIMEOUT_MS,
    log = console.log,
    allowIndexable = false,
    versionOverride,
    fetchImpl = fetch,
  } = {},
) {
  const base = new URL(baseUrl);
  if (
    !["http:", "https:"].includes(base.protocol) ||
    base.username ||
    base.password ||
    base.pathname !== "/" ||
    base.search ||
    base.hash
  ) {
    throw new Error(
      "base-url must be an HTTP(S) origin without credentials, path, query or fragment",
    );
  }
  if (versionOverride !== undefined) assertVersionOverride(versionOverride);
  const expectedVersion =
    versionOverride === undefined
      ? undefined
      : versionOverride.slice(versionOverride.indexOf('="') + 2, -1);
  const overrideHeaders =
    versionOverride === undefined
      ? {}
      : { [OVERRIDE_HEADER]: versionOverride };
  if (allowIndexable) {
    log("smoke: --allow-indexable set (no indexable HTML on this Worker; flag accepted for the apex contract)");
  }
  const state = { failures: 0, routeFailures: 0 };
  let checked = 0;

  // GET /health: liveness through the Worker proxy.
  state.routeFailures = 0;
  try {
    const response = await fetchProbe(new URL("/health", base), overrideHeaders, timeoutMs, fetchImpl);
    const headers = response.headers;
    check(
      response.status === 200, "HTTP 200", `HTTP ${response.status}`,
      "/health", log, state,
    );
    check(
      headers.get("x-content-type-options")?.toLowerCase() === "nosniff",
      "X-Content-Type-Options nosniff",
      headers.get("x-content-type-options") ?? "missing",
      "/health", log, state,
    );
    const served = headers.get(VERSION_HEADER);
    if (expectedVersion === undefined) {
      check(
        typeof served === "string" && served.length > 0,
        "x-two-worker-version present",
        served ?? "missing",
        "/health", log, state,
      );
    } else {
      check(
        served === expectedVersion,
        `x-two-worker-version ${expectedVersion} (pinned canary)`,
        served ?? "missing",
        "/health", log, state,
      );
    }
    const contentType = headers.get("content-type")?.split(";")[0].trim().toLowerCase();
    check(
      contentType === "application/json", "Content-Type application/json",
      contentType ?? "missing", "/health", log, state,
    );
    const body = await response.text();
    let data = null;
    try {
      data = JSON.parse(body);
    } catch {
      data = null;
    }
    check(
      data !== null && data.status === "ok",
      'JSON {"status":"ok"}',
      data === null ? "invalid JSON" : "unexpected health shape",
      "/health", log, state,
    );
    checked += 1;
    if (state.routeFailures === 0) log("PASS /health");
  } catch (error) {
    check(
      false, "HTTP response and body within timeout",
      error instanceof Error ? error.name : "request error",
      "/health", log, state,
    );
    checked += 1;
  }

  // GET /readyz: readiness (200 only when every component is ready).
  const before = state.failures;
  state.routeFailures = 0;
  try {
    const response = await fetchProbe(new URL("/readyz", base), overrideHeaders, timeoutMs, fetchImpl);
    const headers = response.headers;
    check(
      response.status === 200, "HTTP 200 (gateway ready)",
      `HTTP ${response.status}`,
      "/readyz", log, state,
    );
    check(
      headers.get("x-content-type-options")?.toLowerCase() === "nosniff",
      "X-Content-Type-Options nosniff",
      headers.get("x-content-type-options") ?? "missing",
      "/readyz", log, state,
    );
    const served = headers.get(VERSION_HEADER);
    if (expectedVersion === undefined) {
      check(
        typeof served === "string" && served.length > 0,
        "x-two-worker-version present",
        served ?? "missing",
        "/readyz", log, state,
      );
    } else {
      check(
        served === expectedVersion,
        `x-two-worker-version ${expectedVersion} (pinned canary)`,
        served ?? "missing",
        "/readyz", log, state,
      );
    }
    const body = await response.text();
    let data = null;
    try {
      data = JSON.parse(body);
    } catch {
      data = null;
    }
    check(
      data !== null && typeof data.build_revision === "string" && data.build_revision.length > 0,
      "JSON with build_revision",
      data === null ? "invalid JSON" : "missing build_revision",
      "/readyz", log, state,
    );
    checked += 1;
    if (state.routeFailures === 0) log("PASS /readyz");
  } catch (error) {
    check(
      false, "HTTP response and body within timeout",
      error instanceof Error ? error.name : "request error",
      "/readyz", log, state,
    );
    checked += 1;
  }
  void before;

  log(`smoke: ${checked} probes, ${state.failures} failed assertions`);
  return state.failures === 0;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  let args;
  try {
    args = parseArgs(process.argv.slice(2));
  } catch (error) {
    console.error(error instanceof Error ? error.message : "invalid arguments");
    process.exitCode = 2;
  }
  if (args) {
    try {
      if (args.versionOverride !== undefined) assertVersionOverride(args.versionOverride);
      const ok = await smoke(args.baseUrl, {
        allowIndexable: args.allowIndexable,
        versionOverride: args.versionOverride,
      });
      process.exitCode = ok ? 0 : 1;
    } catch (error) {
      console.error(`smoke: ${error instanceof Error ? error.message : "invalid base-url"}`);
      process.exitCode = 2;
    }
  }
}
