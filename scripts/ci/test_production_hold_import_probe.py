"""Fresh-process audit probe shared by production hold tests."""
import json
import subprocess
import sys
from pathlib import Path


def assert_inert_import(test, target, absent, loaded):
    scripts = str(Path(__file__).resolve().parents[1])
    probe = r"""
import importlib, json, os, socket, subprocess, sys
target, absent, loaded = json.loads(sys.argv[3])
sys.path.insert(0, sys.argv[1])
assert all(name not in sys.modules for name in absent)
violations = []
def guard(event, args):
    mutation = event in {
        'os.remove', 'os.rename', 'os.mkdir', 'os.rmdir', 'os.chdir',
        'os.chmod', 'os.chown', 'os.link', 'os.symlink', 'os.truncate',
        'os.putenv', 'os.unsetenv', 'os.system', 'os.exec', 'os.spawn',
        'subprocess.Popen',
    } or event.startswith('socket.')
    if event == 'open':
        mode, flags = args[1], args[2]
        mutation = ((isinstance(mode, str) and any(c in mode for c in 'wax+'))
                    or (isinstance(flags, int) and bool(flags & (
                        os.O_WRONLY | os.O_RDWR | os.O_CREAT | os.O_TRUNC | os.O_APPEND))))
    if mutation:
        violations.append(event)
        raise RuntimeError('blocked side effect')
sys.addaudithook(guard)
if sys.argv[2] == 'negative':
    actions = [lambda: open(os.devnull, 'w'), lambda: socket.socket(),
               lambda: subprocess.Popen(['unused']), lambda: os.putenv('PROBE', '1')]
    for action in actions:
        try:
            action()
        except RuntimeError:
            pass
        else:
            raise AssertionError('audit guard did not block negative control')
    assert len(violations) == len(actions), violations
else:
    importlib.import_module(target)
    assert all(name in sys.modules for name in loaded)
    assert not violations, violations
"""
    for mode in ("negative", "import"):
        with test.subTest(mode=mode):
            result = subprocess.run(
                [sys.executable, "-I", "-B", "-c", probe, scripts, mode,
                 json.dumps([target, absent, loaded])],
                env={}, capture_output=True, text=True, timeout=10,
            )
            test.assertEqual(result.returncode, 0, result.stderr)
            test.assertEqual(result.stdout, "")
            test.assertEqual(result.stderr, "")
