import assert from "node:assert/strict";
import { DatabaseSync } from "node:sqlite";
import { test } from "node:test";
import { TwoBotContainer, type Env } from "../src/index.ts";
import {
  MAX_AUDIT_RESPONSE_BYTES, OPS_AUDIT_PREFIX, OPS_CONFIG_PATH, OPS_KILLSWITCH_PATH,
  OpsControl, OpsRefused, opsRefused, parseConfigPut,
} from "../src/ops-control.ts";
import { DEPLOYMENT_HEADER, OWNER_KEY } from "../src/ownership.ts";

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
  const response = opsRefused(error, true);
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

const killSwitchRow = () => ({
  actor: "fixture-operator", action: "kill_switch", epoch: 1,
  timestamp: "2026-10-03T00:00:00.000Z",
  detail: { fences: { automation: true, automod: false, publish: false, gateway_park: false } },
});

test("audit window refuses a kill_switch row carrying both fences and overrides", async () => {
  const mixed = {
    ...killSwitchRow(),
    detail: { ...killSwitchRow().detail, overrides: { TWO_AUTOMATIONS: "0" } },
  };
  await assert.rejects(harness([[key(1), mixed]]).ops.readAudit("fixture"), invalid);
});

test("audit window refuses fences with an extra fifth key", async () => {
  const extra = {
    ...killSwitchRow(),
    detail: { fences: { ...killSwitchRow().detail.fences, debug: true } },
  };
  await assert.rejects(harness([[key(1), extra]]).ops.readAudit("fixture"), invalid);
});

// Writer-valid fixtures must avoid PUT-refused keys (TWO_MODERATION is refused
// at PUT time); TWO_DEPLOY_ENV keeps the same 14-character envelope.
const AFTER_DROP_FLAGS = [
  "TWO_AUTOMATIONS", "TWO_AUTOMOD", "TWO_DEPLOY_ENV", "TWO_TEXT_COMMANDS",
  "TWO_ANNOUNCEMENTS", "TWO_VOICE", "TWO_ANTI_NUKE", "TWO_TEMP_VOICE",
];

/**
 * Newest-first after-drop boundary: the older row stays small while the newest
 * row is tuned so the envelope *after dropping the oldest row* lands exactly
 * on the requested byte size. The complete two-row envelope always overflows,
 * forcing the drop path; backslash-to-ASCII swaps give one-byte steps.
 */
function afterDropFixture(afterDropBytes: number): [string, unknown][] {
  const overrides: Record<string, string> = Object.fromEntries(
    AFTER_DROP_FLAGS.map((flag) => [flag, "\\".repeat(4096)]),
  );
  const newest = { ...row(), detail: { overrides } };
  const envelope = (rows: unknown) => ({ deploymentId: "fixture", rows, truncated: true });
  const projected = { id: key(2), ...newest };
  const reduction = new TextEncoder().encode(JSON.stringify(envelope([projected]))).byteLength - afterDropBytes;
  assert.ok(reduction >= 0 && reduction <= 4096, "fixture has enough one-byte adjustments");
  overrides.TWO_TEMP_VOICE = "\\".repeat(4096 - reduction) + "a".repeat(reduction);
  const tuned = { id: key(2), ...newest };
  assert.equal(new TextEncoder().encode(JSON.stringify(envelope([tuned]))).byteLength, afterDropBytes);
  assert.ok(parseConfigPut({ ...newest.detail, actor: newest.actor, expectedEpoch: 0 }),
    "tuned newest row stays writer-valid");
  const complete = { deploymentId: "fixture", rows: [tuned, { id: key(1), ...row() }], truncated: false };
  assert.ok(new TextEncoder().encode(JSON.stringify(complete)).byteLength > MAX_AUDIT_RESPONSE_BYTES,
    "complete envelope overflows so the drop path runs");
  return [[key(1), row()], [key(2), newest]];
}

test("audit window keeps the newest row when the after-drop envelope lands exactly on the cap", async () => {
  const history = await harness(afterDropFixture(MAX_AUDIT_RESPONSE_BYTES)).ops.readAudit("fixture");
  assert.equal(history.truncated, true);
  assert.equal(history.rows.length, 1);
  assert.equal(history.rows[0]!.id, key(2));
  assert.equal(new TextEncoder().encode(JSON.stringify(history)).byteLength, MAX_AUDIT_RESPONSE_BYTES);
});

test("audit window drops the newest row too when the after-drop envelope is one byte over the cap", async () => {
  const history = await harness(afterDropFixture(MAX_AUDIT_RESPONSE_BYTES + 1)).ops.readAudit("fixture");
  assert.deepEqual(history, { deploymentId: "fixture", rows: [], truncated: true });
  assert.ok(new TextEncoder().encode(JSON.stringify(history)).byteLength <= MAX_AUDIT_RESPONSE_BYTES);
});

for (const [isRead, action] of [[true, "retry_read"], [false, "reconcile_then_retry"]] as const) {
  test(`storage_unavailable refusal scopes next_action to ${action} for ${isRead ? "reads" : "mutations"}`, async () => {
    const response = opsRefused(new OpsRefused("storage_unavailable", 503), isRead);
    assert.equal(response.status, 503);
    assert.equal(response.headers.get("cache-control"), "no-store");
    const wire = await response.text();
    assert.ok(new TextEncoder().encode(wire).byteLength <= 1024);
    assert.deepEqual(JSON.parse(wire), {
      error: "ops_unavailable", reason: "storage_unavailable", code: "storage_unavailable",
      next_action: action,
    });
  });
}

test("storage_invalid refusal escalates for both reads and mutations", async () => {
  for (const isRead of [true, false]) {
    const body = await opsRefused(new OpsRefused("storage_invalid", 503), isRead).json() as { next_action: string };
    assert.equal(body.next_action, "escalate_storage");
  }
});

// ------------------------------------------------- failing-mutation refusals
// The DO route is the authority for stored state: a PUT/POST whose storage
// fails must answer reconcile_then_retry through the real container fetch,
// never the audit read's retry_read. Offline doubles only, no live services.

const MUTATION_DO_ID = "synthetic-ops-mutation-do";
const MUTATION_TOKEN = "synthetic-ops-mutation-token-0123456789";
const MUTATION_FAILURE = "SYNTHETIC_MUTATION_STORAGE_FAILURE";
const mutationDatabase = new DatabaseSync(":memory:");

function mutationHarness() {
  const map = new Map<string, unknown>([[OWNER_KEY, {
    deploymentId: MUTATION_DO_ID, epoch: 1, phase: "active", actor: "fixture-operator",
    timestamp: "2026-10-03T00:00:00.000Z", oldEpoch: 0, oldDeploymentId: null,
  }]]);
  const ctx = {
    id: { toString: () => MUTATION_DO_ID },
    container: { running: false },
    storage: {
      get: (async (getKey: string) => {
        if (getKey === OWNER_KEY) return map.get(getKey);
        throw new Error(MUTATION_FAILURE);
      }) as DurableObjectStorage["get"],
      put: (async () => { throw new Error(MUTATION_FAILURE); }) as DurableObjectStorage["put"],
      list: (async () => new Map()) as DurableObjectStorage["list"],
      delete: async (deleteKey: string) => { map.delete(deleteKey); },
      kv: {
        get: (getKey: string) => map.get(getKey),
        put: (putKey: string, value: unknown) => { map.set(putKey, value); },
        delete: (deleteKey: string) => { map.delete(deleteKey); },
      },
      sql: {
        exec: (query: string, ...bindings: (string | number | null)[]) =>
          mutationDatabase.prepare(query).all(...bindings),
      },
      setAlarm: async () => {},
      deleteAlarm: async () => {},
      sync: async () => {},
    } as unknown as DurableObjectStorage,
    blockConcurrencyWhile<T>(callback: () => Promise<T>) { return callback(); },
  };
  const env = {
    CF_VERSION_METADATA: { id: MUTATION_DO_ID },
    OPS_CONTROL_TOKEN: MUTATION_TOKEN,
    OPS_CONFIG_WRITES_ENABLED: "1",
    OPS_KILLSWITCH_ENABLED: "1",
  };
  const bot = new TwoBotContainer(ctx as unknown as DurableObjectState<{}>, env as unknown as Env);
  return { bot, map };
}

async function refusedMutation(path: string, method: string, body: unknown) {
  const { bot, map } = mutationHarness();
  const logs: string[] = [];
  const originals = { warn: console.warn, error: console.error, log: console.log };
  const capture = (...args: unknown[]) => { logs.push(args.map(String).join(" ")); };
  console.warn = console.error = console.log = capture;
  try {
    const response = await bot.fetch(new Request(`https://do.invalid${path}`, {
      method,
      headers: {
        authorization: `Bearer ${MUTATION_TOKEN}`,
        [DEPLOYMENT_HEADER]: MUTATION_DO_ID,
        "content-type": "application/json",
      },
      body: JSON.stringify(body),
    }));
    const wire = await response.text();
    return { response, wire, body: JSON.parse(wire), logs, map };
  } finally {
    Object.assign(console, originals);
  }
}

function assertMutationRefusal(result: Awaited<ReturnType<typeof refusedMutation>>, storedKey: string) {
  assert.equal(result.response.status, 503);
  assert.match(result.response.headers.get("content-type") ?? "", /^application\/json/);
  assert.equal(result.response.headers.get("cache-control"), "no-store");
  assert.ok(new TextEncoder().encode(result.wire).byteLength <= 1024);
  assert.deepEqual(result.body, {
    error: "ops_unavailable", reason: "storage_unavailable", code: "storage_unavailable",
    next_action: "reconcile_then_retry",
  });
  assert.ok(!result.wire.includes(MUTATION_FAILURE));
  assert.ok(!result.logs.join("\n").includes(MUTATION_FAILURE));
  assert.equal(result.map.has(storedKey), false, "refused mutations store nothing");
}

test("DO failing PUT /ops/config refuses with reconcile_then_retry, never retry_read", async () => {
  assertMutationRefusal(await refusedMutation(OPS_CONFIG_PATH, "PUT",
    { overrides: { TWO_AUTOMATIONS: "0" }, expectedEpoch: 0, actor: "fixture-operator" }),
    "two-bot:ops-config:v1");
});

test("DO failing POST /ops/kill-switch refuses with reconcile_then_retry", async () => {
  assertMutationRefusal(await refusedMutation(OPS_KILLSWITCH_PATH, "POST", {
    fences: { automation: true, automod: false, publish: false, gateway_park: false },
    expectedEpoch: 0, actor: "fixture-operator",
  }), "two-bot:ops-killswitch:v1");
});
