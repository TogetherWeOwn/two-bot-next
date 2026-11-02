/**
 * Release-tagged error events for Workers Logs.
 *
 * Workers Logs is the tracker: the alert and failure lines carry `release`
 * (the container git SHA from /readyz `build_revision`, stamped at deploy
 * time: staging uses `GITHUB_SHA` via `scripts/staging_rollout.py`, and
 * production uses the deploy SHA via `scripts/production_deploy.py`; lines
 * logged where the tick parsed no probe body carry the visible `"unknown"`
 * bucket) so a new error after a deploy points at that release, plus
 * `worker_version` (`CF_VERSION_METADATA.id`) to join the deploy receipt.
 * Group gateway
 * failures by `class` and metrics alerts by the alert rule in `content`; a
 * validated last error class appears in content only for
 * `job_consecutive_failures` alerts.
 * Strict tokens only: the container controls the
 * probe body, so anything that is not a hex SHA becomes the visible
 * `"unknown"` bucket instead of echoed text. No secrets, URLs, headers or
 * bodies are ever logged here.
 */

/** Git SHA, full (40) or abbreviated (7+), lowercase hex only. */
export const RELEASE_TOKEN = /^[0-9a-f]{7,40}$/;

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

/**
 * Render an edge-supplied release for a span log line. Only a hex SHA
 * reaches the line; anything else (absent var, probe miss, hostile value)
 * becomes the visible `"unknown"` bucket instead of echoed text.
 */
export function normalizeRelease(value: unknown): string {
  return typeof value === "string" && RELEASE_TOKEN.test(value) ? value : UNKNOWN_RELEASE;
}

/** Render the platform Worker version for a log line; untrusted stays visible. */
export function workerVersionTag(id: unknown): string {
  return typeof id === "string" && WORKER_VERSION_TOKEN.test(id) ? id : UNKNOWN_RELEASE;
}
