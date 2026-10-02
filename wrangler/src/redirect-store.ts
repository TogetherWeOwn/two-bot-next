/**
 * Hyperdrive-backed mapping lookup + click write for the go.two.gg port.
 *
 * SQL mirrors two-bot exactly:
 * - lookup: migration 0006 (`invite_campaigns` by slug; disabled rows still
 *   resolve — retiring never breaks a posted link);
 * - insert: `EventStore.record` (`events`, `ON CONFLICT (idempotency_key)
 *   DO NOTHING`, `member_id` NULL, `metadata = {"campaign": slug}`).
 *
 * With `REDIRECT_DB` bound, every call opens one client through `connect`
 * (redirect-db.ts), bounds connect + query by `DB_TIMEOUT_MS` and always ends
 * the client. Unbound, the Worker serves from `REDIRECT_MAPPINGS_JSON` (a JSON
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

export class RedirectStore {
  private db: HyperdriveLike | undefined;
  private connect: ConnectFn | undefined;
  private snapshot: Campaign[];
  private timeoutMs: number;

  constructor(
    db: HyperdriveLike | undefined,
    connect: ConnectFn | undefined,
    snapshot: Campaign[] = [],
    timeoutMs: number = DB_TIMEOUT_MS,
  ) {
    this.db = db;
    this.connect = connect;
    this.snapshot = snapshot;
    this.timeoutMs = timeoutMs;
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
    return this.withClient(async (client) => {
      const { rows } = await client.query(LOOKUP_SQL, [slug]);
      const row = rows[0] as CampaignRow | undefined;
      return row ? rowToCampaign(row) : null;
    });
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
