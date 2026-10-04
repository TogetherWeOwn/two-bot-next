# Voice fixtures

`voice_conditions_golden.json` is the V6b independent golden corpus over the
`voice_conditions` evaluator ([TOG-12468](/TOG/issues/TOG-12468)): 90 oracle
rows authored from `docs/voice-rooms.md` §V6 plus the legacy two-bot tempVoice
runtime state each condition head reads. 26 contexts are copied verbatim from
`tests/voice_templates/corpus.json` and 1 is derived (`v6b-party-capped`,
`party-4` with a full advertised party); the Rust gate asserts both. Rows with
`basis: "spec"` are determinate from §V6 alone and cite only §V6; rows with
`basis: "choice"` pin a choice TOG-12189 documents in
`docs/voice-conditions-core.md` where §V6 alone does not decide the outcome,
cite that document's line, and name the `tests/voice_templates/coverage.json`
ambiguity they settle, if any. The Rust gate asserts both citation forms.
Regenerate with
`python3 crates/core/tests/fixtures/generate_voice_conditions_golden.py` from
the repository root (standard library only); the script asserts the spec
SHA-256 pin and cross-checks every `shared_case` row against the shared
corpus, so regeneration is verification. `voice_conditions_golden.rs`
structurally gates the fixture until the eval-wiring follow-up lands.

# Backup compatibility fixtures

`legacy-v3-native.ndjson` was emitted on 2026-09-30 by the **actual frozen writer**
[`two-bot/src/store/dump.ts @ d5d1179348feb9157bcac8c875de9399d4f5c76a`](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/src/store/dump.ts#L167-L233),
then gunzipped for readable review. Writer source SHA-256:
`af49e90870379a62a38245388031485bdde51add0271bdb7109c8a6a158999d8`.

Regenerate with the commands at the top of `generate-legacy-v3.mjs` (Node 24+;
`stripTypeScriptTypes` is experimental). The generator verifies the source hash
before executing it. `createdAt` changes on regeneration. Rust tests re-gzip the
checked-in bytes; CI needs neither Node nor access to the legacy repository.

The writer receives mocked driver results for two populated tables: `events`
and `join_risk_flags`, using frozen migration 0001/0009/0015 columns. This proves
BIGINT/INTEGER numbers, native booleans, timestamp strings, SQL null, and
JSON-looking TEXT survive inspect/restore. Empty tables deliberately have no
mock columns. **This is a minimal writer-format fixture, not a complete migrated
legacy database dump.** The separate 22-table round-trip fixture tests all table
paths. JSON/JSONB object/array/scalar conversions are separately synthetic tests;
none of the 22 frozen dump tables has a JSONB or PostgreSQL-array column.

Frozen driver overrides are recorded in
[`postgresDriver.ts:16-41`](https://github.com/TogetherWeOwn/two-bot/blob/d5d1179348feb9157bcac8c875de9399d4f5c76a/src/store/postgresDriver.ts#L16-L41):
INT8 becomes a JavaScript Number and TIMESTAMPTZ an ISO string. The writer emits
raw result cells without text casts. Unsupported native composite cells refuse
at restore rather than silently becoming SQL NULL. Legacy JSON `null` cannot
distinguish JSON null from SQL NULL; the legacy contract maps it to SQL NULL.

`pre-reservation-v4.ndjson` is a synthetic text-encoded compatibility archive
with the exact 83-table inventory from the Rust writer before reservation
persistence, plus one voice room row. Empty entries have no columns. It is not a production dump or an
execution of that historical binary: it pins the previous format/inventory
independently of the current `DUMP_TABLES`. Unit inspection and a migrated-schema
restore test prove that v4 recovery points still work, while v5 requires the new
reservation table and unrelated missing v4 tables still refuse.

`backup_schema_seed.sql` is the separate migration-backed complete-schema fixture for
`backup_schema_roundtrip.rs`. It seeds every active covered table, including
FK chains, durable dedupe/lease state, settings revision/audit history, gapped
serial IDs and difficult text/null/JSON/timestamp values. Those tests create
isolated databases with the real cutover migrations and compare every column
of every restored row against an independent fresh migrated destination. The
Rust harness adds a non-`id` ALWAYS identity with a custom start/increment to
exercise catalog sequence discovery beyond the current production serials.
This fixture does not replace the frozen-writer compatibility bytes above.

`aws-sigv4-worked-example-secret.txt` is AWS's public test vector, never a usable
credential. The full PUT signature is independently reconstructed with Python
`hashlib`/`hmac`, and the loopback S3 verifier rejects unordered SignedHeaders
before checking the HMAC (the old fake accepted a self-consistent wrong order).
