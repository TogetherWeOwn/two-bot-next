import test from "node:test";
import assert from "node:assert/strict";
import worker, { authentic, requestIdentity, appConfig } from "../src/staging-protection/worker.ts";
import { parseJson } from "../src/staging-protection/encoding.ts";

const secret = "synthetic-webhook-secret-for-fixtures-only";
const config = { appId: "1", installationId: "2", repositoryId: "3", privateKey: "unused" };
function event() {
  return { action: "requested", environment: "staging-migrate-apply", event: "workflow_dispatch", ref: "refs/heads/main",
    sha: "b".repeat(40), repository: { full_name: "TogetherWeOwn/two-bot-next", id: 3 }, installation: { id: 2 },
    deployment_callback_url: "https://api.github.com/repos/TogetherWeOwn/two-bot-next/actions/runs/37110947191/deployment_protection_rule" };
}
const bytes = value => new TextEncoder().encode(JSON.stringify(value));
async function signature(body, key = secret) {
  const hmac = await crypto.subtle.importKey("raw", new TextEncoder().encode(key), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  return "sha256=" + Buffer.from(await crypto.subtle.sign("HMAC", hmac, body)).toString("hex");
}
function env() {
  let calls = 0;
  return { PROTECTION_ENABLED: "1", GITHUB_APP_ID: "1", GITHUB_INSTALLATION_ID: "2", GITHUB_REPOSITORY_ID: "3",
    GITHUB_APP_PRIVATE_KEY: "unused", GITHUB_WEBHOOK_SECRET: secret,
    PROTECTION_GATES: { idFromName: id => id, get: id => ({ fetch: async request => {
      calls++; assert.equal(id, "37110947191"); assert.equal(new URL(request.url).pathname, "/enqueue");
      assert.deepEqual(parseJson(new Uint8Array(await request.arrayBuffer())), parseJson(bytes(event())));
      return new Response("accepted", { status: 202 });
    } }) }, calls: () => calls };
}
function request(body, sig) {
  return new Request("https://protection.invalid/hooks/staging-migration", { method: "POST", body,
    headers: { "X-GitHub-Event": "deployment_protection_rule", "X-Hub-Signature-256": sig } });
}
test("webhook signature authenticates raw bytes before routing to the run-owned gate", async () => {
  const body = bytes(event()), settings = env();
  assert.equal(await authentic(body, await signature(body), secret), true);
  const result = await worker.fetch(request(body, await signature(body)), settings);
  assert.equal(result.status, 202); assert.equal(settings.calls(), 1);
  assert.equal(await result.text(), "accepted");
  assert.equal(result.headers.get("content-type"), "text/plain;charset=UTF-8");
});
test("signature tampering, missing key/header and malformed body never reach the gate", async () => {
  const body = bytes(event());
  for (const sig of ["", "sha1=1234", "sha256=" + "0".repeat(64), await signature(new TextEncoder().encode("{}"))]) {
    const settings = env();
    assert.equal((await worker.fetch(request(body, sig), settings)).status, 400);
    assert.equal(settings.calls(), 0);
  }
  for (const [raw, key] of [[new TextEncoder().encode("{"), secret], [body, ""], [new Uint8Array(65537), secret]]) {
    const settings = env(); settings.GITHUB_WEBHOOK_SECRET = key;
    assert.equal((await worker.fetch(request(raw, await signature(raw)), settings)).status, 400);
    assert.equal(settings.calls(), 0);
  }
});
test("receiver is dark by default and exposes no other route or method", async () => {
  const settings = env(), body = bytes(event()), sig = await signature(body);
  settings.PROTECTION_ENABLED = "0";
  assert.equal((await worker.fetch(request(body, sig), settings)).status, 503);
  settings.PROTECTION_ENABLED = "1";
  assert.equal((await worker.fetch(new Request("https://protection.invalid/hooks/staging-migration"), settings)).status, 404);
  assert.equal((await worker.fetch(new Request("https://protection.invalid/other", { method: "POST", body }), settings)).status, 404);
  const wrong = request(body, sig); wrong.headers.set("X-GitHub-Event", "push");
  assert.equal((await worker.fetch(wrong, settings)).status, 400);
  assert.equal(settings.calls(), 0);
});
for (const [name, mutate] of [
  ["action", e => { e.action = "other"; }], ["production", e => { e.environment = "production-migrate-apply"; }],
  ["event", e => { e.event = "pull_request"; }], ["branch", e => { e.ref = "refs/heads/feature"; }],
  ["head", e => { e.sha = "b".repeat(39); }], ["repository", e => { e.repository.id = 4; }],
  ["repository name", e => { e.repository.full_name = "TogetherWeOwn/other"; }],
  ["installation", e => { e.installation.id = 4; }],
  ["callback authority", e => { e.deployment_callback_url = e.deployment_callback_url.replace("api.github.com", "evil.invalid"); }],
  ["callback query", e => { e.deployment_callback_url += "?override=1"; }],
  ["callback run", e => { e.deployment_callback_url = e.deployment_callback_url.replace("37110947191", "01"); }],
  ["callback repository", e => { e.deployment_callback_url = e.deployment_callback_url.replace("two-bot-next", "other"); }],
]) test(`authenticated out-of-scope ${name} is refused`, async () => {
  const value = event(); mutate(value);
  assert.throws(() => requestIdentity(parseJson(bytes(value)), config));
  const body = bytes(value), settings = env();
  assert.equal((await worker.fetch(request(body, await signature(body)), settings)).status, 400);
  assert.equal(settings.calls(), 0);
});
test("App configuration requires exact numeric principals", () => {
  assert.equal(appConfig(env()).installationId, "2");
  for (const field of ["GITHUB_APP_ID", "GITHUB_INSTALLATION_ID", "GITHUB_REPOSITORY_ID"]) {
    const settings = env(); settings[field] = "not-an-id";
    assert.throws(() => appConfig(settings));
  }
});
