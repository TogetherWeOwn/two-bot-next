# Redirect configuration safety

The Worker validates redirect configuration before serving campaign redirects,
the bare-domain fallback or the exact `/healthz` redirect probe. This ports the
Worker-relevant contracts identified in legacy changes `f9fbf5c`, `6433578` and
`b62ff28`. The Node redirect listener-port setting is intentionally not ported:
Workers have no listener port. `BOT_PORT` remains the separate Rust Container
proxy configuration, unchanged by this slice.

## Configuration

- `REDIRECT_FALLBACK_CODE`: unset, null or the empty string disables fallback.
  A configured value must be a string of 1–64 ASCII letters, digits or hyphens.
  Non-string values, URLs, spaces and trailing line terminators are invalid;
  values are never coerced or silently trimmed.
- `REDIRECT_MAPPINGS_JSON`: unset or the empty string means no snapshot campaigns.
  Otherwise it must be a string containing a JSON array of campaign rows. Null,
  booleans, numbers, arrays and objects are invalid binding types. Each row
  requires a unique canonical lowercase `slug` (2–40 ASCII alphanumeric/internal-hyphen characters) and a
  valid `invite_code`. `label`, when supplied, must be a string; `disabled_at`
  must be absent, null or a string. Retired campaigns still redirect. Extra
  database export fields are ignored, as before.
- `healthz` and `metrics` are reserved campaign inputs. Case, slash and encoded
  aliases cannot become campaign redirects. Only exact `/healthz` serves the
  redirect probe, and only GET/HEAD yield 200; aliases/subpaths return 404 with
  no lookup, throttle, diagnostic or click, even when a reserved prefix has a
  malformed percent-encoded suffix or configuration is invalid. Near-miss slugs
  such as `healthz-campaign` remain valid.

An invalid fallback or snapshot rejects the redirect configuration as a whole:
503, `Retry-After: 30`, no Location, lookup or click. It does not silently become
an empty store or a disabled fallback. `/health` and `/readyz` still proxy the
Container independently; those are not redirect configuration probes. Internal
reserved aliases remain 404 before configuration parsing.

The snapshot remains the current Worker mapping source. This change does not
wire the deferred Hyperdrive connector, alter cache/proxy policy, add campaign
management UI or contact deployed services.

## Diagnostics and privacy

Configuration errors log only `invite_redirect_invalid_config` plus the fixed
`errorClass` `invalid_fallback` or `invalid_snapshot`; neither config values nor
JSON parse fragments are logged. Lookup errors log
`invite_redirect_lookup_failed` with the validated, bounded slug and exactly one
of two classes: `db_unavailable` (the store's own connect/query deadline fired)
or `internal` (everything else, including non-`Error` throws and errors whose
`name` is attacker-influenced; see `redirectErrorClass` in
`wrangler/src/redirect.ts`). Arbitrary thrown strings, error names/messages/stacks,
connection strings, visitor IP, query string, user agent and referrer are not
included. Invalid stored codes and click-write failures also use class-only
diagnostics.

Divergence from legacy `b62ff28`: the Node service also logged
`invite_redirect_decode_failed` with the raw request path when a percent-escape
did not decode. The Worker does not: a malformed escape, like any other invalid
slug, answers 404 with no lookup and no log line. Only a validated slug is ever
logged, and the raw request path never is.

Rate limiting still uses only Cloudflare's `CF-Connecting-IP` edge header (or
`unknown`); generic proxy headers are not trusted. Successful redirects remain
302/no-store/no-referrer; HEAD never counts; unknown slugs remain 404; lookup
outages use the valid fallback or 503.

## Offline verification

From `wrangler/` with the pinned dev dependencies installed:

```sh
npm run typecheck
node --import ./test/cloudflare-loader.mjs --test \
  test/redirect.test.ts test/redirect-config.test.ts test/container.test.ts
```

All bindings and errors are synthetic. No live endpoint, Discord or database
probe is part of this verification.

## Staging smoke

`scripts/staging_redirect_smoke.py` exercises the same contracts against the
staging Worker origin (read-only E2E): exact `/healthz` 200, reserved alias
404 with no lookup, and unknown slug matching the fallback-or-404 contract;
any 5xx must carry the bounded fail-closed shape above. It makes three GETs,
follows no redirects, asserts no known campaign slug (no click side effects),
sends no credentials and never touches production. The origin comes from
`$STAGING_WORKER_URL` (the same variable the staging gate reads); anything
else is refused before a request is sent.

Run it after each staging deploy and keep the passing receipt with the B4
soak evidence on [TOG-9699](/TOG/issues/TOG-9699): the receipt holds only
check names, shape tokens and the origin, never response bodies or secrets.

```sh
STAGING_WORKER_URL=https://two-bot-next-staging.<sub>.workers.dev \
  python3 scripts/staging_redirect_smoke.py --evidence staging-redirect-smoke-evidence.json
```

Offline fixtures cover the same assertion logic with no network access:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test_staging_redirect_smoke.py -v
```

CI runs them in the worker job beside the rollout provenance suite.
