# LFG domain and store port

This is the self-contained domain/store portion of TOG-10084. The legacy behavior reference is `TogetherWeOwn/two-bot` at parity revision `d5d11793`, specifically `src/announcements/{service,store,discord}.ts` and migration `0024_announcements_feeds.sql`.

## Delivered seams

- `two_bot_core::lfg`: role parsing, title/start validation, pure signup adjudication, select actions/options, rendering, permission checks, reply text, and stable post nonce.
- `two_bot_core::lfg_store` (`db` feature): guild-fenced posts, ordered roles/signups, capacity-serialized signup/move, idempotent leave/close, and failed-post cleanup.
- `crates/cutover/migrations/0170_lfg.sql`: additive legacy-named `lfg_posts`, `lfg_roles`, and `lfg_signups` tables.

Same-role selection returns `joined` without changing the original signup timestamp. Moving into a full role preserves the existing signup. Close and signup share the same advisory-lock key so a close cannot race a capacity decision. A foreign-guild upsert is refused before role replacement. Closed posts keep existing signups but remove select options and refuse new signups; leave remains allowed as in legacy.

Role keys, duplicate detection, JavaScript numeric slot forms, and UTF-16 title/label limits follow legacy. Keys, labels, slots, and titles use ECMAScript WhiteSpace/LineTerminator trimming, including BOM (U+FEFF) and excluding NEL (U+0085); interior characters are preserved. Message truncation keeps the legacy UTF-16 cap without emitting half a surrogate pair. Start-time input deliberately accepts RFC-3339 ISO-8601 timestamps only; it does not reproduce `Date.parse`'s undocumented natural-language/date-format acceptance.

## Shared interaction integration (TOG-10260)

The bot's `CommandRuntime` composes LFG, sticky and feed registrations in one `InteractionRouter`; it routes once and delegates `/lfg`, `/lfg-close`, `/lfg-edit`, `/lfg-list`, and `two:lfg:` string selects to `two_bot_discord::interactions::InteractionRuntime::handle_routed`. Configured-guild/default-off announcement fencing and builder/runtime `ManageEvents` checks remain in the shared registry/router. Signup and leave require no elevated permission. Create/signup/leave/close use a deferred ephemeral acknowledgement before SQL, advisory locks and paced REST; the shared `ActionExecutor` finishes the reply. Read-only `/lfg-list` instead uses a one-second lookup budget and one immediate ephemeral callback, without the mutation executor or history recovery. See the [current-channel discovery contract](lfg-interaction-contract.md#find-open-game-posts-in-this-channel) for eligibility, cursor and whole-row output bounds.

`LfgInteractions` persists the post and roles before sending through that executor. A per-post runtime advisory lock serializes mutations and remote refresh across instances, separately from the store's capacity/closure lock. Acceptance saves only `message_id`, without replacing roles, signups or closure. Signup, leave and close refresh mention-suppressed content/components; closed posts remove the select. ID-only leave is called only after a guild-fenced lookup. `/lfg-edit` uses a dedicated metadata-only update rather than `put_lfg`: title/time edits preserve role rows, capacities, signup identities and post/message IDs. Unchanged metadata produces no rewrite or refresh; saved edits include the canonical start time and disclose audit/refresh failures. See the [LFG input contract](lfg-interaction-contract.md#edit-an-existing-game-post).

Nonce recovery reads at most three paced 100-message history pages and matches the stable nonce, channel and authoritative bot author. Before a scanned row can support absence, its message and author IDs must be canonical nonzero decimal u64 strings (no leading zeros or plus sign) and its channel ID must match the target channel. Missing/malformed identity or author evidence, or missing/mismatched channel evidence, yields the fixed `unreadable message history` unavailable error without echoing upstream data, including on later pages. On a bot-authored row, a present nonce must be null, a string or a signed/unsigned integer; booleans, objects, arrays and fractional numbers yield the same uncertainty error. An omitted or null optional nonce, or a readable integer/string nonmatch, is ordinary history, not malformed evidence. A different valid author already establishes a nonmatch regardless of its optional nonce's shape. Definite pre-mutation rejection or valid exhausted history without acceptance removes the guild-fenced failed post. Denied, malformed or bounded-out history is uncertain, not proof of absence: the post and roles remain, `message_id` stays unset and the reply reports uncertainty; no replacement POST is attempted. Existing audit outcomes are unchanged. Discord's `enforce_nonce` window is recent minutes, not an unlimited durable idempotency guarantee. Sources: [Create Message](https://docs.discord.com/developers/resources/message#create-message-jsonform-params) and [String Select](https://docs.discord.com/developers/components/reference#string-select).

Legacy create/signup/leave/close outcomes use the available shared `rsvp_store::write_audit` announcements seam. Its pool-only insert is not atomic with the domain mutation; an audit failure surfaces as failure rather than a false success. Refresh failure explicitly says the saved mutation remains. Signup audits now use deterministic `lfg:signup:<interaction>` IDs: a retry that reaches the service under the per-post lock reuses the saved outcome instead of reapplying signup or appending another row, but may retry refreshing the existing post. Routed gateway redelivery still requires a valid initial acknowledgement; Discord rejects already-acknowledged interactions with error `40060` before service execution, so it is not a guaranteed refresh-recovery path. An audit failure before that row is saved still leaves the existing domain-store retry behavior; this is not an atomic interaction inbox.

Signup/move is admitted by a bounded five-second guild+actor window before the deferred callback, queue, SQL, audit or channel refresh. Refusals use one ephemeral initial reply with no domain writes or refresh. Leave and close are exempt; other actors/guilds and moderation retain independent admission. The service repeats the gate for direct callers, allowing the already-admitted event. See [automation actor admission](automation-admission.md) for capacity, expiry and restart semantics.

READY supplies the bot user and application ids. A resumed session carries neither, so LFG reuses the lookups the runtime already makes: the ticket runtime's bot-user read and the registry's application read. Gateway identity recovery itself issues no extra identity request. With tickets disabled, the first LFG create resolves the bot user once through the shared executor; signup, leave and close need no identity lookup. Until it is known, nonce recovery reports acceptance as uncertain and keeps durable state instead of matching a guessed author. Once the application id is known, foreign-application interactions are ignored. The Worker's allowlisted flag forwarding already passes the optional `TWO_ANNOUNCEMENTS` string through without enabling it by default. No deployed configuration is changed by this PR. Source: [current bot identity](https://docs.rs/twilight-http/0.17.1/twilight_http/client/struct.Client.html#method.current_user).

`CommandRuntime::dispatch` spawns interaction work off the shard loop, so SQL and paced REST never block Twilight's polling. Dispatch is bounded per lane and per member (see `docs/channel-moderation-runtime.md`). The member interaction `LfgInteractions` service admits `LFG_MAX_IN_FLIGHT` (1) execution before taking a pool connection; that execution holds at most two connections, its advisory-lock transaction plus one store call. Queued interactions hold none, so the 5-connection pool keeps room for the gateway checkpoint writer and other features. A compile-time assertion in the bot ties the cap to `DB_POOL_MAX`. At most `LFG_MAX_WAITING` (8) executions queue behind the running one; each queued select holds a slot of the shared interaction lane, so the next request is answered "LFG is busy right now" instead of queueing. The permit stays held across the paced refresh: it guards the advisory-lock transaction, which orders refreshes per post so a stale render cannot overwrite a newer one. This slice does **not** add a durable interaction inbox or guarantee replay of an interaction aborted during restart; already-persisted LFG state stays available for nonce recovery.

Publication uses the router's **whole** gate-filtered registry, never an LFG-only replacement. With automations disabled, custom rows are intentionally excluded by the router. With automations enabled, publication is deferred until TOG-10080 supplies an authoritative custom-command store/load: unavailable storage is not an empty custom set. Existing remote registrations are left untouched in that case. A failed lookup or publish is logged and stays eligible to retry on a later gateway connection event. Staging/production deployment and enabling announcements remain separately gated.

No private dispatcher, Discord client, or temporary voice channel feature is included here.

## Bounded lifecycle maintenance

The existing jobs supervisor runs `lfg_sweep` every 60 seconds, only for the
configured guild when announcements and the boot identity fence permit it.
It reuses the website jobs' lazy pool and governed executor, resolves the
current bot user once per registration, and uses the interaction runtime's
same database advisory lock per post. Its maintenance service admits one row
at a time before taking connections; it does not add a private scheduler.

Signups stop at the post's start time. A refused member is told to ask an
organizer for a new `/lfg` post. The store also checks `starts_at <= joined_at`
under its existing signup lock. The sweep closes posts at least 15 minutes
past start and removes their signup controls by editing the existing message.
Posts and their original signups are retained, never deleted by the sweep.
The member-visible footer reserves space for the ID needed by `/lfg-close`,
even when a full roster would otherwise consume the 2000-UTF-16-unit body.

An open row without a saved message ID becomes eligible after five minutes.
The same bounded, authoritative bot-author/nonce lookup recovers its message
without reposting. If history has no match or is unreadable, the sweep closes
the retained row to release capacity; unreadable history is still not evidence
that Discord never accepted the message. A recovered expired post is closed
and refreshed too. An unresolved bot identity refuses the pass without changing
rows. This maintenance lookup is independent of the gateway identity cache;
it may make one additional current-user request before any member uses LFG.

Each invocation selects at most `min(limit, 20)` open candidates in stable
creation/ID order; the production limit is 20. Zero budget does no SQL or REST.
A contended runtime lock is skipped for the next tick. Each row is rechecked
under that lock. Row-local database/REST failures log a constant warning and
continue; the processed count includes failed eligible attempts, not skipped
locks. Existing job success/failure counters use the dedicated `lfg_sweep`
label and the batch log carries `processed`. The supervisor retains its
120-second attempt timeout and shutdown cancellation; no work is detached.

Message refresh is best effort: a failed edit cannot undo a committed signup
or close, and member replies say the saved result still applies. A closed row
is no longer eligible, so a failed close refresh is not automatically retried
by this sweep. Stale visible controls remain harmless because signup checks
the saved state/start time. The open-post cap remains 20, the organizer
permission gate is unchanged, and saved select receipts remain authoritative
on replay. This change does not enable announcements or deploy anything.

## Verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- check -p two-bot
python3 scripts/cargo_cache.py run -- test -p two-bot --bin two-bot command_runtime
python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db lfg_store:: -- --ignored --test-threads=1
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db --test lfg_interactions -- --ignored --test-threads=1
```

The actual-router tests additionally cover lifecycle/audit outcomes, ordinary-member signup/leave, refusals, persist-before-POST ordering, ambiguous acceptance, failed-post cleanup, uncertain history preservation, recovery without role/signup/closure replacement and cross-guild targeting. They apply real migrations in a unique test schema and use only the fixed agent-testdb endpoint or CI service above. Discord REST is a scripted loopback double.

The ignored LFG test commands use only `agent-testdb:5432`, user `agent_test`, database `two_bot_test_tog10084`. It never reads `TWO_DATABASE_URL` or an application credential. In the GitHub job, `TWO_LFG_TESTDB_CI=1` selects the fixed loopback endpoint of the ephemeral PostgreSQL service, which shares the single `agent_test` database with the gateway suite. Tests stay isolated through unique post IDs plus per-test cleanup. The migration runner validates all embedded migration checksums; CI starts with a fresh database and applies the migration before exercising transactions.

The store tests cover round trips, guild fencing, leave, capacity, atomic moves, same-role idempotency, simultaneous joins, closure, and close/signup serialization. The workspace suite uses mock Discord, not a live guild.

## Rollback and release

Revert the feature code and keep announcements disabled until runtime integration is independently reviewed. Migration `0170` is additive: leave the tables in place rather than deleting signup data. No destructive rollback is performed by this slice.

The PR/commits use Conventional Commits and `CHANGELOG.md` has an Unreleased entry. The existing release workflow is unchanged. Controller compilation requires the deployed bounded pool; a refused/missing pool is not permission to compile elsewhere. Hosted CI retains its ephemeral build/test services and runs all targeted regressions.
