# Database roles

The bot, migrations and web reader must not share an owner credential. These
roles are PostgreSQL `NOLOGIN` privilege groups; this tooling never creates,
prints or rotates passwords. The operator provisions separate login identities
and grants each exactly one group. Never grant the migrator group to a runtime
or reader login. Groups must have no outgoing memberships (including predefined
roles such as `pg_read_all_data`) or superuser/CREATEDB/CREATEROLE/REPLICATION/
BYPASSRLS attributes.

| Group | Database | Bot schema | Bot tables | Sequences | `web_v1` |
| --- | --- | --- | --- | --- | --- |
| `two_bot_migrator` | CONNECT, CREATE; no TEMP | Owner (DDL) | Owner | Owner | Owner |
| `two_bot_runtime` | CONNECT; no CREATE/TEMP | USAGE, no CREATE | SELECT, INSERT, UPDATE, DELETE | USAGE, SELECT; no UPDATE | No access |
| `two_web_reader` | CONNECT; no CREATE/TEMP | No access | No access | No access | USAGE; SELECT on nine reviewed views |

No runtime or reader grants carry grant options. Runtime cannot create schemas,
tables, temporary tables or functions, alter tables, truncate them, or read the
SQLx migration ledger. Existing append-only audit triggers continue to constrain
DML; a grant does not disable those controls.

The reader cannot read bot base tables, including through inherited/public or
column-level grants. Normal PostgreSQL views deliberately run with their owner's
base-table privileges: only the reviewed `web_v1` projection is exposed. Do not
use `security_invoker` views for this owner-mediated contract. The reader also
needs EXECUTE on exactly `_ts(text)`, `_iso(timestamptz)` and `_json(text)` in
`web_v1`, the invoker-rights formatting/parsing helpers used by those views. No
other application function may be executable by the runtime or reader. Trigger
functions do not need runtime EXECUTE once their triggers have been created.

## Plan and verify

```sh
two-bot db roles plan > database-roles.sql
# Review the rendered SQL; operator applies it separately.
two-bot db roles verify
```

`plan` prints SQL without opening a connection, reading a database URL or executing
anything. There is deliberately **no apply subcommand or --apply flag**. This is
stricter than requiring an explicit non-test apply flag. `sql/database_roles.sql`
and `sql/verify_database_roles.sql` are templates: the CLI embeds the common
`sql/database_role_matrix.sql` object allowlist. Do not apply the unrendered plan.

`verify` uses `TWO_DATABASE_URL`, skips migrations, and inspects catalogs inside a
read-only transaction with a catalog-first search path. Exit 0 means no group
matrix drift; exit 1 means drift or inspection/connection failure; malformed CLI
arguments exit 2. Connection errors with potentially sensitive details are
withheld. Findings identify objects/privileges, never table contents or passwords.
Drift is a failure, not an automatic repair.

The tooling requires **PostgreSQL 15 or newer**, including parameter ACLs and
view `security_invoker` options; an unsupported catalog/query fails verification,
never returns PASS. Verification checks missing groups/objects, group attributes
and memberships, database/schema privileges, ownership/object kinds, effective
table/column/sequence/function privileges (including PUBLIC), grant options, parsed
boolean view invoker settings and unsafe future grants. Explicit grants cover the
current migrations' 41 bot tables, SQLx ledger, eight named SERIAL sequences and
`guild_settings_version_seq`, nine web views and five functions. A detached SERIAL
sequence remains required even after `OWNED BY NONE`. New relations/sequences need
a reviewed matrix update; there are **no wildcard future-table grants**.
Migrator-created functions default to no PUBLIC EXECUTE. Ownership alone does not
prove ordinary ACL privileges: verification checks the migrator's required table,
sequence and helper-function rights, and reapplication restores those rights.

No group may hold explicit parameter SET/ALTER SYSTEM grants, including grants
through PUBLIC or with grant options. Such grants (e.g. `session_replication_role`)
can disable audit triggers without a superuser attribute. They are cluster-wide:
the plan does not revoke them automatically; any correction needs an independently
reviewed operator change.

Normal PUBLIC catalog reads/functions remain available. Additional system-schema
relation, column, function and schema grants are checked against PostgreSQL's
`pg_init_privs` initial PUBLIC ACLs (or its default ACL where no initial ACL exists),
not the possibly drifted current PUBLIC grants. Built-in `information_schema`
objects have no initial ACL records: the policy allows SELECT and schema USAGE
only on its shipped objects (OID below `FirstNormalObjectId`, 16384); user-added
objects do not inherit that exception. Sensitive catalogs such as `pg_authid` and
restricted catalog functions do not gain an exception. Tests check grants without
reading password-verifier values or server files.

A group-only PASS does **not** certify independently provisioned login identities,
other databases in the PostgreSQL cluster, RLS policy behavior, arbitrary changed
view/function definitions, or the contents of the website projection. These
remain deployment/review responsibilities. Audit actual login bindings for
superuser/bypass attributes, direct grants, database ownership, and additional
memberships; do not rely on group verification alone.

## Operator boundary and deployment order

Applying roles to Neon is outside agent execution and automated tests. Use an
authorized operator identity capable of creating groups, changing object owners
and revoking grants; do not copy a credential from another service if it fails.
The plan is transactional and refuses unsafe existing groups instead of changing
their login status or silently removing memberships. It contains no passwords.

1. For the first bootstrap, the authorized provisioning identity applies
   `crates/cutover/migrations` through the established SQLx migration process
   (ledger in `public`) and applies `sql/web_v1.sql` with `public` as the bot-table
   search path. The full role plan requires those objects to exist. For later
   migrations, the dedicated migrator login must `SET ROLE two_bot_migrator`
   before creating objects: creator-specific default ACLs belong to the group,
   not automatically to a member login. Reapply the reviewed plan after migrations
   and view updates, then verify.
2. Review and apply the rendered role plan with the authorized provisioning
   identity. It transfers only allowlisted objects to `two_bot_migrator`; unrelated
   tables are not transferred or granted to the runtime.
3. Verify group drift and independently verify the three login bindings. Point
   gateway `DATABASE_URL` at the runtime login and website at the reader login.
4. Start the gateway **after** migrations. Gateway connections now explicitly
   skip migrations: a DML-only credential must never be used for startup DDL.

**Shared-database impact:** the plan removes database CONNECT/TEMP and schema
USAGE/CREATE grants from PUBLIC, and transfers `public`/`web_v1` schema ownership.
Before applying, explicitly grant unrelated services their intended access and
review schema ownership with the operator. Do not apply blind to a shared Neon
database. The verifier reports unexpected access in other user schemas rather
than silently revoking another application's privileges. Existing column grants,
creator-specific schema default grants, or unrelated PUBLIC function grants may
also require a separately reviewed operator correction.

Rollback is an operator-reviewed restoration of the prior ownership/ACL inventory
and login configuration, not dropping groups or deleting data. Save that inventory
before changing permissions. Never rotate/delete a credential as a side effect.

## Testing

Database tests use only disposable `agent-testdb` / the existing CI PostgreSQL
service and share the strict guard extracted from `internal_action_store`: explicit
empty-password `agent_test`, host `agent-testdb`, port 5432, database `agent_test`,
no URL overrides or inherited nonempty password. The test then creates a generated
scratch database and unique cluster roles; it removes only those generated names.
No production, staging or existing application database is touched.

```sh
# Controller: compiling tests always use the bounded Cargo wrapper.
TWO_ROLES_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/agent_test \
  python3 scripts/cargo_cache.py run -- test -p two-bot-core --features db \
  --test database_roles
```

Hosted CI automatically runs the acceptance test on its ephemeral service during
the existing integration-test step (`GITHUB_ACTIONS=true` selects the fixed test
URL). Without CI or `TWO_ROLES_TEST_DATABASE_URL`, the offline suite makes no
connection; configured failures are never skipped. Tests apply every real migration
and the actual view contract, apply the plan twice, exercise allowed DML/DDL/view
reads and denied runtime CREATE/ALTER/TRUNCATE/temporary-table and reader base-table
operations, then inject and restore privilege drift. Offline tests also cover CLI
execution-flag rejection and matrix coverage.
