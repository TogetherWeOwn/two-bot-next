import assert from "node:assert/strict";
import test from "node:test";
import { verifyStagingAutomodCleanup } from "../scripts/staging-automod-cleanup-verifier.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE } from "../src/staging-use-window.ts";

const now = 1_800_000_000_000;
const command = { id: "1545644954272137300", version: "1545644954272137301" };

function evidence(ownedCommand: typeof command | null = null) {
  const run = { sha: "a".repeat(40), runId: "123456789" };
  const expectedLease = {
    ...run,
    deploymentId: "staging-deployment",
    runAttempt: 1,
    environment: "staging",
    phase: "active",
    purpose: AUTOMOD_STAGING_PURPOSE,
    epoch: 42,
    acquiredAt: now - 180_000,
    expiresAt: now - 61_000,
  };
  return {
    promotionDisabled: true,
    run,
    expectedLease,
    ownedCommand,
    runtimeStop: { lease: { ...expectedLease }, observedAt: now - 59_999, running: false },
    publicationFence: {
      lease: { ...expectedLease }, target: { ...STAGING_AUTOMOD_TARGET }, observedAt: now - 60_000,
      publicationDrained: true, startupDrained: true,
    },
    discordRegistry: {
      lease: { ...expectedLease }, target: { ...STAGING_AUTOMOD_TARGET }, observedAt: now,
      registry: "absent", deletedCommand: ownedCommand && { ...ownedCommand },
    },
  };
}

function assertRefused(input: unknown, reason: string, clock = now) {
  const result = verifyStagingAutomodCleanup(input, clock);
  assert.deepEqual(result, { cleaned: false, reasons: [reason] });
  assert.equal(Object.isFrozen(result), true);
  assert.equal(Object.isFrozen(result.reasons), true);
}

test("qualifies fresh never-created absence on the original expired lease", () => {
  const input = evidence();
  const result = verifyStagingAutomodCleanup(input, now);
  assert.deepEqual(result, { cleaned: true, ...input.run, epoch: input.expectedLease.epoch, verifiedAt: now });
  assert.equal(Object.isFrozen(result), true);
});

test("qualifies absence after deletion of the exact owned command revision", () => {
  const input = evidence(command);
  assert.deepEqual(verifyStagingAutomodCleanup(input, now),
    { cleaned: true, ...input.run, epoch: input.expectedLease.epoch, verifiedAt: now });
});

test("requires an affirmative promotion disable readback", () => {
  for (const promotionDisabled of [false, "true", 1]) {
    assertRefused({ ...evidence(), promotionDisabled }, "promotion_not_disabled");
  }
});

test("rejects stale, future and invalid observation times and clocks", () => {
  const input = evidence();
  for (const observedAt of [now - 60_001, now + 1, NaN, Infinity, -Infinity, `${now}`]) {
    assertRefused({ ...input, runtimeStop: { ...input.runtimeStop, observedAt } }, "runtime_stop_unverified");
    assertRefused({ ...input, publicationFence: { ...input.publicationFence, observedAt } }, "publication_drain_unverified");
    assertRefused({ ...input, discordRegistry: { ...input.discordRegistry, observedAt } }, "registry_absence_unverified");
  }
  for (const clock of [NaN, Infinity, -Infinity, -1, now + 0.5]) {
    assertRefused(input, "invalid_evidence", clock);
  }
});

test("rejects a registry read taken before the confirmed stop", () => {
  const input = evidence();
  const earlyRegistry = { ...input.discordRegistry, observedAt: now - 1 };
  assertRefused({ ...input, discordRegistry: earlyRegistry, runtimeStop: { ...input.runtimeStop, observedAt: now } },
    "registry_absence_unverified");
});

test("requires remote startup drain before confirming the runtime stop", () => {
  const input = evidence();
  const { startupDrained, ...missingStartupDrain } = input.publicationFence;
  assertRefused({ ...input, publicationFence: missingStartupDrain }, "publication_drain_unverified");
  for (const startupDrained of [false, null, "true"]) {
    assertRefused({ ...input, publicationFence: { ...input.publicationFence, startupDrained } },
      "publication_drain_unverified");
  }
  assertRefused({ ...input, publicationFence: { ...input.publicationFence, observedAt: now } }, "runtime_stop_unverified");
});

test("binds all readbacks to the full original AutoMod lease", () => {
  const input = evidence();
  const lease = input.expectedLease;
  for (const mismatched of [{ epoch: 43 }, { deploymentId: "other-deployment" }, { purpose: "other" },
    { phase: "released" }, { acquiredAt: lease.acquiredAt + 1 }, { expiresAt: lease.expiresAt + 1 }]) {
    const other = { ...lease, ...mismatched };
    assertRefused({ ...input, runtimeStop: { ...input.runtimeStop, lease: other } }, "runtime_stop_unverified");
    assertRefused({ ...input, publicationFence: { ...input.publicationFence, lease: other } }, "publication_drain_unverified");
    assertRefused({ ...input, discordRegistry: { ...input.discordRegistry, lease: other } }, "registry_absence_unverified");
  }
});

test("requires recovery authority for the run and first attempt", () => {
  const input = evidence();
  assertRefused({ ...input, run: { ...input.run, runId: "987654321" } }, "recovery_not_authorized");
  assertRefused({ ...input, run: { ...input.run, sha: "b".repeat(40) } }, "recovery_not_authorized");
  for (const badLease of [{ runAttempt: 2 }, { expiresAt: NaN }, { acquiredAt: Infinity }]) {
    assertRefused({ ...input, expectedLease: { ...input.expectedLease, ...badLease } }, "recovery_not_authorized");
  }
});

test("refuses unverified runtime stops and publication drains", () => {
  const input = evidence();
  for (const running of [true, null, undefined, "false"]) {
    assertRefused({ ...input, runtimeStop: { ...input.runtimeStop, running } }, "runtime_stop_unverified");
  }
  for (const publicationDrained of [false, null, undefined, "true"]) {
    assertRefused({ ...input, publicationFence: { ...input.publicationFence, publicationDrained } },
      "publication_drain_unverified");
  }
});

test("refuses residual commands and mismatched Discord targets", () => {
  const input = evidence();
  assertRefused({ ...input, discordRegistry: { ...input.discordRegistry, registry: "present" } },
    "registry_absence_unverified");
  assertRefused({ ...input, discordRegistry: { ...input.discordRegistry, remaining: [STAGING_AUTOMOD_TARGET] } },
    "registry_absence_unverified");
  for (const mismatch of [{ applicationId: command.id }, { guildId: command.id }, { name: "other" }]) {
    const target = { ...STAGING_AUTOMOD_TARGET, ...mismatch };
    assertRefused({ ...input, discordRegistry: { ...input.discordRegistry, target } }, "registry_absence_unverified");
    assertRefused({ ...input, publicationFence: { ...input.publicationFence, target } }, "publication_drain_unverified");
  }
});

test("refuses missing, name-only, malformed and mismatched deletion proof", () => {
  const input = evidence(command);
  for (const deletedCommand of [null, { name: STAGING_AUTOMOD_TARGET.name }, { ...command, id: command.version },
    { ...command, version: command.id }, { ...command, version: "18446744073709551616" }, { ...command, extra: true }]) {
    assertRefused({ ...input, discordRegistry: { ...input.discordRegistry, deletedCommand } }, "command_deletion_unverified");
  }
  assertRefused({ ...input, ownedCommand: null }, "command_deletion_unverified");
});

test("rejects unknown, missing, inherited and accessor fields", () => {
  const input = evidence();
  const { ownedCommand, ...missing } = input;
  assertRefused(missing, "invalid_evidence");
  assertRefused({ ...input, extra: true }, "invalid_evidence");
  assertRefused({ ...input, [Symbol("extra")]: true }, "invalid_evidence");
  const hidden = Object.defineProperty({ ...input }, "extra", { value: true });
  assertRefused(hidden, "invalid_evidence");
  assertRefused(Object.assign(Object.create({ inherited: true }), input), "invalid_evidence");
  assertRefused({ ...input, expectedLease: { ...input.expectedLease, extra: true } }, "recovery_not_authorized");
  assertRefused({ ...input, discordRegistry: { ...input.discordRegistry,
    target: { ...STAGING_AUTOMOD_TARGET, extra: true } } }, "registry_absence_unverified");

  let read = false;
  const accessor = Object.defineProperty({ ...input }, "promotionDisabled", { get: () => { read = true; return true; } });
  assertRefused(accessor, "invalid_evidence");
  assert.equal(read, false);
});
