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
 *   redirect). Invalid stored code → 500, nothing recorded.
 * - Slugs: lowercase alnum + internal hyphens, 2–40 chars; lookup is
 *   case-insensitive, tolerates leading/trailing slashes, percent-decodes
 *   (malformed escape → 404). Disabled campaigns still redirect.
 * - Privacy (docs/PRIVACY.md in two-bot): a click is a campaign + timestamp.
 *   No cookies, IP, UA, referrer — nothing about the visitor is stored,
 *   logged, or set. The caller key for rate limiting never survives the call.
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
export function isValidFallback(code: string | null | undefined): boolean {
  return code == null || code === "" || isValidInviteCode(code);
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
 */
export class TokenBuckets {
  private buckets = new Map<string, { tokens: number; updatedAt: number }>();
  private spec: { capacity: number; refillPerSecond: number };
  private clock: () => number;

  constructor(
    spec: { capacity: number; refillPerSecond: number } = {
      capacity: 60,
      refillPerSecond: 1,
    },
    now: () => number = Date.now,
  ) {
    this.spec = spec;
    this.clock = now;
  }

  take(key: string): { allowed: boolean; retryAfter: number } {
    const t = this.clock();
    const b = this.buckets.get(key) ?? {
      tokens: this.spec.capacity,
      updatedAt: t,
    };
    const elapsed = Math.max(0, (t - b.updatedAt) / 1000);
    b.tokens = Math.min(
      this.spec.capacity,
      b.tokens + elapsed * this.spec.refillPerSecond,
    );
    b.updatedAt = t;
    if (b.tokens >= 1) {
      b.tokens -= 1;
      this.buckets.set(key, b);
      return { allowed: true, retryAfter: 0 };
    }
    this.buckets.set(key, b);
    const wait = Math.ceil(
      (1 - b.tokens) / this.spec.refillPerSecond,
    );
    return { allowed: false, retryAfter: Math.max(1, wait) };
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
 * Reserved internal paths that are never invite campaigns. Canonicalizes
 * using slash stripping, percent-decoding and lowercase matching (also strip
 * decoded edge slashes). `metrics` and `healthz` aliases/subpaths all match;
 * near-miss campaign slugs like `metricsfoo` do not. Undecodable input is not provably
 * reserved — callers still fail closed downstream (404 for GET/HEAD).
 */
export function isReservedInternal(path: string): boolean {
  const bare = path.split("?")[0] ?? "/";
  let canonical: string;
  try {
    canonical = decodeURIComponent(
      bare.replace(/^\/+/, "").replace(/\/+$/, ""),
    ).replace(/^\/+/, "").replace(/\/+$/, "").toLowerCase();
  } catch {
    // Undecodable: not provably reserved; the caller fails closed downstream
    // (404 for GET/HEAD, 405 for other methods).
    return false;
  }
  return RESERVED_SLUGS.some(
    (slug) => canonical === slug || canonical.startsWith(`${slug}/`),
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

  // Reject malformed input before lookup or logging: slugs are bounded labels,
  // not arbitrary visitor-supplied paths that may contain identifying data.
  if (!isValidSlug(slug)) return text(404, "not found\n");

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
