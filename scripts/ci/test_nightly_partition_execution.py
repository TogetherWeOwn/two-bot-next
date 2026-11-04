"""Exercise the production planner/runner with offline Cargo/libtest executables."""
import contextlib
import importlib.util
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("nightly_execution", ROOT / ".github/scripts/nightly_partition.py")
nightly = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(nightly)

# These are protocol fixtures, not alternate implementations of partitioning.
# The log records what really executes (including working directory/manifest),
# independently of the planner's claimed inventory and completion receipts.
BINARY = '''#!/usr/bin/env python3
import json, os, subprocess, sys
from pathlib import Path
config = json.loads(Path(__file__).with_suffix('.json').read_text())
names = config['names']
args = sys.argv[1:]
skips = [args[i+1] for i, value in enumerate(args) if value == '--skip']
selected = [name for name in names if not any(
    name == skip if '--exact' in args else skip in name for skip in skips)]
if '--list' in args:
    if os.environ.get('FAKE_UNKNOWN_LIST'):
        print('unrecognized harness output')
    else:
        for name in selected:
            print(name + ': test')
    sys.exit(0)
assert '--test-threads=1' in args and '--include-ignored' in args and '--exact' in args
with open(os.environ['FAKE_LOG'], 'a') as log:
    log.write(json.dumps({'target': config['target'], 'names': selected,
        'cwd': os.getcwd(), 'manifest': os.environ['CARGO_MANIFEST_DIR'],
        'results': args[args.index('--logfile') + 1] if '--logfile' in args else None}) + '\\n')
if os.environ.get('FAKE_FAIL_TARGET') == config['target']:
    print('test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured;')
    sys.exit(1)
for name in selected:
    annotation = ' - should panic' if name == 'rejects_input' else ''
    print('test ' + name + annotation + ' ... ', end='', flush=True)
    if os.environ.get('FAKE_CHILD_OUTPUT'):
        subprocess.run([sys.executable, '-I', '-c',
            'import sys; sys.stdout.write(sys.argv[1])', os.environ['FAKE_CHILD_OUTPUT']], check=True)
    print('ok')
if '--logfile' in args and os.environ.get('FAKE_MISSING_RESULTS') not in ['1', config['target']]:
    results = os.environ.get('FAKE_RESULTS', ''.join('ok ' + name + '\\n' for name in selected))
    Path(args[args.index('--logfile') + 1]).write_text(results)
print(os.environ.get('FAKE_SUMMARY', 'test result: ok. %d passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s' % len(selected)))
sys.exit(int(os.environ.get('FAKE_EXIT', '0')))
'''
CARGO = '''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
root = Path(os.environ['FAKE_ROOT'])
with open(root / 'cargo-log.jsonl', 'a') as log:
    log.write(json.dumps(sys.argv[1:]) + '\\n')
if sys.argv[1] == 'metadata':
    print((root / 'metadata.json').read_text())
elif '--no-run' in sys.argv:
    for artifact in json.loads((root / 'artifacts.json').read_text()):
        if artifact['target']['name'] != os.environ.get('FAKE_MISSING_TARGET'):
            print(json.dumps(artifact))
elif '--doc' in sys.argv:
    sys.exit(int(os.environ.get('FAKE_DOC_FAIL', '0')))
else:
    sys.exit('unknown Cargo invocation')
'''


class ExecutionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.log = self.root / 'executed.jsonl'
        self.env = dict(PATH=str(self.bin) + os.pathsep + os.environ['PATH'], FAKE_ROOT=str(self.root),
                        FAKE_LOG=str(self.log), GITHUB_SHA='fixture-sha')
        self.addCleanup(mock.patch.stopall)
        mock.patch.dict(os.environ, self.env).start()
        mock.patch.object(nightly, 'ROOT', self.root).start()
        cargo = self.bin / 'cargo'
        cargo.write_text(CARGO)
        cargo.chmod(0o755)
        packages, artifacts = [], []
        self.expected = set()
        for package, kind, target_name, names in [
            ('bot', 'bin', 'bot', [f'case_{i}' for i in range(24)] + ['nested::skip_me']),
            ('core', 'lib', 'core', ['case_0', 'example_with_unicode_é', 'rejects_input', 'nested::skip_me']),
            ('core', 'example', 'tested_example', ['example_test']),
            ('core', 'example', 'ordinary_example', None),
        ]:
            existing = next((p for p in packages if p['id'] == package), None)
            if existing is None:
                directory = self.root / package
                directory.mkdir()
                existing = {'name': package, 'id': package, 'manifest_path': str(directory / 'Cargo.toml'), 'targets': []}
                packages.append(existing)
            target = {'name': target_name, 'kind': [kind], 'test': names is not None}
            existing['targets'].append(target)
            executable = self.bin / target_name
            executable.write_text(BINARY)
            executable.chmod(0o755)
            key = f'{package}|{kind}|{target_name}'
            executable.with_suffix('.json').write_text(json.dumps({'target': key, 'names': names}))
            artifacts.append({'reason': 'compiler-artifact', 'package_id': package,
                              'target': target, 'profile': {'test': names is not None}, 'executable': str(executable)})
            if names:
                self.expected.update((key, name) for name in names if 'skip_me' not in name)
        metadata = {'packages': packages, 'workspace_members': ['bot', 'core'], 'target_directory': str(self.root / 'target')}
        (self.root / 'metadata.json').write_text(json.dumps(metadata))
        (self.root / 'artifacts.json').write_text(json.dumps(artifacts))
        self.args = ['--include-ignored', '--test-threads=1', '--skip', 'skip_me']
        self.plan_path = self.root / 'plan.json'

    def main(self, args):
        with mock.patch.object(sys, 'argv', ['nightly_partition.py'] + args):
            nightly.main()

    def plan(self):
        self.main(['plan', '--plan', str(self.plan_path), '--'] + self.args)
        return json.loads(self.plan_path.read_text())

    def test_actual_six_partition_execution_matches_original_inventory_once(self):
        plan = self.plan()
        receipts = self.root / 'receipts'
        for index in range(6):
            self.main(['run', '--plan', str(self.plan_path), '--partition', str(index),
                       '--receipt', str(receipts / f'partition-{index}.json')])
        executed = [json.loads(line) for line in self.log.read_text().splitlines()]
        actual = [(record['target'], name) for record in executed for name in record['names']]
        self.assertEqual(set(actual), self.expected)
        self.assertEqual(len(actual), len(self.expected))
        result_paths = [record['results'] for record in executed]
        self.assertEqual(len(set(result_paths)), len(result_paths))
        self.assertTrue(all(path and not Path(path).parent.exists() for path in result_paths))
        for record in executed:
            package = record['target'].split('|')[0]
            self.assertEqual(record['cwd'], str(self.root / package))
            self.assertEqual(record['manifest'], record['cwd'])
        needs = {job: {'result': 'success'} for job in nightly.JOBS}
        needs['changes']['outputs'] = {'rust': 'true'}
        nightly.aggregate(needs, plan, receipts, 'fixture-sha')
        commands = [json.loads(line) for line in (self.root / 'cargo-log.jsonl').read_text().splitlines()]
        builds = [cmd for cmd in commands if '--no-run' in cmd]
        self.assertEqual(len(builds), 7)
        for cmd in builds:
            self.assertIn('--workspace', cmd)
            self.assertIn('--all-features', cmd)
            self.assertIn('--locked', cmd)
            for narrowed in ['--lib', '--bins', '--tests', '--all-targets', '--examples']:
                self.assertNotIn(narrowed, cmd)
        self.assertIn('core|example|tested_example', plan['inventory'])
        self.assertNotIn('core|example|ordinary_example', plan['inventory'])

    def test_child_diagnostic_without_newline_does_not_corrupt_results(self):
        plan = self.plan()
        output = io.StringIO()
        with mock.patch.dict(os.environ, {'FAKE_CHILD_OUTPUT': 'child diagnostic'}), contextlib.redirect_stdout(output):
            receipt = self.root / 'receipt.json'
            nightly.run_partition(plan, 0, self.args, receipt, 'fixture-sha')
        self.assertIn(' ... child diagnosticok\n', output.getvalue())
        self.assertEqual(json.loads(receipt.read_text())['executed'], plan['partitions'][0])

    def test_child_bare_ok_does_not_complete_a_pending_result(self):
        plan = self.plan()
        output = io.StringIO()
        with mock.patch.dict(os.environ, {'FAKE_CHILD_OUTPUT': 'ok\n'}), contextlib.redirect_stdout(output):
            receipt = self.root / 'receipt.json'
            nightly.run_partition(plan, 0, self.args, receipt, 'fixture-sha')
        self.assertIn(' ... ok\nok\n', output.getvalue())
        self.assertEqual(json.loads(receipt.read_text())['executed'], plan['partitions'][0])

    def test_good_console_cannot_replace_bad_or_missing_isolated_evidence(self):
        plan = self.plan()
        records = ''.join('ok ' + name + '\n' for target, name in plan['partitions'][0] if target == 'bot|bin|bot')
        for env in [{'FAKE_MISSING_RESULTS': '1'},
                    {'FAKE_MISSING_RESULTS': 'core|example|tested_example'},
                    {'FAKE_RESULTS': ''}, {'FAKE_RESULTS': records + records},
                    {'FAKE_RESULTS': records.replace('ok ', 'failed ', 1)},
                    {'FAKE_RESULTS': records.replace('ok ', 'ignored ', 1)},
                    {'FAKE_RESULTS': records.replace('ok ', 'child diagnosticok ', 1)},
                    {'FAKE_RESULTS': records + 'ok unknown\n'},
                    {'FAKE_RESULTS': '\n'.join(records.splitlines()[1:])},
                    {'FAKE_SUMMARY': ''}, {'FAKE_EXIT': '1'}]:
            receipt = self.root / 'bad.json'
            with self.subTest(env=env), mock.patch.dict(os.environ, env), self.assertRaises(ValueError):
                nightly.run_partition(plan, 0, self.args, receipt, 'fixture-sha')
            self.assertFalse(receipt.exists())

    def test_failing_binary_never_writes_a_success_receipt(self):
        plan = self.plan()
        index = next(i for i, part in enumerate(plan['partitions']) if any(t == 'bot|bin|bot' for t, _ in part))
        receipt = self.root / 'receipt.json'
        with mock.patch.dict(os.environ, {'FAKE_FAIL_TARGET': 'bot|bin|bot'}), self.assertRaises(ValueError):
            nightly.run_partition(plan, index, self.args, receipt, 'fixture-sha')
        self.assertFalse(receipt.exists())

    def test_missing_target_and_unknown_harness_fail_discovery(self):
        for env in [{'FAKE_MISSING_TARGET': 'tested_example'}, {'FAKE_UNKNOWN_LIST': '1'}]:
            with self.subTest(env=env), mock.patch.dict(os.environ, env), self.assertRaises(ValueError):
                nightly.discover(self.args)

    def test_doctests_keep_workspace_features_serial_skips_and_failures(self):
        self.plan()
        self.main(['docs', '--plan', str(self.plan_path)])
        commands = [json.loads(line) for line in (self.root / 'cargo-log.jsonl').read_text().splitlines()]
        self.assertEqual(commands[-1], ['test', '--workspace', '--all-features', '--locked', '--doc', '--no-fail-fast', '--'] + self.args)
        with mock.patch.dict(os.environ, {'FAKE_DOC_FAIL': '1'}), self.assertRaises(subprocess.CalledProcessError):
            self.main(['docs', '--plan', str(self.plan_path)])

    def test_unknown_partition_and_drifted_local_inventory_fail_closed(self):
        plan = self.plan()
        with self.assertRaises(ValueError):
            nightly.run_partition(plan, 6, self.args, self.root / 'bad.json', 'fixture-sha')
        plan['unfiltered']['bot|bin|bot'].append('missing_test')
        with self.assertRaises(ValueError):
            nightly.run_partition(plan, 0, self.args, self.root / 'bad.json', 'fixture-sha')
        self.assertFalse((self.root / 'bad.json').exists())


if __name__ == '__main__':
    unittest.main()
