# TWO Bot Next: internal actions and Worker threat model

Reviewed 2026-10-01 against baseline `5ddab2dfec52853a0c87cc6e555baf6575eae77e`
(delta `44338b28..5ddab2d` assessed; prior review 2026-09-30 against
`44338b28a7feac0093cbd72dc9cecc45ab33a15d`).
This is a source-based STRIDE assessment, **not deployment approval**. No live
credentials, Discord mutations, database probes or key rotation were performed.
The private durable HTTP receiver (PR #114,
`feat/private-internal-actions-http` at `5210a0b`) remains open and unmerged;
the async durable-burn seam and receiver/executor integration remain unwired
on main.

## Scope and current exposure

The historical contract has **18 actions**. The current core allowlist has
**19**: those 18 plus `event.read`
(`crates/core/src/internal_actions.rs:626`; nine moderation verbs at `:676`,
14 idempotency-bound at `:653`). All 19 are assessed below. Note drift:
`docs/parity.md:197` still describes the website→bot row as 18 actions and
omits `event.read` and settings compare-and-set; the core constants above are
authoritative. The legacy `docs/INTERNAL_ACTIONS.md` is not in this checkout;
the ported core, [parity inventory](parity.md) and
[durable-store contract](internal-action-store.md) are the source evidence here.

**Implemented libraries are not deployed receiver controls.** Executor additions
since the prior review are libraries: announcement execution
(`crates/discord/src/internal_actions.rs:115`), member join/role assignment
(`crates/discord/src/internal_exec/member.rs`), guild settings CAS (via #81),
channel-moderation wiring
(`crates/discord/src/internal_channel_moderation.rs:67`) and the ticket runtime
(`crates/bot/src/ticket_runtime.rs`, integrated via #155).

- The Worker routes exact `/health` and `/readyz` to the singleton Container;
  other paths enter the invite redirect handler (`wrangler/src/index.ts:313`).
  Reserved internal paths (`/metrics`, `/metrics/*`, canonicalized aliases)
  return 404 for every method before lookup (`wrangler/src/index.ts:321`,
  `wrangler/src/redirect.ts:177`, `:211`); other non-GET/HEAD paths return 405,
  so current `POST /internal/actions` returns 405 rather than reaching an
  action receiver (`wrangler/src/redirect.ts:216`).
- The Container router serves GET `/health`, GET `/healthz`, and GET
  `/readyz` (`crates/bot/src/server.rs:29-31`) and merges an internal
  `/metrics` scrape on the same listener (`:34`,
  `crates/bot/src/metrics_http.rs:20-21`; the Worker never proxies it).
  Axum `get()` routes also accept HEAD with the body stripped, so HEAD on
  these paths is live on the direct Container listener. Readiness means
  process and gateway readiness, not action-service, settings-store or
  general database readiness; supervised-job status is informational only and
  never changes the readiness code (`crates/bot/src/server.rs:139`).
  `/healthz` reaches the Container only there: the Worker forwards only exact
  `/health` and `/readyz` to the DO (`wrangler/src/index.ts:313`), the DO
  forwards only those two (`:123-128`), and at the Worker level `/healthz` is
  answered statically by the redirect handler (`wrangler/src/redirect.ts:219`),
  never proxied.
- The Container DO independently refuses forwarding paths other than the two
  probes (`wrangler/src/index.ts:123-128`). Neither that forwarding policy nor
  the Worker secret whitelist currently admits internal actions.
- HMAC authorization, private-bind validation, field validators, durable replay
  and execution claims exist as libraries. Signing secrets and the nonce decoy
  are now `Secret`-wrapped (`crates/core/src/internal_actions.rs:150-152`,
  `:213-234`). The async durable-burn seam and receiver/executor integration
  remain unwired (`docs/internal-action-store.md:86-89`). Implementing that
  route is outside this assessment, assigned to
  [TOG-10603](/TOG/issues/TOG-10603); this document does not lift its holds.
  Receiver PR #114 is open, not merged.
- Gateway traffic is received over the bot's outbound Discord connection, not
  an additional inbound HTTP webhook. The runtime commits funnel effects and
  gateway checkpoints; detached dispatch now fans MessageCreate/
  InteractionCreate events (including tickets) to the shared command runtime
  without awaiting REST or store writes (`crates/bot/src/gateway.rs:320`,
  `crates/bot/src/command_runtime.rs:257`, tickets supervisor start/shutdown
  at `gateway.rs:180`, `:192-193`). The gateway loop still does not invoke the
  internal-action executor; the ticket runtime shares `ActionExecutor`
  transport through its own guild-fenced handlers
  (`crates/bot/src/ticket_runtime.rs:211-270`).

## Assets, actors and assumptions

| Asset | Security property / consequence of loss |
| --- | --- |
| Discord guild roles, membership, messages, events and channel overwrites | Integrity and availability; bot permissions can affect the entire configured guild. Deleted messages cannot be restored by idempotency. |
| HMAC signing keys and stable website caller identity | Confidentiality and environment isolation; possession authorizes globally enabled actions, not just one end user's permissions. |
| Discord bot token and member OAuth access token | Confidentiality; bot token is guild-wide capability, member token enables that member's join. Neither may enter logs, audit rows or cached responses. |
| Neon/Postgres data: settings, automation configuration, replay/intent/audit ledgers, gateway checkpoints, ticket transcripts and settings CAS state | Integrity, confidentiality and durability; corrupted claims/checkpoints can duplicate effects or lose evidence. Purged transcripts and CAS tokens change retention and conflict behavior; see F2/F5. |
| Worker/Container capacity, database pool and Discord REST quota | Availability; public probes and authenticated action floods compete with gateway work. |
| Request IDs and scalar audit trail | Attribution and reconciliation without retaining request secrets or untrusted provider errors. |

Adversaries include an unauthenticated Internet client, a Discord member sending
hostile input, a compromised website session, a compromised authorized signer,
a captured signed-request observer, and a misconfigured deployer. Container,
Worker or DB compromise can defeat multiple controls and requires containment,
not a claim that HMAC protects a stolen bot token. CI/deployment identities and
secret custody are trusted prerequisites, not tested by this document.

The website must authenticate the human and authorize each operation *before*
signing. HMAC proves a service held a key; it does not prove `actor_id` came from
an authenticated human or that they may moderate a target. Private addressing is
not authentication, and a Cloudflare public route is not private merely because
its upstream Container socket is private. Production/staging signing secrets,
Discord guilds and database bindings must be disjoint. Staging and production
observability (`wrangler/wrangler.toml:84-86`, `:118-126`: log head sampling 1,
production traces off because fetch spans record the webhook URL) widens log
exposure in both environments without proving anything about isolation. No
deployed isolation, TLS setting, key custody or Cloudflare WAF configuration
was verified here.

## Trust boundaries and data flows

1. **Browser → website:** untrusted user input and session identity. Session/CSRF,
   role and object-level authorization belong to the website; never put the
   service HMAC key in the browser. The website sends one authorized action,
   fresh timestamp/nonce, and a stable idempotency key for one intent.
2. **Website → Worker → Container (future actions flow):** Internet traffic crosses
   the Worker routing/authentication boundary, then a separate DO forwarding
   boundary. Both currently exclude actions (reserved-internal 404s land before
   campaign lookup). Adding a route must be explicit, default-deny and reviewed;
   merely forwarding all paths would expose the bot's remote control. Preserve
   signed body bytes across every hop and require TLS.
3. **Private receiver → core/store → Discord REST (future):** service signature,
   freshness and durable replay refusal precede parsing, authorization and an
   atomic execution claim. Only a committed new claim may cause a side effect;
   the executor assumes policy adjudication happened elsewhere. New since the
   prior review: settings writes carry compare-and-set (`expected_version`,
   `VersionConflict`/409) with opaque tokens, role assignment pins the
   allowlist-resolved role in the same intent/audit transaction
   (`crates/core/src/internal_action_store.rs:92`, `:328`), moderation channel
   reads are guild-fenced (`crates/discord/src/executor.rs:887`), and ticket
   close/reservation runs under row-lock fences with atomic transcript commits
   (`crates/cutover/src/tickets.rs:34`, `:180-199`). None of this wires the
   receiver; it raises the F2 acceptance bar instead.
4. **Discord gateway → Container:** authenticated upstream does not make members'
   payloads trusted. DMs are dropped from message processing; foreign-guild
   event/activity batches are refused at persistence, and effects/checkpoints
   commit together (`crates/bot/src/gateway.rs:326`,
   `crates/cutover/src/gateway_session.rs:63`). Dispatch to the command runtime
   (including tickets) is detached per event and never awaits REST or store
   writes; ticket button admission is bounded (10-second lane, 120-second
   cumulative, supervisor-bounded recovery per #155) and ticket/message-content
   intents apply only when automod or tickets are configured
   (`crates/bot/src/gateway.rs:89-90`). The mock-gateway override accepts
   literal loopback sockets only (`:364`). Do not infer full
   slash-command/moderation deployment from libraries.
5. **Worker/Container → Neon:** the Container connects using `DATABASE_URL`
   (`crates/cutover/src/db.rs:18-43`). Pool default is five connections
   (`:18`) and statement timeout 15 seconds (`:20`); URLs pass an allowlist
   validator and a passfile-silenced parse (`:39-43`,
   `crates/core/src/database_url.rs:23`, `:119`), and connection/migration
   failures are generic (`crates/cutover/src/db.rs:55`, `:79`). Least-privilege
   DB roles, a read-only grant verifier, a DML-only gateway identity and the
   operator-migrates-first contract (via #102) are deployment procedure, not
   source fences. Startup normally applies embedded migrations. The checked-in
   Worker has no Hyperdrive binding, and redirect storage is snapshot-only
   (`wrangler/wrangler.toml:38`, `wrangler/src/redirect-store.ts:10-11`,
   `wrangler/src/index.ts:68`). Staging observability is telemetry, not
   isolation. Neither direct DB TLS enforcement nor a staging/production URL
   fence is established by these sources.

## STRIDE analysis

Severity is the credible blast radius **if the future receiver is enabled**, not
an assertion of a currently exploitable action route. P1 items below are release
gates for that integration. Current-public-surface findings are marked explicitly.

| STRIDE | Concrete threat / boundary | Existing control | Residual risk / required evidence |
| --- | --- | --- | --- |
| Spoofing | Forge a website request or enumerate accepted key IDs | HMAC-SHA256 on exact bytes; unknown key uses a random decoy; unknown ID and wrong signature return the same auth refusal. Secrets and the decoy are now `Secret`-wrapped, shrinking Debug/log exposure (`crates/core/src/internal_actions.rs:150-152`, `:213-234`) | Receiver must reject duplicate/coalesced auth headers and preserve values. No user identity or per-principal action scope is supplied by HMAC. P1 F1/F2. |
| Spoofing | Replay a capture on another environment, under a rotating identity, or after clock rollback | Timestamp/skew and global nonce burn; durable identity contract requires a stable logical caller | Canonical MAC has no host, environment or caller ID; never reuse secrets across environments or aliases. Within one ring `parse_keys` refuses a repeated key ID or two IDs sharing a secret, naming IDs only; cross-environment reuse remains a custody check. Preserve ledgers during rotation. Expiry then rollback can reopen freshness after a nonce is forgotten; TTL alone is not a clock policy. F2/F3/F8. |
| Tampering | Change body/action or exploit different HTTP/JSON parsers | MAC signs raw-body SHA256, not parsed/reencoded JSON; validators and allowlists refuse bad fields | `Idempotency-Key` is not signed; trusted transport is required. A signed body repeating a JSON object key at any depth (compared after unescaping) refuses as `Malformed`/`duplicate_json_key` before dispatch; the nonce stays burned and the key is never echoed. Bound collection before hashing and test proxy/path/method semantics. F1/F3. |
| Tampering | Bypass role/channel maps, foreign-guild fence or protected-target policy | Symbolic role/channel keys, catalog-only settings, moderation adjudication and guild-fenced persistence exist in libraries. Moderation permission/targets now resolve from one `command_permissions` source (`crates/core/src/moderation.rs:103-112`); website moderation channels are guild/type-checked (`crates/discord/src/executor.rs:887`); role assignment pins `resolved_role_id` at claim time (`crates/core/src/internal_action_store.rs:92`) | Receiver must call every relevant validator and obtain trusted live permission/hierarchy facts; `authorize` alone does not validate action fields. Settings CAS conflicts (`VersionConflict`/409 at `crates/core/src/internal_actions.rs:507`) must refresh, never blind-retry. P1 F2. |
| Repudiation | Retry a destructive action after an ambiguous outcome, or lose audit linkage | Store commits scalar intent and audit atomically; only `Claimed` allows execution; stale/unknown claims require reconciliation. Ticket close commits transcript atomically under row-lock fences (`crates/cutover/src/tickets.rs:180-199`); later audits copy the original role pin, never a re-evaluated map | Async store seam is not wired. Request/actor IDs need trusted derivation; no arbitrary provider JSON in terminal records. Transcript purge and CAS-token retention are new deletion/conflict surfaces; cleanup must not reopen duplication. P1 F2, F5. |
| Information disclosure | Leak OAuth/bot/signing/DB credentials via errors, tracing or settings | Redacted key/decision Debug, catalog denies environment-only/unknown settings, typed store errors/results. Since the prior review: `Secret`-wrapped signing keys/decoy, redacted transport/Debug (`crates/discord/src/executor.rs`), `RawResponse` shape-only Debug, `database_url` query allowlist plus passfile-target silencing, and generic DB connection/migration errors | HTTP rejection logger is not implemented. Logging full headers/body/error chains would undo minimization. Staging and production log head sampling 1 (`wrangler/wrangler.toml:84-86`, `:118-120`) raises log stakes in both. Source does not prove runtime TLS/custody. F4/F6. |
| Denial of service | Public probes wake/pin singleton Container or consume pool/crypto/memory | DO path allowlist, six-second readiness probe, redirect bucket, pool/timeouts. Reserved-internal `/metrics*` 404s land before DB lookup (`wrangler/src/index.ts:321`); readiness webhook failures stay generic and never log the secret URL (`wrangler/src/index.ts:260`) | Probes have no application auth/rate gate; redirects' isolate bucket is not a global edge limit and its caller map has no cap or expiry, even for 404s (`wrangler/src/redirect.ts:111-117`, `:229`; reserved paths skip lookup but still take a caller bucket). Action body cap runs after HMAC and nonce burn; authenticated nonce flood precedes key bucket. Gateway fan-out is detached per event, so a hostile event burst spawns bounded runtime work (ticket lane 10 s, cumulative 120 s) rather than stalling heartbeats — bound it anyway. F1/F7. |
| Denial of service | Spend the same clock interval twice in an action bucket | This change keeps a last-seen clock high-water mark | New regression covers both bucket specs; restart/multi-instance buckets are still local. Future receiver needs bounded ingress/concurrency. F7. |
| Elevation of privilege | Compromised signer invokes all enabled verbs or changes its own gates | Environment-only approval flags; settings catalog denies `TWO_INTERNAL_*`, `TWO_MODERATION`, secrets and unknown keys; overwrite requires a second flag. Phase-1 defaults remain exactly `role.assign`, `announcement.post`, `event.upsert` (`crates/core/src/internal_actions.rs:778`); settings CAS rejects stale writes instead of silently reverting (`VersionConflict`) | Keys have no per-caller capabilities. Mapped roles/configured channels and allowed hot/cold settings still carry privilege; validate policy before enabling. P1 F2. |
| Elevation of privilege | Treat public wildcard health bind as an approved actions bind | `assert_private_bind` rejects wildcard/public/hostname addresses with no override (`crates/core/src/internal_actions.rs:1430`) | Current health listener is deliberately `0.0.0.0`; never reuse it for an unguarded receiver or weaken the bind guard to fit the Worker. Internal `/metrics` shares that listener but is never proxied; treat it as non-public by routing, not by the bind. P1 F1. |

## Action-by-action blast radius

Every action requires signature, freshness, replay refusal and the default key
bucket. **I** means the core requires durable-store presence and an idempotency
key; the receiver must actually validate/claim it. No **I** is not permission to
skip durable nonce checks. Flags below describe library approval gates, not
verified deployment configuration (`crates/core/src/internal_actions.rs:626`,
`:778`). All effects are restricted to the configured guild by integration
policy; that fence must not be taken from a caller-supplied guild ID.

| Action | Maximum credible effect / disclosure | Narrowing controls and required integration | I |
| --- | --- | --- | --- |
| `role.assign` | Grant a configured role to an arbitrary named member; a badly mapped privileged role is escalation | Phase-1 default; validated member snowflake, explicit role-key map (currently no inherited self-role seed). `RoleAssignRequest` (`crates/core/src/internal_actions.rs:1242`) plus `resolved_role_id` pinned in the same intent/audit transaction (`crates/core/src/internal_action_store.rs:92`, `:328`); later audits copy the pin. Map only safe self-assignable roles and confirm member/guild. | No |
| `guild.add_member` | Join a member to the configured guild with their OAuth capability | `ALLOW_ADD_MEMBER`; member ID/token presence checks, tighter bucket. `GuildAddMemberRequest` keeps the OAuth token a separate transient argument (`crates/core/src/internal_actions.rs:1272`); member mutations are single-attempt with 1.5 s/2.0 s timeouts, no unfollowed-redirect success, and never format token-bearing bodies into errors (`crates/discord/src/internal_exec/member.rs:20-21`). Validate OAuth subject/consent through Discord, never log or cache token. | No |
| `announcement.post` | Spam, impersonation or mass mentions in mapped channels | Phase-1 default; channel map starts empty; 2,000 UTF-16 units. `AnnouncementExecutor` supports only this verb (`crates/discord/src/internal_actions.rs:150`), sends one 10-second attempt with no status retries, suppresses mentions, returns only scalar IDs, and feeds every 429 into the bot token's shared `CooldownGovernor`, refusing later intents without HTTP while a matching hold is active; a 429 is never resent (`docs/internal-action-executor.md:43-64`). Executor mention suppression must be used on this path. | Yes |
| `event.upsert` | Create/edit guild events and associated public content | Phase-1 default; name ≤100, description ≤1,000 UTF-16 units; valid ordered RFC3339 times; exactly one mapped channel or external location. Verify ownership of any existing event mapping. | Yes |
| `event.cancel` | Cancel a mapped event and disrupt attendance | `ALLOW_EVENT_CANCEL`; verify event/guild mapping and authorized actor rather than accepting any raw event ID. | Yes |
| `automations.import` | Bulk change automation/role behavior; overwrite can destroy configuration | `ALLOW_AUTOMATIONS`, plus `ALLOW_AUTOMATIONS_OVERWRITE` for destructive mode. Schema/count/role validation and transactional import must be wired; body cap alone is insufficient. | Yes |
| `automations.export` | Disclose automation configuration, role/channel mappings and templates | `ALLOW_AUTOMATIONS`; authorize admin read, exclude secrets and minimize output. | No |
| `settings.get` | Read one classified hot/cold setting | `ALLOW_SETTINGS` and settings store; shape and catalog checks deny unknown/env-only keys. Readable settings are not necessarily non-sensitive to all users. | No |
| `settings.set` | Change hot/cold behavior, including safety thresholds, for the guild | Same gates/catalog; serialized value ≤8,192 bytes; validated key type/range plus compare-and-set: omitted `expected_version` saves unconditionally, zero requires absence, otherwise the revision-row lock rejects stale writes with `VersionConflict`/409 (never silently revert). CAS tokens are opaque equality metadata, never extra signed fields. Validate each key's version/audit semantics. Cannot alter env-only action/permission gates. | Yes |
| `moderation.ban` | Remove one member permanently; repeated intents can exclude many members | Both moderation flags; trusted ban permission, protected-target/hierarchy adjudication, mandatory reason. | Yes |
| `moderation.tempban` | Exclude one member for up to one year | Same policy; duration 60–31,536,000 seconds runtime-enforced (`crates/core/src/internal_actions.rs:1072`, wired at `crates/discord/src/internal_channel_moderation.rs:67`); durable expiry/unban job must be safe. | Yes |
| `moderation.kick` | Remove one member; repeated intents can remove much of the guild | Same policy with kick permission; use one-attempt internal moderation path, not a generic retrying kick helper. | Yes |
| `moderation.timeout` | Silence one member for up to 28 days | Same policy with timeout permission; duration 60–2,419,200 seconds runtime-enforced (same wiring). | Yes |
| `moderation.warn` | Create reputational/audit consequences for one member | Same policy, trusted actor/target and mandatory reason; no arbitrary text in scalar store audit. | Yes |
| `moderation.purge` | Irreversibly delete up to 100 messages per intent; repeated intents multiply loss | Same policy with channel authorization; count 1–100, configured guild/channel ownership check. | Yes |
| `moderation.slowmode` | Restrict a channel for up to six hours or remove its slowmode | Same policy with channel authorization; seconds 0–21,600 and guild/channel fence. | Yes |
| `moderation.lockdown` | Deny channel participation; repeated intents can silence the guild | Same policy; snapshot/restore precise overwrites, validate channel ownership, do not grant new permissions as a side effect. | Yes |
| `moderation.unlock` | Restore participation; bad overwrite restoration can widen access | Same policy; only trusted saved overwrite state, not arbitrary caller permission bits; serialize with lockdown. | Yes |
| `event.read` (additional nineteenth) | Disclose one mapped event's status/details | `ALLOW_EVENT_READ`; mapped event and guild ownership checks, minimal response, no arbitrary event enumeration. | No |

The nine moderation actions require **both** `TWO_INTERNAL_ALLOW_MODERATION=1`
and `TWO_MODERATION=1`. The policy library checks actor permissions, self-target,
owner/Owen/bot/protected-role exclusions and role hierarchy
(`crates/core/src/moderation.rs:312`; permission/target sources at `:103-112`).
Duration/number bounds are runtime-enforced, not just builder minima
(`crates/core/src/internal_actions.rs:1072`; cap validators at
`crates/core/src/moderation.rs:378-450` with non-echoing errors). Optional
bot-position inputs do not establish hierarchy when absent: the receiver must
acquire trusted facts and fail closed. REST execution is not adjudication:
`ActionExecutor::execute_outcome` turns one already-adjudicated
`ModerationExecution` into its effect (`crates/discord/src/executor.rs:1542`).
An authenticated website body is not a substitute for these checks.

## Signing, headers, replay and retries

Canonical string (literal LF separators, fixed method/path):

```text
POST\n/internal/actions\n{timestamp}\n{nonce}\n{lowercase_sha256_hex(raw_body)}
```

The signature is `sha256=` followed by lowercase HMAC-SHA256 hex. Key ID selects
the secret but is not a canonical-string field. HTTP header *names* may vary in
case; signed timestamp/nonce values, key IDs and signature bytes must not be
trimmed, lowercased, combined, or reserialized by a proxy. Core nonce syntax
accepts 32 ASCII hex characters in either case. Sender and receiver must use
the same exact representation. The HTTP layer must extract exactly one key-ID (`X-TWO-Key-Id`), timestamp, nonce
and signature header, and (where required) `Idempotency-Key`. Pin timestamp/nonce/
signature wire names against the website signer when implementing the route;
`AuthHeaders` is already-extracted data, not an HTTP header-name specification.
Reject duplicate/coalesced values, wrong method/path, unsupported encoding and
non-JSON content type. This extraction is **not** tested by the framework-free
core.

Freshness accepts an ASCII-digit timestamp of 1–15 characters when its Unix
seconds differ from receiver time by **at most 120**, inclusive. Leading zeroes
are currently parseable but remain distinct signed bytes. ±121, signs, decimal
fractions, trailing spaces and non-ASCII digits refuse. Both the millisecond and
whole-second clocks must come from one trusted clock sample, not request data.

**With freshness and expiry clocks advancing together without rollback**, a
timestamp can remain acceptable across 240 whole seconds plus a fractional
second. Hence the nonce retention minimum is `2 * skew + 1 = 241` seconds, **not
240**. The in-memory guard keeps a nonce through its inclusive TTL boundary and
rejects short TTL/wider skew configurations. It is global across key IDs but
lost on restart. Saturating subtraction refuses rollback while the nonce is
still retained; **it cannot restore a nonce already swept**. For example, accept
at `t`, sweep at `t + 241001 ms`, then roll wall time back to `t`: the original
capture is fresh again and the synchronous pipeline accepts it. A new fresh
request's `offer` can perform the same sweep. This is a characterized gap, not
remediation; finite TTL alone does not establish replay safety under rollback.

Durable burns store a global nonce digest; expiry replacement is atomic and
freshness is rechecked against DB time after lock/pool waits
(`crates/core/src/internal_action_store.rs:298`). Only committed `Ok(true)`
allows continuation; DB errors and ambiguous outcomes refuse. DB time rechecking
also depends on a clock policy: rollback between the expiry predicate and the
freshness sample can make an old capture fresh again. Existing durable rows are
not automatically pruned, so ordinary rollback while a row is retained is not
the memory-sweep case; future cleanup must not reopen it. These durable clock
cases were assessed from source, not executed against a database. F8 requires
explicit fail-closed freshness/expiry behavior across rollback, restart and
failover before receiver activation. The route must insert that async burn **between**
signature/freshness and buckets/body parsing, not call it only after the current
synchronous `authorize` helper (`crates/core/src/internal_actions.rs:1521`;
ordering contract at `crates/core/src/internal_action_store.rs:243`).

Nonce burn precedes parsing, allowlist and key bucket: an authenticated request
that is malformed, disabled, oversized or rate-limited has spent its nonce. A
retry needs a **new nonce/signature/timestamp** and the **same idempotency key and
raw payload** for the same intent. The 14 **I** actions bind action/payload to a
stable authenticated logical caller plus key digest. Changed action/payload is a
mismatch; fresh in-flight or stale/unknown intent is not permission to execute.
`CLAIM_STALE_SECONDS=60` is diagnostic, never an execution lease. There is no
automatic intent/event-ledger pruning; removing records can reopen duplication.
Settings CAS adds one retryable-shaped 409 that is not a replay: `VersionConflict`
means refresh the version and decide again, never blind-retry the same stale write.

`Idempotency-Key` is currently outside the MAC. A party capable of modifying a
valid request's headers before its first receipt can change that intent identity;
TLS/trusted hops are therefore essential. A versioned signing contract should
bind intent identity and audience before wider exposure (F3). Do not silently
change the legacy canonical format.

## Availability, buckets and binding

- Default authenticated key bucket: **20 burst, 1 token/second**. Add-member
  bucket: **10 burst, 0.5 token/second**, in addition to the key bucket. Denial
  returns 429 with `Retry-After >= 1`. Bad signatures do not allocate buckets or
  burn nonces; key-ID spoofing cannot exhaust a real caller's bucket.
- The bucket clock now retains its observed high-water mark on rollback rather
  than crediting recovery twice. `Retry-After` rounds up the **combined** time to
  recover that mark and refill the missing token. Exhaust at 10000 ms and deny
  at 9000 ms: default waits 2 seconds, add-member 3 seconds, not 1 and 2. This
  assumes the clock then advances normally and no competing request consumes
  the refill. Buckets are per-process and key-ID scoped; restart, horizontal
  scaling or rotation aliases can multiply quota. They are not a global guild
  budget or unauthenticated ingress defense.
- The **2 MiB inclusive body cap** currently lives in body parsing, after raw
  bytes have already been collected/hashed. It does not bound HTTP collection,
  crypto work, decompression or connection concurrency. Nonce insertion also
  precedes the bucket: TTL bounds entry lifetime, not flood-driven entry count
  or the cost of the in-memory full-map sweep. Bound ingress separately (F1/F7).
- Redirects use a separate per-isolate/per-caller **60 burst, 1/second** bucket
  (`wrangler/src/redirect.ts:117`) and validated invite codes with a fixed Discord
  destination host (`:92`). Reserved-internal paths 404 before the DB lookup for
  every method (`:211`), but **the caller map still has no size cap, idle expiry
  or eviction** (`:111-117`, `:229`): every distinct caller reaching the throttle
  allocates retained state before slug validation/lookup, including unknown-path
  404s. A many-caller client population can grow isolate memory indefinitely
  until recycle; spoof resistance of caller identification does not cap
  cardinality. Local handler fixtures retained 4096 synthetic callers after 404s,
  then 4097 after another caller at 30 simulated idle days; no live load was sent.
  F7 requires bounded caller state and safe eviction/overflow behavior. Neither
  this bucket nor `max_instances=1` proves global rate enforcement. Source shows
  no application gate on public health/readiness.
- The moderation `ActionExecutor` (`crates/discord/src/executor.rs`) paces its
  own clones at 110 ms general / 350 ms kick with 5-second-timeout
  moderation mutations (`:63-68`, paced-lane reservation held through the fence
  at `:503`, kick/get lanes at `:687`, `:819`). Member join/role paths use
  shorter single-attempt 1.5 s/2.0 s timeouts
  (`crates/discord/src/internal_exec/member.rs:20-21`). The separate
  `AnnouncementExecutor` (`crates/discord/src/internal_actions.rs:115`) is
  unpaced: one ten-second attempt and no retries. Every
  `RateLimited(RateLimitCooldown)` now feeds the caller-supplied per-bot-token
  `CooldownGovernor` (`crates/discord/src/internal_actions/governor.rs`), and the
  executor refuses with `NoEffect(CoolingDown)`, without HTTP, while a global or
  matching channel hold is active, so fresh intents no longer fire into an active
  cooldown. Holds only lengthen, untimed holds wait for reconciliation, and at
  most 1,024 channel holds are kept before overflow widens to the token-wide
  hold. The governor refuses rather than queues, so repeated 429s cannot keep an
  operation alive. It is per process and in memory; the receiver must still hand
  the same governor to all callers, guild workers and Discord transports for the
  token (`docs/internal-action-executor.md:43-64`). In-flight intents are not
  recalled. Pacing protects upstream quota; it is not action authorization or
  exactly-once. F7's governor and multi-intent 429 acceptance cases landed as
  paused-time fixtures for channel, global and repeated 429s plus the key
  ceiling (`crates/discord/src/internal_actions/tests.rs`, `governor.rs`).
  Ticket recovery adds
  bounded budgets (10-second button lane, 120-second cumulative, supervisor
  scope per #155); those bound ticket work, not action ingress.
- `assert_private_bind` accepts specific loopback/RFC1918/CGNAT/link-local IPv4,
  IPv6 loopback/ULA/link-local and mapped private IPv4; it refuses wildcard,
  public, hostname, malformed and host:port inputs with **no override**
  (`crates/core/src/internal_actions.rs:1430`). Bind the validated literal, not
  the original string or a subsequently resolved hostname. No private listener
  or network ACL was verified. The current Worker intentionally forwards
  `LISTEN_ADDR=0.0.0.0:<port>` **for probes only** (`wrangler/src/index.ts:105`);
  a public Worker proxy would still cross a public boundary even with a guarded
  private Container listener.

## Rejection logging and secret minimization

The receiver should emit one structured, bounded rejection record: generated
request ID, fixed code/status, fixed reason category, duration and validated
principal/action only once known. Before verification, use an `unverified`
principal label rather than reflecting arbitrary supplied key IDs. Invalid
signature and unknown key ID share the public 401 message
`Signature verification failed`; freshness/replay refusal comes only after MAC
verification. Do not claim microarchitectural timing equivalence was measured.

Never log raw body, OAuth token, signing secret, bot/DB token, signature, nonce,
full headers, free-text moderation reason, provider JSON/SQL error source or a
formatted whole error chain. `ActionError.message` can reflect untrusted action
or key names; use bounded fixed `log_reason` categories, not that message as an
audit field. Since the prior review the minimization controls improved (key/decoy
`Secret` wrappers, redacted transport/`RawResponse` Debug, `database_url`
allowlist plus passfile-target silencing, generic DB connection/migration
errors), but they are not a deployed logging policy. Staging head sampling 1
makes staging-log discipline load-bearing. Logging must be rate-bounded with
counters for suppressed events, so a rejection flood cannot exhaust storage.

Persist only the durable store's approved scalars/digests/typed terminal results;
return a failure only when the outcome is definitive. Timeout/transport ambiguity
needs `mark_unknown` and independent reconciliation, never a new automatic effect.
Routine sweeps should report findings only, not repeated clean-state messages.

## Key rotation procedure — documentation only

Credential creation/removal/rotation requires the credential-specific authorized
operator procedure. Route the decision brief through CISO and the CEO-curated
owner-reserved packet; this document is **not** that authorization and no step
was executed. Never hunt for or substitute credentials after an auth failure.

1. Record environment, current key IDs (not secrets), stable logical caller,
   custody/binding locations, approver and rollback limits. Verify the receiver
   can load an overlap ring without clearing durable replay/intent state. Stop
   if there is no receiver/custody approval or if environment isolation is unknown.
2. After authorization, provision a distinct high-entropy secret through the
   approved secret store under a new unique key ID (at least 32 random bytes;
   core only checks a 32-byte minimum length). No keys in commands, files,
   comments, browser bundles, logs or ordinary CI output. Do not reuse an old
   secret or the same secret for another environment/alias.
3. Install the receiver's old+new overlap ring first. Map both IDs to the **same
   stable principal** and shared operational quota; do not create a new execution
   scope. Verify with a read-only/local fixture or approved staging canary.
4. Switch the authorized website signer to the new ID and verify successful
   calls/rejection categories using scalar telemetry. Stop on an authentication
   error; investigate the expected binding, never try a different found key.
5. After all signers have switched, drain in-flight requests and allow the last
   old-signed timestamp's complete acceptance interval to close (conservatively
   at least 241 seconds after the last possible old signing, plus bounded
   transport/clock uncertainty, under the verified F8 clock policy). Elapsed
   time alone is not proof of closure if freshness clocks can roll back.
   This interval is not permission to keep a
   compromised key active: incident revocation takes priority, with explicit
   outage/retry consequences.
6. Under the same authorization, retire the old receiver key and verify old ID
   refusal, new ID success and unchanged nonce/intent replay protection. Preserve
   ledgers and scalar evidence. Monitor only finding counters. Secret destruction
   follows the separately approved custody procedure, not ledger pruning.

Rollback before retirement may return the signer to the still-authorized old key
only when it is not suspected compromised. Never restore a revoked/compromised
key, clear replay records or delete intents to recover availability. Cached old
intents remain scoped to the stable caller and reconcile without reexecution.

## Regression evidence and follow-up proposals

The ten prior tests in `crates/core/src/internal_actions.rs` are all still
present (verified 2026-10-01) and continue to pin cheap domain controls and
characterize the remaining clock-policy gap:

- `signing_preserves_header_values_and_raw_body_bytes`: changed key-ID case,
  timestamp/nonce representation, signature whitespace and JSON-equivalent body
  whitespace cannot reuse an original signature.
- `pipeline_signed_skew_edges_and_malformed_timestamps_do_not_burn`: signed ±120
  accepts, ±121/noncanonical numeric syntax refuses without nonce/bucket mutation.
- `pipeline_body_cap_is_inclusive_and_oversize_burns_nonce`: valid 2 MiB JSON
  accepts; one byte more refuses; the refused authenticated nonce stays burned.
- `pipeline_bad_signatures_cannot_poison_nonces_or_key_buckets`: repeated wrong
  signatures and unknown IDs yield identical refusal and leave valid traffic free.
- `pipeline_rejected_json_is_secret_safe_and_cannot_be_replayed`: rejected JSON
  cannot echo a synthetic OAuth marker or reclaim its authenticated nonce.
- `rotation_removes_old_key_without_forgetting_replay_state`: old key refusal
  after retirement; re-signing a burned nonce with the new key still refuses.
- `bind_guard_checks_mapped_ipv6_and_private_range_edges`: hexadecimal mapped
  IPv6 and adjacent private/public ranges cannot bypass the literal guard.
- `buckets_clock_rollback_does_not_refill_spent_tokens_twice`: both bucket specs
  refuse rollback/recovery double-credit, then refill only after new elapsed time.
- `buckets_retry_after_includes_clock_recovery_and_refill`: both bucket specs
  include rollback recovery, combine fractional recovery/refill before rounding,
  and allow the next call after the advertised wait without competing traffic.
- `pipeline_clock_rollback_after_nonce_expiry_reopens_capture`: retained nonce
  refuses rollback; after explicit sweep or another fresh request's sweep, the
  old capture is accepted when wall time rolls back. This regression records
  the known F8 gap; it does **not** demonstrate rollback-safe replay prevention.

New since the prior review (source regressions, not HTTP, deployment or DB
acceptance tests): four signing/key/moderation property tests in the same file,
`crates/core/tests/moderation_caps.rs` (inclusive edges, adjacent refusals,
non-echoing errors, builder parity), `database_url` allowlist/passfile tests,
and member/channel-moderation/ticket suites. Existing durable-store CI covers
transaction races/restarts/ambiguous outcomes. No local cargo toolchain exists
in this run (`cargo: command not found`), so neither `cargo fmt --check` nor
the bounded-cache compile ran locally; hosted CI (check + pr-lint + gitleaks)
on the exact head is the merge gate. Do not bypass the cache wrapper or create
another controller target.

F3 key-spec and body-shape refusals ([TOG-12243](/TOG/issues/TOG-12243)), same
file unless noted; the canonical string, signed fields and frozen vectors are
unchanged:

- `parse_keys_refuses_duplicate_ids_and_reused_secrets` and
  `property_duplicate_key_ids_and_reused_secrets_refuse_naming_ids_only`: a
  repeated ID (`DuplicateKeyId`) or a second ID with an existing secret
  (`ReusedSecret`) refuses the whole spec; error text names IDs, never secrets.
- `property_duplicate_json_keys_refuse_at_any_depth`: a repeated (optionally
  `\uXXXX`-escaped) key at any object/array depth refuses with the scalar
  `duplicate_json_key` class; distinct keys parse exactly as before.
- `pipeline_duplicate_json_keys_refuse_after_burning_the_nonce`: authenticated
  duplicate-key bodies refuse without echoing the key or an OAuth marker, the
  retry is `Replayed`, and both frozen vector bodies still parse.
- `crates/discord/tests/internal_member_store.rs`
  `repeated_json_key_refuses_before_claim_or_rest`: the member store reuses the
  same parser, so a repeated `discord_id` takes no claim and sends no REST call.

Remaining proposals are intentionally **not implemented** here:

| ID / priority | Next owner / proposal | Concrete acceptance test before activation |
| --- | --- | --- |
| F1 / P1 receiver gate | Receiver implementer ([TOG-10603](/TOG/issues/TOG-10603); PR #114 open, unmerged): default-deny Worker/DO routes, separate guarded bind policy, bounded byte collection/time/concurrency, exact method/path/content-type and single-header extraction | Through Worker and direct receiver: alternate paths/methods (including `/metrics*` canonicalization aliases), duplicate mixed-case headers, comma-joined values, oversized/chunked/encoded bodies refuse before unbounded allocation or mutation; valid exact bytes authenticate; public/wildcard bind cannot enable actions. |
| F2 / P1 receiver gate | Receiver implementer with Security review: wire durable pre-parse burn, stable principal mapping, committed claims, trusted user/guild/channel/event and permission/hierarchy facts, all action validators — now including ticket reservation/recovery fences, guild-fenced channel reads, `resolved_role_id` pinning and settings CAS conflict handling | Every verb (plus ticket/member/channel/settings-CAS paths): foreign guild/object and unauthorized/protected actor/target refuse; stale CAS writes conflict instead of reverting; restart/parallel/rejected/timeout requests cannot double-execute; DB failures never fall back to memory-only guards. Review the exact integration head. |
| F3 / P2 signing hardening | Website and receiver owners: design a versioned MAC binding audience, caller and idempotency identity (open). Landed ([TOG-12243](/TOG/issues/TOG-12243)): `parse_keys` refuses duplicate key IDs/reused-secret aliases; signed bodies with a repeated JSON key refuse at any depth | Alter audience/intent/header representation or send ambiguous JSON/key specs: refuse; frozen legacy vectors remain compatible until an explicitly approved version transition. CAS tokens stay opaque equality metadata, never new signed fields without a version. No silent canonical-format change. |
| F4 / P2 rejection telemetry | Receiver implementer: scalar structured logger with bounded labels/suppression | Capture every rejection class with token/body/SQL marker fixtures; no marker or full input escapes and rejection flood stays bounded. `Secret`/redaction and `database_url` allowlist work has landed; staging and production log head-sampling 1 make log proof load-bearing in both. |
| F5 / P2 action-specific safety | Action owners: mapped event ownership, automation import schema/cardinality/overwrite transaction, key-specific setting validation, tempban recovery and lockdown/unlock overwrite serialization | Unmapped events, excessive imports, protected roles, invalid setting types, conflicting channel intents and unknown outcomes fail closed; legitimate operation/reconciliation has scalar evidence. Moderation duration caps and settings CAS have landed as narrowing, not closure. |
| F6 / P1 deployment gate | Deployment owner with CISO: verify secret custody, per-environment guild/DB/key bindings, least-privilege DB role and mandatory authenticated TLS for Neon | Record non-secret binding/TLS/role receipts on the deployment card; test only fixtures/CI or explicitly authorized staging. Least-privilege roles/verifier/DML-only gateway/operator-migrates-first (via #102) and the `database_url` allowlist are procedure progress, not isolation proof; staging observability is telemetry, not a fence. Source URL-prefix validation is not TLS or isolation proof. |
| F7 / P2 current-public-surface and future ingress | Worker/receiver owners: edge probe limits, global guild/caller quotas, bounded redirect caller-map/nonce/intent growth, the shared per-bot-token/channel cooldown governor fed by every `AnnouncementExecutor` 429 (landed; receiver must share one per token), and total REST deadlines; preserve gateway resources | Local many-caller fixtures, including invalid-slug/unknown-campaign 404s and long simulated idle intervals, prove a fixed caller-state ceiling and idle reclamation (reserved-internal 404s and ticket/dispatch budgets have landed; the uncapped caller map has not). Define eviction/overflow behavior so churn cannot reset a depleted caller's quota or create unbounded work. Also prove bounded collection/concurrency; quota survives aliases/instances/restarts, unknown-key traffic cannot starve valid calls, repeated 429 cannot keep an operation alive indefinitely. The executor-level 429 governor is proven locally: the first intent returns `RateLimited(Global/Channel, retry_after_ms)`; a second genuinely new intent before that cooldown elapses is refused as `NoEffect(CoolingDown)` without a Discord send while other channels proceed, and a new intent proceeds only after the cooldown closes; the original key stays terminal throughout. Receiver wiring of one governor per token remains open. No destructive live load tests. |
| F8 / P1 receiver clock-policy gate | Receiver/store owners with Security review: define fail-closed behavior for backwards freshness/expiry clocks (including DB time), bounded retention and safe recovery across restart/failover; TTL coverage is conditional | Accept a capture, expire/sweep/replace its nonce, then roll time back into its signed window: memory and durable paths must refuse, including across instance restart/failover and lock waits. Record the trusted clock/high-water or equivalent policy and recovery criteria; cleanup (including transcript/intent retention) must not erase replay protection. Legitimate traffic resumes only under that verified policy. |

F1/F2/F8 are requirements for the existing receiver slice, not new route work in
this PR. F6 is evidence required at deployment, not authorization to touch secrets.
The other rows are follow-up proposals, not granted resources, new holds on
unrelated staging work or claims of completed remediation. None of the prior
follow-ups are closed by this refresh; owners above are re-affirmed. Update this
model and its regression inventory when a route, action, identity mapping,
storage/retention policy (including ticket transcripts, CAS tokens, or ledger
cleanup), or public exposure (including Worker observability/telemetry bindings)
changes.
