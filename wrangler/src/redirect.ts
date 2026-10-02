/**
 * go.two.gg redirect port (B3, TOG-9696): framework-free request handler.
 *
 * Byte-for-byte behavioral port of `src/redirect/server.ts` (+ `campaigns.ts`,
 * `internal/rateLimit.ts`) from TogetherWeOwn/two-bot (TOG-116). The mapping
 * source is injected: in production it is a Hyperdrive query against the
 * shared Neon Postgres `invite_campaigns` table (S1, TOG-9671); in tests it is
 * a fixture. Resolution behavior is identical either way — the acceptance
 * under test.
 *
 * Legacy contract preserved:
 * - `GET /<slug>` → 302 `https://discord.gg/<code>` (never 301), with
 *   `cache-control: no-store, no-cache, must-revalidate` and
 *   `referrer-policy: no-referrer`; the click is recorded AFTER the redirect
 *   is built, never before, and a failed write still redirects.
 * - HEAD redirects but is never counted (link previewers, incl. Discord's).
 * - Query strings are dropped, never parsed or recorded.
 * - `/healthz` → 200 `ok`; `/favicon.ico` + `/robots.txt` → 404, no DB touch.
 * - Reserved internal slugs (`metrics`, plus `metrics/*` subpaths) → 404 for
 *   every method, before lookup — canonical case/encoding/slash aliases
 *   included, never a redirect, never a click.
 * - Non-GET/HEAD on non-reserved paths → 405 with `Allow: GET, HEAD`.
 * - Per-caller 60-burst / 1-per-sec token bucket runs BEFORE the DB lookup;
 *   denied → 429 + `retry-after: 1`, nothing recorded.
 * - Bare `/` → fallback code redirect (uncounted), else 404.
 * - Lookup outage (throw) → fallback redirect when configured, else 503 +
 *   `retry-after: 30`. Unknown slug (null) → 404 with no Location (no open
 *   redirect). Confirmed misses are cached per isolate for 5s (1,024 slots);
 *   cache hits still consume caller budget. Outages and live rows are uncached.
 *   Invalid stored code → 500, nothing recorded.
 * - Slugs: lowercase alnum + internal hyphens, 2–40 chars; lookup is
 *   case-insensitive, tolerates leading/trailing slashes, percent-decodes
 *   (malformed escape → 404). Disabled campaigns still redirect.
 * - Privacy (docs/PRIVACY.md in two-bot): a click is a campaign + timestamp.
 *   No cookies, IP, UA, referrer — nothing about the visitor is stored,
 *   logged, or set. Caller keys stay only in the isolate's rate-limit buckets.
 */

export interface Campaign {
  slug: string;
  inviteCode: string;
  label?: string;
  disabledAt?: string | null;
}

export interface RedirectClick {
  guildId: string;
  code: string;
  campaign: string;
  occurredAt: string;
  /** Random per request; keeps same-millisecond clicks from collapsing. */
  dedupeToken: string;
  /** Same string a join through this code is attributed to. */
  source: string;
}

export interface RedirectDeps {
  guildId: string;
  fallbackInviteCode?: string | null;
  /** Null = slug has no row; throw = store unreachable (outage path). */
  lookup: (slug: string) => Promise<Campaign | null>;
  /** Fire-and-forget: called after the 302 is built; rejections are logged. */
  recordClick: (click: RedirectClick) => Promise<unknown>;
  onError?: (msg: string, detail: Record<string, string>) => void;
  now?: () => number;
  uuid?: () => string;
  /** Rate-limit decision for this caller; true = over budget. */
  isThrottled?: (callerKey: string) => boolean;
  /** Shared by requests in one isolate; caches only confirmed missing slugs. */
  missCache?: RedirectMissCache;
}

export interface RedirectResult {
  status: number;
  headers: Record<string, string>;
  body: string;
  /** Present when this request counts as a click (GET on a live campaign). */
  click?: RedirectClick;
}

// Absolute end assertions: `$` alone also accepts a trailing line terminator.
const SLUG = /^[a-z0-9][a-z0-9-]{0,38}[a-z0-9](?![\s\S])/;
const INVITE_CODE = /^[A-Za-z0-9-]{1,64}(?![\s\S])/;
/** Internal slugs that are never invite campaigns (matched post-canonicalization). */
const RESERVED_SLUGS = ["metrics", "healthz"];

export function isValidSlug(slug: string): boolean {
  return SLUG.test(slug) && !RESERVED_SLUGS.includes(slug);
}

export function isValidInviteCode(code: string): boolean {
  return INVITE_CODE.test(code);
}

/** Empty/unset disables fallback. A configured code must be valid before serving. */
export function isValidFallback(code: unknown): boolean {
  return code == null || (typeof code === "string" && (code === "" || isValidInviteCode(code)));
}

/** Fixed vocabulary only: never log error messages, arbitrary names or stacks. */
export function redirectErrorClass(err: unknown): string {
  if (err instanceof TypeError) return "TypeError";
  if (err instanceof RangeError) return "RangeError";
  if (err instanceof SyntaxError) return "SyntaxError";
  if (err instanceof Error) return "Error";
  return "Unknown";
}

/** The URL a click is sent on to. Fixed host — never built from the path. */
export function inviteUrl(code: string): string {
  return `https://discord.gg/${code}`;
}

/**
 * Idempotency key for the `events` insert behind `recordClick`.
 * Mirrors two-bot `idempotencyKey()` for `invite_click`: member is always
 * `anon`, and the per-request token is what keeps two same-millisecond clicks
 * from collapsing into one row.
 */
export function clickIdempotencyKey(click: RedirectClick): string {
  const token = click.dedupeToken ? `:${click.dedupeToken}` : "";
  return `${click.guildId}:anon:invite_click:${click.occurredAt}${token}`;
}

/**
 * Minimal per-key token buckets (port of two-bot `TokenBuckets.take`).
 * One isolate = one map; acceptable for a 60-burst crawler cap at this scale.
 *
 * Memory is bounded without ever resetting an active throttle:
 * - Idle expiry: a bucket untouched for `idleTtlMs` is dropped and restarts
 *   full. The TTL is clamped to at least the full-refill window
 *   (`capacity / refillPerSecond`), so eviction can only discard buckets that
 *   refill would already have restored to full — a key that still owes tokens
 *   is never idle-expired, and expiry never grants tokens refill would not.
 * - Cardinality cap: at most `maxBuckets` entries. A new key past the cap is
 *   denied (fail closed) — overflow sheds load, it never disables throttling.
 *   Buckets already tracked are unaffected by the cap.
 * - Bounded cleanup: every `take` touches its key (LRU order, most-recent at
 *   the back) then reaps at most `sweepBudget` idle-expired entries from the
 *   front. Per-call work is O(sweepBudget), independent of map size.
 */
export class TokenBuckets {
  private buckets = new Map<string, { tokens: number; updatedAt: number }>();
  private spec: { capacity: number; refillPerSecond: number };
  private clock: () => number;
  private idleTtlMs: number;
  private maxBuckets: number;
  private sweepBudget: number;

  constructor(
    spec: { capacity: number; refillPerSecond: number } = {
      capacity: 60,
      refillPerSecond: 1,
    },
    now: () => number = Date.now,
    limits: {
      idleTtlMs?: number;
      maxBuckets?: number;
      sweepBudget?: number;
    } = {},
  ) {
    this.spec = spec;
    this.clock = now;
    const fullRefillMs = Math.ceil(
      (spec.capacity / spec.refillPerSecond) * 1000,
    );
    const requestedTtl = limits.idleTtlMs;
    this.idleTtlMs = Math.max(
      Number.isFinite(requestedTtl) ? (requestedTtl as number) : fullRefillMs,
      fullRefillMs,
    );
    this.maxBuckets = Math.max(
      1,
      Number.isFinite(limits.maxBuckets)
        ? Math.floor(limits.maxBuckets as number)
        : 10_000,
    );
    this.sweepBudget = Math.max(
      1,
      Number.isFinite(limits.sweepBudget)
        ? Math.floor(limits.sweepBudget as number)
        : 64,
    );
  }

  /** Tracked buckets. For tests/observability; keys are never logged. */
  get size(): number {
    return this.buckets.size;
  }

  take(key: string): { allowed: boolean; retryAfter: number } {
    const t = this.clock();
    let b = this.buckets.get(key);
    if (b && Math.max(0, t - b.updatedAt) >= this.idleTtlMs) {
      // Untouched for a full refill window: dropping restarts it full, exactly
      // as capped refill would. Cannot apply to a key that still owes tokens
      // (the TTL floor guarantees that), so expiry never resets a throttle.
      this.buckets.delete(key);
      b = undefined;
    }
    if (!b) {
      if (this.buckets.size >= this.maxBuckets) {
        // Fail closed: shed the unknown caller, keep every tracked throttle.
        return {
          allowed: false,
          retryAfter: Math.max(1, Math.ceil(this.idleTtlMs / 1000)),
        };
      }
      b = { tokens: this.spec.capacity, updatedAt: t };
    }
    const elapsed = Math.max(0, (t - b.updatedAt) / 1000);
    b.tokens = Math.min(
      this.spec.capacity,
      b.tokens + elapsed * this.spec.refillPerSecond,
    );
    b.updatedAt = t;
    // Move to the back: map order is recency order, so the sweep below always
    // reaps the least-recently-touched entries first.
    this.buckets.delete(key);
    let verdict: { allowed: boolean; retryAfter: number };
    if (b.tokens >= 1) {
      b.tokens -= 1;
      verdict = { allowed: true, retryAfter: 0 };
    } else {
      const wait = Math.ceil(
        (1 - b.tokens) / this.spec.refillPerSecond,
      );
      verdict = { allowed: false, retryAfter: Math.max(1, wait) };
    }
    this.buckets.set(key, b);
    this.sweep(t);
    return verdict;
  }

  /** Reap up to `sweepBudget` idle-expired entries from the least-recent end. */
  private sweep(t: number): void {
    let budget = this.sweepBudget;
    for (const [key, b] of this.buckets) {
      if (budget <= 0) return;
      // Front is live: recency order means everything behind it is newer.
      if (Math.max(0, t - b.updatedAt) < this.idleTtlMs) return;
      this.buckets.delete(key);
      budget -= 1;
    }
  }
}

/** Short, bounded negative cache: a new campaign becomes visible within 5s. */
export class RedirectMissCache {
  private misses = new Map<string, number>();
  private ttlMs: number;
  private maxEntries: number;
  private clock: () => number;

  constructor(
    spec: { ttlMs: number; maxEntries: number } = {
      ttlMs: 5_000,
      maxEntries: 1_024,
    },
    now: () => number = Date.now,
  ) {
    this.ttlMs = spec.ttlMs;
    this.maxEntries = spec.maxEntries;
    this.clock = now;
  }

  has(slug: string): boolean {
    const expiresAt = this.misses.get(slug);
    if (expiresAt === undefined) return false;
    if (this.clock() >= expiresAt) {
      this.misses.delete(slug);
      return false;
    }
    return true;
  }

  add(slug: string): void {
    if (this.maxEntries <= 0 || this.ttlMs <= 0) return;
    this.misses.delete(slug);
    if (this.misses.size >= this.maxEntries) {
      const oldest = this.misses.keys().next().value;
      if (oldest !== undefined) this.misses.delete(oldest);
    }
    this.misses.set(slug, this.clock() + this.ttlMs);
  }
}

function redirect(location: string): RedirectResult {
  return {
    status: 302,
    headers: {
      location,
      "cache-control": "no-store, no-cache, must-revalidate",
      "referrer-policy": "no-referrer",
      "content-type": "text/plain",
    },
    body: `redirecting to ${location}\n`,
  };
}

const text = (status: number, body: string, extra?: Record<string, string>): RedirectResult => ({
  status,
  headers: { "content-type": "text/plain", ...extra },
  body,
});

/**
 * Reserved internal paths that are never invite campaigns. Recognize only the
 * first segment, stripping literal/encoded leading slashes and decoding once.
 * A malformed suffix cannot unreserve a recognized `metrics`/`healthz` prefix;
 * near-miss campaign slugs like `metricsfoo` still do not match.
 */
export function isReservedInternal(path: string): boolean {
  const bare = path.split("?")[0] ?? "/";
  const prefix = bare.replace(/^(?:\/|%2f)+/i, "").split(/\/|%2f/i, 1)[0] ?? "";
  try {
    return RESERVED_SLUGS.includes(decodeURIComponent(prefix).toLowerCase());
  } catch {
    // An undecodable prefix is not provably reserved; callers fail closed.
    return false;
  }
}

export async function handleRedirect(
  method: string,
  path: string,
  callerKey: string,
  deps: RedirectDeps,
): Promise<RedirectResult> {
  // Query strings are dropped, not parsed: they are the identifying data this
  // service promises not to collect.
  const bare = path.split("?")[0] ?? "/";

  // Reserved paths reject every method before configuration, logging or lookup.
  // Only the exact probe is exempt; aliases/subpaths never become campaigns.
  if (bare !== "/healthz" && isReservedInternal(path)) {
    return text(404, "not found\n");
  }

  const error = deps.onError ?? (() => undefined);
  if (!isValidFallback(deps.fallbackInviteCode)) {
    error("invite_redirect_invalid_config", { errorClass: "invalid_fallback" });
    return text(503, "redirect service misconfigured\n", { "retry-after": "30" });
  }

  if (bare === "/healthz") {
    return method === "GET" || method === "HEAD"
      ? text(200, "ok\n")
      : text(405, "", { allow: "GET, HEAD" });
  }

  if (method !== "GET" && method !== "HEAD") {
    return text(405, "", { allow: "GET, HEAD" });
  }

  if (bare === "/favicon.ico" || bare === "/robots.txt") {
    return text(404, "not found\n");
  }

  // Per-caller cap before the database is touched. The key picks a bucket and
  // never leaves this function.
  if (deps.isThrottled?.(callerKey)) {
    return text(429, "slow down\n", { "retry-after": "1" });
  }

  let slug: string;
  try {
    slug = decodeURIComponent(
      bare.replace(/^\/+/, "").replace(/\/+$/, ""),
    ).toLowerCase();
  } catch {
    return text(404, "not found\n");
  }

  // Bare domain: redirect without counting — nobody clicked a tracked link.
  if (slug === "") {
    if (deps.fallbackInviteCode && isValidInviteCode(deps.fallbackInviteCode)) {
      return redirect(inviteUrl(deps.fallbackInviteCode));
    }
    return text(404, "not found\n");
  }

  // Malformed paths never consume cache slots or touch the store. Cached
  // misses still pass the caller cap above; cache hits do not extend the TTL.
  if (!isValidSlug(slug) || deps.missCache?.has(slug)) {
    return text(404, "not found\n");
  }

  let campaign: Campaign | null;
  try {
    campaign = await deps.lookup(slug);
  } catch (err) {
    error("invite_redirect_lookup_failed", {
      slug,
      errorClass: redirectErrorClass(err),
    });
    if (deps.fallbackInviteCode && isValidInviteCode(deps.fallbackInviteCode)) {
      return redirect(inviteUrl(deps.fallbackInviteCode));
    }
    return text(503, "temporarily unavailable\n", { "retry-after": "30" });
  }

  if (campaign === null) {
    deps.missCache?.add(slug);
    return text(404, "not found\n");
  }

  if (!isValidInviteCode(campaign.inviteCode)) {
    error("invite_redirect_bad_code", {
      slug,
      errorClass: "invalid_invite_code",
    });
    return text(500, "misconfigured campaign\n");
  }

  const res = redirect(inviteUrl(campaign.inviteCode));

  // HEAD is how link previewers ask — not a person, not a click.
  if (method === "HEAD") return res;

  // Redirect first, record after: attached by the caller via waitUntil (Worker)
  // or awaited in tests. A failed write costs a click, never a member.
  const click: RedirectClick = {
    guildId: deps.guildId,
    code: campaign.inviteCode,
    campaign: campaign.slug,
    occurredAt: new Date((deps.now ?? Date.now)()).toISOString(),
    dedupeToken: (deps.uuid ?? (() => Math.random().toString(36).slice(2)))(),
    source: `invite:${campaign.inviteCode}`,
  };
  return { ...res, click };
}
