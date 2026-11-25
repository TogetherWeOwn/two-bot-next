import assert from "node:assert/strict";
import { test } from "node:test";
import { StagingAutomodLifecycle } from "../src/staging-automod-lifecycle.ts";
import type { StagingUse } from "../src/staging-lifecycle.ts";
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
const dryRun = {
  memberIntake: false, enforcement: false, resume: false, registrySync: false, interactionDispatch: false,
  dedicatedIntents: true,
};
type Dependencies = ConstructorParameters<typeof StagingAutomodLifecycle>[0];

function fixture() {
  const storage = new Storage();
  const events: string[] = [];
  const drainedLease: Readonly<StagingUse> = lease;
  const state = {
    at: 2_000, lease: { ...lease }, active: true, running: false, stopped: true, leaveRunning: false, drain: true,
    startupDrained: true, drainedLease, cleanupBefore: 90_000, absenceCode: 10063,
    loseDelete: false, loseDetail: false, command: undefined as typeof preview | undefined, help,
    planned: { ...dryRun }, effective: { ...dryRun }, post: async () => {}, boot: async () => {}, readback: async () => {},
  };
  const journal = () => storage.value as { phase: string; created: unknown } | undefined;
  const rows = () => state.command ? [state.help, state.command] : [state.help];
  let tail = Promise.resolve();
  function exclusive<T>(operation: () => Promise<T>): Promise<T> {
    const result = tail.then(operation);
    tail = result.then(() => {}, () => {});
    return result;
  }
  const boot: NonNullable<Dependencies["dryRun"]>["start"] = async (bound, policy, before) => {
    assert.deepEqual(bound, lease);
    assert.deepEqual(policy, dryRun);
    assert.ok(Object.isFrozen(policy));
    assert.equal(before, lease.expiresAt - input.cleanupReserveMs);
    assert.equal(journal()?.phase, "starting");
    events.push("boot");
    await state.boot();
    state.running = true;
  };
  const start = (enabled?: boolean, runtime = true) => new StagingAutomodLifecycle({
    ...(enabled === undefined ? {} : { enabled }), storage, exclusive, now: () => state.at,
    env: { ...env, STAGING_RUNTIME_POLICY: "bounded", TWO_MODERATION: "1", TWO_OWEN_USER_ID: preview.id },
    ...(runtime ? { dryRun: { policy: state.planned, start: boot } } : {}),
    observe: async () => {
      if (state.running) await state.readback();
      return { lease: state.lease, purpose: "automod-preview", active: state.active, running: state.running,
        policy: state.effective, cleanupBefore: state.cleanupBefore };
    },
    fenceAndDrain: async (bound, before) => {
      assert.deepEqual(bound, lease);
      assert.equal(before, state.cleanupBefore);
      assert.equal(journal()?.phase, "cleanup");
      events.push("drain");
      return { lease: state.drainedLease, publicationDrained: state.drain, startupDrained: state.startupDrained };
    },
    stop: async (bound, before) => {
      assert.deepEqual(bound, lease);
      assert.equal(before, state.cleanupBefore);
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
        case detail:
          if (state.loseDetail) throw new Error("lost detail response");
          return state.command?.id === preview.id ? Response.json(state.command)
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
  f.state.lease.runAttempt--;
  f.state.active = false;
  await assert.rejects(f.start(true).publish(input), refusal);
  await f.start().recover();
  assert.deepEqual(f.events, []);
  await f.start().otherUse(f.other);
});

test("AutoMod dry-run start requires verified publication and intake-off admission", async () => {
  const f = fixture();
  await assert.rejects(f.start(true).startDryRun(lease), refusal);
  f.state.loseDetail = true;
  await assert.rejects(f.start(true).publish(input), refusal);
  assert.ok(f.journal()?.created);
  await assert.rejects(f.start(true).startDryRun(lease), refusal);
  assert.equal(f.events.includes("boot"), false);
  f.state.loseDetail = false;
  await f.start().recover();

  const ready = fixture();
  await ready.start(true).publish(input);
  await assert.rejects(ready.start().startDryRun(lease), refusal);
  await assert.rejects(ready.start(true, false).startDryRun(lease), refusal);
  await assert.rejects(ready.start(true).startDryRun({ ...lease, voiceMove: true }), refusal);
  ready.state.lease.runAttempt++;
  await assert.rejects(ready.start(true).startDryRun(lease), refusal);
  ready.state.lease.runAttempt--;
  ready.state.active = false;
  await assert.rejects(ready.start(true).startDryRun(lease), refusal);
  ready.state.active = true;
  ready.state.planned.memberIntake = true;
  await assert.rejects(ready.start(true).startDryRun(lease), refusal);
  ready.state.planned.memberIntake = false;
  ready.state.at = lease.expiresAt - input.cleanupReserveMs;
  await assert.rejects(ready.start(true).startDryRun(lease), refusal);
  assert.equal(ready.journal()?.phase, "ready");
  assert.equal(ready.events.includes("boot"), false);
});

test("AutoMod dry-run start spends its grant under the staging gate", async () => {
  const f = fixture();
  await f.start(true).publish(input);
  const entered = Promise.withResolvers<void>(), finish = Promise.withResolvers<void>();
  f.state.boot = async () => { entered.resolve(); await finish.promise; };
  const starting = f.start(true).startDryRun(lease);
  await entered.promise;
  const replay = assert.rejects(f.start(true).startDryRun(lease), refusal);
  const competing = assert.rejects(f.start().otherUse(f.other), refusal);
  finish.resolve();
  await Promise.all([starting, replay, competing]);
  assert.equal(f.state.running, true);
  assert.equal(f.events.filter(e => e === "boot").length, 1);

  const expired = fixture();
  await expired.start(true).publish(input);
  expired.storage.afterCommit = () => {
    delete expired.storage.afterCommit;
    expired.state.at = lease.expiresAt - input.cleanupReserveMs;
  };
  await assert.rejects(expired.start(true).startDryRun(lease), refusal);
  assert.equal(expired.journal()?.phase, "starting");
  assert.equal(expired.events.includes("boot"), false);
  await expired.start().recover(lease);
});

test("AutoMod startup recovery needs same-lease drain stopped readback and absence", async () => {
  const f = fixture();
  await f.start(true).publish(input);
  // Simulate a start that returned without establishing runtime isolation.
  f.state.effective.memberIntake = true;
  await assert.rejects(f.start(true).startDryRun(lease), refusal);
  assert.equal(f.state.running, true);
  f.state.at = lease.expiresAt;
  f.state.active = false;
  const recovered = f.start(undefined, false);
  f.state.startupDrained = false;
  await assert.rejects(recovered.recover(lease), refusal);
  f.state.startupDrained = true;
  f.state.drainedLease = { ...lease, epoch: lease.epoch + 1 };
  await assert.rejects(recovered.recover(lease), refusal);
  assert.equal(f.events.includes("stop"), false);

  f.state.drainedLease = lease;
  f.state.leaveRunning = true;
  const reads = f.events.filter(e => e === "read").length;
  await assert.rejects(recovered.recover(lease), refusal);
  assert.equal(f.events.filter(e => e === "read").length, reads);
  f.state.leaveRunning = false;
  f.state.absenceCode = 10004;
  await assert.rejects(recovered.recover(lease), refusal);
  await assert.rejects(recovered.otherUse(f.other), refusal);
  assert.equal(f.journal()?.phase, "cleanup");
  f.state.absenceCode = 10063;
  await recovered.recover(lease);
  await recovered.otherUse(f.other);
  assert.equal(f.journal()?.phase, "complete");
  assert.equal(f.events.filter(e => e === "delete").length, 1);
});

test("AutoMod startup timeout lets recovery check remote drain", async t => {
  for (const stall of ["boot", "readback"] as const) {
    const f = fixture();
    await f.start(true).publish(input);
    const deadline = new AbortController();
    const timeout = t.mock.method(AbortSignal, "timeout", () => deadline.signal);
    const entered = Promise.withResolvers<void>();
    f.state[stall] = () => {
      entered.resolve();
      return new Promise<void>(() => {});
    };
    const starting = assert.rejects(f.start(true).startDryRun(lease), refusal);
    await entered.promise;
    assert.deepEqual(timeout.mock.calls[0]?.arguments, [lease.expiresAt - input.cleanupReserveMs - f.state.at]);
    f.state.at = lease.expiresAt - input.cleanupReserveMs;
    f.state.readback = async () => {};
    f.state.startupDrained = false;
    const recovering = assert.rejects(f.start().recover(lease), refusal);
    deadline.abort();
    timeout.mock.restore();
    await Promise.all([starting, recovering]);
    assert.equal(f.journal()?.phase, "cleanup");
    assert.equal(f.events.includes("stop"), false);
    await assert.rejects(f.start().otherUse(f.other), refusal);

    // The remote side settled even though the client never got its response.
    f.state.running = true;
    f.state.startupDrained = true;
    await f.start().recover(lease);
    assert.equal(f.state.running, false);
    assert.equal(f.journal()?.phase, "complete");
  }
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
      delete f.storage.afterCommit;
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
