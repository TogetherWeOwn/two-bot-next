import assert from "node:assert/strict";
import { test } from "node:test";
import { admitStagingAutomod } from "../src/staging-automod.ts";

const app = "1469137636663758888", guild = "1545644954272137297";
const lease = {
  environment: "staging", phase: "active", epoch: 3, deploymentId: "fixture",
  sha: "a".repeat(40), runId: "12345", runAttempt: 2, acquiredAt: 1_000, expiresAt: 60_000,
} as const;
const input = {
  origin: "https://two-bot-next-staging.example.workers.dev", purposes: ["automod-preview"], lease,
  verifiedRecoveryPair: true, verifiedRuntimeDatabase: true, verifiedGatewayIntents: true, cleanupReserveMs: 5_000,
};
const env = {
  TWO_DEPLOY_ENV: "staging", STAGING_RUNTIME_POLICY: "bounded", GUILD_ID: guild, DISCORD_APPLICATION_ID: app,
  DISCORD_TOKEN: "fixture", TWO_MODERATION: "1", TWO_OWEN_USER_ID: "123456789012345678",
};
const stopped = async () => ({ lease, running: false });
const now = () => 2_000;
const refused = /staging_automod_admission_refused/;
const command = { id: "1000000000000000000", version: "1000000000000000001", application_id: app, guild_id: guild, name: "help" };

test("AutoMod admission proves stopped absence and spare capacity under the exact lease", async () => {
  const calls: string[] = [];
  const result = await admitStagingAutomod(input, env, async () => {
    calls.push("stopped");
    return stopped();
  }, async (url, token, signal) => {
    calls.push(url);
    assert.equal(token, "fixture");
    assert.equal(signal.aborted, false);
    return url.endsWith("/@me") ? { id: app, bot: true } : [command];
  }, now);
  assert.deepEqual(calls, ["stopped", "https://discord.com/api/v10/users/@me",
    `https://discord.com/api/v10/applications/${app}/guilds/${guild}/commands`, "stopped"]);
  assert.deepEqual(result, { lease, baselineAbsent: true, publishBefore: 55_000 });
  assert.equal(Object.isFrozen(result.lease), true);
});

test("AutoMod admission refuses mixed modes, invalid bindings, missing proofs and uncertain absence", async () => {
  const noGet = async () => assert.fail("must refuse before reading Discord");
  for (const change of [
    { origin: "https://two-bot-next.example.workers.dev" }, { origin: `${input.origin}\n` },
    ...["help", "voice", "certify", "promote"].map(mode => ({ purposes: ["automod-preview", mode] })),
    { purposes: [] }, { verifiedRecoveryPair: false }, { verifiedRuntimeDatabase: false },
    { verifiedGatewayIntents: false }, { cleanupReserveMs: 0 }, { cleanupReserveMs: 58_000 },
  ]) await assert.rejects(admitStagingAutomod({ ...input, ...change }, env, stopped, noGet, now), refused);
  for (const change of [
    { TWO_DEPLOY_ENV: "production" }, { STAGING_RUNTIME_POLICY: "always" }, { GUILD_ID: "wrong" },
    { DISCORD_APPLICATION_ID: "wrong" }, { TWO_AUTOMOD: "1" }, { TWO_MODERATION: "0" },
    { TWO_OWEN_USER_ID: "1" }, { TWO_MODERATION_PROTECTED_ROLE_IDS: `${env.TWO_OWEN_USER_ID},` },
  ]) await assert.rejects(admitStagingAutomod(input, { ...env, ...change }, stopped, noGet, now), refused);
  for (const current of [
    { ...lease, phase: "released" as const }, { ...lease, sha: "b".repeat(40) }, { ...lease, epoch: 4 },
    { ...lease, runAttempt: 1 }, { ...lease, expiresAt: 50_000 }, { ...lease, voiceMove: true as const },
  ]) await assert.rejects(admitStagingAutomod(input, env, async () => ({ lease: current, running: false }), noGet, now), refused);
  await assert.rejects(admitStagingAutomod(input, env, async () => ({ lease, running: true }), noGet, now), refused);

  const full = Array.from({ length: 100 }, (_, i) => ({ ...command,
    id: String(1000000000000000000n + BigInt(i)), name: `command${i}` }));
  for (const list of [null, [{ ...command, name: "automod-preview" }], full]) {
    await assert.rejects(admitStagingAutomod(input, env, stopped,
      async url => url.endsWith("/@me") ? { id: app, bot: true } : list, now), refused);
  }
  await assert.rejects(admitStagingAutomod(input, env, stopped, async () => ({ id: "wrong", bot: true }), now), refused);
});

test("AutoMod admission rechecks the lease and cleanup deadline after registry readback", async () => {
  for (const change of [{ lease: { ...lease, epoch: 4 } }, { running: true }, {}]) {
    let read = false;
    const observe = async () => ({ lease, running: false, ...(read ? change : {}) });
    await assert.rejects(admitStagingAutomod(input, env, observe, async url => {
      if (url.endsWith("/@me")) return { id: app, bot: true };
      read = true;
      return [];
    }, () => read && Object.keys(change).length === 0 ? lease.expiresAt : now()), refused);
    assert.equal(read, true);
  }
});
