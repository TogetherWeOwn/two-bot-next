# Admin API: closed gateway

This is a transport prove-out, **not an executable admin API**. All scoped
requests stop before reads, Discord effects, idempotency claims and terminal
execution rows. Gateway audit/confirm storage and verb registration are later
slices. No control tool is registered in MCP; its existing five read tools and
`MCP_TOKEN` gate are unchanged. No production enablement is authorized here.

## Request and authority contract

- `POST /admin/v1/<action>` takes a JSON object with a string `guild_id` and
  optional `params`. `GET /admin/v1/<action>/<guild_id>` has no query parameters.
  The guild must be nonzero and fit Postgres BIGINT; it is never inferred from
  a default guild or caller scope header. Intake and upstream response bodies
  are bounded to 64 KiB; each request has a six-second intake/forward deadline.
- The request's `Authorization: Bearer adm_<16-lowercase-hex>.<secret>` is the
  only bearer input. The secret suffix is at least 32 ASCII base64url characters;
  the whole token is at most 4096 characters. Operators generate at least 32
  **random bytes** before encoding. Length validation is not an entropy proof.
- The Worker constructs a new payload with `action`, `guild_id`, `params` and
  the transient `admin_bearer` from Authorization. Caller-provided token ids,
  holders, scopes, signing/deployment headers and top-level bearer overrides
  cannot replace that binding. The raw bearer is covered by the body hash,
  never placed in a receipt, error, counter label or log.
- Scope comes from the code-owned verb map at both layers. An unmapped action
  reaches only the container's allowlist refusal; it is not a token-validity
  oracle or proof of registry authorization. Flag/allowlist refusal precedes
  registry lookup. For a mapped, enabled action the container looks up the
  bearer prefix, constant-time compares the suffix hash, and checks live
  registry authority. A service MAC alone is never bearer proof.
- The registry must carry a named holder and `human`/`ai_agent` kind, live
  expiry with a maximum 90-day lifetime, guild allowlist and explicit grants.
  AI tokens cannot hold admin. Higher scopes imply reads only; write grants
  require exact membership. Lookup is uncached: revocation is rechecked on
  every lookup, with no cached-positive window. Missing schema/authority or a
  failed/timed-out lookup is unavailable, never a fallback grant. Missing or
  forged bearer proof is unauthorized. Valid authority still receives the
  closed-surface refusal, not an execution receipt.

## Signing and configuration

The optional Worker secrets `ADMIN_GATEWAY_SERVICE_KEY_ID` and
`ADMIN_GATEWAY_SERVICE_KEY` enable only the gateway **route** when both are
valid. The id uses the reserved suffix `-admin-service`, preceded by 1–114
ASCII letters/digits/underscore/hyphen (128 characters maximum in total).
The secret is at least 32 UTF-8 bytes, has no surrounding whitespace or comma,
and must match a unique dedicated entry in the container's existing
`TWO_INTERNAL_KEYS` ring. Do not reuse a website service key. No secret or flag
is added to committed Worker environment configuration by this slice.

The private receiver also requires its existing `TWO_INTERNAL_ACTIONS=1`
configuration. Every ring id ending `-admin-service` is permanently restricted
to the scoped scheme in receiver code, even without Worker bindings. There is
no ambient key-id flag that could disappear and silently turn that identity
into a legacy website key. This namespace is reserved; existing website keys
must not use it. Removing public gateway bindings is still not key revocation:
revoke the dedicated ring entry as well before retiring that identity.

Both hops preserve the following eight LF-joined lines with no trailing LF:

```text
two-internal-action/scope-claim/v1
POST
/internal/actions
{timestamp}
{nonce}
{sha256_hex(exact_forwarded_body)}
{scope}
{guild}
```

`x-two-scope` and `x-two-guild` select this scheme together. Partial, duplicate,
off-grammar, tampered or v1-signed claim headers fail closed before a nonce
burn. A failed scoped signature never retries v1. The `sha256=` marker is
unchanged. `Idempotency-Key` remains outside the MAC, intentionally; this is
not the complete signing-v2 design. Unscoped legacy service callers retain the
original five-line canonical. A v1 payload containing `admin_bearer` cannot
enter legacy execution.

## MCP control bearer binding (reserved seam)

Future controls use individually operator-issued tokens for named clients,
not the shared phase-1 read credential. AI clients may receive read/moderate;
admin remains human-only. Credentials are never tool arguments, URLs, ambient
module state or log fields. Each binding belongs to its authenticated request;
there is no holder/token override in tool arguments. Step-up means presenting a
separately issued appropriate credential and obtaining a server-verified fresh
binding, not setting a boolean or scope in tool arguments.

`mcp-control-binding.ts` is a pure synchronous local guard: exact scope membership
is checked before explicit step-up. Its immutable grants and non-enumerable
bearer prevent accidental mutation/serialization. It does **not** authenticate a
bearer, query the registry, or implement the step-up ceremony. Future tool wiring
must supply verified grants/step-up, repeat registry/guild/expiry/revocation
checks and write gateway audit before exposing any control tool. The existing
`MCP_TOKEN` never grants control authority.

## Refusal counters and staging proof

Worker pre-auth refusals increment only fixed counters, with no per-attempt
auth log, gateway audit row or container request. Missing/malformed bearers get
the fixed `401` `unauthorized` plus `WWW-Authenticate: Bearer`. Syntactically valid
tokens require the authoritative container lookup to reject a bad **value**;
that receiver retains its inherited bounded, sampled scalar refusal telemetry,
without bearer, body or credential fields. This is not a zero-log guarantee for
end-to-end auth refusals; do not disable the container telemetry. A bounded
per-caller bucket (20 burst / 1 token per second) caps guessing with fixed `429`
`slow down\n` and `Retry-After`. Eight concurrent forwards per isolate are
admitted; an uncooperative forward retains admission until it settles, even
past the caller deadline. There are no retries.

While configured, the existing authenticated `GET /ops/metrics` appends:

- `two_bot_admin_gateway_unauthorized_total`
- `two_bot_admin_gateway_throttled_total`

These are **best-effort isolate-local counters**, reset on isolate replacement;
a scrape is not guaranteed to visit every serving isolate. They are suitable
for a bounded staging refusal prove-out, not a lossless fleet security audit.
No caller, token, guild, body, path, or secret is a label. For staging, the
proposed alert threshold is **60 unauthorized responses in five minutes**
(`increase(two_bot_admin_gateway_unauthorized_total[5m]) >= 60` over a stable
observed isolate), paging the existing bot on-call destination through the
existing ops alert webhook. Multi-isolate collection and that alert rule must
be verified/configured by the authorized operator; this PR does not claim an
active alert or a fleet-complete counter. Do not add per-attempt logging to
compensate for a missing collector.

Staging publication is main-only. After exact-head CI/review/CISO gates and
merge, the authorized executor must verify the deployed artifact identity,
health and rollback target before enabling only the closed gateway in staging.
Prove malformed bearer 401 without container contact, a forged registered-token
secret 401 on a mapped/enabled probe, and a live bearer 403 with no effects or
execution rows. Check counter deltas, no Worker pre-auth per-attempt logs and no
credentials in logs. Container refusals may emit the inherited bounded, sampled
scalar telemetry; verify its limits rather than requiring all such logs absent.
No real moderation operation, token issuance, production migration or production
flag change is part of this proof. Deployment and alert activation are not
proven by local fixture tests.

ACT: add a default-dark, closed signed transport. WHY: prove bearer binding and
claim integrity before audit and effects. UNDO: remove staging route bindings
and revoke the dedicated ring entry; use the existing artifact rollback process
if reverting code. Never delete registry/audit data to roll back this slice.
