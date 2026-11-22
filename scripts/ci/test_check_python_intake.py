"""Bad and good fixtures for the Python intake ratchet."""

import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location(
    "check_python_intake", Path(__file__).with_name("check-python-intake.py")
)
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
                                              "scripts/read.py")), [(8, "json.loads")])

    def test_git_range_scopes_files_and_preserves_existing_backlog(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.git(root, "init", "-q")
            (root / "scripts").mkdir()
            (root / ".github" / "scripts").mkdir(parents=True)
            raw = "import json\njson.loads(data)\n"
            existing = root / "scripts" / "existing.py"
            existing.write_text(raw)
            self.commit(root)
            base = self.git(root, "rev-parse", "HEAD").strip()
            existing.write_text("# Move the line without changing the call.\n" + raw)
            (root / guard.INTAKE).write_text(raw)
            (root / "scripts" / "test_fixture.py").write_text(raw)
            (root / ".github/scripts/new.py").write_text(raw)
            self.commit(root)
            self.assertEqual(list(guard.check(root, base, "HEAD")),
                             [(".github/scripts/new.py", 2, "json.loads")])

    @staticmethod
    def git(root, *args):
        return subprocess.check_output(["git", *args], cwd=root, text=True)

    def commit(self, root):
        self.git(root, "add", ".")
        self.git(root, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.org",
                 "-c", "commit.gpgsign=false", "commit", "-qm", "fixture")
