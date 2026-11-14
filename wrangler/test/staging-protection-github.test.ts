import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { GitHub, verifyBinding, boundedBody, MAX_AGE_MS, appJwt } from "../src/staging-protection/github.ts";
import { parseJson, crc32 } from "../src/staging-protection/encoding.ts";
import { sha256 } from "../src/staging-protection/binding.ts";

const config = { appId: "1", installationId: "2", repositoryId: "3", privateKey: "unused" };
const now = Date.parse("2026-11-10T02:00:00Z");
const stamp = delta => new Date(now + delta).toISOString().replace(".000Z", "Z");
const fixture = name => readFileSync(new URL(`../../crates/cutover/tests/fixtures/staging-migrate-${name}.json`, import.meta.url));
const claim = () => parseJson(fixture("apply-claim"));
const plan = () => parseJson(fixture("plan"));
const pins = { host: "staging.invalid", database: "two_bot", branch_id: "" };
const runId = "37110947191", priorId = "37110947190";
const head = "b".repeat(40);

function evidence() {
  const run = { id: BigInt(runId), run_attempt: 2n, path: ".github/workflows/staging-migrate.yml",
    event: "workflow_dispatch", head_branch: "main", head_sha: head, repository: { full_name: "TogetherWeOwn/two-bot-next", id: 3n },
    head_repository: { id: 3n }, status: "waiting", run_started_at: stamp(-600000), updated_at: stamp(-60000) };
  const prior = { ...run, id: BigInt(priorId), run_attempt: 1n, status: "completed", conclusion: "success",
    run_started_at: stamp(-1200000), updated_at: stamp(-900000) };
  const job = name => ({ name, status: "completed", conclusion: "success", run_id: BigInt(runId), run_attempt: 2n,
    started_at: stamp(-500000), completed_at: stamp(-60000), steps: [] });
  const currentJobs = [job("staging-migrate (claim)"), job("staging-migrate (plan)")];
  const priorJobs = [job("staging-migrate (plan)"), ...["claim", "apply"].map(mode => ({ ...job(`staging-migrate (${mode})`), conclusion: "skipped" }))]
    .map(value => ({ ...value, run_id: BigInt(priorId), run_attempt: 1n }));
  priorJobs[0].steps.push({ name: "Confirm read-only plan dispatch", status: "completed", conclusion: "success" });
  const api = new GitHub("synthetic", AbortSignal.timeout(1000), async () => { throw Error("unexpected transport"); });
  const originals = { run: api.run.bind(api), jobs: api.jobs.bind(api), pending: api.pending.bind(api) };
  const values = { run, prior, currentJobs, priorJobs, claim: claim(), currentPlan: plan(), priorPlan: plan(),
    comparison: { merge_base_commit: { sha: "a".repeat(40) }, status: "behind", ahead_by: 0n }, pending: [{ environment: { name: "staging-migrate-apply" } }] };
  api.json = async path => {
    if (path.endsWith(`/runs/${runId}`)) return values.run;
    if (path.endsWith(`/runs/${priorId}`)) return values.prior;
    if (path.includes("pending_deployments")) return values.pending;
    if (path.includes("/jobs?")) {
      const jobs = path.includes(`/runs/${runId}/`) ? values.currentJobs : values.priorJobs;
      return { jobs, total_count: BigInt(jobs.length) };
    }
    if (path.includes("/compare/")) return values.comparison;
    throw Error("unknown endpoint");
  };
  api.artifact = async (run, sha, job, name) => name.endsWith("claim") ? values.claim : run === runId ? values.currentPlan : values.priorPlan;
  return { api, values, originals };
}

test("independent current/prior evidence accepts exact two-chain binding", async () => {
  const { api } = evidence();
  assert.deepEqual(await verifyBinding(api, config, pins, runId, now), { attempt: "2", head });
});

test("GitHub workflow path accepts a ref suffix but still requires main branch evidence", async () => {
  const { api, values } = evidence();
  values.run.path += "@main";
  values.prior.path += "@refs/heads/main";
  assert.deepEqual(await verifyBinding(api, config, pins, runId, now), { attempt: "2", head });
  values.run.head_branch = "feature";
  await assert.rejects(verifyBinding(api, config, pins, runId, now));
});

test("ancestry uses the empty reverse comparison, independent of main's forward diff size", async () => {
  const { api } = evidence();
  const json = api.json;
  let reads = 0;
  const transport = new GitHub("synthetic", AbortSignal.timeout(1000), async url => {
    reads++;
    assert.equal(url, `https://api.github.com/repos/TogetherWeOwn/two-bot-next/compare/main...${"a".repeat(40)}?per_page=1`);
    return new Response(JSON.stringify({ status: "behind", ahead_by: 0, merge_base_commit: { sha: "a".repeat(40) }, files: [], commits: [] }));
  });
  api.json = path => path.includes("/compare/") ? transport.json(path) : json(path);
  assert.deepEqual(await verifyBinding(api, config, pins, runId, now), { attempt: "2", head });
  assert.equal(reads, 1);
});

const refusals = [
  ["wrong repository", e => { e.run.repository.id = 4n; }],
  ["fork head", e => { e.run.head_repository.id = 4n; }],
  ["wrong workflow", e => { e.run.path = ".github/workflows/production-migrate.yml"; }],
  ["wrong event", e => { e.run.event = "push"; }],
  ["non-main", e => { e.run.head_branch = "feature"; }],
  ["stale apply run", e => { e.run.run_started_at = stamp(-MAX_AGE_MS - 1); }],
  ["completed apply run", e => { e.run.status = "completed"; }],
  ["missing pending gate", e => { e.pending = []; }],
  ["production gate only", e => { e.pending[0].environment.name = "production-migrate-apply"; }],
  ["ambiguous pending gate", e => { e.pending.push(e.pending[0]); }],
  ["missing claim job", e => { e.currentJobs.shift(); }],
  ["duplicate claim job", e => { e.currentJobs.push(e.currentJobs[0]); }],
  ["failed claim job", e => { e.currentJobs[0].conclusion = "failure"; }],
  ["job wrong attempt", e => { e.currentJobs[0].run_attempt = 1n; }],
  ["stale claim", e => { e.currentJobs[0].completed_at = stamp(-1800001); }],
  ["claim wrong attempt", e => { e.claim.apply_run_attempt = "1"; }],
  ["current plan differs", e => { e.currentPlan.acl_plan_ref = "other"; }],
  ["prior failed", e => { e.prior.conclusion = "failure"; }],
  ["prior not completed", e => { e.prior.status = "in_progress"; }],
  ["prior newer than apply", e => { e.prior.updated_at = stamp(-100); }],
  ["prior stale", e => { e.prior.run_started_at = stamp(-MAX_AGE_MS - 1); }],
  ["prior plan job failed", e => { e.priorJobs[0].conclusion = "failure"; }],
  ["prior no mode evidence", e => { e.priorJobs[0].steps = []; }],
  ["prior apply with plan-shaped artifact", e => { e.priorJobs[0].steps[0].conclusion = "skipped"; e.priorJobs[1].conclusion = "success"; }],
  ["prior apply job executed", e => { e.priorJobs[2].conclusion = "success"; }],
  ["prior binding differs", e => { e.priorPlan.source_sha = "c".repeat(40); }],
  ["source not on main", e => { e.comparison.merge_base_commit.sha = "d".repeat(40); }],
  ["source ahead of main", e => { e.comparison.status = "ahead"; }],
  ["source diverged from main", e => { e.comparison.status = "diverged"; }],
  ["source has ahead commits", e => { e.comparison.ahead_by = 1n; }],
  ["missing ahead count", e => { delete e.comparison.ahead_by; }],
];
for (const [name, mutate] of refusals) {
  test(`fresh evidence refuses ${name}`, async () => {
    const { api, values } = evidence();
    mutate(values);
    await assert.rejects(verifyBinding(api, config, pins, runId, now));
  });
}

test("API read failure and artifact absence fail closed", async () => {
  for (const method of ["json", "artifact"]) {
    const { api } = evidence();
    api[method] = async () => { throw Error("synthetic unavailable"); };
    await assert.rejects(verifyBinding(api, config, pins, runId, now));
  }
});

test("transport caps JSON, refuses API failures and pagination, and bounds calls", async () => {
  for (const reply of [new Response("{}", { status: 403 }), new Response("{}", { headers: { Link: "next" } }), new Response("x".repeat(1024 * 1024 + 1))]) {
    const api = new GitHub("synthetic", AbortSignal.timeout(1000), async () => reply);
    await assert.rejects(api.json("/synthetic"));
  }
  await assert.rejects(boundedBody(new Response("1234"), 3));
  const api = new GitHub("synthetic", AbortSignal.timeout(1000), async () => new Response("{}"));
  for (let i = 0; i < 24; i++) await api.json("/synthetic");
  await assert.rejects(api.json("/synthetic"));
  await assert.rejects(api.request("https://evil.invalid"));
});

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
async function artifactApi(mutate = () => {}, location = "https://synthetic.blob.core.windows.net/artifact?signed=synthetic") {
  const zip = storedZip("staging-migrate-apply-claim.json", fixture("apply-claim"));
  const artifact = { id: 9n, name: "staging-migrate-apply-claim", expired: false, expires_at: stamp(600000),
    workflow_run: { id: BigInt(runId), head_sha: head }, created_at: stamp(-100000), size_in_bytes: BigInt(zip.length), digest: `sha256:${await sha256(zip)}` };
  const listing = { artifacts: [artifact], total_count: 1n };
  mutate(artifact, listing);
  const calls = [];
  const api = new GitHub("synthetic-installation-token", AbortSignal.timeout(1000), async (url, init) => {
    calls.push({ url, init });
    if (url.startsWith("https://api.github.com/")) return new Response(null, { status: 302, headers: { Location: location } });
    return new Response(zip);
  });
  api.json = async () => listing;
  const job = { started_at: stamp(-200000), completed_at: stamp(-60000) };
  return { api, calls, job };
}
test("artifact digest, provenance and ZIP verified; redirect never gets credentials", async () => {
  const { api, calls, job } = await artifactApi();
  assert.deepEqual(await api.artifact(runId, head, job, "staging-migrate-apply-claim", now), claim());
  assert.equal(calls[0].init.headers.Authorization, "Bearer synthetic-installation-token");
  assert.equal(calls[1].init.headers, undefined);
  assert.equal(calls[1].init.redirect, "manual");
});
for (const [name, mutate] of [
  ["missing", (a, l) => { l.artifacts = []; l.total_count = 0n; }],
  ["ambiguous", (a, l) => { l.artifacts.push(a); l.total_count = 2n; }],
  ["expired", a => { a.expired = true; }],
  ["expiry passed", a => { a.expires_at = stamp(-1); }],
  ["wrong run", a => { a.workflow_run.id = 99n; }],
  ["wrong head", a => { a.workflow_run.head_sha = "c".repeat(40); }],
  ["old attempt artifact", a => { a.created_at = stamp(-300000); }],
  ["created after producer", a => { a.created_at = stamp(-1); }],
  ["digest missing", a => { delete a.digest; }],
  ["digest mismatch", a => { a.digest = "sha256:" + "0".repeat(64); }],
  ["oversize", a => { a.size_in_bytes = 2n * 1024n * 1024n; }],
]) test(`artifact refuses ${name}`, async () => {
  const { api, job } = await artifactApi(mutate);
  await assert.rejects(api.artifact(runId, head, job, "staging-migrate-apply-claim", now));
});
for (const url of ["https://evil.invalid/zip", "https://x.blob.core.windows.net.evil.invalid/zip", "http://x.blob.core.windows.net/zip", "https://user@x.blob.core.windows.net/zip"]) {
  test(`artifact refuses unauthorized redirect ${url}`, async () => {
    const { api, job, calls } = await artifactApi(undefined, url);
    await assert.rejects(api.artifact(runId, head, job, "staging-migrate-apply-claim", now));
    assert.equal(calls.length, 1);
  });
}

test("callback is fixed to staging and validates 204; token teardown uses a separate budget", async () => {
  const calls = [];
  const api = new GitHub("synthetic", AbortSignal.timeout(1000), async (url, init) => { calls.push({ url, init }); return new Response(null, { status: 204 }); });
  await api.decide(runId, true); await api.decide(runId, false); await api.revoke();
  assert.equal(JSON.parse(calls[0].init.body).environment_name, "staging-migrate-apply");
  assert.equal(JSON.parse(calls[0].init.body).state, "approved");
  assert.equal(JSON.parse(calls[1].init.body).state, "rejected");
  assert.equal(calls[2].url, "https://api.github.com/installation/token");
  const failed = new GitHub("synthetic", AbortSignal.timeout(1000), async () => new Response("private body", { status: 403 }));
  await assert.rejects(failed.decide(runId, true), /evidence_refused/);
});

test("App JWT has bounded lifetime and signature; malformed key fails without transport", async () => {
  const pair = await crypto.subtle.generateKey({ name: "RSASSA-PKCS1-v1_5", modulusLength: 2048, publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256" }, true, ["sign", "verify"]);
  const der = Buffer.from(await crypto.subtle.exportKey("pkcs8", pair.privateKey));
  const pem = `-----BEGIN PRIVATE KEY-----\n${der.toString("base64")}\n-----END PRIVATE KEY-----`;
  const jwt = await appJwt("1", pem, now), [header, body, signature] = jwt.split(".");
  const claims = JSON.parse(Buffer.from(body, "base64url").toString());
  assert.equal(claims.exp - claims.iat, 300);
  assert.equal(claims.iss, "1");
  assert.equal(await crypto.subtle.verify("RSASSA-PKCS1-v1_5", pair.publicKey, Buffer.from(signature, "base64url"), new TextEncoder().encode(header + "." + body)), true);
  await assert.rejects(appJwt("1", "synthetic-invalid-key", now));
});
