import assert from "node:assert/strict";
import test from "node:test";
import { evaluateStagingAutomodWindowBudget } from "../scripts/staging-automod-window-guard.ts";
import { STAGING_AUTOMOD_PLAN } from "../src/staging-automod-plan.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";
import { AUTOMOD_STAGING_PURPOSE, MAX_STAGING_USE_MS } from "../src/staging-use-window.ts";

const now = 2_000;
const run = { runId: "12345", sha: "a".repeat(40) };
const lease = { ...run, environment: "staging", phase: "active", purpose: AUTOMOD_STAGING_PURPOSE,
  deploymentId: "fixture", epoch: 3, runAttempt: 1, acquiredAt: 1_000, expiresAt: 7_000 } as const;
const change = { ...run, environment: "staging", action: "acquire", expectedEpoch: lease.epoch,
  deploymentId: lease.deploymentId, runAttempt: lease.runAttempt, expiresAt: lease.expiresAt } as const;
// The recovery journal's lease copy omits the issued reservation's purpose.
const { purpose: _purpose, ...journalLease } = lease;
const intent = { lease: journalLease, publishBefore: 6_000, target: STAGING_AUTOMOD_TARGET, baseline: "b".repeat(64) };
const plan = { ...STAGING_AUTOMOD_PLAN, enabled: true, allowStart: true, notAfter: lease.expiresAt, cleanupMs: 3_000 };
const input = { promotionDisabled: true, plan, run, change, lease, intent, removeBefore: 10_000 };

function expectDenied(value: unknown, at = now, label = "invalid window") {
  const decision = evaluateStagingAutomodWindowBudget(value, at);
  assert.equal(decision.admitted, false, label);
  assert.ok(Object.isFrozen(decision), label);
  assert.ok(decision.reasons.length > 0, label);
  assert.ok(Object.isFrozen(decision.reasons), label);
}

test("admits the bound window with frozen budgets measured from now", () => {
  const decision = evaluateStagingAutomodWindowBudget(input, now);
  assert.deepEqual(decision, { admitted: true, ...run, epoch: lease.epoch, remainingPublishMs: 4_000,
    remainingCleanupMs: 8_000 });
  assert.ok(Object.isFrozen(decision));
  assert.equal(evaluateStagingAutomodWindowBudget({ ...input, intent: { ...intent, lease,
    publishBefore: lease.expiresAt } }, now).admitted, true);
});

test("requires literal promotion disablement and an enabled plan with start permission", () => {
  expectDenied({ ...input, promotionDisabled: false });
  expectDenied({ ...input, promotionDisabled: "true" });
  expectDenied({ ...input, plan: STAGING_AUTOMOD_PLAN });
  expectDenied({ ...input, plan: { ...plan, allowStart: false } });
  expectDenied({ ...input, plan: { ...plan, target: { ...plan.target, name: "other" } } });
  expectDenied({ ...input, intent: { ...intent, target: { ...intent.target, guildId: "999" } } });
});

test("refuses stale or oversized lease and deadline budgets", () => {
  expectDenied(input, lease.acquiredAt - 1, "lease has not begun");
  expectDenied(input, intent.publishBefore, "publication deadline is exclusive");
  expectDenied(input, lease.expiresAt, "expired lease");
  expectDenied({ ...input, plan: { ...plan, notAfter: lease.expiresAt - 1 } }, now, "lease exceeds plan window");
  expectDenied({ ...input, plan: { ...plan, leaseMs: lease.expiresAt - now - 1 } }, now, "reservation exceeds plan budget");
  const acquiredAt = 0;
  const expiresAt = MAX_STAGING_USE_MS + 1;
  expectDenied({ ...input, lease: { ...lease, acquiredAt, expiresAt }, change: { ...change, expiresAt },
    intent: { ...intent, lease: { ...journalLease, acquiredAt, expiresAt } },
    plan: { ...plan, notAfter: expiresAt, leaseMs: expiresAt } }, now, "lease exceeds global cap");
  expectDenied({ ...input, intent: { ...intent, publishBefore: lease.expiresAt + 1 } }, now, "publication after lease");
  expectDenied({ ...input, removeBefore: intent.publishBefore - 1 }, now, "removal before publication");
  expectDenied({ ...input, removeBefore: intent.publishBefore }, now, "no room for cleanup");
  expectDenied({ ...input, removeBefore: input.removeBefore + 1 }, now, "removal exceeds cleanup budget");
});

test("refuses crossed run, reservation or intent evidence and non-first attempts", () => {
  expectDenied({ ...input, run: { ...run, runId: "12346" } });
  expectDenied({ ...input, change: { ...change, action: "release" } });
  expectDenied({ ...input, change: { ...change, expiresAt: lease.expiresAt - 1 } });
  expectDenied({ ...input, change: { ...change, expectedEpoch: lease.epoch + 1 } });
  expectDenied({ ...input, intent: { ...intent, lease: { ...journalLease, deploymentId: "other" } } });
  expectDenied({ ...input, lease: journalLease });
  expectDenied({ ...input, lease: { ...lease, voiceMove: true } });
  expectDenied({ ...input, lease: { ...lease, runAttempt: 2 }, change: { ...change, runAttempt: 2 },
    intent: { ...intent, lease: { ...journalLease, runAttempt: 2 } } });
});

test("refuses missing fields, unknown keys and non-plain objects throughout the input", () => {
  const { promotionDisabled: _promotionDisabled, ...missing } = input;
  expectDenied(missing);
  expectDenied({ ...input, plan: { ...plan, target: { applicationId: plan.target.applicationId,
    guildId: plan.target.guildId } } });
  expectDenied({ ...input, extra: true });
  expectDenied({ ...input, [Symbol("extra")]: true });
  expectDenied(Object.defineProperty({ ...input }, "hidden", { value: true }));
  expectDenied(Object.assign(Object.create(null), input));
  expectDenied(Object.assign(Object.create({ inherited: true }), input));
  expectDenied([]);
  for (const key of ["plan", "run", "change", "lease", "intent"] as const) {
    expectDenied({ ...input, [key]: { ...input[key], extra: true } }, now, key);
  }
  expectDenied({ ...input, intent: { ...intent, lease: { ...journalLease, extra: true } } });
  expectDenied({ ...input, intent: { ...intent, lease: Object.assign(Object.create({}), journalLease) } });
});

test("refuses an accessor without invoking it and handles failed reflection", () => {
  let reads = 0;
  const accessor = { ...input, get promotionDisabled() { ++reads; return true; } };
  expectDenied(accessor);
  assert.equal(reads, 0);
  const { proxy, revoke } = Proxy.revocable({ ...lease }, {});
  revoke();
  expectDenied({ ...input, lease: proxy });
});

test("refuses NaN and non-safe integers for all timestamps and plan durations", () => {
  for (const invalid of [NaN, Infinity, Number.MAX_SAFE_INTEGER + 1, 1.5, -1]) {
    expectDenied(input, invalid, "now");
    expectDenied({ ...input, removeBefore: invalid }, now, "removeBefore");
    expectDenied({ ...input, intent: { ...intent, publishBefore: invalid } }, now, "publishBefore");
    expectDenied({ ...input, change: { ...change, expiresAt: invalid } }, now, "change expiry");
    for (const key of ["notAfter", "leaseMs", "cleanupMs"] as const) {
      expectDenied({ ...input, plan: { ...plan, [key]: invalid } }, now, key);
    }
    for (const key of ["acquiredAt", "expiresAt"] as const) {
      expectDenied({ ...input, lease: { ...lease, [key]: invalid } }, now, `lease ${key}`);
      expectDenied({ ...input, intent: { ...intent, lease: { ...journalLease, [key]: invalid } } }, now, `intent ${key}`);
    }
  }
});
