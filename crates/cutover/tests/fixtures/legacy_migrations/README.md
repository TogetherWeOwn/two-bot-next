# Frozen legacy Postgres DDL

These 54 SQL files are verbatim from `TogetherWeOwn/two-bot` revision
`96777468472f23a02a1e97a43ffab3912fe5df2a`, `migrations/0001–0042`:

https://github.com/TogetherWeOwn/two-bot/tree/96777468472f23a02a1e97a43ffab3912fe5df2a/migrations

Retrieved through the authorized GitHub company-bot broker. Numeric prefixes
repeat (for example 0010), and some numbers are absent in the source. Do not
renumber them, fabricate missing files or run this set through SQLx's numeric
migration tracker. The copy acceptance test applies all `.sql` files in lexical
full-filename order to its own **disposable source database only**. This is fixture
DDL, not a runtime migration path, cutover approval or permission to access real
databases. Changes here must re-pin provenance and update acceptance assertions.

Synthetic rows are separate in `../legacy_copy_seed.sql`; no source data or
credentials were extracted.
