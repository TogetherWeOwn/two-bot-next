# TWO Bot Next: internal actions and Worker threat model

Reviewed 2026-10-10 against `f2e154c1ba06bae319a65030fdf377cfe690f41d`.
The private receiver (#343) and the staging-only ingress (#370) are merged on
main; `event.read` (#583) and the moderation, settings, membership, event and
channel-moderation executors (#663, #678, #679, #680, #686, #687) are wired
through the signed receiver. The website-action ingress is staging-only and
dark by default (see Scope). Production flag state is not verifiable from the
repository; it is recorded on the deployment receipt. This refresh changes no
code and asserts neither on nor off for any live environment.

Reviewed 2026-10-01 against baseline `5ddab2dfec52853a0c87cc6e555baf6575eae77e`
(delta `44338b28..5ddab2d` assessed; prior review 2026-09-30 against
`44338b28a7feac0093cbd72dc9cecc45ab33a15d`). The F7 redirect caller-map and
public-probe text (Denial of service row, redirect rate-limit notes, F7 row)
was refreshed 2026-10-02 against `c122126d678c98e171bdf92cb20d731a9529c93e`;
its `wrangler/` line citations refer to that commit. The F7 row and the
redirect `Retry-After` note were refreshed again 2026-10-02 for TOG-12533 on
top of `36816fba` (#290); the F7 row's `wrangler/` citations refer to that
change. The F3 row, the `Idempotency-Key` paragraph and the F3 references in
the Spoofing/Tampering rows were refreshed 2026-10-02 for TOG-12752: F3 is now
designed in [internal-action signing v2](internal-action-signing-v2.md); no
code changed.
This is a source-based STRIDE assessment, **not deployment approval**. No live
credentials, Discord mutations, database probes or key rotation were performed.
The private HTTP receiver (`crates/bot/src/internal_action_http.rs:1180`)
is merged on main with a body cap (`MAX_BODY_BYTES`,
`crates/core/src/internal_actions.rs:76`), a header cap
(`crates/bot/src/internal_action_http.rs:78-82`, enforced at `:2551`) and a
body timeout (`:81`, enforced at `:1260`); the receiver/executor integration
is wired for the verbs listed in Scope. The Worker ingress comment calls that
ingress "staging-only, dark by default" (`wrangler/src/index.ts:882-883`).

## Scope and current exposure

The historical contract has **18 actions**. The current core allowlist has
**19** (rechecked 2026-10-10): those 18 plus `event.read`
(`crates/core/src/internal_actions.rs:653`; nine moderation verbs at `:703`,
14 idempotency-bound at `:680`). All 19 are assessed below. Note drift:
`docs/parity.md:221` still describes the website→bot row as 18 actions and
omits `event.read` and settings compare-and-set; the core constants above are
authoritative. The legacy `docs/INTERNAL_ACTIONS.md` is not in this checkout;
the ported core, [parity inventory](parity.md) and
[durable-store contract](internal-action-store.md) are the source evidence here.

**Wired receiver executors (merged since the 2026-10-01 baseline).** The
announcement receiver (#343), the staging-only ingress (#370), `event.read`
(#583), moderation timeout (#663), moderation ban/tempban/kick/warn (#678),
settings get/set (#679), event upsert/cancel (#680), membership (#686) and
channel moderation (#687) are wired through the signed receiver
(`crates/bot/src/internal_action_http.rs:1180`): announcement execution
(`crates/discord/src/internal_actions.rs:115`), member join/role assignment
(`crates/discord/src/internal_exec/member.rs`), guild settings CAS,
channel-moderation wiring
(`crates/discord/src/internal_channel_moderation.rs:67`) and the ticket runtime
(`crates/bot/src/ticket_runtime.rs`) now execute behind that route when the
ingress gate below admits them.

- The ingress gate is code, and it is dark by default. The Worker routes exact
  `POST /internal/actions` (`ACTIONS_PATH`,
  `wrangler/src/internal-actions.ts:21`) to the Container only when
  `ingressEnabled(env)` is true (`wrangler/src/index.ts:883`), where
  `ingressEnabled` returns true only when `INTERNAL_ACTIONS_INGRESS` and
  `TWO_INTERNAL_ACTIONS` are both `"1"`
  (`wrangler/src/internal-actions.ts:62-63`; `receiverEnabled` at `:58-59`).
  In every other state the route does not exist: the caller sees current
  redirect/probe behavior and the Container is never contacted. Other paths
  keep current behavior: exact `/health` and `/readyz` go to the singleton
  Container and remaining paths enter the invite redirect handler. Reserved
  internal paths (`/metrics`, `/metrics/*`, canonicalized aliases) return 404
  for every method before lookup; other non-GET/HEAD probe paths return 405.
  Production flag state is not verifiable from the repository; it is recorded
  on the deployment receipt.
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
- The Container DO admits internal actions only through the same gate: the
  `internalActions` handler refuses unless `ingressEnabled` and the request is
  an exact actions request (`wrangler/src/index.ts:538`), then forwards the
  exact bytes to the fixed receiver port (`:544`). Other forwarding paths still
  refuse non-probe paths, and the ingress never starts the Container on an
  unauthenticated flood.
- HMAC authorization, private-bind validation, field validators, durable replay
  and execution claims are enforced in the merged receiver, not libraries.
  Signing secrets and the nonce decoy are `Secret`-wrapped
  (`crates/core/src/internal_actions.rs:150-152`, `:213-234`). The receiver
  serves `POST /internal/actions` with single-header extraction, a 2 MiB body
  cap, a 5-second body timeout and a 20-second request timeout
  (`crates/bot/src/internal_action_http.rs:78-82`, `:1180`, `:1214`, `:1260`,
  `:2551`). This document does not lift deployment holds; residual receiver
  and deployment items stay open under F1, F2 and F6 with the named owner
  roles below.
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
2. **Website → Worker → Container (actions flow, dark by default):** Internet
   traffic crosses the Worker routing/authentication boundary, then a separate
   DO forwarding boundary. Both admit actions only when `INTERNAL_ACTIONS_INGRESS`
   and `TWO_INTERNAL_ACTIONS` are both `"1"`
   (`wrangler/src/internal-actions.ts:62-63`); otherwise the route does not
   exist and callers see current redirect/probe behavior. The route is explicit
   and default-deny; merely forwarding all paths would expose the bot's remote
   control. Preserve signed body bytes across every hop and require TLS.
3. **Private receiver → core/store → Discord REST (present):** service signature,
   freshness and durable replay refusal precede parsing, authorization and an
   atomic execution claim. Only a committed new claim may cause a side effect;
   the executor assumes policy adjudication happened elsewhere. New since the
   prior review: settings writes carry compare-and-set (`expected_version`,
   `VersionConflict`/409) with opaque tokens, role assignment pins the
   allowlist-resolved role in the same intent/audit transaction
   (`crates/core/src/internal_action_store.rs:92`, `:328`), moderation channel
   reads are guild-fenced (`crates/discord/src/executor.rs:887`), and ticket
   close/reservation runs under row-lock fences with atomic transcript commits
   (`crates/cutover/src/tickets.rs:34`, `:180-199`). This now runs behind the
   merged receiver; it raises the F2 acceptance bar for the wired verbs.
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

Severity is the credible blast radius **when the receiver ingress is admitted**,
that is only when `INTERNAL_ACTIONS_INGRESS` and `TWO_INTERNAL_ACTIONS` are both
`"1"` (`wrangler/src/internal-actions.ts:62-63`); otherwise the route does not
exist and callers see current redirect/probe behavior. This is not an assertion
about any live environment: production flag state is not verifiable from the
repository and is recorded on the deployment receipt. P1 items below are release
gates for that integration. Current-public-surface findings are marked explicitly.

| STRIDE | Concrete threat / boundary | Existing control | Residual risk / required evidence |
| --- | --- | --- | --- |
| Spoofing | Forge a website request or enumerate accepted key IDs | HMAC-SHA256 on exact bytes; unknown key uses a random decoy; unknown ID and wrong signature return the same auth refusal. Secrets and the decoy are now `Secret`-wrapped, shrinking Debug/log exposure (`crates/core/src/internal_actions.rs:150-152`, `:213-234`) | Receiver must reject duplicate/coalesced auth headers and preserve values. No user identity or per-principal action scope is supplied by HMAC. P1 F1/F2. |
| Spoofing | Replay a capture on another environment, under a rotating identity, or after clock rollback | Timestamp/skew and global nonce burn; durable identity contract requires a stable logical caller; monotonic high-water guards refuse regressed freshness clocks fail-closed on both paths (`ClockGuard` in `crates/core/src/clock_guard.rs`, memory pipeline in `crates/core/src/internal_actions.rs:1575`, durable mark in `crates/core/src/internal_action_store.rs:270` + `0353_internal_clock_high_water.sql`) | Canonical MAC has no host, environment or caller ID (v2 binds audience and caller: [spec](internal-action-signing-v2.md), implementation pending); never reuse secrets across environments or aliases. Within one ring `parse_keys` refuses a repeated key ID or two IDs sharing a secret, naming IDs only; cross-environment reuse remains a custody check. Preserve ledgers during rotation. TTL alone is not a clock policy: the guards (not the TTL) close the expiry-then-rollback reopen. F2/F3/F8. |
| Tampering | Change body/action or exploit different HTTP/JSON parsers | MAC signs raw-body SHA256, not parsed/reencoded JSON; validators and allowlists refuse bad fields | `Idempotency-Key` is not signed (v2 signs it: [spec](internal-action-signing-v2.md), implementation pending); trusted transport is required. A signed body repeating a JSON object key at any depth (compared after unescaping) refuses as `Malformed`/`duplicate_json_key` before dispatch; the nonce stays burned and the key is never echoed. Bound collection before hashing and test proxy/path/method semantics. F1/F3. |
| Tampering | Bypass role/channel maps, foreign-guild fence or protected-target policy | Symbolic role/channel keys, catalog-only settings, moderation adjudication and guild-fenced persistence exist in libraries. Moderation permission/targets now resolve from one `command_permissions` source (`crates/core/src/moderation.rs:103-112`); website moderation channels are guild/type-checked (`crates/discord/src/executor.rs:887`); role assignment pins `resolved_role_id` at claim time (`crates/core/src/internal_action_store.rs:92`) | Receiver must call every relevant validator and obtain trusted live permission/hierarchy facts; `authorize` alone does not validate action fields. Settings CAS conflicts (`VersionConflict`/409 at `crates/core/src/internal_actions.rs:507`) must refresh, never blind-retry. P1 F2. |
| Repudiation | Retry a destructive action after an ambiguous outcome, or lose audit linkage | Store commits scalar intent and audit atomically; only `Claimed` allows execution; stale/unknown claims require reconciliation. Ticket close commits transcript atomically under row-lock fences (`crates/cutover/src/tickets.rs:180-199`); later audits copy the original role pin, never a re-evaluated map | Durable burn/claim path is wired in the merged receiver (`crates/bot/src/internal_action_http.rs:1300`). Open (Receiver owner): request/actor IDs still need trusted derivation; no arbitrary provider JSON in terminal records. Transcript purge and CAS-token retention remain open deletion/conflict surfaces; cleanup must not reopen duplication. P1 F2, F5. |
| Information disclosure | Leak OAuth/bot/signing/DB credentials via errors, tracing or settings | Redacted key/decision Debug, catalog denies environment-only/unknown settings, typed store errors/results. Since the prior review: `Secret`-wrapped signing keys/decoy, redacted transport/Debug (`crates/discord/src/executor.rs`), `RawResponse` shape-only Debug, `database_url` query allowlist plus passfile-target silencing, and generic DB connection/migration errors | HTTP rejection logger is not implemented. Logging full headers/body/error chains would undo minimization. Staging and production log head sampling 1 (`wrangler/wrangler.toml:84-86`, `:118-120`) raises log stakes in both. Source does not prove runtime TLS/custody. F4/F6. |
| Denial of service | Public probes wake/pin singleton Container or consume pool/crypto/memory | DO path allowlist, six-second readiness probe, pool/timeouts. Public `/health` and `/readyz` return 405 for non-GET/HEAD, then spend a per-caller **60 burst, 1/second** `healthBuckets` budget (429 with `Retry-After`) before the Container is touched, and forward a sanitized probe (`wrangler/src/index.ts:84-88`, `:410-444`; TOG-12245, #259). Redirects spend a separate `clickBuckets` map (`:65-67`, `:496`). Both are `TokenBuckets` with idle expiry no shorter than full refill, a 10,000-key fail-closed cap and a 64-entry sweep budget (`wrangler/src/redirect.ts:126-254`). Reserved-internal `/metrics*` 404s land before the bucket and DB lookup (`wrangler/src/index.ts:459`, `wrangler/src/redirect.ts:344-346`); readiness webhook failures stay generic and never log the secret URL (`wrangler/src/index.ts:382`) | Probe and redirect buckets are per isolate, not a global edge limit: a recycled or different isolate starts every caller full. The cap bounds memory but refuses every new caller while 10,000 keys are live; a new key at the cap first reaps idle entries, so a map of one-time callers drains on new-key traffic alone (fixed, TOG-12387). Probes have no application auth. In the merged receiver the Content-Length check and the 2 MiB / 5 s body read run before signature verification and nonce burn (`crates/bot/src/internal_action_http.rs:2577-2588`, `:1260`, `:1285-1300`); an authenticated nonce flood still burns nonces before the key bucket (`:1308-1310`). Gateway fan-out is detached per event, so a hostile event burst spawns bounded runtime work (ticket lane 10 s, cumulative 120 s) rather than stalling heartbeats — bound it anyway. F1/F7. |
| Denial of service | Spend the same clock interval twice in an action bucket | This change keeps a last-seen clock high-water mark | New regression covers both bucket specs; restart/multi-instance buckets are still local. Ingress concurrency is bounded in the merged path: the Worker admits 8 in-flight requests per isolate (`MAX_IN_FLIGHT`, `wrangler/src/internal-actions.ts:48`, `:232`) and the receiver admits 32, refusing the rest as `busy` (`crates/bot/src/internal_action_http.rs:80`, `:1201`). The Worker cap is per isolate, not global. F7. |
| Elevation of privilege | Compromised signer invokes all enabled verbs or changes its own gates | Environment-only approval flags; settings catalog denies `TWO_INTERNAL_*`, `TWO_MODERATION`, secrets and unknown keys; overwrite requires a second flag. Phase-1 defaults remain exactly `role.assign`, `announcement.post`, `event.upsert` (`crates/core/src/internal_actions.rs:778`); settings CAS rejects stale writes instead of silently reverting (`VersionConflict`) | Keys have no per-caller capabilities. Mapped roles/configured channels and allowed hot/cold settings still carry privilege; validate policy before enabling. P1 F2. |
| Elevation of privilege | Treat public wildcard health bind as an approved actions bind | `assert_private_bind` rejects wildcard/public/hostname addresses with no override inside that function (`crates/core/src/internal_actions.rs:1430`); the single marker-gated wildcard exception in the receiver config is documented below and is pending CTO/CISO decision (review TOG-16872) | Current health listener is deliberately `0.0.0.0`; never reuse it for an unguarded receiver or widen the marker-gated exception beyond the staging receiver. Internal `/metrics` shares that listener but is never proxied; treat it as non-public by routing, not by the bind. P1 F1. |

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
| `moderation.purge` | Irreversibly delete up to 100 messages per intent; repeated intents multiply loss | Same policy with channel authorization; count 1–100, configured guild/channel ownership check; never deletes pinned messages or the bot's own posts; messages over 14 days old are deleted singly, not in the bulk call Discord would reject. | Yes |
| `moderation.slowmode` | Restrict a channel for up to six hours or remove its slowmode | Same policy with channel authorization; seconds 0–21,600 and guild/channel fence. | Yes |
| `moderation.lockdown` | Deny channel participation; repeated intents can silence the guild | Same policy; change only the send/thread/reaction lockdown bits on @everyone (a role overwrite that allows sending still wins), validate channel ownership, refresh recovery seed/generation only when the live send deny is absent; do not grant new permissions as a side effect. | Yes |
| `moderation.unlock` | Restore participation; bad overwrite restoration can widen access | Same policy; read live masks, restore only the recorded lockdown bits (thread/reaction bits only while still locked), preserve other live bits, refuse send-bit drift; serialize with lockdown. External admin GET/write races are not fenced. | Yes |
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
still retained; the F8 high-water guard (`ClockGuard`,
`crates/core/src/clock_guard.rs`, wired into the pipeline at
`crates/core/src/internal_actions.rs:1540`) additionally refuses rollback past
swept entries: accept at `t`, sweep at `t + 241001 ms`, then roll wall time back
to `t`, and the rolled-back capture refuses as `stale_request` with a
`clock_rollback` log reason instead of accepting again — whether the sweep was
explicit or performed by another fresh request's `offer`. Within 5 s of the
mark the mark itself decides freshness (covering lock/pool-wait sampling skew);
further below it refuses fail-closed. Forward jumps behave as before. Finite
TTL alone does not establish replay safety under rollback; the guard does.

Durable burns store a global nonce digest; expiry replacement is atomic and
freshness is rechecked against DB time after lock/pool waits, now under the
persisted high-water mark (`internal_clock_high_water`,
`crates/cutover/migrations/0353_internal_clock_high_water.sql`, enforced in
`crates/core/src/internal_action_store.rs:270` in the same transaction as the
burn). Only committed `Ok(true)` allows continuation; DB errors and ambiguous
outcomes refuse, and a DB-time regression past the 5 s tolerance rolls the burn
back with `InvalidInput` even for a fresh nonce. Restart/failover re-derives
the mark from the table (`nonce_high_water_ms`), so persisted time cannot move
backwards past burned nonces. Existing durable rows are not automatically
pruned, so ordinary rollback while a row is retained is not the memory-sweep
case; future cleanup must not reopen it. The route must insert that async burn
**between** signature/freshness and buckets/body parsing, not call it only after
the current synchronous `authorize` helper (`crates/core/src/internal_actions.rs:1540`;
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
bind intent identity and audience before wider exposure (F3). That contract is
now specified in [internal-action signing v2](internal-action-signing-v2.md)
(audience, key ID, caller, expiry and idempotency key signed under a
`two-internal-action/v2` domain tag; principal-keyed buckets and dedupe; no
replay-window widening or schema change); implementation is pending. Do not
silently change the legacy canonical format: v1 stays as-is until an explicitly
approved version transition.

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
  (`clickBuckets`, `wrangler/src/index.ts:65-67`, `:496`; default spec
  `wrangler/src/redirect.ts:153-156`) and validated invite codes with a fixed
  Discord destination host (`:91-93`, `:110-112`). Reserved-internal paths 404
  for every method before the caller bucket and DB lookup
  (`wrangler/src/index.ts:459`, `wrangler/src/redirect.ts:344-346`). Other
  GET/HEAD paths, including invalid-slug and unknown-campaign 404s, take a
  caller bucket before slug validation/lookup (`wrangler/src/redirect.ts:368-371`).
- **Landed (TOG-12245, #259): the caller map is bounded.** `TokenBuckets`
  (`wrangler/src/redirect.ts:126-254`) idle-expires a bucket only after a full
  refill window (TTL clamped to at least `capacity / refillPerSecond`,
  `:166-173`, `:196-202`), so expiry never resets a depleted caller. It caps the
  map at **10,000** keys; a new key at the cap first runs the bounded reap and
  is refused fail-closed only if every slot is still live, while tracked keys
  keep their state (`:174-179`, `:203-216`), and each `take` reaps
  at most **64** idle entries from the least-recent end (`:180-185`,
  `:243-253`). Public `/health` and `/readyz` now pass a 405 method gate and a
  separate per-caller `healthBuckets` budget before the Container is touched
  (`wrangler/src/index.ts:84-88`, `:410-433`). Local fixtures:
  `wrangler/test/redirect.test.ts:586-763` (200 one-time keys reclaimed after
  the idle window, a depleted key keeps its debt through churn, TTL clamp, cap
  shedding, bounded sweep, idle map drains on new keys alone, reap before
  refusal within budget, live full map still refuses and a depleted key keeps
  its debt), `:342-360` (burst 429 before the store) and
  `wrangler/test/health-probes.test.ts:114-139` (probe burst 429, other callers
  and 405s unaffected, throttled probes never reach the Container). No live
  load was sent.
- Residual risk: the buckets are **per isolate, not global**. Each Worker
  isolate holds its own maps, and a recycled or different isolate starts every
  caller full, so effective quota grows with isolate count; `max_instances = 1`
  bounds the Container, not Worker isolates. The cap trades memory for
  availability: once 10,000 keys are live in one isolate, every new caller gets
  429, and keying on the full client IP lets one IPv6 prefix mint distinct
  keys. **Fixed (TOG-12387):** overflow refusals used to skip the sweep, so a
  full map drained only when an already-tracked key returned (a 100-key fixture
  still refused 50 of 50 new keys after 24 idle hours). A new key at the cap now
  runs the same bounded 64-entry reap before its verdict
  (`wrangler/src/redirect.ts:203-216`); the fixture admits 50 of 50
  (`wrangler/test/redirect.test.ts:670-688`). The redirect 429 used to send a
  fixed `Retry-After: 1`, including cap refusals; since TOG-12533 it forwards
  the bucket's own wait (refill wait, remaining terminal hold, or the idle
  window for a cap refusal; see the F7 row). Canonical caller identity, the
  bounded unknown budget and the terminal hold landed in TOG-12469 (#290).
  Global quota stays open under F7.
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
  public, hostname, malformed and host:port inputs with **no override inside
  that function** (`crates/core/src/internal_actions.rs:1430`). Bind the
  validated literal, not the original string or a subsequently resolved
  hostname. The one exception to "never a wildcard bind" lives outside that
  function, in `InternalActionConfig::from_lookup`: it admits the wildcard
  only with the Worker-set `TWO_INTERNAL_CONTAINER` marker exactly `1`
  (TOG-16851: the Containers port check cannot reach loopback; the container
  network is presumed private but no private listener or network ACL was
  verified; the exception is pending CTO/CISO decision, review TOG-16872).
  The marker is a
  Worker-set deployment claim, not a verified proof, so it must never come
  from Operator input or `wrangler.toml` (rejected by
  `scripts/check-env-bindings.py`). No private listener or network ACL was
  verified. The
  current Worker intentionally forwards `LISTEN_ADDR=0.0.0.0:<port>` **for
  probes only** (`wrangler/src/index.ts:105`); a public Worker proxy would still
  cross a public boundary even with a guarded private Container listener.
- **Staging-only ingress (TOG-12980, CISO conditions on TOG-12979; bind TOG-16851):**
  the staging Worker proxies exactly `POST /internal/actions` to a wildcard
  receiver listener (`0.0.0.0:8091` plus the Worker-set `TWO_INTERNAL_CONTAINER`
  marker, never Operator values). TOG-16851 proved a loopback-only socket is
  unreachable from the Containers port check and `containerFetch`; the container
  network is presumed private but unverified, and the bot refuses the wildcard
  without exactly that marker. It is dark unless the staging-only var
  `INTERNAL_ACTIONS_INGRESS` and
  the Operator secret `TWO_INTERNAL_ACTIONS` are both `1`;
  `scripts/check-env-bindings.py` denies the var in production. The Worker bounds
  method, path, query, content type, 2 MiB body, timeouts, header allowlist,
  per-IP and in-flight caps before the Container is touched, forwards bytes
  unchanged inside the ownership fence, and never relays Container error text or
  starts the Container. Authentication is still the receiver's v1 HMAC and
  durable nonce burn. Caps are per isolate (the residual in F7 above). See
  [the receiver doc](internal-actions-receiver.md#staging-ingress-default-dark).

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
- `pipeline_clock_rollback_after_nonce_expiry_refuses_capture` (replaces the
  former `..._reopens_capture` gap record): retained nonce refuses rollback as
  a replay; after explicit sweep or another fresh request's sweep, the
  rolled-back capture refuses as `stale_request` with a `clock_rollback` log
  reason. This regression demonstrates F8 fail-closed memory behavior.
- `pipeline_clock_within_tolerance_decides_at_high_water`: a delivery within
  5 s behind the mark authorizes against the mark without moving it.
- `pipeline_clock_forward_jump_behaves_as_before`: forward jumps advance the
  mark, authorize fresh captures, and leave old captures stale (not rollback).
- `nonce_db_rollback_after_expiry_refuses_capture`
  (`crates/core/tests/internal_action_store.rs`, DB): burn, expire and replace
  a nonce, roll DB time back into its signed window with injected row/mark
  time (server clock untouched), and the capture refuses — on the live store
  and on a fresh store restoring `nonce_high_water_ms`; forward DB time still
  burns fresh captures.
- `clock_guard` unit tests (`crates/core/src/clock_guard.rs`): first read sets
  the mark, forward time advances it, within-tolerance reads decide at the
  mark, past-tolerance reads refuse without moving the mark, and a restored
  mark refuses an earlier clock while resuming at/above it.

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
| F1 / P1 receiver gate | Receiver owner (merged #343, #370; prior draft #114 superseded): default-deny Worker/DO routes, separate guarded bind policy, bounded byte collection/time/concurrency, exact method/path/content-type and single-header extraction are merged (`crates/bot/src/internal_action_http.rs:78-82`, `:1180`, `:2551`; `wrangler/src/index.ts:883`; `wrangler/src/internal-actions.ts:62-63`). Landed: the Worker caps in-flight ingress at 8 per isolate before the Container (`MAX_IN_FLIGHT`, `wrangler/src/internal-actions.ts:48`, `:232`; tested at `wrangler/test/internal-actions.test.ts:346`) and the receiver admits 32 (`crates/bot/src/internal_action_http.rs:80`, `:1201`). Open: the Worker cap is per isolate, not global, so total Worker in-flight grows with isolate count; Deployment owner records bind/marker receipts on the deployment receipt | Through Worker and direct receiver: alternate paths/methods (including `/metrics*` canonicalization aliases), duplicate mixed-case headers, comma-joined values, oversized/chunked/encoded bodies refuse before unbounded allocation or mutation; valid exact bytes authenticate only when both ingress flags are `"1"`; public/wildcard bind cannot enable actions without the Worker-set marker. |
| F2 / P1 receiver gate | Receiver owner with Security review (merged #583, #663, #678, #679, #680, #686, #687): durable pre-parse burn, stable principal mapping, committed claims, trusted user/guild/channel/event and permission/hierarchy facts, all action validators — including ticket reservation/recovery fences, guild-fenced channel reads, `resolved_role_id` pinning and settings CAS conflict handling — are wired for 17 of the 19 allowlisted verbs (`IMPLEMENTED_ACTIONS`, `crates/core/src/internal_actions.rs:653`). `automations.import` and `automations.export` have no adapter and refuse as `ActionNotAllowed` after authorization (`crates/bot/src/internal_action_http.rs:1390-1392`). Open: Receiver owner wires the two automation verbs (with the F5 import safeguards) or keeps them refused, and proves every wired verb refuses foreign guild/object and unauthorized/protected actor/target on the exact integration head | Every verb (plus ticket/member/channel/settings-CAS paths): foreign guild/object and unauthorized/protected actor/target refuse; stale CAS writes conflict instead of reverting; restart/parallel/rejected/timeout requests cannot double-execute; DB failures never fall back to memory-only guards. Review the exact integration head. |
| F3 / P2 signing hardening | Website and receiver owners: versioned MAC binding audience, caller and idempotency identity — designed ([spec](internal-action-signing-v2.md), [TOG-12752](/TOG/issues/TOG-12752)); implementation pending (six slices listed in the spec). Landed ([TOG-12243](/TOG/issues/TOG-12243)): `parse_keys` refuses duplicate key IDs/reused-secret aliases; signed bodies with a repeated JSON key refuse at any depth | Alter audience/intent/header representation or send ambiguous JSON/key specs: refuse; frozen legacy vectors remain compatible until an explicitly approved version transition. CAS tokens stay opaque equality metadata, never new signed fields without a version. No silent canonical-format change. Activation evidence: the spec's acceptance list (single-field tamper, cross-audience/caller refusal, v1-under-v2-only refusal, inclusive lifetime/skew edges, rotation retry dedupe, log-marker fixtures). |
| F4 / P2 rejection telemetry | Receiver implementer: scalar structured logger with bounded labels/suppression | Capture every rejection class with token/body/SQL marker fixtures; no marker or full input escapes and rejection flood stays bounded. `Secret`/redaction and `database_url` allowlist work has landed; staging and production log head-sampling 1 make log proof load-bearing in both. |
| F5 / P2 action-specific safety | Action owners: mapped event ownership, automation import schema/cardinality/overwrite transaction, key-specific setting validation, tempban recovery and lockdown/unlock overwrite serialization | Unmapped events, excessive imports, protected roles, invalid setting types, conflicting channel intents and unknown outcomes fail closed; legitimate operation/reconciliation has scalar evidence. Moderation duration caps and settings CAS have landed as narrowing, not closure. |
| F6 / P1 deployment gate | Deployment owner with CISO: verify secret custody, per-environment guild/DB/key bindings, least-privilege DB role and mandatory authenticated TLS for Neon | Record non-secret binding/TLS/role receipts on the deployment card; test only fixtures/CI or explicitly authorized staging. Least-privilege roles/verifier/DML-only gateway/operator-migrates-first (via #102) and the `database_url` allowlist are procedure progress, not isolation proof; staging observability is telemetry, not a fence. Source URL-prefix validation is not TLS or isolation proof. |
| F7 / P2 current-public-surface and staging-only ingress (merged #370) | Worker/receiver owners: global edge/guild/caller quotas, bounded nonce/intent growth (redirect caller map, per-isolate probe limits, canonical caller identity, unknown budget and terminal 429 hold landed), the shared per-bot-token/channel cooldown governor fed by every `AnnouncementExecutor` 429 (landed; receiver must share one per token, TOG-11045), and total REST deadlines; preserve gateway resources | Landed for the Worker: bounded redirect caller map and public probe gate (TOG-12245, #259). `TokenBuckets` (`wrangler/src/redirect.ts:171-372`) idle-expires only fully refilled buckets, caps at 10,000 keys fail-closed and sweeps at most 64 per call; `/health`/`/readyz` take a separate `healthBuckets` budget before the Container (`wrangler/src/index.ts:99`, `:548-572`). Fixtures `wrangler/test/redirect.test.ts:589-766` prove idle reclamation, a fixed ceiling, churn that cannot reset a depleted caller, bounded sweep work and reap-before-refuse at the cap; `wrangler/test/health-probes.test.ts:122-147` proves the probe 429. **Landed (TOG-12469, #290):** canonical caller identity, so case, whitespace, IPv6 zone-id and `::ffff:` aliases share one bucket (`canonicalCallerKey`, `wrangler/src/redirect.ts:143-169`); one bounded `unknown` budget for callers with no edge signal, which cannot mint entries or starve valid callers; and a terminal hold after 25 straight denials (60 s) that retries can neither shorten nor extend (`take`, `:277-355`). **Landed (TOG-12533):** the redirect 429 forwards the bucket's own `Retry-After` (whole seconds, at least 1), so a held caller is told the remaining hold instead of a one-second retry loop (`:506-516`, wired at `wrangler/src/index.ts:643`). Refill accrues across a hold, capped at capacity, by design: the hold bounds the denial streak, not admissions, and a caller that honors the wait gets what an idle caller would (review sim, 100 rps for 600 s: 600 admits with the hold, 659 without). Fixtures `wrangler/test/redirect.test.ts:768-986` and `wrangler/test/redirect-worker.test.ts:102-132` prove alias folding, the unknown budget, the hold and refill across it, and a counting-down hold `Retry-After` through the handler and the Worker entry with no store lookup during the hold, while a refill denial still gets 1. Reserved-internal 404s and ticket/dispatch budgets have also landed. Residual (open): buckets are per isolate, not global, so quota does not survive other instances or restarts (a recycled or different isolate starts every caller full); 10,000 live keys in one isolate refuse new callers there, and one IPv6 prefix can still mint distinct keys; a map of idle keys drains on new-key traffic alone (fixed, TOG-12387). Byte/time collection is bounded in the merged path (2 MiB cap, 5 s body and 20 s request timeouts). **Landed:** the Worker caps in-flight ingress at 8 per isolate (`MAX_IN_FLIGHT`, `wrangler/src/internal-actions.ts:48`, `:232`), and `wrangler/test/internal-actions.test.ts:346` proves the over-cap request gets 429 and capacity returns once requests settle. Still open (Worker owner): that cap is per isolate, not global, so total Worker in-flight grows with isolate count; the receiver's own 32-request cap is the bound at the Container. The executor-level 429 governor is proven locally: the first intent returns `RateLimited(Global/Channel, retry_after_ms)`; a second genuinely new intent before that cooldown elapses is refused as `NoEffect(CoolingDown)` without a Discord send while other channels proceed, and a new intent proceeds only after the cooldown closes; the original key stays terminal throughout. Receiver wiring of one governor per token remains open (TOG-11045). No destructive live load tests. |
| F8 / P1 receiver clock-policy gate | Landed: `ClockGuard` high-water policy (`crates/core/src/clock_guard.rs`, 5 s tolerance) enforced on the memory pipeline (`crates/core/src/internal_actions.rs:1540`) and the durable burn (`crates/core/src/internal_action_store.rs:270` + `0353_internal_clock_high_water.sql`); restart/failover restores `nonce_high_water_ms`; bounded state (mark only, no per-nonce history) | Accept a capture, expire/sweep/replace its nonce, then roll time back into its signed window: memory and durable paths refuse, including across instance restart/failover and lock waits — pinned by `pipeline_clock_rollback_after_nonce_expiry_refuses_capture` and `nonce_db_rollback_after_expiry_refuses_capture`. Cleanup (including transcript/intent retention) must not erase replay protection. Legitimate traffic resumes only under that verified policy. |

F1/F2/F8 are requirements for the merged receiver above, not closed by this
refresh. F6 is evidence required at deployment, not authorization to touch secrets.
The other rows are follow-up proposals, not granted resources, new holds on
unrelated staging work or claims of completed remediation. None of the prior
follow-ups are closed by this refresh; owners above are re-affirmed. Update this
model and its regression inventory when a route, action, identity mapping,
storage/retention policy (including ticket transcripts, CAS tokens, or ledger
cleanup), or public exposure (including Worker observability/telemetry bindings)
changes.
