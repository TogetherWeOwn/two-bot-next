"""Standing gate: token-bearing script readers must never follow redirects.

Every script in READERS sends a credential (a Discord bot token, a GitHub
Bearer token, or a Cloudflare API token) over urllib. The stdlib opener
follows 301/302/303/307/308 by default and replays the request headers on
the follow-up, so a bare ``urllib.request.urlopen`` with an Authorization
header -- or a ``build_opener`` without a refusing redirect handler -- leaks
the token to the redirect target. The fixed readers route every authenticated
request through an opener built with a redirect-refusing handler (a
``HTTPRedirectHandler`` subclass whose redirect methods return None or
raise), which turns a 3xx into an error before any second destination is
contacted.

The gate parses each reader with ``ast`` (no imports, no network, no DB) and
fails when a token-bearing file keeps a redirect-following call site:

- any ``urlopen(...)`` call in a file that also sends Authorization, or
- any ``build_opener(...)`` without a refusing handler class in the file, or
- any ``requests.*`` call with Authorization and without
  ``allow_redirects=False`` (no current reader uses requests; future-proof).

Five entries in KNOWN_GAPS are still vulnerable and owned by in-flight
sibling fixes (the alert-history reader and the four voice-synthetic token
readers). They are allowlisted by (file, function) so the gate stays green
while those fixes land; the gate fails on any violation outside the list AND
on any stale entry whose function is already clean, so each sibling fix must
drop its lines and the list shrinks to empty -- the strict end state. Reasons
stay in plain words: this repo is public and tracker ids never enter code.
"""

import ast
import unittest
from pathlib import Path
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = ROOT / "scripts"

# Every script that sends a credential over urllib. Unauthenticated readers
# intentionally stay listed: if one ever gains an Authorization header the
# gate starts checking its call sites instead of silently passing it.
READERS = (
    "synthetic_alert_history.py",
    "staging_voice_synthetic.py",
    "staging_rollout.py",
    "cutover_freeze_drill.py",
    "container_stdout_read.py",
    "prod_passive_probe.py",
    "qa_cutover_probes.py",
)

# (file, function) pairs whose bare-urlopen-with-token violation is still
# open under a sibling fix. Remove a line with the fix that cleans it; an
# entry that no longer matches a live violation fails the gate as stale.
KNOWN_GAPS = frozenset({
    ("synthetic_alert_history.py", "_get"),
    ("staging_voice_synthetic.py", "metrics_snapshot"),
    ("staging_voice_synthetic.py", "rest"),
    ("staging_voice_synthetic.py", "rest_patch"),
    ("staging_voice_synthetic.py", "metrics_value"),
})

REDIRECT_METHODS = frozenset({
    "redirect_request",
    "http_error_301",
    "http_error_302",
    "http_error_303",
    "http_error_307",
    "http_error_308",
})

REQUEST_FUNCS = frozenset({"get", "post", "put", "patch", "delete", "head", "request"})


def _is_redirect_base(base):
    return ((isinstance(base, ast.Name) and base.id == "HTTPRedirectHandler")
            or (isinstance(base, ast.Attribute) and base.attr == "HTTPRedirectHandler"))


def _method_refuses(node):
    """A redirect method refuses when it returns None or raises."""
    for child in ast.walk(node):
        if isinstance(child, ast.Raise):
            return True
        if isinstance(child, ast.Return):
            value = child.value
            if value is None or (isinstance(value, ast.Constant) and value.value is None):
                return True
    return False


def find_violations(path):
    """(file, function, line) triples for redirect-following token paths."""
    text = Path(path).read_text()
    tree = ast.parse(text)
    file_name = Path(path).name
    sends_token = "Authorization" in text

    refusing_handler = set()
    for node in ast.walk(tree):
        if not isinstance(node, ast.ClassDef):
            continue
        if not any(_is_redirect_base(base) for base in node.bases):
            continue
        for item in node.body:
            if (isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef))
                    and item.name in REDIRECT_METHODS and _method_refuses(item)):
                refusing_handler.add(node.name)
                break

    urlopen_sites = []  # (function, line)
    opener_sites = []
    requests_sites = []
    stack = []

    def visit(node, func):
        nonlocal stack
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            stack.append(node.name)
            current = node.name
        else:
            current = func
        for child in ast.iter_child_nodes(node):
            if isinstance(child, ast.Call):
                target = child.func
                if ((isinstance(target, ast.Attribute) and target.attr == "urlopen")
                        or (isinstance(target, ast.Name) and target.id == "urlopen")):
                    urlopen_sites.append((current or "<module>", child.lineno))
                elif ((isinstance(target, ast.Attribute) and target.attr == "build_opener")
                        or (isinstance(target, ast.Name) and target.id == "build_opener")):
                    opener_sites.append((current or "<module>", child.lineno))
                elif (isinstance(target, ast.Attribute) and target.attr in REQUEST_FUNCS
                        and isinstance(target.value, ast.Name)
                        and target.value.id == "requests"):
                    keywords = {kw.arg for kw in child.keywords if kw.arg}
                    values = {kw.arg: kw.value for kw in child.keywords if kw.arg}
                    redirect_off = False
                    allow = values.get("allow_redirects")
                    if isinstance(allow, ast.Constant) and allow.value is False:
                        redirect_off = True
                    if "allow_redirects" not in keywords or not redirect_off:
                        requests_sites.append((current or "<module>", child.lineno))
            visit(child, current)
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            stack.pop()

    visit(tree, "<module>")

    violations = []
    if sends_token:
        for func, line in urlopen_sites:
            violations.append((file_name, func, line))
        if opener_sites and not refusing_handler:
            violations.extend((file_name, func, line) for func, line in opener_sites)
        violations.extend((file_name, func, line) for func, line in requests_sites)
    return violations


def check_tree(scripts_dir=SCRIPTS, readers=READERS, known_gaps=KNOWN_GAPS):
    """Split live violations into (remaining, stale) against the allowlist."""
    live = []
    for name in readers:
        live.extend(find_violations(scripts_dir / name))
    live_keys = {(file_name, func) for file_name, func, _line in live}
    remaining = [item for item in live if (item[0], item[1]) not in known_gaps]
    stale = sorted(gap for gap in known_gaps if gap not in live_keys)
    return remaining, stale


class TokenRedirectGateTests(unittest.TestCase):
    def test_readers_list_is_complete_and_present(self):
        for name in READERS:
            self.assertTrue((SCRIPTS / name).is_file(), name)
        self.assertEqual(len(READERS), 7)

    def test_no_unallowlisted_token_path_can_follow_a_redirect(self):
        remaining, stale = check_tree()
        self.assertEqual(
            remaining, [],
            "token-bearing redirect-following call sites outside the allowlist: "
            + ", ".join(f"{f}:{f_}:{line}" for f, f_, line in remaining))
        self.assertEqual(
            stale, [],
            "allowlist entries already fixed upstream; drop these lines: "
            + ", ".join(f"{f}:{f_}" for f, f_ in stale))

    def test_known_gaps_are_exactly_the_sibling_owned_violations(self):
        live = check_tree()[0] + [item for item in sum(
            (find_violations(SCRIPTS / name) for name in READERS), [])
            if (item[0], item[1]) in KNOWN_GAPS]
        live_keys = {(file_name, func) for file_name, func, _line in live}
        self.assertEqual(live_keys, set(KNOWN_GAPS))

    def test_bare_urlopen_with_token_trips_the_gate(self):
        """Scratch-mutant proof: re-adding urlopen-with-token fails the gate."""
        mutant = (
            "import urllib.request\n"
            "def _get(path, token):\n"
            "    request = urllib.request.Request(\n"
            "        'https://api.example' + path,\n"
            "        headers={'Authorization': 'Bearer ' + token})\n"
            "    with urllib.request.urlopen(request, timeout=10) as response:\n"
            "        return response.read()\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            mutant_path = Path(tmp) / "mutant_reader.py"
            mutant_path.write_text(mutant)
            violations = find_violations(mutant_path)
        self.assertEqual(len(violations), 1)
        self.assertEqual(violations[0][1], "_get")

    def test_no_redirect_opener_passes_the_gate(self):
        fixed = (
            "import urllib.request\n"
            "class _NoRedirect(urllib.request.HTTPRedirectHandler):\n"
            "    def redirect_request(self, *args, **kwargs):\n"
            "        return None\n"
            "def _get(path, token):\n"
            "    opener = urllib.request.build_opener(_NoRedirect)\n"
            "    request = urllib.request.Request(\n"
            "        'https://api.example' + path,\n"
            "        headers={'Authorization': 'Bearer ' + token})\n"
            "    with opener.open(request, timeout=10) as response:\n"
            "        return response.read()\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            fixed_path = Path(tmp) / "fixed_reader.py"
            fixed_path.write_text(fixed)
            self.assertEqual(find_violations(fixed_path), [])

    def test_opener_without_a_refusing_handler_trips_the_gate(self):
        mutant = (
            "import urllib.request\n"
            "def _get(path, token):\n"
            "    opener = urllib.request.build_opener()\n"
            "    request = urllib.request.Request(\n"
            "        'https://api.example' + path,\n"
            "        headers={'Authorization': 'Bearer ' + token})\n"
            "    with opener.open(request, timeout=10) as response:\n"
            "        return response.read()\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            mutant_path = Path(tmp) / "opener_mutant.py"
            mutant_path.write_text(mutant)
            violations = find_violations(mutant_path)
        self.assertEqual(len(violations), 1)

    def test_unauthenticated_reader_with_a_safe_opener_passes(self):
        plain = (
            "import urllib.request\n"
            "class _NoRedirect(urllib.request.HTTPRedirectHandler):\n"
            "    def redirect_request(self, *args, **kwargs):\n"
            "        return None\n"
            "def fetch(url):\n"
            "    opener = urllib.request.build_opener(_NoRedirect)\n"
            "    with opener.open(urllib.request.Request(url), timeout=10) as r:\n"
            "        return r.status, r.read()\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            plain_path = Path(tmp) / "plain_reader.py"
            plain_path.write_text(plain)
            self.assertEqual(find_violations(plain_path), [])

    def test_stale_allowlist_entry_is_reported(self):
        with tempfile.TemporaryDirectory() as tmp:
            scripts = Path(tmp)
            (scripts / "synthetic_alert_history.py").write_text(
                "import urllib.request\n"
                "class _NoRedirect(urllib.request.HTTPRedirectHandler):\n"
                "    def redirect_request(self, *a, **k):\n"
                "        return None\n"
                "def _get(path, token):\n"
                "    opener = urllib.request.build_opener(_NoRedirect)\n"
                "    req = urllib.request.Request('https://x' + path,\n"
                "        headers={'Authorization': 'Bearer ' + token})\n"
                "    with opener.open(req, timeout=10) as r:\n"
                "        return r.read()\n")
            for name in READERS:
                if name != "synthetic_alert_history.py":
                    (scripts / name).write_text("x = 1\n")
            remaining, stale = check_tree(
                scripts_dir=scripts, readers=READERS, known_gaps=KNOWN_GAPS)
        self.assertEqual(remaining, [])
        self.assertIn(("synthetic_alert_history.py", "_get"), stale)


if __name__ == "__main__":
    unittest.main()
