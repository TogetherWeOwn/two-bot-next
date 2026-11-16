#!/usr/bin/env node
// Trusted staging pipeline client. No secret, remote body or URL reaches receipts.
import { appendFileSync } from "node:fs";
import { isDirectInvocation } from "./is-direct-invocation.mjs";
import { parseOwnershipResponse } from "./ownership-control-response.mjs";

const PATH = "/internal/staging-use";
const MAX_WINDOW_MS = 6 * 60 * 60 * 1000;
const MAX_BODY_BYTES = 16 * 1024;
const MAX_REQUEST_MS = 15000;
const ID = /^[a-zA-Z0-9_-]{1,128}$/;
const SHA = /^[0-9a-f]{40}$/;
const RUN = /^[1-9][0-9]{0,19}$/;
const record = (v) => v !== null && typeof v === "object" && !Array.isArray(v);
const positive = (v) => Number.isSafeInteger(v) && v > 0;
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

export class StagingUseError extends Error {
  constructor(code) { super(code); this.code = code; }
}
const require = (condition, code) => { if (!condition) throw new StagingUseError(code); };

function origin(url) {
  let parsed;
  try { parsed = new URL(url); } catch { throw new StagingUseError("staging_origin_invalid"); }
  require(parsed.protocol === "https:" && !parsed.port && !parsed.username && !parsed.password
    && /^two-bot-next-staging\.[a-z0-9-]+\.workers\.dev$/.test(parsed.hostname)
    && parsed.pathname === "/" && !parsed.search && !parsed.hash, "staging_origin_invalid");
  return parsed;
}

export function parseStagingState(value) {
  const ownership = parseOwnershipResponse(value);
  if (!ownership || !record(value) || typeof value.sha !== "string" || !SHA.test(value.sha)
    || !Object.hasOwn(value, "use") || !positive(ownership.owner?.epoch)
    || ownership.owner.phase !== "active" || ownership.owner.deploymentId !== ownership.deploymentId) return null;
  let use = null;
  if (value.use !== null) {
    const u = value.use;
    if (!record(u) || u.environment !== "staging" || typeof u.runId !== "string" || !RUN.test(u.runId)
      || !positive(u.runAttempt) || !positive(u.expiresAt) || !Number.isSafeInteger(u.acquiredAt) || u.acquiredAt < 0
      || u.expiresAt <= u.acquiredAt || u.expiresAt - u.acquiredAt > MAX_WINDOW_MS
      || typeof u.deploymentId !== "string" || !ID.test(u.deploymentId) || !positive(u.epoch)
      || typeof u.sha !== "string" || !SHA.test(u.sha) || !["preparing", "active", "released"].includes(u.phase)) return null;
    use = Object.fromEntries(["environment", "runId", "runAttempt", "expiresAt", "acquiredAt", "deploymentId", "sha", "epoch", "phase"]
      .map((key) => [key, u[key]]));
  }
  // Never project the audit actor or arbitrary success-body/debug fields.
  return { deploymentId: ownership.deploymentId, sha: value.sha, epoch: ownership.owner.epoch,
    running: ownership.running, use };
}

// One deadline spans fetch, headers and the bounded body. Even an uncooperative
// transport/cancel implementation cannot hold the client's deadline open.
async function request(send, url, init, deadline) {
  const remaining = Math.min(MAX_REQUEST_MS, deadline - performance.now());
  require(remaining > 0, "staging_request_timeout");
  const controller = new AbortController();
  let timer;
  let response;
  let reader;
  const release = () => {
    try { Promise.resolve(reader ? reader.cancel() : response?.body?.cancel()).catch(() => {}); } catch {}
  };
  try {
    return await Promise.race([
      (async () => {
        response = await send(url, { ...init, redirect: "error", signal: controller.signal });
        if (controller.signal.aborted) { release(); throw new StagingUseError("staging_request_timeout"); }
        require(!response.redirected, "staging_redirect_refused");
        // Do not parse or print remote errors, even if they look like local codes.
        if (!response.ok) { release(); return { status: response.status, version: null, body: null }; }
        require((response.headers.get("content-type") ?? "").split(";")[0].trim() === "application/json", "staging_response_invalid");
        reader = response.body?.getReader();
        require(reader, "staging_response_invalid");
        const chunks = [];
        let bytes = 0;
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          bytes += value.byteLength;
          require(bytes <= MAX_BODY_BYTES, "staging_response_too_large");
          chunks.push(value);
        }
        let body;
        try { body = JSON.parse(Buffer.concat(chunks).toString("utf8")); }
        catch { throw new StagingUseError("staging_response_invalid"); }
        return { status: response.status, version: response.headers.get("x-two-worker-version"), body };
      })(),
      new Promise((_, reject) => { timer = setTimeout(() => {
        controller.abort(); release(); reject(new StagingUseError("staging_request_timeout"));
      }, remaining); }),
    ]);
  } catch (error) {
    throw error instanceof StagingUseError ? error : new StagingUseError("staging_transport_failed");
  } finally { clearTimeout(timer); release(); }
}

function binding(args, now) {
  require(typeof args.sha === "string" && SHA.test(args.sha) && typeof args.deploymentId === "string"
    && ID.test(args.deploymentId), "staging_identity_invalid");
  require(typeof args.runId === "string" && RUN.test(args.runId) && positive(args.runAttempt), "staging_run_invalid");
  require(positive(args.expiresAt) && args.expiresAt > now && args.expiresAt - now <= MAX_WINDOW_MS,
    "staging_window_invalid");
}

function identity(state, args) {
  require(state.deploymentId === args.deploymentId && state.sha === args.sha, "staging_identity_mismatch");
}

function sameRunWindow(use, args) {
  return use && use.environment === "staging" && use.runId === args.runId
    && use.runAttempt === args.runAttempt && use.expiresAt === args.expiresAt;
}

function window(state, args, now, phase = "active") {
  const use = state.use;
  require(sameRunWindow(use, args) && use.phase === phase
    && (phase === "released" || (use.acquiredAt <= now && now < use.expiresAt
      && use.sha === state.sha && use.deploymentId === state.deploymentId && use.epoch === state.epoch)),
  "staging_window_mismatch");
}

/** Status never starts a container. All other actions pin caller-supplied build
 * and run identities; in particular release never stops some other run's use. */
export async function stagingUse(args, send = fetch, pause = wait, now = Date.now) {
  const base = origin(args.url);
  require(typeof args.token === "string" && args.token.length >= 32, "staging_token_missing");
  require(["status", "observe", "acquire", "assert", "ready", "release"].includes(args.action), "staging_action_invalid");
  // A release may be retried after expiry, but never extends that expiry.
  if (!["status", "observe"].includes(args.action)) binding(args, args.action === "release" ? Math.min(now(), args.expiresAt - 1) : now());
  const headers = { authorization: `Bearer ${args.token}`, "cache-control": "no-cache" };
  const control = async (init) => {
    for (;;) {
      const deadline = args.statusRetryDeadline === undefined || init.method !== "GET"
        ? performance.now() + MAX_REQUEST_MS : args.statusRetryDeadline;
      const response = await request(send, new URL(PATH, base), { ...init, headers }, deadline);
      if (response.status === 503 && init.method === "GET" && args.statusRetryDeadline !== undefined
        && performance.now() + 5000 < deadline) {
        await pause(5000);
        continue;
      }
      require(response.status === 200, "staging_control_refused");
      const state = parseStagingState(response.body);
      require(state, "staging_response_invalid");
      return state;
    }
  };
  let state = await control({ method: "GET" });
  if (args.action === "status") return receipt(args.action, state, now());
  if (args.action === "observe") {
    const at = now();
    const use = state.use;
    // A periodic reader observes authority only: it never starts, grants,
    // releases or extends a window. Idle is not readiness or burn recovery.
    const idle = !use || use.phase === "released" || at >= use.expiresAt;
    if (idle) require(!state.running, "staging_stop_unconfirmed");
    else require(use.phase === "active" && at >= use.acquiredAt
      && use.deploymentId === state.deploymentId && use.sha === state.sha && use.epoch === state.epoch,
    "staging_window_mismatch");
    return { ...receipt(args.action, state, at), sample: !idle,
      code: idle ? "intentional_idle" : "active_window", health: "unknown" };
  }
  identity(state, args);
  const epoch = state.epoch;
  if (args.action === "acquire" || args.action === "release") {
    if (args.action === "release") require(sameRunWindow(state.use, args), "staging_window_mismatch");
    const change = { action: args.action, environment: "staging", deploymentId: args.deploymentId,
      sha: args.sha, expectedEpoch: state.epoch, runId: args.runId, runAttempt: args.runAttempt, expiresAt: args.expiresAt };
    state = await control({ method: "POST", body: JSON.stringify(change) });
    identity(state, args);
    require(state.epoch === epoch, "staging_epoch_mismatch");
  }
  window(state, args, now(), args.action === "release" ? "released" : "active");
  if (args.action === "release") require(!state.running, "staging_stop_unconfirmed");
  if (args.action === "ready") {
    // Never grant readiness beyond expiry. The existing rollout gate still
    // certifies image/digest/build/deploy IDs; this check certifies finite use.
    const deadline = performance.now() + Math.min(300000, args.expiresAt - now());
    for (;;) {
      require(now() < args.expiresAt, "staging_window_expired");
      const result = await request(send, new URL("/readyz", base), { method: "GET", headers: { "cache-control": "no-cache" } }, deadline);
      if (result.status === 200) {
        require(result.version === args.deploymentId && result.body?.build_revision === args.sha,
          "staging_readiness_identity_mismatch");
        const components = result.body?.components;
        require(Array.isArray(components) && components.length > 0
          && components.every((row) => Array.isArray(row) && row.length === 2 && row[1] === "ready")
          && components.some((row) => row[0] === "gateway"), "staging_readiness_failed");
        break;
      }
      require(result.status === 500 || result.status === 503, "staging_readiness_failed");
      require(performance.now() + 5000 < deadline, "staging_readiness_timeout");
      await pause(5000);
    }
    // A takeover/release while the probe was in flight invalidates its result.
    state = await control({ method: "GET" });
    identity(state, args);
    require(state.epoch === epoch, "staging_epoch_mismatch");
    window(state, args, now());
    require(state.running, "staging_readiness_failed");
  }
  return receipt(args.action, state, now());
}

function receipt(action, state, at) {
  return { schema: "two-bot-staging-use.v1", action, environment: "staging", observedAt: new Date(at).toISOString(), ...state };
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  const action = process.argv[2];
  const path = process.env.STAGING_USE_RECEIPT;
  const runId = process.env.GITHUB_RUN_ID;
  const runAttempt = Number(process.env.GITHUB_RUN_ATTEMPT);
  const expiresAt = Number(process.env.STAGING_USE_EXPIRES_AT);
  const run = { runId: typeof runId === "string" && RUN.test(runId) ? runId : null,
    runAttempt: positive(runAttempt) ? runAttempt : null, expiresAt: positive(expiresAt) ? expiresAt : null };
  const emit = (value) => {
    const line = JSON.stringify({ ...run, ...value }) + "\n";
    if (path) appendFileSync(path, line, { mode: 0o600 });
    process.stdout.write(line);
  };
  try {
    require(path, "staging_receipt_required");
    // Receipt persistence precedes the first possible mutation. Runner loss
    // afterwards is covered by the runtime's native persisted expiry, not a trap.
    emit({ schema: "two-bot-staging-use.v1", action: "attempt", environment: "staging", observedAt: new Date().toISOString() });
    const result = await stagingUse({ action, url: process.env.STAGING_WORKER_URL, token: process.env.OWNERSHIP_CONTROL_TOKEN,
      sha: process.env.STAGING_USE_SHA, deploymentId: process.env.STAGING_USE_DEPLOYMENT,
      runId: process.env.GITHUB_RUN_ID, runAttempt: Number(process.env.GITHUB_RUN_ATTEMPT),
      expiresAt: Number(process.env.STAGING_USE_EXPIRES_AT) });
    emit(result);
    // Publish only after the validated receipt is durably retained. Missing
    // output on any error must never open a periodic reader's sample gate.
    if (action === "observe" && process.env.GITHUB_OUTPUT) {
      appendFileSync(process.env.GITHUB_OUTPUT, `sample=${result.sample}\n`, { mode: 0o600 });
    }
  } catch (error) {
    // Provider errors (including arbitrary messages/URLs) are never printable.
    const code = error instanceof StagingUseError ? error.code : "staging_client_failed";
    console.error(code);
    try { if (path) emit({ schema: "two-bot-staging-use.v1", action: "failed", environment: "staging", code, observedAt: new Date().toISOString() }); }
    catch { console.error("staging_receipt_failed"); }
    process.exitCode = 1;
  }
}
