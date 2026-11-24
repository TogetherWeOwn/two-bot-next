import assert from "node:assert/strict";
import { test } from "node:test";
import { admitStagingAutomod } from "../src/staging-automod.ts";
import { publishStagingAutomod, removeOwnedStagingAutomod } from "../src/staging-automod-publication.ts";

const applicationId = "1469137636663758888", guildId = "1545644954272137297";
const api = "https://discord.com/api/v10";
const commands = `${api}/applications/${applicationId}/guilds/${guildId}/commands`;
const target = { applicationId, guildId, name: "automod-preview" };
const scope = { application_id: applicationId, guild_id: guildId };
const lease = {
  environment: "staging", phase: "active", epoch: 3, deploymentId: "fixture",
  sha: "a".repeat(40), runId: "12345", runAttempt: 2, acquiredAt: 1_000, expiresAt: 60_000,
} as const;
const publishBefore = 55_000;
const admission = { lease, publishBefore, baselineAbsent: true } as const;
const env = { TWO_DEPLOY_ENV: "staging", DISCORD_APPLICATION_ID: applicationId, GUILD_ID: guildId, DISCORD_TOKEN: "synthetic" };
const intent = { lease, publishBefore, target, baseline: "a".repeat(64) };
const owned = { intent, id: "123456789012345678", version: "223456789012345678" };
const detail = `${commands}/${owned.id}`;
const help = { ...scope, name: "help", type: 1, description: "Get help", id: "1000000000000000001", version: "1000000000000000002" };
const definition = {
  name: "automod-preview", type: 1, default_member_permissions: "32",
  description: "Preview which automod filter fires on text. Smallest synthetic phrase; do not paste personal data.",
  options: [
    { name: "text", description: "Sample text to evaluate (1-2000 characters)", type: 3, required: true, min_length: 1, max_length: 2000 },
    { name: "mentions", description: "Synthetic explicit user-mention count (0-100, default 0)", type: 4, required: false, min_value: 0, max_value: 100 },
    { name: "attachments", description: "1-10 bare filenames separated only by comma + ASCII space (e.g. a.txt, b.png); no trimming",
      type: 3, required: false, max_length: 1000 },
  ],
};
const preview = { ...scope, ...definition, id: owned.id, version: owned.version };
const refused = /^Error: staging_automod_publication_refused$/;
const uncertain = /^Error: staging_automod_publication_uncertain$/;

function fixture() {
  const events: string[] = [];
  const state = {
    rows: [help], current: admission, grant: { owned, removeBefore: 70_000 }, time: 2_000,
    identity: { id: applicationId, bot: true }, postStatus: 201, posted: preview,
    detail: preview, removed: false, mutate: (_: string) => {},
  };
  const deps = {
    env,
    currentAdmission: async () => state.current,
    currentCleanupGrant: async () => state.grant,
    persistIntent: async (value: typeof intent) => {
      events.push("commit intent");
      assert.deepEqual(value, { ...intent, baseline: value.baseline });
      assert.match(value.baseline, /^[0-9a-f]{64}$/);
      assert.ok(Object.isFrozen(value) && Object.isFrozen(value.lease) && Object.isFrozen(value.target));
      return true;
    },
    recordCreated: async (value: typeof owned) => {
      events.push("commit created");
      assert.deepEqual(value, { ...owned, intent: { ...intent, baseline: value.intent.baseline } });
      return true;
    },
    request: async (url: string, init: RequestInit) => {
      assert.equal(init.redirect, "error");
      assert.equal(init.cache, "no-store");
      assert.equal(init.signal?.aborted, false);
      assert.equal(new Headers(init.headers).get("authorization"), "Bot synthetic");
      const event = `${init.method} ${url}`;
      events.push(event);
      if (init.method === "GET" && url === `${api}/users/@me`) return Response.json(state.identity);
      if (init.method === "GET" && url === commands) {
        state.mutate("list");
        return Response.json(state.rows);
      }
      if (init.method === "GET" && url === detail) {
        state.mutate("detail");
        return state.removed ? Response.json({ code: 10063 }, { status: 404 }) : Response.json(state.detail);
      }
      if (init.method === "POST" && url === commands) {
        assert.deepEqual(JSON.parse(init.body as string), definition);
        assert.equal(new Headers(init.headers).get("content-type"), "application/json");
        state.rows = [state.posted, ...state.rows];
        state.mutate("create");
        return Response.json(state.posted, { status: state.postStatus });
      }
      if (init.method === "DELETE" && url === detail) {
        state.rows = state.rows.filter(row => row.id !== owned.id);
        state.removed = true;
        state.mutate("delete");
        return new Response(null, { status: 204 });
      }
      assert.fail(`unexpected Discord operation ${event}`);
    },
  };
  return { state, deps, events, now: () => state.time };
}

const mutations = (events: string[]) => events.filter(event => /^(POST|DELETE|PUT|PATCH) /.test(event));

test("AutoMod publication commits intent before one guild create and verifies the full delta and detail", async () => {
  const { state, deps, events, now } = fixture();
  const ping = { ...help, id: "1000000000000000004", name: "ping" };
  state.rows = [help, ping];
  state.posted = { ...preview, default_permission: true, dm_permission: true, nsfw: false,
    options: [definition.options[0], { ...definition.options[1], required: undefined }, definition.options[2]] };
  state.mutate = name => {
    if (name === "create") state.rows = [state.posted, ping, Object.fromEntries(Object.entries(help).reverse())];
  };
  const actualAdmission = await admitStagingAutomod({
    lease, origin: "https://two-bot-next-staging.example.workers.dev", purposes: [target.name],
    verifiedRecoveryPair: true, verifiedRuntimeDatabase: true, verifiedGatewayIntents: true, cleanupReserveMs: 5_000,
  }, { ...env, STAGING_RUNTIME_POLICY: "bounded", TWO_MODERATION: "1", TWO_OWEN_USER_ID: owned.id },
  async () => ({ lease, running: false }), async url => url.endsWith("/@me") ? state.identity : state.rows, now);
  const receipt = await publishStagingAutomod(actualAdmission, deps, now);
  assert.deepEqual(receipt, { ...owned, intent: { ...intent, baseline: receipt.intent.baseline } });
  assert.deepEqual(events, [`GET ${api}/users/@me`, `GET ${commands}`, "commit intent", `GET ${commands}`,
    `POST ${commands}`, "commit created", `GET ${commands}`, `GET ${detail}`]);
});

test("AutoMod publication refuses wrong scope, stale admission and unsafe registries before creating", async () => {
  for (const value of [undefined, { ...admission, publishBefore: 2_000 }, { ...admission, publishBefore: lease.expiresAt },
    { ...admission, lease: { ...lease, phase: "released" } }, { ...admission, lease: { ...lease, voiceMove: true } }]) {
    const { deps, events, now } = fixture();
    await assert.rejects(publishStagingAutomod(value, deps, now), refused);
    assert.deepEqual(events, []);
  }
  for (const change of [{ DISCORD_APPLICATION_ID: "wrong" }, { GUILD_ID: "wrong" }, { TWO_DEPLOY_ENV: "production" },
    { DISCORD_TOKEN: "" }]) {
    const { deps, events, now } = fixture();
    await assert.rejects(publishStagingAutomod(admission, { ...deps, env: { ...env, ...change } }, now), refused);
    assert.deepEqual(events, []);
  }
  const full = Array.from({ length: 100 }, (_, i) => ({ ...help, id: String(1000000000000000100n + BigInt(i)), name: `cmd${i}` }));
  for (const rows of [
    [preview], [{ ...preview, type: 2 }], [help, help], [help, { ...help, id: owned.id }],
    [{ ...help, guild_id: "wrong" }], [{ ...help, application_id: "wrong" }],
    [{ ...help, id: "18446744073709551616" }], [{ ...help, version: "0" }], full,
  ]) {
    const { state, deps, events, now } = fixture();
    state.rows = rows;
    await assert.rejects(publishStagingAutomod(admission, deps, now), refused);
    assert.deepEqual(events, [`GET ${api}/users/@me`, `GET ${commands}`]);
  }
  const { state, deps, events, now } = fixture();
  state.identity = { id: guildId, bot: true };
  await assert.rejects(publishStagingAutomod(admission, deps, now), refused);
  assert.deepEqual(events, [`GET ${api}/users/@me`]);
});

test("AutoMod publication refuses uncommitted intent and rechecks the registry and lease at the write", async () => {
  for (const change of ["denied", "error", "rebound", "expired", "duplicate", "baseline drift"]) {
    const { state, deps, events, now } = fixture();
    const persistIntent = async (value: typeof intent) => {
      await deps.persistIntent(value);
      if (change === "error") throw new Error("private fixture data");
      if (change === "rebound") state.current = { ...admission, lease: { ...lease, epoch: 4 } };
      if (change === "expired") state.time = publishBefore;
      if (change === "duplicate") state.rows.push(preview);
      if (change === "baseline drift") state.rows[0] = { ...help, description: "drift without a version change" };
      return change !== "denied";
    };
    await assert.rejects(publishStagingAutomod(admission, { ...deps, persistIntent }, now), uncertain);
    assert.ok(events.includes("commit intent"));
    assert.deepEqual(mutations(events), []);
  }
});

test("AutoMod publication keeps intent on uncertain writes and persists valid receipts before failed readback", async () => {
  for (const change of ["upsert", "bad receipt", "version", "option bounds", "permissions", "delta", "detail", "persist"]) {
    const { state, deps, events, now } = fixture();
    if (change === "upsert") state.postStatus = 200;
    if (change === "bad receipt") state.posted = { ...preview, id: help.id };
    if (change === "version") state.posted = { ...preview, version: "?" };
    if (change === "option bounds") state.posted = { ...preview, options: [
      { ...definition.options[0], max_length: 2001 }, ...definition.options.slice(1),
    ] };
    if (change === "permissions") state.posted = { ...preview, default_member_permissions: "0" };
    if (change === "delta") state.mutate = event => { if (event === "create") state.rows = [preview]; };
    if (change === "detail") state.detail = { ...preview, version: help.version };
    const recordCreated = async (value: typeof owned) => {
      await deps.recordCreated(value);
      return change !== "persist";
    };
    await assert.rejects(publishStagingAutomod(admission, { ...deps, recordCreated }, now), uncertain);
    assert.equal(events.includes("commit created"), ["delta", "detail", "persist"].includes(change), change);
    assert.deepEqual(mutations(events), [`POST ${commands}`]);
  }
});

test("AutoMod cleanup uses the exact durable owned ID and verifies absence after one targeted delete", async () => {
  const { state, deps, events, now } = fixture();
  state.time = lease.expiresAt + 1;
  state.rows = [preview, help];
  const digest = await removeOwnedStagingAutomod(state.grant, deps, now);
  assert.match(digest, /^[0-9a-f]{64}$/);
  assert.deepEqual(state.rows, [help]);
  assert.deepEqual(events, [`GET ${api}/users/@me`, `GET ${commands}`, `GET ${detail}`,
    `DELETE ${detail}`, `GET ${commands}`, `GET ${detail}`]);
});

test("AutoMod cleanup refuses expired grants, changed leases and unowned or modified commands", async () => {
  for (const change of ["missing", "expired", "owned scope", "lease", "id", "version", "detail", "options", "duplicate"]) {
    const { state, deps, events, now } = fixture();
    state.rows = [preview, help];
    const grant = state.grant;
    if (change === "expired") state.time = grant.removeBefore;
    if (change === "owned scope") state.grant = { ...grant, owned: { ...owned,
      intent: { ...intent, target: { ...target, name: "help" } } } };
    if (change === "lease") state.mutate = event => {
      if (event === "detail") state.grant = { ...grant, owned: { ...owned,
        intent: { ...intent, lease: { ...lease, epoch: 4 } } } };
    };
    if (change === "id") state.rows[0] = { ...preview, id: "1000000000000000003" };
    if (change === "version") state.rows[0] = { ...preview, version: help.version };
    if (change === "detail") state.detail = { ...preview, guild_id: "wrong" };
    if (change === "options") state.detail = { ...preview, options: [
      { ...definition.options[0], autocomplete: true }, ...definition.options.slice(1),
    ] };
    if (change === "duplicate") state.rows.push({ ...preview, type: 2, id: "1000000000000000003" });
    await assert.rejects(removeOwnedStagingAutomod(change === "missing" ? undefined : grant, deps, now), refused);
    assert.deepEqual(mutations(events), [], change);
  }
});

test("AutoMod cleanup holds when unrelated rows change or the deleted ID is not affirmatively absent", async () => {
  for (const change of ["delta", "still present"]) {
    const { state, deps, events, now } = fixture();
    state.rows = [help, preview];
    state.mutate = event => {
      if (event !== "delete") return;
      if (change === "delta") state.rows = [];
      if (change === "still present") state.removed = false;
    };
    await assert.rejects(removeOwnedStagingAutomod(state.grant, deps, now), uncertain);
    assert.deepEqual(mutations(events), [`DELETE ${detail}`]);
  }
});

test("AutoMod publication bounds malformed, redirected and stalled Discord replies", { timeout: 1_000 }, async t => {
  const { deps, events, now } = fixture();
  for (const reply of [
    () => new Response('[{"name":"help","name":"automod-preview"}]'),
    () => new Response(" ".repeat(256 * 1024 + 1)),
    () => new Response(new Uint8Array([0xff])),
    () => Response.json([], { headers: { location: "https://example.com" } }),
  ]) {
    const request = async (url: string, init: RequestInit) => url === commands ? reply() : deps.request(url, init);
    await assert.rejects(publishStagingAutomod(admission, { ...deps, request }, now), refused);
  }
  const keepalive = setTimeout(() => {}, 500);
  t.after(() => clearTimeout(keepalive));
  await assert.rejects(publishStagingAutomod(admission, { ...deps, request: () => new Promise(() => {}) },
    () => publishBefore - 10), refused);
  assert.deepEqual(mutations(events), []);
});
