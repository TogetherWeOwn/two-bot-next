# Database roles

The bot, migrations and web reader must not share an owner credential. These
roles are PostgreSQL `NOLOGIN` privilege groups; this tooling never creates,
prints or rotates passwords. The operator provisions separate login identities
and grants each exactly one group. Never grant the migrator group to a runtime,
reader or read-only login. Groups must have no outgoing memberships (including
predefined roles such as `pg_read_all_data`) or superuser/CREATEDB/CREATEROLE/
REPLICATION/BYPASSRLS attributes. The read-only migrator group owns nothing and
holds no membership path into the migrator group.

| Group | Database | Bot schema | Bot tables | Sequences | `web_v1` |
| --- | --- | --- | --- | --- | --- |
| `two_bot_migrator` | CONNECT, CREATE; no TEMP | Owner (DDL) | Owner | Owner | Owner |
| `two_bot_runtime` | CONNECT; no CREATE/TEMP | USAGE, no CREATE | Ordinary tables: SELECT, INSERT, UPDATE, DELETE; admission lanes: SELECT, INSERT, UPDATE only | USAGE, SELECT; no UPDATE | No access |
| `two_web_reader` | CONNECT; no CREATE/TEMP | No access | No access | No access | USAGE; SELECT on nine reviewed views |
| `two_bot_migrator_ro` | CONNECT; no CREATE/TEMP | USAGE, no CREATE | SELECT on bot tables, admission lanes and SQLx ledger; no DML | No access | USAGE; no view or function access |

No runtime, reader or read-only migrator grants carry grant options. Runtime cannot create schemas,
tables, temporary tables or functions, alter tables, truncate them, or read the
SQLx migration ledger. The `two_bot_migrator_ro` group exists for the
read-only migration-plan step: it reads bot tables, the admission lanes and the
ledger through SELECT only, with no DML, DDL, sequence, function or default
privileges, so a plan run can never change data or schema. Migrator-only tables
stay unreadable to it, exactly as for runtime and reader.

Existing append-only audit triggers continue to constrain
DML; a grant does not disable those controls. Both `discord_send_admission` and
`voice_create_reservations` are restricted admission lanes: runtime has SELECT,
INSERT and UPDATE only, never DELETE/TRUNCATE. The reservation rows preserve
accepted room-create history across rollback, deletion and restart; runtime can
settle a hold but cannot erase that durable history. Reader and PUBLIC receive
no admission-lane access. The verifier requires these three runtime privileges,
read-only SELECT for the migrator-plan role, and rejects extra erase or public access.
Member erasure is the operator-only path that writes the migrator-only audit
table, so it deletes reservation rows as the migrator, never as runtime.

Two tables are migrator-only (`migrator` kind): `member_erasure_audit`
(operator accountability written by the operator-only erasure path) and
`invite_campaigns` (go.two.gg redirect store). They still have readers outside
these three groups, so neither group receives a grant: `two-bot backup` reads
both through `DUMP_TABLES` (`crates/core/src/backup/dump_file.rs`, via
`dump.rs`; a runtime-group login cannot read them, so the nightly dump would
abort), and the go.two.gg `REDIRECT_DB` connector reads `invite_campaigns`
(`wrangler/src/redirect-store.ts`, unbound today with a snapshot fallback).
Each of those readers needs its own reviewed identity. The plan transfers both
tables to the migrator and grants the migrator ALL with no runtime or reader
grant; the verifier requires migrator ownership/privileges and rejects any
runtime/reader access, including TOAST-storage ownership drift.

The reader cannot read bot base tables, including through inherited/public or
column-level grants. Normal PostgreSQL views deliberately run with their owner's
base-table privileges: only the reviewed `web_v1` projection is exposed. Do not
use `security_invoker` views for this owner-mediated contract. The reader also
needs EXECUTE on exactly `_ts(text)`, `_iso(timestamptz)` and `_json(text)` in
`web_v1`, the invoker-rights formatting/parsing helpers used by those views. No
other application function may be executable by the runtime or reader. Trigger
functions do not need runtime EXECUTE once their triggers have been created.

## Channel lane reconciliation access

The local operator-only `two-bot moderation release-channel` command uses existing
grants; it never migrates or creates roles. Use an already approved,
environment-bound operator login, not a runtime-owned credential recovered from a
process. `--operator` is attribution; the audit additionally records the actual
PostgreSQL login and effective role. No public/slash/website release route exists.

| Table | Inspection | Explicit confirmed release | Existing group |
| --- | --- | --- | --- |
| `moderation_channel_executions` | SELECT | SELECT, DELETE | `two_bot_runtime` DML; `two_bot_migrator_ro` inspection only |
| `moderation_idempotency` | SELECT | SELECT, UPDATE | `two_bot_runtime` DML; `two_bot_migrator_ro` inspection only |
| `moderation_audit` | None | INSERT (strict, same transaction) | `two_bot_runtime` DML |
| `moderation_lockdowns` | Reconciliation reads outside the CLI | **No access/write by this command** | Existing recovery policy unchanged |

The ledger is retained as a terminal replay tombstone, not deleted, and the
member-erasure plan skips it (`done` / `operator_released`), so the personal audit
rows can be erased without re-arming the old key. The caller-supplied key may
itself be a member snowflake; if so, it remains as an identity-bearing replay
fence because changing it could let delayed delivery repeat the uncertain
mutation. The linked audit row, including matching keys and free-text reasons,
remains covered by the erasure manifest. Release replaces the obsolete ownership
token and actor-derived request hash with fixed `operator_released` markers; the
original token and hash are redacted from inspection and audit snapshots. A
read-only login cannot confirm release; `two_web_reader` cannot inspect or
release these base tables. No new privilege is required. Follow the
[channel reconciliation checklist](channel-lane-reconciliation.md) before any
confirmation; a database grant is not incident approval.

## Plan and verify

```sh
two-bot db roles plan > database-roles.sql
two-bot db roles plan --phase bootstrap > database-roles-bootstrap.sql
# Review the rendered SQL; operator applies it separately.
two-bot db roles verify
```

Both phases snapshot whether `current_user` already had a direct
`two_bot_migrator` membership before any group is created. If the identity
already has usable SET/USAGE, the plan leaves that membership and its grantor,
ADMIN, INHERIT and SET options untouched. If a temporary membership is needed,
it is granted explicitly as `CURRENT_USER` (`WITH INHERIT TRUE, SET TRUE` on
PostgreSQL 16+, plain `GRANT` on 15) and only that grant is revoked before
`COMMIT`; the plan refuses when the membership is still unusable. Without it a
non-superuser provisioning identity fails at `ALTER SCHEMA public OWNER TO
two_bot_migrator` and loses `public` access once ownership flips. If the
identity's own unusable grant (granted by itself) would need its options changed
for the plan to proceed, the plan refuses without changing the membership.
PostgreSQL 16+ automatically gives a CREATEROLE creator an ADMIN-only membership
on a newly created role; that membership is distinct from the plan's temporary
grant and is preserved. When the identity held no direct membership before the
plan, the plan also revokes any membership granted by the identity itself at
the end, which covers the usable self-grant that `CREATE ROLE` adds when the
identity's `createrole_self_grant` setting is non-empty (PostgreSQL 16+); that
row is never one the operator put there. Memberships the same setting adds in
the runtime, web reader and read-only migrator groups are outside this rule and
are not touched. The bootstrap render differs from
the default (`full`) render only by the skip lines; neither render contains a
password or a login grant.

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
current migrations' 86 ordinary bot tables plus two restricted admission lanes,
SQLx ledger, two migrator-only tables, twelve sequences,
nine web views and six functions (three trigger helpers plus three `web_v1`
helpers). The read-only migrator group additionally reads the ordinary tables,
the admission lanes and the ledger; it reads no migrator-only table, sequence,
view or function. This includes
`gateway_onboarding_jobs` and its sequence: the DML-only gateway must recover and
write this queue, while the web reader must not access it. A detached SERIAL
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

Functions in a schema a group cannot use (no USAGE) are not callable by it and are
not drift. Managed Postgres installs extension helpers this way: PlanetScale puts
`hypopg` in its own `pscale_extensions` schema with default PUBLIC EXECUTE and no
USAGE for other roles. A USAGE grant on such a schema makes its functions count again.

Normal PUBLIC catalog reads/functions remain available. No group may own a
system-schema relation, function or schema (ownership grants implicit DDL and
grant authority even when EXECUTE already belongs to PUBLIC), except the migrator's
automatically transferred TOAST storage/indexes of allowlisted bot tables. Additional system-schema
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

1. For the first bootstrap, the order is fixed: (a) the authorized
   provisioning identity applies the rendered bootstrap phase
   (`two-bot db roles plan --phase bootstrap`), which transfers the existing
   allowlisted objects and skips relations/sequences the pending migrations
   have not created yet; (b) the operator provisions the dedicated migrator
   login as a member of `two_bot_migrator`; (c) the migration runner plans the
   pending set read-only for review; (d) the runner applies the pending
   migrations with `SET ROLE two_bot_migrator`, so new objects are
   migrator-owned from creation; (e) the provisioning identity applies the
   full phase (`two-bot db roles plan`, the default), idempotently; (f) verify
   reads 0 findings. For later migrations, the dedicated migrator login must
   `SET ROLE two_bot_migrator` before creating objects: creator-specific
   default ACLs belong to the group, not automatically to a member login.
   Reapply the reviewed full plan after migrations and view updates, then
   verify. `sql/web_v1.sql` is applied with `public` as the bot-table search
   path before the bootstrap phase; functions stay strict in both phases.
2. Review and apply the rendered role plan with the authorized provisioning
   identity. It transfers only allowlisted objects to `two_bot_migrator`; unrelated
   tables are not transferred or granted to the runtime.
3. Verify group drift and independently verify the four login bindings. Point
   gateway `DATABASE_URL` at the runtime login, website at the reader login,
   and the read-only plan step at a login holding only `two_bot_migrator_ro`.
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
in version order (directory enumeration, not a hardcoded subset) and the actual
view contract, apply the plan twice, exercise allowed DML/DDL/view reads and
denied runtime CREATE/ALTER/TRUNCATE/temporary-table and reader base-table
operations, then inject and restore privilege drift. Ticket and transcript tests
exercise valid runtime CRUD, deny reader SELECT/INSERT and runtime ALTER/TRUNCATE,
and detect missing runtime and excess reader privileges on both tables, restoring
a clean matrix after each drift. The B2 regression exercises runtime CRUD on the
six runtime tables and migrator-only denial on the two `migrator` tables, with
drift cases for both classes. Offline tests also cover CLI execution-flag
rejection and directory-enumerating matrix coverage over every migration file.
