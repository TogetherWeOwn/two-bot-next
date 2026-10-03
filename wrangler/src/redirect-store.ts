/**
 * Hyperdrive-backed mapping lookup + click write for the go.two.gg port.
 *
 * SQL mirrors two-bot exactly:
 * - lookup: migration 0006 (`invite_campaigns` by slug; disabled rows still
 *   resolve — retiring never breaks a posted link);
 * - insert: `EventStore.record` (`events`, `ON CONFLICT (idempotency_key)
 *   DO NOTHING`, `member_id` NULL, `metadata = {"campaign": slug}`).
 *
 * With `REDIRECT_DB` bound, a cache miss opens one client through `connect`
 * (redirect-db.ts), bounds connect + query by `DB_TIMEOUT_MS` and always ends
 * the client; hits and short-lived misses are served from the in-memory
 * `CampaignLookupCache` below. Unbound, the Worker serves from `REDIRECT_MAPPINGS_JSON` (a JSON
 * snapshot of the same rows, e.g. exported via `npm run campaigns`); clicks
 * are then logged and dropped, never faked into a store. Resolution behavior —
 * the byte-identical acceptance — is the same either way.
 */

import {
  clickIdempotencyKey,
  isValidSlug,
  isValidInviteCode,
  type Campaign,
  type RedirectClick,
} from "./redirect.ts";

export interface HyperdriveLike {
  connectionString: string;
}

/** Minimal query surface the store needs (node-postgres compatible). */
export interface DbClient {
  query(
    text: string,
    params: unknown[],
  ): Promise<{ rows: Record<string, unknown>[] }>;
  end(): Promise<void>;
}

export type ConnectFn = (connectionString: string) => Promise<DbClient>;

/**
 * Upper bound on connect + query for one store call. A visitor's redirect
 * waits on the lookup; past this the outage path serves the fallback invite.
 */
export const DB_TIMEOUT_MS = 3_000;
/** Ending a client never holds a redirect longer than this. */
const END_TIMEOUT_MS = 1_000;

/** Fixed message: the caller logs only the class (see redirectErrorClass). */
export class DbTimeoutError extends Error {
  constructor() {
    super("redirect store timed out");
    // Explicit: a subclass instance otherwise reports name "Error", and the
    // classifier in redirect.ts matches on this name (an import would cycle
    // back into redirect.ts and break the standalone Miniflare embed).
    this.name = "DbTimeoutError";
  }
}

/** End a client without throwing or waiting more than END_TIMEOUT_MS. */
async function endClient(client: DbClient): Promise<void> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    await Promise.race([
      Promise.resolve().then(() => client.end()),
      new Promise<void>((resolve) => { timer = setTimeout(resolve, END_TIMEOUT_MS); }),
    ]);
  } catch {
    // The query already settled; a failed goodbye must not change its result.
  } finally {
    clearTimeout(timer);
  }
}

interface CampaignRow {
  slug: string;
  invite_code: string;
  label: string;
  disabled_at: string | null;
  created_at: string;
}

const LOOKUP_SQL = `SELECT slug, invite_code, label, disabled_at, created_at
  FROM invite_campaigns WHERE slug = $1`;

/**
 * Legacy campaigns.ts contract (TOG-11153, parity §12 row 1): hits live 30s,
 * null misses live at most 2s (`negativeTtlMs ?? min(2_000, ttlMs)`), so a
 * newly added campaign resolves quickly while URL-walking bots are blunted.
 * The store adds the bound legacy lacks: at most DEFAULT_MAX_ENTRIES entries
 * with oldest-first eviction, so a slug spray cannot grow isolate memory.
 */
export const DEFAULT_HIT_TTL_MS = 30_000;
export const MAX_NEGATIVE_TTL_MS = 2_000;
export const DEFAULT_MAX_ENTRIES = 1_024;

const INSERT_CLICK_SQL = `INSERT INTO events (event_type, member_id, guild_id, occurred_at, source, metadata, idempotency_key)
  VALUES ('invite_click', NULL, $1, $2, $3, $4, $5)
  ON CONFLICT (idempotency_key) DO NOTHING`;

function rowToCampaign(row: CampaignRow): Campaign {
  return {
    slug: row.slug,
    inviteCode: row.invite_code,
    label: row.label,
    disabledAt: row.disabled_at,
  };
}

/** Parse the `REDIRECT_MAPPINGS_JSON` snapshot fallback (same row shape). */
export function parseMappingsSnapshot(json: unknown): Campaign[] {
  // Treat snapshots as configuration, not trusted database rows. Reject the
  // entire snapshot (including duplicates) rather than silently dropping rows.
  try {
    if (typeof json !== "string") throw new Error();
    const rows: unknown = JSON.parse(json);
    if (!Array.isArray(rows)) throw new Error();
    const seen = new Set<string>();
    return rows.map((row: unknown) => {
      if (row === null || typeof row !== "object" || Array.isArray(row)) throw new Error();
      const data = row as Record<string, unknown>;
      if (
        typeof data.slug !== "string" || !isValidSlug(data.slug) || seen.has(data.slug) ||
        typeof data.invite_code !== "string" || !isValidInviteCode(data.invite_code) ||
        (data.label !== undefined && typeof data.label !== "string") ||
        (data.disabled_at != null && typeof data.disabled_at !== "string")
      ) throw new Error();
      seen.add(data.slug);
      return {
        slug: data.slug,
        inviteCode: data.invite_code,
        ...(typeof data.label === "string" ? { label: data.label } : {}),
        ...(data.disabled_at === null || typeof data.disabled_at === "string"
          ? { disabledAt: data.disabled_at } : {}),
      };
    });
  } catch {
    // JSON parse messages can contain fragments of the supplied config/secrets.
    throw new Error("Invalid redirect mappings snapshot");
  }
}

export class CampaignLookupCache {
  private entries = new Map<string, { value: Campaign | null; expiresAt: number }>();
  private hitTtlMs: number;
  private negativeTtlMs: number;
  private maxEntries: number;
  private clock: () => number;

  constructor(
    spec: { hitTtlMs?: number; negativeTtlMs?: number; maxEntries?: number } = {},
    now: () => number = Date.now,
  ) {
    const hit = spec.hitTtlMs ?? DEFAULT_HIT_TTL_MS;
    this.hitTtlMs = Number.isFinite(hit) ? Math.max(0, hit) : DEFAULT_HIT_TTL_MS;
    const negative = spec.negativeTtlMs ?? Math.min(MAX_NEGATIVE_TTL_MS, this.hitTtlMs);
    // Negative entries are short-lived by contract: clamped to at most 2s
    // (and never past the hit TTL), so a newly added slug resolves quickly
    // and a misconfigured large TTL cannot become a long-lived negative.
    this.negativeTtlMs = Number.isFinite(negative)
      ? Math.min(Math.max(0, negative), MAX_NEGATIVE_TTL_MS, this.hitTtlMs)
      : Math.min(MAX_NEGATIVE_TTL_MS, this.hitTtlMs);
    const max = spec.maxEntries ?? DEFAULT_MAX_ENTRIES;
    this.maxEntries = Number.isFinite(max) ? Math.max(0, Math.floor(max)) : DEFAULT_MAX_ENTRIES;
    this.clock = now;
  }

  /** Tracked entries (hits + misses). For tests/observability. */
  get size(): number {
    return this.entries.size;
  }

  /** A defined return is a live entry; `undefined` means uncached or expired. */
  get(slug: string): { value: Campaign | null } | undefined {
    const entry = this.entries.get(slug);
    if (entry === undefined) return undefined;
    if (this.clock() >= entry.expiresAt) {
      this.entries.delete(slug);
      return undefined;
    }
    return { value: entry.value };
  }

  set(slug: string, value: Campaign | null): void {
    const ttl = value === null ? this.negativeTtlMs : this.hitTtlMs;
    if (this.maxEntries <= 0 || ttl <= 0) return;
    this.entries.delete(slug);
    if (this.entries.size >= this.maxEntries) {
      const oldest = this.entries.keys().next().value;
      if (oldest !== undefined) this.entries.delete(oldest);
    }
    this.entries.set(slug, { value, expiresAt: this.clock() + ttl });
  }

  clear(): void {
    this.entries.clear();
  }
}

export class RedirectStore {
  private db: HyperdriveLike | undefined;
  private connect: ConnectFn | undefined;
  private snapshot: Campaign[];
  private timeoutMs: number;
  private lookupCache: CampaignLookupCache;

  constructor(
    db: HyperdriveLike | undefined,
    connect: ConnectFn | undefined,
    snapshot: Campaign[] = [],
    timeoutMs: number = DB_TIMEOUT_MS,
    lookupCache: CampaignLookupCache = new CampaignLookupCache(),
  ) {
    this.db = db;
    this.connect = connect;
    this.snapshot = snapshot;
    this.timeoutMs = timeoutMs;
    this.lookupCache = lookupCache;
  }

  get live(): boolean {
    return this.db !== undefined && this.connect !== undefined;
  }

  /** Null = no such slug (404). Throw = unreachable (outage path). */
  async lookup(slug: string): Promise<Campaign | null> {
    if (!isValidSlug(slug)) return null;
    if (!this.live) {
      return this.snapshot.find((c) => c.slug === slug) ?? null;
    }
    const cached = this.lookupCache.get(slug);
    if (cached !== undefined) return cached.value;
    // Throws (outage) are never cached: only a completed query stores an
    // entry, so a failed lookup retries the database on the next request.
    const result = await this.withClient(async (client) => {
      const { rows } = await client.query(LOOKUP_SQL, [slug]);
      const row = rows[0] as CampaignRow | undefined;
      return row ? rowToCampaign(row) : null;
    });
    this.lookupCache.set(slug, result);
    return result;
  }

  async recordClick(click: RedirectClick): Promise<void> {
    if (!this.live) return;
    await this.withClient((client) =>
      client.query(INSERT_CLICK_SQL, [
        click.guildId,
        click.occurredAt,
        click.source,
        JSON.stringify({ campaign: click.campaign }),
        clickIdempotencyKey(click),
      ]),
    );
  }

  /**
   * One client per call: connect + `run` share one deadline, and the client
   * is always ended — including one that connects after the deadline.
   */
  private async withClient<T>(run: (client: DbClient) => Promise<T>): Promise<T> {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const deadline = new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new DbTimeoutError()), this.timeoutMs);
    });
    const connecting = Promise.resolve().then(() =>
      (this.connect as ConnectFn)((this.db as HyperdriveLike).connectionString),
    );
    let client: DbClient | undefined;
    try {
      client = await Promise.race([connecting, deadline]);
      return await Promise.race([run(client), deadline]);
    } finally {
      clearTimeout(timer);
      if (client) {
        await endClient(client);
      } else {
        connecting.then(endClient, () => undefined);
      }
    }
  }
}
