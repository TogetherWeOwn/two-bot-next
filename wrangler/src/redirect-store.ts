/**
 * Hyperdrive-backed mapping lookup + click write for the go.two.gg port.
 *
 * SQL mirrors two-bot exactly:
 * - lookup: migration 0006 (`invite_campaigns` by slug; disabled rows still
 *   resolve — retiring never breaks a posted link);
 * - insert: `EventStore.record` (`events`, `ON CONFLICT (idempotency_key)
 *   DO NOTHING`, `member_id` NULL, `metadata = {"campaign": slug}`).
 *
 * Until S1 (TOG-9671) lands the shared Neon database, `REDIRECT_DB` is
 * unbound and the Worker serves from `REDIRECT_MAPPINGS_JSON` (a JSON snapshot
 * of the same rows, e.g. exported via `npm run campaigns`); clicks are then
 * logged and dropped, never faked into a store. Resolution behavior — the
 * byte-identical acceptance — is the same either way.
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

  constructor(
    db: HyperdriveLike | undefined,
    connect: ConnectFn | undefined,
    snapshot: Campaign[] = [],
  ) {
    this.db = db;
    this.connect = connect;
    this.snapshot = snapshot;
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
    const client = await (this.connect as ConnectFn)(
      (this.db as HyperdriveLike).connectionString,
    );
    try {
      const { rows } = await client.query(LOOKUP_SQL, [slug]);
      const row = rows[0] as CampaignRow | undefined;
      return row ? rowToCampaign(row) : null;
    } finally {
      await client.end();
    }
  }

  async recordClick(click: RedirectClick): Promise<void> {
    if (!this.live) return;
    const client = await (this.connect as ConnectFn)(
      (this.db as HyperdriveLike).connectionString,
    );
    try {
      await client.query(INSERT_CLICK_SQL, [
        click.guildId,
        click.occurredAt,
        click.source,
        JSON.stringify({ campaign: click.campaign }),
        clickIdempotencyKey(click),
      ]);
    } finally {
      await client.end();
    }
  }
}
