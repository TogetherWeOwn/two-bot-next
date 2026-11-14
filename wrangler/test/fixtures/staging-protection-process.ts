// Child-process fixture for disk-backed workerd crash recovery. No real GitHub egress.
// IPC exposes only synthetic requests/state; the production verifier and alarm are unmodified.
import { createHmac, generateKeyPairSync } from "node:crypto";
import { build } from "esbuild";
import { Miniflare, convertV4MiniflareOptions } from "miniflare";

const [persist, mode] = process.argv.slice(2);
if (!persist || !mode || !process.send) throw new Error("crash fixture requires parent IPC and owned storage");
const send = (message: object) => process.send!(message);
const secret = "synthetic-webhook-secret-for-fixtures-only";
const runId = "37110947191", repo = "TogetherWeOwn/two-bot-next";
const root = `/repos/${repo}/actions/runs/${runId}`;
const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
const bundle = await build({
  stdin: { contents: `
import worker, { StagingProtectionGate as Base } from "./src/staging-protection/worker.ts";
export class StagingProtectionGate extends Base {
  async fetch(request) {
    const path = new URL(request.url).pathname;
    if (path === "/__defer") { await this.ctx.storage.put("test:defer", true); return new Response("ok"); }
    if (path === "/__state") return Response.json({
      queued: await this.ctx.storage.get("queued"), fence: await this.ctx.storage.get("attempt:2"),
      alarm: await this.ctx.storage.getAlarm()
    });
    const response = await super.fetch(request);
    if (response.status === 202 && await this.ctx.storage.get("test:defer")) {
      // Only the initial enqueue is delayed to make killing after ack deterministic.
      // Restart never calls setAlarm: the original persisted alarm must wake a cold object.
      await this.ctx.storage.setAlarm(Date.now() + 10000);
    }
    return response;
  }
}
export default worker;`, resolveDir: new URL("../..", import.meta.url).pathname, loader: "ts" },
  bundle: true, write: false, format: "esm", platform: "neutral", target: "esnext",
  external: ["cloudflare:workers"],
});
const json = (body: object, status = 200) => new Response(JSON.stringify(body), { status });
const mf = new Miniflare(convertV4MiniflareOptions({
  modules: true, script: bundle.outputFiles[0]!.text, compatibilityDate: "2026-09-29",
  resourcePersistencePath: persist,
  durableObjects: { PROTECTION_GATES: { className: "StagingProtectionGate", useSQLite: true } },
  bindings: { PROTECTION_ENABLED: "1", GITHUB_APP_ID: "1", GITHUB_INSTALLATION_ID: "2",
    GITHUB_REPOSITORY_ID: "3", GITHUB_APP_PRIVATE_KEY: privateKey.export({ type: "pkcs8", format: "pem" }).toString(),
    GITHUB_WEBHOOK_SECRET: secret, STAGING_HOST: "staging.invalid", STAGING_DATABASE: "synthetic", STAGING_BRANCH_ID: "br-synthetic" },
  outboundService: async (request: Request) => {
    const url = new URL(request.url), path = url.pathname, method = request.method;
    if (url.origin !== "https://api.github.com") throw new Error("fixture refused unexpected origin");
    send({ event: "outbound", method, path });
    if (method === "POST" && path === "/app/installations/2/access_tokens") return json({
      token: "synthetic-process-fixture-token", expires_at: new Date(Math.floor(Date.now() / 1000) * 1000 + 3600000).toISOString().replace(".000Z", "Z"),
    }, 201);
    if (method === "GET" && path === root) return json({ id: Number(runId), run_attempt: 2,
      path: ".github/workflows/staging-migrate.yml", event: "workflow_dispatch", head_branch: "main", head_sha: "b".repeat(40),
      repository: { full_name: repo, id: 3 }, head_repository: { id: 3 }, status: "waiting",
      run_started_at: new Date(Date.now() - 60000).toISOString().replace(/\.\d{3}Z$/, "Z") });
    if (method === "GET" && path === `${root}/pending_deployments`) return json([{ environment: { name: "staging-migrate-apply" } }]);
    // Missing producers deliberately take the real verifier's fail-closed rejection path.
    if (method === "GET" && path === `${root}/attempts/2/jobs`) return json({ jobs: [], total_count: 0 });
    if (method === "POST" && path === `${root}/deployment_protection_rule`) {
      send({ event: "decision", body: await request.json() });
      if (mode === "hold-callback") await new Promise<void>(() => {});
      return new Response(null, { status: 204 });
    }
    if (method === "DELETE" && path === "/installation/token") return new Response(null, { status: 204 });
    throw new Error("fixture refused unscripted request");
  },
}));
const gateCall = async (path: string) => {
  const gates = await mf.getDurableObjectNamespace("PROTECTION_GATES");
  return gates.getByName(runId).fetch(`https://gate.invalid${path}`, { method: "POST" });
};
process.on("message", async (message: { id: number; action: string }) => {
  try {
    let value: unknown;
    if (message.action === "defer") value = (await gateCall("/__defer")).status;
    else if (message.action === "state") value = await (await gateCall("/__state")).json();
    else if (message.action === "enqueue") {
      const body = JSON.stringify({ action: "requested", environment: "staging-migrate-apply", event: "workflow_dispatch",
        ref: "refs/heads/main", sha: "b".repeat(40), repository: { full_name: repo, id: 3 }, installation: { id: 2 },
        deployment_callback_url: `https://api.github.com${root}/deployment_protection_rule` });
      const response = await mf.dispatchFetch("https://worker.invalid/hooks/staging-migration", { method: "POST", body,
        headers: { "X-GitHub-Event": "deployment_protection_rule",
          "X-Hub-Signature-256": "sha256=" + createHmac("sha256", secret).update(body).digest("hex") } });
      value = { status: response.status, body: await response.text() };
    } else if (message.action === "dispose") {
      await mf.dispose(); send({ id: message.id, value: "disposed" }); process.disconnect(); return;
    } else throw new Error("unknown fixture command");
    send({ id: message.id, value });
  } catch { send({ id: message.id, error: "synthetic process fixture failed" }); }
});
await mf.ready;
send({ event: "ready" });
