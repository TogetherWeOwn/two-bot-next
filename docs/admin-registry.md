# Admin API token registry foundation

This is a schema and pure-authorization contract, **not a live admin gateway**.
No binary lookup, token issuance, public route or enablement is added here.
The existing HMAC service-key path is unchanged. All later gateway and deployment
security gates still apply.

## Authority contract

The historical table is created by cutover `0456` / store `0457`; the forward
repair is cutover `0464_admin_api_token_authority.sql` and its store `0463`
mirror. Applied historical SQL and checksums are immutable. The starboard
foundation uses store `0458` / cutover `0459`; giveaways use cutover `0460` /
store `0461`. The holder repair remains a
separate forward migration.

`admin_api_tokens` has twelve columns: `token_id`, `secret_hash`, `scopes`,
`guild_ids`, `label`, `issued_by`, `issued_at`, `expires_at`, `revoked_at`,
`last_used_at`, `holder_id`, `holder_kind`. The existing primary key and
`(revoked_at, expires_at)` lookup index are unchanged. No grants or persistent
tables are added.

- `holder_id` is required, has no default and must contain a non-whitespace
  character. It names an operator-selected principal, not a caller override or
  a value inferred from a label, issuing operator or guild member.
- `holder_kind` is required with no default, exactly `human` or `ai_agent`.
  AI holders cannot carry `admin`, including in a mixed scope array. A human
  may carry separately granted `moderate` and `admin` scopes.
- Issuance and expiry are required and finite. The lifetime is strictly positive
  and at most **7,776,000 elapsed seconds** (90 times 24 hours), inclusive at
  the upper bound. Numeric epoch subtraction avoids calendar-day/DST changes
  and finite timestamp-endpoint arithmetic overflow. UPDATEs obey the same
  constraints as INSERTs; no expiry or holder is silently rewritten.
- Scope, guild allowlist, token-id and hash constraints remain in force.
  Missing or NULL authority is not an authorization fallback.

`InternalFlags::admin_scope_allows` grants reads for any recognized held scope.
Writes require their **exact** listed scope: admin-only does not grant moderate
writes, and moderate does not grant admin writes. Unknown scopes contribute
nothing; unmapped actions refuse. `role.assign` remains admin. The pure
`admin_token_authorizes` helper additionally checks expiry, revocation and guild
allowlisting; callers must supply a verified authoritative row. This helper is
not a holder check, secret verifier or registry lookup.

Future bearer compatibility is unchanged: `adm_<16lowerhex>.<32+ ASCII base64url
characters>`, total length at most 4096. Only the secret suffix bytes are hashed;
`token_id` is the public lookup identifier. No bearer parser or caller-supplied
identifier override is introduced by this repair.

## Upgrade and backout

The forward file runs in the normal SQLx migration transaction. It locks the
registry before inspecting the catalog and refuses any existing `holder_id` or
`holder_kind` default, even on an empty partial schema. Any existing holder
column must have the exact built-in `pg_catalog.text` type with no type modifier.
Domains (with or without defaults), varchar and other types refuse before DDL:
a domain default can fabricate authority on an omitted-column INSERT even when
no column default exists. The migration never coerces types, rewrites domains,
drops defaults or silently repairs fallback authority. Existing holders must also
use deterministic collations: nondeterministic case-insensitive equality can
otherwise accept `HUMAN` as the closed kind `human`. Both holder columns are
checked under the same lock before DDL. Deterministic database-default and `C`
collations remain compatible; collations are never rewritten. Compatible plain-TEXT
partial schemas without defaults may upgrade: missing columns are added without
defaults, made NOT NULL, and checked with validated constraints. Before any registry DDL, all four named
authority constraints are compared with canonical CHECK expressions parsed on
that server using an empty ordinary reference table in the locked registry's own
schema. The existing schema CREATE privilege suffices; database TEMPORARY is not
required and no privileges are granted. The helper is dropped before registry
ALTER statements, within the same SQLx file transaction. Failure rolls back helper,
registry and history changes; success leaves no helper. A helper-name collision
refuses without replacing or removing the pre-existing object.
A same-name constraint with a different expression, a different constraint kind
or NO INHERIT refuses atomically; VALIDATE alone cannot strengthen a weak CHECK.
Exact canonical pre-existing checks are accepted, including NOT VALID checks
that are then validated. Equivalent but noncanonical expressions may refuse
conservatively rather than silently replacing authority. Guarded creation lets
either chain run first and the other repeat the DDL without changing valid rows,
existing constraint identities or migration history. Run each file through SQLx's
normal per-file transaction; do not execute individual statements outside it.

Any existing historical row lacks a named holder. The upgrade therefore aborts
atomically, preserving the original rows, schema and migration history. It does
not invent holders, delete rows, truncate expiry or leave partial DDL. Expired
or revoked historical rows are not exempt. An incompatible pre-existing partial
schema or row must also satisfy every constraint before the transaction commits.

If migration is refused, stop the rollout. An authorized operator must inspect
and reconcile credential authority through a separately approved procedure;
this document does not authorize deleting/reissuing tokens or applying a live
migration. Do not bypass the constraint or use another database identity. DDL
locks the registry during the transaction; use the existing bounded migration
execution contract, not ad hoc production SQL.

Backout is **binary-only while the gateway remains closed**: retain the forward
columns, NOT NULLs and validated checks. A historical DROP/recreate of `0456`
would lose named-holder and lifetime protections and credential state; it is
not the repair's operational backout. Keep migration history intact.

## Backup, rollback and privacy

Backup discovers live columns dynamically. The current v16 inventory includes the
event voice pair, starboard tables, giveaway tables and the reaction-order ledger;
this holder repair adds no table or archive version. New archives
include both holder columns. Populated pre-holder archives cannot restore valid
authority: missing holder columns hit NOT NULL and roll back the restore
transaction. An older readable
archive that legitimately omits the registry (v4–v11) retains its existing
versioned clearing behavior; it must not fabricate credential holders.

Rollback delta export uses `to_jsonb(t.*)` and includes the holder fields. Import
of an old row with missing authority refuses transactionally rather than guessing.
Expiry is not recency evidence; no holder-edit workflow or timestamp is added.

`holder_id` is explicitly detected as identity-bearing and retained in the
reviewed erasure exception manifest as credential authority, potentially naming
a member. Guild-local activity erasure must not rewrite a principal or silently
alter cross-guild grants. `issued_by` remains issuing-operator attribution.
Credential revocation/retention requires its own authorized disposition; these
exceptions have no automatic erasure expiry. See [privacy](privacy.md). This
classification requires security review on the exact repair head before merge.

## Verification

Independent acceptance lives in `admin_registry_repair_acceptance` and
`admin_token_registry_acceptance`. Both real migration chains are exercised on
the guarded disposable test service, including atomic refusal of a populated
historical schema, empty/populated column- and domain-default refusal, noncanonical
holder-type and nondeterministic-collation refusal with raw type/domain/catalog/history
preservation, uppercase-kind admin INSERT probes, compatible
plain-TEXT no-default upgrades, actual admin-scope omitted-holder INSERTs,
exact 90-day positives, malformed authority, AI admin, invalid updates and
nonfinite/overlong lifetimes. Both-chain no-TEMP controls use a verified restricted
LOGIN without effective TEMPORARY or privileged memberships: empty/canonical
upgrades, genuine authority refusal with unchanged rows/catalog/history, mirrored
convergence and no helper residue. Existing backup roundtrip, erasure
schema coverage and cross-chain convergence suites remain required regressions.
Follow [contribution test-database rules](../CONTRIBUTING.md#database-tests) and
[controller build rules](../AGENTS.md#controller-builds); never test or migrate
against staging or production for this foundation repair.
