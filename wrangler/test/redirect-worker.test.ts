/** Production fetch wiring, with local store and caller-budget fixtures only. */
import { test } from "node:test";
import assert from "node:assert/strict";
import worker, { type Env } from "../src/index.ts";
import { RedirectStore } from "../src/redirect-store.ts";
import { TokenBuckets, type RedirectClick } from "../src/redirect.ts";

const env = { GUILD_ID: "synthetic-guild" } as Env;

async function fetch(path: string, headers: Record<string, string> = {}) {
  const writes: Promise<unknown>[] = [];
  const ctx = { waitUntil: (promise: Promise<unknown>) => { writes.push(promise); } };
  const response = await worker.fetch(new Request(`https://worker.invalid${path}`, { headers }),
    env, ctx as ExecutionContext);
  await Promise.all(writes);
  return response;
}

test("Worker shares misses across request-scoped store instances", async (t) => {
  const lookups: string[] = [];
  t.mock.method(RedirectStore.prototype, "lookup", async (slug: string) => {
    lookups.push(slug);
    return null;
  });
  for (const path of ["/worker-missing", "/WORKER-MISSING/", "/%77orker-missing"]) {
    assert.equal((await fetch(path, { "CF-Connecting-IP": "192.0.2.1" })).status, 404);
  }
  assert.deepEqual(lookups, ["worker-missing"]);
});

test("spoofed X-Forwarded-For cannot change the edge IP bucket or stored click", async (t) => {
  const callerKeys: string[] = [];
  const clicks: RedirectClick[] = [];
  const buckets = new TokenBuckets({ capacity: 2, refillPerSecond: 1 }, () => 0);
  const take = TokenBuckets.prototype.take;
  t.mock.method(TokenBuckets.prototype, "take", (key: string) => {
    callerKeys.push(key);
    return take.call(buckets, key);
  });
  t.mock.method(RedirectStore.prototype, "lookup", async (slug: string) => ({
    slug, inviteCode: "freshCode",
  }));
  t.mock.method(RedirectStore.prototype, "recordClick", async (click: RedirectClick) => {
    clicks.push(click);
  });
  const edge = "203.0.113.44";
  const statuses: number[] = [];
  for (const spoof of ["198.51.100.1", "198.51.100.2, 127.0.0.1", "198.51.100.3"]) {
    statuses.push((await fetch("/worker-live", {
      "CF-Connecting-IP": edge, "X-Forwarded-For": spoof,
    })).status);
  }
  assert.deepEqual(statuses, [302, 302, 429]);
  assert.deepEqual(callerKeys, [edge, edge, edge]);
  assert.equal(clicks.length, 2);
  for (const click of clicks) {
    assert.deepEqual(Object.keys(click).sort(), [
      "campaign", "code", "dedupeToken", "guildId", "occurredAt", "source",
    ]);
    assert.ok(!JSON.stringify(click).includes(edge), "visitor IP must never be stored");
    assert.ok(!JSON.stringify(click).includes("198.51.100"));
  }

  statuses.length = 0;
  for (const spoof of ["198.51.100.4", "198.51.100.5", "198.51.100.6"]) {
    statuses.push((await fetch("/worker-live", { "X-Forwarded-For": spoof })).status);
  }
  assert.deepEqual(statuses, [302, 302, 429]);
  assert.deepEqual(callerKeys.slice(3), ["unknown", "unknown", "unknown"],
    "no edge header must not fall back to client-supplied forwarding headers");
});
