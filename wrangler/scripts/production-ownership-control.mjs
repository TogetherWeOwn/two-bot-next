#!/usr/bin/env node
// Production-only ownership control client (M1.5, docs/cutover-production-takeover-design.md P2-P3).
// Secrets are read from env, never argv or stdout. Staging keeps its own
// client (ownership-control.mjs) and behavior; this file never talks to staging.
//
// P2 is `status` (authenticated GET read, no start). P3 is `takeover` with the
// P2 epoch (optimistic epoch check, single POST, bounded 5xx-only retry that
// re-reads first). `fence` parks all versions for the rollback path
// (docs/cutover-rollback-runbook.md section 4). A routine deploy never unparks
// the singleton: a fenced/pristine record needs OWNERSHIP_RELEASE_FENCE=true,
// which the production workflow sets only when its `takeover` dispatch input
// is true (B4 cutover execution). Production activation (GO) stays separate.
class ControlError extends Error {}

class HttpFailure extends ControlError {
  constructor(message, status, reason) {
    super(message);
    this.status = status;
    this.reason = reason;
  }
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// Closed vocabulary of Worker refusal codes (wrangler/src/ownership.ts and
// src/index.ts control/probe gates); anything else prints as unrecognized.
export const REFUSAL_REASONS = new Set([
  "deployment_id_missing", "storage_unavailable", "storage_invalid", "not_owner", "epoch_conflict",
  "epoch_exhausted", "shutdown_unconfirmed", "deployment_mismatch", "stale_keepalive", "operation_failed",
]);
const MAX_REFUSAL_BODY_BYTES = 1024;

// The staging origin shape this client always refuses. Production origins are
// pinned by equality with PRODUCTION_WORKER_URL (see control()), not by a
// hostname pattern, so a future custom domain keeps working.
const STAGING_HOST_PATTERN = /^two-bot-next-staging\.[a-z0-9-]+\.workers\.dev$/;

async function refusalReason(response) {
  try {
    const reader = response.body.getReader();
    const chunks = [];
    let size = 0;
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        size += value.byteLength;
        if (size > MAX_REFUSAL_BODY_BYTES) return "unrecognized";
        chunks.push(value);
      }
    } finally {
      await reader.cancel();
    }
    const reason = JSON.parse(Buffer.concat(chunks).toString("utf8"))?.reason;
    return REFUSAL_REASONS.has(reason) ? reason : "unrecognized";
  } catch {
    return "unrecognized";
  }
}

// Wrangler's default deferred code update can keep old code answering 5xx for
// up to 300 s; retry 5xx only. 401/400/405/409 are never retried blindly.
function retryableTakeoverFailure(error) {
  return error instanceof HttpFailure && error.status >= 500 && error.status <= 599;
}

// A non-HTTP refusal after an earlier 5xx (for example a fence left by a failed takeover POST) keeps that 5xx reason.
function withEarlierRefusal(error, earlier, summary) {
  if (!earlier || error instanceof HttpFailure) return error;
  const detail = `earlier takeover refusal HTTP ${earlier.status} reason=${earlier.reason} ${summary}`;
  if (error instanceof ControlError) return new ControlError(`${error.message}; ${detail}`);
  return new ControlError(`Ownership control failed; ${detail}; stop, do not change credentials`);
}

function checkOrigin(url, productionUrl, stagingUrl) {
  let origin;
  try {
    origin = new URL(url);
  } catch {
    throw new ControlError("PRODUCTION_WORKER_URL is not a valid URL; stop before deployment");
  }
  if (origin.protocol !== "https:" || origin.port || origin.username || origin.password ||
      origin.pathname !== "/" || origin.search || origin.hash) {
    throw new ControlError("Expected the approved production Worker origin (https, no port, credentials, path or query)");
  }
  if (STAGING_HOST_PATTERN.test(origin.hostname)) {
    throw new ControlError("Refusing a staging origin on the production control path; stop before deployment");
  }
  let approved;
  try {
    approved = new URL(productionUrl);
  } catch {
    throw new ControlError("PRODUCTION_WORKER_URL is not configured; stop before deployment");
  }
  if (approved.protocol !== "https:") {
    throw new ControlError("PRODUCTION_WORKER_URL must be an https:// production Worker URL; stop before deployment");
  }
  if (origin.origin !== approved.origin) {
    throw new ControlError("URL is not the approved production Worker origin; stop before deployment");
  }
  if (stagingUrl) {
    try {
      if (origin.origin === new URL(stagingUrl).origin) {
        throw new ControlError("Production Worker URL equals the staging URL; refusing to take over staging");
      }
    } catch (error) {
      if (error instanceof ControlError) throw error;
    }
  }
  return origin;
}

function validDeploymentId(value) {
  return typeof value === "string" && /^[a-zA-Z0-9_-]{1,128}$/.test(value);
}

export async function control({ action, url, productionUrl = process.env.PRODUCTION_WORKER_URL ?? "", stagingUrl = process.env.STAGING_WORKER_URL ?? "", token, actor, expectedEpoch, expectedDeploymentId, releaseFence = false, takeoverAttempts = 34, takeoverRetryDelayMs = 10000, takeoverWindowMs = 330000, statusAttempts = 34, statusRetryDelayMs = 10000 }, send = fetch, wait = sleep) {
  const origin = checkOrigin(url, productionUrl, stagingUrl);
  if (!token || token.length < 32) throw new ControlError("OWNERSHIP_CONTROL_TOKEN is missing or invalid; stop before deployment");
  if (!["preflight", "status", "takeover", "fence"].includes(action)) throw new ControlError("Unknown action");
  if (action === "preflight") return { configured: true };
  if (expectedDeploymentId !== undefined && !validDeploymentId(expectedDeploymentId)) {
    throw new ControlError("Expected deployment id is invalid; stop before posting");
  }
  const endpoint = new URL("/internal/ownership", origin);
  const headers = { authorization: `Bearer ${token}` };
  const started = Date.now();
  const elapsedSeconds = () => Math.round((Date.now() - started) / 1000);
  let attempts = 0;
  const read = async (init) => {
    // Never follow redirects carrying the control secret to another origin.
    const response = await send(endpoint, { ...init, headers, redirect: "error", signal: AbortSignal.timeout(15000) });
    if (!response.ok) {
      const reason = await refusalReason(response);
      throw new HttpFailure(`Ownership control failed (HTTP ${response.status}) reason=${reason} attempts=${attempts} elapsed=${elapsedSeconds()}s; stop, do not change credentials`, response.status, reason);
    }
    const state = await response.json();
    if (!state.deploymentId || !("owner" in state) || typeof state.running !== "boolean") {
      throw new ControlError("Invalid ownership control response");
    }
    return state;
  };
  // P2: read control state without starting the container. A deployment
  // mismatch or a running container is NO-GO for the step that produced it.
  const readState = async () => {
    const current = await read({ method: "GET" });
    if (expectedDeploymentId !== undefined && current.deploymentId !== expectedDeploymentId) {
      throw new ControlError("Production deployment mismatch; the serving version is not the deployed one; stop, do not post");
    }
    if (expectedDeploymentId !== undefined && current.running) {
      throw new ControlError("Ownership transition not confirmed; container already running before takeover; preserve maintenance");
    }
    return current;
  };
  let postedEpoch;
  const once = async (first) => {
    attempts += 1;
    const current = await readState();
    if (!actor || !/^[a-zA-Z0-9_.:@/-]{1,128}$/.test(actor)) throw new ControlError("Explicit audit actor is required");
    // A routine deploy never unparks the singleton: a fenced or pristine
    // record needs the explicit release handoff (the production workflow's
    // `takeover` dispatch input), never a default.
    if (current.owner?.phase !== "active" && !releaseFence) {
      throw new ControlError("Singleton is intentionally fenced or uninitialized; explicit production release required");
    }
    // A 5xx after a committed takeover leaves the owner active at the posted epoch; confirm it without posting again.
    if (postedEpoch !== undefined && current.owner?.phase === "active" && current.owner.epoch === postedEpoch &&
        current.owner.deploymentId === current.deploymentId) {
      if (current.running) throw new ControlError("Ownership transition not confirmed; preserve maintenance");
      return current;
    }
    // Our epoch committed but this read is answered by a different deployment than
    // the one that owns it. Refuse to hand a stale version a further commit;
    // the forward handover (owner still on the old version while the new one
    // answers) re-posts at the fresh epoch below.
    if (postedEpoch !== undefined && current.owner?.phase === "active" && current.owner.epoch === postedEpoch &&
        current.owner.deploymentId !== current.deploymentId) {
      if (expectedDeploymentId === undefined || current.deploymentId !== expectedDeploymentId) {
        throw new ControlError("Ownership transition not confirmed; posted epoch is owned by another deployment; refusing to post, preserve maintenance");
      }
    }
    // P3 posts exactly the P2 epoch on the first attempt; retries re-read
    // first, so a stale/replayed epoch returns 409 instead of committing.
    const epoch = first ? expectedEpoch : (current.owner?.epoch ?? 0);
    if (!Number.isSafeInteger(epoch) || epoch < 0) throw new ControlError("Explicit expected epoch is required");
    postedEpoch = epoch + 1;
    const result = await read({ method: "POST", body: JSON.stringify({ action, actor, expectedEpoch: epoch }) });
    if (expectedDeploymentId !== undefined && result.deploymentId !== expectedDeploymentId) {
      throw new ControlError("Ownership transition not confirmed; answering version moved during takeover; preserve maintenance");
    }
    if (result.running || result.owner?.epoch !== epoch + 1 ||
        (action === "takeover" && (result.owner.phase !== "active" || result.owner.deploymentId !== result.deploymentId)) ||
        (action === "fence" && (result.owner.phase !== "fenced" || result.owner.deploymentId !== null))) {
      throw new ControlError("Ownership transition not confirmed; preserve maintenance");
    }
    return result;
  };
  if (action === "status") {
    // After a version change the ownership Durable Object keeps running the
    // previous code until Cloudflare's deferred update reaches it (up to
    // about 300 s), and the edge can still route to the old version: the read
    // answers 503 deployment_mismatch, or a 200 naming the old deployment.
    // Both are propagation, not a NO-GO, so the P2 read waits for the new
    // version (34 reads, 10 s apart, the takeover window) before refusing.
    // Every other refusal stops at once.
    for (let attempt = 1; ; attempt++) {
      attempts = attempt;
      try {
        return await readState();
      } catch (error) {
        const propagating = expectedDeploymentId !== undefined && (
          (error instanceof HttpFailure && error.status === 503 && error.reason === "deployment_mismatch") ||
          (!(error instanceof HttpFailure) && /^Production deployment mismatch/.test(error.message)));
        if (!propagating || attempt >= statusAttempts) throw error;
        await wait(statusRetryDelayMs);
      }
    }
  }
  let earlier;
  let first = true;
  for (let attempt = 1; ; attempt++) {
    try {
      return await once(first);
    } catch (error) {
      if (!retryableTakeoverFailure(error) || attempt >= takeoverAttempts ||
          Date.now() - started + takeoverRetryDelayMs > takeoverWindowMs) {
        throw withEarlierRefusal(error, earlier, `attempts=${attempts} elapsed=${elapsedSeconds()}s`);
      }
      earlier = error;
      first = false;
      await wait(takeoverRetryDelayMs);
    }
  }
}

if (process.argv[1] === new URL(import.meta.url).pathname) {
  const [action, epoch] = process.argv.slice(2);
  try {
    const result = await control({
      action, url: process.env.PRODUCTION_WORKER_URL ?? "",
      token: process.env.OWNERSHIP_CONTROL_TOKEN,
      actor: process.env.OWNERSHIP_ACTOR,
      expectedEpoch: epoch === undefined ? undefined : Number(epoch),
      expectedDeploymentId: process.env.OWNERSHIP_EXPECTED_DEPLOYMENT || undefined,
      releaseFence: process.env.OWNERSHIP_RELEASE_FENCE === "true",
    });
    console.log(JSON.stringify(result));
  } catch (error) {
    // Only our own fixed-text validation errors are printable. Provider/JSON
    // errors are untrusted, even when their message starts with "Expected".
    console.error(error instanceof ControlError
      ? error.message : "Ownership control failed; stop and report without substituting credentials");
    process.exitCode = 1;
  }
}
