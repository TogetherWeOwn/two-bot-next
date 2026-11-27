import assert from "node:assert/strict";
import test from "node:test";
import { verifyStagingAutomodAuthorityPins as verify } from "../scripts/staging-automod-authority-pin-guard.ts";
import { STAGING_AUTOMOD_PLAN } from "../src/staging-automod-plan.ts";
import { STAGING_AUTOMOD_TARGET as target } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE as purpose, MAX_STAGING_USE_MS } from "../src/staging-use-window.ts";

const now = 100_000;
const maxClock = 8_640_000_000_000_000;
const maxAgeMs = 2_000;
const base = { schema: "two-bot-staging-automod-authority-pin.v1", environment: "staging" };
const disabled = { ...base, verdict: "disabled" };
const passed = { ...base, verdict: "passed" };

function fixture(at = now) {
  const scope = { environment: "staging", purpose, runAttempt: 1, runId: "1234", deploymentId: "deploy-1",
    sha: "a".repeat(40), epoch: 5 } as const;
  const issuedLease = { ...scope, phase: "active", acquiredAt: Math.max(0, at - maxAgeMs), expiresAt: at + 10_000 };
  const approval = (action: "publish" | "cleanup", approvedAt: number) => ({ action, approvedAt, issuedLease: { ...issuedLease },
    target: { ...target } });
  return {
    enabled: true, now: at, maxAgeMs, ...scope, target, issuedLease,
    plan: { ...STAGING_AUTOMOD_PLAN, enabled: true, allowStart: true, notAfter: at + 20_000 },
    approvals: { publish: approval("publish", Math.max(0, at - maxAgeMs)), cleanup: approval("cleanup", at) },
    promotion: { PRODUCTION_AUTO_PROMOTE_DISABLED: true, observedAt: Math.max(0, at - maxAgeMs) },
  };
}

function fails(value: unknown, code: string) {
  const result = verify(value);
  assert.deepEqual(result, { ...base, verdict: "failed", code });
  assert.ok(Object.isFrozen(result));
}

test("stays disabled without reading any evidence or invoking enablement traps", () => {
  let calls = 0;
  const trap = () => { calls++; throw new Error("synthetic sensitive input"); };
  const getter = Object.defineProperty({}, "enabled", { get: trap });
  const forged = new Proxy({}, {
    getOwnPropertyDescriptor() {
      calls++;
      return { configurable: true, enumerable: true, value: true };
    },
  });
  const revoked = Proxy.revocable(fixture(), {});
  revoked.revoke();
  for (const value of [undefined, null, "on", { enabled: 1 }, { enabled: "true" }, getter,
    Object.create(getter), Object.create({ enabled: true }), forged, revoked.proxy, new Proxy(fixture(), {}),
    new Proxy({}, { get: trap, getOwnPropertyDescriptor: trap, getPrototypeOf: trap }),
    { enabled: false, get now() { return trap(); }, get approvals() { return trap(); } }]) {
    const result = verify(value);
    assert.deepEqual(result, disabled);
    assert.ok(Object.isFrozen(result));
    assert.equal(calls, 0);
  }
});

test("passes on exact live staging pins at inclusive freshness and clock boundaries", () => {
  for (const input of [fixture(), fixture(0), fixture(maxClock)]) {
    const result = verify(input);
    assert.deepEqual(result, passed);
    assert.ok(Object.isFrozen(result));
  }
  const input = fixture(0);
  assert.deepEqual(verify({ ...input, maxAgeMs: 0 }), passed);
  assert.deepEqual(verify(Object.assign(Object.create(null), input)), passed);
});

test("rejects invalid clock, freshness policy, plan and staging scope", () => {
  const input = fixture();
  for (const value of [-1, maxClock + 1, 0.5, NaN, Infinity, "100000", 100_000n]) {
    fails({ ...input, now: value }, "invalid_clock");
  }
  for (const value of [-1, 0.5, Infinity, Number.MAX_SAFE_INTEGER + 1, "2000"]) {
    fails({ ...input, maxAgeMs: value }, "invalid_max_age");
  }
  for (const plan of [STAGING_AUTOMOD_PLAN, { ...input.plan, enabled: "true" }, { ...input.plan, allowStart: false },
    { ...input.plan, notAfter: now }, { ...input.plan, notAfter: now + 0.5 }, { ...input.plan, notAfter: Infinity },
    { ...input.plan, cleanupMs: 0 }, { ...input.plan, cleanupMs: -1 }, { ...input.plan, cleanupMs: 0.5 },
    { ...input.plan, target: { ...target, name: "other" } }]) {
    fails({ ...input, plan }, "plan_refused");
  }
  for (const change of [{ environment: "production" }, { purpose: "other" }, { runAttempt: 2 }, { runAttempt: "1" },
    { target: { ...target, applicationId: "other" } }, { target: { ...target, guildId: "other" } },
    { target: { ...target, name: "other" } }]) {
    fails({ ...input, ...change }, "scope_mismatch");
  }
});

test("binds the active issued lease and two distinct approvals to the exact run", () => {
  const input = fixture();
  const { issuedLease, approvals } = input;
  const differentPin = [{ runId: "1235" }, { deploymentId: "deploy-2" }, { sha: "b".repeat(40) }, { epoch: 6 }];
  for (const change of differentPin) fails({ ...input, ...change }, "lease_unconfirmed");
  for (const change of [{ environment: "production" }, { runAttempt: 2 }, { purpose: undefined }, { phase: "released" },
    { voiceMove: undefined }, { acquiredAt: now + 1 }, { expiresAt: now },
    { expiresAt: issuedLease.acquiredAt + MAX_STAGING_USE_MS + 1 }]) {
    fails({ ...input, issuedLease: { ...issuedLease, ...change } }, "lease_unconfirmed");
  }
  for (const action of ["publish", "cleanup"] as const) {
    const code = `${action}_approval_unconfirmed`;
    for (const change of [...differentPin, { runAttempt: 2 }, { purpose: undefined }, { environment: "production" },
      { phase: "preparing" }, { acquiredAt: issuedLease.acquiredAt + 1 }, { expiresAt: issuedLease.expiresAt + 1 }]) {
      const approval = { ...approvals[action], issuedLease: { ...issuedLease, ...change } };
      fails({ ...input, approvals: { ...approvals, [action]: approval } }, code);
    }
    for (const change of [{ action: "start" }, { target: { ...target, name: "other" } }, { issuedLease: undefined },
      { approvedAt: now - maxAgeMs - 1 }, { approvedAt: now + 1 }, { approvedAt: -1 }, { approvedAt: "100000" }]) {
      fails({ ...input, approvals: { ...approvals, [action]: { ...approvals[action], ...change } } }, code);
    }
  }
  fails({ ...input, approvals: { cleanup: approvals.cleanup } }, "publish_approval_unconfirmed");
  fails({ ...input, approvals: { publish: approvals.publish } }, "cleanup_approval_unconfirmed");
  fails({ ...input, approvals: { publish: approvals.publish, cleanup: approvals.publish } }, "cleanup_approval_unconfirmed");
});

test("requires fresh positive evidence that automatic production promotion is disabled", () => {
  const input = fixture();
  for (const value of [undefined, false, "true", 1]) {
    fails({ ...input, promotion: { ...input.promotion, PRODUCTION_AUTO_PROMOTE_DISABLED: value } }, "promotion_unconfirmed");
  }
  for (const observedAt of [undefined, now - maxAgeMs - 1, now + 1, -1, now - 0.5, NaN]) {
    fails({ ...input, promotion: { ...input.promotion, observedAt } }, "promotion_unconfirmed");
  }
});

test("rejects proxies, getters and inherited evidence without running traps or exposing values", () => {
  const input = fixture();
  let calls = 0;
  const trap = () => { calls++; throw new Error("synthetic sensitive input"); };
  const proxy = <T extends object>(value: T) => new Proxy(value, {
    get: trap, getOwnPropertyDescriptor: trap, getPrototypeOf: trap, has: trap, ownKeys: trap,
  });
  const accessor = <T extends object>(value: T, key: keyof T) =>
    Object.defineProperty({ ...value }, key, { get: trap, enumerable: true });
  const inherited = <T extends object>(value: T, key: keyof T) => {
    const copy = { ...value };
    delete copy[key];
    return Object.setPrototypeOf(copy, proxy(value));
  };
  const { plan, approvals, issuedLease, promotion } = input;
  const revoked = Proxy.revocable(issuedLease, {});
  revoked.revoke();

  fails(accessor(input, "now"), "invalid_input");
  fails(Object.setPrototypeOf({ ...input }, proxy({})), "invalid_input");
  for (const value of [proxy(plan), accessor(plan, "notAfter"), inherited(plan, "allowStart"), { ...plan, target: proxy(target) }]) {
    fails({ ...input, plan: value }, "plan_refused");
  }
  for (const value of [proxy(target), accessor(target, "name"), inherited(target, "guildId")]) {
    fails({ ...input, target: value }, "scope_mismatch");
  }
  for (const value of [proxy(issuedLease), revoked.proxy, accessor(issuedLease, "sha"), inherited(issuedLease, "runAttempt")]) {
    fails({ ...input, issuedLease: value }, "lease_unconfirmed");
  }
  fails({ ...input, approvals: proxy(approvals) }, "publish_approval_unconfirmed");
  fails({ ...input, approvals: accessor(approvals, "cleanup") }, "cleanup_approval_unconfirmed");
  for (const value of [proxy(approvals.cleanup), accessor(approvals.cleanup, "approvedAt"),
    { ...approvals.cleanup, target: proxy(target) }, { ...approvals.cleanup, issuedLease: proxy(issuedLease) }]) {
    fails({ ...input, approvals: { ...approvals, cleanup: value } }, "cleanup_approval_unconfirmed");
  }
  for (const value of [proxy(promotion), accessor(promotion, "observedAt"), inherited(promotion, "PRODUCTION_AUTO_PROMOTE_DISABLED")]) {
    fails({ ...input, promotion: value }, "promotion_unconfirmed");
  }
  fails({ ...input, runId: "synthetic sensitive input" }, "lease_unconfirmed");
  assert.equal(calls, 0);
});
