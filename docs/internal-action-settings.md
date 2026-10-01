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

`settings.set` accepts an optional `expected_version` signed integer token.
It is the observed **CAS token**, not the copyable legacy `version` column or
poll revision. Tokens are opaque: compare for equality, never magnitude. Zero
expects an absent override, never an existing row. Migration `0334` assigns a
fresh negative token to every existing row, including legacy version-zero rows;
all previously accepted nonnegative tokens become stale. Clients must reread
metadata after upgrade. Negative tokens stay within JavaScript's exact integer
range; the noncycling sequence refuses writes on exhaustion rather than reuse.
A supplied stale token returns non-retryable HTTP 409 `version_conflict`; refresh
before deliberately submitting a new save. Omitting it retains the legacy
unconditional-save behavior. Comparison occurs under the existing revision-row
lock, before value, audit or poll revision changes, so simultaneous saves
against one token cannot both succeed.

`SettingsOutcome.observed_version` exposes the observed/committed version to a
future HTTP adapter as metadata, not an extra field in the legacy `result`.
After deletion it is zero. An absent-row token does not detect a create/delete
cycle that returns the key to absence. Clients needing that stronger guarantee
need a separate revision contract; this slice does not redefine legacy results.

The value write/delete, audit transition and poll-revision advance commit in
one settings transaction. Audit failure rolls back all three. Additive `0334`
separates destination-owned `cas_version` from copyable legacy `version`. Its
new descending allocator is never reseeded: standalone `nextval` only creates
gaps, not reuse. Every insert/update, including direct SQL/upserts, replaces
caller-supplied CAS tokens via a target-schema-bound row trigger. The volatile
column default backfills existing rows under a DDL lock without settings DML,
value/audit/poll changes or acquiring the revision lock (avoiding an upgrade
cycle with waiting store writers). Preserved legacy versions do not participate
in CAS or poll safety. Copy replay can therefore preserve all legacy columns,
issue no DML, and leave CAS metadata unchanged; a genuine copied change gets a
fresh token even if its source version is unchanged.

Applied `0330`–`0333` checksums stay unchanged. The earlier allocator repair
cannot reconstruct vanished shadow tokens or serialize read/setval with every
standalone allocation; it is retired from CAS, not treated as a safe high-water
mark. `0333`'s schema-bound revision trigger remains: qualified writers lock and
advance the same revision used by store CAS and polling, never a shadow table.
A missing target revision row still refuses writes. Deletes advance the poll
revision. No runtime hot-reload consumer is changed here.

The outer durable store and this transaction are separate. A receiver must not
re-execute an ambiguous write after losing a terminal-response commit. Its
terminal-response adapter must preserve the value-free `{key,outcome}` result;
HTTP/durable-result integration belongs to the receiver slice.

## Verification

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core internal_settings --lib
python3 scripts/cargo_cache.py run -- test -p two-bot-cutover internal_settings --lib
python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --test settings_db -- --ignored
```

The existing `settings_db` harness pins `agent-testdb:5432` locally, or CI's
loopback service container, user `agent_test` with an empty password. It never
reads an app database URL. Each test creates and drops only its generated
schema. The existing `check` workflow explicitly runs this suite, including the
executor regressions, against its disposable Postgres service.

Reference: legacy `TogetherWeOwn/two-bot`, `src/internal/actions.ts`,
`settingsGet` / `settingsSet` and `test/unit.internalsettings.test.ts` at commit
`96777468472f23a02a1e97a43ffab3912fe5df2a`.
