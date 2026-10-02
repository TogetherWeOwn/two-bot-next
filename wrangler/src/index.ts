/**
 * two-bot-next Worker + Durable Object wrapper (S1 skeleton).
 *
 * One singleton Container runs the Rust bot. The gateway is an *outbound*
 * WebSocket — it generates no inbound traffic, so Container idle billing
 * would sleep the instance and silently drop guild/voice events (ADR 0001).
 * The keepalive below prevents that: every KEEPALIVE_SECONDS the DO wakes
 * itself via schedule() (never a raw alarm() override — the Container base
 * class owns alarm()), renews the activity timeout, and probes /readyz.
 * Inbound fetches to /health and /readyz are proxied to the container
 * (renewing activity implicitly); everything else 404s.
 *
 * Secrets (DISCORD_TOKEN, DATABASE_URL) arrive as Worker secrets and are
 * forwarded as container env vars on start. They are never logged.
 *
 * B3 (TOG-9696): the same Worker also serves the go.two.gg redirect — every
 * path except /health and /readyz is a tracked invite link (see redirect.ts,
 * a behavioral port of two-bot's src/redirect/server.ts). The mapping source
 * is Hyperdrive → shared Neon Postgres when the optional REDIRECT_DB binding
 * exists; otherwise REDIRECT_MAPPINGS_JSON carries a snapshot of the same rows.
 */

import { Container } from "@cloudflare/containers";
// SDK 0.3.7 reattaches outbound callbacks even for an already-running process.
// https://developers.cloudflare.com/containers/container-package/
export { ContainerProxy } from "@cloudflare/containers";
import {
  OwnershipFence, OwnershipRefused, CONTROL_PATH, DEPLOYMENT_HEADER,
  authenticated, deploymentId, readChange, refused,
} from "./ownership.ts";
import {
  TokenBuckets,
  RedirectMissCache,
  handleRedirect,
  isReservedInternal,
  isValidFallback,
  redirectErrorClass,
  type RedirectClick,
} from "./redirect.ts";
import { RedirectStore, parseMappingsSnapshot } from "./redirect-store.ts";
import { connectPostgres } from "./redirect-db.ts";
import { forwardedFlagVars, type ForwardedFlagEnv } from "./container-env.ts";
import {
  EMPTY_STATE,
  evaluateMetrics,
  parseExposition,
  transitionMessages,
  type MetricsAlertState,
} from "./alert-rules.ts";

/** Plus the optional reviewed TWO_* flags in container-env.ts (TOG-12020). */
export interface Env extends ForwardedFlagEnv {
  TWO_BOT: DurableObjectNamespace<TwoBotContainer>;
  /** Cloudflare version identity, never a client-supplied owner name. */
  CF_VERSION_METADATA?: { id: string };
  /** Dedicated control bearer secret; absent means no control operations. */
  OWNERSHIP_CONTROL_TOKEN?: string;
  DISCORD_TOKEN?: string;
  DATABASE_URL?: string;
  GUILD_ID?: string;
  // Explicit: not a TWO_* flag, so outside the container-env allowlist.
  DISCORD_APPLICATION_ID?: string;
  BOT_PORT?: string;
  KEEPALIVE_SECONDS?: string;
  /** Consecutive failed probes; default covers ~10 minutes of keepalive ticks. */
  UNREADY_ALERT_FAILURES?: string;
  /** Optional Worker secret; never forwarded to the container or logged. */
  OPS_ALERT_WEBHOOK_URL?: string;
  /** Optional Worker secret: bearer token for GET /ops/metrics. Unset → route 404s. */
  METRICS_SCRAPE_TOKEN?: string;
  /** Optional Hyperdrive binding to shared Postgres; absent → snapshot, clicks dropped. */
  REDIRECT_DB?: Hyperdrive;
  /** Invite code for `/` and DB outages. Optional but recommended. */
  REDIRECT_FALLBACK_CODE?: string;
  /** JSON snapshot of invite_campaigns rows (mapping source without REDIRECT_DB). */
  REDIRECT_MAPPINGS_JSON?: string;
}

// Per-isolate crawler cap (60 burst, 1/sec refill — matches legacy
// CLICK_BUCKET). Module-level so one isolate shares the budget.
const clickBuckets = new TokenBuckets();
// Store instances are request-scoped; misses must survive across requests —
// but only while the backing configuration is identical. A changed snapshot
// or mapping source must not inherit another config's misses, or a newly
// added slug would 404 until the TTL expires.
let redirectMissKey = "";
let redirectMisses = new RedirectMissCache();
function missCacheFor(env: Env): RedirectMissCache {
  const raw = env.REDIRECT_MAPPINGS_JSON;
  const key = `${env.REDIRECT_DB === undefined ? "snapshot" : "live"}:${typeof raw === "string" ? raw : typeof raw}`;
  if (key !== redirectMissKey) {
    redirectMissKey = key;
    redirectMisses = new RedirectMissCache();
  }
  return redirectMisses;
}

// Public probe cap (threat-model F7, TOG-12245): /health and /readyz are
// unauthenticated, so each caller gets a bounded budget before the Container
// is touched. Over budget → 429. The Container's own alarm probe bypasses
// the Worker and is unaffected.
const healthBuckets = new TokenBuckets();

function redirectStore(env: Env): RedirectStore {
  const raw = env.REDIRECT_MAPPINGS_JSON;
  const snapshot = raw === undefined || raw === "" ? [] : parseMappingsSnapshot(raw);
  // Without the binding there is no connector: the store serves the snapshot
  // and drops clicks (logged, never faked) exactly as before TOG-12194.
  const connect = env.REDIRECT_DB === undefined ? undefined : connectPostgres;
  return new RedirectStore(env.REDIRECT_DB, connect, snapshot);
}

interface KeepalivePayload {
  startedAt: number;
  deploymentId?: string;
  epoch?: number;
}

interface ReadinessState {
  failures: number;
  firstFailureAt: number | null;
  alerted: boolean;
  lastProbeAt: number;
  lastStatus: number | null;
}

type ReadinessEvent = "container_unready_alert" | "container_unready_recovery";

const SINGLETON_NAME = "two-bot";
const DEFAULT_KEEPALIVE_SECONDS = 60;
const DEFAULT_UNREADY_SECONDS = 600;
const READINESS_KEY = "two-bot:readiness";
const METRICS_ALERT_KEY = "two-bot:metrics-alerts";
const OPS_METRICS_PATH = "/ops/metrics";

/** Compare via digests so length/prefix timing does not leak the token. */
async function tokenMatches(provided: string, expected: string): Promise<boolean> {
  const enc = new TextEncoder();
  const [a, b] = await Promise.all([
    crypto.subtle.digest("SHA-256", enc.encode(provided)),
    crypto.subtle.digest("SHA-256", enc.encode(expected)),
  ]);
  const x = new Uint8Array(a);
  const y = new Uint8Array(b);
  let diff = 0;
  for (let i = 0; i < x.length; i++) diff |= x[i]! ^ y[i]!;
  return diff === 0;
}

// SDK failures do not carry a trustworthy Rust startup class. Never serialize
// arbitrary exception text (or claim stderr crossed the Container boundary).
function containerUnavailable(): Response {
  console.error(JSON.stringify({ event: "container_probe_failed", error_class: "container_unavailable" }));
  return Response.json({ ready: false, error_class: "container_unavailable" }, { status: 500 });
}

// Allowlist, not denylist: only the bot's own answers may reach the public
// probe, i.e. 200 or its parked readiness as JSON 503. SDK 0.3.7 synthesizes
// text 429 (raw e.message), 500 and 503 bodies from startup failures.
// Source: https://github.com/cloudflare/containers/blob/v0.3.7/src/lib/container.ts
function isBotProbeResponse(response: Response): boolean {
  if (response.status === 200) return true;
  const mediaType = response.headers.get("content-type")?.split(";")[0]?.trim().toLowerCase();
  return response.status === 503 && mediaType === "application/json";
}

function containerPort(raw: string | undefined): number {
  if (raw === undefined) return 8080;
  const port = Number(raw);
  if (!/^\d+$/.test(raw) || !Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error("BOT_PORT must be an integer between 1 and 65535");
  }
  return port;
}

// Non-TWO_* container input: application ID for command registry sync.
// The TWO_* publication flags ride the reviewed container-env allowlist.
const APPLICATION_ID_KEY = "DISCORD_APPLICATION_ID" as const;

/** Readonly view of the secrets/vars the DO forwards into the container. */
function containerEnvVars(env: Env, port: number): Record<string, string> {
  const vars: Record<string, string> = forwardedFlagVars(env);
  if (env.DISCORD_TOKEN) vars["DISCORD_TOKEN"] = env.DISCORD_TOKEN;
  if (env.DATABASE_URL) vars["DATABASE_URL"] = env.DATABASE_URL;
  if (env.GUILD_ID) vars["GUILD_ID"] = env.GUILD_ID;
  const applicationId = env[APPLICATION_ID_KEY];
  if (applicationId !== undefined) vars[APPLICATION_ID_KEY] = applicationId;
  vars["LISTEN_ADDR"] = `0.0.0.0:${port}`;
  return vars;
}

export class TwoBotContainer extends Container<Env> {
  override defaultPort = containerPort(this.env.BOT_PORT);
  // Belt and braces: the schedule() keepalive below is the primary guard;
  // a long sleepAfter means a missed tick or two never costs the session.
  sleepAfter = "30m";
  pingEndpoint = "health";
  // SDK auto-start (containerFetch -> startAndWaitForPorts) bypasses start().
  // Class defaults feed every startup path; explicit envVars replace them.
  // https://developers.cloudflare.com/containers/examples/env-vars-and-secrets/
  override envVars = containerEnvVars(this.env, this.defaultPort);

  private readonly ownership = new OwnershipFence(this.ctx.storage);
  private readonly initialization: Promise<void>;
  private recoveryFailed = false;
  private static readonly KEEPALIVE_KEY = "two-bot:keepalive:v1";

  constructor(ctx: DurableObjectState<{}>, env: Env) {
    super(ctx, env);
    // A deployment can reattach to a running old process. Reconcile before
    // ingress or alarms are delivered, not just before the next cold start.
    this.initialization = ctx.blockConcurrencyWhile(async () => {
      try {
        await this.ownership.require(this.id());
      } catch (error) {
        refused(error);
        try { await this.destroyInactive(); }
        catch { this.recoveryFailed = true; }
      }
    });
  }

  private id(): string { return deploymentId(this.env.CF_VERSION_METADATA?.id); }

  /** Nested SDK calls use nested DO gates, never a non-reentrant JS mutex.
   * Catch inside the gate: throwing there resets the DO. All container I/O
   * stays inside the gate, so a takeover drains admitted starts/probes first.
   * https://developers.cloudflare.com/durable-objects/api/state/#blockconcurrencywhile
   */
  private async gated<T>(operation: () => Promise<T>): Promise<T> {
    await this.initialization;
    const result = await this.ctx.blockConcurrencyWhile(async () => {
      try { return { ok: true as const, value: await operation() }; }
      catch (error) { return { ok: false as const, error }; }
    });
    if (!result.ok) throw result.error;
    return result.value;
  }

  private async owned<T>(operation: () => Promise<T>): Promise<T> {
    return this.gated(async () => {
      if (this.recoveryFailed) throw new OwnershipRefused("shutdown_unconfirmed");
      await this.ownership.require(this.id());
      return operation();
    });
  }

  private async destroyInactive(): Promise<void> {
    this.deleteSchedules("keepalive");
    await this.ctx.storage.delete(TwoBotContainer.KEEPALIVE_KEY);
    if (this.ctx.container?.running) await super.destroy();
    if (this.ctx.container?.running) throw new OwnershipRefused("shutdown_unconfirmed");
  }

  override async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === CONTROL_PATH) return this.control(request);
    if (url.pathname !== "/health" && url.pathname !== "/readyz" && url.pathname !== OPS_METRICS_PATH) {
      return new Response("not found", { status: 404 });
    }
    try {
      return await this.owned(async () => {
        await this.ownership.require(this.id(), request.headers.get(DEPLOYMENT_HEADER) ?? "");
        // Reached only through the Worker's bearer-token gate (see default export).
        if (url.pathname === OPS_METRICS_PATH) {
          await this.armKeepalive();
          const upstream = await this.containerFetch("http://c/metrics");
          return new Response(await upstream.text(), {
            status: upstream.status,
            headers: { "content-type": "text/plain; version=0.0.4; charset=utf-8", "cache-control": "no-store" },
          });
        }
        // Bound the entire probe, including auto-start, below the 30s DO gate.
        const probe = new Request(request, { signal: AbortSignal.timeout(6000) });
        try {
          const response = await this.containerFetch(probe);
          // SDK startup failures arrive as responses rather than rejections.
          // Drop any other body without exposing its error message.
          if (!isBotProbeResponse(response)) {
            await response.arrayBuffer();
            return containerUnavailable();
          }
          await this.armKeepalive();
          return response;
        } catch {
          return containerUnavailable();
        }
      });
    } catch (error) { return refused(error); }
  }

  private async control(request: Request): Promise<Response> {
    if (!await authenticated(request, this.env.OWNERSHIP_CONTROL_TOKEN)) {
      return new Response("unauthorized", { status: 401 });
    }
    if (request.method !== "GET" && request.method !== "POST") {
      return new Response("method not allowed", { status: 405, headers: { allow: "GET, POST" } });
    }
    const change = request.method === "POST" ? await readChange(request) : null;
    if (request.method === "POST" && !change) {
      return new Response("invalid ownership change", { status: 400 });
    }
    try {
      return await this.gated(async () => {
        const id = this.id();
        if (request.headers.get(DEPLOYMENT_HEADER) !== id) throw new OwnershipRefused("deployment_mismatch");
        const owner = change
          ? await this.ownership.change(id, change, () => this.destroyInactive())
          : await this.ownership.read();
        if (change) {
          this.recoveryFailed = false;
          console.log(`two-bot ownership change: ${JSON.stringify(owner)}`);
        }
        // Takeover does not start the gateway. A subsequent owned probe does.
        return Response.json({ deploymentId: id, owner: owner ?? null, running: this.ctx.container?.running ?? false }, {
          headers: { "cache-control": "no-store" },
        });
      });
    } catch (error) { return refused(error); }
  }

  // SDK 0.3.7 containerFetch auto-starts via startAndWaitForPorts, NOT start.
  // Guard all three supported entries, including direct binding/RPC calls.
  override async start(...args: Parameters<Container<Env>["start"]>): Promise<void> {
    return this.owned(() => super.start(...args));
  }

  override async startAndWaitForPorts(...args: Parameters<Container<Env>["startAndWaitForPorts"]>): Promise<void> {
    return this.owned(() => super.startAndWaitForPorts(...args));
  }

  override async containerFetch(...args: Parameters<Container<Env>["containerFetch"]>): Promise<Response> {
    return this.owned(() => super.containerFetch(...args));
  }

  private keepaliveArming: Promise<void> | undefined;
  private keepaliveRunning = false;

  /** One persisted chain, with ownership checked outside fail-open monitoring. */
  private async armKeepalive(): Promise<void> {
    const owner = await this.ownership.require(this.id());
    if (this.keepaliveRunning) return;
    if (this.keepaliveArming) return this.keepaliveArming;
    // SDK 0.3.7 inserts a new row on every schedule() call. An old epoch's
    // pending callback is never authority to keep this deployment alive.
    this.keepaliveArming = (async () => {
      const [pending] = await this.listSchedules<KeepalivePayload>("keepalive");
      if (pending?.payload?.deploymentId === this.id() && pending.payload.epoch === owner.epoch) return;
      this.deleteSchedules("keepalive");
      await this.schedule(this.keepaliveSeconds(), "keepalive", {
        startedAt: Date.now(), deploymentId: this.id(), epoch: owner.epoch,
      } satisfies KeepalivePayload);
    })().catch(() => {
      // Scheduling failures are monitoring failures, not permission failures.
      // Never suppress the ownership read above or expose provider error text.
      console.warn(JSON.stringify({ event: "container_keepalive_arm_failed" }));
    }).finally(() => { this.keepaliveArming = undefined; });
    return this.keepaliveArming;
  }

  private keepaliveSeconds(): number {
    const raw = Number(this.env.KEEPALIVE_SECONDS);
    return Number.isFinite(raw) && raw > 0 ? raw : DEFAULT_KEEPALIVE_SECONDS;
  }

  /** Stale epochs drain without renewing, probing, notifying or rearming. */
  public async keepalive(payload: KeepalivePayload, schedule?: { taskId: string }): Promise<void> {
    // The SDK can pass undefined for rows deleted from its due-task snapshot.
    if (!schedule || this.keepaliveRunning) return;
    this.keepaliveRunning = true;
    try {
      await this.owned(async () => {
        const owner = await this.ownership.require(this.id());
        if (payload?.deploymentId !== this.id() || payload.epoch !== owner.epoch) {
          throw new OwnershipRefused("stale_keepalive");
        }
        await this.keepaliveArming;
        this.renewActivityTimeout();
        let admitted = true;
        try {
          let status: number | null = null;
          try {
            const res = await this.containerFetch("http://c/readyz", { signal: AbortSignal.timeout(6000) });
            status = res.status;
            if (!res.ok) console.warn(`two-bot /readyz unhealthy: ${status}`);
            // Drain rather than cancel the SDK 0.3.7 proxy response pipe.
            await res.arrayBuffer();
          } catch (error) {
            if (error instanceof OwnershipRefused) {
              admitted = false;
              throw error;
            }
            status = null;
            console.warn("two-bot keepalive probe failed");
          }
          await this.recordReadiness(status);
          await this.evaluateMetricsAlerts();
        } finally {
          // Collapse duplicate chains, preserving unrelated SDK schedules.
          // No takeover can interleave with this owned callback's I/O.
          this.deleteSchedules("keepalive");
          if (admitted) {
            await this.ownership.require(this.id());
            await this.schedule(this.keepaliveSeconds(), "keepalive", payload);
          }
        }
      });
    } catch (error) {
      if (error instanceof OwnershipRefused) refused(error);
      else throw error;
    } finally {
      this.keepaliveRunning = false;
    }
  }

  private unreadyAlertFailures(): number {
    const raw = this.env.UNREADY_ALERT_FAILURES;
    const count = Number(raw);
    if (raw && /^\d+$/.test(raw) && Number.isSafeInteger(count) && count > 0) {
      return count;
    }
    return Math.max(1, Math.ceil(DEFAULT_UNREADY_SECONDS / this.keepaliveSeconds()));
  }

  private async recordReadiness(status: number | null): Promise<void> {
    const now = Date.now();
    const ready = status !== null && status >= 200 && status < 300;
    const threshold = this.unreadyAlertFailures();
    // Container extends DurableObject; KV survives restarts and DO eviction.
    // No network await between the read and write: DO storage input gates
    // protect this transition from interleaving read/modify/write calls.
    // https://developers.cloudflare.com/durable-objects/api/sqlite-storage-api/#access-storage
    const previous = await this.ctx.storage.get<ReadinessState>(READINESS_KEY);
    const failures = ready ? 0 : (previous?.failures ?? 0) + 1;
    let event: ReadinessEvent | undefined;
    if (ready && previous?.alerted) {
      event = "container_unready_recovery";
    } else if (!ready && failures >= threshold && !previous?.alerted) {
      event = "container_unready_alert";
    }
    const state: ReadinessState = {
      failures,
      firstFailureAt: ready ? null : previous?.firstFailureAt ?? now,
      alerted: !ready && (previous?.alerted === true || event === "container_unready_alert"),
      lastProbeAt: now,
      lastStatus: status,
    };
    // Persist the transition BEFORE notifying: at most one attempt per event,
    // even if the webhook times out after accepting it or an alarm is retried.
    await this.ctx.storage.put(READINESS_KEY, state);
    if (!event) return;

    console.warn(JSON.stringify({
      event,
      service: "two-bot-next",
      consecutive_failures: ready ? previous!.failures : failures,
      threshold,
      status,
      first_failure_at: ready ? previous!.firstFailureAt : state.firstFailureAt,
      observed_at: now,
    }));
    await this.postReadinessWebhook(event);
  }

  /** Pull /metrics, evaluate rules, notify on transitions. Never throws. */
  private async evaluateMetricsAlerts(): Promise<void> {
    try {
      const res = await this.containerFetch("http://c/metrics", { signal: AbortSignal.timeout(6000) });
      if (!res.ok) {
        await res.arrayBuffer();
        return;
      }
      const samples = parseExposition(await res.text());
      const previous = (await this.ctx.storage.get<MetricsAlertState>(METRICS_ALERT_KEY)) ?? EMPTY_STATE;
      const { firing, state } = evaluateMetrics(samples, previous, Date.now() / 1000);
      // Persist before notifying: at most one attempt per transition.
      await this.ctx.storage.put(METRICS_ALERT_KEY, state);
      for (const content of transitionMessages(previous.firing, firing)) {
        console.warn(JSON.stringify({ event: "metrics_alert", service: "two-bot-next", content }));
        await this.postWebhookText(content);
      }
    } catch {
      console.warn("two-bot metrics scrape failed");
    }
  }

  private async postWebhookText(content: string): Promise<void> {
    const binding = this.env.OPS_ALERT_WEBHOOK_URL;
    if (!binding) return;
    try {
      const url = new URL(binding);
      if (url.protocol !== "https:" || url.username || url.password) throw new Error("invalid webhook binding");
      const response = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json" },
        redirect: "error",
        signal: AbortSignal.timeout(6000),
        body: JSON.stringify({ content, allowed_mentions: { parse: [], replied_user: false } }),
      });
      await response.body?.cancel();
      if (!response.ok) console.warn(JSON.stringify({ event: "metrics_alert_webhook_failed", status: response.status }));
    } catch {
      console.warn(JSON.stringify({ event: "metrics_alert_webhook_failed", status: null }));
    }
  }

  private async postReadinessWebhook(event: ReadinessEvent): Promise<void> {
    const binding = this.env.OPS_ALERT_WEBHOOK_URL;
    if (!binding) return;
    try {
      const url = new URL(binding);
      if (url.protocol !== "https:" || url.username || url.password) {
        throw new Error("invalid webhook binding");
      }
      const response = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json" },
        redirect: "error",
        signal: AbortSignal.timeout(6000),
        body: JSON.stringify({
          content: event === "container_unready_alert"
            ? "two-bot-next: Container /readyz has repeatedly failed. Check the gateway connection and Worker logs."
            : "two-bot-next: Container /readyz is ready again. The unready incident has recovered.",
          // https://docs.discord.com/developers/resources/webhook#execute-webhook
          allowed_mentions: { parse: [], replied_user: false },
        }),
      });
      await response.body?.cancel();
      if (!response.ok) {
        console.warn(JSON.stringify({ event: "container_unready_webhook_failed", notification: event, status: response.status }));
      }
    } catch {
      // Fetch errors can contain the secret URL. Never log the error or body.
      console.warn(JSON.stringify({ event: "container_unready_webhook_failed", notification: event, status: null }));
    }
  }

  override async onStart(): Promise<void> {
    try {
      await this.owned(async () => {
        console.log("two-bot container started");
        await this.armKeepalive();
      });
    } catch (error) {
      // A read/write failure after native startup cannot leave an unconfirmed
      // gateway running just because the HTTP request will fail closed.
      try { await this.destroyInactive(); }
      catch { this.recoveryFailed = true; }
      throw error;
    }
  }

  override onStop(): void {
    console.log("two-bot container stopped");
  }

  override onError(_error: unknown): void {
    console.error(JSON.stringify({ event: "container_error", error_class: "container_lifecycle_failed" }));
    // The SDK logs errors thrown by hooks too; replace rather than rethrow the
    // original exception, and do not retain a credential-bearing cause.
    throw new Error("container_lifecycle_failed");
  }
}

export default {
  async fetch(
    request: Request,
    env: Env,
    ctx: ExecutionContext,
  ): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname === CONTROL_PATH) {
      if (!await authenticated(request, env.OWNERSHIP_CONTROL_TOKEN)) {
        return new Response("unauthorized", { status: 401 });
      }
      try {
        const forwarded = new Request(request);
        // A stale Worker sharing this namespace cannot impersonate the current
        // DO version; always overwrite untrusted ingress identity.
        forwarded.headers.set(DEPLOYMENT_HEADER, deploymentId(env.CF_VERSION_METADATA?.id));
        return await env.TWO_BOT.getByName(SINGLETON_NAME).fetch(forwarded);
      } catch (error) { return refused(error); }
    }

    if (url.pathname === "/health" || url.pathname === "/readyz") {
      // Public probes carry no credentials, so only GET/HEAD are meaningful.
      // Anything else cannot be a scraper or load-balancer check: refuse it
      // before the Container is touched.
      if (request.method !== "GET" && request.method !== "HEAD") {
        return new Response("method not allowed", {
          status: 405,
          headers: { allow: "GET, HEAD" },
        });
      }
      // Per-caller budget before the Container is touched. The key picks a
      // bucket and never survives the call.
      const probe = healthBuckets.take(
        request.headers.get("cf-connecting-ip") ?? "unknown",
      );
      if (!probe.allowed) {
        return new Response("slow down\n", {
          status: 429,
          headers: {
            "content-type": "text/plain",
            "retry-after": String(probe.retryAfter),
          },
        });
      }
      // Sanitize the forwarded probe: keep the origin (Host) the Container
      // expects, but drop the query, caller headers and body. The only header
      // the DO sees is the Worker-stamped deployment id the fence requires.
      const clean = new URL(request.url);
      clean.search = "";
      clean.hash = "";
      // A missing version identity cannot address a deployment: refuse like
      // the control and metrics paths instead of blaming the container.
      let deployment: string;
      try {
        deployment = deploymentId(env.CF_VERSION_METADATA?.id);
      } catch (error) { return refused(error); }
      let response: Response;
      try {
        const sanitized = new Request(clean.toString(), {
          method: request.method,
          headers: { [DEPLOYMENT_HEADER]: deployment },
        });
        const upstream = await env.TWO_BOT.getByName(SINGLETON_NAME).fetch(sanitized);
        // SDK startup failures arrive as responses rather than rejections.
        // Drop any other body without exposing its error message.
        if (!isBotProbeResponse(upstream)) {
          await upstream.arrayBuffer();
          response = containerUnavailable();
        } else {
          response = upstream;
        }
      } catch {
        // Includes DO construction/binding failures before its fetch handler.
        response = containerUnavailable();
      }
      // The outer Worker owns provenance, not the container or DO version.
      // Copy the response to get mutable headers without changing status/body.
      const result = new Response(response.body, response);
      if (env.CF_VERSION_METADATA) {
        result.headers.set("x-two-worker-version", env.CF_VERSION_METADATA.id);
      } else {
        result.headers.delete("x-two-worker-version");
      }
      return result;
    }

    // Authenticated off-container scrape path. No configured token → 404 (the
    // route does not exist); missing/wrong bearer → 401. Exact path only.
    if (url.pathname === OPS_METRICS_PATH) {
      if (!env.METRICS_SCRAPE_TOKEN || request.method !== "GET") return new Response("not found", { status: 404 });
      const m = /^Bearer (.+)$/.exec(request.headers.get("authorization") ?? "");
      if (!m || !(await tokenMatches(m[1]!, env.METRICS_SCRAPE_TOKEN))) {
        return new Response("unauthorized", { status: 401, headers: { "www-authenticate": "Bearer" } });
      }
      try {
        // The fenced DO route requires the deployment header (see fetch);
        // stamp it here like the health/readyz forward above.
        const forwarded = new Request(request);
        forwarded.headers.set(DEPLOYMENT_HEADER, deploymentId(env.CF_VERSION_METADATA?.id));
        return await env.TWO_BOT.getByName(SINGLETON_NAME).fetch(forwarded);
      } catch (error) { return refused(error); }
    }

    // Internal metrics and healthz aliases never become invite campaigns.
    // The exact /healthz redirect probe is handled below after config validation.
    if (url.pathname !== "/healthz" && isReservedInternal(url.pathname)) {
      return new Response("not found", { status: 404 });
    }

    // B3: everything else is a go.two.gg tracked link. Clicks record after
    // the 302 via waitUntil — the visitor never waits on the database, and a
    // failed write costs a click, never a member. Record failures are logged
    // with slug only (never visitor data — see redirect.ts privacy note).
    // Workers have no Node listen-port setting. Validate redirect configuration
    // before serving campaigns or the redirect probe, without logging values.
    const invalidConfig = (errorClass: string) => {
      console.error(`invite_redirect_invalid_config ${JSON.stringify({ errorClass })}`);
      return new Response("redirect service misconfigured\n", {
        status: 503,
        headers: { "content-type": "text/plain", "retry-after": "30" },
      });
    };
    if (!isValidFallback(env.REDIRECT_FALLBACK_CODE)) return invalidConfig("invalid_fallback");
    let store: RedirectStore;
    try {
      store = redirectStore(env);
    } catch {
      return invalidConfig("invalid_snapshot");
    }
    const result = await handleRedirect(
      request.method,
      url.pathname,
      // Cloudflare supplies this at ingress. Never trust X-Forwarded-For;
      // when no edge IP exists, callers share the conservative unknown bucket.
      request.headers.get("cf-connecting-ip") ?? "unknown",
      {
        guildId: env.GUILD_ID ?? "",
        fallbackInviteCode: env.REDIRECT_FALLBACK_CODE ?? null,
        lookup: (slug) => store.lookup(slug),
        recordClick: (click: RedirectClick) => store.recordClick(click),
        onError: (msg, detail) =>
          console.error(`${msg} ${JSON.stringify(detail)}`),
        throttle: (key) => clickBuckets.take(key),
        missCache: missCacheFor(env),
      },
    );
    if (result.click) {
      const click = result.click;
      ctx.waitUntil(
        store.recordClick(click).catch((err: unknown) =>
          console.error(
            `invite_click_record_failed ${JSON.stringify({ campaign: click.campaign, errorClass: redirectErrorClass(err) })}`,
          ),
        ),
      );
    }
    return new Response(result.body, {
      status: result.status,
      headers: result.headers,
    });
  },
};
