import assert from "node:assert/strict";
import { createHash, createHmac, randomBytes } from "node:crypto";
import { DEPLOYMENT_HEADER, OwnershipFence } from "../../src/ownership.ts";

export const origin = "https://bot.test";
export const path = "/internal/actions";
export const scope = { keyId: "snapshot-fixture", guildId: "18446744073709551615" };
export const body = `{ "guild_id": "${scope.guildId}",\n  "action": "live.snapshot" }\n`;
const secret = randomBytes(32);
const timestamp = "1790000000";
const nonce = "0123456789abcdef0123456789abcdef";

export function signature(bytes: string | Uint8Array, target = path): string {
  const digest = createHash("sha256").update(bytes).digest("hex");
  return `sha256=${createHmac("sha256", secret).update(`POST\n${target}\n${timestamp}\n${nonce}\n${digest}`).digest("hex")}`;
}

export function request(payload = body, url = origin + path): Request {
  return new Request(url, { method: "POST", body: payload, headers: {
    "content-type": "application/json",
    "x-two-key-id": scope.keyId,
    "x-two-timestamp": timestamp,
    "x-two-nonce": nonce,
    "x-two-signature": signature(payload),
  } });
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
    channels: [{ id: "3", name: "応接室", kind: "voice", parentId: null, position: 0, overwrites: [] }],
    roles: [{ id: scope.guildId, permissions: "0" }],
    members: [{ userId: "2", roleIds: [scope.guildId], nick: null, bot: false, pending: false }],
    voice: [{ userId: "2", channelId: "3" }],
    events: [],
  },
};

export function receiver(fetch: (request: Request, port: number) => Promise<Response>) {
  const fence = new OwnershipFence({
    get: async () => ({
      deploymentId: "owner", epoch: 1, phase: "active", actor: "fixture", timestamp: "2026-11-24T20:00:00Z",
      oldEpoch: 0, oldDeploymentId: null,
    }),
  } as unknown as DurableObjectStorage);
  return {
    fence, deploymentId: "owner", stampedDeploymentId: "owner", running: () => true, fetch,
    whileOwned: <T>(run: () => Promise<T>) => run(),
  };
}
