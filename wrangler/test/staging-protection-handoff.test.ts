// Real workerd (Miniflare) acceptance for the staging protection Worker's durable enqueue/alarm handoff.
// The production worker.ts is bundled unmodified. The only addition is a test subclass of the Durable Object
// (test-only `/__*` routes, never reachable through the Worker's ingress path) that can defer or fire the alarm
// and list persisted keys, so no test depends on racing the one-second production alarm.
// Everything is synthetic: an ephemeral RSA key, a fixed webhook secret and an in-process outboundService
// that is the Worker's only egress (no real network, credential or GitHub endpoint).
// Limits: memory-persisted Durable Object storage surviving Miniflare setOptions() models a Worker/DO reload,
// not a process crash or disk recovery.
import { test } from "node:test";
import assert from "node:assert/strict";
import { createHmac, generateKeyPairSync } from "node:crypto";
import { build } from "esbuild";
import { Miniflare, convertV4MiniflareOptions } from "miniflare";

const SECRET = "synthetic-webhook-secret-for-fixtures-only";
const RUN_ID = "37110947191";
const REPO = "TogetherWeOwn/two-bot-next";
const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
const PEM = privateKey.export({ type: "pkcs8", format: "pem" }).toString();

const entry = `
import worker, { StagingProtectionGate as Base } from "./src/staging-protection/worker.ts";
export class StagingProtectionGate extends Base {
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === "/__defer") { await this.ctx.storage.put("test:defer", 1); return new Response("ok"); }
    if (path === "/__fire") { await this.ctx.storage.setAlarm(Date.now()); return new Response("ok"); }
    if (path === "/__state") {
      const keys = [...(await this.ctx.storage.list()).keys()];
      const queued = await this.ctx.storage.get("queued");
      return Response.json({ keys, delivery: queued?.delivery ?? null, bytes: queued?.body?.length ?? null });
    }
    const response = await super.fetch(request);
    if (response.status === 202 && await this.ctx.storage.get("test:defer")) {
      await this.ctx.storage.setAlarm(Date.now() + 3600000);
    }
    return response;
  }
}
export default worker;`;
const bundle = await build({
  stdin: { contents: entry, resolveDir: new URL("..", import.meta.url).pathname, sourcefile: "entry.ts", loader: "ts" },
  bundle: true, write: false, format: "esm", platform: "neutral", target: "esnext",
  external: ["cloudflare:workers"],
});
const code = bundle.outputFiles[0]!.text;

const within = async <T>(promise: Promise<T>, ms: number, what: string): Promise<T> => {
  let timer: NodeJS.Timeout | undefined;
  try {
    return await Promise.race([promise, new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${what} exceeded ${ms}ms`)), ms);
    })]);
  } finally { clearTimeout(timer); }
};
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
async function until(check: () => boolean | Promise<boolean>, ms: number, what: string) {
  const end = Date.now() + ms;
  while (Date.now() < end) { if (await check()) return; await sleep(100); }
  throw new Error(`timed out waiting for ${what}`);
}

const webhook = (sha = "b".repeat(40)) => JSON.stringify({
  action: "requested", environment: "staging-migrate-apply", event: "workflow_dispatch",
  ref: "refs/heads/main", sha, repository: { full_name: REPO, id: 3 }, installation: { id: 2 },
  deployment_callback_url: `https://api.github.com/repos/${REPO}/actions/runs/${RUN_ID}/deployment_protection_rule`,
});
const signed = (body: string) => ({
  method: "POST", body,
  headers: { "X-GitHub-Event": "deployment_protection_rule",
    "X-Hub-Signature-256": "sha256=" + createHmac("sha256", SECRET).update(body).digest("hex") },
});

function harness(t: { after: (fn: () => Promise<void>) => void }) {
  const seen: string[] = [];
  let release!: () => void;
  let hold: Promise<void> | undefined = new Promise<void>((resolve) => { release = resolve; });
  const options = (enabled: boolean, suffix = "") => convertV4MiniflareOptions({
    modules: true,
    script: code + suffix,
    compatibilityDate: "2026-09-29",
    durableObjects: { PROTECTION_GATES: { className: "StagingProtectionGate", useSQLite: true } },
    bindings: {
      PROTECTION_ENABLED: enabled ? "1" : "0", GITHUB_APP_ID: "1", GITHUB_INSTALLATION_ID: "2",
      GITHUB_REPOSITORY_ID: "3", GITHUB_APP_PRIVATE_KEY: PEM, GITHUB_WEBHOOK_SECRET: SECRET,
      STAGING_HOST: "staging.invalid", STAGING_DATABASE: "synthetic", STAGING_BRANCH_ID: "br-synthetic",
    },
    // The Worker's only egress. Every outbound GitHub call is recorded; none leaves the process.
    outboundService: async (request: Request) => {
      seen.push(`${request.method} ${new URL(request.url).pathname}`);
      if (hold) await hold;
      return new Response("{}", { status: 500 });
    },
  });
  const mf = new Miniflare(options(true));
  t.after(() => { hold = undefined; release(); return mf.dispose(); });
  // setOptions invalidates Node-side stubs; always reacquire.
  const gate = async () => (await mf.getDurableObjectNamespace("PROTECTION_GATES")).getByName(RUN_ID);
  const gateCall = (path: string) => gate().then((g) => g.fetch(`https://gate.invalid${path}`, { method: "POST" }));
  const state = async () => (await gateCall("/__state")).json() as Promise<{ keys: string[]; delivery: string | null; bytes: number | null }>;
  const ingress = (body = webhook()) => mf.dispatchFetch("https://worker.invalid/hooks/staging-migration", signed(body));
  return { mf, seen, options, gateCall, state, ingress,
    release: () => { hold = undefined; release(); } };
}

test("workerd: ingress acks 202 promptly while delayed GitHub I/O runs in the alarm, even for concurrent deliveries", async (t) => {
  const h = harness(t);
  const first = await within(h.ingress(), 10_000, "first ingress");
  assert.equal(first.status, 202);
  assert.equal(await first.text(), "accepted");
  assert.deepEqual((await h.state()).keys.includes("queued"), true, "durable queue committed before the ack");

  // The one-second alarm now calls the mocked GitHub token endpoint, which is held.
  await until(() => h.seen.length >= 1, 10_000, "alarm to reach outbound GitHub");
  assert.match(h.seen[0]!, /^POST \/app\/installations\/2\/access_tokens$/);
  const heldAt = Date.now();

  const burst = async (label: string) => {
    const started = Date.now();
    const results = await within(Promise.all([h.ingress(), h.ingress(), h.ingress()]), 10_000, label);
    assert.deepEqual(results.map((r) => r.status), [202, 202, 202]);
    assert.ok(Date.now() - started < 10_000);
  };
  await burst("concurrent ingress during delayed I/O");
  await sleep(Math.max(0, 10_500 - (Date.now() - heldAt)));
  await burst("concurrent ingress after the I/O was delayed beyond 10s");
  assert.ok(Date.now() - heldAt > 10_000, "outbound call was held for more than 10s");

  h.release();
  await until(async () => !(await h.state()).keys.includes("queued"), 30_000, "queue to drain after release");
});

test("workerd: disabling before the alarm refuses all outbound network and drains the queue", async (t) => {
  const h = harness(t);
  await h.gateCall("/__defer");
  assert.equal((await h.ingress()).status, 202);
  assert.ok((await h.state()).keys.includes("queued"));
  assert.deepEqual(h.seen, []);

  await h.mf.setOptions(h.options(false, "\n// disabled reload"));
  assert.equal((await h.ingress()).status, 503, "disabled ingress refuses");
  assert.ok((await h.state()).keys.includes("queued"), "persisted delivery survives the reload");

  await h.gateCall("/__fire");
  await until(async () => !(await h.state()).keys.includes("queued"), 15_000, "alarm to consume the disabled delivery");
  assert.deepEqual(h.seen, [], "no outbound request while disabled");
});

test("workerd: a persisted queued delivery survives a Miniflare reload and is processed by the alarm", async (t) => {
  const h = harness(t);
  h.release();
  await h.gateCall("/__defer");
  assert.equal((await h.ingress()).status, 202);
  const before = await h.state();
  assert.ok(before.keys.includes("queued"));
  assert.ok(before.bytes! > 0);
  assert.deepEqual(h.seen, []);

  // New isolate, same Durable Object storage. This is a reload, not a crash proof.
  await h.mf.setOptions(h.options(true, "\n// restart receipt"));
  const after = await h.state();
  assert.equal(after.delivery, before.delivery);
  assert.equal(after.bytes, before.bytes);
  assert.deepEqual(h.seen, [], "reload alone causes no network I/O");

  await h.gateCall("/__fire");
  await until(() => h.seen.length >= 1, 15_000, "restored delivery to reach outbound GitHub");
  assert.match(h.seen[0]!, /access_tokens$/);
  await until(async () => !(await h.state()).keys.includes("queued"), 15_000, "restored delivery to complete");
});
