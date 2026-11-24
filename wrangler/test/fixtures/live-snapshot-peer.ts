import assert from "node:assert/strict";
import { createHash, createHmac, randomBytes } from "node:crypto";
import { DEPLOYMENT_HEADER, OwnershipFence } from "../../src/ownership.ts";

export const origin = "https://bot.test";
export const path = "/internal/actions";
// https://github.com/TogetherWeOwn/two-web-next/blob/0d1f56a0965820af0e9f3cdf8f5b79cc3e798e/src/live/transport.ts
// https://github.com/TogetherWeOwn/two-web-next/blob/0d1f56a0965820af0e9f3cdf8f5b79cc3e798e/src/live/ingress.ts
export const webSnapshot = { responseBytes: 1_048_576, deadlineMs: 2_500 };
export const scope = { keyId: "snapshot-fixture", guildId: "18446744073709551615" };
export const body = `{ "guild_id": "${scope.guildId}",\n  "action": "live.snapshot" }\n`;
export const snapshotSecret = randomBytes(32).toString("hex");
const timestamp = "1790000000";
const nonce = "0123456789abcdef0123456789abcdef";

export function signature(bytes: string | Uint8Array, target = path, signedAt = timestamp, requestNonce = nonce): string {
  const digest = createHash("sha256").update(bytes).digest("hex");
  return `sha256=${createHmac("sha256", snapshotSecret).update(`POST\n${target}\n${signedAt}\n${requestNonce}\n${digest}`).digest("hex")}`;
}

export function request(payload = body, url = origin + path, signedAt = timestamp, requestNonce = nonce): Request {
  return new Request(url, { method: "POST", body: payload, redirect: "manual", headers: {
    "content-type": "application/json",
    "x-two-key-id": scope.keyId,
    "x-two-timestamp": signedAt,
    "x-two-nonce": requestNonce,
    "x-two-signature": signature(payload, path, signedAt, requestNonce),
  } });
}

export function stalledBody(): ReadableStream<Uint8Array> {
  return new ReadableStream({ pull: () => new Promise(() => {}) });
}

export function stalledUpload(): Request {
  return new Request(request(), { body: stalledBody(), duplex: "half" });
}

export const strippedHeaders = ["authorization", "cookie", DEPLOYMENT_HEADER, "x-forwarded-for"];

export async function verifyForwarded(inbound: Request, forward: Request, port: number): Promise<void> {
  assert.equal(port, 8091);
  assert.equal(forward.method, "POST");
  assert.equal(forward.url, "http://c/internal/actions");
  assert.equal(forward.redirect, "manual");
  const bytes = new Uint8Array(await forward.arrayBuffer());
  assert.deepEqual(bytes, new TextEncoder().encode(body));
  assert.equal(JSON.parse(new TextDecoder().decode(bytes)).guild_id, scope.guildId);
  for (const name of ["x-two-key-id", "x-two-timestamp", "x-two-nonce", "x-two-signature"]) {
    assert.equal(forward.headers.get(name), inbound.headers.get(name));
  }
  assert.equal(forward.headers.get("x-two-signature"), signature(bytes, new URL(forward.url).pathname));
  for (const name of strippedHeaders) assert.equal(forward.headers.get(name), null);
}

export const snapshot = {
  v: 1, guild_id: scope.guildId, seq: 9, observed_at: "2026-11-24T20:00:00.123456789Z",
  topics: {
    guild: { ownerId: "2", afkChannelId: "3" },
    channels: [
      { id: "3", name: "応接室", kind: "voice", parentId: null, position: 0xffff_ffff, overwrites: [] },
      { id: "4", name: "Lobby", kind: "other", parentId: null, position: 0, overwrites: [] },
    ],
    roles: [{ id: scope.guildId, permissions: "18446744073709551615" }, { id: "5", permissions: "0" }],
    members: [{ userId: "2", roleIds: [scope.guildId], nick: null, bot: false, pending: false }],
    voice: [{ userId: "2", channelId: "3" }],
    events: [],
  },
};

export const invalidSnapshots = [
  { ...snapshot, guild_id: "4" },
  { ...snapshot, seq: Number.MAX_SAFE_INTEGER + 1 },
  { ...snapshot, observed_at: "2026-02-30T00:00:00Z" },
  { ...snapshot, topics: { ...snapshot.topics, guild: { ...snapshot.topics.guild, extra: "synthetic-private-marker" } } },
  { ...snapshot, topics: { ...snapshot.topics, roles: [] } },
  { ...snapshot, topics: { ...snapshot.topics, roles: [{ id: scope.guildId, permissions: "18446744073709551616" }] } },
  { ...snapshot, topics: { ...snapshot.topics, channels: [{ ...snapshot.topics.channels[0], position: 0x1_0000_0000 }] } },
  { ok: true, result: snapshot },
];

export function receiver(fetch: (request: Request, port: number) => Promise<Response>) {
  let held = false;
  const fence = new OwnershipFence({
    get: async () => {
      assert.equal(held, true);
      return {
        deploymentId: "owner", epoch: 1, phase: "active", actor: "fixture", timestamp: "2026-11-24T20:00:00Z",
        oldEpoch: 0, oldDeploymentId: null,
      };
    },
  } as unknown as DurableObjectStorage);
  return {
    fence, deploymentId: "owner", stampedDeploymentId: "owner", running: () => true,
    fetch: async (request: Request, port: number) => {
      assert.equal(held, true);
      return fetch(request, port);
    },
    async whileOwned<T>(run: () => Promise<T>): Promise<T> {
      held = true;
      try { return await run(); }
      finally { held = false; }
    },
  };
}
