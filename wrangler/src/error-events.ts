/**
 * Release-tagged error events for Workers Logs (release-tagged error
 * tracking grouped by error_class).
 *
 * Workers Logs is the tracker: every structured Worker error event carries
 * `release` (the container git SHA from /readyz `build_revision`, stamped at
 * deploy time from `GITHUB_SHA` via `scripts/staging_rollout.py`) so a new
 * error after a deploy points at that release, plus `worker_version`
 * (`CF_VERSION_METADATA.id`) to join the deploy receipt. `error_class`
 * stays the grouping key. Strict tokens only: the container controls the
 * probe body, so anything that is not a hex SHA becomes the visible
 * `"unknown"` bucket instead of echoed text. No secrets, URLs, headers or
 * bodies are ever logged here.
 */

/** Git SHA, full (40) or abbreviated (7+), lowercase hex only. */
const RELEASE_TOKEN = /^[0-9a-f]{7,40}$/;

/** Cloudflare Worker version id (UUID); anything else is untrusted. */
const WORKER_VERSION_TOKEN = /^[0-9A-Za-z-]{8,64}$/;

/** Mirrors the keepalive bounded /readyz reader in index.ts. */
const MAX_PROBE_BODY_BYTES = 64 * 1024;

export const UNKNOWN_RELEASE = "unknown";

/** Extract the container git SHA from a /readyz body, or null. */
export function parseRelease(body: string): string | null {
  if (new TextEncoder().encode(body).byteLength > MAX_PROBE_BODY_BYTES) return null;
  try {
    const report: unknown = JSON.parse(body);
    const revision = (report as { build_revision?: unknown } | null)?.build_revision;
    if (typeof revision === "string" && RELEASE_TOKEN.test(revision)) return revision;
  } catch {
    // Not the bot's JSON: nothing to report.
  }
  return null;
}

/** Render a parsed release for a log line; unparsed stays visible. */
export function releaseTag(release: string | null): string {
  return release ?? UNKNOWN_RELEASE;
}

/** Render the platform Worker version for a log line; untrusted stays visible. */
export function workerVersionTag(id: unknown): string {
  return typeof id === "string" && WORKER_VERSION_TOKEN.test(id) ? id : UNKNOWN_RELEASE;
}
