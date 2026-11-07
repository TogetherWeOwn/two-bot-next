#!/usr/bin/env node
// Workflow orchestration over the pinned, redacting staging control client.
import { appendFileSync } from "node:fs";
import { isDirectInvocation } from "./is-direct-invocation.mjs";
import { StagingUseError, stagingUse } from "./staging-use.mjs";

const MAX_WINDOW_MINUTES = 360;
const positive = (value) => Number.isSafeInteger(value) && value > 0;
const require = (condition, code) => { if (!condition) throw new StagingUseError(code); };
const sameAttempt = (use, args) => use && use.runId === args.runId && use.runAttempt === args.runAttempt;
const sameWindow = (use, args) => sameAttempt(use, args) && use.expiresAt === args.expiresAt;

/** The workflow owns the lock across ALL downstream jobs. An inherited consumer
 * only asserts readiness; it cannot acquire, renew or release the caller's use.
 * The immutable expiry is an Actions output, never an untrusted HTTP field used
 * as a shell command. Native expiry remains authoritative if no cleanup runs. */
export async function pipelineUse(args, invoke = stagingUse, now = Date.now, emit = () => {}) {
  require(["begin", "consume", "rebind", "finish"].includes(args.action), "staging_pipeline_action_invalid");
  require(typeof args.runId === "string" && /^[1-9][0-9]{0,19}$/.test(args.runId)
    && positive(args.runAttempt), "staging_run_invalid");
  if (args.action === "begin") {
    require(positive(args.minutes) && args.minutes <= MAX_WINDOW_MINUTES, "staging_window_invalid");
    require(args.expiresAt === undefined, "staging_pipeline_begin_inherited");
  } else require(positive(args.expiresAt), "staging_window_invalid");
  if (args.action === "consume" || args.action === "rebind") {
    require(typeof args.sha === "string" && /^[0-9a-f]{40}$/.test(args.sha)
      && typeof args.deploymentId === "string" && /^[a-zA-Z0-9_-]{1,128}$/.test(args.deploymentId),
    "staging_identity_invalid");
  }
  const base = { url: args.url, token: args.token, runId: args.runId, runAttempt: args.runAttempt };
  const call = async (action, binding = {}) => {
    const result = await invoke({ ...base, ...binding, action });
    emit(result);
    return result;
  };
  // Persist an attempt before even status: an aborted or refused begin still
  // has an evidence trail. The low-level client never projects audit/secret data.
  emit({ schema: "two-bot-staging-pipeline.v1", action: "attempt", operation: args.action,
    environment: "staging", runId: args.runId, runAttempt: args.runAttempt,
    observedAt: new Date(now()).toISOString() });

  if (args.action === "consume") {
    // No discovery: a called job must use the caller's validated exact build.
    return call("ready", { sha: args.sha, deploymentId: args.deploymentId, expiresAt: args.expiresAt });
  }
  const state = await call("status");
  if (args.action === "finish") {
    // A begin can fail before publishing a grant. Cleanup records that fact but
    // must not infer authority to stop a different attempt's running singleton.
    if (!sameAttempt(state.use, args)) {
      const result = { schema: "two-bot-staging-pipeline.v1", action: "not-owned", environment: "staging",
        runId: args.runId, runAttempt: args.runAttempt, observedAt: new Date(now()).toISOString() };
      emit(result);
      return result;
    }
    require(sameWindow(state.use, args), "staging_window_mismatch");
    return call("release", { sha: state.sha, deploymentId: state.deploymentId, expiresAt: args.expiresAt });
  }
  let expiresAt;
  if (args.action === "begin") {
    // A retry cannot mint a longer window, even when its first acquire/ready
    // response was lost. A released or expired attempt can never begin again.
    if (sameAttempt(state.use, args)) {
      require(state.use.phase !== "released" && now() < state.use.expiresAt, "staging_use_ended");
      expiresAt = state.use.expiresAt;
    } else {
      require(!state.use || state.use.phase === "released" || now() >= state.use.expiresAt,
        "staging_use_conflict");
      expiresAt = now() + args.minutes * 60000;
    }
  } else {
    // A deployment/rollback changes the owner and build, NOT the authorization
    // window. Never turn rebind into a fresh grant if the old one is missing.
    require(sameWindow(state.use, args) && state.use.phase !== "released", "staging_window_mismatch");
    require(state.deploymentId === args.deploymentId && state.sha === args.sha, "staging_identity_mismatch");
    expiresAt = args.expiresAt;
  }
  const binding = { sha: state.sha, deploymentId: state.deploymentId, expiresAt };
  // Export the chosen expiry before mutation. A failed or cancelled acquire
  // can still be released by the parent; the native alarm is the last resort.
  emit({ schema: "two-bot-staging-pipeline.v1", action: "window", environment: "staging",
    ...binding, runId: args.runId, runAttempt: args.runAttempt, observedAt: new Date(now()).toISOString() });
  await call("acquire", binding);
  return call("ready", binding);
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  const path = process.env.STAGING_USE_RECEIPT;
  const output = process.env.GITHUB_OUTPUT;
  const runId = process.env.GITHUB_RUN_ID;
  const runAttempt = Number(process.env.GITHUB_RUN_ATTEMPT);
  const correlation = { runId: typeof runId === "string" && /^[1-9][0-9]{0,19}$/.test(runId) ? runId : null,
    runAttempt: positive(runAttempt) ? runAttempt : null };
  const emit = (result) => {
    require(path, "staging_receipt_required");
    const line = JSON.stringify({ ...correlation, ...result }) + "\n";
    appendFileSync(path, line, { mode: 0o600 });
    // Only validated local values and parsed client projections enter outputs.
    // Status must never export an unrelated run's grant.
    if (output && result.action === "window") appendFileSync(output,
      `expires_at=${result.expiresAt}\n`, { mode: 0o600 });
    if (output && result.action === "ready") appendFileSync(output,
      `expires_at=${result.use.expiresAt}\ndeployment_id=${result.deploymentId}\nsha=${result.sha}\n`, { mode: 0o600 });
    process.stdout.write(line);
  };
  try {
    require(path, "staging_receipt_required");
    await pipelineUse({ action: process.argv[2], url: process.env.STAGING_WORKER_URL,
      token: process.env.OWNERSHIP_CONTROL_TOKEN, runId: process.env.GITHUB_RUN_ID,
      runAttempt: Number(process.env.GITHUB_RUN_ATTEMPT), minutes: Number(process.env.STAGING_USE_MINUTES),
      expiresAt: process.env.STAGING_USE_EXPIRES_AT === undefined ? undefined : Number(process.env.STAGING_USE_EXPIRES_AT),
      sha: process.env.STAGING_USE_SHA, deploymentId: process.env.STAGING_USE_DEPLOYMENT }, stagingUse, Date.now, emit);
  } catch (error) {
    const code = error instanceof StagingUseError ? error.code : "staging_pipeline_failed";
    console.error(code);
    try { if (path) emit({ schema: "two-bot-staging-pipeline.v1", action: "failed", code,
      environment: "staging", observedAt: new Date().toISOString() }); }
    catch { console.error("staging_receipt_failed"); }
    process.exitCode = 1;
  }
}
