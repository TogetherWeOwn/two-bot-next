import assert from "node:assert/strict";
import { test } from "node:test";
import { MAX_AUDIT_RESPONSE_BYTES, OPS_AUDIT_PREFIX, OpsControl, OpsRefused, opsRefused } from "../src/ops-control.ts";

const key = (sequence: number) => `${OPS_AUDIT_PREFIX}${String(sequence).padStart(16, "0")}`;
const row = () => ({
  actor: "fixture-operator", action: "config_put", epoch: 1,
  timestamp: "2026-10-03T00:00:00.000Z", detail: { overrides: { TWO_AUTOMATIONS: "0" } },
});

function harness(entries: [string, unknown][]) {
  const calls: unknown[] = [];
  const storage = {
    list: async (options: { prefix: string; limit: number; reverse: boolean }) => {
      calls.push(options);
      return new Map(entries.filter(([id]) => id.startsWith(options.prefix))
        .sort(([a], [b]) => a < b ? 1 : a > b ? -1 : 0).slice(0, options.limit));
    },
    get: async () => { assert.fail("audit read must not fetch other state"); },
    put: async () => { assert.fail("audit read must not write"); },
  };
  return { calls, ops: new OpsControl(storage as unknown as DurableObjectStorage, {}) };
}

const invalid = (error: unknown) => error instanceof OpsRefused && error.reason === "storage_invalid";

for (const id of [key(0), `${OPS_AUDIT_PREFIX}hostile-key`, `${OPS_AUDIT_PREFIX}9999999999999999`]) {
  test(`audit window rejects non-writer append key ${id}`, async () => {
    await assert.rejects(harness([[id, row()]]).ops.readAudit("fixture"), invalid);
  });
}

for (const timestamp of ["0", "2026-02-30T00:00:00.000Z", "2026-10-03"]) {
  test(`audit window rejects non-writer timestamp ${timestamp}`, async () => {
    await assert.rejects(harness([[key(1), { ...row(), timestamp }]]).ops.readAudit("fixture"), invalid);
  });
}

test("audit window rejects mixed supported action details", async () => {
  const mixed = { ...row(), detail: { ...row().detail, fences: { automation: true } } };
  await assert.rejects(harness([[key(1), mixed]]).ops.readAudit("fixture"), invalid);
});

test("audit window validates malformed lookahead even when 100 newer rows fill the page", async () => {
  const entries: [string, unknown][] = Array.from({ length: 101 }, (_, n) => [key(n + 1), row()]);
  entries[0] = [key(1), null];
  const h = harness(entries);
  await assert.rejects(h.ops.readAudit("fixture"), invalid);
  assert.deepEqual(h.calls, [{ prefix: OPS_AUDIT_PREFIX, limit: 101, reverse: true }]);
});

test("audit window checks rows behind an oversized newest row before byte truncation", async () => {
  const overrides = Object.fromEntries([
    "TWO_AUTOMATIONS", "TWO_AUTOMOD", "TWO_MODERATION", "TWO_TEXT_COMMANDS",
    "TWO_ANNOUNCEMENTS", "TWO_VOICE", "TWO_ANTI_NUKE", "TWO_TEMP_VOICE",
  ].map((flag) => [flag, "\t".repeat(4096)]));
  const large = { ...row(), detail: { overrides } };
  const wire = { deploymentId: "fixture", rows: [{ id: key(2), ...large }], truncated: false };
  assert.ok(new TextEncoder().encode(JSON.stringify(wire)).byteLength > MAX_AUDIT_RESPONSE_BYTES);
  await assert.rejects(harness([[key(2), large], [key(1), { ...row(), actor: "" }]])
    .ops.readAudit("fixture"), invalid);
});

test("audit window budgets escaped multibyte deployment metadata at the actual wire boundary", async () => {
  const h = harness([[key(1), row()]]);
  const base = await h.ops.readAudit("");
  const overhead = new TextEncoder().encode(JSON.stringify(base)).byteLength;
  const remaining = MAX_AUDIT_RESPONSE_BYTES - overhead;
  const deploymentId = "é".repeat(Math.floor(remaining / 2)) + "x".repeat(remaining % 2);
  const exact = await h.ops.readAudit(deploymentId);
  assert.equal(exact.truncated, false);
  assert.equal(new TextEncoder().encode(await Response.json(exact).text()).byteLength, MAX_AUDIT_RESPONSE_BYTES);
  const overflow = await h.ops.readAudit(deploymentId + "\"");
  assert.deepEqual(overflow, { deploymentId: deploymentId + "\"", rows: [], truncated: true });
  assert.ok(new TextEncoder().encode(await Response.json(overflow).text()).byteLength <= MAX_AUDIT_RESPONSE_BYTES);
});

test("audit window refuses an envelope too large even without rows", async () => {
  const sentinel = "é".repeat(MAX_AUDIT_RESPONSE_BYTES);
  const error = await harness([]).ops.readAudit(sentinel).then(
    () => { assert.fail("oversized envelope must refuse"); },
    (error: unknown) => error,
  );
  assert.ok(error instanceof OpsRefused && error.reason === "response_too_large");
  const response = opsRefused(error);
  const wire = await response.text();
  assert.equal(response.status, 503);
  assert.equal(response.headers.get("cache-control"), "no-store");
  assert.ok(new TextEncoder().encode(wire).byteLength < 1024);
  assert.deepEqual(JSON.parse(wire), {
    error: "ops_unavailable", reason: "response_too_large", code: "response_too_large",
    next_action: "escalate_deployment",
  });
  assert.ok(!wire.includes(sentinel));
});
