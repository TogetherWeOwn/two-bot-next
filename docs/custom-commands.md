# Custom-command domain and persistence

This slice ports parity §1 #14–#16, #22–#23 as framework-free `two-bot-core`
logic and a `db`-feature SQLx store. Migration `0130_custom_commands.sql` is
embedded by the cutover migration runner. The command admin definitions already
live in `feature_commands.rs`.

## Verification

```sh
cargo test -p two-bot-core --locked
cargo clippy -p two-bot-core --all-targets --features db --locked -- -D warnings
cargo test -p two-bot-core --features db --locked --test custom_command_store -- --ignored
```

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

There is no production interaction router or response executor in this slice's
base checkout. These modules do **not** publish commands or reply to Discord.
Wire them through the shared S4 interfaces when those land; do not add a private
dispatcher or HTTP client.

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

SQLx 0.9's documented `Executor` contract supports both pools and transaction
connections; dereference transactions as `&mut *tx`:
https://docs.rs/sqlx/0.9.0/sqlx/trait.Executor.html

The registry tests prove **intent and rebuilt contents**, not a live Discord
republish. Transport-level mock proofs remain part of the wiring commit.
No production guild/token is needed or authorized for these tests.
