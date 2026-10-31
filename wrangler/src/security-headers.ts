/**
 * Decided Worker response hardening (TOG-22701).
 *
 * Every Worker-constructed response (probe/metrics/control JSON and text,
 * redirect mapping answers) carries this set:
 * - `x-content-type-options: nosniff` — a text/plain probe or JSON error must
 *   never be MIME-sniffed into scriptable content.
 * - `referrer-policy: no-referrer` — matches the long-standing 302 on the
 *   redirect path; machine-consumed responses leak no referrer either way.
 * - `x-frame-options: DENY` — deny framing outright. Machine-consumed text
 *   and JSON need no CSP; the HTML dashboard adds its own strict CSP, including
 *   `frame-ancestors 'none'`, in `dashboardPageResponse`.
 *
 * Proxied container bytes pass through untouched; only Worker-built status,
 * headers and bodies are hardened. `redirect.ts` mirrors this set inline: it
 * embeds standalone in the Miniflare acceptance (see redirect-cache.test.ts)
 * and cannot import this module, so the security-headers test pins both to
 * the same values.
 */

/** The decided header set. Values are lowercase-safe literals. */
export const SECURITY_HEADERS: Record<string, string> = {
  "x-content-type-options": "nosniff",
  "referrer-policy": "no-referrer",
  "x-frame-options": "DENY",
};

/**
 * Merge `extra` plain-object headers with the decided set. The decided set
 * wins on collision, so callers cannot weaken it; all other headers
 * (`content-type`, `cache-control`, `retry-after`, `allow`, `location`,
 * `www-authenticate`) pass through.
 */
export function withSecurityHeaders(
  extra?: Record<string, string>,
): Record<string, string> {
  return { ...extra, ...SECURITY_HEADERS };
}

/** Stamp the decided set onto a mutable `Headers` (e.g. a copied response). */
export function applySecurityHeaders(headers: Headers): void {
  for (const [name, value] of Object.entries(SECURITY_HEADERS)) {
    headers.set(name, value);
  }
}
