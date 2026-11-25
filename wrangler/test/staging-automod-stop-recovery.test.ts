import assert from "node:assert/strict";
import { test } from "node:test";
import { StagingAutomodLifecycle } from "../src/staging-automod-lifecycle.ts";
import publication from "./fixtures/staging-automod-publication.json" with { type: "json" };
import { lease, now, Storage } from "./fixtures/staging-automod-recovery.ts";

const { env, definition, identity, replies: { help, preview } } = publication;
const bot = "https://discord.com/api/v10/users/@me";
const guild = `https://discord.com/api/v10/applications/${env.DISCORD_APPLICATION_ID}/guilds/${env.GUILD_ID}/commands`;
const detail = `${guild}/${preview.id}`;
const cleanupBefore = 90_000;
const input = {
  lease, origin: "https://two-bot-next-staging.example.workers.dev", purposes: ["automod-preview"],
  verifiedRecoveryPair: true, verifiedRuntimeDatabase: true, verifiedGatewayIntents: true, cleanupReserveMs: 5_000,
};

test("AutoMod recovery holds after a lost stop acknowledgement and rechecks absence after restart", async () => {
  const storage = new Storage();
  const calls: string[] = [];
  let running = false;
  let command: typeof preview | undefined;
  let stopAttempts = 0;
  let absenceReads = 0;
  const rows = () => command ? [help, command] : [help];
  const start = (enabled = false) => new StagingAutomodLifecycle({
    enabled, storage, now,
    // The test awaits each operation before reconstructing the coordinator.
    exclusive: operation => operation(),
    env: { ...env, STAGING_RUNTIME_POLICY: "bounded", TWO_MODERATION: "1", TWO_OWEN_USER_ID: preview.id },
    get: async url => {
      if (url === bot) return identity;
      assert.equal(url, guild);
      return rows();
    },
    observe: async () => ({ lease, purpose: "automod-preview", active: true, running, cleanupBefore }),
    fenceAndDrain: async (bound, before) => {
      assert.deepEqual(bound, lease);
      assert.equal(before, cleanupBefore);
      assert.deepEqual(storage.value, held);
      calls.push("drain");
      return { lease: bound, publicationDrained: true, startupDrained: true };
    },
    stop: async (bound, before) => {
      assert.deepEqual(bound, lease);
      assert.equal(before, cleanupBefore);
      calls.push("stop");
      running = false;
      if (++stopAttempts === 1) throw new Error("stop acknowledgement lost after stop applied");
      return true;
    },
    request: async (url, init) => {
      if (init.method === "POST") {
        calls.push("create");
        assert.equal(url, guild);
        assert.deepEqual(JSON.parse(String(init.body)), definition);
        command = preview;
        return Response.json(command, { status: 201 });
      }
      if (init.method === "DELETE") {
        calls.push("delete");
        assert.equal(url, detail);
        assert.equal(running, false);
        assert.deepEqual(command, preview);
        command = undefined;
        return new Response(null, { status: 204 });
      }
      assert.equal(init.method, "GET");
      if (url === bot) {
        calls.push("bot");
        return Response.json(identity);
      }
      if (url === `${guild}?with_localizations=true`) {
        calls.push("list");
        return Response.json(rows());
      }
      assert.equal(url, detail);
      if (command) {
        calls.push("detail");
        return Response.json(command);
      }
      calls.push("absence");
      // DELETE's own readback succeeded. Lose the journal's independent read.
      if (++absenceReads === 2) throw new Error("final absence read lost");
      return Response.json({ code: 10063 }, { status: 404 });
    },
  });

  const { intent, id, version } = await start(true).publish(input);
  assert.deepEqual({ id, version }, { id: preview.id, version: preview.version });
  assert.deepEqual(calls.splice(0), ["bot", "list", "list", "create", "list", "detail"]);
  const ready = { intent, created: { id, version }, phase: "ready" };
  assert.deepEqual(storage.value, ready);
  const held = { ...ready, phase: "cleanup" };
  const assertHeld = async () => {
    assert.deepEqual(storage.value, held);
    await assert.rejects(start(true).publish(input), /staging_automod_lifecycle_refused/);
    await assert.rejects(start().otherUse(async () => assert.fail("competing writer ran")), /staging_automod_lifecycle_refused/);
    assert.deepEqual(calls, []);
  };

  // The isolated dry run is already running when recovery begins.
  running = true;
  await assert.rejects(start().recover(lease), /staging_automod_cleanup_unverified/);
  assert.equal(running, false);
  assert.deepEqual(command, preview);
  assert.deepEqual(calls.splice(0), ["drain", "stop"]);
  await assertHeld();

  await assert.rejects(start().recover({ ...lease, runAttempt: lease.runAttempt + 1 }), /staging_automod_lifecycle_refused/);
  assert.deepEqual(calls, []);
  await assert.rejects(start().recover(lease), /staging_automod_cleanup_unverified/);
  assert.deepEqual(calls.splice(0), ["drain", "stop", "bot", "list", "detail", "bot", "list", "detail",
    "delete", "list", "absence", "bot", "list", "absence"]);
  assert.deepEqual(rows(), [help]);
  await assertHeld();

  await start().recover(lease);
  assert.deepEqual(calls.splice(0), ["drain", "stop", "bot", "list", "absence"]);
  const complete = { ...held, phase: "complete" };
  assert.deepEqual(storage.value, complete);
  await start().recover(lease);
  assert.deepEqual(calls, []);
  assert.equal(await start().otherUse(async () => "released"), "released");
  await assert.rejects(start(true).publish(input), /staging_automod_publication_uncertain/);
  assert.deepEqual(calls, ["bot", "list"]);
  assert.deepEqual(storage.value, complete);
});
