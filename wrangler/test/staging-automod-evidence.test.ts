import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { assembleStagingAutomodEvidence } from "../scripts/staging-automod-evidence.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";
import { lease } from "./fixtures/staging-automod-recovery.ts";

const expectedLease = { ...lease, purpose: AUTOMOD_STAGING_PURPOSE, runAttempt: 1 };
const passed = { verdict: "passed", start: true, recover: true };
const unconfirmedLease = { verdict: "failed", code: "lease_unconfirmed" };
const unconfirmedActions = { verdict: "failed", start: false, recover: false };
const cli = fileURLToPath(new URL("../scripts/staging-automod-evidence.ts", import.meta.url));

test("assembles a complete successful result without copying underlying evidence", () => {
  const discord = {
    content: "synthetic Discord payload",
    member: { user: { id: "1000000000000000000", username: "synthetic-member" } },
  };
  const credentials = { authorization: "Bearer synthetic-evidence-credential" };
  const input = {
    expectedLease,
    discord,
    attempt: { lease: expectedLease, credentials, result: { ...passed, discord, credentials } },
  };
  const run = spawnSync(process.execPath, [cli], { input: JSON.stringify(input), encoding: "utf8" });
  assert.equal(run.status, 0);
  assert.equal(run.stdout, JSON.stringify(passed) + "\n");
  assert.equal(run.stderr, "");
});

test("CLI refuses a receipt in place of an action confirmation and omits extra receipts", () => {
  const startReceipt = {
    schema: "two-bot-staging-automod-job.v1", environment: "staging", action: "start",
    lease: expectedLease, observedAt: new Date(expectedLease.acquiredAt).toISOString(), verdict: "passed",
  };
  const extraReceipt = { ...startReceipt, details: "synthetic receipt payload" };
  const confirmedRecovery = { verdict: "failed", start: false, recover: true };
  for (const { result, expected } of [
    { result: { ...passed, start: startReceipt, startReceipt: extraReceipt }, expected: unconfirmedActions },
    { result: { ...confirmedRecovery, startReceipt: extraReceipt }, expected: confirmedRecovery },
  ]) {
    const input = { expectedLease, attempt: { lease: expectedLease, result } };
    const run = spawnSync(process.execPath, [cli], { input: JSON.stringify(input), encoding: "utf8" });
    assert.equal(run.status, 1);
    assert.equal(run.stdout, JSON.stringify(expected) + "\n");
    assert.equal(run.stderr, "");
  }
});

test("cannot confirm either action for a different or missing lease", () => {
  for (const mismatch of [
    { environment: "production" }, { purpose: "voice-move" }, { phase: "released" }, { runAttempt: 2 },
    { runId: "12346" }, { deploymentId: "other-fixture" }, { sha: "b".repeat(40) }, { epoch: 4 },
    { acquiredAt: expectedLease.acquiredAt + 1 }, { expiresAt: expectedLease.expiresAt + 1 },
    { voiceMove: true },
  ]) {
    const differentLease = { ...expectedLease, ...mismatch };
    assert.deepEqual(assembleStagingAutomodEvidence({
      expectedLease, attempt: { lease: differentLease, result: passed },
    }), unconfirmedLease);
  }
  assert.deepEqual(assembleStagingAutomodEvidence({
    expectedLease, attempt: { result: passed },
  }), unconfirmedLease);
  assert.deepEqual(assembleStagingAutomodEvidence({
    expectedLease, attempt: Object.create({ lease: expectedLease, result: passed }),
  }), unconfirmedLease);
  const retryLease = { ...expectedLease, runAttempt: 2 };
  assert.deepEqual(assembleStagingAutomodEvidence({
    expectedLease: retryLease, attempt: { lease: retryLease, result: passed },
  }), unconfirmedLease);
});

test("incomplete or contradictory action results cannot claim success", () => {
  for (const result of [
    undefined, { verdict: "passed", recover: true }, { verdict: "passed", start: true, recover: "true" },
    { verdict: "passed", start: true, recover: false }, { ...passed, verdict: "failed" },
    { ...passed, code: "synthetic-evidence-credential" }, Object.create(passed),
  ]) {
    assert.deepEqual(assembleStagingAutomodEvidence({
      expectedLease, attempt: { lease: expectedLease, result },
    }), unconfirmedActions);
  }
  assert.deepEqual(assembleStagingAutomodEvidence({
    expectedLease, attempt: { lease: expectedLease, result: unconfirmedLease },
  }), unconfirmedLease);
});

test("preserves recovery confirmation when startup was unconfirmed", () => {
  const result = { verdict: "failed", start: false, recover: true };
  const input = { expectedLease, attempt: { lease: expectedLease, result } };
  const run = spawnSync(process.execPath, [cli], { input: JSON.stringify(input), encoding: "utf8" });
  assert.equal(run.status, 1);
  assert.equal(run.stdout, JSON.stringify(result) + "\n");
  assert.equal(run.stderr, "");

  const failedRecovery = { verdict: "failed", start: true, recover: false };
  assert.deepEqual(assembleStagingAutomodEvidence({
    expectedLease, attempt: { lease: expectedLease, result: failedRecovery },
  }), failedRecovery);
});

test("rejects ambiguous, malformed, and oversized CLI input without echoing it", () => {
  const input = JSON.stringify({ expectedLease, attempt: { lease: expectedLease, result: passed } });
  for (const invalid of [
    input.replace('"start":true', '"start":false,"st\\u0061rt":true'),
    'synthetic-evidence-credential',
    Buffer.concat([Buffer.from(input.slice(0, -1) + ',"ignored":"'), Buffer.from([0xff]), Buffer.from('"}')]),
    input.padEnd(4097, " "),
  ]) {
    const run = spawnSync(process.execPath, [cli], { input: invalid, encoding: "utf8" });
    assert.equal(run.status, 1);
    assert.equal(run.stdout, JSON.stringify(unconfirmedLease) + "\n");
    assert.equal(run.stderr, "");
  }
});
