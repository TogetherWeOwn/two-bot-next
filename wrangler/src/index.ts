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
 */

import { Container } from "@cloudflare/containers";

export interface Env {
  TWO_BOT: DurableObjectNamespace<TwoBotContainer>;
  DISCORD_TOKEN?: string;
  DATABASE_URL?: string;
  GUILD_ID?: string;
  BOT_PORT?: string;
  KEEPALIVE_SECONDS?: string;
}

interface KeepalivePayload {
  startedAt: number;
}

const SINGLETON_NAME = "two-bot";
const DEFAULT_KEEPALIVE_SECONDS = 60;

/** Readonly view of the secrets/vars the DO forwards into the container. */
function containerEnvVars(env: Env): Record<string, string> {
  const vars: Record<string, string> = {};
  if (env.DISCORD_TOKEN) vars["DISCORD_TOKEN"] = env.DISCORD_TOKEN;
  if (env.DATABASE_URL) vars["DATABASE_URL"] = env.DATABASE_URL;
  if (env.GUILD_ID) vars["GUILD_ID"] = env.GUILD_ID;
  vars["LISTEN_ADDR"] = `0.0.0.0:${env.BOT_PORT ?? "8080"}`;
  return vars;
}

export class TwoBotContainer extends Container<Env> {
  defaultPort = 8080;
  // Belt and braces: the schedule() keepalive below is the primary guard;
  // a long sleepAfter means a missed tick or two never costs the session.
  sleepAfter = "30m";
  pingEndpoint = "health";

  override async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname === "/health" || url.pathname === "/readyz") {
      this.armKeepalive();
      return this.containerFetch(request);
    }

    return new Response("not found", { status: 404 });
  }

  /**
   * Start the container with secrets from the Worker env when it is not
   * already running. containerFetch() auto-starts with class defaults, but
   * secrets must be passed explicitly — this is the only place Worker
   * secrets cross into the container, as env vars (never in image/layers).
   */
  override async start(...args: Parameters<Container<Env>["start"]>) {
    const [startOptions, waitOptions] = args;
    return super.start(
      {
        envVars: { ...containerEnvVars(this.env), ...startOptions?.envVars },
        ...startOptions,
      },
      waitOptions,
    );
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
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname === "/health" || url.pathname === "/readyz") {
      const container = env.TWO_BOT.getByName(SINGLETON_NAME);
      return container.fetch(request);
    }

    return new Response(
      "two-bot-next worker alive; /health and /readyz proxy to the container",
      { status: 200, headers: { "content-type": "text/plain" } },
    );
  },
};
