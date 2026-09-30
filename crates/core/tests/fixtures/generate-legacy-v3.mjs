// Run with Node 24+, an exact frozen dump.ts export and a scratch output path.
// gh api repos/TogetherWeOwn/two-bot/contents/src/store/dump.ts?ref=d5d1179348feb9157bcac8c875de9399d4f5c76a \
//   -H 'Accept: application/vnd.github.raw+json' > "$PAPERCLIP_RUN_SCRATCH_DIR/legacy-dump.ts"
// node crates/core/tests/fixtures/generate-legacy-v3.mjs "$PAPERCLIP_RUN_SCRATCH_DIR/legacy-dump.ts" "$PAPERCLIP_RUN_SCRATCH_DIR/legacy-v3.gz"
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { stripTypeScriptTypes } from 'node:module';
import { gunzipSync } from 'node:zlib';

const [sourcePath, scratchOutput] = process.argv.slice(2);
if (!sourcePath || !scratchOutput) throw new Error('expected frozen source path and scratch gzip path');
const source = readFileSync(sourcePath, 'utf8');
if (createHash('sha256').update(source).digest('hex') !== 'af49e90870379a62a38245388031485bdde51add0271bdb7109c8a6a158999d8') {
  throw new Error('source is not the frozen legacy writer');
}
const js = stripTypeScriptTypes(source, { mode: 'strip' });
const { dump } = await import(`data:text/javascript;base64,${Buffer.from(js).toString('base64')}`);

// Mock driver results, matching frozen migrations 0001/0009/0015. Empty tables
// intentionally have no columns: this is a minimal writer-format fixture, not
// a complete migration/schema fixture. Never connects to a database or Discord.
const rows = {
  events: [{
    id: 1, event_type: 'member_join', member_id: null, guild_id: 'fixture-guild',
    occurred_at: '2026-08-01T12:00:00.000Z', recorded_at: '2026-08-01T12:00:00.000Z',
    source: 'discord', metadata: '{"channelId":"c1"}', idempotency_key: 'fixture-event-1',
  }],
  join_risk_flags: [{
    event_id: 'fixture-risk-1', guild_id: 'fixture-guild', member_id: 'fixture-member',
    account_created_at: '2026-08-01T11:59:00.000Z', joined_at: '2026-08-01T12:00:00.000Z',
    source: 'unknown', score: 3, reasons_json: '["new account"]', bulk_join_window: false,
    flagged: true, created_at: '2026-08-01T12:00:00.000Z',
  }],
};
const mock = {
  async transaction(fn) { return fn(this); },
  async exec(sql) {
    if (sql !== 'SET TRANSACTION ISOLATION LEVEL REPEATABLE READ') throw new Error(sql);
  },
  prepare(sql) {
    return {
      async all(...args) {
        if (sql.includes('information_schema.columns')) {
          return Object.keys(rows[args[0]]?.[0] ?? {}).map(column_name => ({ column_name }));
        }
        if (sql === 'SELECT id FROM schema_migrations ORDER BY id') {
          return ['0001_initial', '0009_timestamptz_and_boolean', '0015_anti_nuke_containment'].map(id => ({ id }));
        }
        const table = sql.match(/ FROM (\w+) ORDER BY .* LIMIT \? OFFSET \?$/)?.[1];
        if (!table) throw new Error(sql);
        const [batch, offset] = args;
        return (rows[table] ?? []).slice(offset, offset + batch);
      },
      async get() {
        if (sql === 'SELECT COALESCE(MAX(id), 0) AS n FROM events') return { n: 1 };
        const table = sql.match(/^SELECT COUNT\(\*\) AS n FROM (\w+)$/)?.[1];
        if (!table) throw new Error(sql);
        return { n: (rows[table] ?? []).length };
      },
    };
  },
};
await dump(mock, scratchOutput);
writeFileSync(new URL('./legacy-v3-native.ndjson', import.meta.url), gunzipSync(readFileSync(scratchOutput)));
