import assert from "node:assert/strict";
import { test } from "node:test";
import { admitStagingAutomod } from "../src/staging-automod.ts";
import { publishStagingAutomod, removeOwnedStagingAutomod,
  type StagingAutomodAdmission, type StagingAutomodCleanupGrant } from "../src/staging-automod-publication.ts";
import fixture from "./fixtures/staging-automod-publication.json" with { type: "json" };

const { env, definition, identity, replies, cases } = fixture;
const admission = fixture.admission as StagingAutomodAdmission;
const grant = fixture.grant as StagingAutomodCleanupGrant;
const api = "https://discord.com/api/v10";
const commands = `${api}/applications/${env.DISCORD_APPLICATION_ID}/guilds/${env.GUILD_ID}/commands`;
const detail = `${commands}/${grant.owned.id}`;
const routes = { bot: ["GET", `${api}/users/@me`], list: ["GET", `${commands}?with_localizations=true`],
  post: ["POST", commands], detail: ["GET", detail], delete: ["DELETE", detail] };
const error = (kind: string) => new RegExp(`^Error: staging_automod_publication_${kind}$`);
type Scenario = { steps: (string | number | null)[][]; input?: object | null; grant?: object | null;
  env?: object; time?: number; revoke?: boolean; persist?: string | boolean; record?: boolean;
  fromPreflight?: boolean; error?: string };
function reply(key: string): unknown {
  const value = replies[key as keyof typeof replies];
  if (key === "full") return Array.from({ length: 100 }, (_, i) => ({ ...replies.help,
    name: `command${i}`, id: String(1000000000000000100n + BigInt(i)) }));
  if (Array.isArray(value)) return value.map(reply);
  if (value && "$copy" in value) {
    const { $copy, ...patch } = value;
    return { ...(reply($copy) as object), ...patch };
  }
  return value;
}

async function replay(operation: "publish" | "remove", scenarios: Record<string, Scenario>) {
  for (const [name, scenario] of Object.entries(scenarios)) {
    const calls: unknown[] = [], committed: unknown[] = [];
    const steps = [...scenario.steps];
    const input = scenario.input === null ? undefined : { ...admission, ...scenario.input };
    let time = scenario.time ?? 2_000;
    const next = (...call: unknown[]) => { calls.push(call); return steps.shift(); };
    const deps = {
      env: { ...env, ...scenario.env },
      currentAdmission: async () => scenario.revoke && committed.length ? undefined : admission,
      currentCleanupGrant: async () => scenario.revoke && calls.length === 3
        ? { ...grant, owned: { ...grant.owned, version: identity.id } } : { ...grant, ...scenario.grant },
      persistIntent: async (intent: unknown) => {
        committed.push(intent);
        next("intent");
        if (scenario.persist === "expire") time = admission.publishBefore;
        if (scenario.persist === "throw") throw new Error("private fixture data");
        return scenario.persist !== false;
      },
      recordCreated: async (owned: unknown) => {
        committed.push(owned);
        next("receipt");
        return scenario.record !== false;
      },
      request: async (url: string, init: RequestInit) => {
        const step = next(init.method, url);
        if (!step) throw new Error("unexpected Discord request");
        const [route, payload, status = 200] = step;
        const headers = new Headers(init.headers);
        assert.deepEqual([init.redirect, init.cache, init.signal?.aborted, headers.get("authorization")],
          ["error", "no-store", false, "Bot synthetic"]);
        if (route === "post") {
          assert.equal(headers.get("content-type"), "application/json");
          assert.deepEqual(JSON.parse(init.body as string), definition);
        }
        if (payload === "invalidUtf8") return new Response(new Uint8Array([0xff]));
        if (payload === "oversized") return new Response(" ".repeat(256 * 1024 + 1));
        if (payload === "redirect") return Response.json([], { headers: { location: "https://example.com" } });
        if (payload === "duplicateKey") return new Response(JSON.stringify(reply("present")).replace(
          '"name":"automod-preview"', '"name":"automod-preview","name":"other"'));
        return status === 204 ? new Response(null, { status: 204 }) : Response.json(reply(String(payload)), { status: Number(status) });
      },
    };
    const admitted = scenario.fromPreflight ? await admitStagingAutomod({
      lease: admission.lease, origin: "https://two-bot-next-staging.example.workers.dev", purposes: ["automod-preview"],
      verifiedRecoveryPair: true, verifiedRuntimeDatabase: true, verifiedGatewayIntents: true, cleanupReserveMs: 5_000,
    }, { ...env, STAGING_RUNTIME_POLICY: "bounded", TWO_MODERATION: "1", TWO_OWEN_USER_ID: grant.owned.id },
    async () => ({ lease: admission.lease, running: false }), async url => url.endsWith("/@me") ? identity : reply("baseline"),
    () => time) : input;
    const result = operation === "publish" ? publishStagingAutomod(admitted, deps, () => time)
      : removeOwnedStagingAutomod(scenario.grant === null ? undefined : { ...grant, ...scenario.grant }, deps, () => time);
    if (scenario.error) await assert.rejects(result, error(scenario.error), name);
    else {
      const receipt = await result;
      if (typeof receipt === "string") assert.match(receipt, /^[a-f0-9]{64}$/);
      else {
        assert.deepEqual(receipt, { ...grant.owned, intent: { ...grant.owned.intent, baseline: receipt.intent.baseline } });
        assert.match(receipt.intent.baseline, /^[a-f0-9]{64}$/);
        assert.deepEqual(committed, [receipt.intent, receipt]);
      }
    }
    assert.deepEqual(calls, scenario.steps.map(([kind]) => routes[kind as keyof typeof routes] ?? [kind]), name);
  }
}
test("AutoMod publication commits intent before one guild create and verifies the full delta and detail", () => replay("publish", cases.published));
test("AutoMod publication refuses wrong scope, stale admission and unsafe registries before creating", () => replay("publish", cases.refused));
test("AutoMod publication refuses uncommitted intent and rechecks the registry and lease at the write", () => replay("publish", cases.intent));
test("AutoMod publication keeps intent on uncertain writes and persists valid receipts before failed readback", () => replay("publish", cases.uncertain));
test("AutoMod cleanup uses the exact durable owned ID and verifies absence after one targeted delete", () => replay("remove", cases.removed));
test("AutoMod cleanup refuses expired grants, changed leases and unowned or modified commands", () => replay("remove", cases.unsafeCleanup));
test("AutoMod cleanup holds when unrelated rows change or the deleted ID is not affirmatively absent", () => replay("remove", cases.uncertainCleanup));
test("AutoMod publication bounds malformed, redirected and stalled Discord replies", { timeout: 1_000 }, async t => {
  await replay("publish", cases.malformed);
  const keepalive = setTimeout(() => {}, 500);
  t.after(() => clearTimeout(keepalive));
  await assert.rejects(publishStagingAutomod(admission, { env, currentAdmission: async () => admission,
    request: () => new Promise(() => {}), persistIntent: async () => assert.fail(), recordCreated: async () => assert.fail() },
  () => admission.publishBefore - 10), error("refused"));
});
