import assert from "node:assert/strict";
import { test } from "node:test";
import { verifyStagingAutomodReadbackConsistency as verify } from "../scripts/staging-automod-readback-consistency.ts";
import { STAGING_AUTOMOD_TARGET } from "../src/staging-automod-recovery.ts";

const HEADER = { schema: "two-bot-staging-automod-readback-consistency.v1", environment: "staging" };
const DISABLED = { ...HEADER, verdict: "disabled" };
const VERIFIED = { ...HEADER, verdict: "verified" };
const NOW = 1_800_000_000_000;
const GUILD_READBACK = {
  target: STAGING_AUTOMOD_TARGET,
  runId: "123456789",
  deploymentId: "01234567-89ab-cdef-0123-456789abcdef",
  sourceSha: "a".repeat(40),
  ownerEpoch: 1,
  commandId: "123456789012345678",
  commandVersion: "123456789012345679",
  commandHash: "b".repeat(64),
  scope: "guild",
  extraCommandCount: 0,
  unpinnedCommandCount: 0,
};

function fixture(firstChanges = {}, secondChanges = {}) {
  const first = Object.freeze({
    ...GUILD_READBACK, sourceId: "first-collector", observedAt: NOW - 100, ...firstChanges,
  });
  const second = Object.freeze({
    ...GUILD_READBACK, sourceId: "second-collector", observedAt: NOW - 50, ...secondChanges,
  });
  return {
    input: { enabled: true, first, second },
    context: {
      now: NOW, maxAgeMs: 100, maxSkewMs: 50,
      sources: new WeakMap<object, string>([[first, "first-collector"], [second, "second-collector"]]),
      promotion: { observedAt: NOW - 100, PRODUCTION_AUTO_PROMOTE_DISABLED: true },
    },
  };
}

function expectFailed(result: ReturnType<typeof verify>, code: string) {
  assert.deepEqual(result, { ...HEADER, verdict: "failed", code });
  assert.ok(Object.isFrozen(result));
}

function traps() {
  let calls = 0;
  const trap = () => {
    calls += 1;
    throw new Error("sensitive trap error");
  };
  const proxy = new Proxy({}, { get: trap, getPrototypeOf: trap, getOwnPropertyDescriptor: trap, ownKeys: trap });
  return { trap, proxy, assertUntouched: () => assert.equal(calls, 0) };
}

test("disabled only reads an own data flag and never touches readbacks", () => {
  const { trap, proxy, assertUntouched } = traps();
  const revoked = Proxy.revocable({ enabled: true }, {});
  revoked.revoke();
  const { context } = fixture();
  const untrustedContext = new Proxy(context, { get: trap, getOwnPropertyDescriptor: trap });
  for (const input of [
    undefined, null, "true", {}, { enabled: 1 }, { enabled: "true" }, { enabled: false, get first() { return trap(); } },
    { get enabled() { return trap(); } }, Object.create({ enabled: true }), Object.create(proxy), proxy, revoked.proxy,
  ]) {
    const result = verify(input, untrustedContext);
    assert.deepEqual(result, DISABLED);
    assert.ok(Object.isFrozen(result));
  }
  assertUntouched();
});

test("verifies matching fresh authenticated guild projections at both time boundaries", () => {
  const { input, context } = fixture();
  const result = verify(input, context);
  assert.deepEqual(result, VERIFIED);
  assert.ok(Object.isFrozen(result));

  const exact = fixture({ observedAt: NOW }, { observedAt: NOW });
  assert.deepEqual(verify(Object.assign(Object.create(null), exact.input), {
    ...exact.context, maxAgeMs: 0, maxSkewMs: 0,
    promotion: { observedAt: NOW, PRODUCTION_AUTO_PROMOTE_DISABLED: true },
  }), VERIFIED);
});

test("binds immutable readbacks to two distinct independently authenticated source IDs", () => {
  const { input, context } = fixture();
  const impostor = Object.freeze({ ...input.first });
  expectFailed(verify({ ...input, first: impostor }, context), "unauthenticated_source");
  context.sources.set(input.first, "other-collector");
  expectFailed(verify(input, context), "unauthenticated_source");

  const alias = fixture({}, { sourceId: "first-collector" });
  alias.context.sources.set(alias.input.second, "first-collector");
  expectFailed(verify(alias.input, alias.context), "duplicate_source");
  expectFailed(verify({ ...alias.input, second: alias.input.first }, alias.context), "duplicate_source");
  const mutable = { ...input.first };
  context.sources.set(mutable, "first-collector");
  expectFailed(verify({ ...input, first: mutable }, context), "invalid_readback");
});

test("rejects invalid time policy, stale or future observations, and excessive skew", () => {
  const { input, context } = fixture();
  for (const invalid of [-1, 0.5, NaN, Infinity, Number.MAX_SAFE_INTEGER + 1, "100"]) {
    for (const field of ["now", "maxAgeMs", "maxSkewMs"]) {
      expectFailed(verify(input, { ...context, [field]: invalid }), "invalid_window");
    }
  }
  for (const observedAt of [NOW - 101, NOW + 1, -1, 0.5, NaN, Infinity, Number.MAX_SAFE_INTEGER + 1, String(NOW)]) {
    const stale = fixture({}, { observedAt });
    expectFailed(verify(stale.input, stale.context), "readback_not_fresh");
  }
  expectFailed(verify(input, { ...context, maxSkewMs: 49 }), "readback_skew");
});

test("requires the fixed target and agreement on every serving and command pin", () => {
  const { input, context } = fixture();
  for (const target of [
    { ...STAGING_AUTOMOD_TARGET, applicationId: "123456789012345678" },
    { ...STAGING_AUTOMOD_TARGET, guildId: "123456789012345678" },
    { ...STAGING_AUTOMOD_TARGET, name: "automod" },
    { ...STAGING_AUTOMOD_TARGET, extra: true },
  ]) {
    const changed = fixture({ target: Object.freeze(target) }, { target });
    expectFailed(verify(changed.input, changed.context), "target_mismatch");
  }
  const mutable = fixture({ target: { ...STAGING_AUTOMOD_TARGET } });
  expectFailed(verify(mutable.input, mutable.context), "target_mismatch");
  const alternatives = {
    runId: "123456780", deploymentId: "01234567-89ab-cdef-0123-456789abcdee", sourceSha: "c".repeat(40),
    ownerEpoch: 2, commandId: "123456789012345680", commandVersion: "123456789012345681", commandHash: "d".repeat(64),
  };
  for (const [key, value] of Object.entries(alternatives)) {
    const changed = fixture({}, { [key]: value });
    expectFailed(verify(changed.input, changed.context), "readback_mismatch");
  }
  for (const change of [
    { runId: "9007199254740992" }, { deploymentId: "worker" }, { sourceSha: "A".repeat(40) },
    { ownerEpoch: 0 }, { commandId: "18446744073709551616" }, { commandVersion: "00123" },
    { commandHash: "e".repeat(63) }, { commandHash: "b".repeat(64) + "\n" },
  ]) {
    const malformed = fixture(change, change);
    expectFailed(verify(malformed.input, malformed.context), "invalid_pins");
  }
  assert.deepEqual(verify(input, context), VERIFIED);
});

test("requires both collectors to confirm guild scope and zero extra or unpinned commands", () => {
  for (const change of [
    { scope: "global" }, { extraCommandCount: 1 }, { unpinnedCommandCount: 1 }, { extraCommandCount: "0" },
  ]) {
    for (const { input, context } of [fixture(change), fixture({}, change)]) {
      expectFailed(verify(input, context), "command_scope_unconfirmed");
    }
  }
});

test("requires an affirmative and fresh production auto-promotion readback", () => {
  const { input, context } = fixture();
  for (const promotion of [
    undefined, { observedAt: NOW }, { observedAt: NOW, PRODUCTION_AUTO_PROMOTE_DISABLED: "true" },
    { observedAt: NOW, PRODUCTION_AUTO_PROMOTE_DISABLED: false },
    { observedAt: NOW - 101, PRODUCTION_AUTO_PROMOTE_DISABLED: true },
    { observedAt: NOW + 1, PRODUCTION_AUTO_PROMOTE_DISABLED: true },
    Object.create({ observedAt: NOW, PRODUCTION_AUTO_PROMOTE_DISABLED: true }),
  ]) {
    expectFailed(verify(input, { ...context, promotion }), "production_auto_promotion_unconfirmed");
  }
});

test("rejects enabled trap inputs without running traps or echoing untrusted content", () => {
  const { trap, proxy, assertUntouched } = traps();
  const { input, context } = fixture();
  const secret = "private collector payload";
  expectFailed(verify({ enabled: true, get first() { return trap(); }, second: input.second }, context), "invalid_input");
  expectFailed(verify(Object.assign(Object.create(proxy), input), context), "invalid_input");
  expectFailed(verify(input, new Proxy(context, { get: trap, ownKeys: trap })), "invalid_window");
  expectFailed(verify(input, { ...context, sources: new Proxy(context.sources, { get: trap }) }), "unauthenticated_source");
  expectFailed(verify({ ...input, first: proxy }, context), "invalid_readback");
  const accessor = Object.freeze({ ...input.first, get sourceSha() { return trap(); } });
  expectFailed(verify({ ...input, first: accessor }, context), "invalid_readback");
  const hidden = Object.freeze(Object.defineProperty({ ...input.first }, Symbol(), { value: secret }));
  expectFailed(verify({ ...input, first: hidden }, context), "invalid_readback");
  const targetTrap = fixture({ target: proxy });
  expectFailed(verify(targetTrap.input, targetTrap.context), "target_mismatch");
  expectFailed(verify(input, { ...context, promotion: proxy }), "production_auto_promotion_unconfirmed");
  const promotion = { observedAt: NOW, get PRODUCTION_AUTO_PROMOTE_DISABLED() { return trap(); } };
  expectFailed(verify(input, { ...context, promotion }), "production_auto_promotion_unconfirmed");
  const sensitive = fixture({ commandHash: secret });
  expectFailed(verify(sensitive.input, sensitive.context), "invalid_pins");
  assertUntouched();
});
