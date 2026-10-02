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
 *   denied → 429 + the bucket's own `retry-after` (whole seconds, at least 1:
 *   the refill wait, what remains of a terminal hold, or the idle window for
 *   a cap refusal), nothing recorded.
 * - Caller keys are canonicalized before bucketing (case, surrounding
 *   whitespace, IPv6 zone id, `::ffff:`-mapped quad and empty/missing all
 *   share one bucket), so aliases cannot multiply quota. Callers with no edge
 *   signal share one bounded unknown budget that cannot touch valid callers.
 *   A key denied N times in a row is held terminal for a bounded cooldown;
 *   retries during the hold neither consume nor extend it.
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
  /**
   * Rate-limit verdict for this caller (`TokenBuckets.take`). A denial answers
   * 429 with the verdict's `retryAfter`, so a held caller learns the real
   * remaining hold instead of a one-second retry loop.
   */
  throttle?: (callerKey: string) => ThrottleVerdict;
  /** Shared by requests in one isolate; caches only confirmed missing slugs. */
  missCache?: RedirectMissCache;
}

/** One bucket decision. `retryAfter` is whole seconds; 0 when allowed. */
export interface ThrottleVerdict {
  allowed: boolean;
  retryAfter: number;
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
 * Canonical caller identity for quota accounting (TOG-12469).
 *
 * Every `take` runs the raw key through this first, so aliases of one caller
 * share one bucket and quota cannot be multiplied by spelling:
 * - non-strings, empty and whitespace-only keys share the single `unknown`
 *   budget; the literal `unknown` (any case) folds into it too, so traffic
 *   with no edge signal can never mint map entries or touch valid callers;
 * - ASCII case is folded (IPv6 hex is case-insensitive);
 * - an IPv6 zone id (`fe80::1%eth0`) is stripped — it names an interface,
 *   not a caller;
 * - `::ffff:a.b.c.d` folds to the quad, so one stack is not two callers.
 *
 * Keys are capped at 256 chars to bound per-entry memory; truncation can only
 * merge long keys into a shared bucket, never split one caller into two.
 */
export function canonicalCallerKey(raw: unknown): string {
  if (typeof raw !== "string") return "unknown";
  let key = raw.trim().toLowerCase();
  if (key === "" || key === "unknown") return "unknown";
  const zone = key.indexOf("%");
  if (zone >= 0) key = key.slice(0, zone);
  const mapped = /^::ffff:(\d{1,3}(?:\.\d{1,3}){3})$/.exec(key);
  if (mapped) key = mapped[1] as string;
  if (key === "") return "unknown";
  return key.length > 256 ? key.slice(0, 256) : key;
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
 * - Cardinality cap: at most `maxBuckets` entries. A new key at the cap first
 *   reaps idle-expired entries; it is denied (fail closed) only if the map is
 *   still full of live ones, so overflow sheds load and never disables
 *   throttling. Buckets already tracked are unaffected by the cap.
 * - Bounded cleanup: every `take` touches its key (LRU order, most-recent at
 *   the back) then reaps at most `sweepBudget` idle-expired entries from the
 *   front; a new key at the cap runs the same reap before its verdict. Per-call
 *   work is O(sweepBudget), independent of map size.
 * - Retry lifetime (TOG-12469): `maxConsecutiveDenials` denials in a row put
 *   the key in a terminal hold until `terminalCooldownMs` elapses. Takes
 *   during the hold are refused without touching tokens and without extending
 *   the hold, so a retry loop can neither succeed early nor keep the hold
 *   alive; one success resets the streak. A held entry is enforcement state,
 *   never idle: expiry and the sweep skip it. Like the Rust `CooldownGovernor`,
 *   this only refuses — it never queues, sleeps or retries. The verdict
 *   carries the remaining hold, and the redirect 429 forwards it.
 * - Refill accrues across a hold (TOG-12533, deliberate): refill is measured
 *   from the last pre-hold take, capped at capacity, so a caller that honors
 *   the advertised wait returns to the bucket any idle caller would have.
 *   The hold bounds the denial streak, not admissions: admits stay within
 *   `capacity + refillPerSecond × elapsed` with or without it (review sim,
 *   100 rps for 600 s: 600 admits with the hold, 659 without). Withholding
 *   refill would leave a caller that obeys `retry-after` no better off than
 *   one that ignores it.
 */
export class TokenBuckets {
  private buckets = new Map<
    string,
    { tokens: number; updatedAt: number; denials: number; heldUntil?: number }
  >();
  private spec: { capacity: number; refillPerSecond: number };
  private clock: () => number;
  private idleTtlMs: number;
  private maxBuckets: number;
  private sweepBudget: number;
  private maxConsecutiveDenials: number;
  private terminalCooldownMs: number;

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
      maxConsecutiveDenials?: number;
      terminalCooldownMs?: number;
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
    // Terminal hold after sustained over-budget hammering. The default (25
    // straight denials → 60s hold) sits far above legitimate burst+retry
    // traffic, so ordinary callers never see it; hammering callers do.
    this.maxConsecutiveDenials = Math.max(
      1,
      Number.isFinite(limits.maxConsecutiveDenials)
        ? Math.floor(limits.maxConsecutiveDenials as number)
        : 25,
    );
    this.terminalCooldownMs = Math.max(
      1,
      Number.isFinite(limits.terminalCooldownMs)
        ? Math.floor(limits.terminalCooldownMs as number)
        : 60_000,
    );
  }

  /** Tracked buckets. For tests/observability; keys are never logged. */
  get size(): number {
    return this.buckets.size;
  }

  take(key: string): ThrottleVerdict {
    // Canonicalize first: aliases share one bucket, and every no-signal
    // caller shares the single `unknown` budget instead of minting entries.
    const canonical = canonicalCallerKey(key);
    const t = this.clock();
    let b = this.buckets.get(canonical);
    if (b && b.heldUntil !== undefined) {
      if (t < b.heldUntil) {
        // Terminal hold: refuse without touching tokens, recency or the hold
        // itself, so retries can neither succeed early nor extend the work.
        return {
          allowed: false,
          retryAfter: Math.max(1, Math.ceil((b.heldUntil - t) / 1000)),
        };
      }
      // Hold elapsed: clear the streak and fall through; idle expiry below
      // may additionally restart the bucket full.
      delete b.heldUntil;
      b.denials = 0;
    }
    if (b && Math.max(0, t - b.updatedAt) >= this.idleTtlMs) {
      // Untouched for a full refill window: dropping restarts it full, exactly
      // as capped refill would. Cannot apply to a key that still owes tokens
      // (the TTL floor guarantees that), so expiry never resets a throttle.
      this.buckets.delete(canonical);
      b = undefined;
    }
    if (!b) {
      if (this.buckets.size >= this.maxBuckets) {
        // Reap first, so a map filled with one-time keys drains on new-key
        // traffic alone instead of waiting for a tracked key to return.
        this.sweep(t);
      }
      if (this.buckets.size >= this.maxBuckets) {
        // Still full of live entries. Fail closed: shed the unknown caller and
        // keep every tracked throttle.
        return {
          allowed: false,
          retryAfter: Math.max(1, Math.ceil(this.idleTtlMs / 1000)),
        };
      }
      b = { tokens: this.spec.capacity, updatedAt: t, denials: 0 };
    }
    const elapsed = Math.max(0, (t - b.updatedAt) / 1000);
    b.tokens = Math.min(
      this.spec.capacity,
      b.tokens + elapsed * this.spec.refillPerSecond,
    );
    b.updatedAt = t;
    // Move to the back: map order is recency order, so the sweep below always
    // reaps the least-recently-touched entries first.
    this.buckets.delete(canonical);
    let verdict: ThrottleVerdict;
    if (b.tokens >= 1) {
      b.tokens -= 1;
      b.denials = 0;
      verdict = { allowed: true, retryAfter: 0 };
    } else {
      const wait = Math.ceil(
        (1 - b.tokens) / this.spec.refillPerSecond,
      );
      b.denials += 1;
      if (b.denials >= this.maxConsecutiveDenials) {
        // Bounded retry lifetime: this streak is over. The hold starts now
        // and the verdict advertises it, so the caller learns the terminal
        // wait on the denial that earned it.
        b.heldUntil = t + this.terminalCooldownMs;
        verdict = {
          allowed: false,
          retryAfter: Math.max(1, Math.ceil(this.terminalCooldownMs / 1000)),
        };
      } else {
        verdict = { allowed: false, retryAfter: Math.max(1, wait) };
      }
    }
    this.buckets.set(canonical, b);
    this.sweep(t);
    return verdict;
  }

  /** Visit up to `sweepBudget` entries from the least-recent end. */
  private sweep(t: number): void {
    let budget = this.sweepBudget;
    for (const [key, b] of this.buckets) {
      if (budget <= 0) return;
      budget -= 1;
      // A live hold is enforcement state, not idle: skip it without stopping
      // the reap behind it. The visit cap above keeps per-call work bounded
      // even when many holds are live at once.
      if (b.heldUntil !== undefined && t < b.heldUntil) continue;
      // Front is live: recency order means everything behind it is newer.
      if (Math.max(0, t - b.updatedAt) < this.idleTtlMs) return;
      this.buckets.delete(key);
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

/** Whole seconds, at least 1; a non-finite wait falls back to the legacy 1. */
const retryAfterSeconds = (seconds: number): number =>
  Number.isFinite(seconds) ? Math.max(1, Math.ceil(seconds)) : 1;

/**
 * Reserved internal paths that are never invite campaigns. Recognize only the
 * first segment, stripping literal/encoded leading slashes and decoding once.
 * A malformed suffix cannot unreserve a recognized `metrics`/`healthz` prefix;
 * near-miss campaign slugs like `metricsfoo` still do not match.
 * The ownership-fence control plane (`internal/ownership`, plus subpaths) is
 * likewise reserved: it is served by the Worker (TOG-11143), never a campaign.
 */
export function isReservedInternal(path: string): boolean {
  const bare = path.split("?")[0] ?? "/";
  const prefix = bare.replace(/^(?:\/|%2f)+/i, "").split(/\/|%2f/i, 1)[0] ?? "";
  try {
    if (RESERVED_SLUGS.includes(decodeURIComponent(prefix).toLowerCase())) {
      return true;
    }
  } catch {
    // An undecodable prefix is not provably reserved; callers fail closed.
    return false;
  }
  let canonical: string;
  try {
    canonical = decodeURIComponent(
      bare.replace(/^\/+/, "").replace(/\/+$/, ""),
    ).toLowerCase();
  } catch {
    return false;
  }
  return (
    canonical === "internal/ownership" ||
    canonical.startsWith("internal/ownership/")
  );
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

  // Per-caller cap before the database is touched. The bucket canonicalizes
  // the key (aliases share one quota; no-signal callers share one bounded
  // unknown budget), and a sustained-denial hold is terminal for its
  // cooldown. The key never leaves this function; only the verdict's wait
  // does, so a held caller is told the real remaining hold.
  const verdict = deps.throttle?.(callerKey);
  if (verdict && !verdict.allowed) {
    return text(429, "slow down\n", {
      "retry-after": String(retryAfterSeconds(verdict.retryAfter)),
    });
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
