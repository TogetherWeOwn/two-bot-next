# Internal-action signing v2 (threat-model F3)

Status: **designed, not implemented** (TOG-12752). This document specifies a
versioned request MAC for `POST /internal/actions`. It binds the audience
(environment), the caller principal, the idempotency key and an expiry, in
addition to what v1 already covers. Nothing here changes code, configuration or
credentials. The implementation slices are listed at the end and in
[threat-model F3](threat-model.md).

It closes three v1 gaps recorded in the threat model:

- `Idempotency-Key` is outside the MAC. Anyone who can modify a valid request's
  headers before its first receipt can change the intent identity (Tampering
  row, F3).
- The MAC has no audience, so if one secret is ever installed in two
  environments, a capture from one is valid in the other (Spoofing row, F2/F3).
- The MAC has no caller. Caller identity comes only from the receiver's
  key-ID mapping, so the signature does not bind the principal it acts for.

## Scope and non-goals

In scope:
- The v2 canonical string and signature.
- Header grammars and verification order.
- The freshness and replay window, and idempotency dedupe.
- Key and scheme configuration, and key rotation.
- Failure and logging behaviour.
- The migration from v1.

Not in scope:
- Response authentication.
- Asymmetric signatures. These would be a future v3 with its own spec.
- Per-user or per-guild authorization. The MAC authenticates the website as a
  caller, not the member it acts for; F2 owns trusted user, guild and
  permission facts.
- A separate signed action field. The action lives in the body, and the body
  digest already covers it.
- The receiver route itself (F1, [TOG-10603](/TOG/issues/TOG-10603), PR #114).

## v1 today (reference, unchanged by this document)

- Code: `crates/core/src/internal_actions.rs`.
  - Canonical string: `canonical_string` at `:116`, `POST\n/internal/actions\n{timestamp}\n{nonce}\n{sha256_hex(raw_body)}`.
  - Signature: `sign` at `:125`, HMAC-SHA256 under the raw secret, sent as `X-TWO-Signature: sha256=<hex>`.
- Unsigned headers: `X-TWO-Key-Id` and `Idempotency-Key`.
- Freshness: `X-TWO-Timestamp` (1–15 digits) within ±`SKEW_SECONDS` = 120 s (`:59`).
- Nonce:
  - `X-TWO-Nonce` is 32 hex characters in either case.
  - Burns are global and durable for `NONCE_TTL_SECONDS` = 241 s (`:70`).
  - The `internal_nonces` table enforces `expires_at >= burned_at + 241 s` (`crates/cutover/migrations/0350_internal_actions.sql:6`).
- Order (`authorize`, `:1564`):
  1. Headers present.
  2. Nonce shape.
  3. MAC; an unknown key ID verifies against a random decoy, so it is indistinguishable from a bad signature.
  4. `ClockGuard` (F8).
  5. Skew.
  6. TTL coverage check.
  7. Nonce burn.
  8. Per-key bucket.
  9. Body parse, which refuses repeated JSON keys.
  10. Allowlist.
  11. `guild.add_member` bucket.
- Dedupe: `RequestIdentity` (`crates/core/src/internal_action_store.rs:57`) keys a slot on `(sha256(caller), sha256(idempotency key))` and requires the same action and payload digest.

The frozen v1 vectors stay valid until an explicitly approved version
transition (see Migration, step G). v2 is a new scheme next to v1, not an edit
of it.

## v2 wire format

### Headers

The route extracts every header below **exactly once**. A missing, repeated or
comma-joined value refuses before any MAC work (F1 single-header extraction).

| Header | v2 meaning |
| --- | --- |
| `X-TWO-Signature` | `v2=` followed by exactly 64 lowercase hex characters. Any other prefix is not v2. |
| `X-TWO-Key-Id` | Signing key ID, signed in v2. |
| `X-TWO-Audience` | Intended receiver and environment, e.g. `two-bot-next:staging`. **New.** |
| `X-TWO-Caller` | Caller principal, e.g. `two-web-next`. **New.** |
| `X-TWO-Timestamp` | `issued_at`, Unix seconds. |
| `X-TWO-Expires` | `expires_at`, Unix seconds. **New.** |
| `X-TWO-Nonce` | 128-bit random value, lowercase hex. |
| `Idempotency-Key` | Optional on the wire; signed in v2. Which actions require it is unchanged. |

The scheme is chosen by the signature prefix: `sha256=` is v1 and `v2=` is v2.
Any other prefix refuses as `unsupported_signature_version`. Whether that scheme
is *accepted* is decided only by server configuration for the key (see
[Configuration](#keys-callers-audience-and-scheme-configuration)), so a request
cannot select a weaker scheme than its key allows. A v1 request that also
carries the v2-only headers is verified purely as v1, and those unsigned headers
are ignored. They must never feed audience, caller, expiry or dedupe decisions.

### Field grammars

Each field is validated **before** the MAC is computed. No grammar admits LF,
so the line-joined canonical string has exactly one parse.

| Field | Grammar | Notes |
| --- | --- | --- |
| audience | `^[a-z0-9][a-z0-9._:-]{0,127}$` | Comes only from the header. Never derived from `Host`, the URL or a proxy header. |
| key ID | `^[A-Za-z0-9._-]{1,64}$` | Same shape as the F4 `KeyLabel` ([rejection telemetry](rejection-telemetry.md)). |
| caller | `^[A-Za-z0-9._-]{1,128}$` | Same as PR #114's `TWO_INTERNAL_CALLERS` names. Case-sensitive. |
| issued_at, expires_at | `^[1-9][0-9]{0,14}$` | Canonical decimal, no leading zero or sign, so one value has one byte form. |
| nonce | `^[0-9a-f]{32}$` | Lowercase only. v1 accepts either case. |
| idempotency key | v1 grammar (`valid_idempotency_key`, `internal_actions.rs:1080`: 8–200 of `[A-Za-z0-9._:-]`) | A header that is present but empty refuses. Absent is a separate, signed state. |
| body digest | lowercase hex SHA-256 of the exact raw body bytes | Computed by the receiver after F1's bounded collection, never sent. |

### Canonical string

The canonical string is eleven ASCII lines joined by a single LF (`0x0A`), with
no trailing LF:

```text
two-internal-action/v2
POST
/internal/actions
<audience>
<key id>
<caller>
<issued_at>
<expires_at>
<nonce>
<idempotency key, or the empty string when the header is absent>
<lowercase hex SHA-256 of the raw body>
```

- Line 1 is the domain and version tag.
- Lines 2 and 3 are the constant route. The receiver writes them as literals
  and never copies them from the received request line, so proxy path
  normalisation cannot change what is verified.
- An absent idempotency key gives an empty line 10. A present key is at least
  8 characters, so "absent" and "present" can never sign the same bytes.

### Signature

`X-TWO-Signature: v2=` + lowercase hex of `HMAC-SHA256(secret, canonical_v2)`.

- `secret` is the same UTF-8 secret bytes v1 uses for that key ID. No new
  credential is needed to adopt v2.
- Domain separation comes from the message:
  - Every v1 canonical string begins `POST\n`.
  - Every v2 canonical string begins `two-internal-action/v2\n`.
  - No byte string is valid under both, so a v1 tag can never verify as a v2
    tag (or the reverse) under one secret.
  - A future scheme that changes the primitive must use a fresh tag. It should
    also use a fresh secret or a derived key.
- The receiver strictly decodes the 64 hex characters and compares the 32-byte
  MAC in constant time.
- Unknown key IDs verify against the ring's random decoy with the same work, as
  in v1 (`KeyRing`, `internal_actions.rs:236`).

## Verification order

The order is load-bearing: no step that costs durable state or reveals policy
runs before authentication.

1. **Shape.** Every required header is present exactly once, and every field
   matches its grammar. The signature prefix is known. Refusal: 401
   `unauthorized`.
2. **MAC.** Compute the v2 canonical string from the received header values and
   the raw body digest, then verify under the key ID (decoy for unknown IDs).
   Refusal: 401 `unauthorized`. A bad signature burns nothing and takes no
   bucket token.
3. **Policy, after authentication.** These values are now authentic, so a
   mismatch is a sender or configuration error, never a forgery. Refusal: 401
   `unauthorized`, with the same wire message as a bad signature.
   - The key's configured scheme set contains `v2`.
   - The audience equals the receiver's configured audience, byte for byte.
   - The caller equals the principal mapped to that key ID.
4. **Clock.** `ClockGuard` evaluates the clock reading against its high-water
   mark (F8, unchanged). Whole seconds are `guarded_ms / 1000`, as in v1.
5. **Freshness.** See below. Refusal: 401 `stale_request`, with nothing burned.
6. **Nonce burn.** Global and durable, as in v1. Refusal: 409 `replayed`.
7. **Bucket.** Keyed by the **principal**, not the key ID (see
   [Key rotation](#key-rotation)). Refusal: 429 `rate_limited`.
8. **Body and action checks**, unchanged from v1: repeated-key refusal,
   allowlist, then the `guild.add_member` bucket keyed by principal.
9. **Dedupe identity.** `RequestIdentity::new(principal, signed idempotency key,
   action, raw body)`.

## Freshness and the replay window

Constants, fixed by this spec rather than configured:

- Skew `S` = 60 s.
- Maximum signed lifetime `L_MAX` = 120 s.
- Website default lifetime: `expires_at = issued_at + 60`.

With `now` as the guarded whole-second clock, a request is fresh only when all
three hold:

```text
1 <= expires_at - issued_at <= L_MAX        else bad_lifetime
issued_at - S <= now                         else not_yet_valid
now <= expires_at + S                        else expired
```

All bounds are inclusive. For a signed lifetime `l`, the request is acceptable
for at most `l + 2S + 1 <= L_MAX + 2S + 1 = 241` whole seconds. That is never
wider than v1's 241-second window (`2 * 120 + 1`), and the sender can narrow it
by signing a shorter lifetime.

**Nonce coverage.** The first acceptance happens at `t0 >= issued_at - S`. Its
burn is retained until at least `t0 + 241 >= issued_at + L_MAX + S + 1`, which
is after the last instant the request is fresh (`expires_at + S <= issued_at +
L_MAX + S`). Every replay attempt inside the signed window therefore meets a
live burn. The margin is the same one second v1 has.

The existing 241-second `NONCE_TTL_SECONDS` and the `internal_nonces` `CHECK`
already satisfy this, so **v2 needs no schema change**. The implementation must
keep v1's runtime coverage check (`nonce_ttl_too_short`, 500 `internal`) in the
form `NONCE_TTL >= L_MAX + 2S + 1`, and add a unit test that fails if either
constant moves without the other.

Nonce burns stay **global across schemes and keys**. A v1 and a v2 request that
share a nonce collide, which is harmless because senders draw 128 random bits
per request. The `ClockGuard` closes expiry-then-rollback reopen exactly as it
does for v1 (F8).

## Idempotency identity and dedupe storage

- **Signed key.** The idempotency key that reaches `RequestIdentity` is the
  signed line-10 value. A header-modifying party can no longer fork one intent
  into two or merge two intents into one.
- **Slot.** Unchanged: `(sha256(principal), sha256(idempotency key))`, which
  must match the action and the payload digest. `principal` is the mapped
  caller (#114 `caller_for`), never the raw key ID. The scheme version and key
  ID are deliberately **not** part of the slot, so a retry dedupes across a key
  rotation and across a v1→v2 flip.
- **Retry.** A retry is a new request: new nonce, new `issued_at` and
  `expires_at`, the same idempotency key and a byte-identical body. It reaches
  the same slot and gets the stored outcome as a replay, or `in_progress`. It
  never re-executes.
- **Retention.** Unchanged. Intent slots are not pruned
  ([internal-action store](internal-action-store.md)). Any future pruning must
  outlast the website's longest retry horizon and needs its own threat-model
  update, because removing a slot can reopen duplication.
- **Both schemes.** The v1 path must also resolve the principal through the
  key-ID mapping before building `RequestIdentity`. Without that, a v1 slot and
  a v2 slot for the same intent would not meet.

## Keys, callers, audience and scheme configuration

All of these are `TWO_INTERNAL_*` names, so they are env-only by prefix
(`crates/core/src/settings.rs:51`). Each new name must also be listed in the
settings catalog and classified for the TWO_* environment drift check in its
slice. Only `TWO_INTERNAL_KEYS` is secret.

| Variable | Grammar | Rule |
| --- | --- | --- |
| `TWO_INTERNAL_KEYS` | `id:secret[,id:secret…]` | Unchanged (`parse_keys`, `internal_actions.rs:191`). Duplicate IDs and reused secrets refuse. Keys enabled for v2 must use the 64-character key-ID shape. |
| `TWO_INTERNAL_CALLERS` | `id:caller[,…]` | As in PR #114. Every key exactly once. Rotation-stable principal. |
| `TWO_INTERNAL_KEY_SCHEMES` | `id:v1`, `id:v2` or `id:v1+v2`, comma-separated | **New.** Every key exactly once. An empty or unknown set refuses. |
| `TWO_INTERNAL_AUDIENCE` | audience grammar | **New.** Required when any key allows `v2`. One value per receiver process. |

- **Startup.** Invalid or incomplete configuration stops startup with a reason
  that names the variable and never echoes a value, as PR #114's
  `InternalActionConfig` does. It never falls back to v1.
- **Unset scheme variable.** Until the activation slice, an unset
  `TWO_INTERNAL_KEY_SCHEMES` means `v1` for every key, which is exactly today's
  behaviour. The activation slice makes the variable required.
- **Audience is per environment.** Staging and production audiences must
  differ, e.g. `two-bot-next:staging` and `two-bot-next:production`. Config
  cannot check this across environments, so it is a non-secret F6 deployment
  receipt.
- **Website (two-web-next).** Each bot endpoint gets:
  - `BOT_SIGNING_VERSION`: `v1` or `v2`. Defaults to `v1` until the website
    slice flips it.
  - `BOT_AUDIENCE` and `BOT_CALLER`: required when the version is `v2`, with
    the same grammars, and refusing at startup when invalid.
  - The legacy bot only ever receives v1.

## Key rotation

Executing any step below is a credential action, and therefore **owner-reserved**:
- minting a secret,
- installing a secret,
- destroying a secret.

Route it as an `Operator:` card through the CEO. This section is a procedure,
not an authorization. No secret, derived key or MAC value belongs in a card,
log or this document.

### Two keys overlapping (same scheme)

1. **Add.** Install `K_new` with a fresh secret on the receiver. Map it to the
   **same** principal in `TWO_INTERNAL_CALLERS`, give it the target scheme set,
   and deploy. `K_old` stays.
2. **Switch.** Point the website signer at `K_new` and deploy.
3. **Drain.** A v2 request signed with `K_old` stays acceptable until at most
   `L_MAX + S` = 180 s after its `issued_at` (v1: `SKEW_SECONDS` = 120 s after
   its timestamp). Remove `K_old` only after the per-key accept counter (see
   logging) has shown zero `K_old` accepts for **15 minutes** after the website
   rollout completed. The wait covers slow isolate rollover; the signed window
   is only the floor.
4. **Remove.** Delete `K_old` from the receiver mapping, then have the owner
   destroy the secret.

Because the bucket and the dedupe slot key on the principal:
- Quota does not double while two keys are live.
- A request first sent under `K_old` and retried under `K_new` replays its
  stored outcome instead of executing twice.

**Compromise.** Remove the compromised key at once, with no drain. In-flight
requests under it fail and their retries under the new key dedupe.

### Two schemes overlapping (v1 → v2)

The recommended path for two-bot-next is to **rotate into v2**:
- the new key gets `v2` only;
- the old key keeps `v1`;
- the drain above retires v1 and the old key together.

Per-key accept counts then show migration progress directly, and no single key
accepts both schemes. If a key must accept `v1+v2`, bound the overlap by the
same zero-accept rule, measured per scheme, and a dated deadline on the
deployment card.

## Failure behaviour and logging

**Wire.** Wire responses are unchanged, so callers learn nothing new:

| Case | Response |
| --- | --- |
| Any shape, MAC or post-MAC policy refusal | 401 `unauthorized`, message `Signature verification failed` (`AUTH_FAILURE_MESSAGE`, `internal_actions.rs:97`) |
| Freshness refusal | 401 `stale_request` |
| Nonce reuse | 409 `replayed` |

An unknown key ID and a bad signature stay indistinguishable on the wire.

**`log_reason`.** Internal values, used by tests and never logged under F4.
Each value is closed and static:
- Shape: `missing_auth_headers`, `duplicate_auth_header`,
  `bad_auth_header_shape`.
- Signature and policy: `unsupported_signature_version`, `bad_signature`,
  `scheme_not_allowed`, `audience_mismatch`, `caller_mismatch`.
- Freshness and replay: `bad_lifetime`, `not_yet_valid`, `expired`,
  `clock_rollback`, `replayed_nonce`.

None of them embeds a received value.

**F4 telemetry** ([rejection telemetry](rejection-telemetry.md)):
- The `RejectionClass` mapping is unchanged.
- Records gain one closed `scheme` label: `v1`, `v2` or `unsupported`.
- The existing `key` label rules still apply: a key ID is printed only when the
  ring holds it.
- A bounded accept counter labelled by `scheme` and configured key label drives
  the rotation drain and the v1 exit criterion.

**Never logged, in any record, error, `Debug` or panic message:**
- the secret;
- the received or expected signature;
- the canonical string;
- the nonce;
- the idempotency key;
- the body or its digest;
- any caller-supplied audience, caller or key-ID value that does not match
  configuration.

`Debug` for the v2 header set must be hand-written and print only presence and
lengths. The implementation adds a marker fixture, like F4's, proving none of
these reach a log.

## Migration from v1

| Step | Change | Rollback |
| --- | --- | --- |
| A | This spec (docs only). | n/a |
| B | Core v2 sign and verify library, grammars, scheme policy, new config parsing, frozen v2 vectors. Nothing is enabled. | Revert PR. |
| C | Website v2 signer behind `BOT_SIGNING_VERSION`, default `v1`. Cross-checked against the same vectors. | Config `v1`. |
| D | Receiver wiring (TOG-10603 / PR #114): v2 headers, principal-keyed bucket and identity, `TWO_INTERNAL_KEY_SCHEMES` required. | Config, or revert before activation. |
| E | Staging E2E on `next.togetherweown.com` → `two-bot-next-staging` with v2. Tamper, cross-audience and rotation tests pass and are recorded. | Config. |
| F | Production activation under the existing F1/F2/F6/F8 gates and deployment approval. | Config. |
| G | Retire v1 acceptance code in two-bot-next after the legacy bot retires and seven days show zero v1 accepts. This is an explicitly approved version-transition PR. | Revert PR. |

**Recommendation.** The two-bot-next receiver has never been deployed, so it
should activate with `v2` only and never accept v1 traffic. In that case step G
only removes dead code.

**Fallback.** If activation cannot wait for steps B–C, the receiver may start on
`v1` under the current residual risk (unsigned idempotency key, trusted
transport required) and flip to v2 by rotation. That choice belongs to the CEO
and CISO on the deployment card, not to this spec.

**Ledgers.** The legacy bot's ledgers and the next bot's ledgers are separate,
so a v1 retry sent to the legacy bot never dedupes against next. That is a
cutover sequencing concern (`docs/cutover.md`), unchanged by v2.

## Implementation slices (not part of this PR)

1. **Core v2 sign/verify.**
   - Canonical builder, strict grammars and the signature header.
   - Scheme policy; parsing of `TWO_INTERNAL_KEY_SCHEMES` and
     `TWO_INTERNAL_AUDIENCE`; settings-catalog and drift classification.
   - The coverage invariant.
   - Frozen `crates/core/tests/fixtures/internal-action-signing-v2.json`,
     generated from test-only secrets and loaded with `include_str!`. No key
     literal in Rust source.
2. **Principal keying.** Principal-keyed buckets, and `RequestIdentity` built
   from the mapped principal for both schemes. No schema change.
3. **F4 telemetry.** The `scheme` label, the accept counter and the
   leakage-marker fixtures.
4. **Receiver wiring** on TOG-10603 / PR #114: single extraction of the new
   headers and the full verification order above.
5. **two-web-next signer.** v2 signer and config, verified with WebCrypto
   against a byte-identical copy of the vector file. The file's SHA-256 is
   pinned in both repos so drift fails CI.
6. **Rollout.** Staging E2E, then the activation configuration and the
   deployment receipts (audience per environment, scheme sets).

## Acceptance tests (activation evidence for F3)

- Changing any one of these refuses as `bad_signature` against the frozen
  vectors:
  - audience, key ID, caller, `issued_at`, `expires_at`, nonce or
    idempotency key;
  - one body byte;
  - adding or removing the idempotency key.
- An authentic request for another audience, or with a caller that differs from
  the key's mapped principal, refuses with 401 and burns no nonce.
- v1 vectors pass under a `v1` key and refuse under a `v2`-only key. v2 vectors
  refuse under a `v1`-only key. A `sha256=` tag over a v2 canonical string, or
  a `v2=` tag over a v1 string, refuses.
- Duplicate, comma-joined, empty or uppercase-hex headers, and leading-zero
  timestamps, refuse before MAC work.
- Lifetime 0, `L_MAX`, `L_MAX + 1`, and the `issued_at - S` and
  `expires_at + S` edges behave exactly as the inclusive bounds above.
- Replays inside the signed window refuse on the memory and durable paths,
  including after a sweep plus clock rollback (F8 fixtures extended to v2).
- After a rotation, a retry under the new key with the same idempotency key
  returns the stored outcome without a second execution. Both keys share one
  bucket.
- Log-marker fixtures: no secret, signature, nonce, idempotency key, body or
  unmatched caller-supplied value appears in any record or `Debug` output.
