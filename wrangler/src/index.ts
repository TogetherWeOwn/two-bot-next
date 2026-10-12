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
import {
  CampaignLookupCache,
  DB_TIMEOUT_MS,
  RedirectStore,
  parseMappingsSnapshot,
} from "./redirect-store.ts";
import { connectPostgres } from "./redirect-db.ts";
import {
  forwardedDiscordIdVars,
  forwardedFlagVars,
  isSnowflake,
  type ForwardedDiscordIdEnv,
  type ForwardedFlagEnv,
} from "./container-env.ts";
import {
  ACTIONS_PATH,
  CONTAINER_MARKER,
  CONTAINER_MARKER_VALUE,
  RECEIVER_BIND,
  RECEIVER_PORT,
  REQUEST_TIMEOUT_MS,
  allowlistedHeaders,
  envelope,
  forwardRequest,
  handleActionsIngress,
  ingressEnabled,
  isActionsRequest,
  notFound,
  readBody,
  receiverEnabled,
  relayReceiverResponse,
} from "./internal-actions.ts";
import {
  OPS_AUDIT_PATH,
  OPS_CONFIG_PATH,
  OPS_ENFORCEMENT_PENDING,
  OPS_KILLSWITCH_PATH,
  OpsControl,
  configWritesEnabled,
  gateOpsRequest,
  isOpsPath,
  killswitchEnabled,
  opsRefused,
  parseConfigPut,
  parseKillswitchPost,
  readOpsBody,
} from "./ops-control.ts";
import {
  EMPTY_STATE,
  evaluateMetrics,
  interruptDispatchDrops,
  transitionMessages,
  type MetricsAlertState,
} from "./alert-rules.ts";
import { parseAlertScrape } from "./metrics-scrape.ts";
import { applySecurityHeaders, withSecurityHeaders } from "./security-headers.ts";
import {
  OPS_DASHBOARD_PATH,
  MIN_DASHBOARD_CLIENT_ID_LENGTH,
  MIN_DASHBOARD_SECRET_LENGTH,
  dashboardPageResponse,
  dashboardRefusal,
} from "./ops-dashboard.ts";

/**
 * Plus the optional reviewed TWO_* flags in container-env.ts (TOG-12020) and
 * the optional reviewed DISCORD_* snowflake IDs there (TOG-19025, M3.6).
 */
export interface Env extends ForwardedFlagEnv, ForwardedDiscordIdEnv {
  TWO_BOT: DurableObjectNamespace<TwoBotContainer>;
  /** Cloudflare version identity, never a client-supplied owner name. */
  CF_VERSION_METADATA?: { id: string };
  /** Dedicated control bearer secret; absent means no control operations. */
  OWNERSHIP_CONTROL_TOKEN?: string;
  DISCORD_TOKEN?: string;
  DATABASE_URL?: string;
  GUILD_ID?: string;
  TWO_AUTOMATIONS?: string;
  TWO_TEXT_COMMANDS?: string;
  TWO_AUTOMOD?: string;
  // Explicit: not a TWO_* flag, so outside the flag allowlist. Validated as
  // a snowflake before forwarding (see containerEnvVars below).
  DISCORD_APPLICATION_ID?: string;
  /**
   * Infrastructure ID protected from temporary-room deletion. Declared here
   * for its doc comment; forwarded via FORWARDED_DISCORD_IDS like the other
   * non-secret DISCORD_* IDs (TOG-19025).
   */
  DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID?: string;
  BOT_PORT?: string;
  KEEPALIVE_SECONDS?: string;
  /** Consecutive failed probes; default covers ~10 minutes of keepalive ticks. */
  UNREADY_ALERT_FAILURES?: string;
  /** Optional Worker secret; never forwarded to the container or logged. */
  OPS_ALERT_WEBHOOK_URL?: string;
  /** Worker-only forwarding switch. Only "on" posts; all other values are log-only. */
  OPS_ALERT_FORWARDING?: string;
  /** Optional Worker secret: bearer token for GET /ops/metrics and the
   * read-only /ops JSON roll-ups below. Unset → those routes 404. */
  METRICS_SCRAPE_TOKEN?: string;
  /**
   * Optional Worker secrets: Cloudflare Access service-token pair gating the
   * read-only ops dashboard page and summary. Either unset (or short) → both
   * routes 404. Never committed as wrangler.toml vars.
   */
  OPS_DASHBOARD_CLIENT_ID?: string;
  OPS_DASHBOARD_CLIENT_SECRET?: string;
  /** Dedicated least-privilege bearer for the admin control surface
   * (GET /ops/config, /ops/kill-switch, /ops/audit). Absent/short → dark. */
  OPS_CONTROL_TOKEN?: string;
  /** Exact "1" admits PUT /ops/config. Staging-only; never production
   * without the CISO review (AC4). Enforced by check-env-bindings.py. */
  OPS_CONFIG_WRITES_ENABLED?: string;
  /** Exact "1" admits POST /ops/kill-switch. Staging-only; never production
   * without the CISO review (AC4). Enforced by check-env-bindings.py. */
  OPS_KILLSWITCH_ENABLED?: string;
  /** Optional Hyperdrive binding to shared Postgres; absent → snapshot, clicks dropped. */
  REDIRECT_DB?: Hyperdrive;
  /** Invite code for `/` and DB outages. Optional but recommended. */
  REDIRECT_FALLBACK_CODE?: string;
  /** JSON snapshot of invite_campaigns rows (mapping source without REDIRECT_DB). */
  REDIRECT_MAPPINGS_JSON?: string;
  /** Staging-only var (wrangler.toml [env.staging.vars]); see internal-actions.ts. */
  INTERNAL_ACTIONS_INGRESS?: string;
  /** Operator-set Worker secrets for the private receiver. Never flag-forwarded. */
  TWO_INTERNAL_ACTIONS?: string;
  TWO_INTERNAL_CALLERS?: string;
  TWO_INTERNAL_CHANNEL_KEYS?: string;
  /** Signing keys: a secret, forwarded by its own explicit line and never logged. */
  TWO_INTERNAL_KEYS?: string;
}

/** Both alert senders share this exact Discord-only destination boundary. */
function opsAlertWebhookUrl(binding: string): URL {
  // Validate the literal spelling before URL parsing can normalize whitespace,
  // userinfo, backslashes or dot segments. Only the default HTTPS port is allowed.
  // https://docs.discord.com/developers/resources/webhook#execute-webhook
  if (binding !== binding.trim() ||
      !/^https:\/\/discord\.com(?::443)?\/api\/webhooks\/[0-9]+\/[A-Za-z0-9_-]+$/.test(binding)) {
    throw new Error("invalid webhook binding");
  }
  return new URL(binding);
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

// Attempt cap for the authenticated /ops/metrics scrape. Every attempt takes
// one token synchronously before the secret comparison, so a burst of
// concurrent guesses cannot share a single token across many comparisons
// (the comparison awaits digests, during which other in-flight requests would
// otherwise all pass a read-only precheck). An exhausted caller is refused
// without any comparison, so guessing cannot confirm a bearer while
// throttled. Buckets are per caller (as with the probe cap), so another
// caller's guessing cannot throttle a correct bearer — even a caller shed
// only because the 10,000-entry table is full is still compared, so a scanner
// flood cannot lock out the authenticated scraper; a correct bearer from
// the same exhausted caller waits out the retry-after like any other request.
// A correct bearer consumes one token per scrape, which the production
// scraper (~1/15 s against a 10-burst/1-per-second bucket) never nears.
// CONTROL_PATH deliberately shares nothing here: it is a separate ownership
// gate whose token already requires 32 characters (see ownership.ts
// authenticated()), and its lockout semantics belong to that fence.
const metricsAuthBuckets = new TokenBuckets({ capacity: 10, refillPerSecond: 1 });
// A short scrape token is an operator misconfiguration, not per-request
// information: say so once per isolate so scanners cannot flood the logs.
let metricsShortTokenLogged = false;

// Attempt cap for the dashboard service-token gate. Separate budget from the
// metrics scrape so guessing at one cannot throttle the other; otherwise the
// same take-before-compare semantics (see metricsAuthBuckets above).
const dashboardAuthBuckets = new TokenBuckets({ capacity: 10, refillPerSecond: 1 });

// Store instances are request-scoped, but the lookup cache must survive
// across requests to blunt repeated lookups — so it lives here beside the
// miss cache, keyed (and reset) on the same configuration identity. A changed
// snapshot or mapping source must not inherit another config's entries.
let redirectLookupKey = "";
let redirectLookupCache = new CampaignLookupCache();
function lookupCacheFor(env: Env): CampaignLookupCache {
  const raw = env.REDIRECT_MAPPINGS_JSON;
  const key = `${env.REDIRECT_DB === undefined ? "snapshot" : "live"}:${typeof raw === "string" ? raw : typeof raw}`;
  if (key !== redirectLookupKey) {
    redirectLookupKey = key;
    redirectLookupCache = new CampaignLookupCache();
  }
  return redirectLookupCache;
}

function redirectStore(env: Env): RedirectStore {
  const raw = env.REDIRECT_MAPPINGS_JSON;
  const snapshot = raw === undefined || raw === "" ? [] : parseMappingsSnapshot(raw);
  // Without the binding there is no connector: the store serves the snapshot
  // and drops clicks (logged, never faked) exactly as before TOG-12194.
  const connect = env.REDIRECT_DB === undefined ? undefined : connectPostgres;
  return new RedirectStore(env.REDIRECT_DB, connect, snapshot, DB_TIMEOUT_MS, lookupCacheFor(env));
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
// Read-only operator roll-ups (TOG-22860), served by the container as JSON.
// Gateway/backfill stay on the scrape bearer like /ops/metrics; the summary
// serves two audiences (scrape bearer or dashboard pair — see the Worker
// gate below), and the DO translates its `ready` bit into the 200-vs-503
// status the dashboard renders. Same "/ops/summary" value as
// ops-dashboard.ts (kept local so the JSON-path list stays in one place).
// Exact paths only: unknown /ops/* subpaths still fall through to the
// redirect lookup below, where the slug allowlist rejects them.
const OPS_SUMMARY_PATH = "/ops/summary";
const OPS_GATEWAY_PATH = "/ops/gateway";
const OPS_BACKFILL_PATH = "/ops/backfill";
const OPS_JSON_PATHS: string[] = [OPS_SUMMARY_PATH, OPS_GATEWAY_PATH, OPS_BACKFILL_PATH];
const OPS_METRICS_CONTENT_TYPE = "text/plain; version=0.0.4; charset=utf-8";
const OPS_JSON_CONTENT_TYPE = "application/json";
// The scrape token follows the ownership control token's floor: anything
// shorter is treated as not configured. A short staging token must be
// reissued, never padded (none is provisioned today).
const MIN_SCRAPE_TOKEN_LENGTH = 32;
// The DO serves the scrape from inside blockConcurrencyWhile, so the
// container fetch must be bounded well below the 30s DO gate.
const METRICS_FETCH_TIMEOUT_MS = 6000;
// Prometheus exposition is small; cap the proxied body so a compromised or
// wedged container cannot exhaust the isolate reading it.
const MAX_METRICS_BODY_BYTES = 64 * 1024;
// The DO serves the summary roll-up from inside blockConcurrencyWhile, so
// the container fetch must be bounded well below the 30s DO gate.
const SUMMARY_FETCH_TIMEOUT_MS = 6000;
// The roll-up is small; cap the proxied body like the metrics scrape.
const MAX_SUMMARY_BODY_BYTES = 64 * 1024;

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

// Shared read-scope gate for /ops/metrics and the JSON roll-ups. Returns
// the refusal response, or null when the request is authorized. Only GET is
// meaningful: anything else 404s like an unconfigured token, so method
// probes cannot distinguish a missing route from a missing token.
async function opsReadRefusal(request: Request, env: Env): Promise<Response | null> {
  if (!env.METRICS_SCRAPE_TOKEN || env.METRICS_SCRAPE_TOKEN.length < MIN_SCRAPE_TOKEN_LENGTH
    || request.method !== "GET") {
    if (env.METRICS_SCRAPE_TOKEN && env.METRICS_SCRAPE_TOKEN.length < MIN_SCRAPE_TOKEN_LENGTH
      && !metricsShortTokenLogged) {
      metricsShortTokenLogged = true;
      console.error(JSON.stringify({
        event: "metrics_scrape_token_misconfigured",
        reason: "token_below_minimum_length",
        minimum_length: MIN_SCRAPE_TOKEN_LENGTH,
      }));
    }
    return new Response("not found", { status: 404, headers: withSecurityHeaders() });
  }
  const caller = request.headers.get("cf-connecting-ip") ?? "unknown";
  // Take budget synchronously before the secret comparison: no guess —
  // right or wrong — is evaluated while throttled, and concurrent
  // guesses each consume their own token instead of sharing one across
  // the awaited digest comparison below. A correct bearer on a fresh
  // budget is unaffected (one token of a 10-burst).
  const verdict = metricsAuthBuckets.take(caller);
  if (!verdict.allowed && !metricsAuthBuckets.peek(caller).allowed) {
    // Tracked caller out of budget (or in terminal hold): refuse without
    // any comparison, so guessing cannot confirm a bearer while
    // exhausted. peek is synchronous, so this cannot race the take above.
    // An untracked caller denied only because the caller table is full
    // falls through to the comparison below instead, so a flood of
    // one-time scanners can never lock out the authenticated scraper.
    return new Response("slow down\n", {
      status: 429,
      headers: withSecurityHeaders({
        "content-type": "text/plain",
        "retry-after": String(verdict.retryAfter),
      }),
    });
  }
  const m = /^Bearer (.+)$/.exec(request.headers.get("authorization") ?? "");
  if (!m || m[1]!.length > 4096 || !(await tokenMatches(m[1]!, env.METRICS_SCRAPE_TOKEN))) {
    return new Response("unauthorized", { status: 401, headers: withSecurityHeaders({ "www-authenticate": "Bearer" }) });
  }
  return null;
}

// SDK failures do not carry a trustworthy Rust startup class. Never serialize
// arbitrary exception text (or claim stderr crossed the Container boundary).
function containerUnavailable(): Response {
  console.error(JSON.stringify({ event: "container_probe_failed", error_class: "container_unavailable" }));
  return Response.json({ ready: false, error_class: "container_unavailable" }, { status: 500, headers: withSecurityHeaders() });
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

// TOG-13044: while the gateway task is failing, the bot serves its fixed
// phase/class on /readyz for a bounded linger. Container stdout is not in
// Workers Logs, so the keepalive re-emits those two tokens as a structured
// warn. Strict tokens only: nothing else from the probe body is ever logged.
const FAILURE_TOKEN = /^[a-z0-9_]{1,32}$/;
const MAX_PROBE_BODY_BYTES = 64 * 1024;

/** Bound awaits even when the SDK/response stream does not observe abort. */
async function abortable<T>(work: Promise<T>, signal: AbortSignal): Promise<T> {
  let onAbort = () => {};
  const aborted = new Promise<never>((_, reject) => {
    onAbort = () => reject(new Error("metrics scrape failed"));
    signal.addEventListener("abort", onAbort, { once: true });
    if (signal.aborted) onAbort();
  });
  try {
    return await Promise.race([work, aborted]);
  } finally {
    signal.removeEventListener("abort", onAbort);
  }
}

/** Read at most `limit` bytes; cancel oversize/failed bodies without buffering. */
async function readBoundedText(response: Response, limit: number, signal?: AbortSignal): Promise<string | null> {
  const reader = response.body?.getReader();
  if (!reader) return "";
  const chunks: Uint8Array[] = [];
  let size = 0;
  let complete = false;
  try {
    for (;;) {
      const read = reader.read();
      const { done, value } = await (signal ? abortable(read, signal) : read);
      if (done) { complete = true; break; }
      size += value.byteLength;
      if (size > limit) return null;
      chunks.push(value);
    }
    const bytes = new Uint8Array(size);
    let offset = 0;
    for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
    return new TextDecoder().decode(bytes);
  } finally {
    // A hostile cancel hook can itself stall or reject. Do not await it.
    if (!complete) void reader.cancel().catch(() => {});
    reader.releaseLock();
  }
}

function gatewayFailure(body: string): { phase: string; class: string } | null {
  if (new TextEncoder().encode(body).byteLength > MAX_PROBE_BODY_BYTES) return null;
  try {
    const report: unknown = JSON.parse(body);
    const field = (report as { gateway_failure?: unknown } | null)?.gateway_failure;
    if (typeof field !== "object" || field === null) return null;
    const { phase, class: errorClass } = field as Record<string, unknown>;
    if (typeof phase === "string" && typeof errorClass === "string"
      && FAILURE_TOKEN.test(phase) && FAILURE_TOKEN.test(errorClass)) {
      return { phase, class: errorClass };
    }
  } catch {
    // Not the bot's JSON: nothing to report.
  }
  return null;
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
// The TWO_* publication flags ride the reviewed container-env flag allowlist;
// the non-secret DISCORD_* IDs ride FORWARDED_DISCORD_IDS there (TOG-19025).
const APPLICATION_ID_KEY = "DISCORD_APPLICATION_ID" as const;

/** Readonly view of the secrets/vars the DO forwards into the container. */
function containerEnvVars(env: Env, port: number): Record<string, string> {
  const vars: Record<string, string> = {
    ...forwardedFlagVars(env),
    ...forwardedDiscordIdVars(env),
  };
  if (env.DISCORD_TOKEN) vars["DISCORD_TOKEN"] = env.DISCORD_TOKEN;
  if (env.DATABASE_URL) vars["DATABASE_URL"] = env.DATABASE_URL;
  if (env.GUILD_ID) vars["GUILD_ID"] = env.GUILD_ID;
  // Pass gates unchanged; only the bot decides whether an exact "1" enables them.
  if (env.TWO_AUTOMATIONS !== undefined) {
    vars["TWO_AUTOMATIONS"] = env.TWO_AUTOMATIONS;
  }
  if (env.TWO_TEXT_COMMANDS !== undefined) {
    vars["TWO_TEXT_COMMANDS"] = env.TWO_TEXT_COMMANDS;
  }
  // Missing moderation configuration must stay missing, not become disabled.
  if (env.TWO_AUTOMOD !== undefined) {
    vars["TWO_AUTOMOD"] = env.TWO_AUTOMOD;
  }
  // Application ID is a public snowflake, not a secret: forward it only when
  // it parses as one, so a typo cannot fail the command registry sync closed
  // with a confusing error. The session lobby ID rides FORWARDED_DISCORD_IDS
  // above (already merged and validated).
  const applicationId = env[APPLICATION_ID_KEY];
  if (typeof applicationId === "string" && isSnowflake(applicationId)) {
    vars[APPLICATION_ID_KEY] = applicationId;
  }
  vars["LISTEN_ADDR"] = `0.0.0.0:${port}`;
  // Private internal-actions receiver (TOG-12980, bind TOG-16851). Dark unless
  // the Operator sets TWO_INTERNAL_ACTIONS to exactly "1"; any other value
  // forwards nothing, so a typo cannot crash-loop the gateway (the receiver
  // boots all-or-nothing). The bind and the container marker are the Worker's
  // constants, never Operator-set values.
  if (receiverEnabled(env)) {
    if (port === RECEIVER_PORT) throw new Error("BOT_PORT must differ from the internal-actions receiver port");
    vars["TWO_INTERNAL_ACTIONS"] = "1";
    vars["TWO_INTERNAL_BIND"] = RECEIVER_BIND;
    vars[CONTAINER_MARKER] = CONTAINER_MARKER_VALUE;
    if (env.TWO_INTERNAL_CALLERS) vars["TWO_INTERNAL_CALLERS"] = env.TWO_INTERNAL_CALLERS;
    if (env.TWO_INTERNAL_CHANNEL_KEYS) vars["TWO_INTERNAL_CHANNEL_KEYS"] = env.TWO_INTERNAL_CHANNEL_KEYS;
    if (env.TWO_INTERNAL_KEYS) vars["TWO_INTERNAL_KEYS"] = env.TWO_INTERNAL_KEYS;
  }
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
    // The receiver port joins startup port checks only while the receiver is
    // enabled. Dark never touches the property, so the SDK resolves exactly
    // the ports it does on base main (`requiredPorts` stays undefined).
    if (receiverEnabled(env)) this.requiredPorts = [this.defaultPort, RECEIVER_PORT];
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
    if (url.pathname === ACTIONS_PATH) return this.internalActions(request);
    if (url.pathname === OPS_CONFIG_PATH || url.pathname === OPS_KILLSWITCH_PATH || url.pathname === OPS_AUDIT_PATH) {
      return this.opsControl(request);
    }
    if (url.pathname !== "/health" && url.pathname !== "/readyz" && url.pathname !== OPS_METRICS_PATH
      && !OPS_JSON_PATHS.includes(url.pathname)) {
      return new Response("not found", { status: 404, headers: withSecurityHeaders() });
    }
    try {
      return await this.owned(async () => {
        await this.ownership.require(this.id(), request.headers.get(DEPLOYMENT_HEADER) ?? "");
        // Reached only through the Worker's ops gates (see default export).
        if (url.pathname === OPS_METRICS_PATH) {
          await this.armKeepalive();
          return this.proxyOpsBody("http://c/metrics", OPS_METRICS_CONTENT_TYPE, "metrics unavailable\n");
        }
        // The summary roll-up serves the scrape bearer and the dashboard page
        // alike. Readiness is meaningful data for the dashboard, so the
        // container's answer proxies with a readiness status: the roll-up
        // always answers 200 with a `ready` bit, which becomes the 503 the
        // dashboard renders — while a 503 JSON answer (or any 200) proxies
        // with its own status, and anything else is a 504 with a generic
        // body. Container error text never crosses the boundary.
        if (url.pathname === OPS_SUMMARY_PATH) {
          await this.armKeepalive();
          const fetchSignal = AbortSignal.timeout(SUMMARY_FETCH_TIMEOUT_MS);
          const summaryUnavailable = (status: number) =>
            new Response("summary unavailable\n", {
              status,
              headers: withSecurityHeaders({ "content-type": "text/plain", "cache-control": "no-store" }),
            });
          let upstream: Response;
          try {
            upstream = await this.containerFetch(`http://c${OPS_SUMMARY_PATH}`, {
              signal: fetchSignal,
            });
          } catch {
            return summaryUnavailable(504);
          }
          if (fetchSignal.aborted || !isBotProbeResponse(upstream)) {
            await upstream.body?.cancel().catch(() => {});
            return summaryUnavailable(504);
          }
          let body: string | null;
          try {
            body = await readBoundedText(upstream, MAX_SUMMARY_BODY_BYTES);
          } catch {
            return summaryUnavailable(504);
          }
          if (body === null) {
            return summaryUnavailable(502);
          }
          let status = upstream.status;
          if (status === 200) {
            try {
              const parsed: unknown = JSON.parse(body);
              if (typeof parsed === "object" && parsed !== null && "ready" in parsed
                && (parsed as { ready: unknown }).ready === false) {
                status = 503;
              }
            } catch {
              // Not JSON the roll-up never sends: proxy untouched rather
              // than invent a failure the container did not report.
            }
          }
          return new Response(body, {
            status,
            headers: withSecurityHeaders({ "content-type": "application/json", "cache-control": "no-store" }),
          });
        }
        if (url.pathname === OPS_GATEWAY_PATH || url.pathname === OPS_BACKFILL_PATH) {
          await this.armKeepalive();
          return this.proxyOpsBody(`http://c${url.pathname}`, OPS_JSON_CONTENT_TYPE, "ops unavailable\n");
        }
        // Bound the entire probe, including auto-start, below the 30s DO gate.
        const probe = new Request(request, { signal: AbortSignal.timeout(6000) });
        try {
          const response = await this.containerFetch(probe);
          // SDK startup failures arrive as responses rather than rejections.
          // Bound the discard like the keepalive read: small error text drains
          // so the SDK proxy pipe settles, oversize cancels via the reader.
          if (!isBotProbeResponse(response)) {
            await readBoundedText(response, MAX_PROBE_BODY_BYTES).catch(() => null);
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

  /**
   * Bounded proxy for the read-only ops surfaces (Prometheus exposition and
   * the JSON roll-ups). The SDK resolves fetch failures (including the abort
   * timeout) as a 500/503/429 Response, so a catch alone never fires. Treat
   * an aborted signal or any non-2xx upstream as a 504 with a generic body,
   * and drain the SDK error text instead of proxying it. `unavailableBody`
   * is the only body a failure ever returns, so container error text cannot
   * cross the boundary.
   */
  private async proxyOpsBody(containerUrl: string, contentType: string, unavailableBody: string): Promise<Response> {
    const fetchSignal = AbortSignal.timeout(METRICS_FETCH_TIMEOUT_MS);
    const unavailable = (status: number) =>
      new Response(unavailableBody, {
        status,
        headers: withSecurityHeaders({ "content-type": "text/plain", "cache-control": "no-store" }),
      });
    let upstream: Response;
    try {
      upstream = await this.containerFetch(containerUrl, {
        signal: fetchSignal,
      });
    } catch {
      return unavailable(504);
    }
    if (fetchSignal.aborted || !upstream.ok) {
      // Cancel without reading: a huge non-2xx body must not bypass
      // the 64 KiB cap by being drained here.
      await upstream.body?.cancel().catch(() => {});
      return unavailable(504);
    }
    let body: string | null;
    try {
      body = await readBoundedText(upstream, MAX_METRICS_BODY_BYTES);
    } catch {
      return unavailable(504);
    }
    if (body === null) {
      return unavailable(502);
    }
    return new Response(body, {
      status: upstream.status,
      headers: withSecurityHeaders({ "content-type": contentType, "cache-control": "no-store" }),
    });
  }

  /**
   * Staging-only receiver ingress behind the ownership fence. Re-checks the
   * Worker's gate, then forwards the exact bytes to the fixed receiver port.
   * Public ingress never starts the Container: probes and keepalive own that,
   * so an unauthenticated flood cannot cause a cold start.
   */
  private async internalActions(request: Request): Promise<Response> {
    if (!ingressEnabled(this.env) || !isActionsRequest(request)) return notFound();
    const checked = allowlistedHeaders(request.headers);
    if ("refusal" in checked) return checked.refusal;
    const body = await readBody(request);
    if ("refusal" in body) return body.refusal;
    const forward = forwardRequest(
      `http://c${ACTIONS_PATH}`,
      checked.headers,
      body.bytes,
      AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    );
    try {
      return await this.owned(async () => {
        await this.ownership.require(this.id(), request.headers.get(DEPLOYMENT_HEADER) ?? "");
        if (!this.ctx.container?.running) return envelope(503, "unavailable", true);
        try {
          return await relayReceiverResponse(await this.containerFetch(forward, RECEIVER_PORT));
        } catch {
          return envelope(503, "unavailable", true);
        }
      });
    } catch (error) {
      // Logs the closed ownership reason; the caller only sees the envelope.
      refused(error);
      return envelope(503, "unavailable", true);
    }
  }

  private async control(request: Request): Promise<Response> {
    if (!await authenticated(request, this.env.OWNERSHIP_CONTROL_TOKEN)) {
      return new Response("unauthorized", { status: 401, headers: withSecurityHeaders() });
    }
    if (request.method !== "GET" && request.method !== "POST") {
      return new Response("method not allowed", { status: 405, headers: withSecurityHeaders({ allow: "GET, POST" }) });
    }
    const change = request.method === "POST" ? await readChange(request) : null;
    if (request.method === "POST" && !change) {
      return new Response("invalid ownership change", { status: 400, headers: withSecurityHeaders() });
    }
    try {
      return await this.gated(async () => {
        const id = this.id();
        if (request.headers.get(DEPLOYMENT_HEADER) !== id) throw new OwnershipRefused("deployment_mismatch");
        if (!change) {
          const owner = await this.ownership.read();
          // Takeover does not start the gateway. A subsequent owned probe does.
          return Response.json({ deploymentId: id, owner: owner ?? null, running: this.ctx.container?.running ?? false }, {
            headers: withSecurityHeaders({ "cache-control": "no-store" }),
          });
        }
        // Recovery re-runs the full write/destroy/write sequence even for a
        // repeat (force), and only a commit clears the flag. A no-op repeat
        // returns the stored record at the posted epoch while every commit
        // lands exactly one epoch higher, so equality is the no-op signal.
        const owner = await this.ownership.change(id, change, () => this.destroyInactive(), { force: this.recoveryFailed });
        if (change.action === "takeover" && owner.epoch === change.expectedEpoch) {
          console.log(`two-bot ownership takeover already applied at epoch ${owner.epoch}`);
        } else {
          this.recoveryFailed = false;
          console.log(`two-bot ownership change: ${JSON.stringify(owner)}`);
        }
        // Takeover does not start the gateway. A subsequent owned probe does.
        return Response.json({ deploymentId: id, owner, running: this.ctx.container?.running ?? false }, {
          headers: withSecurityHeaders({ "cache-control": "no-store" }),
        });
      });
    } catch (error) { return refused(error); }
  }

  /**
   * Admin control surface (TOG-22862). Defense in depth: the Worker gate
   * already authenticated and checked the write switches, but the DO
   * re-checks both — the DO is the authority for stored state. All state
   * transitions run inside the DO concurrency gate; enforcement in the Rust
   * runtime lands after the CISO review (AC4).
   */
  private async opsControl(request: Request): Promise<Response> {
    if (!await authenticated(request, this.env.OPS_CONTROL_TOKEN)) {
      return new Response("unauthorized", { status: 401, headers: withSecurityHeaders() });
    }
    const url = new URL(request.url);
    const isConfig = url.pathname === OPS_CONFIG_PATH;
    const isKillswitch = url.pathname === OPS_KILLSWITCH_PATH;
    const isAudit = url.pathname === OPS_AUDIT_PATH;
    if (!isOpsPath(request, url.pathname)) {
      return new Response("not found", { status: 404, headers: withSecurityHeaders() });
    }
    if (isConfig && request.method !== "GET" && request.method !== "PUT") {
      return new Response("method not allowed", { status: 405, headers: withSecurityHeaders({ allow: "GET, PUT" }) });
    }
    if (isKillswitch && request.method !== "GET" && request.method !== "POST") {
      return new Response("method not allowed", { status: 405, headers: withSecurityHeaders({ allow: "GET, POST" }) });
    }
    if (isAudit && request.method !== "GET") {
      return new Response("method not allowed", { status: 405, headers: withSecurityHeaders({ allow: "GET" }) });
    }
    if (isConfig && request.method === "PUT" && !configWritesEnabled(this.env)) {
      return new Response("not found", { status: 404, headers: withSecurityHeaders() });
    }
    if (isKillswitch && request.method === "POST" && !killswitchEnabled(this.env)) {
      return new Response("not found", { status: 404, headers: withSecurityHeaders() });
    }
    let put: unknown = null;
    if ((isConfig && request.method === "PUT") || (isKillswitch && request.method === "POST")) {
      put = await readOpsBody(request);
      if (put === null || put === undefined) {
        return new Response("invalid ops change", { status: 400, headers: withSecurityHeaders() });
      }
    }
    try {
      return await this.gated(async () => {
        const id = this.id();
        if (request.headers.get(DEPLOYMENT_HEADER) !== id) throw new OwnershipRefused("deployment_mismatch");
        const ops = new OpsControl(this.ctx.storage, this.env as unknown as Record<string, unknown>);
        if (isConfig && request.method === "GET") {
          const config = await ops.readConfig();
          return Response.json({ deploymentId: id, ...config }, {
            headers: withSecurityHeaders({ "cache-control": "no-store" }),
          });
        }
        if (isConfig) {
          const change = parseConfigPut(put);
          if (!change) {
            return new Response("invalid ops change", { status: 400, headers: withSecurityHeaders() });
          }
          const record = await ops.putConfig(change);
          console.log(JSON.stringify({ event: "ops_config_put", epoch: record.epoch, actor: record.actor }));
          return Response.json({ deploymentId: id, record, enforcement: OPS_ENFORCEMENT_PENDING }, {
            headers: withSecurityHeaders({ "cache-control": "no-store" }),
          });
        }
        if (isKillswitch && request.method === "GET") {
          const record = await ops.readKillswitch();
          return Response.json({ deploymentId: id, record, enforcement: OPS_ENFORCEMENT_PENDING }, {
            headers: withSecurityHeaders({ "cache-control": "no-store" }),
          });
        }
        if (isKillswitch) {
          const change = parseKillswitchPost(put);
          if (!change) {
            return new Response("invalid ops change", { status: 400, headers: withSecurityHeaders() });
          }
          const record = await ops.postKillswitch(change);
          console.log(JSON.stringify({ event: "ops_kill_switch", epoch: record.epoch, actor: record.actor }));
          return Response.json({ deploymentId: id, record, enforcement: OPS_ENFORCEMENT_PENDING }, {
            headers: withSecurityHeaders({ "cache-control": "no-store" }),
          });
        }
        const rows = await ops.listAudit();
        return Response.json({ deploymentId: id, rows }, {
          headers: withSecurityHeaders({ "cache-control": "no-store" }),
        });
      });
    } catch (error) {
      if (error instanceof OwnershipRefused) return refused(error);
      return opsRefused(error);
    }
  }

  // SDK 0.3.7 containerFetch auto-starts via startAndWaitForPorts, NOT start.
  // Guard all three supported entries, including direct binding/RPC calls.
  override async start(...args: Parameters<Container<Env>["start"]>): Promise<void> {
    return this.owned(() => super.start(...args));
  }

  override async startAndWaitForPorts(...args: Parameters<Container<Env>["startAndWaitForPorts"]>): Promise<void> {
    // containerFetch always names one port. While the receiver is enabled its
    // port is checked with it; the explicit port stays first for the SDK.
    const [ports, ...rest] = args;
    const checked = typeof ports === "number" && receiverEnabled(this.env)
      ? [...new Set([ports, this.defaultPort ?? ports, RECEIVER_PORT])]
      : ports;
    return this.owned(() => super.startAndWaitForPorts(checked, ...rest));
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
            // Cap the keepalive /readyz read like the metrics scrape: a
            // compromised container controls this body, so an oversized answer
            // is a failed probe, never a buffered parse. Small bodies still
            // drain fully, so the SDK 0.3.7 proxy pipe settles cleanly.
            const body = await readBoundedText(res, MAX_PROBE_BODY_BYTES);
            if (body === null) {
              status = null;
              console.warn("two-bot keepalive probe failed");
            } else {
              const failure = isBotProbeResponse(res) ? gatewayFailure(body) : null;
              if (failure) {
                console.warn(JSON.stringify({
                  event: "container_gateway_failure", phase: failure.phase, class: failure.class,
                }));
              }
            }
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
      const previous = (await this.ctx.storage.get<MetricsAlertState>(METRICS_ALERT_KEY)) ?? EMPTY_STATE;
      let samples;
      try {
        const signal = AbortSignal.timeout(METRICS_FETCH_TIMEOUT_MS);
        const request = this.containerFetch("http://c/metrics", { signal }).then((res) => {
          // Release even a late SDK response after the deadline has already won.
          if (signal.aborted) void res.body?.cancel().catch(() => {});
          return res;
        });
        const res = await abortable(request, signal);
        if (!signal.aborted && res.ok) {
          const body = await readBoundedText(res, MAX_METRICS_BODY_BYTES, signal);
          samples = body === null ? null : parseAlertScrape(body);
        } else {
          // Release a failed body without unbounded buffering; the failure path
          // below preserves firing state.
          void res.body?.cancel().catch(() => {});
        }
        if (signal.aborted) samples = null;
      } catch {
        samples = null;
      }
      if (!samples) {
        // A failed scrape is not recovery, nor part of a consecutive growth
        // window. Preserve all firing keys and other rules' existing state.
        console.warn("two-bot metrics scrape failed");
        await this.ctx.storage.put(METRICS_ALERT_KEY, interruptDispatchDrops(previous));
        return;
      }
      const { firing, state } = evaluateMetrics(samples, previous, Date.now() / 1000);
      // Persist before notifying: at most one attempt per transition.
      await this.ctx.storage.put(METRICS_ALERT_KEY, state);
      for (const content of transitionMessages(previous.firing, firing, previous.jobErrorClasses, state.jobErrorClasses)) {
        console.warn(JSON.stringify({ event: "metrics_alert", service: "two-bot-next", content }));
        await this.postWebhookText(content);
      }
    } catch {
      console.warn("two-bot metrics scrape failed");
    }
  }

  private async postWebhookText(content: string): Promise<void> {
    if (this.env.OPS_ALERT_FORWARDING !== "on") return;
    const binding = this.env.OPS_ALERT_WEBHOOK_URL;
    if (!binding) return;
    try {
      const url = opsAlertWebhookUrl(binding);
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
    if (this.env.OPS_ALERT_FORWARDING !== "on") return;
    const binding = this.env.OPS_ALERT_WEBHOOK_URL;
    if (!binding) return;
    try {
      const url = opsAlertWebhookUrl(binding);
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
        return new Response("unauthorized", { status: 401, headers: withSecurityHeaders() });
      }
      try {
        const forwarded = new Request(request);
        // A stale Worker sharing this namespace cannot impersonate the current
        // DO version; always overwrite untrusted ingress identity.
        forwarded.headers.set(DEPLOYMENT_HEADER, deploymentId(env.CF_VERSION_METADATA?.id));
        return await env.TWO_BOT.getByName(SINGLETON_NAME).fetch(forwarded);
      } catch (error) { return refused(error); }
    }

    // Staging-only receiver ingress. Dark (no match here, today's behavior)
    // unless INTERNAL_ACTIONS_INGRESS and TWO_INTERNAL_ACTIONS are both "1".
    if (url.pathname === ACTIONS_PATH && ingressEnabled(env)) {
      return handleActionsIngress(
        request,
        request.headers.get("cf-connecting-ip") ?? "unknown",
        async (sanitized) => {
          // Stamp the fence id server-side; a caller-supplied one was already
          // dropped by the header allowlist.
          sanitized.headers.set(DEPLOYMENT_HEADER, deploymentId(env.CF_VERSION_METADATA?.id));
          return env.TWO_BOT.getByName(SINGLETON_NAME).fetch(sanitized);
        },
      );
    }

    // Admin control surface (TOG-22862). The gate authenticates, enforces the
    // write switches and rate-caps guessing; the fenced DO owns state below.
    // CONTROL_PATH keeps its own gate and shares nothing here.
    if (url.pathname === OPS_CONFIG_PATH || url.pathname === OPS_KILLSWITCH_PATH || url.pathname === OPS_AUDIT_PATH) {
      const gate = await gateOpsRequest(request, env);
      if (gate) return gate;
      try {
        const forwarded = new Request(request);
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
          headers: withSecurityHeaders({ allow: "GET, HEAD" }),
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
          headers: withSecurityHeaders({
            "content-type": "text/plain",
            "retry-after": String(probe.retryAfter),
          }),
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
        // Cancel without reading: a huge non-bot body must not bypass the
        // probe cap by being drained here.
        if (!isBotProbeResponse(upstream)) {
          await upstream.body?.cancel().catch(() => {});
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
      // Harden the Worker-owned edge copy; body and status pass through.
      applySecurityHeaders(result.headers);
      return result;
    }

    // Authenticated off-container scrape paths: /ops/metrics plus the
    // scrape-only JSON roll-ups (/ops/gateway, /ops/backfill). /ops/summary
    // has its own dual gate below (scrape bearer or dashboard pair), so it
    // is matched there, not here. No configured token (or one shorter than
    // the 32-character floor — same bar as the ownership control token) →
    // 404 (the route does not exist); missing/wrong bearer → 401, with
    // per-caller throttling of attempts (429 + retry-after). Every attempt
    // takes a token synchronously before the secret comparison, so
    // concurrent guesses cannot share one token; a throttled caller is
    // refused without any comparison, so guessing cannot confirm a bearer
    // while exhausted. Buckets are per caller, so someone else's guessing
    // cannot throttle a correct bearer — and a caller shed only because the
    // caller table is full is still compared, so a scanner flood cannot lock
    // out the authenticated scraper. Exact paths only: unknown /ops/*
    // subpaths fall through to the redirect lookup below.
    // CONTROL_PATH keeps its own gate (ownership.ts authenticated()): it
    // shares neither this bucket nor its budget.
    if (url.pathname === OPS_METRICS_PATH
      || url.pathname === OPS_GATEWAY_PATH || url.pathname === OPS_BACKFILL_PATH) {
      const refusal = await opsReadRefusal(request, env);
      if (refusal) return refusal;
      try {
        // The fenced DO route requires the deployment header (see fetch);
        // stamp it here like the health/readyz forward above.
        const forwarded = new Request(request);
        forwarded.headers.set(DEPLOYMENT_HEADER, deploymentId(env.CF_VERSION_METADATA?.id));
        return await env.TWO_BOT.getByName(SINGLETON_NAME).fetch(forwarded);
      } catch (error) { return refused(error); }
    }

    // Read-only ops dashboard (TOG-22861). The static page holds no data
    // and is ungated so a browser can open it. The page serves static HTML
    // (no container touch).
    if (url.pathname === OPS_DASHBOARD_PATH) {
      if (request.method !== "GET") {
        return new Response("not found", { status: 404, headers: withSecurityHeaders() });
      }
      return dashboardPageResponse();
    }
    // Read-only ops summary (TOG-22860 + TOG-22861): the roll-up serves two
    // operator audiences, so either credential opens it — the metrics scrape
    // bearer or the dashboard service-token pair (Cloudflare Access service
    // tokens: both CF-Access-Client-Id and CF-Access-Client-Secret on every
    // summary request, strict mode — no cookies or sessions). The scrape
    // gate is consulted first and its 429 is a hard fence, never bypassed
    // via the other gate; a non-throttled scrape refusal falls through to
    // the dashboard gate. Non-GET and doubly-unconfigured still 404 (the
    // route does not exist); otherwise a failed attempt is 401 with the
    // Bearer challenge. Each gate throttles on its own budget before any
    // comparison, so guessing at one cannot throttle the other.
    if (url.pathname === OPS_SUMMARY_PATH) {
      if (request.method !== "GET") {
        return new Response("not found", { status: 404, headers: withSecurityHeaders() });
      }
      const forwardSummary = async () => {
        // The fenced DO route requires the deployment header (see fetch);
        // stamp it here like the health/readyz forward above.
        const forwarded = new Request(request);
        forwarded.headers.set(DEPLOYMENT_HEADER, deploymentId(env.CF_VERSION_METADATA?.id));
        try {
          return await env.TWO_BOT.getByName(SINGLETON_NAME).fetch(forwarded);
        } catch (error) { return refused(error); }
      };
      const scrapeConfigured = !!env.METRICS_SCRAPE_TOKEN
        && env.METRICS_SCRAPE_TOKEN.length >= MIN_SCRAPE_TOKEN_LENGTH;
      if (scrapeConfigured) {
        const refusal = await opsReadRefusal(request, env);
        if (refusal === null) return forwardSummary();
        if (refusal.status === 429) return refusal;
      }
      const caller = request.headers.get("cf-connecting-ip") ?? "unknown";
      const dashboardConfigured = !!env.OPS_DASHBOARD_CLIENT_ID
        && env.OPS_DASHBOARD_CLIENT_ID.length >= MIN_DASHBOARD_CLIENT_ID_LENGTH
        && !!env.OPS_DASHBOARD_CLIENT_SECRET
        && env.OPS_DASHBOARD_CLIENT_SECRET.length >= MIN_DASHBOARD_SECRET_LENGTH;
      if (dashboardConfigured) {
        const verdict = dashboardAuthBuckets.take(caller);
        if (!verdict.allowed && !dashboardAuthBuckets.peek(caller).allowed) {
          return new Response("slow down\n", {
            status: 429,
            headers: withSecurityHeaders({
              "content-type": "text/plain",
              "retry-after": String(verdict.retryAfter),
            }),
          });
        }
        if ((await dashboardRefusal(request, env)) === null) return forwardSummary();
      }
      if (!scrapeConfigured && !dashboardConfigured) {
        return new Response("not found", { status: 404, headers: withSecurityHeaders() });
      }
      return new Response("unauthorized", {
        status: 401,
        headers: withSecurityHeaders({ "www-authenticate": "Bearer" }),
      });
    }

    // Internal metrics, ops roll-ups, dashboard routes and healthz aliases
    // never become invite campaigns. /ops/metrics, the scrape JSON paths,
    // /ops/dashboard and /ops/summary are exact-matched above, so only
    // unknown /ops/* subpaths reach this lookup, where the slug allowlist
    // rejects them.
    // The exact /healthz redirect probe is handled below after config validation.
    if (url.pathname !== "/healthz" && isReservedInternal(url.pathname)) {
      return new Response("not found", { status: 404, headers: withSecurityHeaders() });
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
        headers: withSecurityHeaders({ "content-type": "text/plain", "retry-after": "30" }),
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
