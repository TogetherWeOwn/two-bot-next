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
  SIGNING_HEADERS,
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
  EVENTSUB_PATH,
  EVENTSUB_RECEIVER_BIND,
  EVENTSUB_RECEIVER_PORT,
  allowlistedHeaders as eventsubAllowlistedHeaders,
  eventsubIngressEnabled,
  eventsubReceiverEnabled,
  handleEventsubIngress,
  isEventsubRequest,
  readBody as readEventsubBody,
  relayEventsubResponse,
  signEventsubRefresh,
} from "./eventsub.ts";
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
  EMPTY_SCRAPE_LOSS,
  EMPTY_STATE,
  LOG_QUERY_URL,
  SCRAPE_LOSS_THRESHOLD,
  alertContextSuffix,
  evaluateMetrics,
  interruptCommunityFactsDrain,
  interruptDispatchDrops,
  interruptAuditHalt,
  interruptPrefixVerdictRefusals,
  sanitizeScrapeLossState,
  scrapeLossFiringMessage,
  scrapeLossResolvedMessage,
  transitionMessages,
  type AlertContext,
  type MetricsAlertState,
  type ScrapeLossState,
} from "./alert-rules.ts";
import { parseAlertScrape } from "./metrics-scrape.ts";
import {
  buildRemediationGuardReceipt,
  evaluateRemediation,
  interruptRemediationHoldDown,
  remediationTriggerFiring,
  sanitizeRemediationState,
  type RemediationState,
} from "./remediation.ts";
import { applyStagingFaultInjection } from "./staging-fault.ts";
import { parseRelease, releaseTag, workerVersionTag } from "./error-events.ts";
import { applySecurityHeaders, withSecurityHeaders } from "./security-headers.ts";
import { REQUEST_ID_HEADER, ensureRequestId } from "./request-id.ts";
import { handleMcp, MCP_AUDIT_READ_HEADER } from "./mcp.ts";
import {
  DASHBOARD_CLIENT_ID_HEADER,
  DASHBOARD_CLIENT_SECRET_HEADER,
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
  /** Consecutive failed probes; default covers ~2 minutes of keepalive ticks. */
  UNREADY_ALERT_FAILURES?: string;
  /** Optional Worker secret; never forwarded to the container or logged. */
  OPS_ALERT_WEBHOOK_URL?: string;
  /** Worker-only forwarding switch. Only "on" posts; all other values are log-only. */
  OPS_ALERT_FORWARDING?: string;
  /** Optional Worker secret: bearer token for GET /ops/metrics and the
   * read-only /ops JSON roll-ups below. Unset → those routes 404. */
  METRICS_SCRAPE_TOKEN?: string;
  /** Optional Worker secret: bearer token for POST /mcp (`mcp:read`). Unset → route 404s. */
  MCP_TOKEN?: string;
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
  /** Operator-set Worker var: full GET URL of the /ops/config record the
   * container polls on every settings tick (Slice 0a). Forwarded on its own
   * explicit containerEnvVars line when set, never flag-forwarded (`_URL`
   * names cannot join FORWARDED_FLAGS). */
  TWO_OPS_CONFIG_URL?: string;
  /** Optional Hyperdrive binding to shared Postgres; absent → snapshot, clicks dropped. */
  REDIRECT_DB?: Hyperdrive;
  /** Invite code for `/` and DB outages. Optional but recommended. */
  REDIRECT_FALLBACK_CODE?: string;
  /** JSON snapshot of invite_campaigns rows (mapping source without REDIRECT_DB). */
  REDIRECT_MAPPINGS_JSON?: string;
  /** Staging-only var (wrangler.toml [env.staging.vars]); see internal-actions.ts. */
  INTERNAL_ACTIONS_INGRESS?: string;
  /** Staging-only var (wrangler.toml [env.staging.vars]); see eventsub.ts. */
  EVENTSUB_INGRESS?: string;
  /**
   * Deploy-stamped container Git SHA for Worker-edge span lines. Set only in
   * the generated deploy configs (scripts/staging_rollout.py,
   * scripts/production_deploy.py), never a committed var. Anything that is
   * not a hex SHA logs as the visible `"unknown"` bucket. Never forwarded to
   * the container or logged verbatim elsewhere.
   */
  CONTAINER_RELEASE?: string;
  /** Operator-set Worker secret enabling the EventSub forward. Never flag-forwarded. */
  TWO_EVENTSUB?: string;
  /** Operator-set Worker secret carrying the Twitch signing secret. Never flag-forwarded. */
  TWO_TWITCH_WEBHOOK_SECRET?: string;
  /** Operator-set Worker secret: HMAC key authenticating the per-delivery
   * secret refresh the DO attaches for no-restart rotation. Absent means
   * rotation stays disarmed (legacy startup-secret behavior). Never
   * flag-forwarded, never logged. */
  TWO_EVENTSUB_RELOAD_KEY?: string;
  /** Operator-set reconcile callback URL. Forwarded on its own explicit containerEnvVars line while the EventSub receiver is forwarded, never flag-forwarded. */
  TWO_TWITCH_CALLBACK_URL?: string;
  /** Operator-set reconcile broadcaster allowlist. Forwarded on its own explicit containerEnvVars line while the EventSub receiver is forwarded, never flag-forwarded. */
  TWO_TWITCH_BROADCASTERS?: string;
  /** Operator-set Helix client id. Forwarded on its own explicit containerEnvVars line while the EventSub receiver is forwarded, never flag-forwarded. */
  TWO_TWITCH_CLIENT_ID?: string;
  /** Operator-set Helix client secret. Forwarded on its own explicit containerEnvVars line while the EventSub receiver is forwarded, never flag-forwarded. */
  TWO_TWITCH_CLIENT_SECRET?: string;
  /** Operator-set go-live delivery gate. Forwarded as exactly "1" on its own explicit containerEnvVars line while the EventSub receiver is forwarded, never flag-forwarded. */
  TWO_EVENTSUB_DELIVERY?: string;
  /** Operator-set announcements channel for go-live posts. Forwarded on its own explicit containerEnvVars line while the delivery gate travels, never flag-forwarded. */
  TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID?: string;
  /** Operator-set Discord invite URL for go-live posts. Forwarded on its own explicit containerEnvVars line while the delivery gate travels, never flag-forwarded. */
  TWO_EVENTSUB_INVITE_URL?: string;
  /** Staging rollback-drill switch (docs/staging-rollout-gate.md). Set only
   * in the generated staging deploy config by a manual deploy-staging
   * dispatch, never a committed var (check-env-bindings.py). Reaches the
   * Container on an explicit fenced line below, never flag-forwarded. The
   * TWO_DEPLOY_ENV marker it is fenced by rides FORWARDED_FLAGS. */
  TWO_GATEWAY_READINESS_FAIL_INJECTION?: string;
  /** Staging remediation-drill switches (staging fault injector): Worker-only
   * trigger input for the guarded auto-remediation soak. Set only in the
   * generated staging deploy config by a manual deploy-staging dispatch,
   * never a committed var (check-env-bindings.py). Never forwarded to the
   * Container (NOT_FORWARDED in container-env.ts) and never logged. */
  TWO_VOICE_STALL_FAULT_INJECTION?: string;
  TWO_GATEWAY_UNPAIRED_FAULT_INJECTION?: string;
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
// MCP bearer gate (phase 1: shared `mcp:read` bearer; DCR/PKCE is the
// staging target in docs/mcp.md). Same brute-force shape as the scrape gate:
// budget is taken synchronously before the digest comparison, and an
// exhausted caller is refused without any comparison.
const mcpAuthBuckets = new TokenBuckets({ capacity: 10, refillPerSecond: 1 });
let mcpShortTokenLogged = false;
// A short scrape token is an operator misconfiguration, not per-request
// information: say so once per isolate so scanners cannot flood the logs.
let metricsShortTokenLogged = false;

// Attempt cap for the dashboard service-token gate. Separate budget from the
// metrics scrape so guessing at one cannot throttle the other; otherwise the
// same take-before-compare semantics (see metricsAuthBuckets above).
const dashboardAuthBuckets = new TokenBuckets({ capacity: 10, refillPerSecond: 1 });

// Attempt cap for the signed backfill-progress read (TOG-22953). The
// container owns the x-two-* HMAC, so the Worker cannot validate the
// signature — but unauthenticated floods must still be cut before the
// singleton DO's concurrency gate (armKeepalive + container auto-start wait up
// to 6 s per request inside blockConcurrencyWhile). Same take-before-check
// shape as the scrape gate: one token per attempt, 429 + retry-after when
// exhausted, so a flood of one-time scanners can never lock out operators.
const backfillProgressBuckets = new TokenBuckets({ capacity: 10, refillPerSecond: 1 });

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

interface KeepaliveNotification {
  content: string;
  readinessEvent?: ReadinessEvent;
}

// Two sequential six-second probes plus at most six seconds for ALL forwarding
// leave twelve seconds for ownership/storage/scheduling in the 30-second gate.
// Clip forwarding further if essential work has already used that headroom.
const NOTIFICATION_TIMEOUT_MS = 6000;
const KEEPALIVE_FORWARDING_CUTOFF_MS = 24_000;

const SINGLETON_NAME = "two-bot";
const DEFAULT_KEEPALIVE_SECONDS = 60;
const DEFAULT_UNREADY_SECONDS = 120;
const READINESS_KEY = "two-bot:readiness";
const METRICS_ALERT_KEY = "two-bot:metrics-alerts";
const SCRAPE_LOSS_KEY = "two-bot:metrics-scrape-loss";
const REMEDIATION_KEY = "two-bot:remediation";
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
// Signed backfill-progress read (TOG-22953, slice 8 of TOG-22852): the
// container owns the x-two-* HMAC, so this path never takes the scrape
// bearer — the Worker forwards it with headers unchanged (see the gate
// below) and the DO proxies it like the other JSON roll-ups.
const OPS_BACKFILL_PROGRESS_PATH = "/ops/backfill/progress";
const OPS_JSON_PATHS: string[] = [OPS_SUMMARY_PATH, OPS_GATEWAY_PATH, OPS_BACKFILL_PATH, OPS_BACKFILL_PROGRESS_PATH];
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
        release: releaseTag(null),
        worker_version: workerVersionTag(env.CF_VERSION_METADATA?.id),
        log_url: LOG_QUERY_URL,
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

// Edge pre-check for the signed backfill-progress read (TOG-22953). The
// signature itself is validated only in the container; this only keeps
// unsigned or malformed floods from reaching the singleton DO. Refuses with
// the route's indistinguishable 401 when any x-two-* header is missing, empty
// or over its byte cap (caps reused from SIGNING_HEADERS, the receiver's own
// limits — no Bearer challenge: this route never takes a bearer). Returns the
// refusal, or null when the headers are present and within cap.
function backfillProgressRefusal(request: Request): Response | null {
  for (const [name, max] of SIGNING_HEADERS) {
    const value = request.headers.get(name);
    if (value === null || value === "" || value.length > max) {
      return new Response("unauthorized", { status: 401, headers: withSecurityHeaders() });
    }
  }
  return null;
}

// SDK failures do not carry a trustworthy Rust startup class. Never serialize
// arbitrary exception text (or claim stderr crossed the Container boundary).
function containerUnavailable(workerVersionId: unknown): Response {
  console.error(JSON.stringify({
    event: "container_probe_failed",
    error_class: "container_unavailable",
    release: releaseTag(null),
    worker_version: workerVersionTag(workerVersionId),
    log_url: LOG_QUERY_URL,
  }));
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

// While the gateway task is failing, the bot serves its fixed phase/class
// plus the failing step and SQLSTATE code on /readyz for a bounded linger.
// Container stdout is not in Workers Logs, so the keepalive re-emits those
// tokens as a structured warn alongside the container build revision.
// Strict tokens only: nothing else from the probe body is ever logged.
const FAILURE_TOKEN = /^[a-z0-9_]{1,32}$/;
const SQLSTATE_TOKEN = /^(none|[0-9A-Z]{5})$/;
const BUILD_TOKEN = /^[A-Za-z0-9._-]{1,64}$/;
// Deploy IDs are `YYYY.MM.DD.N+sha7` (scripts/deploy_id.py): the `+` needs
// its own token, since BUILD_TOKEN excludes it. Shape-only here; the deploy
// gates own canonical validation, this only gates log echo.
const DEPLOY_ID_TOKEN = /^\d{4}\.\d{2}\.\d{2}\.\d{1,6}\+[0-9a-f]{7}$/;
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

/** Release a body best-effort: a stalling cancel hook must not hold the caller. */
function releaseBody(response: Response): void {
  void response.body?.cancel().catch(() => {});
}

/**
 * Settle a container fetch at `signal`'s deadline even when the SDK ignores
 * abort. A response that arrives after the deadline is released best-effort
 * and never delivered to the caller.
 */
async function deadlineFetch(work: Promise<Response>, signal: AbortSignal): Promise<Response> {
  const request = work.then((res) => {
    if (signal.aborted) releaseBody(res);
    return res;
  });
  return abortable(request, signal);
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

export interface ProbeDetail {
  failure: { phase: string; class: string; failedStep?: string; errorCode?: string } | null;
  buildRevision?: string;
  buildId?: string;
  deployId?: string;
  /**
   * The /readyz gateway component: true is `ready` (Connected), false is
   * `starting` (Armed) or `down` (Unconfigured/Draining). Absent unless the
   * body parsed as a bot report naming the gateway component; the stuck-
   * gateway rule holds on an absent value, never guessing.
   */
  gatewayReady?: boolean;
}

/**
 * The /readyz gateway component status (`readiness_report` in
 * crates/bot/src/server.rs emits `components` as `[name, status]` pairs with
 * the `ready`/`starting`/`down` vocabulary from crates/core/src/health.rs),
 * or undefined when the value is not a parseable bot report naming the
 * gateway component. Only `ready` counts as ready; unknown vocabulary holds
 * rather than resolving or firing the stuck-gateway rule, so a future status
 * never silently flips it.
 */
export function gatewayComponentReady(components: unknown): boolean | undefined {
  if (!Array.isArray(components)) return undefined;
  for (const entry of components) {
    if (!Array.isArray(entry) || entry.length !== 2) return undefined;
    const [name, status] = entry;
    if (typeof name !== "string" || typeof status !== "string") return undefined;
    if (name === "gateway") {
      if (status === "ready") return true;
      if (status === "starting" || status === "down") return false;
      return undefined;
    }
  }
  return undefined;
}

function probeDetail(body: string): ProbeDetail {
  if (new TextEncoder().encode(body).byteLength > MAX_PROBE_BODY_BYTES) return { failure: null };
  try {
    const report = JSON.parse(body) as {
      gateway_failure?: unknown;
      build_revision?: unknown;
      build_id?: unknown;
      deploy_id?: unknown;
    } | null;
    let failure: ProbeDetail["failure"] = null;
    const field = report?.gateway_failure;
    if (typeof field === "object" && field !== null) {
      const { phase, class: errorClass, failed_step, error_code } = field as Record<string, unknown>;
      if (typeof phase === "string" && typeof errorClass === "string"
        && FAILURE_TOKEN.test(phase) && FAILURE_TOKEN.test(errorClass)) {
        failure = { phase, class: errorClass };
        if (typeof failed_step === "string" && FAILURE_TOKEN.test(failed_step)) {
          failure.failedStep = failed_step;
        }
        if (typeof error_code === "string" && SQLSTATE_TOKEN.test(error_code)) {
          failure.errorCode = error_code;
        }
      }
    }
    const detail: ProbeDetail = { failure };
    const gatewayReady = gatewayComponentReady((report as { components?: unknown } | null)?.components);
    if (gatewayReady !== undefined) detail.gatewayReady = gatewayReady;
    if (typeof report?.build_revision === "string" && BUILD_TOKEN.test(report.build_revision)) {
      detail.buildRevision = report.build_revision;
    }
    if (typeof report?.build_id === "string" && BUILD_TOKEN.test(report.build_id)) {
      detail.buildId = report.build_id;
    }
    if (typeof report?.deploy_id === "string" && DEPLOY_ID_TOKEN.test(report.deploy_id)) {
      detail.deployId = report.deploy_id;
    }
    return detail;
  } catch {
    // Not the bot's JSON: nothing to report.
    return { failure: null };
  }
}

function gatewayFailure(body: string): { phase: string; class: string } | null {
  return probeDetail(body).failure;
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

/**
 * The private EventSub receiver boots all-or-nothing: the Worker forwards the
 * enable flag only with the signing secret present. Without the secret nothing
 * is forwarded, so a half-provisioned enable stays dark instead of
 * crash-looping the shared container on a fatal missing-secret startup.
 */
function eventsubReceiverForwarded(env: Env): boolean {
  return eventsubReceiverEnabled(env) && !!env.TWO_TWITCH_WEBHOOK_SECRET;
}

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
  // Staging rollback drill (docs/staging-rollout-gate.md): the readiness
  // failure switch reaches the Container only under the staging deploy-env
  // marker with the exact "1" value; the Rust fence repeats both checks, so
  // a stray binding on any other Worker forwards and activates nothing.
  // Not flag-forwarded (NOT_FORWARDED in container-env.ts).
  if (env.TWO_DEPLOY_ENV === "staging" && env.TWO_GATEWAY_READINESS_FAIL_INJECTION === "1") {
    vars["TWO_GATEWAY_READINESS_FAIL_INJECTION"] = "1";
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
  // Private EventSub receiver. Dark unless the Operator sets TWO_EVENTSUB to
  // exactly "1" with the signing secret present; any other state forwards
  // nothing, so a typo or a missing secret cannot crash-loop the gateway
  // (the receiver boots all-or-nothing). The bind and the container marker
  // are the Worker's constants, never Operator-set values. The signing secret,
  // reconcile inputs and Helix credentials each travel on their own explicit
  // line and are never logged. Absent stays absent so a half-provisioned
  // enable fails loudly in the Container (reconcile_config_invalid) instead
  // of silently running dark.
  if (eventsubReceiverForwarded(env)) {
    if (port === EVENTSUB_RECEIVER_PORT) throw new Error("BOT_PORT must differ from the EventSub receiver port");
    if (port === RECEIVER_PORT) throw new Error("BOT_PORT must differ from the internal-actions receiver port");
    vars["TWO_EVENTSUB"] = "1";
    vars["TWO_EVENTSUB_BIND"] = EVENTSUB_RECEIVER_BIND;
    vars[CONTAINER_MARKER] = CONTAINER_MARKER_VALUE;
    vars["TWO_TWITCH_WEBHOOK_SECRET"] = env.TWO_TWITCH_WEBHOOK_SECRET as string;
    // No-restart rotation arming: only with the receiver forward, on its own
    // explicit line. Absent stays absent, so rotation stays disarmed instead
    // of failing the container boot.
    if (env.TWO_EVENTSUB_RELOAD_KEY) vars["TWO_EVENTSUB_RELOAD_KEY"] = env.TWO_EVENTSUB_RELOAD_KEY;
    if (env.TWO_TWITCH_CALLBACK_URL) vars["TWO_TWITCH_CALLBACK_URL"] = env.TWO_TWITCH_CALLBACK_URL;
    if (env.TWO_TWITCH_BROADCASTERS) vars["TWO_TWITCH_BROADCASTERS"] = env.TWO_TWITCH_BROADCASTERS;
    if (env.TWO_TWITCH_CLIENT_ID) vars["TWO_TWITCH_CLIENT_ID"] = env.TWO_TWITCH_CLIENT_ID;
    if (env.TWO_TWITCH_CLIENT_SECRET) vars["TWO_TWITCH_CLIENT_SECRET"] = env.TWO_TWITCH_CLIENT_SECRET;
    // Go-live delivery is a second gate inside the receiver forward: only
    // exactly "1" travels, with its channel and invite inputs on their own
    // lines. Absent stays absent so a half-provisioned delivery enable
    // fails loudly in the Container instead of silently staying ack-only.
    if (env.TWO_EVENTSUB_DELIVERY === "1") {
      vars["TWO_EVENTSUB_DELIVERY"] = "1";
      if (env.TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID) vars["TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID"] = env.TWO_EVENTSUB_ANNOUNCEMENTS_CHANNEL_ID;
      if (env.TWO_EVENTSUB_INVITE_URL) vars["TWO_EVENTSUB_INVITE_URL"] = env.TWO_EVENTSUB_INVITE_URL;
    }
  }
  // /ops override enforcement (Slice 0a): the container GETs the ops record
  // from this URL on every settings tick. Non-secret but URL-shaped, so it
  // is never flag-forwarded (NOT_FORWARDED in container-env.ts). Absent stays
  // absent and the Rust layer stays disabled. The fetch bearer is the control
  // token's value, forwarded only while the URL is set so existing
  // OPS_CONTROL_TOKEN deployments do not hand the container a token it cannot
  // use; it is never logged.
  if (env.TWO_OPS_CONFIG_URL) vars["TWO_OPS_CONFIG_URL"] = env.TWO_OPS_CONFIG_URL;
  if (env.TWO_OPS_CONFIG_URL && env.OPS_CONTROL_TOKEN) {
    vars["TWO_OPS_CONFIG_TOKEN"] = env.OPS_CONTROL_TOKEN;
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
  // Last container git SHA seen on /readyz this tick. Tags the keepalive
  // error events (gateway failures by class; metrics alerts by rule/content);
  // a failed probe leaves "unknown" rather than a stale release from a replaced image.
  private lastRelease: string | null = null;
  private static readonly KEEPALIVE_KEY = "two-bot:keepalive:v1";

  constructor(ctx: DurableObjectState<{}>, env: Env) {
    super(ctx, env);
    // Each receiver port joins startup port checks only while its receiver is
    // enabled. Dark never touches the property, so the SDK resolves exactly
    // the ports it does on base main (`requiredPorts` stays undefined).
    const wanted = [this.defaultPort];
    if (receiverEnabled(env)) wanted.push(RECEIVER_PORT);
    if (eventsubReceiverForwarded(env)) wanted.push(EVENTSUB_RECEIVER_PORT);
    if (wanted.length > 1) this.requiredPorts = [...new Set(wanted)];
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
    if (url.pathname === EVENTSUB_PATH) return this.internalEventsub(request);
    const mcpAuditRead = url.origin === "http://c" && url.pathname === OPS_AUDIT_PATH
      && !url.search && request.method === "GET" && request.headers.get(MCP_AUDIT_READ_HEADER) === "1";
    if (url.pathname === OPS_CONFIG_PATH || url.pathname === OPS_KILLSWITCH_PATH
      || (url.pathname === OPS_AUDIT_PATH && !mcpAuditRead)) {
      return this.opsControl(request);
    }
    if (!mcpAuditRead && url.pathname !== "/health" && url.pathname !== "/readyz" && url.pathname !== "/version"
      && url.pathname !== OPS_METRICS_PATH && !OPS_JSON_PATHS.includes(url.pathname)) {
      return new Response("not found", { status: 404, headers: withSecurityHeaders() });
    }
    try {
      return await this.owned(async () => {
        await this.ownership.require(this.id(), request.headers.get(DEPLOYMENT_HEADER) ?? "");
        // Reached only through the Worker's ops gates (see default export).
        // MCP's internal dispatch reads the Rust audit, not the DO's admin
        // history at the same path. Never forward the dispatch/caller headers.
        if (mcpAuditRead) {
          return this.proxyOpsBody("http://c/ops/audit", OPS_JSON_CONTENT_TYPE, "audit unavailable\n");
        }
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
            upstream = await deadlineFetch(this.containerFetch(`http://c${OPS_SUMMARY_PATH}`, {
              signal: fetchSignal,
            }), fetchSignal);
          } catch {
            return summaryUnavailable(504);
          }
          if (fetchSignal.aborted || !isBotProbeResponse(upstream)) {
            releaseBody(upstream);
            return summaryUnavailable(504);
          }
          let body: string | null;
          try {
            body = await readBoundedText(upstream, MAX_SUMMARY_BODY_BYTES, fetchSignal);
          } catch {
            return summaryUnavailable(504);
          }
          if (fetchSignal.aborted) {
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
        if (url.pathname === OPS_BACKFILL_PROGRESS_PATH) {
          await this.armKeepalive();
          // The route is container-authenticated (x-two-* HMAC): the SDK
          // builds a bare request from a string URL, so the signing headers
          // must cross explicitly or every call 401s into a 504. Only the
          // allowlisted signing headers cross; the deployment fence header
          // stays at the DO gate.
          const signed = new Headers();
          for (const [name] of SIGNING_HEADERS) {
            const value = request.headers.get(name);
            if (value !== null) signed.set(name, value);
          }
          return this.proxyOpsBody(`http://c${url.pathname}`, OPS_JSON_CONTENT_TYPE, "ops unavailable\n", signed);
        }
        // Bound the entire probe, including auto-start, below the 30s DO gate.
        // The deadline covers the fetch AND rejected-body consumption: a
        // sub-cap stall cannot extend caller settlement, and late or
        // abort-shaped bodies are released best-effort without replacing
        // the fixed refusal.
        const probeSignal = AbortSignal.timeout(6000);
        const probe = new Request(request, { signal: probeSignal });
        let probeResponse: Response;
        try {
          probeResponse = await deadlineFetch(this.containerFetch(probe), probeSignal);
        } catch {
          return containerUnavailable(this.env.CF_VERSION_METADATA?.id);
        }
        if (probeSignal.aborted) {
          releaseBody(probeResponse);
          return containerUnavailable(this.env.CF_VERSION_METADATA?.id);
        }
        try {
          // SDK startup failures arrive as responses rather than rejections.
          // Bound the discard like the keepalive read: small error text drains
          // so the SDK proxy pipe settles, oversize cancels via the reader.
          if (!isBotProbeResponse(probeResponse)) {
            await readBoundedText(probeResponse, MAX_PROBE_BODY_BYTES, probeSignal).catch(() => null);
            return containerUnavailable(this.env.CF_VERSION_METADATA?.id);
          }
          if (probeSignal.aborted) {
            releaseBody(probeResponse);
            return containerUnavailable(this.env.CF_VERSION_METADATA?.id);
          }
          await this.armKeepalive();
          return probeResponse;
        } catch {
          return containerUnavailable(this.env.CF_VERSION_METADATA?.id);
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
   * cross the boundary. When `headers` is given (the signed backfill-progress
   * read), a 401 from the container is the route's own auth failure (bad key,
   * signature, skew, replay) and stays a fixed 401 — never a 504 outage.
   * When `headers` is given, the fetch goes out as a Request carrying them:
   * a string URL builds a bare request and drops container-authenticated
   * headers (x-two-*), turning every signed call into a 504.
   */
  private async proxyOpsBody(containerUrl: string, contentType: string, unavailableBody: string, headers?: Headers): Promise<Response> {
    const fetchSignal = AbortSignal.timeout(METRICS_FETCH_TIMEOUT_MS);
    const unavailable = (status: number) =>
      new Response(unavailableBody, {
        status,
        headers: withSecurityHeaders({ "content-type": "text/plain", "cache-control": "no-store" }),
      });
    let upstream: Response;
    try {
      upstream = headers
        ? await deadlineFetch(this.containerFetch(new Request(containerUrl, { headers, signal: fetchSignal })), fetchSignal)
        : await deadlineFetch(this.containerFetch(containerUrl, {
          signal: fetchSignal,
        }), fetchSignal);
    } catch {
      return unavailable(504);
    }
    if (fetchSignal.aborted) {
      releaseBody(upstream);
      return unavailable(504);
    }
    if (headers && upstream.status === 401) {
      // Signed-read auth failure, not an outage: fixed 401 with the same body
      // the Worker pre-check uses, so container error text cannot leak.
      releaseBody(upstream);
      return new Response("unauthorized", {
        status: 401,
        headers: withSecurityHeaders({ "content-type": "text/plain", "cache-control": "no-store" }),
      });
    }
    if (!upstream.ok) {
      // Cancel without reading: a huge non-2xx body must not bypass
      // the 64 KiB cap by being drained here.
      releaseBody(upstream);
      return unavailable(504);
    }
    let body: string | null;
    try {
      body = await readBoundedText(upstream, MAX_METRICS_BODY_BYTES, fetchSignal);
    } catch {
      return unavailable(504);
    }
    if (fetchSignal.aborted) {
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
    // The Worker's stamped id (or a fresh one): the forward already carries
    // it via the allowlisted headers, and these envelopes echo it so a 503
    // still joins the flow's Worker and container logs.
    const requestId = ensureRequestId(request.headers.get(REQUEST_ID_HEADER));
    const checked = allowlistedHeaders(request.headers, requestId, this.lastRelease);
    if ("refusal" in checked) return checked.refusal;
    const body = await readBody(request, requestId, this.lastRelease);
    if ("refusal" in body) return body.refusal;
    const forward = forwardRequest(
      `http://c${ACTIONS_PATH}`,
      checked.headers,
      body.bytes,
      AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    );
    // The existing receiver deadline on `forward` bounds the fetch AND the
    // body relay below: no larger or independent budget is created. A fetch
    // that ignores abort, a stalled tail, or a never-settling cancel settles
    // here at that deadline; late bodies are released best-effort and never
    // relayed. A timeout refuses only this wrapper call: it never retries the
    // signed action and never proves the receiver stood down.
    const signal = forward.signal;
    try {
      return await this.owned(async () => {
        await this.ownership.require(this.id(), request.headers.get(DEPLOYMENT_HEADER) ?? "");
        if (!this.ctx.container?.running) return envelope(503, "unavailable", true, {}, requestId);
        let upstream: Response;
        try {
          upstream = await deadlineFetch(this.containerFetch(forward, RECEIVER_PORT), signal);
        } catch {
          return envelope(503, "unavailable", true, {}, requestId);
        }
        if (signal.aborted) {
          releaseBody(upstream);
          return envelope(503, "unavailable", true, {}, requestId);
        }
        let relayed: Response;
        try {
          relayed = await relayReceiverResponse(upstream, requestId, signal, this.lastRelease);
        } catch {
          return envelope(503, "unavailable", true, {}, requestId);
        }
        // The relayed body is fully buffered bytes; past the deadline it is
        // simply not served, so there is nothing to release.
        if (signal.aborted) return envelope(503, "unavailable", true, {}, requestId);
        return relayed;
      });
    } catch (error) {
      // Logs the closed ownership reason; the caller only sees the envelope.
      refused(error);
      return envelope(503, "unavailable", true, {}, requestId);
    }
  }

  /**
   * Staging-only EventSub ingress behind the ownership fence. Re-checks the
   * Worker's gate, then forwards the exact bytes to the EventSub receiver
   * port. Public ingress never starts the Container: probes and keepalive own
   * that, so an unauthenticated flood cannot cause a cold start. The private
   * receiver binds 8092 while TWO_EVENTSUB is "1" with the signing secret
   * present; when dark or cold the forward fails closed to the 503 envelope
   * below.
   */
  private async internalEventsub(request: Request): Promise<Response> {
    if (!eventsubIngressEnabled(this.env) || !isEventsubRequest(request)) return notFound();
    // Same join as the actions path above: the forward carries the stamped
    // id, and these envelopes echo it so a 503 still joins the flow's logs.
    const requestId = ensureRequestId(request.headers.get(REQUEST_ID_HEADER));
    const checked = eventsubAllowlistedHeaders(request.headers, requestId, this.lastRelease);
    if ("refusal" in checked) return checked.refusal;
    const body = await readEventsubBody(request, requestId, this.lastRelease);
    if ("refusal" in body) return body.refusal;
    const forward = forwardRequest(
      `http://c${EVENTSUB_PATH}`,
      checked.headers,
      body.bytes,
      AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    );
    // No-restart rotation, when armed: attach the Worker's current secret
    // with its refresh HMAC — all server-side values, bound to this
    // delivery's request id. A missing secret, or a signing failure, fails
    // closed to the fixed envelope (never serialized, never logged).
    const reloadKey = this.env.TWO_EVENTSUB_RELOAD_KEY;
    if (reloadKey) {
      const secret = this.env.TWO_TWITCH_WEBHOOK_SECRET;
      let refresh: Headers | null = null;
      try {
        refresh = secret ? await signEventsubRefresh({ reloadKey, secret, requestId }) : null;
      } catch {
        refresh = null;
      }
      if (!refresh) return envelope(503, "unavailable", true, {}, requestId);
      refresh.forEach((value, name) => forward.headers.set(name, value));
    }
    // Same bound as the actions path above: the existing receiver deadline on
    // `forward` covers the fetch AND the body relay, with no larger or
    // independent budget. Late bodies are released best-effort and never
    // relayed; a timeout never retries the delivery.
    const signal = forward.signal;
    try {
      return await this.owned(async () => {
        await this.ownership.require(this.id(), request.headers.get(DEPLOYMENT_HEADER) ?? "");
        if (!this.ctx.container?.running) return envelope(503, "unavailable", true, {}, requestId);
        let upstream: Response;
        try {
          upstream = await deadlineFetch(this.containerFetch(forward, EVENTSUB_RECEIVER_PORT), signal);
        } catch {
          return envelope(503, "unavailable", true, {}, requestId);
        }
        if (signal.aborted) {
          releaseBody(upstream);
          return envelope(503, "unavailable", true, {}, requestId);
        }
        let relayed: Response;
        try {
          relayed = await relayEventsubResponse(upstream, requestId, signal, this.lastRelease);
        } catch {
          return envelope(503, "unavailable", true, {}, requestId);
        }
        // The relayed body is fully buffered bytes; past the deadline it is
        // simply not served, so there is nothing to release.
        if (signal.aborted) return envelope(503, "unavailable", true, {}, requestId);
        return relayed;
      });
    } catch (error) {
      // Logs the closed ownership reason; the caller only sees the envelope.
      refused(error);
      return envelope(503, "unavailable", true, {}, requestId);
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
        const history = await ops.readAudit(id);
        return Response.json(history, {
          headers: withSecurityHeaders({ "cache-control": "no-store" }),
        });
      });
    } catch (error) {
      if (error instanceof OwnershipRefused) return refused(error);
      // GETs are reads; PUT/POST are mutations with reconcile-then-retry refusals.
      return opsRefused(error, request.method === "GET");
    }
  }

  // SDK 0.3.7 containerFetch auto-starts via startAndWaitForPorts, NOT start.
  // Guard all three supported entries, including direct binding/RPC calls.
  override async start(...args: Parameters<Container<Env>["start"]>): Promise<void> {
    return this.owned(() => super.start(...args));
  }

  override async startAndWaitForPorts(...args: Parameters<Container<Env>["startAndWaitForPorts"]>): Promise<void> {
    // containerFetch always names one port. While a receiver is enabled its
    // port is checked with it; the explicit port stays first for the SDK.
    const [ports, ...rest] = args;
    let checked = ports;
    if (typeof ports === "number" && (receiverEnabled(this.env) || eventsubReceiverForwarded(this.env))) {
      const extra = [ports, this.defaultPort ?? ports];
      if (receiverEnabled(this.env)) extra.push(RECEIVER_PORT);
      if (eventsubReceiverForwarded(this.env)) extra.push(EVENTSUB_RECEIVER_PORT);
      checked = [...new Set(extra)];
    }
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
      console.warn(JSON.stringify({
        event: "container_keepalive_arm_failed",
        release: releaseTag(this.lastRelease),
        worker_version: workerVersionTag(this.env.CF_VERSION_METADATA?.id),
        log_url: LOG_QUERY_URL,
      }));
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
        const turnStartedAt = Date.now();
        const notifications: KeepaliveNotification[] = [];
        const owner = await this.ownership.require(this.id());
        if (payload?.deploymentId !== this.id() || payload.epoch !== owner.epoch) {
          throw new OwnershipRefused("stale_keepalive");
        }
        await this.keepaliveArming;
        this.renewActivityTimeout();
        let admitted = true;
        try {
          let status: number | null = null;
          let probe: ProbeDetail = { failure: null };
          try {
            // Refresh the release tag before any error event below: a failed
            // probe must not reuse a previous image's SHA. containerFetch can
            // trigger startup and port checks, and the SDK awaits onError from
            // those paths, so the clear must precede the fetch.
            this.lastRelease = null;
            const probeSignal = AbortSignal.timeout(6000);
            const res = await deadlineFetch(
              this.containerFetch("http://c/readyz", { signal: probeSignal }), probeSignal);
            status = res.status;
            if (!res.ok) {
              // Logged before the probe body is read, so this tick's SHA is
              // not known yet: stamp unknown, never the previous image's SHA
              // still in lastRelease (a failed probe must not reuse it).
              console.warn(JSON.stringify({
                event: "container_readyz_unhealthy",
                status,
                release: releaseTag(null),
                worker_version: workerVersionTag(this.env.CF_VERSION_METADATA?.id),
                log_url: LOG_QUERY_URL,
              }));
            }
            // Cap the keepalive /readyz read like the metrics scrape: a
            // compromised container controls this body, so an oversized answer
            // is a failed probe, never a buffered parse. Small bodies still
            // drain fully, so the SDK 0.3.7 proxy pipe settles cleanly.
            const body = await readBoundedText(res, MAX_PROBE_BODY_BYTES, probeSignal);
            if (probeSignal.aborted) {
              // A probe that only completed past its deadline is no evidence
              // of readiness: record it as failed, never as recovery.
              status = null;
              console.warn(JSON.stringify({
                event: "container_keepalive_probe_failed",
                release: releaseTag(this.lastRelease),
                worker_version: workerVersionTag(this.env.CF_VERSION_METADATA?.id),
                log_url: LOG_QUERY_URL,
              }));
            } else if (body === null) {
              status = null;
              console.warn(JSON.stringify({
                event: "container_keepalive_probe_failed",
                release: releaseTag(this.lastRelease),
                worker_version: workerVersionTag(this.env.CF_VERSION_METADATA?.id),
                log_url: LOG_QUERY_URL,
              }));
            } else {
              const botBody = isBotProbeResponse(res);
              probe = botBody ? probeDetail(body) : { failure: null };
              this.lastRelease = botBody ? parseRelease(body) : null;
              const failure = probe.failure;
              if (failure) {
                console.warn(JSON.stringify({
                  event: "container_gateway_failure",
                  phase: failure.phase,
                  class: failure.class,
                  failed_step: failure.failedStep ?? "unknown",
                  error_code: failure.errorCode ?? "none",
                  build_revision: probe.buildRevision ?? "unknown",
                  deploy_id: probe.deployId ?? "unknown",
                  release: releaseTag(this.lastRelease),
                  worker_version: workerVersionTag(this.env.CF_VERSION_METADATA?.id),
                  log_url: LOG_QUERY_URL,
                }));
              }
            }
          } catch (error) {
            if (error instanceof OwnershipRefused) {
              admitted = false;
              throw error;
            }
            status = null;
            this.lastRelease = null;
            console.warn(JSON.stringify({
              event: "container_keepalive_probe_failed",
              release: releaseTag(this.lastRelease),
              worker_version: workerVersionTag(this.env.CF_VERSION_METADATA?.id),
              log_url: LOG_QUERY_URL,
            }));
          }
          const context: AlertContext = {
            buildRevision: probe.buildRevision ?? "unknown",
            failedStep: probe.failure?.failedStep,
            errorCode: probe.failure?.errorCode,
          };
          await this.recordReadiness(status, notifications, context);
          await this.evaluateMetricsAlerts(notifications, context, probe.gatewayReady);
        } finally {
          // Collapse duplicate chains, preserving unrelated SDK schedules.
          // No takeover can interleave with this owned callback's I/O.
          this.deleteSchedules("keepalive");
          if (admitted) {
            await this.ownership.require(this.id());
            await this.schedule(this.keepaliveSeconds(), "keepalive", payload);
          }
        }
        // Persistence, primary logs, remediation and the successor are complete
        // before optional transport starts. Stay inside the same ownership gate.
        await this.forwardNotifications(notifications, turnStartedAt);
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

  private async recordReadiness(status: number | null, notifications: KeepaliveNotification[], context?: AlertContext): Promise<void> {
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
      build_revision: context?.buildRevision ?? "unknown",
      release: releaseTag(this.lastRelease),
      worker_version: workerVersionTag(this.env.CF_VERSION_METADATA?.id),
      failed_step: context?.failedStep ?? "unknown",
      error_code: context?.errorCode ?? "none",
      log_url: LOG_QUERY_URL,
    }));
    const suffix = alertContextSuffix(context);
    notifications.push({
      readinessEvent: event,
      content: event === "container_unready_alert"
        ? `two-bot-next: Container /readyz has repeatedly failed. Check the gateway connection and Worker logs.${suffix}`
        : `two-bot-next: Container /readyz is ready again. The unready incident has recovered.${suffix}`,
    });
  }

  /** Pull /metrics, evaluate rules, log and collect transitions. Never throws. */
  private async evaluateMetricsAlerts(notifications: KeepaliveNotification[], context?: AlertContext, gatewayReady?: boolean): Promise<void> {
    try {
      const previous = (await this.ctx.storage.get<MetricsAlertState>(METRICS_ALERT_KEY)) ?? EMPTY_STATE;
      const scrapeLoss = sanitizeScrapeLossState(await this.ctx.storage.get<ScrapeLossState>(SCRAPE_LOSS_KEY));
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
        // It does advance the observer-loss streak: three consecutive failed
        // scheduled scrapes ticket once, further failures stay quiet, and the
        // streak lives apart from metric-rule state.
        console.warn("two-bot metrics scrape failed");
        const streak = Math.min(scrapeLoss.streak + 1, SCRAPE_LOSS_THRESHOLD);
        const firing = scrapeLoss.firing || streak >= SCRAPE_LOSS_THRESHOLD;
        const justFired = firing && !scrapeLoss.firing;
        // Persist before notifying: at most one attempt per transition, even
        // if the webhook fails or an alarm is retried.
        await this.ctx.storage.put(
          METRICS_ALERT_KEY,
          interruptPrefixVerdictRefusals(interruptAuditHalt(interruptCommunityFactsDrain(interruptDispatchDrops(previous)))),
        );
        await this.ctx.storage.put(SCRAPE_LOSS_KEY, { streak, firing });
        // Nor is it remediation recovery: unobserved time must not run down
        // a scope's clear hold-down. A corrupt guard is left for the next
        // evaluated tick to sanitize, and never blocks the alert path.
        try {
          const nowSeconds = Math.floor(Date.now() / 1000);
          const guard = sanitizeRemediationState(await this.ctx.storage.get<RemediationState>(REMEDIATION_KEY), nowSeconds);
          const interrupted = interruptRemediationHoldDown(guard);
          if (interrupted !== guard) await this.ctx.storage.put(REMEDIATION_KEY, interrupted);
        } catch {
          console.warn("two-bot remediation hold-down interrupt failed; alerts already recorded");
        }
        if (justFired) {
          const content = scrapeLossFiringMessage(context);
          console.warn(JSON.stringify({
            event: "metrics_alert",
            service: "two-bot-next",
            content,
            build_revision: context?.buildRevision ?? "unknown",
            release: releaseTag(this.lastRelease),
            worker_version: workerVersionTag(this.env.CF_VERSION_METADATA?.id),
            log_url: LOG_QUERY_URL,
          }));
          notifications.push({ content });
        }
        return;
      }
      // Staging-only fault injector: synthetic drill input for the
      // guarded-remediation soak. Dark unless the generated staging deploy
      // config arms it; production and unarmed staging evaluate the scraped
      // samples untouched. Never logged: the switches never leave this gate.
      const faulted = applyStagingFaultInjection(samples, previous, this.env);
      const { firing, state, voiceLiveNotReadyEvidence } = evaluateMetrics(faulted, previous, Date.now() / 1000, gatewayReady);
      // Persist before notifying: at most one attempt per transition.
      await this.ctx.storage.put(METRICS_ALERT_KEY, state);
      for (const content of transitionMessages(previous.firing, firing, previous.jobErrorClasses, state.jobErrorClasses, context)) {
        console.warn(JSON.stringify({
          event: "metrics_alert",
          service: "two-bot-next",
          content,
          build_revision: context?.buildRevision ?? "unknown",
          release: releaseTag(this.lastRelease),
          worker_version: workerVersionTag(this.env.CF_VERSION_METADATA?.id),
          log_url: LOG_QUERY_URL,
        }));
        notifications.push({ content });
      }
      // A complete valid scrape clears only observer-loss state: silently
      // below the ticket threshold, or with exactly one recovery once firing.
      // After the metric transitions above, so a transient failure of this
      // write can only delay the scrape-loss resolve (retried next tick),
      // never skip an already-persisted metric notification.
      const recovering = scrapeLoss.firing;
      if (scrapeLoss.streak !== 0 || scrapeLoss.firing) {
        await this.ctx.storage.put(SCRAPE_LOSS_KEY, { ...EMPTY_SCRAPE_LOSS });
      }
      if (recovering) {
        const content = scrapeLossResolvedMessage(context);
        console.warn(JSON.stringify({
          event: "metrics_alert",
          service: "two-bot-next",
          content,
          build_revision: context?.buildRevision ?? "unknown",
          release: releaseTag(this.lastRelease),
          worker_version: workerVersionTag(this.env.CF_VERSION_METADATA?.id),
          log_url: LOG_QUERY_URL,
        }));
        notifications.push({ content });
      }
      // Guarded remediation recommendations ride the same alert path: one
      // line per recommendation, only while its trigger key fires. A corrupt
      // guard record restarts the guard rather than blocking alerts.
      try {
        const stored = await this.ctx.storage.get<RemediationState>(REMEDIATION_KEY);
        const nowSeconds = Math.floor(Date.now() / 1000);
        // No current age sample this scrape (the gauge was missing or
        // malformed): a firing `voice_live_not_ready` is only the alert
        // holding its previous state, and a quiet one is not an observed
        // clear. Either way that scope's progression and recovery clock
        // must not move until a current age sample arrives.
        const unobserved = voiceLiveNotReadyEvidence ? [] : ["voice_live_not_ready"];
        const remediation = evaluateRemediation(firing, sanitizeRemediationState(stored, nowSeconds), nowSeconds, unobserved);
        await this.ctx.storage.put(REMEDIATION_KEY, remediation.state);
        for (const message of remediation.messages) {
          const content = `${message.text}.${alertContextSuffix(context)}`;
          console.warn(JSON.stringify({
            event: "remediation_recommendation",
            service: "two-bot-next",
            content,
            build_revision: context?.buildRevision ?? "unknown",
            release: releaseTag(this.lastRelease),
            worker_version: workerVersionTag(this.env.CF_VERSION_METADATA?.id),
            log_url: LOG_QUERY_URL,
          }));
          notifications.push({ content });
        }
        // Staging-only persisted guard receipt: read back the guard just
        // written and log the allowlisted counters/timestamps/phases with
        // evaluation time, firing trigger keys, decision and build correlation.
        // Exactly one receipt per keepalive tick in staging, none in production.
        // No endpoint, permission, dump, or storage mutation beyond the
        // evaluator persistence above; thresholds and state-machine semantics
        // are untouched.
        if (this.env.TWO_DEPLOY_ENV === "staging") {
          try {
            // Flush the just-written guard before the read-back: DO storage
            // serves recent writes from its in-memory cache/write buffer and
            // put() resolves before the flush, so without sync() the get()
            // below always repeats the in-memory value and can never prove
            // durability. A failed flush throws here and lands in the
            // readback_failed branch below.
            await this.ctx.storage.sync();
            const persisted = await this.ctx.storage.get<RemediationState>(REMEDIATION_KEY);
            const receipt = buildRemediationGuardReceipt({
              persisted,
              expected: remediation.state,
              firing,
              messages: remediation.messages,
              nowSeconds,
              unobserved,
              buildRevision: context?.buildRevision ?? "unknown",
              release: releaseTag(this.lastRelease),
              workerVersion: workerVersionTag(this.env.CF_VERSION_METADATA?.id),
            });
            console.warn(JSON.stringify({
              event: "remediation_guard_receipt",
              service: "two-bot-next",
              ...receipt,
              log_url: LOG_QUERY_URL,
            }));
          } catch {
            console.warn(JSON.stringify({
              event: "remediation_guard_receipt",
              service: "two-bot-next",
              ok: false,
              deploy_env: "staging",
              evaluated_at_seconds: nowSeconds,
              firing: remediationTriggerFiring(firing),
              reason: "readback_failed",
              build_revision: context?.buildRevision ?? "unknown",
              release: releaseTag(this.lastRelease),
              worker_version: workerVersionTag(this.env.CF_VERSION_METADATA?.id),
              log_url: LOG_QUERY_URL,
            }));
          }
        }
      } catch {
        console.warn("two-bot remediation guard failed; alerts already recorded");
      }
    } catch {
      console.warn("two-bot metrics scrape failed");
    }
  }

  /** One turn-local allowance, not a durable queue or a per-POST timeout. */
  private async forwardNotifications(notifications: KeepaliveNotification[], turnStartedAt: number): Promise<void> {
    if (!notifications.length || this.env.OPS_ALERT_FORWARDING !== "on" || !this.env.OPS_ALERT_WEBHOOK_URL) return;
    const allowance = Math.min(NOTIFICATION_TIMEOUT_MS, KEEPALIVE_FORWARDING_CUTOFF_MS - (Date.now() - turnStartedAt));
    if (allowance <= 0) return;
    const deadline = Date.now() + allowance;
    const signal = AbortSignal.timeout(allowance);
    for (const notification of notifications) {
      // The wall-clock check also refuses a fresh attempt before a delayed
      // abort event is dispatched. Never resume this drain from late I/O.
      if (signal.aborted || Date.now() >= deadline) break;
      await this.postWebhook(notification, signal);
    }
  }

  private async postWebhook(notification: KeepaliveNotification, signal: AbortSignal): Promise<void> {
    let status: number | null = null;
    try {
      const url = opsAlertWebhookUrl(this.env.OPS_ALERT_WEBHOOK_URL!);
      const response = await deadlineFetch(fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json" },
        redirect: "error",
        signal,
        body: JSON.stringify({ content: notification.content, allowed_mentions: { parse: [], replied_user: false } }),
      }), signal);
      status = response.status;
      // Cleanup shares the aggregate deadline, including rejection sinks for
      // late failures. A late fetch response is released by deadlineFetch.
      await abortable(response.body?.cancel() ?? Promise.resolve(), signal);
      if (response.ok && !signal.aborted) return;
    } catch {
      // Fetch/cancel errors can contain the secret URL. Never log their text.
      // Keep an already-received HTTP status when cleanup fails or times out.
    }
    console.warn(JSON.stringify({
      event: notification.readinessEvent ? "container_unready_webhook_failed" : "metrics_alert_webhook_failed",
      ...(notification.readinessEvent ? { notification: notification.readinessEvent } : {}),
      status,
      release: releaseTag(this.lastRelease),
      worker_version: workerVersionTag(this.env.CF_VERSION_METADATA?.id),
      log_url: LOG_QUERY_URL,
    }));
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
    // A stop invalidates the parsed release: the replacement image is
    // unprobed, so its startup failure must stamp "unknown", not the old SHA.
    this.lastRelease = null;
    console.log("two-bot container stopped");
  }

  override onError(_error: unknown): void {
    console.error(JSON.stringify({
      event: "container_error",
      error_class: "container_lifecycle_failed",
      release: releaseTag(this.lastRelease),
      worker_version: workerVersionTag(this.env.CF_VERSION_METADATA?.id),
      log_url: LOG_QUERY_URL,
    }));
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
        env.CONTAINER_RELEASE,
      );
    }

    // Staging-only EventSub ingress. Dark (no match here, today's behavior)
    // unless EVENTSUB_INGRESS and TWO_EVENTSUB are both "1".
    if (url.pathname === EVENTSUB_PATH && eventsubIngressEnabled(env)) {
      return handleEventsubIngress(
        request,
        request.headers.get("cf-connecting-ip") ?? "unknown",
        async (sanitized) => {
          // Stamp the fence id server-side; a caller-supplied one was already
          // dropped by the header allowlist.
          sanitized.headers.set(DEPLOYMENT_HEADER, deploymentId(env.CF_VERSION_METADATA?.id));
          return env.TWO_BOT.getByName(SINGLETON_NAME).fetch(sanitized);
        },
        env.CONTAINER_RELEASE,
      );
    }

    // Bot HTTP MCP server (Streamable HTTP, read-only tools first). Exact
    // path only: only POST is served (browser probes get 405, never tool
    // execution). Unconfigured/short token → 404 (the route does not exist);
    // missing/wrong bearer → 401 with WWW-Authenticate, before the Container
    // is touched. Authenticated request bodies are collected under their own
    // inbound bound (64 KiB / 6 s, before parsing) in mcp.ts, and every tool
    // call below keeps its separate outbound container bound (6 s / 64 KiB).
    if (url.pathname === "/mcp") {
      if (request.method !== "POST") {
        return new Response("method not allowed", {
          status: 405,
          headers: withSecurityHeaders({ allow: "POST" }),
        });
      }
      if (!env.MCP_TOKEN || env.MCP_TOKEN.length < MIN_SCRAPE_TOKEN_LENGTH) {
        if (env.MCP_TOKEN && env.MCP_TOKEN.length < MIN_SCRAPE_TOKEN_LENGTH
          && !mcpShortTokenLogged) {
          mcpShortTokenLogged = true;
          console.error(JSON.stringify({
            event: "mcp_token_misconfigured",
            reason: "token_below_minimum_length",
            minimum_length: MIN_SCRAPE_TOKEN_LENGTH,
          }));
        }
        return new Response("not found", { status: 404, headers: withSecurityHeaders() });
      }
      const caller = request.headers.get("cf-connecting-ip") ?? "unknown";
      const verdict = mcpAuthBuckets.take(caller);
      if (!verdict.allowed && !mcpAuthBuckets.peek(caller).allowed) {
        return new Response("slow down\n", {
          status: 429,
          headers: withSecurityHeaders({
            "content-type": "text/plain",
            "retry-after": String(verdict.retryAfter),
          }),
        });
      }
      const m = /^Bearer (.+)$/.exec(request.headers.get("authorization") ?? "");
      if (!m || m[1]!.length > 4096 || !(await tokenMatches(m[1]!, env.MCP_TOKEN))) {
        return new Response("unauthorized", { status: 401, headers: withSecurityHeaders({ "www-authenticate": "Bearer" }) });
      }
      return handleMcp(request, env, {
        BOT_PORT: env.BOT_PORT,
        KEEPALIVE_SECONDS: env.KEEPALIVE_SECONDS,
      });
    }

    // Admin control surface (TOG-22862). The gate authenticates, enforces the
    // write switches and rate-caps guessing; the fenced DO owns state below.
    // CONTROL_PATH keeps its own gate and shares nothing here.
    if (url.pathname === OPS_CONFIG_PATH || url.pathname === OPS_KILLSWITCH_PATH || url.pathname === OPS_AUDIT_PATH) {
      const gate = await gateOpsRequest(request, env);
      if (gate) return gate;
      try {
        const forwarded = new Request(request);
        forwarded.headers.delete(MCP_AUDIT_READ_HEADER);
        forwarded.headers.set(DEPLOYMENT_HEADER, deploymentId(env.CF_VERSION_METADATA?.id));
        return await env.TWO_BOT.getByName(SINGLETON_NAME).fetch(forwarded);
      } catch (error) { return refused(error); }
    }

    if (url.pathname === "/health" || url.pathname === "/readyz" || url.pathname === "/version") {
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
      // expects, but drop the query, caller headers and body. The DO sees the
      // Worker-stamped deployment id the fence requires, plus the validated
      // `x-request-id` so the container's request span joins this probe.
      const clean = new URL(request.url);
      clean.search = "";
      clean.hash = "";
      // A missing version identity cannot address a deployment: refuse like
      // the control and metrics paths instead of blaming the container.
      let deployment: string;
      try {
        deployment = deploymentId(env.CF_VERSION_METADATA?.id);
      } catch (error) { return refused(error); }
      const probeId = ensureRequestId(request.headers.get(REQUEST_ID_HEADER));
      let response: Response;
      try {
        const sanitized = new Request(clean.toString(), {
          method: request.method,
          headers: { [DEPLOYMENT_HEADER]: deployment, [REQUEST_ID_HEADER]: probeId },
        });
        const upstream = await env.TWO_BOT.getByName(SINGLETON_NAME).fetch(sanitized);
        // SDK startup failures arrive as responses rather than rejections.
        // Cancel without reading: a huge non-bot body must not bypass the
        // probe cap by being drained here. Best-effort: a stalling cancel
        // hook must not hold the fixed refusal.
        if (!isBotProbeResponse(upstream)) {
          releaseBody(upstream);
          response = containerUnavailable(env.CF_VERSION_METADATA?.id);
        } else {
          response = upstream;
        }
      } catch {
        // Includes DO construction/binding failures before its fetch handler.
        response = containerUnavailable(env.CF_VERSION_METADATA?.id);
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

    // Signed backfill-progress read (TOG-22953): the container owns the
    // x-two-* HMAC, so there is no scrape-bearer gate here — but the request
    // still never reaches the singleton DO unexamined. GET only (405
    // otherwise), then a per-caller attempt budget (429 + retry-after when
    // exhausted, taken before the header check so a flood cannot confirm
    // anything while throttled), then the x-two-* presence/length pre-check
    // (the route's 401, no Bearer challenge). Only then does the Worker stamp
    // the deployment fence id and forward the request. Exact path only:
    // unknown /ops/* subpaths still fall through to the redirect lookup below.
    if (url.pathname === OPS_BACKFILL_PROGRESS_PATH) {
      if (request.method !== "GET") {
        return new Response("method not allowed", { status: 405, headers: withSecurityHeaders({ allow: "GET" }) });
      }
      const caller = request.headers.get("cf-connecting-ip") ?? "unknown";
      const verdict = backfillProgressBuckets.take(caller);
      if (!verdict.allowed && !backfillProgressBuckets.peek(caller).allowed) {
        return new Response("slow down\n", {
          status: 429,
          headers: withSecurityHeaders({
            "content-type": "text/plain",
            "retry-after": String(verdict.retryAfter),
          }),
        });
      }
      const refusal = backfillProgressRefusal(request);
      if (refusal) return refusal;
      try {
        const forwarded = new Request(request);
        forwarded.headers.set(DEPLOYMENT_HEADER, deploymentId(env.CF_VERSION_METADATA?.id));
        return await env.TWO_BOT.getByName(SINGLETON_NAME).fetch(forwarded);
      } catch (error) { return refused(error); }
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
    // Read-only ops summary (TOG-22860 + TOG-22861, dual-gate budget
    // TOG-29408): the roll-up serves two operator audiences, so either
    // credential opens it — the metrics scrape bearer or the dashboard
    // service-token pair (Cloudflare Access service tokens: both
    // CF-Access-Client-Id and CF-Access-Client-Secret on every summary
    // request, strict mode — no cookies or sessions). Each credential spends
    // only its own per-caller budget: the scrape gate runs only when an
    // Authorization header is presented, and the dashboard gate only when a
    // dashboard header is presented. Guessing at one therefore cannot
    // throttle the other. For a bearer-presenting request the scrape gate
    // runs first and its 429 is a hard fence, never bypassed via the other
    // gate; a non-throttled scrape refusal falls through to the dashboard
    // gate. Non-GET and doubly-unconfigured still 404 (the route does not
    // exist); otherwise a failed attempt is 401 with the Bearer challenge.
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
      // Only a bearer-presenting request is evaluated by (and spends) the
      // scrape budget; a dashboard-only request skips this gate entirely so
      // a spent scrape bucket cannot refuse it.
      const hasScrapeAttempt = request.headers.has("authorization");
      if (scrapeConfigured && hasScrapeAttempt) {
        const refusal = await opsReadRefusal(request, env);
        if (refusal === null) return forwardSummary();
        if (refusal.status === 429) return refusal;
      }
      const caller = request.headers.get("cf-connecting-ip") ?? "unknown";
      const dashboardConfigured = !!env.OPS_DASHBOARD_CLIENT_ID
        && env.OPS_DASHBOARD_CLIENT_ID.length >= MIN_DASHBOARD_CLIENT_ID_LENGTH
        && !!env.OPS_DASHBOARD_CLIENT_SECRET
        && env.OPS_DASHBOARD_CLIENT_SECRET.length >= MIN_DASHBOARD_SECRET_LENGTH;
      // Only a dashboard-presenting request is evaluated by (and spends) the
      // dashboard budget; a bearer-only guess never touches it.
      const hasDashboardAttempt = request.headers.has(DASHBOARD_CLIENT_ID_HEADER)
        || request.headers.has(DASHBOARD_CLIENT_SECRET_HEADER);
      if (dashboardConfigured && hasDashboardAttempt) {
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
