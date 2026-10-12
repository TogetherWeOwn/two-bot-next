# Read-only ops dashboard (TOG-22861)

A static Worker-served page (`GET /ops/dashboard`) polling a JSON summary
(`GET /ops/summary`, a bounded proxy of the container's `/readyz`). Strictly
read-only: the page issues only `GET` fetches and the summary only reports
status. All mutations stay on their existing API routes.

## Access (Operator setup)

Human auth is Cloudflare Access service tokens, enforced twice:

1. **Edge (authoritative):** the Access app fronting these paths carries a
   Service Auth rule permitting the dashboard token, with organization
   **strict service-token mode** on: solely Service Auth rules count, no
   authorization cookie is issued, and every call repeats both headers.
   https://developers.cloudflare.com/cloudflare-one/identity/service-tokens/
2. **Worker (defense in depth):** the Worker re-validates both
   `CF-Access-Client-Id` and `CF-Access-Client-Secret` against its own
   secrets on every request, so a direct `workers.dev` hit cannot bypass the
   gate. Unconfigured/short secrets → 404; missing/wrong → 401 (never a
   login redirect). Attempts are per-caller throttled (10-burst, 1/sec).

Install the pair as Worker secrets (never `wrangler.toml` vars):

```sh
wrangler secret put --env staging OPS_DASHBOARD_CLIENT_ID
wrangler secret put --env staging OPS_DASHBOARD_CLIENT_SECRET  # >= 32 chars
```

Same for `--env production`. Removing either secret closes both routes (404).

## Break-glass

- **Staging** keeps its `workers.dev` route (`workers_dev = true`) as the
  Access-independent origin. With the headers set, this bypasses a broken
  custom-domain route but never the service-token check:

  ```sh
  curl -s \
    -H "CF-Access-Client-Id: $ID" -H "CF-Access-Client-Secret: $SECRET" \
    https://two-bot-next-staging.<subdomain>.workers.dev/ops/summary
  ```

- **Production** never serves `workers.dev` (`workers_dev = false`): the
  dashboard and summary are only reachable on the Access-fronted custom
  domain. Pinned by `wrangler/scripts/check-env-bindings.py`.

## Read-only guarantee

- The page contains no `POST`/`PUT`/`DELETE`/`PATCH` and stores credentials
  only in memory (never `localStorage`); disconnect clears them.
- The summary rejects non-`GET` with 404 and proxies at most 64 KiB of the
  container's readiness JSON with `cache-control: no-store`.
- `/ops/*` paths are reserved in `wrangler/src/redirect.ts` and never become
  invite campaigns.

## Rollback

1. Delete the Worker secrets (`wrangler secret delete --env <name>
   OPS_DASHBOARD_CLIENT_ID OPS_DASHBOARD_CLIENT_SECRET`) → both routes 404.
2. If the routes themselves must go, revert `wrangler/src/ops-dashboard.ts`
   and its wiring in `wrangler/src/index.ts`, then redeploy.
3. `workers.dev` flips back by reverting the three `workers_dev` lines in
   `wrangler/wrangler.toml`.
