# Internal scheduled-event executors

`ActionExecutor::execute_event` handles the Discord/mirror portion of
`event.upsert`, `event.cancel` and `event.read`, independently of the HTTP
receiver. It uses Twilight routes and the existing single-attempt transport.
There is no gateway, live Discord test, timer, or RSVP change in this slice.

## Receiver boundary

The receiver must authenticate and validate requests, enforce action flags,
acquire the durable mutation claim, and resolve `event_key` in its configured
guild before constructing `EventCall`. `Upsert { event_id: None, .. }` creates;
a trusted mapped ID updates. Cancel/read take the trusted mapped ID, never a
raw website `event_id`. The receiver retains/records the key mapping and owns
terminal result replay; these executors neither add an HTTP route nor implement
that mapping/replay policy. A replay must not invoke the executor again.

Pass a stamp callback rendering UTC milliseconds (`now_iso`). After each valid
Discord response the executor calls it once, and that reading serves both the
mirror row and the read response, then awaits `ScheduledEventMirror::upsert`
before acknowledging. Stamping after the return (never a pre-send reading) keeps
last-observed-wins honest: a slow PATCH wins over a snapshot whose GET was
served from the pre-mutation row, while the poller stamps before its GET so a
late-arriving fetch cannot clobber a newer mutation. The `PgPool`
implementation writes one `(guild_id, event_id)` row via
`website_store::upsert_event`; it never runs the poller's full-guild swap.
Null channel/description values replace older values. Unrelated rows survive.

## Legacy parity and strict mirror handling

Source comparison uses legacy two-bot revision
`e099be0382d640b6b714f3a397ef1421bb03615a`:

- `src/internal/discordActions.ts:86–102`: privacy level 2, voice entity 2 or
  external entity 3, omitted description stays omitted, external clears the
  channel and supplies location metadata.
- `src/internal/actions.ts:385–428`: created/updated/cancelled results contain
  only `outcome` and `event_id`. Cancellation is PATCH `{ "status": 4 }`, not
  DELETE; mapping is retained.
- `src/internal/actions.ts:446–473` and `discordActions.ts:243–273`: read returns
  exactly `outcome`, `event_id`, `name`, `starts_at`, `location`, `status`, and
  `observed_at`, without creator, attendees, end time, or channel ID. Lifecycle
  status uses `SCHEDULED`, `ACTIVE`, `COMPLETED`, `CANCELED`; the DB uses
  lowercase `cancelled`. The read result retains the upstream start string;
  the DB mirror normalizes it to UTC milliseconds.
- `src/internal/actions.ts:510–518`: `requireTimestamp` sends Discord
  `new Date(ms).toISOString()` (UTC, `.mmmZ`), so the validated
  `starts_at`/`ends_at` use that form, not the raw RFC 3339 input.
- `test/e2e.internalactions.test.ts:252–260,1159–1248,1333–1415` provides the
  Launch Night request vector, mutation/replay and narrow read contracts.
  `test/unit.internalallowlist.test.ts:536–564` pins the placement body shape;
  `test/unit.eventcancel.test.ts:21–29` pins error classification.

Fresh-key cancellation of an already-cancelled event is still a Discord
rejection (typically 400). A 404 is also `discord_rejected`, not a fabricated
successful cancellation. Same-key idempotency is durable replay of the first
result. No 429, 5xx or uncertain mutation is automatically retried here.

Status classification shares one helper with the announcement transport:
`two_bot_discord::executor::is_definitive_rejection` (400/401/403/404/405 plus
413/415/422). 404 proves absence, 413/415/422 prove Discord validated before
mutating, so all four are terminal `discord_rejected` on every event call
(create/update/cancel/read), release the send-admission lane on receipt, and
never retain the execution fence. 408, redirects, 409, 425 and 5xx stay
uncertain (`needs_reconciliation` on the mutation path).

An upsert whose mapped event was deleted in Discord fails the same way: its PATCH
404 is `discord_rejected`, and the mapping is kept. That key needs an operator to
clear the mapping before a new create. Classifying that 404 as absence is a
follow-up.

Synchronous persistence adds stricter response checks than legacy mutations,
which ignored PATCH bodies: require a complete normalizable row, known status,
parseable start time, and canonical decimal nonzero `u64` event and guild IDs.
The guild ID must be present and match the configured guild; the event ID must
match the requested ID when known. Non-null channel IDs must obey the same
canonical rule; missing/null channels remain valid. The poller and this writer
share `internal_events::canonical_snowflake` before normalization or storage.
A cancellation response must actually be cancelled. Unknown numeric status
names remain supported by `event_status_name`, but cannot be persisted as a
valid mirror row. Unreadable responses leave the prior row intact and do not
complete a mutation's retained admission permit.

`EventActionError::is_safe_pre_mutation()` marks a proven Discord or local refusal.
The receiver releases a claim only for the send guard and admission-blocked refusals
(`is_admission_blocked()`) and records a Discord rejection as terminal. Unreadable
success and mirror failure are **not** release signals: they keep the execution
fence and require reconciliation, never a second event to recover a missing
acknowledgement. On the read path, mirror failures map to `internal` and unreadable
rows to `discord_unavailable`; on the mutation path both become `needs_reconciliation`.

## Verification

Golden request JSON: `crates/discord/tests/fixtures/internal_event_bodies.json`.
The scripted REST tests assert create/update/cancel/read wires and results,
four status names, already-cancelled/404/error classification, malformed or
mismatched responses, synchronous acknowledgement, and write failure behavior.

Controller compilation must use the bounded cache wrapper:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --test internal_events
python3 scripts/cargo_cache.py run -- clippy -p two-bot-discord --all-targets --all-features -- -D warnings
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/agent_test \
  python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db \
  --test internal_events -- --ignored
```

The opt-in DB test validates effective connection options **before** connecting,
pins the empty test password, rejects URL/socket/startup overrides and all
non-test targets, owns a unique schema, applies the real website-table migration,
and asserts mirror rows after each action and failed requests. It preserves
same-guild unrelated events and another guild's same-ID row. CI explicitly runs
it on its ephemeral `agent-testdb` service; production/staging are forbidden.

A missing/refused controller pool is not permission to build directly or use an
external target. Hosted CI keeps its existing compiling commands on ephemeral
runners, as required by the build-cache runbook.

## Event write cutoff and rollback

Keep Web event dispatch closed unless Bot Operations has qualified the current
and every retained runtime against a verified independent control. This
procedure describes a fallback for holding event writes closed. It carries
no authority to change guild permissions or enable the receiver.

### Enforcement

The rollback-independent fallback is the Discord guild permission check at
mutation time. The guild administrator owns the policy. Bot Operations is
accountable for verifying the policy against the admitted runtime and rollback
identities and recording the policy revision. Worker ownership and an ingress
flag can block new work while leaving a sender with a committed claim on its
way to Discord. Both the runtime and its retained rollback use Discord's API
directly. A Worker outbound handler added to a newer revision would leave the
retained Worker without that handler.

| Signed action | Discord request |
| --- | --- |
| `event.upsert` create | `POST /guilds/{guild}/scheduled-events` |
| `event.upsert` edit | `PATCH /guilds/{guild}/scheduled-events/{event}` |
| `event.cancel` | The same `PATCH` with status 4 |
| Alternate deletion | `DELETE /guilds/{guild}/scheduled-events/{event}` |
| `role.assign` write | `PUT /guilds/{guild}/members/{member}/roles/{role}` |
| `announcement.post` write | `POST /channels/{channel}/messages` |

For each current and rollback credential, identify its Discord bot member and
every guild and destination the affected routes can use. Remove effective
`CREATE_EVENTS` and `MANAGE_EVENTS` at guild level for external events and on
every eligible voice and stage channel for channel events. Check the bot's
membership, all held roles including `@everyone`, channel overwrites including
member-specific overrides, and guild ownership or `ADMINISTRATOR` bypass.
Existing events the bot created can still be edited or deleted with
`CREATE_EVENTS` alone. Verify that the bot cannot regain either permission
through a role it can assign, through an overwrite it can change, or through
an alternate credential. A role assignment must still have `MANAGE_ROLES` and
an eligible role below the bot's highest role. Announcement channels must
still allow the bot to view and send there, including any content permissions
the configured messages require. Verify those positive controls separately for
each identity. The scope is all scheduled-event writes by the affected identity
in the covered guilds, including native bot writers. [Discord event permissions](https://docs.discord.com/developers/resources/guild-scheduled-event#guild-scheduled-event-permissions-requirements) · [Discord guild permissions](https://docs.discord.com/developers/topics/permissions#permission-overwrites)

### Activation and recovery

1. Close the Web dispatcher, pin current and retained Worker/OCI identities,
   and identify the distinct Discord identities they use.
2. In a separately authorized operation, the guild administrator applies the
   event-denied policy. Record the guild and credential identities, protected
   change or audit references, and an authenticated readback of effective roles,
   hierarchy and overwrites. A missing, stale or incomplete readback keeps
   dispatch closed. Readback identifies policy state and scope. It supplies
   no per-attempt acknowledgement.
3. Keep Discord permissions in place throughout process restart, database
   restore, old/new overlap and Worker rollback. A Bot or database rollback
   cannot restore Discord permissions. If a separate guild configuration restore
   can change those permissions, require the effective policy to be read back
   and held before that restore can be treated as covered.
4. Reopening event permission is a separate administrator change, with Web
   held closed. The event-denied fallback has no generation or per-attempt
   permit that could authorize dispatch. An open policy by itself provides no
   dispatch contract.

A request evaluated by Discord before the policy change may already have
affected an event. A permission rejection only establishes refusal of that
specific Discord request. The current Bot relay returns a generic
`discord_rejected` and gives Web no authenticated cutoff receipt linked to
its original idempotency key, action and payload. Web must preserve the
original key and keep ambiguous work held. An authenticated durable
non-admission receipt would need to bind the original caller and key, action
and payload, and attest no wire attempt was admitted on any covered path.
An authenticated outcome proven against the original attempt may settle that
attempt. A lost response, timeout, stale owner, revoked permit, permission
readback, or elapsed time cannot settle it. Never dispatch under a second key.

### Bot permission readback

The container mounts `POST /ops/event-permissions` only with
`TWO_EVENT_PERMISSION_READBACK=1`, `GUILD_ID`, a canonical
`TWO_EVENT_PERMISSION_READBACK_BOT_ID` and a dedicated `id:secret` in
`TWO_EVENT_PERMISSION_READBACK_KEY`. Access stays inside the container and
works while signed action ingress is off. Send JSON with the string fields
`guild_id` and `bot_id` and use the existing four `x-two-*` headers to sign
`POST\n/ops/event-permissions\n{timestamp}\n{nonce}\n{sha256_hex(body)}`.
The process-local nonce cache refuses replay within the signature window.
The redacted response contains the signed nonce, sample time, owner or
administrator bypass, guild event bits and voice and stage counts for each
bit and their union after overwrites. The read uses the connected gateway
cache without I/O. Gateway startup still requires its normal database.
Refusals carry no result (503 unavailable, 404 wrong scope or disabled,
401 unauthenticated). The operator ties the nonce to the pinned runtime
and private scope. The time dates the local sample. Qualification needs
separate evidence of the applied policy revision, channel prerequisites,
permission recovery paths, other writers, and role and announcement controls.

### Disposable rehearsal

Pull the admitted Bot images by digest through the registry's read-only path
and pre-pull the pinned Postgres and recorder images below. The script launches
only locally present images. It starts a fresh
loopback-only network namespace with no external interface, supplies synthetic
credentials, and routes the original binary's TLS requests for `discord.com`
to the local recorder. It uses the shipping receiver's compile-time guild
fence as an inert fixture identifier. It does not use a live guild or the
Worker ingress.

The runner reads the schema from the current image's
`org.opencontainers.image.revision` commit. If that revision is missing from
the checkout, fetch it from `origin` before starting.

```sh
docker pull postgres@sha256:372621a978f4513b082d946d6320e2b11a3b359648b81fba8a2ba0df33958f62 &&
docker pull node@sha256:e7add2a490bb17d97b485b8641bd88455fff7839ade0f5eba493c3508124d413 &&
current_revision="$(docker image inspect --format '{{ index .Config.Labels "org.opencontainers.image.revision" }}' "$current_digest_reference")" &&
(git cat-file -e "${current_revision}^{commit}" || git fetch origin "$current_revision") &&
git cat-file -e "$current_revision:sql/web_v1.sql" &&
PYTHONDONTWRITEBYTECODE=1 python3 tests/event_write_cutoff/rehearse.py \
  --current "$current_digest_reference" --retained "$retained_digest_reference"
```

The recorder models a guild-wide grant or denial for external events. Its
policy file lives outside the app database. Real role hierarchy and channel
overwrites require a separate effective-permission readback. The script records
policy order, intent and token admission at each barrier, per-route counts,
immutable Bot image references and the recorder source hash. It checks that an
original key remains ambiguous after restoring a snapshot containing the claim.
It also forces a synthetic request under that same key after restoring a
snapshot from before the claim, to verify that the cutoff still denies the
request. Web must keep ambiguous work held in both cases.

The runner uses the current artifact's schema for both positions. The same
image may back both positions when the serving and retained receipts agree.
The full verdict requires both positive controls. A retained announcement
transport using compiled WebPKI roots rejects the synthetic CA before the
request reaches the recorder. Its announcement control remains incomplete
until both admitted images can use the disposable trust root at the same
Discord origin.

The announcement control invokes `rehearse-internal-actions`. For source proof, pass the commit-labeled release image ID to `--candidate`.

### Assemble evidence by release position

Use the reviewed serving and rollback records as independent pins for `current`
and `retained`. Keep the evidence private. Compare each position as follows.

| Evidence | Compare with the reviewed position |
| --- | --- |
| Captured rehearsal stdout and exit status | Run [the existing verifier](../tests/event_write_cutoff/verify_release_positions.py) with the selected `image` and `source_revision` for both positions. It verifies the full digest-pinned image reference and source revision for each position. |
| Successful v1 permission response | Match `scope_nonce` exactly to the nonce on the signed request in the private attribution record for that position. Use a distinct nonce for each position, even when the same image serves both. Compare the request's `guild_id` and `bot_id` with the pinned scope. |
| Private request attribution | Use the record of the runtime that answered that request. Compare its position, OCI digest (the `sha256:…` part of the selected `image`) and full source revision with the reviewed pins. Copying intended pins into the request note leaves the responding runtime unattributed. |
| Separate Worker identity evidence | Compare the Worker version associated with that responding runtime and position in the deployment evidence with the reviewed Worker version. The rehearsal runs without the Worker, and the container-local permission response carries only the nonce, observation time and permission projection. |

A matching nonce joins the response to the request. A matching observation time,
Git revision or permission count cannot supply a missing OCI digest, Worker
version or scope. Record **attribution gap** for a missing or ambiguous link and
**attribution mismatch** for a conflicting value. The permission response in
either case cannot serve as proof for that position. The observation time dates
the local read. The applied Discord policy revision and the remaining controls
in [Bot permission readback](#bot-permission-readback) still need their own evidence.

Replay the synthetic pair and the wrong-build and attribution-gap cases from
the repository root. The fixture uses the existing rehearsal verifier and
in-memory examples of the private evidence. It makes no live requests.

```sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/ci/test_event_cutoff_permission_attribution.py -v
```
