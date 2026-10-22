# Read-only ops dashboard (TOG-22861)

A static Worker-served page (`GET /ops/dashboard`) polling a JSON summary
(`GET /ops/summary`, a bounded proxy of the container's `/ops/summary`
roll-up, never `/readyz`). Strictly read-only: the page issues only `GET`
fetches and the summary only reports status. All mutations stay on their
existing API routes. The dashboard renders readiness from the HTTP status:
the DO rewrites a 200 roll-up with `ready: false` into a 503.

## Access (Operator setup)

The static page (`GET /ops/dashboard`) holds no data and is ungated so a
browser can open it directly. Human auth is Cloudflare Access service tokens
on the summary (`GET /ops/summary`), enforced twice:

1. **Edge (authoritative):** the Access app fronting the summary carries a
   Service Auth rule permitting the dashboard token, with organization
   **strict service-token mode** on: solely Service Auth rules count, no
   authorization cookie is issued, and every summary call repeats both headers.
   https://developers.cloudflare.com/cloudflare-one/identity/service-tokens/
2. **Worker (defense in depth):** the Worker re-validates both
   `CF-Access-Client-Id` and `CF-Access-Client-Secret` against its own
   secrets on every summary request, so a direct `workers.dev` hit cannot
   bypass the gate. Unconfigured/short secrets → 404; missing/wrong → 401
   (never a login redirect). Attempts are per-caller throttled (10-burst, 1/sec).
   The Worker also accepts the scrape bearer (`METRICS_SCRAPE_TOKEN`) on the
   same path for operator automation: the scrape gate runs first and its 429
   is a hard fence, while a non-throttled scrape refusal falls through to the
   service-token gate.

Open the page in any browser, then paste the service-token pair into its
Connect form; the page sends the headers on its `GET /ops/summary` fetches.

Install the pair as Worker secrets (never `wrangler.toml` vars):

```sh
wrangler versions secret put --env staging OPS_DASHBOARD_CLIENT_ID --message <sha>
wrangler versions secret put --env staging OPS_DASHBOARD_CLIENT_SECRET --message <sha>  # >= 32 chars
wrangler versions deploy --env staging
```

Stamp with the live `/readyz` `build_revision` of that environment: a plain
`wrangler secret put` publishes an unstamped version that can never form a rollback
receipt (see `docs/production-deploy.md`). Precheck first: `versions secret put`
builds on the newest-created version, not necessarily the serving one — confirm the
last block of `wrangler versions list --env <name>` (the list prints oldest first,
so the last block is the newest version) is the serving version (same id
as `wrangler deployments status --env <name>`), and if they differ redeploy the intended
SHA through the workflow first (full rule in `docs/production-deploy.md`).

Same for `--env production`. Closing the summary requires removing both the
dashboard pair and `METRICS_SCRAPE_TOKEN`: removing only the dashboard pair
leaves scrape-bearer access open (and vice versa); doubly-unconfigured still
404s, otherwise unauthenticated callers get 401. The static page stays
reachable (it holds no data).

## Observation freshness

- Connect immediately retires the prior observation while waiting for a fresh
  report. A valid report shows READY (200) or NOT READY (503), with its browser
  observation time (`updated`, UTC). This is a snapshot, not continuous health.
- Connected pages retry every 15 seconds. One request may be active; the next
  poll cancels superseded work. The **10-second total deadline** covers both
  response headers and JSON body consumption. Timeout, network/access failures
  and invalid reports replace readiness with an explicit failure and retry
  guidance; they never renew its observation time.
- After **30 seconds** without a new valid report, the panel becomes stale and
  loses success styling even if another poll has not run. The last observation
  time remains visible; it is historical, not evidence that the bot is healthy.
  Browser timers can pause in suspended tabs: keep the page active for monitoring
  and reconnect after resuming rather than trusting a frozen screen.
- Disconnect aborts pending work, stops polling/expiry timers, clears both
  credential inputs and labels the panel disconnected. Enter credentials and
  Connect to resume. Reconnect also aborts old work; late responses cannot
  restore a disconnected, timed-out or superseded observation.
- For unavailable/stale status, wait for the next retry or check connectivity
  and access credentials, then reconnect. A new valid report restores fresh
  readiness. A 503 NOT READY is a bot report, not a browser transport failure;
  use its component/job details for triage.

## Break-glass

- **Staging** keeps its `workers.dev` route (`workers_dev = true`) as the
  Access-independent origin. With the headers set, this bypasses a broken
  custom-domain route but never the service-token check:

  ```sh
  curl -s \
    -H "CF-Access-Client-Id: $ID" -H "CF-Access-Client-Secret: $SECRET" \
    https://two-bot-next-staging.<subdomain>.workers.dev/ops/summary
  ```

- **Production** keeps its `workers.dev` route (`workers_dev = true`) until
  a custom domain and Access app exist: production has no custom domain or
  route today, so `workers.dev` is its only origin. Flipping it to `false`
  moves production to the Access-fronted custom domain in a separate change.
  Pinned by `wrangler/scripts/check-env-bindings.py`.

## Read-only guarantee

- The page contains no `POST`/`PUT`/`DELETE`/`PATCH` and stores credentials
  only in memory (never `localStorage`); disconnect clears them.
- The summary rejects non-`GET` with 404 and proxies at most 64 KiB of the
  container's `/ops/summary` roll-up JSON with `cache-control: no-store`,
  mapping `ready: false` to 503 so the page renders NOT READY.
- `/ops/*` paths are reserved in `wrangler/src/redirect.ts` and never become
  invite campaigns.

## Rollback

1. Delete the Worker secrets one key at a time, each stamped with the serving
   SHA (same precheck and stamp as above):
   `wrangler versions secret delete --env <name> OPS_DASHBOARD_CLIENT_ID --message <sha>`,
   then `OPS_DASHBOARD_CLIENT_SECRET`, then `METRICS_SCRAPE_TOKEN`,
   then `wrangler versions deploy --env <name>` → the summary 404s (the static
   page stays reachable). A plain `wrangler secret delete` publishes an unstamped
   version that can never form a rollback receipt. Deleting only the dashboard pair
   leaves scrape-bearer access open.
2. If the routes themselves must go, revert `wrangler/src/ops-dashboard.ts`
   and its wiring in `wrangler/src/index.ts`, then redeploy.
3. `workers.dev` flips back by reverting the three `workers_dev` lines in
   `wrangler/wrangler.toml`.
