# Legacy-versus-next data verification

`legacy_verify` independently compares two PostgreSQL databases using the shared
JSON v1 mapping in `crates/cutover/src/legacy_mapping.rs`. It never runs migrations,
copies rows, or offers `--apply`. Exit **0** means all selected mapped tables match;
**1** means a complete report contains differences; **2** means a refused mapping,
invalid arguments, connection/query failure, null key or duplicate normalized key.
An error is never treated as an empty table or a successful partial report.

## Mapping contract (shared with `legacy_copy`)

`crates/cutover/mappings/example.json` is a **synthetic fixture example**, not a
production cutover plan. Authoritative legacy DDL and approved table coverage
must be established by the copy-tool registry; this verifier does not invent it.
JSON is supported; YAML is not. Unknown fields are errors, not silently ignored.

```json
{
  "version": 1,
  "tables": [{
    "group": "members",
    "source": "public.legacy_members",
    "target": "public.next_members",
    "keys": ["id"],
    "conflict": ["member_id"],
    "conflict_policy": "upsert",
    "columns": [
      {"source": "id", "target": "member_id", "pg_type": "bigint"},
      {"source": "enabled", "target": "enabled", "pg_type": "boolean"},
      {"source": "joined_at", "target": "joined_at", "pg_type": "timestamptz"},
      {"source": "metadata", "target": "metadata", "pg_type": "jsonb"}
    ]
  }],
  "pending_groups": [{"group": "tickets", "reason": "next schema not merged"}]
}
```

- `source` / `target`: table or schema.table. Names are ASCII PostgreSQL identifiers
  (letters/underscore first, then letters/digits/underscore, maximum 63 bytes).
  Identifiers are quoted; arbitrary SQL expressions are not accepted.
- `columns`: explicit one-to-one source/target correspondence. `pg_type` is the
  cast applied on **both** sides before comparison: `text`, `bigint`, `integer`,
  `smallint`, `boolean`, `timestamptz`, `timestamp`, `date`, `jsonb`, `json`,
  `numeric`, `uuid`, or `bytea`. Unsupported conversions refuse the plan.
- `keys`: source primary-key tuple; `conflict`: corresponding target key tuple.
  Their order and lengths must match, and each pair must refer to the same mapped
  scalar column. This preserves source pagination keys versus target conflict
  keys for the copier. Null/duplicate normalized keys are errors even if the
  database's native constraint permits them (e.g. text IDs `1` and `01` cast to bigint).
  JSON keys are unsupported. Generated identities with different values require
  a genuine shared natural key, not a claim of primary-key equality.
- `conflict_policy`: `upsert` (default) or `insert_only` (append-only ledgers).
  Preserved for the copier; it cannot permit or change writes in the verifier.
  The copy tool owns sequence/trigger handling. Generated/revision columns not
  explicitly mapped are outside this report, not implicitly proven equal.
- `pending_groups`: unique names plus concrete reasons, with no ready tables
  under that same name. Selecting a pending or unknown group is a named refusal.
  With no `--group`, **all** groups are selected, so any pending group refuses.
  To verify the ready portion deliberately, repeat `--group <name>`.
- Version 1 has no copy-only extension bag: additional metadata requires a
  jointly documented contract update, not ignored unknown fields.

## Invocation and credential safety

Prefer environment-variable references so URLs are not present in process
arguments. Supply only credentials already authorized for this read purpose.
The tool never falls back to an application `DATABASE_URL`, `.pgpass`, or another
credential after failure. Inherited libpq connection settings (`PGHOST`, `PGUSER`,
`PGPASSWORD`, `PGOPTIONS`, client keys, etc.) are refused. Each explicit URL must
include host, username and database; omitted password means empty password.
Only the `sslmode` URL query parameter is accepted. Database errors and URLs are
not printed. Protect reports: samples contain data identifiers.

```sh
# SOURCE_READ_URL / TARGET_READ_URL must be supplied through authorized bindings.
# On the controller, compilation/testing always uses the bounded-cache wrapper.
python3 scripts/cargo_cache.py run -- build -p two-bot-cutover --bin legacy_verify
# Run the built executable from the admitted slot identified by the wrapper:
<admitted-slot>/target/debug/legacy_verify \
  --source-url-env SOURCE_READ_URL --target-url-env TARGET_READ_URL \
  --mapping <approved-mapping.json> --group members --sample-limit 10 \
  > <run-scratch>/verification.json
```

Read access does **not** authorize execution against real databases for tests.
All acceptance tests use only the disposable test service below. Operational
cutover execution requires its own authorized endpoint/credential context.

## What the report proves

Each table reports source/target row counts, exact missing/extra key counts,
deterministic sampled key tuples, and one SHA-256 checksum per mapped column.
`--sample-limit` is 0–1000 (default 10); zero omits identifier samples but still
computes exact differences. Counts include every row, not only the samples.

- Separate repeatable-read transactions begin with
  `SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY` on **both** sides.
  Session-local timezone is UTC, DateStyle ISO/YMD, and bytea format hex.
  Both transactions explicitly roll back on success; errors also roll back.
- Timestamptz casts preserve microseconds and normalize instants to UTC (also
  preserving PostgreSQL infinity values). Plain `timestamp` is wall-clock time,
  not an invented timezone. Boolean casts normalize PostgreSQL boolean spellings;
  invalid values refuse rather than being coerced to false.
- JSON casts discard insignificant whitespace/object insertion order. Recursive
  canonicalization sorts object keys, preserves array order and string contents,
  and canonicalizes decimal numbers **without floating-point precision loss**.
  Literal objects, including serde_json's private marker keys, remain objects,
  never scalar numbers. Nesting beyond 128 levels is refused.
  Numeric columns remove insignificant trailing zeroes. TEXT remains byte-exact;
  choose JSON normalization deliberately for legacy JSON stored as text.
- SQL NULL, JSON null, the string `"null"`, and an empty string remain distinct.
- A server cursor fetches at most 1024 decoded rows at once. Rows are ordered by
  normalized key components under the C collation, independent of native ID type,
  collation or insertion order. Each checksum includes length-framed key tuples
  and values, so swapping values between IDs is a mismatch, not a false PASS.
- Exact key sets require **O(number of keys)** client memory; this is not a
  constant-memory verifier. Values are hashed online, not retained in full.
  Each statement has a 30-second timeout; database sorting may use server resources.

This is not a distributed atomic snapshot: independent source/target databases
cannot share a transaction snapshot. Freeze copy/application writes before final
cutover evidence. Equality applies only to the mapped projection; unmapped
columns, unmapped tables, constraints, sequences and triggers are not certified.
A checksum mismatch identifies a column, not individual changed cells. Missing
and extra rows affect all keyed checksums, so consult the key report first.

## Acceptance (test containers only)

The integration test reuses the repository's exact test-endpoint guard:
`postgres://agent_test@agent-testdb:5432/agent_test`, or the exact loopback variant
only when `GITHUB_ACTIONS=true`. It builds empty-password connection options
without `.pgpass`; app URLs and URL overrides are rejected before networking.
It creates two unique scratch **databases**, seeds synthetic source text versus
target native types, and drops only those databases (also after assertion panics).
The `check` workflow runs it explicitly against its disposable Postgres service.

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --lib legacy_
python3 scripts/cargo_cache.py run -- test -p two-bot-cutover --bin legacy_verify
TWO_TEST_DATABASE_URL=postgres://agent_test@agent-testdb:5432/agent_test \
  python3 scripts/cargo_cache.py run -- test -p two-bot-cutover \
  --test legacy_verify_db -- --ignored
python3 scripts/cargo_cache.py run -- clippy -p two-bot-cutover --all-targets -- -D warnings
```

| Given / when | Required observable result |
| --- | --- |
| Identical mapped data with different booleans, timestamp offsets and JSON layout | All checksums equal; 3 rows per side; CLI exit 0 |
| JSON marker object versus scalar, including nested arrays, mapped as json and jsonb | Equal counts/keys; only metadata mismatches; jsonb CLI exit 1 |
| Equal numeric/nonnumeric marker objects, including nested arrays, mapped as json and jsonb | All checksums equal; jsonb CLI exit 0 |
| Target lacks key 2 | One missing key, sample `["2"]`; target count 2; CLI exit 1 |
| Target changes only display_name | Equal keys/counts; only that column mismatches; CLI exit 1 |
| Target adds key 4 | One extra key, sample `["4"]`; target count 4; CLI exit 1 |
| Target replaces a row but preserves its count | Missing and extra key reports both nonzero |
| Write attempted inside each verifier transaction | PostgreSQL SQLSTATE 25006 on both endpoints |
| Source text keys 1 and 01 normalize to the same key | Named duplicate-key refusal; CLI exit 2 |
| 1104 rows inserted in different orders | Equality across the 1024-row cursor boundary |
| Empty mapped tables | Zero counts, equal empty checksums, PASS |
| Arbitrary database URLs or SQL-like mapping names/casts | Refused before database reads |

Permission/network failures are hard failures, never retries with other credentials.
The tests do not establish a maximum production dataset or performance SLA; measure
that separately on approved synthetic test data. No Discord, Redis, production or
staging database is needed for this feature.
