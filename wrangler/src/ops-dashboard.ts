/**
 * Read-only ops dashboard (TOG-22861): a static Worker-served page polling
 * `GET /ops/summary`.
 *
 * - Human auth is Cloudflare Access service tokens on the summary: every
 *   request to `GET /ops/summary` must carry both `CF-Access-Client-Id` and
 *   `CF-Access-Client-Secret` headers. The page itself is static and ungated
 *   so a browser can open it; it holds no data and only fetches the summary.
 *   The edge Access app (Service Auth rule, strict mode) validates the
 *   summary first; the Worker re-validates against its own
 *   `OPS_DASHBOARD_CLIENT_ID` / `OPS_DASHBOARD_CLIENT_SECRET` Worker secrets
 *   so a direct `workers.dev` hit cannot bypass the gate either. Strict mode
 *   semantics: both headers on every summary request, no cookies or
 *   sessions, and failures are 401 text (never a login redirect).
 * - Strictly read-only: the page issues only `GET /ops/summary` fetches and
 *   the summary proxies only the container's `GET /ops/summary` roll-up JSON
 *   (never `/readyz`; the DO maps `ready: false` to 503). All mutations
 *   stay on their existing API routes; nothing here accepts POST/PUT/DELETE.
 * - Break-glass: staging and production both keep their `workers.dev` routes
 *   (see wrangler.toml) until a custom domain and Access app exist; either
 *   flips to the custom domain in a separate change.
 *
 * Secrets live only as Worker secrets (never wrangler.toml vars — enforced by
 * scripts/check-env-bindings.py) and are never logged.
 */

import { withSecurityHeaders } from "./security-headers.ts";

/** Exact paths served by the outer Worker (before the redirect handler). */
export const OPS_DASHBOARD_PATH = "/ops/dashboard";
export const OPS_SUMMARY_PATH = "/ops/summary";

/** Service-token headers, as sent by Access service-token clients. */
export const DASHBOARD_CLIENT_ID_HEADER = "cf-access-client-id";
export const DASHBOARD_CLIENT_SECRET_HEADER = "cf-access-client-secret";

/** The client secret follows the ownership/scrape-token floor. */
export const MIN_DASHBOARD_SECRET_LENGTH = 32;
/** Client IDs are UUID-shaped; this floor only rejects typos, not attackers. */
export const MIN_DASHBOARD_CLIENT_ID_LENGTH = 8;
/** Bound the digest input like the metrics bearer cap. */
const MAX_CREDENTIAL_CHARS = 4096;

/** Minimal shape this module needs; the full Worker `Env` satisfies it. */
export interface DashboardEnv {
  OPS_DASHBOARD_CLIENT_ID?: string;
  OPS_DASHBOARD_CLIENT_SECRET?: string;
}

/** Compare via digests so length/prefix timing does not leak the secret. */
async function tokenMatches(provided: string, expected: string): Promise<boolean> {
  const enc = new TextEncoder();
  const [a, b] = await Promise.all([
    crypto.subtle.digest("SHA-256", enc.encode(provided)),
    crypto.subtle.digest("SHA-256", enc.encode(expected)),
  ]);
  const x = new Uint8Array(a);
  const y = new Uint8Array(b);
  let diff = 0;
  for (let i = 0; i < x.length; i++) diff |= x[i]! ^ y[i]!;
  return diff === 0;
}

// A short Operator secret is a misconfiguration, not per-request
// information: say so once per isolate so scanners cannot flood the logs.
let dashboardShortSecretLogged = false;

/**
 * Service-token gate for the summary. The static page is ungated (it holds
 * no data); only `/ops/summary` calls this. Returns `null` when the request
 * is authorized, else the refusal to send:
 * - unconfigured or short Operator secrets → 404 (the route does not exist);
 * - missing/wrong/oversize credentials → 401.
 * Never throws and never echoes secrets.
 */
export async function dashboardRefusal(
  request: Request,
  env: DashboardEnv,
): Promise<Response | null> {
  const clientId = env.OPS_DASHBOARD_CLIENT_ID;
  const clientSecret = env.OPS_DASHBOARD_CLIENT_SECRET;
  if (
    !clientId || clientId.length < MIN_DASHBOARD_CLIENT_ID_LENGTH
    || !clientSecret || clientSecret.length < MIN_DASHBOARD_SECRET_LENGTH
  ) {
    if (
      ((clientSecret && clientSecret.length < MIN_DASHBOARD_SECRET_LENGTH)
        || (clientId && clientId.length < MIN_DASHBOARD_CLIENT_ID_LENGTH))
      && !dashboardShortSecretLogged
    ) {
      dashboardShortSecretLogged = true;
      console.error(JSON.stringify({
        event: "dashboard_token_misconfigured",
        reason: "credential_below_minimum_length",
      }));
    }
    return new Response("not found", { status: 404, headers: withSecurityHeaders() });
  }
  const providedId = request.headers.get(DASHBOARD_CLIENT_ID_HEADER) ?? "";
  const providedSecret = request.headers.get(DASHBOARD_CLIENT_SECRET_HEADER) ?? "";
  if (
    providedId.length > MAX_CREDENTIAL_CHARS
    || providedSecret.length > MAX_CREDENTIAL_CHARS
    || !(await tokenMatches(providedId, clientId))
    || !(await tokenMatches(providedSecret, clientSecret))
  ) {
    return new Response("unauthorized", { status: 401, headers: withSecurityHeaders() });
  }
  return null;
}

/** The static dashboard page: no external resources, only `GET` fetches. */
export const DASHBOARD_HTML = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>two-bot-next ops dashboard (read-only)</title>
<style>
body { font-family: system-ui, sans-serif; margin: 2rem; max-width: 60rem; }
table { border-collapse: collapse; margin: 1rem 0; }
th, td { border: 1px solid #999; padding: 0.3rem 0.6rem; text-align: left; }
.ok { color: #0a0; } .bad { color: #a00; }
#auth input { margin-right: 0.5rem; }
.muted { color: #555; font-size: 0.9rem; }
</style>
</head>
<body>
<h1>two-bot-next ops dashboard (read-only)</h1>
<p class="muted">Polls <code>/ops/summary</code> every 15 s. Read-only: this page
never sends anything but <code>GET</code>, and the summary only reports status.
All changes go through their own API routes.</p>
<section id="auth">
<label>CF-Access-Client-Id <input id="clientId" type="text" autocomplete="off"></label>
<label>CF-Access-Client-Secret <input id="clientSecret" type="password" autocomplete="off"></label>
<button id="connect" type="button">Connect</button>
<button id="disconnect" type="button">Disconnect</button>
<span id="status" class="muted">disconnected</span>
</section>
<section id="summary"><p class="muted">No data yet.</p></section>
<script>
const POLL_MS = 15000;
const POLL_DEADLINE_MS = 10000;
const FRESH_MS = 30000;
let timer = null;
let freshnessTimer = null;
let activePoll = null;
let lastObserved = null;
// Generation/order fences remain authoritative even if a transport ignores
// abort. One active attempt owns one deadline across both headers and body.
let connectGen = 0;
let pollSeq = 0;
let lastAppliedSeq = 0;
function clearFreshness() {
  if (freshnessTimer !== null) clearTimeout(freshnessTimer);
  freshnessTimer = null;
}
function unavailable(message) {
  clearFreshness();
  document.getElementById("summary").innerHTML = '<p class="bad">' + esc(message) + '</p>'
    + '<p class="muted">' + (lastObserved ? 'Last observed ' + esc(lastObserved) + '.' : 'No observation yet.') + '</p>';
}
function creds() {
  return {
    id: document.getElementById("clientId").value,
    secret: document.getElementById("clientSecret").value,
  };
}
async function poll() {
  if (activePoll) activePoll.cancel();
  const gen = connectGen;
  const seq = ++pollSeq;
  const controller = new AbortController();
  let release;
  const cancelled = new Promise((resolve) => { release = resolve; });
  const attempt = {
    retired: false,
    cancel() {
      attempt.retired = true;
      clearTimeout(deadline);
      controller.abort();
      release(null);
      if (activePoll === attempt) activePoll = null;
    },
  };
  function obsolete() {
    return attempt.retired || gen !== connectGen || seq <= lastAppliedSeq;
  }
  const status = document.getElementById("status");
  const deadline = setTimeout(() => {
    if (!obsolete()) {
      lastAppliedSeq = seq;
      status.textContent = "summary timed out";
      unavailable("Summary unavailable: request timed out. Wait for automatic retry or reconnect.");
    }
    attempt.cancel();
  }, POLL_DEADLINE_MS);
  activePoll = attempt;
  // Race the entire read, not just fetch(). Cancellation releases the poll
  // even when a non-cooperative promise never settles; the race observes any
  // eventual rejection and the retired fence prevents late DOM writes.
  async function read() {
    const { id, secret } = creds();
    let res;
    try {
      res = await fetch("/ops/summary", {
        headers: { "cf-access-client-id": id, "cf-access-client-secret": secret },
        signal: controller.signal,
      });
    } catch (e) {
      return { failure: "fetch failed", message: "summary unavailable: fetch failed" };
    }
    if (obsolete()) return null;
    const contentType = res.headers.get("content-type") || "";
    const isReadinessJson = contentType.indexOf("application/json") !== -1
      && (res.status === 200 || res.status === 503);
    if (!isReadinessJson) {
      return { failure: "summary " + res.status, message: "summary request failed: " + res.status };
    }
    try {
      return { data: await res.json(), status: res.status };
    } catch (e) {
      return { failure: "summary " + res.status, message: "summary request failed: " + res.status };
    }
  }
  try {
    const result = await Promise.race([read(), cancelled]);
    if (!result || obsolete()) return;
    if (result.failure) {
      lastAppliedSeq = seq;
      status.textContent = result.failure;
      unavailable(result.message + ". Wait for automatic retry or check access credentials and reconnect.");
      return;
    }
    let html;
    try {
      html = render(result.data, result.status);
    } catch (e) {
      lastAppliedSeq = seq;
      status.textContent = "summary " + result.status;
      unavailable("summary report invalid: " + result.status + ". Wait for retry or check the bot report.");
      return;
    }
    lastAppliedSeq = seq;
    lastObserved = new Date().toISOString();
    status.textContent = "updated " + lastObserved;
    document.getElementById("summary").innerHTML = html;
    clearFreshness();
    freshnessTimer = setTimeout(() => {
      if (gen !== connectGen || seq !== lastAppliedSeq) return;
      status.textContent = "summary stale";
      unavailable("Summary stale: observation expired. Wait for automatic retry or reconnect.");
    }, FRESH_MS);
  } finally {
    attempt.cancel();
  }
}
function esc(s) {
  return String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
}
// Intake bounds: current reports carry a handful of each, so 256 is 10x+
// headroom while bounding rendered rows. Each collection is capped
// independently: either one over its cap refuses the whole report before
// render, so a hostile or bloated summary can never render unbounded rows.
const MAX_COMPONENTS = 256;
const MAX_JOBS = 256;
function isValidReport(data) {
  // Mirror the server summary shape: an object whose components (when
  // present) are [name, state] string pairs and whose jobs (when present)
  // map names to objects. Anything else (null entries, null job values,
  // non-pair entries, non-object reports) is malformed: rendering it would
  // either throw or show a vacuous READY, so reject it before render.
  if (!data || typeof data !== "object" || Array.isArray(data)) return false;
  if (data.components !== undefined) {
    if (!Array.isArray(data.components)) return false;
    if (data.components.length > MAX_COMPONENTS) return false;
    for (const entry of data.components) {
      if (!Array.isArray(entry) || entry.length !== 2
        || typeof entry[0] !== "string" || typeof entry[1] !== "string") return false;
    }
  }
  if (data.jobs !== undefined) {
    if (!data.jobs || typeof data.jobs !== "object" || Array.isArray(data.jobs)) return false;
    if (Object.keys(data.jobs).length > MAX_JOBS) return false;
    for (const job of Object.values(data.jobs)) {
      if (!job || typeof job !== "object" || Array.isArray(job)) return false;
    }
  }
  return true;
}
function render(data, status) {
  if (!isValidReport(data)) throw new Error("invalid summary report");
  const comps0 = Array.isArray(data && data.components) ? data.components : [];
  // Readiness mirrors the server HealthReport ready rule: every listed
  // component reports "ready" (vacuous truth when none are listed). The
  // readyz body carries no ready field, so never read one. When the poll
  // status is known it decides directly (200 vs 503).
  const ready = typeof status === "number"
    ? status === 200
    : comps0.every(function (c) { return c && c[1] === "ready"; });
  let html = '<h2 class="' + (ready ? 'ok' : 'bad') + '">' + (ready ? 'READY' : 'NOT READY') + '</h2>';
  const comps = Array.isArray(data && data.components) ? data.components : [];
  html += "<h3>Components</h3><table><tr><th>name</th><th>state</th></tr>";
  for (const [name, state] of comps) html += "<tr><td>" + esc(name) + "</td><td>" + esc(state) + "</td></tr>";
  html += "</table>";
  const jobs = (data && data.jobs) || {};
  html += "<h3>Jobs</h3><table><tr><th>job</th><th>last success</th><th>failures</th></tr>";
  for (const [name, job] of Object.entries(jobs)) {
    html += "<tr><td>" + esc(name) + "</td><td>" + esc(job.last_success ?? "?") + "</td><td>" + esc(job.consecutive_failures ?? 0) + "</td></tr>";
  }
  html += "</table>";
  if (data && data.gateway_failure) {
    html += "<p>gateway failure: " + esc(data.gateway_failure.phase) + " / " + esc(data.gateway_failure.class) + "</p>";
  }
  html += '<p class="muted">build ' + esc((data && data.build_revision) || '?') + ' / ' + esc((data && data.build_id) || '?') + '</p>';
  return html;
}
document.getElementById("connect").addEventListener("click", () => {
  if (timer !== null) clearInterval(timer);
  connectGen++;
  lastAppliedSeq = 0;
  if (activePoll) activePoll.cancel();
  document.getElementById("status").textContent = "connecting";
  unavailable("Summary unavailable: connecting; prior observation is stale. Wait for a fresh report.");
  poll();
  timer = setInterval(poll, POLL_MS);
});
document.getElementById("disconnect").addEventListener("click", () => {
  if (timer !== null) { clearInterval(timer); timer = null; }
  connectGen++;
  if (activePoll) activePoll.cancel();
  document.getElementById("clientId").value = "";
  document.getElementById("clientSecret").value = "";
  document.getElementById("status").textContent = "disconnected";
  unavailable("Summary disconnected: prior observation is stale. Enter credentials and Connect to refresh.");
});
</script>
</body>
</html>
`;

// Hash the exact inline block bytes, including their surrounding newlines.
// The dashboard test checks these pins whenever the static page changes.
const DASHBOARD_CSP = [
  "default-src 'none'",
  "connect-src 'self'",
  "base-uri 'none'",
  "form-action 'none'",
  "frame-ancestors 'none'",
  "script-src 'sha256-D3Mj7MW5UP9M3/Y0JBDkauGJcAjNdmVv3UbplE0tXLY='",
  "style-src 'sha256-XfBF+coe4Stt3RC6YIku87CPj/YAhk41yequAyR3nkA='",
].join("; ");

/** Serve the static page: exact path, `GET` only, never cached. */
export function dashboardPageResponse(): Response {
  return new Response(DASHBOARD_HTML, {
    status: 200,
    headers: withSecurityHeaders({
      "content-type": "text/html; charset=utf-8",
      "cache-control": "no-store",
      "content-security-policy": DASHBOARD_CSP,
    }),
  });
}
