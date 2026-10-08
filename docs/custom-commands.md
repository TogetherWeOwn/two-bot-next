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

**Gateway composition is implemented, not yet runtime-verified or deployed.**
The bot now constructs the shared executor and registered router, bootstraps real
application/guild context before constructing the shard, and awaits custom-command
execution after the ordinary pipeline and before checkpoint COMMIT. READY and
RESUMED both synchronize the complete gated registry. Cold RESUME uses the shared
executor's fresh guild-name read until the gateway cache supplies a name.

The gateway injects main's shared `CommandRuntime`: sticky, feeds and custom
commands use the same `Arc<InteractionRouter>`, pool and cloned `ActionExecutor`
(including its proxy and pacing state). Custom-command ownership is checked before
builtin fallback, so acknowledged custom interactions never receive a second
unavailable reply. Only the custom service's serialized, DB-backed full-set
publisher runs on READY/RESUMED; no builtin-only publication can erase custom rows.
The combined mock gateway fixture checks registry coexistence and single replies
for custom, management, feed and sticky commands. It is added, not locally run.

The ordinary message pipeline still has **no automod inspection service**. Prefix
execution therefore requires an explicit `TWO_AUTOMOD=0`, in addition to both
custom-command gates. Missing, malformed, or enabled automod configuration yields
`Unavailable` and no prefix lookup/send. This deliberately stricter interim rule
must not be presented as automod enforcement or unmatched/exempt integration.
Neither `MessageCreate` nor `capture_only: false` proves acceptance. Completing
normal automod-enabled prefix operation still requires the ordinary path's actual
inspection result, not a second matcher in this custom-command adapter.

Runtime dispatch and checkpoint persistence share one total deadline: the lesser
of five seconds and one quarter of HELLO's heartbeat interval. Readiness stays
unavailable during that work; timeout cancels it and stops the shard without
advancing the checkpoint. There are no detached command jobs. Durable prefix
reservations survive cancellation and prevent replay of an uncertain POST.
Returned per-command errors are sanitized and the dispatch is checkpointed rather
than repeatedly replaying an already acknowledged interaction. Registry-sync
failure instead stops readiness and leaves its checkpoint unchanged; a restart
rebuilds the full desired registry from current state.

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
with mention suppression. A process-local five-second guild+actor window admits
non-builtin prefix candidates before that lookup, including unknown triggers;
ordinary chat and builtin tokens do not consume it. Clones share the bounded
map. Refused candidates produce no SQL, send or audit, and redelivery of the
admitted message still reaches the permanent replay claim. See
[automation actor admission](automation-admission.md) for expiry, capacity and
the explicit restart contract.

A committed, immutable `command.text_attempt` audit row reserves the source message
ID before any POST. `ON CONFLICT (id) DO NOTHING` excludes concurrent/replayed
invocations across runtime instances. Its `unknown` outcome and `delivery_pending`
reason mean only that an attempt was reserved, not that Discord accepted it.
The separate `command.run` result records success or definite failure, with fixed
reason codes and no incoming content. Timeout, transport, 5xx and 429 outcomes use
the shared executor's uncertainty classification: they leave the attempt unresolved
and never append a misleading definitive-failure result. IDs are deterministic `custom:text:attempt:<id>` and
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
Worker forwards those two values and `TWO_AUTOMOD` unchanged on both container
startup paths; it never fabricates a disabled moderation value. Neither custom
flag is enabled by default, and no deployment settings are changed. The
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

Bootstrap uses Twilight 0.17.1 request builders through the same executor's
bounded, one-attempt read path; it validates nonzero application identity, exact
guild identity and a nonblank guild name before constructing the shard:
- https://docs.rs/twilight-http/0.17.1/twilight_http/request/struct.GetUserApplicationInfo.html
- https://docs.rs/twilight-http/0.17.1/twilight_http/request/guild/struct.GetGuild.html

`crates/discord/tests/bootstrap_context.rs` adds loopback-only metadata validation,
GET-only routes, no retries and shared-timeout coverage. The hosted workspace
integration-test step includes them. `crates/bot/src/gateway_tests/commands.rs`
adds real-shard/mock-REST/testdb fixtures for READY slash and prefix dispatch,
checkpoint progress, cold RESUME without cached guild context, unavailable
moderation and feature gates, terminal command errors, delayed POST cancellation
and replay suppression, registry failure and application-identity mismatch. The
existing hosted `gateway_tests -- --ignored --test-threads=1` step includes them.
All new Rust fixtures remain uncompiled/unexecuted locally. No production
guild/token is needed or authorized for these tests.
