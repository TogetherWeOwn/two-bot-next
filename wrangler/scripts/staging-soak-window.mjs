#!/usr/bin/env node
// A fresh live measurement interval; staging-green age never supplies credit.
import { appendFileSync } from "node:fs";
import { setTimeout as wait } from "node:timers/promises";
import { isDirectInvocation } from "./is-direct-invocation.mjs";
import { pipelineUse } from "./staging-use-pipeline.mjs";
import { StagingUseError } from "./staging-use.mjs";

const require = (condition, code) => { if (!condition) throw new StagingUseError(code); };

/** Readiness never acquires or extends a grant. The caller holds the canonical
 * lock across baseline, this interval, final evidence and confirmed release. */
export async function soakWindow(args, consume = pipelineUse, pause = wait,
  now = Date.now, monotonic = () => performance.now(), emit = () => {}) {
  require(typeof args.automatic === "boolean", "staging_pipeline_mode_invalid");
  const duration = args.automatic ? 1800000 : 0;
  require(Number.isSafeInteger(args.expiresAt) && now() + duration < args.expiresAt,
    "staging_window_invalid");
  const { automatic, ...binding } = args;
  const assertReady = async () => {
    require(now() < args.expiresAt, "staging_window_expired");
    await consume({ ...binding, action: "consume" }, undefined, now, emit);
    require(now() < args.expiresAt, "staging_window_expired");
  };
  await assertReady();
  const start = monotonic();
  const event = (action) => emit({ schema: "two-bot-staging-soak.v1", action,
    environment: "staging", runId: args.runId, runAttempt: args.runAttempt,
    sha: args.sha, deploymentId: args.deploymentId, expiresAt: args.expiresAt,
    durationMs: duration, observedAt: new Date(now()).toISOString() });
  event("soak-start");
  for (;;) {
    const elapsed = monotonic() - start;
    require(Number.isFinite(elapsed) && elapsed >= 0, "staging_window_invalid");
    if (elapsed >= duration) break;
    require(now() + (duration - elapsed) < args.expiresAt, "staging_window_expired");
    await pause(Math.min(60000, duration - elapsed));
    await assertReady();
  }
  // Even a manual zero-delay sample requires the same original grant and exact
  // build at both boundaries. No idle/unknown-health result can pass readiness.
  await assertReady();
  event("soak-complete");
}

if (isDirectInvocation(process.argv[1], import.meta.url)) {
  const path = process.env.STAGING_USE_RECEIPT;
  const emit = (value) => {
    require(path, "staging_receipt_required");
    const line = JSON.stringify(value) + "\n";
    appendFileSync(path, line, { mode: 0o600 });
    process.stdout.write(line);
  };
  try {
    require(path, "staging_receipt_required");
    require(["true", "false"].includes(process.env.FULL_SOAK), "staging_pipeline_mode_invalid");
    await soakWindow({ automatic: process.env.FULL_SOAK === "true",
      url: process.env.STAGING_WORKER_URL, token: process.env.OWNERSHIP_CONTROL_TOKEN,
      runId: process.env.GITHUB_RUN_ID, runAttempt: Number(process.env.GITHUB_RUN_ATTEMPT),
      sha: process.env.STAGING_USE_SHA, deploymentId: process.env.STAGING_USE_DEPLOYMENT,
      expiresAt: Number(process.env.STAGING_USE_EXPIRES_AT) }, pipelineUse, wait,
    Date.now, () => performance.now(), emit);
  } catch (error) {
    const code = error instanceof StagingUseError ? error.code : "staging_pipeline_failed";
    console.error(code);
    try { if (path) emit({ schema: "two-bot-staging-soak.v1", action: "failed", code,
      environment: "staging", observedAt: new Date().toISOString() }); }
    catch { console.error("staging_receipt_failed"); }
    process.exitCode = 1;
  }
}
