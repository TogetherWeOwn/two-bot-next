"""Bad and good fixtures for the Python intake ratchet."""

import importlib.util
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("check_python_intake", Path(__file__).with_name("check-python-intake.py"))
guard = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(guard)

class IntakeRatchetTests(unittest.TestCase):
    def test_raw_uses_include_aliases_and_indirect_references(self):
        source = """\
import json as wire
import urllib.request as http
from urllib import request as transport
from json import load as read
wire.loads(
    payload,
)
recv = http.urlopen
transport.build_opener()
read(stream)
getattr(wire, "loads")(payload)
wire.JSONDecoder().decode(payload)
http.urlretrieve(url)
def helper():
    import harmless as wire
"""
        self.assertEqual(list(guard.additions("", source, "scripts/read.py")), [
            (4, "json.load"),
            (5, "json.loads"),
            (8, "urllib.request.urlopen"),
            (9, "urllib.request.build_opener"),
            (10, "json.load"),
            (11, "json.loads"),
            (12, "json.JSONDecoder"),
            (13, "urllib.request.urlretrieve"),
        ])

    def test_existing_uses_stay_at_baseline_and_safe_calls_pass(self):
        before = "import json\nold = json.loads(data)\n"
        after = """\
from script_intake import decode_json, no_redirect_opener
import json
old = json.loads(data)
new = decode_json(payload)
opener = no_redirect_opener()
payload = json.dumps(new)
"""
        self.assertEqual(list(guard.additions(before, after, "scripts/read.py")), [])
        self.assertEqual(list(guard.additions(before, after + "json.loads(data)\n",
                                              "scripts/read.py")), [(7, "json.loads")])

    def test_local_rename_keeps_one_baseline_use(self):
        before = """\
import urllib.request

def fetch():
    request = build_request()
    return urllib.request.urlopen(request, timeout=10)
"""
        renamed = before.replace("request =", "probe_request =").replace(
            "urlopen(request", "urlopen(probe_request")
        path = "scripts/read.py"
        self.assertEqual(list(guard.additions(before, renamed, path)), [])
        added = renamed.replace("    return ", "    urllib.request.urlopen(probe_request, timeout=10)\n    return ")
        self.assertEqual(list(guard.additions(before, added, path)), [(6, "urllib.request.urlopen")])
        changed = renamed.replace("timeout=10", "timeout=20")
        self.assertEqual(list(guard.additions(before, changed, path)), [(5, "urllib.request.urlopen")])

    def test_git_range_warn_and_hard_modes(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.git(root, "init", "-q")
            (root / "scripts").mkdir()
            raw = "import json\njson.loads(data)\n"
            existing = root / "scripts" / "existing.py"
            existing.write_text(raw)
            self.commit(root)
            base = self.git(root, "rev-parse", "HEAD").strip()
            existing.rename(root / "scripts" / "renamed.py")
            (root / guard.INTAKE).write_text(raw.replace("data", "shared"))
            (root / "scripts" / "test_fixture.py").write_text(raw.replace("data", "fixture"))
            (root / "scripts" / "new.py").write_text(raw.replace("data", "new_data"))
            self.commit(root)
            self.assertEqual(list(guard.check(root, base)), [("scripts/new.py", 2, "json.loads")])
            with patch.object(guard, "ROOT", root), patch.object(sys, "argv", ["guard", base]):
                self.assertEqual(guard.main(), 0)
                sys.argv.append("--hard")
                self.assertEqual(guard.main(), 1)

    def git(self, root, *args):
        return subprocess.check_output(["git", *args], cwd=root, text=True)

    def commit(self, root):
        self.git(root, "add", ".")
        self.git(root, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.org",
                 "-c", "commit.gpgsign=false", "commit", "-qm", "fixture")

    def test_unreadable_python_does_not_hide_other_findings(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.git(root, "init", "-q")
            scripts = root / "scripts"
            scripts.mkdir()
            (scripts / "clean.py").write_text("value = 1\n")
            linked = scripts / "linked.py"
            linked.write_text("value = 2\n")
            self.commit(root)
            base = self.git(root, "rev-parse", "HEAD").strip()
            (scripts / "bad.py").write_text("def syntax_error(\n")
            linked.unlink()
            linked.symlink_to("clean.py")
            (scripts / "new.py").write_text("import json\njson.loads(raw)\n")
            self.commit(root)
            self.assertEqual(list(guard.check(root, base)), [
                ("scripts/bad.py", 1, None), ("scripts/linked.py", 1, None),
                ("scripts/new.py", 2, "json.loads"),
            ])

    def test_actions_warning_escapes_path_properties(self):
        output = StringIO()
        with patch.dict(guard.os.environ, {"GITHUB_ACTIONS": "true"}), redirect_stdout(output):
            guard.report("synthetic diagnostic", hard=False, path="scripts/a,\x1bb.py", line=2)
            guard.report("synthetic error", hard=True)
        self.assertEqual(output.getvalue(),
                         "::warning file=scripts/a%2C\\x1bb.py,line=2::synthetic diagnostic\n"
                         "::error::synthetic error\n")
