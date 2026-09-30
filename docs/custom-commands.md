# Custom-command domain and persistence

This slice ports parity §1 #14–#16, #22–#23 as framework-free `two-bot-core`
logic and a `db`-feature SQLx store. Migration `0130_custom_commands.sql` is
embedded by the cutover migration runner. The command admin definitions already
live in `feature_commands.rs`.

## Verification

On the controller, use the bounded cache wrapper (see `docs/build-cache.md`):

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --lib custom_commands
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets --features db -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db --test custom_command_store -- --ignored
```

Hosted CI uses the equivalent direct Cargo commands on its ephemeral runner.
A refused controller cache admission is not permission to compile outside it.

The last command connects only to `agent-testdb:5432`, database/user
`agent_test`, empty password. It never reads `DATABASE_URL` or application
credentials. CI sets both `CI=true` and `TWO_CUSTOM_COMMAND_TEST_CI=1` to use its
local Postgres service container instead. Schema objects are created under
`pg_temp` in a rolled-back transaction, not in persistent application schemas.

Coverage includes placeholders and UTF-16 length parity, builtin collisions,
feature gates, first-token triggers, registry merge and republish decisions,
upsert provenance, unique trigger conflicts, disabled lookup, guild isolation,
shared audit rows, advisory lock isolation and atomic rollback. Default tests
need no live service; the explicit database test fails loudly when unavailable.

## Shared router/executor handoff

The shared S4 interaction router and REST executor are now present on `main`.
`two-bot-discord::custom_commands::CustomCommandRuntime` (feature `db`) now
consumes their routing decisions and executes management/dynamic-slash handlers.
It defers responses before transactional work, edits through the shared executor,
suppresses mentions, audits writes/runs, and serializes mutation plus full-set
publication. A publication failure reports a saved-but-not-synchronized result.

**Not activated in the gateway yet.** READY injection, heartbeat-safe gateway
execution and a caller supplying the ordinary message path's actual moderation
decision are still pending. `handle_message` now executes explicitly accepted
prefixes, but is not an unconditional gateway listener. Do not use `MessageCreate`
or `capture_only: false` as acceptance. Cold RESUME can lack a cached guild name;
this adapter refuses rendering without context rather than inventing a `{server}`
value.

The following checklist includes both implemented adapter contracts and the
remaining gateway/accepted-prefix work:

1. Scope admin interactions to the configured guild, enforce ManageGuild at
   runtime, and call `require_automations_enabled` before any admin read/write.
   Lowercase the supplied name/trigger and provide the default description.
2. Validate `/command` with `validate_put_input`. Begin a transaction, call
   `lock_command_capacity`, and re-read the guild rows on `&mut *tx`. Check
   capacity against `builtin_command_names()` before inserting a new name.
   Pass the same connection to `put_command` and `audit`, then commit. Persist
   rejected outcomes without template/message content. SQL uniqueness errors
   require rollback before recording the rejection in a fresh transaction.
3. After a committed add/update or successful remove, consume
   `resync_registry` through the shared publisher. Rebuild from current DB
   rows plus **all** builtin definitions. An absent remove does not republish.
   A publication failure must not be reported as a fully synchronized write.
4. `/command-list` uses the sorted guild rows and `format_command_list`.
5. Dynamic slash dispatch uses `adjudicate_run`; a known row while automations
   are off gets `AUTOMATIONS_DISABLED_REPLY`. Render inside the delivery/audit
   outcome path so both oversize output and REST failure leave a failed
   `command.run` fact. Emit all output with empty allowed-mention parsing.
6. Only after the automod-accepted guild/channel message hook, call
   `accepted_text_trigger`. Both automation/text flags must be on, bot authors
   and builtin tokens are excluded, and only the first token is examined.
   Resolve with `find_text_trigger`, render/deliver through the shared executor,
   and audit the outcome without storing incoming content.
7. Disable-time deregistration scopes its delete set from DB-backed names,
   including disabled rows, rather than treating Discord's entire live list as
   removable. `deregister_set` deduplicates that DB-derived set. Keep the
   in-flight refusal gate active during deregistration and surface incomplete
   REST deletes instead of declaring disable complete.

## Explicit prefix acceptance and replay safety

`AutomationMessageAcceptance` distinguishes deliberately disabled automod,
completed unmatched inspection, policy exemption, matched inspection, unavailable
inspection, and capture-only operation. Only the first three permit automations.
A match rejects prefix execution even in dry-run mode or when sanctions/deletion
were refused. Missing services and unknown errors fail closed. The caller must
reuse the existing inspection result; this slice does not run a second matcher or
claim to implement the missing automod service. The ordinary message path must
complete before invoking the callback.

This is based on legacy `two-bot` revision
[`9677746`](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/src/discord/client.ts#L426-L535)
and its [consumer](https://github.com/TogetherWeOwn/two-bot/blob/96777468472f23a02a1e97a43ffab3912fe5df2a/src/automations/gateway.ts).
Two deliberate tightenings: legacy unknown inspection errors can emit acceptance
because its `null` result is falsy; the port refuses them. The port also explicitly
excludes webhooks, rather than relying on their bot-author flag.

Before DB lookup, the adapter checks the configured guild, human/non-webhook
author, both feature gates, acceptance, and a non-builtin first token. Leading
whitespace does not trigger; arguments are ignored. Enabled guild-scoped rows
render real user, username, server and channel context through the shared executor
with mention suppression.

A committed, immutable `command.text_attempt` audit row reserves the source message
ID before any POST. `ON CONFLICT (id) DO NOTHING` excludes concurrent/replayed
invocations across runtime instances. Its `unknown` outcome and `delivery_pending`
reason mean only that an attempt was reserved, not that Discord accepted it.
The separate `command.run` result records success or failure, with fixed reason
codes and no incoming content. IDs are deterministic `custom:text:attempt:<id>` and
`custom:text:result:<id>` text values in the existing audit schema.

**At-most-once attempt, not guaranteed delivery:** cancellation, an ambiguous
commit/network response, or a failed result audit must never clear the reservation
or resend automatically. A crash after reservation may lose the reply; an attempt
without a result stays unknown for operator reconciliation. The source message ID
is also an enforced Discord nonce, but the durable reservation—not Discord's short
nonce window—is the replay guard. Do not purge attempt rows independently of the
corresponding gateway replay horizon.

The gateway intent now requests Message Content when both `TWO_AUTOMATIONS=1` and
`TWO_TEXT_COMMANDS=1`; existing automod/ticket intent reasons are preserved. The
Worker forwards those two values unchanged on both container startup paths.
Neither flag is enabled by default, and no deployment settings are changed. The
privileged intent must also be authorized for the Discord application before
operators opt into text commands.

SQLx 0.9's documented `Executor` contract supports both pools and transaction
connections; dereference transactions as `&mut *tx`:
https://docs.rs/sqlx/0.9.0/sqlx/trait.Executor.html

Transport-level runtime fixtures are in
`crates/discord/tests/custom_command_runtime.rs`; hosted CI runs them explicitly
with `--features db --test custom_command_runtime -- --include-ignored`. They
use the loopback mock REST double and a single testdb connection with session-local
`pg_temp` tables. Unlike the store-only fixture, those tables survive service
commits and disappear when the pool closes. No application credentials are read.
They cover routing refusals, full-set publication, dynamic rendering, safe reply
edits, audit rollback and honest publication/delivery failures. Prefix fixtures
add zero-I/O rejection gates, explicit acceptance, first-token rendering, disabled
and unknown rows, concurrent/restarted/unknown attempts, render and delivery
failures, and audit failure before/after POST. These Rust fixtures are **added,
not locally executed**, while the controller cache pool is absent. Worker gate
passthrough fixtures are added too; executing them requires the pinned local
`@cloudflare/containers` SDK.

Deferred completion follows Twilight 0.17's documented `update_response` builder:
https://docs.rs/twilight-http/0.17.0/twilight_http/request/application/interaction/struct.UpdateResponse.html

No production guild/token is needed or authorized for these tests.
