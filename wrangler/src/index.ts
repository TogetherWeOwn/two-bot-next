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
 * is Hyperdrive → shared Neon Postgres once S1 lands; until then
 * REDIRECT_MAPPINGS_JSON carries a snapshot of the same rows.
 */

import { Container } from "@cloudflare/containers";
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
  DISCORD_TOKEN?: string;
  DATABASE_URL?: string;
  GUILD_ID?: string;
  BOT_PORT?: string;
  KEEPALIVE_SECONDS?: string;
  /** Consecutive failed probes; default covers ~10 minutes of keepalive ticks. */
  UNREADY_ALERT_FAILURES?: string;
  /** Optional Worker secret; never forwarded to the container or logged. */
  OPS_ALERT_WEBHOOK_URL?: string;
  /** Optional Worker secret: bearer token for GET /ops/metrics. Unset → route 404s. */
  METRICS_SCRAPE_TOKEN?: string;
  /** Hyperdrive binding to shared Postgres (S1). Absent until S1 lands. */
  REDIRECT_DB?: Hyperdrive;
  /** Invite code for `/` and DB outages. Optional but recommended. */
  REDIRECT_FALLBACK_CODE?: string;
  /** JSON snapshot of invite_campaigns rows (pre-S1 mapping source). */
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
  // node-postgres ships inside the Worker via the `nodejs_compat` flag only
  // when S1 wires Hyperdrive; until then connect stays undefined and the
  // store serves the snapshot with clicks dropped (logged, never faked).
  return new RedirectStore(env.REDIRECT_DB, undefined, snapshot);
}

interface KeepalivePayload {
  startedAt: number;
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

function containerPort(raw: string | undefined): number {
  if (raw === undefined) return 8080;
  const port = Number(raw);
  if (!/^\d+$/.test(raw) || !Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error("BOT_PORT must be an integer between 1 and 65535");
  }
  return port;
}

/** Readonly view of the secrets/vars the DO forwards into the container. */
function containerEnvVars(env: Env, port: number): Record<string, string> {
  const vars: Record<string, string> = forwardedFlagVars(env);
  if (env.DISCORD_TOKEN) vars["DISCORD_TOKEN"] = env.DISCORD_TOKEN;
  if (env.DATABASE_URL) vars["DATABASE_URL"] = env.DATABASE_URL;
  if (env.GUILD_ID) vars["GUILD_ID"] = env.GUILD_ID;
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

  override async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname === "/health" || url.pathname === "/readyz") {
      await this.armKeepalive();
      return this.containerFetch(request);
    }

    // Reached only through the Worker's bearer-token gate (see default export).
    if (url.pathname === OPS_METRICS_PATH && request.method === "GET") {
      await this.armKeepalive();
      const upstream = await this.containerFetch("http://c/metrics");
      return new Response(await upstream.text(), {
        status: upstream.status,
        headers: { "content-type": "text/plain; version=0.0.4; charset=utf-8", "cache-control": "no-store" },
      });
    }

    return new Response("not found", { status: 404 });
  }

  private keepaliveArming: Promise<void> | undefined;
  private keepaliveRunning = false;

  /** Arm one persistent chain, including after Container restart/DO eviction. */
  private armKeepalive(): Promise<void> {
    if (this.keepaliveRunning) return Promise.resolve();
    if (this.keepaliveArming) return this.keepaliveArming;
    // SDK 0.3.7 creates a new task ID on EVERY schedule() call. Coalesce local
    // callers and check persisted schedules before inserting a new task.
    // Source: https://github.com/cloudflare/containers/blob/v0.3.7/src/lib/container.ts
    this.keepaliveArming = (async () => {
      if ((await this.listSchedules("keepalive")).length === 0) {
        await this.schedule(this.keepaliveSeconds(), "keepalive", {
          startedAt: Date.now(),
        } satisfies KeepalivePayload);
      }
    })().catch(() => {
      // Monitoring setup must not replace health/readiness responses or fail
      // SDK startup via onStart. Later callers retry; never log error details.
      // https://developers.cloudflare.com/containers/api/container-class/#onstart
      console.warn(JSON.stringify({ event: "container_keepalive_arm_failed" }));
    }).finally(() => { this.keepaliveArming = undefined; });
    return this.keepaliveArming;
  }

  private keepaliveSeconds(): number {
    const raw = Number(this.env.KEEPALIVE_SECONDS);
    return Number.isFinite(raw) && raw > 0 ? raw : DEFAULT_KEEPALIVE_SECONDS;
  }

  /**
   * Periodic keepalive: called by the Container scheduler every
   * KEEPALIVE_SECONDS. Renews the activity timeout (so the instance never
   * idles out from under the gateway) and probes /readyz; then re-arms.
   * Invoked by name via schedule() — keep public.
   */
  public async keepalive(payload: KeepalivePayload, schedule?: { taskId: string }): Promise<void> {
    // SDK 0.3.7 passes undefined when an earlier callback deleted a row in
    // its due-task snapshot. Such stale callbacks must not sample or re-arm.
    if (!schedule || this.keepaliveRunning) return;
    this.keepaliveRunning = true;
    try {
      await this.keepaliveArming;
      // The SDK already looked up this due task by ID before invoking us.
      // listSchedules() in 0.3.7 is unordered LIMIT 1: it can select a future
      // legacy row, and a failed extra lookup loses this task when the SDK
      // deletes it after the callback. Trust the live callback context instead.
      // Source: https://github.com/cloudflare/containers/blob/v0.3.7/src/lib/container.ts
      this.renewActivityTimeout();
      try {
        let status: number | null = null;
        try {
          const res = await this.containerFetch("http://c/readyz", {
            signal: AbortSignal.timeout(6000),
          });
          status = res.status;
          if (!res.ok) {
            console.warn(`two-bot /readyz unhealthy: ${status}`);
          }
          // /readyz is a small JSON response. Drain rather than cancel: SDK
          // 0.3.7's proxy pipe has an unhandled rejection on cancellation.
          await res.arrayBuffer();
        } catch {
          status = null;
          console.warn("two-bot keepalive probe failed");
        }
        await this.recordReadiness(status);
        await this.evaluateMetricsAlerts();
      } finally {
        // Replace the executing row AND any legacy duplicate chains with one
        // successor. onStart/inbound requests must not arm during this tick.
        // https://developers.cloudflare.com/containers/api/container-class/#schedule
        this.deleteSchedules("keepalive");
        await this.schedule(this.keepaliveSeconds(), "keepalive", payload);
      }
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
    console.log("two-bot container started");
    await this.armKeepalive();
  }

  override onStop(): void {
    console.log("two-bot container stopped");
  }

  override onError(error: unknown): void {
    console.error(`two-bot container error: ${String(error)}`);
    throw error;
  }
}

export default {
  async fetch(
    request: Request,
    env: Env,
    ctx: ExecutionContext,
  ): Promise<Response> {
    const url = new URL(request.url);

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
      // expects, but drop the query, caller headers and body.
      const clean = new URL(request.url);
      clean.search = "";
      clean.hash = "";
      const sanitized = new Request(clean.toString(), {
        method: request.method,
      });
      const container = env.TWO_BOT.getByName(SINGLETON_NAME);
      return container.fetch(sanitized);
    }

    // Authenticated off-container scrape path. No configured token → 404 (the
    // route does not exist); missing/wrong bearer → 401. Exact path only.
    if (url.pathname === OPS_METRICS_PATH) {
      if (!env.METRICS_SCRAPE_TOKEN || request.method !== "GET") return new Response("not found", { status: 404 });
      const m = /^Bearer (.+)$/.exec(request.headers.get("authorization") ?? "");
      if (!m || !(await tokenMatches(m[1]!, env.METRICS_SCRAPE_TOKEN))) {
        return new Response("unauthorized", { status: 401, headers: { "www-authenticate": "Bearer" } });
      }
      return env.TWO_BOT.getByName(SINGLETON_NAME).fetch(request);
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
        isThrottled: (key) => !clickBuckets.take(key).allowed,
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
