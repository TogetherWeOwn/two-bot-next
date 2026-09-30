# Internal settings actions

The settings executor is independent of the HTTP receiver and runtime hot-reload
application. Its caller must enforce the signed action allowlist and claim the
outer durable idempotency key before executing `settings.set`. Replaying that
stored result must not execute another write, audit row or version increment.

## Legacy response contract

`settings.get` reads the guild's stored override only, never the process
environment. The ordered result fields are:

```json
{"key":"TWO_RAID_JOIN_THRESHOLD","value":"8","source":"store"}
```

An absent override returns `value: null` and `source: "unset"`. This tells the
dashboard that the runtime falls back to its environment/default without
revealing the fallback value. It is intentionally not an environment read-through.

`settings.set` returns only the key and outcome:

```json
{"key":"TWO_RAID_JOIN_THRESHOLD","outcome":"saved"}
```

A JSON null deletes the override and returns `outcome: "unset"`. The admin's
`updated_by` Discord ID is recorded as the audit actor, not inferred from the bot.
No set response or log outcome includes the submitted value.

Both actions refuse unknown and environment-only keys, including secret,
capability, boot and network settings. Key classification is fail-closed.
Request/result Debug output is redacted, and raw SQL error details are discarded
at the action boundary because database errors can include row values. Decoded
U+0000 in any value string or nested object key is refused before SQL with a
non-retryable `malformed`/400 result; Postgres JSONB cannot store it. The refusal
contains no submitted value. A literal backslash-u escape is not a decoded NUL.

## Execution and optimistic concurrency

`two_bot_core::internal_settings::SettingsCommand::parse` produces a validated
command. `two_bot_cutover::internal_settings::execute_settings` executes it
against `SettingsStore` using the configured guild ID, not a body-supplied guild.
The receiver must still enforce action enablement, signing and replay protection;
a validated command is not an authorization grant.

`settings.set` accepts an optional `expected_version` non-negative integer.
It is the observed **row version**, not the global poll revision. Zero expects
an absent override, never a stored token of zero. A legacy version-zero row
still returns `source: "store"`; expected-zero saves/deletes refuse it. An
explicit legacy unconditional save assigns it a new positive token, after which
version-checked writes work normally. A supplied stale version returns
non-retryable HTTP 409 `version_conflict`; refresh before deliberately submitting
a new save. Omitting it retains the legacy unconditional-save behavior. The
comparison occurs under the existing revision-row lock, before the value, audit
or poll revision changes, so simultaneous saves against one version cannot both
succeed.

`SettingsOutcome.observed_version` exposes the observed/committed version to a
future HTTP adapter as metadata, not an extra field in the legacy `result`.
After deletion it is zero. An absent-row token does not detect a create/delete
cycle that returns the key to absence. Clients needing that stronger guarantee
need a separate revision contract; this slice does not redefine legacy results.

The value write/delete, audit transition and poll-revision advance commit in
one settings transaction. Audit failure rolls back all three. Row versions
advance on every insert/update, including supported direct SQL writers, via
migration `0331`'s database-owned row trigger. Caller-supplied versions are
ignored; the upgrade seeds the allocator above existing row tokens without
changing migration `0330`. Additive migration `0332` binds allocation to the
trigger's target table schema, independent of the caller's sequence search path.
Additive migration `0333` locks settings DML and reseeds that canonical allocator
above existing tokens (including intervening shadow-issued `0331` tokens) and
unused allocations without rewinding it. It also binds the statement trigger's
revision table to the target schema: qualified direct writers lock/advance the
same revision used by store CAS and polling, not a caller's shadow table. A
missing target revision row still refuses the statement. Applied `0330`–`0332`
checksums and existing row tokens remain unchanged. Deletion advances the
transactional poll revision. No runtime hot-reload consumer is changed here.

The outer durable store and this transaction are separate. A receiver must not
re-execute an ambiguous write after losing a terminal-response commit. Its
terminal-response adapter must preserve the value-free `{key,outcome}` result;
HTTP/durable-result integration belongs to the receiver slice.

## Verification

```sh
cargo test -p two-bot-core --locked internal_settings --lib
cargo test -p two-bot-cutover --locked internal_settings --lib
cargo test -p two-bot-cutover --test settings_db --locked -- --ignored
```

The existing `settings_db` harness pins `agent-testdb:5432` locally, or CI's
loopback service container, user `agent_test` with an empty password. It never
reads an app database URL. Each test creates and drops only its generated
schema. The existing `check` workflow explicitly runs this suite, including the
executor regressions, against its disposable Postgres service.

Reference: legacy `TogetherWeOwn/two-bot`, `src/internal/actions.ts`,
`settingsGet` / `settingsSet` and `test/unit.internalsettings.test.ts` at commit
`96777468472f23a02a1e97a43ffab3912fe5df2a`.
