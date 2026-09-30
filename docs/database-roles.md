# Database roles

The bot, migrations and web reader must not share an owner credential. These
roles are PostgreSQL `NOLOGIN` privilege groups; this tooling never creates,
prints or rotates passwords. The operator provisions separate login identities
and grants each exactly one group. Never grant the migrator group to a runtime
or reader login.

| Group | Database | Bot schema | Bot tables | Sequences | `web_v1` |
| --- | --- | --- | --- | --- | --- |
| `two_bot_migrator` | CONNECT, CREATE | Owner (DDL) | Owner | Owner | Owner |
| `two_bot_runtime` | CONNECT | USAGE, no CREATE | SELECT, INSERT, UPDATE, DELETE | USAGE, SELECT | No access |
| `two_web_reader` | CONNECT | No access | No access | No access | USAGE; SELECT on views only |

Runtime cannot create schemas, tables, temporary tables or executable functions.
The reader cannot read bot base tables, including through inherited/public
grants. Normal PostgreSQL views deliberately run with their owner's base-table
privileges: only the reviewed `web_v1` projection is exposed. Do not use
`security_invoker` views for this owner-mediated contract.

## Operator boundary

`two-bot db roles plan` prints SQL; it does not open a database connection or
execute anything. Review the complete plan before applying it with an authorized
operator identity. Applying roles to Neon is outside automated tests and agent
execution. The plan has no password statements and does not migrate data.

`two-bot db roles verify` is a read-only inspection. Drift is a failure, not an
automatic repair. It must be run with an audit identity that can inspect the
catalog, not a credential copied from another service. Errors never justify
substituting another inherited credential.

A deployment needs verification for the privilege groups **and** its actual
login bindings. A group-only PASS does not certify an operator's independently
provisioned login identities or another database in the PostgreSQL cluster.

## Testing

Database tests may use only disposable `agent-testdb` or a CI PostgreSQL service,
behind the existing test-database guard. Never production or staging. The test
creates its own scratch database; role changes and negative probes must stay in
that test container. The test does not inspect, alter or drop any existing
application database.
