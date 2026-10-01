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
JSON parse fragments are logged. Lookup errors log the validated, bounded slug
and one of `TypeError`, `RangeError`, `SyntaxError`, `Error`, `Unknown`.
Arbitrary thrown strings, error names/messages/stacks, connection strings,
visitor IP, query string, user agent and referrer are not included. Invalid
stored codes and click-write failures also use class-only diagnostics.

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
