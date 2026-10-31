#!/usr/bin/env node
// Gradual-promotion smoke checker for two-bot-next.
//
// The shared central gradual-promote action invokes this file at every stage:
//   node bin/smoke.mjs "https://$DOMAIN" [--allow-indexable] [--json] --version-override '<worker-name>="<lowercase-version-uuid>"'
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
//   ownership (see the staging limit below). Without an override the live
//   version must still be a present lowercase UUID.
// - Edge hardening: the Worker-owned copy carries `x-content-type-options:
//   nosniff` on both probes.
//
// Bounded intake: each probe body is read incrementally through the body
// reader and refused above MAX_PROBE_BYTES (16 KiB) of actual bytes.
// Content-Length is never trusted (absent or lying lengths cannot smuggle a
// body past the budget, and a lying large claim cannot refuse a small valid
// body). The unread tail is cancelled best-effort (never awaited) and a
// refused body is never valid health evidence. One AbortSignal.timeout
// deadline per probe (default 5 s, or the caller's timeoutMs) bounds both
// the fetch and the body read, so a stalled tail settles FAIL at the
// deadline instead of hanging. Probes stay GET-only with manual redirects.
//
// Redacted diagnostics: remote header values and bodies are never
// interpolated into logs. Mismatches carry fixed codes (missing, mismatch,
// not_nosniff, bad_media, over_budget, ...) or shape-validated bounded
// identifiers, so hostile, control-char, overlong or secret-shaped values
// fail without echo.
//
// JSON mode: --json (or smoke(url, { json: true })) logs exactly one
// machine-readable `smoke.v1` object with stable route IDs
// ["health","readyz"], per-route checks with stable "<route>.<name>" IDs, a
// verdict matching the boolean return, a bounded reason_code ("ok" iff pass)
// and next_action ("none" iff pass). Default text output, the
// --version-override contract and exit codes 0/1/2 are unchanged. The probes
// stay GET-only and idempotent; there is no mutation or dry-run path.
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
import { isDirectInvocation } from "../wrangler/scripts/is-direct-invocation.mjs";

export const OVERRIDE_HEADER = "Cloudflare-Workers-Version-Overrides";
export const VERSION_HEADER = "x-two-worker-version";
// Documented finite intake budget: each probe body is read incrementally and
// refused above this many actual bytes. Content-Length is never consulted.
export const MAX_PROBE_BYTES = 16 * 1024;
const USER_AGENT = "two-bot-next-smoke/1.0";
const DEFAULT_TIMEOUT_MS = 5_000;
const JSON_SCHEMA = "smoke.v1";

// Worker name: Cloudflare script-name rule (lowercase start, then
// lowercase/digits/dash/underscore). Version: lowercase UUID, the same shape
// the rollback command itself requires.
export const VERSION_OVERRIDE_PATTERN =
  /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}="[0-9a-f-]{36}"$/;
const UUID_PATTERN =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

const NEXT_ACTION_BY_CHECK = {
  status: "check_service",
  security: "check_edge",
  version: "check_canary",
  media: "check_service",
  budget: "check_service",
  shape: "check_service",
};

const HEALTH_SHAPE = {
  expected: 'JSON {"status":"ok"}',
  parse(data) {
    if (data !== null && data.status === "ok") {
      return { ok: true, code: "ok", actual: "match" };
    }
    return {
      ok: false,
      code: data === null ? "invalid_json" : "bad_shape",
      actual: data === null ? "invalid JSON" : "unexpected health shape",
    };
  },
};

const READYZ_SHAPE = {
  expected: "JSON with build_revision",
  parse(data) {
    if (data !== null && typeof data.build_revision === "string" && data.build_revision.length > 0) {
      return { ok: true, code: "ok", actual: "match" };
    }
    return {
      ok: false,
      code: data === null ? "invalid_json" : "bad_shape",
      actual: data === null ? "invalid JSON" : "missing build_revision",
    };
  },
};

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
  let json = false;
  let versionOverride;
  let usage = false;
  for (let i = 0; i < flags.length; i += 1) {
    if (flags[i] === "--allow-indexable") {
      allowIndexable = true;
    } else if (flags[i] === "--json") {
      json = true;
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
      'Usage: node bin/smoke.mjs <base-url> [--allow-indexable] [--json] [--version-override worker-name="<version-uuid>"]',
    );
  }
  // Fail fast on the review bug: an unquoted or non-UUID override exits 2
  // before any probe, so a miswired gate can never pass on sampled traffic.
  if (versionOverride !== undefined) assertVersionOverride(versionOverride);
  return { baseUrl, allowIndexable, versionOverride, json };
}

function check(valid, expected, actual, route, log, state) {
  if (!valid) {
    state.failures += 1;
    state.routeFailures += 1;
    log(`FAIL ${route}: expected ${expected}; actual ${actual}`);
  }
}

async function fetchProbe(url, headers, timeoutMs, fetchImpl) {
  // One whole-request deadline per probe: the same signal bounds the fetch
  // and the incremental body read, and the probe stays GET-only with manual
  // redirects.
  const signal = AbortSignal.timeout(timeoutMs);
  const response = await fetchImpl(url, {
    method: "GET",
    redirect: "manual",
    signal,
    headers: { "user-agent": USER_AGENT, ...headers },
  });
  return { response, signal };
}

function readWithSignal(reader, signal) {
  const pending = reader.read();
  if (!signal || signal.aborted) {
    if (signal && signal.aborted) {
      throw signal.reason instanceof Error ? signal.reason : new Error("aborted");
    }
    return pending;
  }
  return new Promise((resolve, reject) => {
    const onAbort = () => {
      reject(signal.reason instanceof Error ? signal.reason : new Error("aborted"));
    };
    signal.addEventListener("abort", onAbort, { once: true });
    pending.then(
      (value) => {
        signal.removeEventListener("abort", onAbort);
        resolve(value);
      },
      (error) => {
        signal.removeEventListener("abort", onAbort);
        reject(error);
      },
    );
  });
}

// Read one probe body incrementally without trusting Content-Length: count
// actual bytes, refuse above MAX_PROBE_BYTES, and race every read against
// the probe's whole-request deadline so a stalled tail settles FAIL at the
// deadline. Tail cleanup is best-effort and never awaited, so a cancel()
// that never settles cannot block the verdict.
async function readBoundedText(response, signal) {
  const body = response.body;
  if (body === null || body === undefined) {
    return { outcome: "ok", text: "" };
  }
  const reader = body.getReader();
  const cancelBestEffort = () => {
    try {
      const pending = reader.cancel();
      if (pending !== undefined && pending !== null && typeof pending.catch === "function") {
        pending.catch(() => {});
      }
    } catch {
      // Best-effort cleanup only.
    }
  };
  const chunks = [];
  let bytes = 0;
  try {
    for (;;) {
      const next = await readWithSignal(reader, signal);
      if (next.done) {
        break;
      }
      const chunk = typeof next.value === "string" ? new TextEncoder().encode(next.value) : next.value;
      bytes += chunk.byteLength;
      if (bytes > MAX_PROBE_BYTES) {
        cancelBestEffort();
        return { outcome: "over_budget", text: "" };
      }
      chunks.push(chunk);
    }
  } catch (error) {
    cancelBestEffort();
    throw error;
  }
  try {
    reader.releaseLock();
  } catch {
    // Already released or cancelled; nothing left to clean up.
  }
  const joined = new Uint8Array(bytes);
  let offset = 0;
  for (const chunk of chunks) {
    joined.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return { outcome: "ok", text: new TextDecoder().decode(joined) };
}

function transportFailure(routeId, route, names, error) {
  const actual = error instanceof Error ? error.name : "request error";
  const checks = names.map((name) => ({
    id: `${routeId}.${name}`,
    name,
    ok: false,
    code: "probe_error",
    expected: "HTTP response and body within timeout",
    actual,
  }));
  return { routeId, route, ok: false, checks, transportError: actual };
}

async function probeRoute(spec) {
  const { route, routeId, url, overrideHeaders, timeoutMs, fetchImpl, expectedVersion, wantMedia, shape } = spec;
  const names = ["status", "security", "version"];
  if (wantMedia) {
    names.push("media");
  }
  names.push("budget", "shape");
  let response;
  let signal;
  try {
    ({ response, signal } = await fetchProbe(url, overrideHeaders, timeoutMs, fetchImpl));
  } catch (error) {
    return transportFailure(routeId, route, names, error);
  }
  const headers = response.headers;
  const checks = [];
  const push = (name, ok, code, expected, actual) => {
    checks.push({ id: `${routeId}.${name}`, name, ok, code, expected, actual });
  };
  const statusOk = response.status === 200;
  push(
    "status",
    statusOk,
    statusOk ? "ok" : "bad_status",
    routeId === "readyz" ? "HTTP 200 (gateway ready)" : "HTTP 200",
    `HTTP ${response.status}`,
  );
  // Security: compare only; the remote value is never interpolated.
  const servedSecurity = headers.get("x-content-type-options");
  const securityOk = servedSecurity?.toLowerCase() === "nosniff";
  push(
    "security",
    securityOk,
    securityOk ? "ok" : servedSecurity === null ? "missing" : "not_nosniff",
    "X-Content-Type-Options nosniff",
    servedSecurity === null ? "missing" : securityOk ? "nosniff" : "not_nosniff",
  );
  // Version: live mode requires a present lowercase UUID; override mode
  // requires equality with the caller-supplied pin. Either way the remote
  // value is never interpolated.
  const servedVersion = headers.get(VERSION_HEADER);
  if (expectedVersion === undefined) {
    const versionOk = typeof servedVersion === "string" && UUID_PATTERN.test(servedVersion);
    push(
      "version",
      versionOk,
      versionOk ? "ok" : servedVersion === null ? "missing" : "bad_shape",
      "x-two-worker-version lowercase UUID",
      servedVersion === null ? "missing" : versionOk ? "present" : "invalid_shape",
    );
  } else {
    const versionOk = servedVersion === expectedVersion;
    push(
      "version",
      versionOk,
      versionOk ? "ok" : servedVersion === null ? "missing" : "mismatch",
      `x-two-worker-version ${expectedVersion} (pinned canary)`,
      servedVersion === null ? "missing" : versionOk ? "match" : "mismatch",
    );
  }
  if (wantMedia) {
    const rawMedia = headers.get("content-type");
    const media = rawMedia?.split(";")[0].trim().toLowerCase();
    const mediaOk = media === "application/json";
    push(
      "media",
      mediaOk,
      mediaOk ? "ok" : rawMedia === null ? "missing" : "bad_media",
      "Content-Type application/json",
      rawMedia === null ? "missing" : mediaOk ? "application/json" : "bad_media",
    );
  }
  let read;
  try {
    read = await readBoundedText(response, signal);
  } catch (error) {
    return transportFailure(routeId, route, names, error);
  }
  const budgetOk = read.outcome === "ok";
  push(
    "budget",
    budgetOk,
    budgetOk ? "ok" : "over_budget",
    `body within ${MAX_PROBE_BYTES} bytes`,
    budgetOk ? "within budget" : "over_budget",
  );
  if (!budgetOk) {
    // A refused body is never valid health evidence, even when its unseen
    // prefix would have parsed.
    push("shape", false, "refused", shape.expected, "refused over-budget body");
  } else {
    let data = null;
    try {
      data = JSON.parse(read.text);
    } catch {
      data = null;
    }
    const parsed = shape.parse(data);
    push("shape", parsed.ok, parsed.code, shape.expected, parsed.actual);
  }
  return { routeId, route, ok: checks.every((entry) => entry.ok), checks, transportError: null };
}

function buildVerdict(routes, passed) {
  let reasonCode = "ok";
  let nextAction = "none";
  if (!passed) {
    const firstFailure = routes
      .flatMap((route) => route.checks.map((entry) => ({ route, entry })))
      .find((candidate) => !candidate.entry.ok);
    if (firstFailure) {
      const { route, entry } = firstFailure;
      reasonCode = `${route.routeId}_${entry.name}_${entry.code}`;
      nextAction =
        entry.code === "probe_error" ? "retry_probe" : (NEXT_ACTION_BY_CHECK[entry.name] ?? "check_service");
    }
  }
  return {
    schema: JSON_SCHEMA,
    verdict: passed ? "pass" : "fail",
    reason_code: reasonCode,
    next_action: nextAction,
    routes: routes.map((route) => ({
      id: route.routeId,
      ok: route.ok,
      checks: route.checks.map((entry) => ({ id: entry.id, ok: entry.ok, code: entry.code })),
    })),
  };
}

export async function smoke(
  baseUrl,
  {
    timeoutMs = DEFAULT_TIMEOUT_MS,
    log = console.log,
    allowIndexable = false,
    versionOverride,
    fetchImpl = fetch,
    json = false,
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
  if (allowIndexable && !json) {
    log("smoke: --allow-indexable set (no indexable HTML on this Worker; flag accepted for the apex contract)");
  }

  const routes = [
    await probeRoute({
      route: "/health",
      routeId: "health",
      url: new URL("/health", base),
      overrideHeaders,
      timeoutMs,
      fetchImpl,
      expectedVersion,
      wantMedia: true,
      shape: HEALTH_SHAPE,
    }),
    await probeRoute({
      route: "/readyz",
      routeId: "readyz",
      url: new URL("/readyz", base),
      overrideHeaders,
      timeoutMs,
      fetchImpl,
      expectedVersion,
      wantMedia: false,
      shape: READYZ_SHAPE,
    }),
  ];
  const passed = routes.every((route) => route.ok);

  // JSON mode logs exactly one machine-readable verdict object and nothing
  // else, so promotion automation can parse stdout directly.
  if (json) {
    log(JSON.stringify(buildVerdict(routes, passed)));
    return passed;
  }

  const state = { failures: 0, routeFailures: 0 };
  for (const result of routes) {
    state.routeFailures = 0;
    if (result.transportError !== null) {
      check(false, "HTTP response and body within timeout", result.transportError, result.route, log, state);
    } else {
      for (const entry of result.checks) {
        check(entry.ok, entry.expected, entry.actual, result.route, log, state);
      }
    }
    if (state.routeFailures === 0) log(`PASS ${result.route}`);
  }

  log(`smoke: ${routes.length} probes, ${state.failures} failed assertions`);
  return state.failures === 0;
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
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
        json: args.json,
      });
      process.exitCode = ok ? 0 : 1;
    } catch (error) {
      console.error(`smoke: ${error instanceof Error ? error.message : "invalid base-url"}`);
      process.exitCode = 2;
    }
  }
}
