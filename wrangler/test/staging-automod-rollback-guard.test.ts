import assert from "node:assert/strict";
import test from "node:test";
import {
  evaluateStagingAutomodRollbackGuard as evaluate,
  type StagingAutomodRollbackGuardInput,
} from "../scripts/staging-automod-rollback-guard.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE, MAX_STAGING_USE_MS } from "../src/staging-use-window.ts";

const now = 1_800_000_000_000;
const maxTime = 8_640_000_000_000_000;
const common = { schema: "two-bot-staging-automod-rollback-guard.v1", environment: "staging" };
const scope = { runId: "12345", runAttempt: 2, deploymentId: "worker-version-1", epoch: 17 };
const sha = "a".repeat(40);
const command = { id: "1212345678901234567", version: "1234567890123456789" };
const target = STAGING_AUTOMOD_TARGET;
const lease = {
  ...scope, sha, environment: "staging", purpose: AUTOMOD_STAGING_PURPOSE, phase: "active",
  acquiredAt: now - 61_000, expiresAt: now - 1_000,
} as const;

function input() {
  return {
    enabled: true,
    now,
    pin: { ...scope, sha, target, command },
    promotion: { observedAt: now - 1_000, PRODUCTION_AUTO_PROMOTE_DISABLED: true },
    ownership: {
      observedAt: now - 1_000,
      owner: { deploymentId: scope.deploymentId, epoch: scope.epoch },
      requestedAttempt: { runId: scope.runId, runAttempt: scope.runAttempt },
      sha, use: null, runtimeState: "stopped", stopVerified: true, cleanupPending: false,
    },
    publication: { ...scope, recordedAt: now - 1_000, disposition: "not_published", target, command },
  } satisfies StagingAutomodRollbackGuardInput;
}

function expectFailure(value: unknown, code: string) {
  const result = evaluate(value);
  assert.deepEqual(result, { ...common, verdict: "failed", code });
  assert.ok(Object.isFrozen(result));
}

test("defaults to disabled without invoking getters or traps", () => {
  let calls = 0;
  const trap = () => { calls++; throw new Error("synthetic confidential data"); };
  const evidence = new Proxy({}, { get: trap, ownKeys: trap, getOwnPropertyDescriptor: trap, getPrototypeOf: trap });
  const inherited = Object.create(new Proxy({ enabled: true }, { get: trap, getOwnPropertyDescriptor: trap }));
  const accessor = Object.defineProperty({}, "enabled", { get: trap });
  const revoked = Proxy.revocable({ enabled: true }, {});
  revoked.revoke();
  for (const value of [undefined, null, true, inherited, accessor, evidence, revoked.proxy,
    ...[undefined, false, "true", 1].map(enabled => ({
      enabled, get now() { return trap(); }, get pin() { return trap(); }, promotion: evidence,
      ownership: evidence, publication: evidence,
    })),
  ]) {
    const result = evaluate(value);
    assert.deepEqual(result, { ...common, verdict: "disabled" });
    assert.ok(Object.isFrozen(result));
  }
  assert.equal(calls, 0);
});

test("accepts affirmative absence or reversion and a stopped absent or matching expired lease", () => {
  const value = input();
  for (const use of [null, lease, { ...lease, phase: "released" }]) {
    for (const disposition of ["not_published", "reverted"]) {
      const result = evaluate({ ...value, ownership: { ...value.ownership, use },
        publication: { ...value.publication, disposition } });
      assert.deepEqual(result, { ...common, verdict: "passed" });
      assert.ok(Object.isFrozen(result));
    }
  }
});

test("bounds the clock and every readback timestamp", () => {
  const value = input();
  for (const invalid of [-1, NaN, Infinity, 0.5, maxTime + 1, "1800000000000"]) {
    expectFailure({ ...value, now: invalid }, "invalid_clock");
  }
  for (const observedAt of [undefined, -1, NaN, Infinity, now + 1, now - 0.5, now - 60_001]) {
    expectFailure({ ...value, promotion: { ...value.promotion, observedAt } }, "production_auto_promotion_unconfirmed");
    expectFailure({ ...value, ownership: { ...value.ownership, observedAt } }, "staging_ownership_unconfirmed");
    expectFailure({ ...value, publication: { ...value.publication, recordedAt: observedAt } }, "staging_publication_unconfirmed");
  }
  for (const [clock, at] of [[0, 0], [now, now - 60_000], [maxTime, maxTime]] as const) {
    assert.equal(evaluate({ ...value, now: clock, promotion: { ...value.promotion, observedAt: at },
      ownership: { ...value.ownership, observedAt: at },
      publication: { ...value.publication, recordedAt: at } }).verdict, "passed");
  }
});

test("requires literal production disable and independently bound staging readbacks", () => {
  const value = input();
  for (const flag of [undefined, false, "true", 1]) {
    expectFailure({ ...value, promotion: { ...value.promotion, PRODUCTION_AUTO_PROMOTE_DISABLED: flag } },
      "production_auto_promotion_unconfirmed");
  }
  for (const patch of [{ runId: "67890" }, { runAttempt: 1 }]) {
    expectFailure({ ...value, ownership: { ...value.ownership, requestedAttempt: { ...scope, ...patch } } },
      "staging_ownership_unconfirmed");
    expectFailure({ ...value, publication: { ...value.publication, ...patch } }, "staging_publication_unconfirmed");
  }
  for (const patch of [{ deploymentId: "other-worker-version" }, { epoch: 18 }]) {
    expectFailure({ ...value, ownership: { ...value.ownership, owner: { ...scope, ...patch } } },
      "staging_ownership_unconfirmed");
    expectFailure({ ...value, publication: { ...value.publication, ...patch } }, "staging_publication_unconfirmed");
  }
  for (const patch of [{ applicationId: command.id }, { guildId: command.id }, { name: "other-command" }]) {
    expectFailure({ ...value, pin: { ...value.pin, target: { ...target, ...patch } } }, "staging_pin_invalid");
    expectFailure({ ...value, publication: { ...value.publication, target: { ...target, ...patch } } },
      "staging_publication_unconfirmed");
  }
  expectFailure({ ...value, ownership: { ...value.ownership, sha: "b".repeat(40) } }, "staging_ownership_unconfirmed");
  for (const patch of [{ runId: "012345" }, { runAttempt: 0 }, { epoch: Infinity }, { sha: "" },
    { command: { ...command, id: "18446744073709551616" } }]) {
    expectFailure({ ...value, pin: { ...value.pin, ...patch } }, "staging_pin_invalid");
  }
});

test("requires verified stop, completed cleanup and matching expiry at observation", () => {
  const value = input();
  for (const patch of [{ stopVerified: false }, { cleanupPending: true }, { runtimeState: "running" },
    { stopVerified: undefined }, { cleanupPending: 0 }, { runtimeState: "unknown" }]) {
    for (const use of [null, lease]) {
      expectFailure({ ...value, ownership: { ...value.ownership, use, ...patch } }, "staging_lease_unconfirmed");
    }
  }
  expectFailure({ ...value, ownership: { ...value.ownership, use: undefined } }, "staging_lease_unconfirmed");
  for (const patch of [{ runId: "67890" }, { runAttempt: 1 }, { deploymentId: "other-worker-version" },
    { epoch: 18 }, { sha: "b".repeat(40) }, { environment: "production" }, { purpose: undefined },
    { phase: "preparing" }, { voiceMove: true }, { expiresAt: now - 999 }, { expiresAt: Infinity },
    { acquiredAt: lease.expiresAt }, { acquiredAt: lease.expiresAt - MAX_STAGING_USE_MS - 1 }]) {
    expectFailure({ ...value, ownership: { ...value.ownership, use: { ...lease, ...patch } } }, "staging_lease_unconfirmed");
  }
});

test("rejects unconfirmed publication and extra disposition or command state", () => {
  const value = input();
  for (const patch of [{ disposition: "published" }, { disposition: "unknown" }, { disposition: undefined },
    { command: { ...command, id: command.version } }, { command: { ...command, version: command.id } },
    { command: { ...command, disposition: "published" } }, { command: null },
    { extra: [{ disposition: "published" }] }, { [Symbol("publication")]: "published" }]) {
    expectFailure({ ...value, publication: { ...value.publication, ...patch } }, "staging_publication_unconfirmed");
  }
  const hidden = Object.defineProperty({ ...value.publication }, "otherCommand", { value: "published" });
  expectFailure({ ...value, publication: hidden }, "staging_publication_unconfirmed");
  expectFailure({ ...value, publication: null }, "staging_publication_unconfirmed");
});

test("rejects malformed evidence and never returns or evaluates private data", () => {
  const value = input();
  const sentinel = "synthetic confidential data";
  let calls = 0;
  const trap = () => { calls++; throw Error(sentinel); };
  function malformed(record: object, key: string) {
    return [Object.create(record), Object.defineProperty({ ...record }, key, { get: trap }),
      new Proxy(record, { get: trap, getOwnPropertyDescriptor: trap, getPrototypeOf: trap, ownKeys: trap }),
      Object.assign([], record)];
  }
  for (const promotion of malformed(value.promotion, "PRODUCTION_AUTO_PROMOTE_DISABLED")) {
    expectFailure({ ...value, promotion }, "production_auto_promotion_unconfirmed");
  }
  for (const owner of malformed(value.ownership.owner, "epoch")) {
    expectFailure({ ...value, ownership: { ...value.ownership, owner } }, "staging_ownership_unconfirmed");
  }
  for (const use of malformed(lease, "expiresAt")) {
    expectFailure({ ...value, ownership: { ...value.ownership, use } }, "staging_lease_unconfirmed");
  }
  for (const command of malformed(value.publication.command, "version")) {
    expectFailure({ ...value, publication: { ...value.publication, command } }, "staging_publication_unconfirmed");
  }
  expectFailure({ ...value, get now() { return trap(); } }, "invalid_clock");
  expectFailure(Object.assign(Object.create(new Proxy(value, { get: trap })), { enabled: true }), "invalid_clock");
  expectFailure({ ...value, pin: { ...value.pin, deploymentId: sentinel } }, "staging_pin_invalid");
  const result = evaluate({ ...value, token: sentinel, promotion: { ...value.promotion, get token() { return trap(); } } });
  assert.deepEqual(result, { ...common, verdict: "passed" });
  assert.equal(calls, 0);
});
