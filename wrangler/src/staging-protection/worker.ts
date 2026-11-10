import { DurableObject } from "cloudflare:workers";
import { parseJson, type Json } from "./encoding.ts";
import { ENVIRONMENT, id, object, REPOSITORY, requireEvidence, target, text } from "./binding.ts";
import { boundedBody, installationClient, verifyBinding, type AppConfig, type GitHub } from "./github.ts";

export interface ProtectionEnv {
  PROTECTION_ENABLED: string;
  GITHUB_APP_ID: string;
  GITHUB_INSTALLATION_ID: string;
  GITHUB_REPOSITORY_ID: string;
  GITHUB_APP_PRIVATE_KEY: string;
  GITHUB_WEBHOOK_SECRET: string;
  STAGING_HOST: string;
  STAGING_DATABASE: string;
  STAGING_BRANCH_ID: string;
  PROTECTION_GATES: DurableObjectNamespace<StagingProtectionGate>;
}
export function appConfig(env: ProtectionEnv): AppConfig {
  for (const value of [env.GITHUB_APP_ID, env.GITHUB_INSTALLATION_ID, env.GITHUB_REPOSITORY_ID]) {
    requireEvidence(typeof value === "string" && /^[1-9][0-9]{0,19}$/.test(value));
  }
  requireEvidence(typeof env.GITHUB_APP_PRIVATE_KEY === "string" && env.GITHUB_APP_PRIVATE_KEY.length <= 16384);
  return { appId: env.GITHUB_APP_ID, installationId: env.GITHUB_INSTALLATION_ID,
    repositoryId: env.GITHUB_REPOSITORY_ID, privateKey: env.GITHUB_APP_PRIVATE_KEY };
}
export async function authentic(body: Uint8Array, signature: string, secret: string): Promise<boolean> {
  if (!/^sha256=[0-9a-f]{64}$/.test(signature) || typeof secret !== "string" || secret.length < 32 || secret.length > 256) return false;
  const key = await crypto.subtle.importKey("raw", new TextEncoder().encode(secret), { name: "HMAC", hash: "SHA-256" }, false, ["verify"]);
  const bytes = Uint8Array.from(signature.slice(7).match(/../g)!, value => parseInt(value, 16));
  return crypto.subtle.verify("HMAC", key, bytes, body);
}
export function requestIdentity(value: Json, config: AppConfig) {
  const event = object(value);
  requireEvidence(event.action === "requested" && event.environment === ENVIRONMENT && event.event === "workflow_dispatch"
    && event.ref === "refs/heads/main" && /^[0-9a-f]{40}$/.test(text(event.sha)));
  requireEvidence(object(event.repository).full_name === REPOSITORY && id(object(event.repository).id) === config.repositoryId
    && id(object(event.installation).id) === config.installationId);
  // Never follow a webhook-provided URL. Only this exact endpoint can select a run.
  const callback = text(event.deployment_callback_url);
  const match = /^https:\/\/api\.github\.com\/repos\/TogetherWeOwn\/two-bot-next\/actions\/runs\/([1-9][0-9]{0,19})\/deployment_protection_rule$/.exec(callback);
  requireEvidence(match);
  return { runId: match[1]!, sha: text(event.sha) };
}
function response(status: number, message: string): Response {
  return new Response(message, { status, headers: { "Content-Type": "text/plain", "Cache-Control": "no-store" } });
}
export default {
  async fetch(request: Request, env: ProtectionEnv): Promise<Response> {
    if (env.PROTECTION_ENABLED !== "1") return response(503, "unavailable");
    if (request.method !== "POST" || new URL(request.url).pathname !== "/hooks/staging-migration") return response(404, "not_found");
    try {
      requireEvidence(request.headers.get("X-GitHub-Event") === "deployment_protection_rule");
      const body = await boundedBody(new Response(request.body), 65536);
      requireEvidence(await authentic(body, request.headers.get("X-Hub-Signature-256") ?? "", env.GITHUB_WEBHOOK_SECRET));
      const identity = requestIdentity(parseJson(body), appConfig(env));
      const gate = env.PROTECTION_GATES.get(env.PROTECTION_GATES.idFromName(identity.runId));
      return await gate.fetch(new Request("https://gate/enqueue", { method: "POST", body }));
    } catch {
      // Neither parser/crypto exceptions nor remote error bodies escape to logs or callers.
      console.log(JSON.stringify({ event: "staging_protection", outcome: "ingress_refused" }));
      return response(400, "refused");
    }
  },
};

type QueuedDelivery = { delivery: string; body: Uint8Array };
const RETENTION_MS = 25 * 60 * 60 * 1000;
export class StagingProtectionGate extends DurableObject<ProtectionEnv> {
  async fetch(request: Request): Promise<Response> {
    try {
      requireEvidence(this.env.PROTECTION_ENABLED === "1" && request.method === "POST"
        && new URL(request.url).pathname === "/enqueue");
      const body = await boundedBody(new Response(request.body), 65536);
      requestIdentity(parseJson(body), appConfig(this.env));
      // Commit the bounded inbox and its wake atomically before acknowledging GitHub.
      // One replaceable delivery per run; concurrent redelivery never waits on GitHub I/O.
      await this.ctx.storage.transaction(async storage => {
        await storage.put("queued", { delivery: crypto.randomUUID(), body } satisfies QueuedDelivery);
        await storage.setAlarm(Date.now() + 1000);
      });
      return response(202, "accepted");
    } catch {
      console.log(JSON.stringify({ event: "staging_protection", outcome: "handoff_unavailable" }));
      return response(503, "unavailable");
    }
  }
  async evaluate(body: Uint8Array): Promise<Response> {
    const now = Date.now();
    let api: GitHub | undefined;
    let outcome = "unavailable", attempt = "", runId = "";
    try {
      requireEvidence(this.env.PROTECTION_ENABLED === "1");
      const config = appConfig(this.env);
      const identity = requestIdentity(parseJson(body), config);
      runId = identity.runId;
      const pins = target({ host: this.env.STAGING_HOST, database: this.env.STAGING_DATABASE,
        branch_id: this.env.STAGING_BRANCH_ID });
      api = await installationClient(config, now, AbortSignal.timeout(20000));
      const run = await api.run(identity.runId, config, now, false);
      requireEvidence(run.head_sha === identity.sha);
      attempt = id(run.run_attempt);
      const key = `attempt:${attempt}`;
      // Only the fence acquisition is serialized. Never hold a global input gate over
      // network I/O: that would also delay acknowledgement of concurrent redeliveries.
      const acquired = await this.ctx.storage.transaction(async storage => {
        if (await storage.get(key)) return false;
        if (!await storage.get("queued")) await storage.setAlarm(now + RETENTION_MS);
        await storage.put(key, { state: "evaluating", at: now });
        return true;
      });
      if (!acquired) {
        outcome = "duplicate";
        return response(200, outcome);
      }
      let approved = false;
      try {
        const verified = await verifyBinding(api, config, pins, identity.runId, now);
        requireEvidence(verified.attempt === attempt && verified.head === identity.sha);
        approved = true;
      } catch { /* Every missing, stale, mismatched or failed read rejects. */ }
      // Re-read immediately before either decision. A rerun/gate transition invalidates it.
      const latest = await api.run(identity.runId, config, Date.now(), false);
      requireEvidence(id(latest.run_attempt) === attempt && latest.head_sha === identity.sha);
      await api.pending(identity.runId);
      await this.ctx.storage.put(key, { state: approved ? "approving" : "rejecting", at: now });
      await api.decide(identity.runId, approved);
      outcome = approved ? "approved" : "rejected";
      await this.ctx.storage.put(key, { state: outcome, at: now });
      return response(200, outcome);
    } catch {
      return response(503, "unavailable");
    } finally {
      let revoked = false;
      if (api) {
        try { await api.revoke(); revoked = true; } catch { /* Expiry remains GitHub's one-hour upper bound. */ }
      }
      console.log(JSON.stringify({ event: "staging_protection", run_id: runId, attempt,
        outcome, token_revoked: revoked }));
    }
  }
  async alarm(): Promise<void> {
    const queued = await this.ctx.blockConcurrencyWhile(async () => {
      const pending = await this.ctx.storage.get<QueuedDelivery>("queued");
      if (!pending) await this.ctx.storage.deleteAll();
      else await this.ctx.storage.setAlarm(Date.now() + 60000);
      return pending;
    });
    if (!queued) return;
    await this.evaluate(queued.body);
    await this.ctx.storage.transaction(async storage => {
      const latest = await storage.get<QueuedDelivery>("queued");
      if (latest?.delivery === queued.delivery) {
        await storage.delete("queued");
        await storage.setAlarm(Date.now() + RETENTION_MS);
      } else if (latest) {
        // A delivery arriving during evaluation may name a new rerun attempt.
        await storage.setAlarm(Date.now() + 1000);
      }
    });
  }
}
