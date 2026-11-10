import { parseJson, storedArtifact, type Json } from "./encoding.ts";
import { array, decimal, ENVIRONMENT, id, object, PLAN_MARKER, REPOSITORY, requireEvidence,
  sha256, text, validateClaim, validatePlan, WORKFLOW, type Target } from "./binding.ts";

const API = "https://api.github.com";
const ROOT = `/repos/${REPOSITORY}`;
export const MAX_AGE_MS = 24 * 60 * 60 * 1000;
export const CLAIM_AGE_MS = 30 * 60 * 1000;
export interface AppConfig {
  appId: string;
  installationId: string;
  repositoryId: string;
  privateKey: string;
}
export async function boundedBody(response: Response, limit: number): Promise<Uint8Array> {
  requireEvidence(response.body !== null);
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.length;
      requireEvidence(size <= limit);
      chunks.push(value);
    }
  } finally {
    await reader.cancel();
  }
  const result = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) { result.set(chunk, offset); offset += chunk.length; }
  return result;
}
function b64(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/=/g, "").replace(/\+/g, "-").replace(/\//g, "_");
}
export async function appJwt(appId: string, pem: string, now: number): Promise<string> {
  requireEvidence(/^[1-9][0-9]{0,19}$/.test(appId));
  const match = /^-----BEGIN PRIVATE KEY-----\s+([A-Za-z0-9+/=\s]+)\s+-----END PRIVATE KEY-----\s*$/.exec(pem);
  requireEvidence(match);
  const der = Uint8Array.from(atob(match[1]!.replace(/\s/g, "")), value => value.charCodeAt(0));
  const key = await crypto.subtle.importKey("pkcs8", der, { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, false, ["sign"]);
  const enc = new TextEncoder();
  const body = b64(enc.encode(JSON.stringify({ alg: "RS256", typ: "JWT" }))) + "."
    + b64(enc.encode(JSON.stringify({ iat: Math.floor(now / 1000) - 60, exp: Math.floor(now / 1000) + 240, iss: appId })));
  return body + "." + b64(new Uint8Array(await crypto.subtle.sign("RSASSA-PKCS1-v1_5", key, enc.encode(body))));
}
function timestamp(value: Json | undefined): number {
  const raw = text(value);
  requireEvidence(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/.test(raw));
  const parsed = Date.parse(raw);
  requireEvidence(Number.isFinite(parsed));
  return parsed;
}
function fresh(value: Json | undefined, now: number, age: number): number {
  const parsed = timestamp(value);
  requireEvidence(parsed <= now && now - parsed <= age);
  return parsed;
}
export class GitHub {
  private token: string;
  private fetcher: typeof fetch;
  private signal: AbortSignal;
  private reads = 0;
  constructor(token: string, signal: AbortSignal, fetcher: typeof fetch = fetch) {
    this.token = token; this.signal = signal; this.fetcher = fetcher;
  }
  async request(path: string, method = "GET", body?: string): Promise<Response> {
    requireEvidence(++this.reads <= 24 && path.startsWith("/") && !path.startsWith("//"));
    const headers: Record<string, string> = {
      Authorization: `Bearer ${this.token}`, Accept: "application/vnd.github+json",
      "X-GitHub-Api-Version": "2022-11-28", "User-Agent": "staging-migration-protection",
    };
    if (body !== undefined) headers["Content-Type"] = "application/json";
    return this.fetcher(API + path, { method, headers, ...(body === undefined ? {} : { body }),
      redirect: "manual", signal: this.signal });
  }
  async json(path: string): Promise<Json> {
    const response = await this.request(path);
    requireEvidence(response.status === 200 && !response.headers.has("Link"));
    return parseJson(await boundedBody(response, 1024 * 1024));
  }
  async run(runId: string, config: AppConfig, now: number, prior: boolean) {
    const run = object(await this.json(`${ROOT}/actions/runs/${runId}`));
    requireEvidence(id(run.id) === runId && run.path === WORKFLOW && run.event === "workflow_dispatch"
      && run.head_branch === "main" && /^[0-9a-f]{40}$/.test(text(run.head_sha)));
    requireEvidence(object(run.repository).full_name === REPOSITORY && id(object(run.repository).id) === config.repositoryId
      && id(object(run.head_repository).id) === config.repositoryId);
    fresh(run.run_started_at, now, MAX_AGE_MS);
    requireEvidence(prior ? run.status === "completed" && run.conclusion === "success"
      : run.status === "waiting" || run.status === "in_progress" || run.status === "pending");
    return run;
  }
  async jobs(runId: string, attempt: string) {
    const listing = object(await this.json(`${ROOT}/actions/runs/${runId}/attempts/${attempt}/jobs?per_page=100`));
    const jobs = array(listing.jobs).map(object);
    requireEvidence(listing.total_count === BigInt(jobs.length) && jobs.length > 0);
    for (const job of jobs) requireEvidence(id(job.run_id) === runId && id(job.run_attempt) === attempt);
    return jobs;
  }
  job(jobs: { [key: string]: Json }[], name: string, conclusion: string) {
    const matches = jobs.filter(job => job.name === name);
    requireEvidence(matches.length === 1);
    const job = matches[0]!;
    requireEvidence(job.status === "completed" && job.conclusion === conclusion);
    return job;
  }
  async pending(runId: string): Promise<void> {
    const pending = array(await this.json(`${ROOT}/actions/runs/${runId}/pending_deployments`)).map(object);
    requireEvidence(pending.filter(item => object(item.environment).name === ENVIRONMENT).length === 1);
  }
  async artifact(runId: string, head: string, job: { [key: string]: Json }, name: string, now: number): Promise<Json> {
    const listing = object(await this.json(`${ROOT}/actions/runs/${runId}/artifacts?per_page=100`));
    const all = array(listing.artifacts).map(object);
    requireEvidence(listing.total_count === BigInt(all.length));
    const matches = all.filter(item => item.name === name);
    requireEvidence(matches.length === 1);
    const artifact = matches[0]!;
    requireEvidence(artifact.expired === false && timestamp(artifact.expires_at) > now
      && id(object(artifact.workflow_run).id) === runId && object(artifact.workflow_run).head_sha === head);
    const created = fresh(artifact.created_at, now, MAX_AGE_MS);
    requireEvidence(created >= timestamp(job.started_at) && created <= timestamp(job.completed_at));
    requireEvidence(typeof artifact.size_in_bytes === "bigint" && artifact.size_in_bytes > 0n
      && artifact.size_in_bytes <= 1024n * 1024n + 4096n);
    const response = await this.request(`${ROOT}/actions/artifacts/${id(artifact.id)}/zip`);
    requireEvidence(response.status === 302);
    const location = new URL(response.headers.get("Location") ?? "");
    // Download the signed storage URL without the installation Authorization header.
    requireEvidence(location.protocol === "https:" && location.port === "" && location.username === ""
      && location.password === "" && location.hash === ""
      && /^[a-z0-9-]+\.blob\.core\.windows\.net$/.test(location.hostname));
    const download = await this.fetcher(location.href, { redirect: "manual", signal: this.signal });
    requireEvidence(download.status === 200);
    const zip = await boundedBody(download, 1024 * 1024 + 4096);
    requireEvidence(artifact.digest === `sha256:${await sha256(zip)}`);
    return parseJson(storedArtifact(zip, `${name}.json`));
  }
  async decide(runId: string, approved: boolean): Promise<void> {
    const response = await this.request(`${ROOT}/actions/runs/${runId}/deployment_protection_rule`, "POST",
      JSON.stringify({ environment_name: ENVIRONMENT, state: approved ? "approved" : "rejected",
        comment: approved ? "Independent staging binding verified." : "Independent staging evidence refused." }));
    requireEvidence(response.status === 204);
  }
  async revoke(): Promise<void> {
    // Cleanup has its own five-second budget, even after an evaluation timeout.
    const response = await this.fetcher(API + "/installation/token", { method: "DELETE",
      headers: { Authorization: `Bearer ${this.token}`, "User-Agent": "staging-migration-protection",
        "X-GitHub-Api-Version": "2022-11-28" }, redirect: "manual", signal: AbortSignal.timeout(5000) });
    requireEvidence(response.status === 204);
  }
}
export async function installationClient(config: AppConfig, now: number, signal: AbortSignal): Promise<GitHub> {
  // Only call-time resolution; no token cache or credential-bearing audit fields.
  const issuer = new GitHub(await appJwt(config.appId, config.privateKey, now), signal);
  requireEvidence(/^[1-9][0-9]{0,19}$/.test(config.installationId) && /^[1-9][0-9]{0,19}$/.test(config.repositoryId));
  const response = await issuer.request(`/app/installations/${config.installationId}/access_tokens`, "POST",
    `{"repository_ids":[${config.repositoryId}],"permissions":{"actions":"read","deployments":"write","contents":"read"}}`);
  requireEvidence(response.status === 201);
  const result = object(parseJson(await boundedBody(response, 16384)));
  fresh(result.expires_at, now + 60 * 60 * 1000 + 60000, 120000);
  return new GitHub(text(result.token), signal);
}
export async function verifyBinding(api: GitHub, config: AppConfig, pins: Target, runId: string, now: number) {
  const current = await api.run(runId, config, now, false);
  const attempt = id(current.run_attempt), head = text(current.head_sha);
  await api.pending(runId);
  const jobs = await api.jobs(runId, attempt);
  const claimJob = api.job(jobs, "staging-migrate (claim)", "success");
  const planJob = api.job(jobs, "staging-migrate (plan)", "success");
  fresh(claimJob.completed_at, now, CLAIM_AGE_MS);
  const claim = validateClaim(await api.artifact(runId, head, claimJob, "staging-migrate-apply-claim", now), runId, attempt, head, pins);
  await validatePlan(await api.artifact(runId, head, planJob, "staging-migrate-manifest", now), claim);
  const priorId = decimal(claim.plan_run_id);
  const prior = await api.run(priorId, config, now, true);
  requireEvidence(timestamp(prior.updated_at) <= timestamp(current.run_started_at));
  const priorJobs = await api.jobs(priorId, id(prior.run_attempt));
  const priorJob = api.job(priorJobs, "staging-migrate (plan)", "success");
  const markers = array(priorJob.steps).map(object).filter(step => step.name === PLAN_MARKER);
  requireEvidence(markers.length === 1 && markers[0]!.conclusion === "success" && markers[0]!.status === "completed");
  api.job(priorJobs, "staging-migrate (claim)", "skipped");
  api.job(priorJobs, "staging-migrate (apply)", "skipped");
  await validatePlan(await api.artifact(priorId, text(prior.head_sha), priorJob, "staging-migrate-manifest", now), claim);
  const comparison = object(await api.json(`${ROOT}/compare/${text(claim.source_sha)}...main`));
  requireEvidence(object(comparison.merge_base_commit).sha === claim.source_sha
    && (comparison.status === "ahead" || comparison.status === "identical"));
  return { attempt, head };
}
