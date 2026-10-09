#!/usr/bin/env node
// Staging-only control client. Secrets are read from env, never argv or stdout.
// Production handoff remains B4's separately authorized execution sheet.
class ControlError extends Error {}

class HttpFailure extends ControlError {
  constructor(message, status, reason) {
    super(message);
    this.status = status;
    this.reason = reason;
  }
}

// A retry read answered by a Worker version other than the deployed one.
// Propagation is still mixed, so this attempt must not post. Retryable inside
// the takeover window like a 5xx; the window bound still applies.
class DeploymentChanged extends ControlError {}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// Closed vocabulary of Worker refusal codes (wrangler/src); anything else prints as unrecognized.
export const REFUSAL_REASONS = new Set([
  "deployment_id_missing", "storage_unavailable", "storage_invalid", "not_owner", "epoch_conflict",
  "epoch_exhausted", "shutdown_unconfirmed", "deployment_mismatch", "stale_keepalive", "operation_failed",
]);
const MAX_REFUSAL_BODY_BYTES = 1024;

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

// Wrangler's default deferred code update is expected to keep old code answering 5xx for up to 300 s (docs/runbook.md); retry 5xx only.
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

export async function control({ action, url, token, actor, expectedEpoch, expectedDeploymentId, releaseFence = false, takeoverAttempts = 34, takeoverRetryDelayMs = 10000, takeoverWindowMs = 330000 }, send = fetch, wait = sleep) {
  const origin = new URL(url);
  if (origin.protocol !== "https:" || origin.port || origin.username || origin.password ||
      !/^two-bot-next-staging\.[a-z0-9-]+\.workers\.dev$/.test(origin.hostname) ||
      origin.pathname !== "/" || origin.search || origin.hash) {
    throw new ControlError("Expected the approved two-bot-next-staging workers.dev origin");
  }
  if (!token || token.length < 32) throw new ControlError("OWNERSHIP_CONTROL_TOKEN is missing or invalid; stop before deployment");
  if (!["preflight", "status", "takeover", "fence", "deployment-takeover"].includes(action)) throw new ControlError("Unknown action");
  if (action === "preflight") return { configured: true };
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
  let postedEpoch;
  const once = async () => {
    attempts += 1;
    const current = await read({ method: "GET" });
    if (action === "status") return current;
    if (!actor || !/^[a-zA-Z0-9_.:@/-]{1,128}$/.test(actor)) throw new ControlError("Explicit audit actor is required");
    if (action === "deployment-takeover") {
      // Routine deployments may hand off an active owner. A parked or pristine
      // singleton needs an explicit workflow-dispatch release, never push/default.
      if (current.owner?.phase !== "active" && !releaseFence) {
        throw new ControlError("Singleton is intentionally fenced or uninitialized; explicit staging release required");
      }
      // Pin to the version this deploy produced (the receipt's worker_version),
      // never to whichever version answers first: right after deploy the old
      // version can still answer reads while propagation is mixed, and posting
      // there only earns a 503. A read answered by any other version skips the
      // post and re-reads inside the window instead.
      if (!expectedDeploymentId) {
        throw new ControlError("Deployment-takeover needs the deployed version id; pass the receipt worker_version and do not post unpinned");
      }
      if (current.deploymentId !== expectedDeploymentId) {
        throw new DeploymentChanged(`Ownership transition not confirmed; answering deployment is not the deployed version; refusing to post, preserve maintenance (attempts=${attempts} elapsed=${elapsedSeconds()}s)`);
      }
      // A 5xx after a committed takeover leaves the owner active at the posted epoch; confirm it without posting again.
      if (postedEpoch !== undefined && current.owner?.phase === "active" && current.owner.epoch === postedEpoch &&
          current.owner.deploymentId === current.deploymentId) {
        if (current.running) throw new ControlError("Ownership transition not confirmed; preserve maintenance");
        return current;
      }
      // Our epoch committed but owned by another active deployment: another actor
      // committed at our epoch, so never re-post into it. Crash-recovery
      // (fenced) still re-posts.
      if (postedEpoch !== undefined && current.owner?.phase === "active" && current.owner.epoch === postedEpoch &&
          current.owner.deploymentId !== expectedDeploymentId) {
        throw new ControlError("Ownership transition not confirmed; posted epoch is owned by another deployment; refusing to post, preserve maintenance");
      }
      expectedEpoch = current.owner?.epoch ?? 0;
    }
    if (!Number.isSafeInteger(expectedEpoch) || expectedEpoch < 0) throw new ControlError("Explicit expected epoch is required");
    const targetAction = action === "deployment-takeover" ? "takeover" : action;
    postedEpoch = expectedEpoch + 1;
    const result = await read({ method: "POST", body: JSON.stringify({ action: targetAction, actor, expectedEpoch }) });
    // Requests can still route to a draining version after the pre-post read:
    // a 200 from the pinned version proves worker and singleton agree there,
    // so an answer from any other version is propagation, not a commit.
    // Retry it like a stale read instead of failing or misreading the epoch.
    if (action === "deployment-takeover" && result.deploymentId !== expectedDeploymentId) {
      throw new DeploymentChanged(`Ownership transition not confirmed; takeover answered by a different version than the deployed one; refusing to confirm, preserve maintenance (attempts=${attempts} elapsed=${elapsedSeconds()}s)`);
    }
    if (result.running || result.owner?.epoch !== expectedEpoch + 1 ||
        (targetAction === "takeover" && (result.owner.phase !== "active" || result.owner.deploymentId !== result.deploymentId)) ||
        (targetAction === "fence" && (result.owner.phase !== "fenced" || result.owner.deploymentId !== null))) {
      throw new ControlError("Ownership transition not confirmed; preserve maintenance");
    }
    return result;
  };
  if (action !== "deployment-takeover") return once();
  let earlier;
  for (let attempt = 1; ; attempt++) {
    try {
      return await once();
    } catch (error) {
      // A mixed-version read skips the post and re-reads; only 5xx refusals are
      // kept as the earlier refusal because DeploymentChanged carries no status.
      const mixedVersionRetry = error instanceof DeploymentChanged;
      if ((!retryableTakeoverFailure(error) && !mixedVersionRetry) || attempt >= takeoverAttempts ||
          Date.now() - started + takeoverRetryDelayMs > takeoverWindowMs) {
        throw withEarlierRefusal(error, earlier, `attempts=${attempts} elapsed=${elapsedSeconds()}s`);
      }
      if (error instanceof HttpFailure) earlier = error;
      await wait(takeoverRetryDelayMs);
    }
  }
}

if (process.argv[1] === new URL(import.meta.url).pathname) {
  const [action, epoch] = process.argv.slice(2);
  try {
    const result = await control({
      action, url: process.env.STAGING_WORKER_URL ?? "",
      token: process.env.OWNERSHIP_CONTROL_TOKEN,
      actor: process.env.OWNERSHIP_ACTOR,
      expectedEpoch: epoch === undefined ? undefined : Number(epoch),
      expectedDeploymentId: process.env.OWNERSHIP_EXPECTED_DEPLOYMENT,
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
