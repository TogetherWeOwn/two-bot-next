// Offline exporter preconditions: no legacy dependencies, network or DB required.
import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const exporter = fileURLToPath(new URL('./export-legacy-registry.mjs', import.meta.url));
const frozenRevision = 'd5d1179348feb9157bcac8c875de9399d4f5c76a';
const source = 'src/leveling/discord.ts';
const lockfile = 'package-lock.json';
const sentinel = 'existing golden must not be overwritten\n';

function fixture(t) {
  const scratch = mkdtempSync(join(process.env.PAPERCLIP_RUN_SCRATCH_DIR || tmpdir(), 'registry-export-'));
  t.after(() => rmSync(scratch, { recursive: true, force: true }));
  const clone = join(scratch, 'legacy');
  mkdirSync(clone);
  const git = (...args) => execFileSync('git', ['-C', clone, ...args], { encoding: 'utf8' }).trim();
  const write = (path, value) => {
    mkdirSync(dirname(join(clone, path)), { recursive: true });
    writeFileSync(join(clone, path), value);
  };
  git('init', '--quiet');
  write('.gitignore', 'node_modules/\n');
  write(source, "throw new Error('source must not be imported');\n");
  write(lockfile, '{}\n');
  git('add', '.');
  git('-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
    'commit', '--quiet', '-m', 'test: initial offline fixture');
  // Deliberately not the real frozen commit: dirty validation must run BEFORE
  // revision validation, dependency resolution and source imports.
  assert.notEqual(git('rev-parse', 'HEAD'), frozenRevision);
  const output = join(scratch, 'golden.json');
  writeFileSync(output, sentinel);
  const run = () => spawnSync(process.execPath, [exporter, clone, output], { encoding: 'utf8' });
  return { clone, output, git, write, run };
}

const dirtyCases = [
  ['unstaged source', f => f.write(source, '// modified rank description\n')],
  ['staged source', f => { f.write(source, '// staged drift\n'); f.git('add', source); }],
  ['deleted source', f => rmSync(join(f.clone, source))],
  ['unstaged lockfile', f => f.write(lockfile, '{"drift":true}\n')],
  ['staged lockfile', f => { f.write(lockfile, '{"drift":true}\n'); f.git('add', lockfile); }],
  ['untracked source', f => f.write('src/leveling/shadow.ts', '// untracked input\n')],
];
for (const [name, mutate] of dirtyCases) {
  test(`reject ${name} without touching the existing golden`, t => {
    const f = fixture(t);
    mutate(f);
    const result = f.run();
    assert.equal(result.status, 1, result.stderr);
    assert.match(result.stderr, /Refusing dirty legacy clone/);
    assert.doesNotMatch(result.stderr, /Expected docs\/parity.md frozen legacy revision/);
    assert.equal(readFileSync(f.output, 'utf8'), sentinel);
  });
}

test('dirty rejection does not create a new output', t => {
  const f = fixture(t);
  rmSync(f.output);
  f.write(source, '// dirty\n');
  const result = f.run();
  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stderr, /Refusing dirty legacy clone/);
  assert.equal(existsSync(f.output), false);
});

test('ignored npm dependencies pass the clean guard, but wrong HEAD still rejects', t => {
  const f = fixture(t);
  f.write('node_modules/discord.js/index.js', "throw new Error('dependency must not be loaded');\n");
  const result = f.run();
  assert.equal(result.status, 1, result.stderr);
  assert.match(result.stderr, /Expected docs\/parity.md frozen legacy revision/);
  assert.doesNotMatch(result.stderr, /Refusing dirty legacy clone/);
  assert.equal(readFileSync(f.output, 'utf8'), sentinel);
});
