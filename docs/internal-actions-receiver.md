# Private internal-actions receiver

## Implementation and deployment boundary

**Direct-container support is not the shipped Worker capability set.** The
container source integrates an opt-in private `POST /internal/actions`
listener with the durable store, announcement executor, event-read,
event-mutation, settings, automations, moderation, membership and
channel-moderation executors, nonce-commit authentication capability and strict
receiver configuration. It supports `announcement.post`, the keyless `event.read`
(behind `TWO_INTERNAL_ALLOW_EVENT_READ`), `event.upsert` and `event.cancel`
(cancel behind `TWO_INTERNAL_ALLOW_EVENT_CANCEL`), `settings.get`/`settings.set`
(behind `TWO_INTERNAL_ALLOW_SETTINGS`), `voice.config.get`/`voice.config.set`
(behind the separate `TWO_INTERNAL_ALLOW_VOICE_CONFIG` gate), the automations pair
`automations.export` (keyless redacted read) / `automations.import` (claimed
transactional import), the restrictive member verbs `moderation.ban`,
`moderation.tempban`, `moderation.kick`, `moderation.warn` and
`moderation.timeout`, the membership pair (`role.assign` through the
allowlisted role-key map, `guild.add_member`
behind its own allowlist flag with a transient OAuth token), and the
channel-moderation verbs (`moderation.purge`, `moderation.slowmode`,
`moderation.lockdown`, `moderation.unlock`), regardless of the core action
catalogue's broader defaults. Every other verb stays refused by the per-effect
fences.
The public health/readiness/metrics router has no action route. The shipped
Worker exposes only the staging ingress below, dark until separately enabled.
Its explicit container wiring carries neither role keys nor the optional
`TWO_INTERNAL_ALLOW_*` gates: `role.assign` has no configured role target, and
the gated verbs above stay refused. The ungated `announcement.post` and
`event.upsert` paths remain subject to receiver authentication, target validation
and durable execution fences; this is not evidence that either has run.
Adding secrets cannot enable unforwarded role/capability gates. Do not stage
those inputs to try to enable them. Widening forwarding requires a separate
reviewed change; direct-container configuration support is not rollout authority.

No signing key, secret custodian or export authority is supplied by this code.
Key creation and transfer into the website's staging secret store stay on HOLD
until separately approved and recorded. Never use the signing test fixture as a
runtime credential.

## Staging ingress (default dark)

TOG-12980, with the security conditions C1-C10 of the CISO verdict on TOG-12979.
Production has no ingress, route, variable or secret for this receiver.

| Layer | Behavior |
| --- | --- |
| Gate | Worker var `INTERNAL_ACTIONS_INGRESS = "1"`, declared only under `[env.staging.vars]`, **and** the Operator's Worker secret `TWO_INTERNAL_ACTIONS` exactly `1`. Either one missing, or any other value, leaves the route absent: today's response, no container contact. `wrangler/scripts/check-env-bindings.py` fails a top-level or production declaration and any `TWO_INTERNAL_*` var in `wrangler.toml`. |
| Route | Exactly `POST /internal/actions`, no query string, no trailing slash. Everything else, including other methods, is today's behavior (404). |
| Request bounds | `application/json` (optionally `; charset=utf-8`), no `Content-Encoding`, body 1 byte to 2 MiB (413 over, declared or streamed); separate budgets below. Per-IP token bucket (burst 20, 1/s) and 8 concurrent requests per isolate answer 429 before the container is touched. |
| Headers | The forwarded-header table below is the allowlist, including validated/minted `x-request-id`. `Authorization`, `Cookie`, `X-Forwarded-*` and any caller-supplied deployment header are dropped. The Worker stamps `x-two-bot-deployment-id` itself for the DO ownership fence; it is not caller authentication. |
| Body | Forwarded byte for byte; the signature covers the raw bytes. Never re-serialized. |
| Fence | Forwarded only inside the Durable Object's ownership gate. A stale, fenced or missing deployment answers 503 `unavailable` and never reaches the container. |
| Startup | Public ingress never starts the container; probes and the keepalive do. While the container is not running the answer is 503 `unavailable`. |
| Answers | The receiver's JSON envelope is validated against the [relayed answer contract](#relayed-answer-contract) and projected: successes keep their action-specific result verbatim beside the correlated id (plus the optional top-level settings version), refusals keep their table status/code/retryability with the fixed message. Headers relayed are `Content-Type`, `Idempotent-Replay` and `Retry-After` only, `Cache-Control: no-store`. Any non-JSON, oversize, transport-failure, malformed, contradictory or unknown-code answer (including SDK startup text that embeds error messages) becomes the fixed `unavailable` envelope. Worker refusals use the same envelope with a fixed message. |
| Logs | Refusal events use exactly the scalar log fields below; not a success/access log for every request. Never raw header values, nonce, signature, body or query. The sole header-derived log value is the shape-validated correlation id. |

### Intake, forwarding and correlation

The Worker reads the inbound body within a separate 5-second budget before the
20-second forwarding budget begins. These are not a single whole-request limit.
The forwarding budget includes the DO/receiver round trip and bounded response
collection, not a fresh budget for each hop. A forwarding timeout can leave an
effect outcome unknown; cancellation is not proof that nothing happened.

| Phase | Budget (ms) | Starts | Status | Code | Retryable | Next action |
| --- | --- | --- | --- | --- | --- | --- |
| Intake | 5000 | Before forwarding | 408 | `request_timeout` | true | Retry same Idempotency-Key; fresh nonce/signature |
| Forwarding | 20000 | After body read | 504 | `upstream_timeout` | true | Unknown outcome: retry same Idempotency-Key; reconcile needs_reconciliation first |

Both timeouts use the fixed `request refused` message. Intake timeout contacts
no container. Honor `Retry-After` when present. For any uncertain outcome, keep
the same intent's `Idempotency-Key`, with a fresh timestamp/nonce/signature for
each attempt; never mint a new intent key to escape `in_progress` or
`needs_reconciliation`. The latter requires reconciliation before resubmission.
A response-read abort can instead become Worker `503 unavailable`; it is not
proof of no effect either (see [next actions](#relayed-answer-contract)).

| Forwarded header | Value policy |
| --- | --- |
| x-two-key-id | Required single printable ASCII token, no comma; at most 128 bytes |
| x-two-timestamp | Required single printable ASCII token, no comma; at most 15 bytes |
| x-two-nonce | Required single printable ASCII token, no comma; at most 32 bytes |
| x-two-signature | Required single printable ASCII token, no comma; at most 71 bytes |
| idempotency-key | Optional at ingress; when present a single printable ASCII token, no comma; at most 200 bytes. Receiver action rules still apply |
| content-type | Required accepted JSON media type; at most 128 bytes |
| x-request-id | Validated or minted; 26-character Crockford base32; correlation only, not identity |

A caller's generated-shape `x-request-id` is preserved; absent, malformed or
list-valued ids are replaced. It joins the forwarded request, response JSON
`request_id`, DO and receiver logs. Shape validation does not make it a trusted
caller identity or a deduplication key. Signing headers remain untrusted until
the receiver verifies HMAC and burns the nonce; ingress validation alone does
not authenticate them.

| Log field | Value / provenance |
| --- | --- |
| event | Fixed `internal_actions_ingress` |
| status | Worker-selected numeric refusal status |
| error_class | Fixed refusal code, never remote exception text |
| request_id | Validated/minted request correlation id, not authenticated identity |
| release | Worker-supplied container build SHA, normalized to 7–40 lowercase hex characters or `unknown`; never a caller deployment header |
| log_url | https://developers.cloudflare.com/workers/observability/logs/workers-logs/ |

`release` is container build correlation, not a Worker-version or caller claim;
`log_url` is the fixed public log-query guide, not a request URL or an
account-scoped log link. Neither exposes secrets. Missing build context uses
`unknown`; the caller cannot supply a release through request headers.

Authentication is unchanged: legacy v1 HMAC and a durable nonce burn at the
receiver. The ingress adds no bearer, key or second identity.

### Container wiring

These are the complete receiver inputs shipped by `containerEnvVars`
(`wrangler/src/index.ts`), **not** `FORWARDED_FLAGS`. They reach the container
only while `TWO_INTERNAL_ACTIONS` is exactly `1`:

| Name | Source |
| --- | --- |
| `TWO_INTERNAL_ACTIONS` | Worker secret, forwarded as `1` |
| `TWO_INTERNAL_BIND` | Worker constant `0.0.0.0:8091` (TOG-16851: the Containers port check and `containerFetch` cannot reach a loopback-only socket). An Operator-supplied value is ignored, so the bind can never become a public address. |
| `TWO_INTERNAL_CALLERS` | Worker secret, e.g. `web-staging:website-staging` |
| `TWO_INTERNAL_CONTAINER` | Worker constant `1`: the Worker sets it to mark the process as running inside the container network, where the wildcard bind is reachable only via `containerFetch` and the startup port check. That network is presumed private but unverified. The marker is a deployment claim, not a proof: the bot trusts it only because the Worker (not the Operator) sets it. A wildcard bind without exactly this marker is refused. |
| `TWO_INTERNAL_CHANNEL_KEYS` | Worker secret, e.g. `smoke-throwaway:<channel snowflake in the TWO Staging guild>` |
| `TWO_INTERNAL_KEYS` | Worker secret `web-staging:<64 hex>`; never a var, never logged |

`NOT_FORWARDED` in `wrangler/src/container-env.ts` deliberately excludes the
following. Setting them as Worker secrets cannot change the container's
capabilities; this table is not an instruction to create or stage them.

| Not forwarded | Effect |
| --- | --- |
| `TWO_INTERNAL_ROLE_KEYS` | No allowlisted role target; `role.assign` refuses |
| `TWO_INTERNAL_ALLOW_ADD_MEMBER` | `guild.add_member` stays gated off |
| `TWO_INTERNAL_ALLOW_AUTOMATIONS` | Automation export/import stay gated off |
| `TWO_INTERNAL_ALLOW_AUTOMATIONS_OVERWRITE` | Automation overwrite stays gated off |
| `TWO_INTERNAL_ALLOW_EVENT_CANCEL` | `event.cancel` stays gated off |
| `TWO_INTERNAL_ALLOW_EVENT_READ` | `event.read` stays gated off |
| `TWO_INTERNAL_ALLOW_MODERATION` | Member/channel moderation stay gated off, even with `TWO_MODERATION=1` |
| `TWO_INTERNAL_ALLOW_SETTINGS` | Settings get/set stay gated off |
| `TWO_INTERNAL_ALLOW_VOICE_CONFIG` | Voice configuration get/set stay gated off |

Fixed receiver port: **8091** (health stays on `BOT_PORT` 8080, and the two may
not be equal while the receiver is enabled). While enabled, 8091 joins the
container's startup port checks, so a sidecar that cannot reach the receiver
fails the start instead of reporting a half-working bot. TOG-16851: a
loopback-only socket is unreachable from the Containers port check and
`containerFetch`, so the Worker binds the wildcard and the bot accepts it only
with the `TWO_INTERNAL_CONTAINER` marker; other binds still pass
`assert_private_bind`.

### Enable order and rollback

Every Worker secret change publishes a new Worker version, and the ownership
fence keeps a new version from serving until the Operator transfers ownership
(see [persisted ownership control](runbook.md#persisted-ownership-control)).
Stage the secrets with `wrangler versions secret put --env staging <KEY> --message <sha>` (a version
that is not yet deployed), deploy that one version, then run the takeover once.
Stamp with the live `/readyz` `build_revision` of staging; an unstamped secret-change version can
never form a rollback receipt (see `docs/production-deploy.md`). Precheck first:
`versions secret put` builds on the newest-created version, not necessarily the
serving one — confirm the last block of `wrangler versions list --env staging`
(the list prints oldest first, so the last block is the newest version)
is the serving version (same id as `wrangler deployments status --env staging`),
and if they differ redeploy the intended SHA through the workflow first (full rule
in `docs/production-deploy.md`).

For a binary containing voice configuration actions, the authorized migration
executor must apply `0469_voice_settings_versions.sql` and the reviewed role
plan **before** that binary can serve, including with the receiver or voice
capability dark. Existing Discord writers use the new audit schema regardless
of those gates. The original receiver enablement sequence below is not a
voice deployment authorization; its merge-first step does not waive required
pre-merge staging evidence. Keep the additive voice schema and audit on rollback;
never drop voice columns or delete audit history.

1. Merge the original receiver change only after its applicable gates pass. `deploy-staging` deploys it dark; `/health` and `/readyz` are unchanged.
2. Operator stages `TWO_INTERNAL_CALLERS` and `TWO_INTERNAL_CHANNEL_KEYS` (plain values), only under the separately authorized enablement contract; no role keys or optional capability gates travel through this Worker.
3. Operator generates the key on the Operator host and stages `TWO_INTERNAL_KEYS`, and sets the website's `staging` environment secret `BOT_SHARED_SECRET` to the same value, without printing it.
4. Operator applies migrations `0423` (settings CAS) and `0424` (event outcome) first (the bot does not apply migrations; an enabled receiver's finish fails without the columns), then stages `TWO_INTERNAL_ACTIONS` as `1` **last**, deploys the version, and transfers ownership. The container restart applies the settings: an invalid combination exits the process (the receiver boots all-or-nothing) and keeps staging red until step 5.
5. Rollback: stage deletion of `TWO_INTERNAL_ACTIONS` (`wrangler versions secret delete --env staging TWO_INTERNAL_ACTIONS --message <sha>`, same precheck and stamp as above), deploy and transfer ownership; or use the existing Worker-version rollback. The route is absent again and the next container start carries no receiver setting.

Migration `0424` adds the nullable `internal_idempotency.outcome` column that
event receipts read back. Roll back in this order: disable the receiver (step 5)
and deploy the previous binary, then drop the column. Dropping it while this
binary runs fails every finish and every success replay, not only event receipts.

Migration `0448` adds nullable `stop_reason` columns to `internal_idempotency`
and `internal_action_log`, so a purge receipt and its terminal audit keep an
early stop's reason for outer replays. Every finish writes the column and every
replay reads it, so apply it before this binary and roll back in the same order
as `0424`: disable the receiver and deploy the previous binary, then drop both
columns.

TOG-16851 proved a loopback-only receiver never becomes healthy: the Containers
port check and `containerFetch` cannot reach it, so the container waited on 8091
forever. The fix binds the wildcard behind the Worker-set container marker (the
container network is presumed private — no private listener or network ACL was
verified; HMAC/caller checks are unchanged). If the startup
port check for 8091 fails after this change, roll back and hand the question to
the CTO.

## Authentication across a durable nonce commit

`AuthenticatedRequest::verify` authenticates the four v1 headers and exact body
bytes before evaluating the process-clock `ClockGuard` or checking freshness.
It neither parses JSON nor consumes a bucket. The returned capability is opaque,
non-cloneable and not printable. It keeps those same bytes, header values and
guarded process time across the async database wait.

`burn_durably(&InternalActionStore)` consumes that capability and grants a
`NonceBurnedRequest` only after a successful new nonce commit. The store checks
freshness again against its independent, persisted DB-clock high-water mark.
A replay, DB failure, incompatible skew or excessive DB rollback cannot grant
authorization. There is no volatile fallback. Do not restore a process-clock
guard from the DB-clock mark: they are different clock domains.

Only the burned capability exposes post-replay `authorize`: per-key bucket,
unique-key JSON parsing and action/feature checks, in that order. Cancellation
before commit never authorizes; cancellation after commit may burn an attempt
without executing it. The existing in-memory `authorize` uses the same stages
with its cache burn, preserving its ClockGuard and TTL-coverage checks.

This is still the legacy v1 signing contract. The [v2 specification](internal-action-signing-v2.md)
is not an implemented verifier. No unsigned caller/audience header is trusted.
Stable caller mapping, idempotency claim, action validation, outbound admission,
execution and durable finalization remain receiver responsibilities; this
capability does not itself permit a Discord effect.

## Voice configuration contract

Direct-container support only: `TWO_INTERNAL_ALLOW_VOICE_CONFIG=1` enables both
verbs, independently of `TWO_INTERNAL_ALLOW_SETTINGS`. The shipped Worker does
not forward this capability. Shipping this code does not enable a route or grant
signing/secret authority. No staging apply-check or production enablement is
implied by unit/database results.

- `voice.config.get` is keyless. It returns
  `{ok:true,result:{config,logging_settings,updated_by,updated_at},version,request_id}`.
  `config` is the existing version-1 `VoiceConfiguration` export document,
  including creators, channel templates, aliases, random lists and guild
  settings. The envelope `version` is a separate, opaque signed integer
  concurrency token; a never-configured guild has token `0`.
- `voice.config.set` requires `Idempotency-Key`, the complete `config` document,
  and `updated_by` (the dashboard actor's decimal Discord ID). Optional
  `expected_version` compares against the last committed guild change. A stale
  token yields non-retryable `409 version_conflict`, with no configuration or
  voice-audit write. Omission is a blind write. Unknown request/document fields,
  wrong-guild documents, malformed IDs, invalid templates and unsupported new
  time zones are refused. The guild is server-selected, never body-selected.
- A successful save returns `{outcome:"saved"}` under `result` and the committed
  token beside it. Replays return that first committed token even if another
  save landed later; they never reapply, bump the revision or append an audit
  row. A database/receipt failure with uncertain outcome requires reconciliation,
  never a new intent key.
- `config.settings.creation_enabled`, `command_role_id` and `command_roles`
  read/write the same access controls used by `/access` and the gateway.
  Creator fields read/write the rows used by `/textchannels`, `/position`,
  `/group`, `/inheritpermissions`, `/defaultlimit` and `/alwaysprivate`.
- Runtime `/logging` uses `off`/`brief`/`full`, unlike the import document's
  separate `logging` section (`errors`/`lifecycle`/`verbose`). It is therefore
  exposed separately as `result.logging_settings` with `level`, nullable
  `channel_id` and nullable `mention_role_id` (IDs are decimal strings). Supply
  the same object as optional top-level `logging_settings` on set to update it
  atomically. Omission preserves runtime logging; it never silently translates
  between the two different logging models.

Dashboard callers must authenticate the admin session, enforce its guild
allowlist, verify live Discord Manage Server authority and validate referenced
resources in that guild before signing, as for other settings actions. The bot
validates configuration values/reference syntax and fixes the target guild; it
**does not** treat `updated_by` as proof of a Discord permission or manufacture a
trusted inventory from submitted IDs. Signing keys remain trusted control-plane
capabilities. Do not enable a dashboard client without those caller-side checks.

Apply migration `0469_voice_settings_versions.sql` through the authorized
migration executor **before** deploying this binary: existing Discord
configuration writers also use the new revision/audit transaction. It adds
`version`/`updated_by`/`updated_at` to voice configuration tables, an append-only
`voice_settings_audit`, and the exact first-save revision on durable internal
receipts. Pre-migration rows retain unknown attribution and version zero; no
historical actor is guessed. Discord writes remain write-through, record a
`discord:<command>` actor, and invalidate any older dashboard token. Each
successful store operation appends one voice audit row; invalid/stale writes and
idempotent replays append none. Configuration replacement does not alter live
rooms or text companions.

Rollback: disable the voice capability and deploy the previous binary through
the existing deployment contract. Keep the additive schema and audit evidence;
do not delete audit rows or rewrite migration history. Staging deployment,
capability forwarding and live Discord apply-checks are separate execution
steps with their existing gates, not authorized by this reference.

## Automation import contract

`automations.import` accepts the version-1 command document produced by
`automations.export`; the existing MEE6 translation path is unchanged. Parsing
preserves command document order and duplicate names. Malformed wire entries
contribute to `skipped`; parsed definitions still undergo normal name,
description, template and trigger validation.

Planning simulates current content and case-insensitive trigger ownership in
document order. Each accepted occurrence sees the results of earlier accepted
occurrences, including those with the same name. Changed content requires
`overwrite: true`, even for a name first created earlier in this import;
identical content converges without overwrite. An accepted update releases its
previous trigger, including when it becomes slash-only. The next accepted
claimant owns a released trigger. Refused occurrences never change simulated
content or acquire/release triggers. Validation refusals are reported first,
followed by overwrite/trigger refusals in document order.

The transactional service applies only the exact parsed-entry indices accepted
by that plan, in document order, while holding the guild capacity lock. The
capacity precheck still counts the union of existing names and validated
candidate names and refuses the whole import before command writes if it is
over budget. Accepted same-content occurrences are rewritten and count as
`imported`; each rejected occurrence and malformed wire entry counts as
`skipped`. `conflicts` retains translation collisions followed by refused
overwrite/trigger names. Fresh rows are enabled; rewrites preserve the current
enabled flag and original creator/timestamp.

Each accepted occurrence has one audit ID
`<import-audit-id>#cmd:<name>#<ordinal>`, with a zero-based per-name ordinal among
all parsed occurrences, including rejected ones. The command writes, occurrence
audits and `<import-audit-id>#summary` commit together. The receiver replays the
committed summary's original counts and conflict names without replanning or
rewriting commands after later admin edits.

Authentication, durable nonce burn, action claims and feature gates are
unchanged. Overwrite still requires the separately configured
`TWO_INTERNAL_ALLOW_AUTOMATIONS_OVERWRITE` capability in addition to the base
automations flag. These import semantics do not enable ingress, publish
commands, grant capabilities or authorize a rollout. Parser/planner coverage
lives in `automation_import_occurrence_planner_acceptance`; the corresponding
`automation_import_occurrence_service_acceptance` suite checks transactional
rows, provenance, audit facts and replay readback on disposable databases only.

## Configuration contract

This is the direct-container parser contract, not the Worker secret allowlist.
For the shipped deployment use [Container wiring](#container-wiring); a parser
accepting an input does not mean the Worker forwards it.

`two_bot_core::internal_action_config::InternalActionConfig::from_env` returns
`None` when `TWO_INTERNAL_ACTIONS` is absent or exactly `0`. In that case it
never reads receiver keys or bind settings. Exactly `1` requests a receiver;
other values, including an empty or non-Unicode flag, are errors.

An enabled receiver requires all of these settings, without defaults:

| Setting | Meaning |
| --- | --- |
| `TWO_INTERNAL_BIND` | Literal IP plus explicit nonzero port. IPv6 uses brackets. No hostname, URL, public address or ephemeral port. A specific private IP always passes; the wildcard (`0.0.0.0`, `::`) passes only with `TWO_INTERNAL_CONTAINER` exactly `1`. |
| `TWO_INTERNAL_CONTAINER` | Required only with a wildcard bind: exactly `1`, Worker-set. Ignored for a specific private bind. |
| `TWO_INTERNAL_KEYS` | Existing comma-separated `key-id:secret` signing specification; each secret is at least 32 bytes. At most 64 keys, with unique IDs and distinct secrets. |
| `TWO_INTERNAL_CALLERS` | Comma-separated `key-id:caller` mappings. Exactly one entry for each signing key, no unknown entries. The caller is a stable logical identity, not a key-rotation version. |
| `TWO_INTERNAL_CHANNEL_KEYS` | Explicit nonempty comma-separated channel-key/Discord-ID map. Names are unique; IDs are canonical, nonzero, u64-representable snowflakes. |
| `TWO_INTERNAL_ROLE_KEYS` | Optional comma-separated role-key/Discord-ID map, empty by default. Names are unique; IDs are canonical, nonzero, u64-representable snowflakes. An empty map refuses every `role.assign` key; a malformed entry is a boot error. |

Key IDs, caller names, channel-key names and role-key names are 1–128 ASCII alphanumeric,
period, underscore or hyphen characters. Multiple rotating keys may identify the
same logical caller, but every key ID must have a distinct signing secret, even
for that same caller. The shared parser refuses secret aliases: the key ID
itself is not authenticated by the legacy signature format.

Runtime loads this separately from the gateway's health-only configuration
fallback. Invalid enabled configuration, missing gateway credentials/database,
a non-staging guild or private bind failure is fatal, not permission to continue
with a different bind or missing authentication. The authoritative runtime pool
backs the receiver; no volatile nonce or idempotency fallback exists. The private
listener binds before gateway/job task startup and shares their sticky shutdown
and bounded HTTP drain. Unexpected listener exit stops its sibling and jobs.
Binding privately does not approve ingress or replace TLS/network access controls.

Resolve `caller_for(key_id)` only after verifying the signature. Pass the stable
caller to `RequestIdentity::new`, together with the client's original idempotency
key and the exact authenticated bytes. Do not use the signing-key ID as that
identity: rotation must not grant a second execution of an existing intent.
Changing a caller mapping or target-channel configuration is an operational
identity/policy change, not an automatic migration of stored requests.

All configuration errors use fixed messages and setting names. Parser failures
are not forwarded because those errors can quote malformed input. Debug prints
only the bind address and counts, never keys, secrets or caller mappings.

## HTTP boundary and execution ownership

The following bounds belong to the Rust listener, independently of the Worker's
[sequential intake/forwarding budgets](#intake-forwarding-and-correlation).
Only exact `POST /internal/actions` is accepted; query strings and alternate
paths are refused. The boundary requires singleton v1 signing headers and
`application/json` (optionally `charset=utf-8`), rejects content encoding, and
caps header count/bytes (64/8192), body bytes (2 MiB), body collection (5 seconds),
active requests (32) and total request duration (20 seconds). These application
bounds are **not** transport connection/header-read limits; approved ingress
must supply those controls. No generic request tracing middleware is attached.
Rejections use bounded closed scalar telemetry with periodic/shutdown flushes,
never raw paths, bodies, signatures, secrets or unknown caller labels.

After authentication, durable nonce burn and per-key admission, the receiver
validates the announcement or the event mutation (`event_key`, and the full
event input plus staging-guild key resolution for upserts) and the original
`Idempotency-Key`, then atomically claims its intent and audit record. Only a
committed `Claimed` result permits an effect. Replay returns the stored
terminal response; mismatched payloads, in-flight or reconciliation-required
intents never send. The stable caller mapping preserves this ownership across
distinct-key rotation and restart. Upserts register their key only after
Discord confirms; cancels retain the mapping.

Runtime constructs the single-attempt announcement adapter with
`PgSendAdmission` over the same database/token authority as other governed
outbound paths, plus its local `CooldownGovernor`. Core inbound buckets and a
shared Twilight client are not substitutes for that durable gate. A Discord
429 installs the adapter's cooldown and stores a terminal refusal, never an
automatic resend. Unknown transport outcomes, cancellation or failed receipt
finalization retain durable ownership and require reconciliation. Success is
returned only after the receipt/audit transaction commits. See
[the executor contract](internal-action-executor.md#single-attempt-and-safe-results).

A local admission refusal (the guard, or a token lane held by another send)
proves nothing was sent, so the event claim is released and the same key may
retry, unless another intent has mapped the key in the meantime; that retry is
refused as a version conflict and needs a new key. Event reads and mutations share
one process-wide gate, so a mutation that has registered its key is visible to the
next read or mutation for that key. A request that cannot enter the gate within 8
seconds is refused `in_progress` before any claim. A claimed mutation runs to its
receipt in its own receiver-tracked task, even if the client has timed out, and
carries its admission permit with it: capacity stays held through receipt
persistence, so repeated timed-out clients cannot admit more than the 32-slot
work limit. A create whose Discord
call times out, gets a 5xx or an unreadable reply, or whose key mapping or mirror
write fails, can leave a Discord event with no key mapping; a create under a new
Idempotency-Key for that key would then make a second event. The same can happen
if the process stops mid-create. Callers must not re-submit a
`needs_reconciliation` operation under a new key: reconcile the Discord event by
hand first. An in-flight intent whose owner has stopped answers `in_progress` to its own key
until its claim goes stale, then `needs_reconciliation`; a live owner can still
finish after that. A panic after a claim answers `needs_reconciliation`. A 504 on a
mutation is an unknown outcome that may still complete: retry the same key only.
Shutdown latches drain mode — and a single shutdown deadline inside the shared
5-second HTTP drain budget — at the request: the Axum drain and the child join
share that deadline's remaining time, so an admitted handler holding a
connection cannot starve the child drain past the supervisor's backstop.
Shutdown stops new mutation effects after the claim (replay, in-flight and
reconciliation answers keep working) and releases the fresh provably-never-sent
claim so the same key may retry, then joins tracked children within the
deadline's remainder: clean completion is reported only after every child is
reaped, and work still in flight at the deadline is aborted and reported as a
drain timeout, never waited on past the budget. Aborted or failed children keep
their claim fence occupied, so a retry sees `in_progress`/`needs_reconciliation`
and never a second effect.
A claimed mutation holds the gate until it finishes, so a slow database can keep
queued requests waiting for their 8 seconds. The events poller writes the mirror
outside the gate, but both writers stamp `updated_at` with the same UTC-millis
clock and only overwrite rows at or below their own `observed_at`: a snapshot
taken before a mutation leaves that mutation's newer mirror row in place, and a
stale mutation write cannot overwrite a newer snapshot row; key mappings and
receipts are unaffected. Operators reconcile unknown event intents with the
inspection-first, explicitly confirmed `two-bot reconcile-event` CLI: list the
fenced intents, verify the Discord event by hand, then resolve the exact intent
as created/updated/cancelled (healing the key mapping for upserts) or no-effect.

The website-compatible envelopes contain `ok`, `request_id`, and either
`result.message_id` (announcements), `result.{outcome,event_id}` (event
upsert/cancel, with `outcome` replayed from the stored receipt, never
re-derived), `result.outcome` (membership: `assigned`/`already_held`,
`added`/`already_member`) or `error.{code,message,retryable}`. Durable replay
adds
`Idempotent-Replay: true`; inbound bucket refusal carries `Retry-After`.
Messages are fixed/redacted and all envelopes use `Cache-Control: no-store`.

## Relayed answer contract

The Worker relays only envelopes the receiver's constructors emit
(`crates/core/src/internal_actions.rs`, `crates/bot/src/internal_action_http.rs`).
Every relayed body carries the forwarded `request_id` (generated shape, echoed
by the receiver); a missing, misshapen or mismatched id fails closed. Extra
fields and the receiver's message text never leave: callers see the shapes
below and nothing else.

These are receiver wire shapes, not a claim that every family is enabled through
the shipped Worker. [Container wiring](#container-wiring) determines which gates
and role targets can reach it.

Success (`200` only, `result` always an object, no `error` key):

| Family | `result` | Notes |
| --- | --- | --- |
| announcement.post | `{message_id}` | |
| event.upsert/cancel | `{outcome, event_id}` | `outcome` replayed from the stored receipt |
| event.read | full observed event | behind its allowlist flag |
| settings.get/set | `{key, ...}` plus top-level integer `version` | `version` beside `result`, never inside |
| voice.config.get | `{config, logging_settings, updated_by, updated_at}` plus top-level integer `version` | Receiver-only; opaque CAS token beside `result`, not a forwarded capability |
| voice.config.set | `{outcome:"saved"}` plus top-level integer `version` | Receiver-only; first committed token retained on replay, capability not forwarded |
| automations.export/import | command document / `{imported, skipped, conflicts}` | |
| role.assign / guild.add_member | `{outcome}` | `assigned`/`already_held`, `added`/`already_member` |
| moderation.\* | `{outcome[, affected]}` | |

Refusals keep their code, status and retry policy with the fixed message
`request refused`:

| Code | Status | Retryable |
| --- | --- | --- |
| malformed | 400 | no |
| unauthorized | 401 | no |
| stale_request | 401 | no |
| action_not_allowed | 403 | no |
| not_found | 404 | no |
| method_not_allowed | 405 | no |
| request_timeout | 408 | yes |
| replayed | 409 | no |
| version_conflict | 409 | no |
| in_progress | 409 | yes |
| needs_reconciliation | 409 | no |
| discord_rejected | 422 | no |
| rate_limited | 429 | yes |
| internal | 500 | yes |
| discord_unavailable | 502 | yes |
| no_effect | 502 | no |
| busy | 503 | yes |
| upstream_timeout | 504 | yes |

Compact next-action map (the `retryable` flag is the machine rule; the map is
the human gloss):

| Situation | Next action |
| --- | --- |
| `retryable: true` (408, 409 `in_progress`, 429, 500, 502 `discord_unavailable`, 503 `busy`, 504) | Retry the same `Idempotency-Key`; never mint a new key for an unknown outcome. Honor `Retry-After` when present. |
| `needs_reconciliation` (409) | Reconcile by hand first (see the event rules above); never re-submit under a new key. |
| `replayed` / `version_conflict` (409) | Same key already decided or superseded: read the stored outcome, use a new key only for a new intent. |
| 401 / 403 | Fix the caller: signing key, timestamp/nonce freshness, action allowlist. Do not retry the bytes. |
| `malformed` (400), `not_found` (404), `method_not_allowed` (405), `discord_rejected` (422), `no_effect` (502) | Fix the request: shape, route, verb, payload or target state. |
| `unavailable` (503, Worker-only) | Inspect container readiness, ownership and correlated logs; transport failure, oversize, deadline-aborted response collection or an unusable receiver answer is not a receiver verdict or proof of no effect. Retry the same intent key only, with fresh signing headers. A dark route is absent (404), not this envelope. |

## Tests

`wrangler/test/receiver-docs.test.ts` checks this source of truth offline against
`BODY_TIMEOUT_MS`/`REQUEST_TIMEOUT_MS`, synthetic `handleActionsIngress` outputs,
validated/minted headers, actual refusal log fields and the real DO's startup
environment with excluded inputs present. Negative fixtures restore the stale
deadline, header, log and role-key claims and must fail conformance. The
`WORKER_DOCS` selector registers this input; `npm test` discovers the check.
From `wrangler/`:

```sh
node --import ./test/cloudflare-loader.mjs --test test/receiver-docs.test.ts test/internal-actions.test.ts test/container-env.test.ts test/span-correlation.test.ts
npm run typecheck
npm test
```

These commands use synthetic fixtures only. Do not execute enablement or
rollback examples as documentation validation; this correction authorizes no
secret staging, ingress change, deployment or capability widening.

The configuration unit tests use only the existing public signing vectors. They
exercise dark defaults, strict enable values, missing/non-Unicode settings,
private literal binds, wildcard binds only with the container marker,
distinct-key rotation with caller continuity,
duplicate/ambiguous mappings, same-secret aliases for the same or different
callers, canonical channels and redaction.
They do not read runtime credentials, open sockets, access databases or send to
Discord.

Receiver tests use a module-private injected effect and guarded, migrated
`TestDatabase` instances. They cover protocol caps and body deadlines, request
capacity, redacted authentication failures, nonce-before-parse ordering,
concurrent/restarted/key-rotated replay, byte mismatch, unsupported actions,
membership happy-path/replay/refusal (allowlisted role assignment, OAuth-backed
joins, redacted unknown-key/malformed refusals, dark add-member flag),
cancellation/stale ownership, unknown/no-effect outcomes, unavailable stores,
failed receipt finalization, event verb flags, malformed event bodies (including a
101-character location), concurrent creates for one key, a gate-wait refusal before
any claim for a mutation and for a read, a mutation completing its receipt after its
client is dropped, a mirror failure reported by the mutation path, admission-refusal
release for creates and cancels, and listener supervision. Nonces are generated
fresh for each attempt; the nonce-replay test intentionally reuses one generated
value. They do not send live Discord actions.

Combined receiver/real-adapter acceptance uses an ephemeral loopback HTTP double
and independent migrated test pools. It covers successful durable receipts,
rotation/restart replay, single-attempt 429 refusal, token-wide holds blocking
new intents and other transports, and invalid receipts retaining both intent and
send-lane ownership. The Discord crate's `test-support` origin seam is enabled
only as a bot dev-dependency: it refuses non-loopback origins and executors without
admission. Runtime construction has no origin override. Added test source is not
evidence of execution; record exact-head compiler/test results separately.

On the persistent controller, run compiling checks only through the approved
bounded-cache wrapper, for example:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib internal_action_config
```

If cache admission is refused, retain that result and use the existing hosted CI
path; do not substitute a target directory or invoke Cargo compilation directly.
