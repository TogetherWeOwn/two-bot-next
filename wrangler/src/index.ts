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
  handleRedirect,
  type Campaign,
  type RedirectClick,
} from "./redirect.ts";
import { RedirectStore, parseMappingsSnapshot } from "./redirect-store.ts";

export interface Env {
  TWO_BOT: DurableObjectNamespace<TwoBotContainer>;
  DISCORD_TOKEN?: string;
  DATABASE_URL?: string;
  GUILD_ID?: string;
  TWO_ANNOUNCEMENTS?: string;
  BOT_PORT?: string;
  KEEPALIVE_SECONDS?: string;
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

function redirectStore(env: Env): RedirectStore {
  let snapshot: Campaign[] = [];
  try {
    snapshot = env.REDIRECT_MAPPINGS_JSON
      ? parseMappingsSnapshot(env.REDIRECT_MAPPINGS_JSON)
      : [];
  } catch {
    snapshot = [];
  }
  // node-postgres ships inside the Worker via the `nodejs_compat` flag only
  // when S1 wires Hyperdrive; until then connect stays undefined and the
  // store serves the snapshot with clicks dropped (logged, never faked).
  return new RedirectStore(env.REDIRECT_DB, undefined, snapshot);
}

interface KeepalivePayload {
  startedAt: number;
}

const SINGLETON_NAME = "two-bot";
const DEFAULT_KEEPALIVE_SECONDS = 60;

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
  const vars: Record<string, string> = {};
  if (env.DISCORD_TOKEN) vars["DISCORD_TOKEN"] = env.DISCORD_TOKEN;
  if (env.DATABASE_URL) vars["DATABASE_URL"] = env.DATABASE_URL;
  if (env.GUILD_ID) vars["GUILD_ID"] = env.GUILD_ID;
  if (env.TWO_ANNOUNCEMENTS !== undefined) {
    vars["TWO_ANNOUNCEMENTS"] = env.TWO_ANNOUNCEMENTS;
  }
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
      this.armKeepalive();
      return this.containerFetch(request);
    }

    return new Response("not found", { status: 404 });
  }

  /** Arm the self-perpetuating schedule() keepalive (idempotent). */
  private armKeepalive(): void {
    // schedule() rejects while an identical pending task exists — that just
    // means the loop is already armed.
    void this.schedule(this.keepaliveSeconds(), "keepalive", {
      startedAt: Date.now(),
    } satisfies KeepalivePayload).catch(() => undefined);
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
  public async keepalive(payload: KeepalivePayload): Promise<void> {
    this.renewActivityTimeout();
    try {
      const res = await this.containerFetch("http://c/readyz", {
        signal: AbortSignal.timeout(6000),
      });
      if (!res.ok) {
        console.warn(`two-bot /readyz unhealthy: ${res.status}`);
      }
    } catch (err) {
      console.warn(`two-bot keepalive probe failed: ${String(err)}`);
    }
    await this.schedule(this.keepaliveSeconds(), "keepalive", payload);
  }

  override onStart(): void {
    console.log("two-bot container started");
    this.armKeepalive();
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
      const container = env.TWO_BOT.getByName(SINGLETON_NAME);
      return container.fetch(request);
    }

    // B3: everything else is a go.two.gg tracked link. Clicks record after
    // the 302 via waitUntil — the visitor never waits on the database, and a
    // failed write costs a click, never a member. Record failures are logged
    // with slug only (never visitor data — see redirect.ts privacy note).
    const store = redirectStore(env);
    const result = await handleRedirect(
      request.method,
      url.pathname,
      request.headers.get("cf-connecting-ip") ?? "unknown",
      {
        guildId: env.GUILD_ID ?? "",
        fallbackInviteCode: env.REDIRECT_FALLBACK_CODE ?? null,
        lookup: (slug) => store.lookup(slug),
        recordClick: (click: RedirectClick) => store.recordClick(click),
        onError: (msg, detail) =>
          console.error(`${msg} ${JSON.stringify(detail)}`),
        isThrottled: (key) => !clickBuckets.take(key).allowed,
      },
    );
    if (result.click) {
      const click = result.click;
      ctx.waitUntil(
        store.recordClick(click).catch((err: unknown) =>
          console.error(
            `invite_click_record_failed ${JSON.stringify({ campaign: click.campaign, err: String(err) })}`,
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
