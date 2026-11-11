// Real StagingProtectionGate.evaluate -> installationClient -> verifyBinding -> decide/revoke call chain.
// Only two boundaries are faked, both in memory and both synthetic:
//   1. globalThis.fetch is replaced by a scripted GitHub REST/blob server (no network, no real credentials).
//   2. DurableObjectState is an in-memory storage plus a promise-chain blockConcurrencyWhile.
// This is NOT workerd: it proves the gate's logic and its write-ahead fence ordering against injected
// storage/transport failures, but not real Durable Object input gates, isolate eviction or process crashes.
// A "crash" below means a storage or transport call that throws at a chosen point while the fake storage
// keeps whatever was already written, and a "reconstructed" gate is a new instance over the same storage.
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import worker, { StagingProtectionGate } from "../src/staging-protection/worker.ts";
import { crc32 } from "../src/staging-protection/encoding.ts";
import { sha256 } from "../src/staging-protection/binding.ts";

const webhookSecret = "synthetic-webhook-secret-for-fixtures-only";
const runId = "37110947191", priorId = "37110947190";
const REPO = "TogetherWeOwn/two-bot-next";
const WORKFLOW = ".github/workflows/staging-migrate.yml";
const head0 = "b".repeat(40);
const fixture = name => readFileSync(new URL(`../../crates/cutover/tests/fixtures/staging-migrate-${name}.json`, import.meta.url));

// Ephemeral synthetic App key, generated per test run and never written anywhere.
const pair = await crypto.subtle.generateKey({ name: "RSASSA-PKCS1-v1_5", modulusLength: 2048,
  publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256" }, true, ["sign", "verify"]);
const der = Buffer.from(await crypto.subtle.exportKey("pkcs8", pair.privateKey));
const privateKeyPem = `-----BEGIN PRIVATE KEY-----\n${der.toString("base64")}\n-----END PRIVATE KEY-----`;

function storedZip(name, data) {
  const filename = new TextEncoder().encode(name), content = new Uint8Array(data);
  const central = 30 + filename.length + content.length;
  const bytes = new Uint8Array(central + 46 + filename.length + 22);
  const v = new DataView(bytes.buffer), u16 = (o, n) => v.setUint16(o, n, true), u32 = (o, n) => v.setUint32(o, n, true);
  u32(0, 0x04034b50); u16(4, 20); u32(14, crc32(content)); u32(18, content.length); u32(22, content.length); u16(26, filename.length);
  bytes.set(filename, 30); bytes.set(content, 30 + filename.length);
  u32(central, 0x02014b50); u16(central + 6, 20); u32(central + 16, crc32(content)); u32(central + 20, content.length);
  u32(central + 24, content.length); u16(central + 28, filename.length); bytes.set(filename, central + 46);
  const end = central + 46 + filename.length;
  u32(end, 0x06054b50); u16(end + 8, 1); u16(end + 10, 1); u32(end + 12, 46 + filename.length); u32(end + 16, central);
  return bytes;
}

function webhook(sha = head0) {
  return { action: "requested", environment: "staging-migrate-apply", event: "workflow_dispatch", ref: "refs/heads/main", sha,
    repository: { full_name: REPO, id: 3 }, installation: { id: 2 },
    deployment_callback_url: `https://api.github.com/repos/${REPO}/actions/runs/${runId}/deployment_protection_rule` };
}
const bodyBytes = (sha) => new TextEncoder().encode(JSON.stringify(webhook(sha)));
const gateRequest = (sha) => new Request("https://gate/enqueue", { method: "POST", body: bodyBytes(sha) });

// In-memory DurableObjectStorage. failOn(op, key, value) may return "before" (throw, nothing persisted)
// or "after" (persist, then throw) to inject a storage failure at an exact step.
function memoryStorage(trace) {
  const map = new Map();
  let transactionTail = Promise.resolve();
  const s = {
    map, alarm: null, failOn: null,
    async get(key) {
      if (s.failOn?.("get", key)) throw new Error("synthetic storage failure");
      return map.has(key) ? structuredClone(map.get(key)) : undefined;
    },
    async put(key, value) {
      trace.push(`storage.put:${value.state}`);
      const mode = s.failOn?.("put", key, value);
      if (mode === "before") throw new Error("synthetic storage failure");
      map.set(key, structuredClone(value));
      if (mode === "after") throw new Error("synthetic storage failure");
    },
    async setAlarm(at) {
      trace.push("storage.setAlarm");
      if (s.failOn?.("setAlarm")) throw new Error("synthetic storage failure");
      s.alarm = at;
    },
    async delete(key) { return map.delete(key); },
    async deleteAll() { map.clear(); s.alarm = null; },
    // Transactions serialize and roll back on failure; this remains a simulation, not workerd.
    transaction(callback) {
      const run = transactionTail.then(async () => {
        const before = structuredClone([...map]), alarm = s.alarm;
        try { return await callback(s); }
        catch (error) { map.clear(); for (const [k, v] of before) map.set(k, v); s.alarm = alarm; throw error; }
      });
      transactionTail = run.catch(() => {});
      return run;
    },
  };
  return s;
}
// blockConcurrencyWhile serializes callbacks per context, as one Durable Object instance does.
function context(storage) {
  let tail = Promise.resolve();
  return { storage, blockConcurrencyWhile(callback) {
    const run = tail.then(callback);
    tail = run.catch(() => {});
    return run;
  } };
}

function harness(t) {
  const trace = [];
  const gh = {
    base: Date.now(), attempt: 2, head: head0, status: "waiting", pending: ["staging-migrate-apply"],
    compareStatus: "behind", claimText: s => s, planText: s => s, planJobs: true, blobStatus: 200,
    tokenStatus: 201, revokeMode: "ok", callbackMode: "ok", callbackDelay: 0, pauseToken: async () => {},
    before: () => {}, // before(key, count): mutate evidence between reads
    calls: [], decisions: [], tokens: 0, revoked: [], inflight: 0, maxInflight: 0, counts: {},
  };
  const stamp = delta => new Date(Math.floor((gh.base + delta) / 1000) * 1000).toISOString().replace(".000Z", "Z");
  const logs = [];
  for (const level of ["log", "info", "warn", "error", "debug"]) t.mock.method(console, level, (...args) => { logs.push(args.map(String).join(" ")); });
  const runBody = (id, extra) => ({ id: Number(id), path: WORKFLOW, event: "workflow_dispatch", head_branch: "main",
    repository: { full_name: REPO, id: 3 }, head_repository: { id: 3 }, ...extra });
  const currentRun = () => runBody(runId, { run_attempt: gh.attempt, head_sha: gh.head, status: gh.status,
    run_started_at: stamp(-600000), updated_at: stamp(-60000) });
  const priorRun = () => runBody(priorId, { run_attempt: 1, head_sha: head0, status: "completed", conclusion: "success",
    run_started_at: stamp(-1200000), updated_at: stamp(-900000) });
  const job = (id, attempt, name, conclusion = "success", steps = []) => ({ name, status: "completed", conclusion, run_id: Number(id),
    run_attempt: attempt, started_at: stamp(-500000), completed_at: stamp(-60000), steps });
  const claimText = () => gh.claimText(fixture("apply-claim").toString()
    .replace('"apply_run_attempt": "2"', `"apply_run_attempt": "${gh.attempt}"`)
    .replace(/"workflow_head_sha": "b+"/, `"workflow_head_sha": "${gh.head}"`));
  const zips = () => ({
    9: storedZip("staging-migrate-apply-claim.json", new TextEncoder().encode(claimText())),
    10: storedZip("staging-migrate-manifest.json", new TextEncoder().encode(gh.planText(fixture("plan").toString()))),
    11: storedZip("staging-migrate-manifest.json", new TextEncoder().encode(gh.planText(fixture("plan").toString()))),
  });
  const artifact = async (id, name, owner, sha) => ({ id, name, expired: false, expires_at: stamp(600000),
    workflow_run: { id: Number(owner), head_sha: sha }, created_at: stamp(-100000), size_in_bytes: zips()[id].length,
    digest: `sha256:${await sha256(zips()[id])}` });
  const json = (value, status = 200) => new Response(JSON.stringify(value), { status });

  async function handle(url, init = {}) {
    init.signal?.throwIfAborted();
    const u = new URL(url), method = init.method ?? "GET", path = u.pathname;
    gh.calls.push({ method, host: u.host, path, headers: init.headers, body: init.body });
    const root = `/repos/${REPO}`;
    const hit = key => { gh.counts[key] = (gh.counts[key] ?? 0) + 1; gh.before(key, gh.counts[key]); };
    if (u.host.endsWith(".blob.core.windows.net")) {
      hit("blob");
      init.signal?.throwIfAborted();
      return gh.blobStatus === 200 ? new Response(zips()[Number(path.split("/").pop())]) : new Response("", { status: gh.blobStatus });
    }
    assert.equal(u.host, "api.github.com");
    if (method === "POST" && path === "/app/installations/2/access_tokens") {
      trace.push("github.token");
      await gh.pauseToken();
      if (gh.tokenStatus !== 201) return json({ message: "denied" }, gh.tokenStatus);
      gh.tokens++;
      return json({ token: `ghs_synthetic_installation_token_${gh.tokens}`,
        expires_at: new Date(Math.floor(Date.now() / 1000) * 1000 + 3600000).toISOString().replace(".000Z", "Z") }, 201);
    }
    if (method === "DELETE" && path === "/installation/token") {
      trace.push("github.revoke");
      gh.revoked.push(init.headers.Authorization);
      if (gh.revokeMode === "throw") throw new Error("synthetic revoke transport failure");
      return new Response(null, { status: gh.revokeMode === "ok" ? 204 : 500 });
    }
    if (method === "POST" && path === `${root}/actions/runs/${runId}/deployment_protection_rule`) {
      trace.push("github.decide");
      gh.inflight++; gh.maxInflight = Math.max(gh.maxInflight, gh.inflight);
      try {
        if (gh.callbackDelay) await new Promise(resolve => setTimeout(resolve, gh.callbackDelay));
        gh.decisions.push(JSON.parse(init.body));
        if (gh.callbackMode === "throw") throw new Error("synthetic ambiguous callback transport failure");
        return new Response(null, { status: gh.callbackMode === "ok" ? 204 : 500 });
      } finally { gh.inflight--; }
    }
    assert.equal(method, "GET", `unexpected ${method} ${path}`);
    if (path === `${root}/actions/runs/${runId}`) { hit("run"); return json(currentRun()); }
    if (path === `${root}/actions/runs/${priorId}`) return json(priorRun());
    if (path === `${root}/actions/runs/${runId}/pending_deployments`) {
      hit("pending"); return json(gh.pending.map(name => ({ environment: { name } })));
    }
    if (path === `${root}/actions/runs/${runId}/attempts/${gh.attempt}/jobs`) {
      const jobs = [job(runId, gh.attempt, "staging-migrate (claim)"), ...(gh.planJobs ? [job(runId, gh.attempt, "staging-migrate (plan)")] : [])];
      return json({ jobs, total_count: jobs.length });
    }
    if (path === `${root}/actions/runs/${priorId}/attempts/1/jobs`) {
      const jobs = [job(priorId, 1, "staging-migrate (plan)", "success", [{ name: "Confirm read-only plan dispatch", status: "completed", conclusion: "success" }]),
        job(priorId, 1, "staging-migrate (claim)", "skipped"), job(priorId, 1, "staging-migrate (apply)", "skipped")];
      return json({ jobs, total_count: jobs.length });
    }
    if (path === `${root}/actions/runs/${runId}/artifacts`) {
      const all = [await artifact(9, "staging-migrate-apply-claim", runId, gh.head), await artifact(10, "staging-migrate-manifest", runId, gh.head)];
      return json({ artifacts: all, total_count: all.length });
    }
    if (path === `${root}/actions/runs/${priorId}/artifacts`) {
      return json({ artifacts: [await artifact(11, "staging-migrate-manifest", priorId, head0)], total_count: 1 });
    }
    const zip = /^\/repos\/TogetherWeOwn\/two-bot-next\/actions\/artifacts\/(\d+)\/zip$/.exec(path);
    if (zip) return new Response(null, { status: 302, headers: { Location: `https://synthetic.blob.core.windows.net/artifact/${zip[1]}?sig=synthetic` } });
    if (path === `${root}/compare/main...${"a".repeat(40)}`) {
      assert.equal(u.search, "?per_page=1");
      return json({ merge_base_commit: { sha: "a".repeat(40) }, status: gh.compareStatus, ahead_by: 0 });
    }
    throw new Error(`unscripted GitHub request ${method} ${path}`);
  }
  t.mock.method(globalThis, "fetch", handle);

  const storage = memoryStorage(trace);
  const env = { PROTECTION_ENABLED: "1", GITHUB_APP_ID: "1", GITHUB_INSTALLATION_ID: "2", GITHUB_REPOSITORY_ID: "3",
    GITHUB_APP_PRIVATE_KEY: privateKeyPem, GITHUB_WEBHOOK_SECRET: webhookSecret,
    STAGING_HOST: "staging.invalid", STAGING_DATABASE: "two_bot", STAGING_BRANCH_ID: "" };
  const ctx = context(storage);
  const h = { gh, storage, env, ctx, logs, trace,
    gate: (c = ctx) => new StagingProtectionGate(c, env),
    // A new Durable Object instance with a new serialization context over the same persisted storage.
    reconstructed: () => new StagingProtectionGate(context(storage), env),
    audit: () => logs.filter(line => line.includes("staging_protection")).map(line => JSON.parse(line)) };
  return h;
}

async function deliver(gate, sha) {
  const response = await gate.evaluate(bodyBytes(sha));
  return { status: response.status, text: await response.text() };
}
// Everything a secret could reach: log sinks, storage contents, request URLs/bodies, thrown messages.
function assertNoSecretLeak(h, extraSinks = []) {
  const sinks = [...h.logs, JSON.stringify([...h.storage.map]), ...extraSinks,
    ...h.gh.calls.map(call => `${call.host}${call.path} ${typeof call.body === "string" ? call.body : ""}`)];
  const forbidden = [/ghs_synthetic_installation_token/, new RegExp(webhookSecret), new RegExp(der.toString("base64").slice(0, 40).replace(/[+/]/g, "."))];
  for (const sink of sinks) {
    for (const pattern of forbidden) assert.doesNotMatch(sink, pattern);
    assert.doesNotMatch(sink, /eyJ[A-Za-z0-9_-]{10,}/, "JWT-shaped value in a sink");
    assert.doesNotMatch(sink, /BEGIN PRIVATE KEY/);
  }
  // Installation token travels only in Authorization headers to api.github.com, never to the blob host.
  for (const call of h.gh.calls) {
    if (call.host.endsWith(".blob.core.windows.net")) assert.equal(call.headers, undefined);
  }
}
const decisionStates = h => h.gh.decisions.map(d => d.state);
const stored = (h, attempt = 2) => h.storage.map.get(`attempt:${attempt}`)?.state;

test("success: verified evidence approves once, fenced before the callback and revoked last", async t => {
  const h = harness(t);
  const result = await deliver(h.gate());
  assert.deepEqual(result, { status: 200, text: "approved" });
  assert.deepEqual(h.gh.decisions, [{ environment_name: "staging-migrate-apply", state: "approved", comment: "Independent staging binding verified." }]);
  assert.equal(stored(h), "approved");
  // Order: token, alarm, evaluating fence, ... approving fence, callback, final state, revoke.
  assert.deepEqual(h.trace.filter(x => x !== "github.token"), ["storage.setAlarm", "storage.put:evaluating", "storage.put:approving",
    "github.decide", "storage.put:approved", "github.revoke"]);
  assert.equal(h.trace[0], "github.token");
  assert.ok(h.storage.alarm > Date.now() + 24 * 3600 * 1000, "alarm outlives the maximum accepted run age");
  assert.deepEqual(h.audit(), [{ event: "staging_protection", run_id: runId, attempt: "2", outcome: "approved", token_revoked: true }]);
  assert.equal(h.gh.revoked.length, 1);
  assert.equal(h.gh.revoked[0], "Bearer ghs_synthetic_installation_token_1");
  assert.equal(h.gh.calls.at(-1).method, "DELETE");
  assertNoSecretLeak(h);
});

test("full chain through the worker route signs, routes to the run-owned gate and dedupes redelivery", async t => {
  const h = harness(t);
  h.env.PROTECTION_GATES = { idFromName: name => name, get: name => {
    assert.equal(name, runId);
    // Same run id => same persisted storage; a fresh instance each delivery models eviction between deliveries.
    return { fetch: request => h.reconstructed().fetch(request) };
  } };
  const body = bodyBytes();
  const key = await crypto.subtle.importKey("raw", new TextEncoder().encode(webhookSecret), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  const sig = "sha256=" + Buffer.from(await crypto.subtle.sign("HMAC", key, body)).toString("hex");
  const post = () => worker.fetch(new Request("https://protection.invalid/hooks/staging-migration", { method: "POST", body,
    headers: { "X-GitHub-Event": "deployment_protection_rule", "X-Hub-Signature-256": sig } }), h.env);
  assert.equal(await (await post()).text(), "accepted");
  assert.deepEqual(h.gh.calls, [], "acknowledgement performs no GitHub I/O");
  assert.ok(h.storage.map.has("queued")); assert.ok(h.storage.alarm);
  await h.reconstructed().alarm();
  assert.equal(stored(h), "approved");
  assert.equal(await (await post()).text(), "accepted");
  await h.reconstructed().alarm();
  assert.deepEqual(h.audit().map(a => a.outcome), ["approved", "duplicate"]);
  assert.deepEqual(decisionStates(h), ["approved"]);
  assertNoSecretLeak(h);
});

test("handoff storage failures return no acknowledgement and commit neither queue nor alarm", async t => {
  for (const operation of ["put", "setAlarm"]) {
    const h = harness(t);
    h.storage.failOn = op => op === operation ? "before" : undefined;
    assert.equal((await h.gate().fetch(gateRequest())).status, 503);
    assert.equal(h.storage.map.size, 0); assert.equal(h.storage.alarm, null);
    assert.deepEqual(h.gh.calls, []);
    h.storage.failOn = null;
    assert.equal((await h.reconstructed().fetch(gateRequest())).status, 202);
    await h.reconstructed().alarm();
    assert.deepEqual(decisionStates(h), ["approved"]);
  }
});

test("alarm evaluation does not hold the ingress lock; an interleaved delivery keeps its wake", async t => {
  const h = harness(t), gate = h.gate();
  await gate.fetch(gateRequest());
  const entered = Promise.withResolvers(), resume = Promise.withResolvers();
  h.gh.pauseToken = async () => { entered.resolve(); await resume.promise; };
  const running = gate.alarm();
  await entered.promise;
  try {
    assert.equal((await gate.fetch(gateRequest())).status, 202);
    assert.equal(h.gh.decisions.length, 0, "acknowledged while token request is unresolved");
  } finally { resume.resolve(); }
  await running;
  assert.equal(stored(h), "approved");
  assert.ok(h.storage.map.has("queued"), "new delivery is not deleted with the earlier snapshot");
  assert.ok(h.storage.alarm <= Date.now() + 1100, "new delivery retains a prompt durable wake");
  await h.reconstructed().alarm();
  assert.equal(h.storage.map.has("queued"), false);
  assert.deepEqual(decisionStates(h), ["approved"]);
  assertNoSecretLeak(h);
});

test("alarm restart before callback honors the attempt fence; disable before alarm never mints", async t => {
  const h = harness(t);
  await h.gate().fetch(gateRequest());
  h.storage.map.set("attempt:2", { state: "approving", at: Date.now() });
  await h.reconstructed().alarm();
  assert.deepEqual(decisionStates(h), []);
  assert.equal(stored(h), "approving");
  assert.equal(h.storage.map.has("queued"), false);
  const disabled = harness(t);
  await disabled.gate().fetch(gateRequest());
  disabled.env.PROTECTION_ENABLED = "0";
  await disabled.reconstructed().alarm();
  assert.deepEqual(disabled.gh.calls, []);
  assert.equal(disabled.storage.map.has("queued"), false);
});

test("real runner connection-usage floats in both plan archives do not alter binding", async t => {
  const h = harness(t);
  h.gh.planText = s => s.replace('"audit": {', '"audit": {"connection_usage":{"pct_full":12.5,"connections_used":5},');
  assert.deepEqual(await deliver(h.gate()), { status: 200, text: "approved" });
  assert.deepEqual(decisionStates(h), ["approved"]);
  assertNoSecretLeak(h);
});

for (const chain of ["ledger", "ledger_store"]) {
  for (const suffix of [".0", "e0"]) {
    test(`float syntax cannot mask changed ${chain} evidence (${suffix})`, async t => {
      const h = harness(t);
      h.gh.planText = s => s.replace(`"${chain}_before": []`, `"${chain}_before": [{"version":9007199254740992${suffix}}]`)
        .replace(`"${chain}_after": []`, `"${chain}_after": [{"version":9007199254740993${suffix}}]`);
      assert.deepEqual(await deliver(h.gate()), { status: 200, text: "rejected" });
      assert.deepEqual(decisionStates(h), ["rejected"]);
      assertNoSecretLeak(h);
    });
  }
}

test("verification timeout has a separate bounded rejection phase and never retries a callback", async t => {
  const h = harness(t), budgets = [], controllers = [];
  t.mock.method(AbortSignal, "timeout", ms => {
    budgets.push(ms);
    const controller = new AbortController();
    controllers.push(controller);
    return controller.signal;
  });
  h.gh.before = key => { if (key === "blob") controllers[0].abort(new DOMException("synthetic timeout", "TimeoutError")); };
  assert.deepEqual(await deliver(h.gate()), { status: 200, text: "rejected" });
  assert.deepEqual(budgets, [20000, 20000, 5000]);
  assert.equal(controllers[0].signal.aborted, true);
  assert.equal(controllers[1].signal.aborted, false);
  assert.equal(h.gh.tokens, 1);
  assert.deepEqual(decisionStates(h), ["rejected"]);
  assert.equal(stored(h), "rejected");
  assert.equal(h.gh.revoked.length, 1);
  assert.equal((await deliver(h.reconstructed())).text, "duplicate");
  assert.deepEqual(decisionStates(h), ["rejected"]);
  assertNoSecretLeak(h);
});

const rejections = [
  ["fractional syntax is not an integer schema version", gh => { gh.claimText = s => s.replace('"schema_version": 1', '"schema_version": 1.0'); }],
  ["fractional syntax is not a migration version", gh => { gh.planText = s => s.replace('"version": 9007199254740993', '"version": 9007199254740993.0'); }],
  ["claim names a different target", gh => { gh.claimText = s => s.replace('"host": "staging.invalid"', '"host": "other.invalid"'); }],
  ["source is not on main", gh => { gh.compareStatus = "ahead"; }],
  ["plan job is missing", gh => { gh.planJobs = false; }],
  ["artifact download fails", gh => { gh.blobStatus = 500; }],
  ["second pending gate", gh => { gh.pending = ["staging-migrate-apply", "staging-migrate-apply"]; }],
  ["pending gate is for another environment", gh => { gh.pending = ["production-migrate-apply"]; }],
  ["claim is for another attempt", gh => { gh.claimText = s => s.replace(/"apply_run_attempt": "\d+"/, '"apply_run_attempt": "9"'); }],
];
for (const [name, mutate] of rejections) {
  test(`verification rejection: ${name} sends one rejected callback and no approval`, async t => {
    const h = harness(t);
    mutate(h.gh);
    // Evidence refused during verifyBinding runs the real verifier; the gate still reaches the callback.
    const result = await deliver(h.gate());
    if (name === "second pending gate" || name === "pending gate is for another environment") {
      // api.pending() in the final re-read also fails, so no decision at all: fail closed without a callback.
      assert.deepEqual(result, { status: 503, text: "unavailable" });
      assert.deepEqual(h.gh.decisions, []);
      assert.equal(stored(h), "evaluating");
    } else {
      assert.deepEqual(result, { status: 200, text: "rejected" });
      assert.deepEqual(decisionStates(h), ["rejected"]);
      assert.equal(h.gh.decisions[0].comment, "Independent staging evidence refused.");
      assert.equal(stored(h), "rejected");
    }
    assert.equal(h.gh.revoked.length, 1);
    assertNoSecretLeak(h);
    // Redelivery never flips a settled or fenced decision.
    assert.equal((await deliver(h.reconstructed())).text, "duplicate");
    assert.ok(!decisionStates(h).includes("approved"));
  });
}

test("duplicate delivery after approval is a no-op with its own token revoked", async t => {
  const h = harness(t);
  await deliver(h.gate());
  const again = await deliver(h.gate());
  assert.deepEqual(again, { status: 200, text: "duplicate" });
  assert.deepEqual(decisionStates(h), ["approved"]);
  assert.equal(h.gh.tokens, 2); assert.equal(h.gh.revoked.length, 2);
  assert.deepEqual(h.audit().map(a => a.outcome), ["approved", "duplicate"]);
  assert.equal(h.trace.filter(x => x === "storage.setAlarm").length, 1, "duplicate does not re-arm the alarm");
});

test("concurrent deliveries on one instance serialize; exactly one callback", async t => {
  const h = harness(t);
  h.gh.callbackDelay = 20;
  const gate = h.gate(), other = h.gate(); // two object instances sharing one context, as concurrent requests do
  const results = await Promise.all([deliver(gate), deliver(other), deliver(gate)]);
  assert.deepEqual(results.map(r => r.text).sort(), ["approved", "duplicate", "duplicate"]);
  assert.deepEqual(decisionStates(h), ["approved"]);
  assert.equal(h.gh.maxInflight, 1);
  assert.equal(h.gh.tokens, 3); assert.equal(h.gh.revoked.length, 3);
  assertNoSecretLeak(h);
});

test("reconstructed gate over the same storage honors every persisted fence state", async t => {
  for (const state of ["evaluating", "approving", "rejecting", "approved", "rejected"]) {
    const h = harness(t);
    h.storage.map.set("attempt:2", { state, at: 1 });
    const result = await deliver(h.reconstructed());
    assert.deepEqual(result, { status: 200, text: "duplicate" }, state);
    assert.deepEqual(h.gh.decisions, [], state);
  }
});

test("alarm expiry clears the fence but a stale delivery cannot approve again", async t => {
  const h = harness(t);
  await deliver(h.gate());
  t.mock.method(Date, "now", () => h.gh.base + 25 * 3600 * 1000);
  await h.gate().alarm();
  assert.equal(h.storage.map.size, 0); assert.equal(h.storage.alarm, null);
  assert.deepEqual(await deliver(h.reconstructed()), { status: 503, text: "unavailable" });
  assert.deepEqual(decisionStates(h), ["approved"]);
  assert.equal(h.storage.map.size, 0, "expired run cannot recreate an approval fence");
  assert.equal(h.gh.tokens, 2, "stale workflow evidence is checked after a fresh call-time token");
  assert.equal(h.gh.revoked.length, 2, "both tokens are revoked, including the stale-run refusal");
  assert.equal(h.gh.counts.run, 5, "refusal reaches the current-run freshness check");
});

test("ambiguous callback transport failure is never repeated, only fenced and revoked", async t => {
  const h = harness(t);
  h.gh.callbackMode = "throw";
  assert.deepEqual(await deliver(h.gate()), { status: 503, text: "unavailable" });
  assert.equal(stored(h), "approving");
  assert.equal(h.gh.decisions.length, 1);
  assert.equal(h.gh.revoked.length, 1);
  assert.deepEqual(h.audit(), [{ event: "staging_protection", run_id: runId, attempt: "2", outcome: "unavailable", token_revoked: true }]);
  h.gh.callbackMode = "ok";
  assert.equal((await deliver(h.reconstructed())).text, "duplicate");
  assert.equal(h.gh.decisions.length, 1, "no automatic second callback");
  assertNoSecretLeak(h);
});

test("callback HTTP 500 after an approval fence behaves like an ambiguous callback", async t => {
  const h = harness(t);
  h.gh.callbackMode = "500";
  assert.equal((await deliver(h.gate())).status, 503);
  assert.equal(stored(h), "approving");
  assert.equal((await deliver(h.gate())).text, "duplicate");
  assert.equal(h.gh.decisions.length, 1);
});

test("ambiguous callback on the rejection path keeps the rejecting fence", async t => {
  const h = harness(t);
  h.gh.compareStatus = "ahead"; h.gh.callbackMode = "throw";
  assert.equal((await deliver(h.gate())).status, 503);
  assert.equal(stored(h), "rejecting");
  assert.deepEqual(decisionStates(h), ["rejected"]);
  assert.equal((await deliver(h.reconstructed())).text, "duplicate");
  assert.equal(h.gh.decisions.length, 1);
});

test("storage failure before the evaluating fence sends no callback and a retry succeeds", async t => {
  for (const [label, rig] of [
    ["get", s => { s.failOn = op => op === "get"; }],
    ["setAlarm", s => { s.failOn = op => op === "setAlarm"; }],
    ["put evaluating", s => { s.failOn = (op, key, value) => op === "put" && value.state === "evaluating" ? "before" : undefined; }],
  ]) {
    const h = harness(t);
    rig(h.storage);
    assert.equal((await deliver(h.gate())).status, 503, label);
    assert.deepEqual(h.gh.decisions, [], label);
    assert.equal(h.storage.map.size, 0, label);
    assert.equal(h.gh.revoked.length, 1, label);
    h.storage.failOn = null;
    assert.equal((await deliver(h.reconstructed())).text, "approved", label);
    assert.deepEqual(decisionStates(h), ["approved"], label);
  }
});

test("storage failure at the approving fence (before callback) never calls back, never approves on retry", async t => {
  for (const mode of ["before", "after"]) {
    const h = harness(t);
    h.storage.failOn = (op, key, value) => op === "put" && value.state === "approving" ? mode : undefined;
    assert.equal((await deliver(h.gate())).status, 503, mode);
    assert.deepEqual(h.gh.decisions, [], mode);
    // Whether or not the approving write landed, the earlier evaluating fence persists.
    assert.equal(stored(h), mode === "before" ? "evaluating" : "approving", mode);
    h.storage.failOn = null;
    assert.equal((await deliver(h.reconstructed())).text, "duplicate", mode);
    assert.deepEqual(h.gh.decisions, [], mode);
    assert.equal(h.gh.revoked.length, 2, mode);
  }
});

test("storage failure after the callback leaves the fence and never repeats the callback", async t => {
  for (const mode of ["before", "after"]) {
    const h = harness(t);
    h.storage.failOn = (op, key, value) => op === "put" && value.state === "approved" ? mode : undefined;
    assert.deepEqual(await deliver(h.gate()), { status: 503, text: "unavailable" }, mode);
    assert.deepEqual(decisionStates(h), ["approved"], mode);
    assert.equal(stored(h), mode === "before" ? "approving" : "approved", mode);
    h.storage.failOn = null;
    assert.equal((await deliver(h.reconstructed())).text, "duplicate", mode);
    assert.deepEqual(decisionStates(h), ["approved"], mode);
    assert.equal(h.gh.revoked.length, 2, mode);
    assertNoSecretLeak(h);
  }
});

test("storage failure after a rejection callback also never repeats it", async t => {
  const h = harness(t);
  h.gh.compareStatus = "ahead";
  h.storage.failOn = (op, key, value) => op === "put" && value.state === "rejected" ? "before" : undefined;
  assert.equal((await deliver(h.gate())).status, 503);
  assert.equal(stored(h), "rejecting");
  h.storage.failOn = null;
  assert.equal((await deliver(h.reconstructed())).text, "duplicate");
  assert.deepEqual(decisionStates(h), ["rejected"]);
});

test("rerun attempt change between verification and the final read: no callback, fence stays, new attempt is independent", async t => {
  const h = harness(t);
  // Reads of the current run: 1 gate, 2 verifyBinding, 3 final. Rerun lands after verification.
  h.gh.before = (key, n) => { if (key === "run" && n === 3) h.gh.attempt = 3; };
  assert.deepEqual(await deliver(h.gate()), { status: 503, text: "unavailable" });
  assert.deepEqual(h.gh.decisions, []);
  assert.equal(stored(h, 2), "evaluating");
  assert.equal(stored(h, 3), undefined);
  assert.equal(h.gh.revoked.length, 1);
  h.gh.before = () => {};
  // The attempt-3 delivery has its own fence and verifies against attempt-3 evidence.
  assert.equal((await deliver(h.reconstructed())).text, "approved");
  assert.equal(stored(h, 3), "approved");
  assert.equal(stored(h, 2), "evaluating");
  assert.equal(h.gh.decisions.length, 1);
  assert.equal((await deliver(h.reconstructed())).text, "duplicate");
});

test("rerun attempt change at the post-verification pending check: no callback on the old attempt", async t => {
  const h = harness(t);
  // Post-final-read interleaving: attempt 3's gate lands at pending call #2,
  // after verification. Pending alone cannot bind the attempt, so the
  // reordered final run read must observe the new attempt and refuse.
  h.gh.before = (key, n) => { if (key === "pending" && n === 2) h.gh.attempt = 3; };
  assert.deepEqual(await deliver(h.gate()), { status: 503, text: "unavailable" });
  assert.deepEqual(h.gh.decisions, []);
  assert.equal(stored(h, 2), "evaluating");
  assert.equal(stored(h, 3), undefined);
  assert.equal(h.gh.revoked.length, 1);
  h.gh.before = () => {};
  // The attempt-3 delivery has its own fence and verifies independently.
  assert.equal((await deliver(h.reconstructed())).text, "approved");
  assert.equal(stored(h, 3), "approved");
  assert.equal(h.gh.decisions.length, 1);
});

test("rerun attempt change at the last pre-callback re-read: no callback", async t => {
  const h = harness(t);
  // Reads of the current run: 1 gate, 2 verifyBinding, 3 final, 4 last
  // pre-callback. Attempt 3 lands just before the final re-read.
  h.gh.before = (key, n) => { if (key === "run" && n === 4) h.gh.attempt = 3; };
  assert.deepEqual(await deliver(h.gate()), { status: 503, text: "unavailable" });
  assert.deepEqual(h.gh.decisions, []);
  assert.equal(stored(h, 2), "approving", "write-ahead fence persists; no callback was sent");
  assert.equal(h.gh.revoked.length, 1);
  h.gh.before = () => {};
  assert.equal((await deliver(h.reconstructed())).text, "approved");
  assert.equal(stored(h, 3), "approved");
  assert.equal(h.gh.decisions.length, 1);
});

test("a rerun that finished attempt 2 does not let attempt 3 inherit its approval", async t => {
  const h = harness(t);
  assert.equal((await deliver(h.gate())).text, "approved");
  h.gh.attempt = 3;
  assert.equal((await deliver(h.reconstructed())).text, "approved");
  assert.equal(stored(h, 2), "approved"); assert.equal(stored(h, 3), "approved");
  assert.equal(h.gh.decisions.length, 2);
  // Stale attempt-2 claim served for attempt 3 is rejected, not approved.
  const g = harness(t);
  g.gh.attempt = 3;
  g.gh.claimText = s => s.replace('"apply_run_attempt": "3"', '"apply_run_attempt": "2"');
  assert.equal((await deliver(g.gate())).text, "rejected");
});

test("head change between verification and the final read: no callback", async t => {
  const h = harness(t);
  h.gh.before = (key, n) => { if (key === "run" && n === 3) h.gh.head = "c".repeat(40); };
  assert.deepEqual(await deliver(h.gate()), { status: 503, text: "unavailable" });
  assert.deepEqual(h.gh.decisions, []);
  assert.equal(stored(h), "evaluating");
  // The stale webhook fails the initial head check before fence lookup.
  assert.deepEqual(await deliver(h.reconstructed()), { status: 503, text: "unavailable" });
  assert.deepEqual(h.gh.decisions, []);
});

test("head change before verification yields no callback at all, not even a rejection", async t => {
  const h = harness(t);
  h.gh.before = (key, n) => { if (key === "run" && n === 2) h.gh.head = "c".repeat(40); };
  assert.equal((await deliver(h.gate())).status, 503);
  assert.deepEqual(h.gh.decisions, []);
});

test("webhook head that differs from the live run head is refused before any fence", async t => {
  const h = harness(t);
  h.gh.head = "c".repeat(40);
  assert.equal((await deliver(h.gate())).status, 503);
  assert.equal(h.storage.map.size, 0);
  assert.equal(h.trace.includes("storage.setAlarm"), false);
  assert.deepEqual(h.gh.decisions, []);
  assert.equal(h.gh.revoked.length, 1);
});

test("gate transition between verification and callback (pending gone or run no longer waiting) sends nothing", async t => {
  for (const mutate of [gh => { gh.pending = []; }, gh => { gh.status = "completed"; }]) {
    const h = harness(t);
    h.gh.before = (key, n) => {
      if ((key === "pending" && n === 2) || (key === "run" && n === 3)) mutate(h.gh);
    };
    assert.equal((await deliver(h.gate())).status, 503);
    assert.deepEqual(h.gh.decisions, []);
    assert.equal(stored(h), "evaluating");
  }
});

test("run that is not waiting at first read is refused without a fence", async t => {
  const h = harness(t);
  h.gh.status = "completed";
  assert.equal((await deliver(h.gate())).status, 503);
  assert.equal(h.storage.map.size, 0);
  assert.deepEqual(h.gh.decisions, []);
});

test("revocation: failure is audited but does not change the decision; teardown uses the issued token", async t => {
  for (const mode of ["throw", "500"]) {
    const h = harness(t);
    h.gh.revokeMode = mode;
    assert.deepEqual(await deliver(h.gate()), { status: 200, text: "approved" });
    assert.deepEqual(h.audit(), [{ event: "staging_protection", run_id: runId, attempt: "2", outcome: "approved", token_revoked: false }]);
    assert.equal(h.gh.revoked[0], "Bearer ghs_synthetic_installation_token_1");
    assertNoSecretLeak(h);
  }
});

test("revocation happens on every path that holds a token, and not when no token was issued", async t => {
  const rejected = harness(t); rejected.gh.compareStatus = "ahead";
  await deliver(rejected.gate());
  assert.equal(rejected.gh.revoked.length, 1);
  const ambiguous = harness(t); ambiguous.gh.callbackMode = "throw";
  await deliver(ambiguous.gate());
  assert.equal(ambiguous.gh.revoked.length, 1);
  const denied = harness(t); denied.gh.tokenStatus = 500;
  assert.deepEqual(await deliver(denied.gate()), { status: 503, text: "unavailable" });
  assert.equal(denied.gh.revoked.length, 0);
  assert.equal(denied.storage.map.size, 0);
  assert.deepEqual(denied.gh.decisions, []);
  assertNoSecretLeak(denied);
});

test("invalid App key rejects before any network call and leaks nothing", async t => {
  const h = harness(t);
  h.env.GITHUB_APP_PRIVATE_KEY = "synthetic-invalid-key";
  const failure = await deliver(h.gate());
  assert.deepEqual(failure, { status: 503, text: "unavailable" });
  assert.deepEqual(h.gh.calls, []);
  assert.equal(h.storage.map.size, 0);
  assertNoSecretLeak(h, [JSON.stringify(failure)]);
});

test("audit sink holds only fixed non-secret fields across mixed outcomes", async t => {
  const h = harness(t);
  await deliver(h.gate()); await deliver(h.gate());
  h.gh.callbackMode = "throw"; h.gh.attempt = 3;
  await deliver(h.gate());
  for (const entry of h.audit()) {
    assert.deepEqual(Object.keys(entry).sort(), ["attempt", "event", "outcome", "run_id", "token_revoked"]);
  }
  assert.equal(h.logs.length, 3);
  assertNoSecretLeak(h);
});
