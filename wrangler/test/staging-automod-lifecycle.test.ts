import assert from "node:assert/strict";
import { test } from "node:test";
import { StagingAutomodLifecycle } from "../src/staging-automod-lifecycle.ts";
import publication from "./fixtures/staging-automod-publication.json" with { type: "json" };
import { lease, Storage } from "./fixtures/staging-automod-recovery.ts";

const { env, definition, identity, replies } = publication;
const { help, preview } = replies;
const guild = `https://discord.com/api/v10/applications/${env.DISCORD_APPLICATION_ID}/guilds/${env.GUILD_ID}/commands`;
const detail = `${guild}/${preview.id}`;
const input = {
  lease, origin: "https://two-bot-next-staging.example.workers.dev", purposes: ["automod-preview"],
  verifiedRecoveryPair: true, verifiedRuntimeDatabase: true, verifiedGatewayIntents: true, cleanupReserveMs: 5_000,
};
const refusal = /staging_automod_/;

function fixture() {
  const storage = new Storage();
  const events: string[] = [];
  const state = {
    at: 2_000, lease: { ...lease }, running: false, stopped: true, leaveRunning: false, drain: true, absenceCode: 10063,
    loseDelete: false, command: undefined as typeof preview | undefined, help,
    post: async () => {},
  };
  const journal = () => storage.value as { phase: string; created: unknown } | undefined;
  const rows = () => state.command ? [state.help, state.command] : [state.help];
  let tail = Promise.resolve();
  function exclusive<T>(operation: () => Promise<T>): Promise<T> {
    const result = tail.then(operation);
    tail = result.then(() => {}, () => {});
    return result;
  }
  const start = (enabled?: boolean) => new StagingAutomodLifecycle({
    ...(enabled === undefined ? {} : { enabled }), storage, exclusive, now: () => state.at,
    env: { ...env, STAGING_RUNTIME_POLICY: "bounded", TWO_MODERATION: "1", TWO_OWEN_USER_ID: preview.id },
    observe: async () => ({ lease: state.lease, purpose: "automod-preview", running: state.running, cleanupBefore: 90_000 }),
    fenceAndDrain: async bound => {
      assert.deepEqual(bound, lease);
      assert.equal(journal()?.phase, "cleanup");
      events.push("drain");
      return state.drain;
    },
    stop: async bound => {
      assert.deepEqual(bound, lease);
      events.push("stop");
      if (state.stopped && !state.leaveRunning) state.running = false;
      return state.stopped;
    },
    get: async (url, token) => {
      assert.equal(token, "synthetic");
      events.push("preflight");
      if (url === "https://discord.com/api/v10/users/@me") return identity;
      assert.equal(url, guild);
      return rows();
    },
    request: async (url, init) => {
      assert.equal(new Headers(init.headers).get("authorization"), "Bot synthetic");
      if (init.method === "POST") {
        assert.equal(url, guild);
        assert.deepEqual(JSON.parse(String(init.body)), definition);
        assert.equal(journal()?.phase, "publishing");
        assert.equal(journal()?.created, null);
        assert.equal(state.running, false);
        events.push("post");
        await state.post();
        state.command = preview;
        return Response.json(preview, { status: 201 });
      }
      if (init.method === "DELETE") {
        assert.equal(url, detail);
        assert.deepEqual(journal()?.created, { id: preview.id, version: preview.version });
        assert.equal(journal()?.phase, "cleanup");
        assert.equal(state.running, false);
        events.push("delete");
        state.command = undefined;
        if (state.loseDelete) throw new Error("interrupted after delete");
        return new Response(null, { status: 204 });
      }
      assert.equal(init.method, "GET");
      events.push("read");
      switch (url) {
        case "https://discord.com/api/v10/users/@me": return Response.json(identity);
        case `${guild}?with_localizations=true`: return Response.json(rows());
        case detail: return state.command?.id === preview.id ? Response.json(state.command)
          : Response.json({ code: state.absenceCode }, { status: 404 });
        default: throw new Error("unexpected synthetic route");
      }
    },
  });
  const other = async () => { events.push("other"); };
  return { start, state, events, storage, journal, other };
}

test("AutoMod lifecycle is off by default and rejects mixed or stale staging use", async () => {
  const f = fixture();
  await assert.rejects(f.start().publish(input), refusal);
  await assert.rejects(f.start(true).publish({ ...input, purposes: ["automod-preview", "help"] }), refusal);
  f.state.lease.runAttempt++;
  await assert.rejects(f.start(true).publish(input), refusal);
  await f.start().recover();
  assert.deepEqual(f.events, []);
  await f.start().otherUse(f.other);
});

test("AutoMod lifecycle serializes writers and holds the journal until exact cleanup", async () => {
  const f = fixture();
  const entered = Promise.withResolvers<void>(), finish = Promise.withResolvers<void>();
  f.state.post = async () => { entered.resolve(); await finish.promise; };
  const first = f.start(true).publish(input);
  await entered.promise;
  const second = assert.rejects(f.start(true).publish(input), refusal);
  const competing = assert.rejects(f.start().otherUse(f.other), refusal);
  assert.equal(f.events.includes("other"), false);
  finish.resolve();
  assert.equal((await first).id, preview.id);
  await Promise.all([second, competing]);

  f.state.running = true;
  await f.start().recover(lease);
  assert.deepEqual(f.events.filter(e => ["post", "drain", "stop", "delete", "other"].includes(e)),
    ["post", "drain", "stop", "delete"]);
  assert.deepEqual(f.state.help, help);
  assert.equal(f.journal()?.phase, "complete");
  await f.start().otherUse(f.other);
  await assert.rejects(f.start(true).publish(input), refusal);
  assert.equal(f.events.filter(e => e === "post").length, 1);
});

test("AutoMod lifecycle recovers committed intent after cancellation, expiry or interruption", async () => {
  for (const reason of ["cancel", "expiry", "interrupt"]) {
    const f = fixture(), cancel = new AbortController();
    f.storage.afterCommit = () => {
      f.storage.afterCommit = undefined;
      if (reason === "cancel") cancel.abort();
      if (reason === "expiry") f.state.at = lease.expiresAt;
      if (reason === "interrupt") throw new Error("interrupted after intent commit");
    };
    await assert.rejects(f.start(true).publish(input, cancel.signal), refusal, reason);
    assert.equal(f.journal()?.phase, "publishing", reason);
    await assert.rejects(f.start().otherUse(f.other), refusal);
    await f.start().recover(reason === "interrupt" ? undefined : lease);
    assert.equal(f.journal()?.phase, "complete", reason);
    assert.deepEqual(f.events.filter(e => ["post", "drain", "stop", "delete"].includes(e)), ["drain", "stop"]);
  }
});

test("AutoMod lifecycle holds on undrained publication, ambiguous ownership or unproven absence", async () => {
  const f = fixture(), cancel = new AbortController();
  f.state.post = async () => { cancel.abort(); throw new Error("lost create response"); };
  await assert.rejects(f.start(true).publish(input, cancel.signal), refusal);
  await assert.rejects(f.start().recover({ ...lease, epoch: lease.epoch + 1 }), refusal);
  f.state.lease.epoch++;
  await assert.rejects(f.start().recover(), refusal);
  assert.equal(f.events.includes("drain"), false);
  f.state.lease.epoch--;
  f.state.drain = false;
  await assert.rejects(f.start().recover(), refusal);
  assert.equal(f.events.includes("stop"), false);

  // The remote write landed after the interrupted client lost its response.
  f.state.command = preview;
  f.state.drain = true;
  f.state.stopped = false;
  await assert.rejects(f.start().recover(), refusal);
  f.state.stopped = true;
  f.state.running = true;
  f.state.leaveRunning = true;
  const readsBefore = f.events.filter(e => e === "read").length;
  await assert.rejects(f.start().recover(), refusal);
  assert.equal(f.events.filter(e => e === "read").length, readsBefore);
  f.state.leaveRunning = false;
  await assert.rejects(f.start().recover(), refusal);
  f.state.command = undefined;
  f.state.help = { ...help, description: "drift" };
  await assert.rejects(f.start().recover(), refusal);
  assert.equal(f.events.includes("delete"), false);
  assert.equal(f.journal()?.phase, "cleanup");
  await assert.rejects(f.start().otherUse(f.other), refusal);
  f.state.help = help;
  await f.start().recover();
  assert.equal(f.journal()?.phase, "complete");
});

test("AutoMod lifecycle refuses a changed command and verifies absence after an interrupted delete", async () => {
  const f = fixture();
  await f.start(true).publish(input);
  f.state.command = { ...preview, version: help.version };
  await assert.rejects(f.start().recover(), refusal);
  assert.equal(f.events.includes("delete"), false);
  f.state.command = preview;
  f.state.loseDelete = true;
  await assert.rejects(f.start().recover(), refusal);
  f.state.absenceCode = 10004;
  await assert.rejects(f.start().recover(), refusal);
  await assert.rejects(f.start().otherUse(f.other), refusal);
  f.state.absenceCode = 10063;
  await f.start().recover();
  assert.equal(f.journal()?.phase, "complete");
  assert.equal(f.events.filter(e => e === "delete").length, 1);
  assert.deepEqual(f.state.help, help);
});
