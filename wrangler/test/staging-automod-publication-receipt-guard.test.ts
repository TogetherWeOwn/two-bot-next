import assert from "node:assert/strict";
import test from "node:test";
import { verifyStagingAutomodPublicationReceipt as verify } from "../scripts/staging-automod-publication-receipt-guard.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";
import { MAX_STAGING_USE_MS } from "../src/staging-use-window.ts";

const now = 1_800_000_000_000;
const maxTimestamp = 8_640_000_000_000_000;
const guildId = STAGING_AUTOMOD_TARGET.guildId;
const common = {
  schema: "two-bot-staging-automod-publication-receipt-guard.v1",
  environment: "staging",
  observedAt: new Date(now).toISOString(),
};
const binding = {
  runId: "12345", runAttempt: 1, deploymentId: "11111111-1111-4111-8111-111111111111",
  workerVersionId: "22222222-2222-4222-8222-222222222222", sourceSha: "a".repeat(40), ownerEpoch: 17,
  ruleId: "1234567890123456789", previewReceiptId: "preview-receipt-1", actionDigest: "b".repeat(64),
};
const pin = {
  ...binding, stagingGuildId: guildId, approvedMode: "dry_run", leaseAcquiredAt: now - 70_000,
  leaseExpiresAt: now + 1_000,
};
const receipt = {
  ...binding, environment: "staging", productionAutoPromoteDisabled: true, guildId, mode: "dry_run",
  publishedAt: now - 1_000, observedAt: now - 100,
};
const input = { enabled: true, pin, receipt };
const live = {
  ...input,
  pin: { ...pin, approvedMode: "live_staging" },
  receipt: { ...receipt, mode: "live_staging", approvalRunId: "45678", approvalSha: pin.sourceSha },
};

function rejects(candidate: unknown, code: string, context = code) {
  assert.deepEqual(verify(candidate, now), { ...common, verdict: "failed", code }, context);
}

test("defaults to disabled before reading evidence", t => {
  t.mock.timers.enable({ apis: ["Date"], now });
  let reads = 0;
  const getter = () => { reads++; throw new Error("accessor invoked"); };
  const candidate = {};
  Object.defineProperties(candidate, { pin: { get: getter }, receipt: { get: getter } });
  const expected = { ...common, verdict: "disabled" };
  for (const enabled of [undefined, null, false, "true", 1]) {
    const result = verify(Object.assign(candidate, { enabled }));
    assert.deepEqual(result, expected);
    assert.ok(Object.isFrozen(result));
  }
  assert.deepEqual(verify(Object.create({ enabled: true })), expected);
  assert.deepEqual(verify(Object.defineProperty({}, "enabled", { get: getter })), expected);
  assert.deepEqual(verify({}), expected);
  assert.deepEqual(verify(undefined), expected);
  assert.deepEqual(verify({}, NaN), { ...expected, observedAt: null });
  assert.equal(reads, 0);
});

test("accepts pinned dry-run and approved live-staging receipts", () => {
  for (const candidate of [input, live]) {
    const before = structuredClone(candidate);
    const result = verify(candidate, now);
    assert.deepEqual(result, { ...common, verdict: "passed" });
    assert.ok(Object.isFrozen(result));
    assert.deepEqual(candidate, before);
  }
  const ownOnly = { enabled: true, pin: Object.assign(Object.create(null), pin),
    receipt: Object.assign(Object.create(null), receipt) };
  assert.deepEqual(verify(ownOnly, now), { ...common, verdict: "passed" });
});

test("rejects every binding mismatch and equally malformed pins", () => {
  for (const [key, other] of Object.entries({
    runId: "67890", runAttempt: 2, deploymentId: binding.workerVersionId, workerVersionId: binding.deploymentId,
    sourceSha: "c".repeat(40), ownerEpoch: 18, ruleId: "1234567890123456790", previewReceiptId: "preview-receipt-2",
    actionDigest: "d".repeat(64),
  })) {
    rejects({ ...input, receipt: { ...receipt, [key]: other } }, "binding_mismatch", key);
    rejects({ ...input, pin: { ...pin, [key]: other } }, "binding_mismatch", key);
    const missing = Object.fromEntries(Object.entries(receipt).filter(([field]) => field !== key));
    rejects({ ...input, receipt: missing }, "invalid_receipt", key);
  }
  for (const [key, invalid] of [
    ["runId", "01"], ["runAttempt", 0], ["runAttempt", "1"], ["ownerEpoch", 0], ["ownerEpoch", 1.5],
    ["deploymentId", ""], ["workerVersionId", ""], ["ruleId", ""], ["previewReceiptId", " "],
    ["sourceSha", "A".repeat(40)], ["sourceSha", "a".repeat(39)], ["actionDigest", "g".repeat(64)],
    ["actionDigest", `sha256:${binding.actionDigest}`], ["actionDigest", "b".repeat(63)],
  ] as const) {
    rejects({ ...input, pin: { ...pin, [key]: invalid }, receipt: { ...receipt, [key]: invalid } }, "invalid_pin", key);
  }
});

test("rejects other environments, guilds and promotion flags", () => {
  rejects({ ...input, receipt: { ...receipt, environment: "production" } }, "environment_mismatch");
  for (const flag of [false, "true", 1, undefined]) {
    rejects({ ...input, receipt: { ...receipt, productionAutoPromoteDisabled: flag } }, "promotion_not_disabled");
  }
  for (const otherGuild of ["1234567890123456789", "", "0", `0${guildId}`, "18446744073709551616", Number(guildId)]) {
    rejects({ ...input, receipt: { ...receipt, guildId: otherGuild } }, "guild_mismatch");
    rejects({ ...input, pin: { ...pin, stagingGuildId: otherGuild }, receipt: { ...receipt, guildId: otherGuild } },
      "guild_mismatch");
  }
});

test("requires an exact mode and live approval on the pinned source", () => {
  rejects({ ...input, receipt: { ...receipt, mode: "live_staging" } }, "mode_mismatch");
  rejects({ ...live, receipt }, "mode_mismatch");
  rejects({ ...input, pin: { ...pin, approvedMode: "live" }, receipt: { ...receipt, mode: "live" } }, "mode_mismatch");
  rejects({ ...live, receipt: { ...receipt, mode: "live_staging" } }, "approval_unconfirmed");
  for (const approvalRunId of ["", " ", 45678]) {
    rejects({ ...live, receipt: { ...live.receipt, approvalRunId } }, "approval_unconfirmed");
  }
  for (const approvalSha of [undefined, "A".repeat(40), "c".repeat(40)]) {
    rejects({ ...live, receipt: { ...live.receipt, approvalSha } }, "approval_unconfirmed");
  }
});

test("enforces the lease order and inclusive freshness and lease boundaries", () => {
  const boundaryPin = { ...pin, leaseAcquiredAt: now - 60_000, leaseExpiresAt: now };
  const boundaryReceipt = { ...receipt, publishedAt: boundaryPin.leaseAcquiredAt, observedAt: now };
  assert.deepEqual(verify({ ...input, pin: boundaryPin, receipt: boundaryReceipt }, now),
    { ...common, verdict: "passed" });
  rejects({ ...input, receipt: { ...receipt, publishedAt: now - 60_001 } }, "receipt_stale");
  rejects({ ...input, receipt: { ...receipt, publishedAt: now - 60_001, observedAt: now - 60_001 } }, "receipt_stale");
  for (const badPin of [
    { leaseAcquiredAt: receipt.publishedAt + 1 }, { leaseExpiresAt: now - 1 },
    { leaseExpiresAt: pin.leaseAcquiredAt }, { leaseExpiresAt: pin.leaseAcquiredAt + MAX_STAGING_USE_MS + 1 },
    { leaseAcquiredAt: -1 }, { leaseExpiresAt: maxTimestamp + 1 },
  ]) rejects({ ...input, pin: { ...pin, ...badPin } }, "deadline_outside_window");
  for (const badReceipt of [
    { publishedAt: receipt.observedAt + 1 }, { observedAt: now + 1 }, { publishedAt: NaN },
    { observedAt: Infinity }, { observedAt: now - 0.5 }, { publishedAt: String(receipt.publishedAt) },
  ]) rejects({ ...input, receipt: { ...receipt, ...badReceipt } }, "deadline_outside_window");
});

test("validates the trusted clock before processing enabled evidence", () => {
  for (const invalid of [NaN, Infinity, -Infinity, -1, now + 0.5, maxTimestamp + 1, Number.MAX_SAFE_INTEGER]) {
    const result = verify(input, invalid);
    assert.deepEqual(result, { ...common, observedAt: null, verdict: "failed", code: "invalid_clock" });
    assert.ok(Object.isFrozen(result));
  }
  assert.equal(verify(input, maxTimestamp).observedAt, new Date(maxTimestamp).toISOString());
});

test("rejects non-data shapes and unknown own keys without invoking getters or proxy traps", () => {
  let reads = 0;
  const trap = () => { reads++; throw new Error("trap invoked"); };
  for (const field of ["receipt", "pin"] as const) {
    const evidence = input[field];
    const code = field === "receipt" ? "invalid_receipt" : "invalid_pin";
    const revoked = Proxy.revocable(evidence, {});
    revoked.revoke();
    for (const invalid of [
      null, Object.assign([], evidence), Object.create(evidence), { ...evidence, extra: true },
      { ...evidence, [Symbol("extra")]: true },
      Object.defineProperty({ ...evidence }, "hidden", { value: true }),
      Object.defineProperty({ ...evidence }, "runId", { get: trap }),
      new Proxy(evidence, { get: trap, getPrototypeOf: trap, ownKeys: trap, getOwnPropertyDescriptor: trap }),
      revoked.proxy,
    ]) rejects({ ...input, [field]: invalid }, code, field);
    rejects(Object.defineProperty({ ...input }, field, { get: trap }), code, field);
  }
  rejects({ ...live, receipt: Object.defineProperty({ ...live.receipt }, "approvalSha", { get: trap }) },
    "invalid_receipt");
  assert.equal(reads, 0);
});
