#!/usr/bin/env node
// Staging-only control client. Secrets are read from env, never argv or stdout.
// Production handoff remains B4's separately authorized execution sheet.
class ControlError extends Error {}

export async function control({ action, url, token, actor, expectedEpoch, releaseFence = false }, send = fetch) {
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
  const read = async (init) => {
    // Never follow redirects carrying the control secret to another origin.
    const response = await send(endpoint, { ...init, headers, redirect: "error", signal: AbortSignal.timeout(15000) });
    if (!response.ok) throw new ControlError(`Ownership control failed (HTTP ${response.status}); stop, do not change credentials`);
    const state = await response.json();
    if (!state.deploymentId || !("owner" in state) || typeof state.running !== "boolean") {
      throw new ControlError("Invalid ownership control response");
    }
    return state;
  };
  const current = await read({ method: "GET" });
  if (action === "status") return current;
  if (!actor || !/^[a-zA-Z0-9_.:@/-]{1,128}$/.test(actor)) throw new ControlError("Explicit audit actor is required");
  if (action === "deployment-takeover") {
    // Routine deployments may hand off an active owner. A parked or pristine
    // singleton needs an explicit workflow-dispatch release, never push/default.
    if (current.owner?.phase !== "active" && !releaseFence) {
      throw new ControlError("Singleton is intentionally fenced or uninitialized; explicit staging release required");
    }
    expectedEpoch = current.owner?.epoch ?? 0;
  }
  if (!Number.isSafeInteger(expectedEpoch) || expectedEpoch < 0) throw new ControlError("Explicit expected epoch is required");
  const targetAction = action === "deployment-takeover" ? "takeover" : action;
  const result = await read({ method: "POST", body: JSON.stringify({ action: targetAction, actor, expectedEpoch }) });
  if (result.running || result.owner?.epoch !== expectedEpoch + 1 ||
      (targetAction === "takeover" && (result.owner.phase !== "active" || result.owner.deploymentId !== result.deploymentId)) ||
      (targetAction === "fence" && (result.owner.phase !== "fenced" || result.owner.deploymentId !== null))) {
    throw new ControlError("Ownership transition not confirmed; preserve maintenance");
  }
  return result;
}

if (process.argv[1] === new URL(import.meta.url).pathname) {
  const [action, epoch] = process.argv.slice(2);
  try {
    const result = await control({
      action, url: process.env.STAGING_WORKER_URL ?? "",
      token: process.env.OWNERSHIP_CONTROL_TOKEN,
      actor: process.env.OWNERSHIP_ACTOR,
      expectedEpoch: epoch === undefined ? undefined : Number(epoch),
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
