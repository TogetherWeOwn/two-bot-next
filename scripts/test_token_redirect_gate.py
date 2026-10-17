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

The gate parses each file with ``ast`` (no imports, no network, no DB) and
fails when a token-bearing file keeps a redirect-following call site:

- any ``urlopen(...)`` call in a file that also sends Authorization
  (``urlopen`` follows redirects with no opt-out), or
- any ``build_opener(...)`` call whose own arguments name no refusing
  handler (a dead handler class elsewhere in the file excuses nothing), or
- any ``requests`` call -- ``requests.<verb>``, an ``import requests as`` alias,
  a ``from requests import`` verb, or a method on a session receiver (any
  factory spelling, subclass, or inline construction) -- without
  ``allow_redirects=False`` (no current reader uses requests; future-proof).

A handler counts as refusing only when a redirect method in it raises or
returns None on every path: no ``super()`` call, no delegation to another
redirect method, no non-None return. Conditional refusal that still follows
some statuses does not count.

Coverage is not just the READERS list: a sweep test asserts every
``scripts/*.py`` file that mentions Authorization is violation-free unless it
is a READER covered by the allowlist test, so a new token-sending script (or
a new token path in an old one) trips the gate even before anyone adds it to
READERS.

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


def _returns_none(value):
    return value is None or (isinstance(value, ast.Constant) and value.value is None)


def _method_refuses(node):
    """A redirect method refuses only when it never follows.

    Any ``super()`` call, any call delegating to another redirect method, or
    any non-None return disqualifies it: a handler that refuses some statuses
    while following others still leaks the token.
    """
    refuses = False
    for child in ast.walk(node):
        if isinstance(child, ast.Call):
            target = child.func
            if isinstance(target, ast.Name) and target.id == "super":
                return False
            if isinstance(target, ast.Attribute) and target.attr in REDIRECT_METHODS:
                return False
        if isinstance(child, ast.Raise):
            refuses = True
        if isinstance(child, ast.Return):
            if _returns_none(child.value):
                refuses = True
            else:
                return False
    return refuses


def _request_aliases(tree):
    """Names bound to the requests module, its verbs, and its Session."""
    modules = {"requests"}
    verbs = set()
    session = {"Session"}
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            for alias in node.names:
                if alias.name == "requests":
                    modules.add(alias.asname or "requests")
        elif isinstance(node, ast.ImportFrom) and node.module == "requests":
            for alias in node.names:
                bound = alias.asname or alias.name
                if alias.name.lower() == "session":
                    session.add(bound)
                else:
                    verbs.add(bound)
    # Subclasses of requests.Session are session makers too.
    for node in ast.walk(tree):
        if not isinstance(node, ast.ClassDef):
            continue
        for base in node.bases:
            if ((isinstance(base, ast.Attribute) and base.attr.lower() == "session"
                 and isinstance(base.value, ast.Name) and base.value.id in modules)
                    or (isinstance(base, ast.Name) and base.id in session)):
                session.add(node.name)
                break
    return modules, verbs, session


def _redirects_allowed(call):
    """Whether a requests call explicitly opts out of redirects."""
    for keyword in call.keywords:
        if keyword.arg == "allow_redirects":
            return not (isinstance(keyword.value, ast.Constant)
                        and keyword.value.value is False)
    return True


def find_violations(path):
    """(file, function, line) triples for redirect-following token paths."""
    text = Path(path).read_text()
    tree = ast.parse(text)
    file_name = Path(path).name
    sends_token = "Authorization" in text
    if not sends_token:
        return []

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

    modules, verbs, sessions = _request_aliases(tree)

    def _is_session_maker(func):
        return ((isinstance(func, ast.Attribute) and func.attr.lower() == "session"
                 and isinstance(func.value, ast.Name) and func.value.id in modules)
                or (isinstance(func, ast.Name) and func.id in sessions))

    # Names in each function scope bound to a session construction result, so
    # method calls on them are checked like direct requests calls.
    session_bound = set()
    for node in ast.walk(tree):
        if not isinstance(node, (ast.Assign, ast.AnnAssign, ast.NamedExpr)):
            continue
        targets = []
        if isinstance(node, ast.Assign):
            targets = list(node.targets)
        elif node.value is not None:
            targets = [node.target]
        if not isinstance(node.value, ast.Call):
            continue
        if not _is_session_maker(node.value.func):
            continue
        for target in targets:
            if isinstance(target, ast.Name):
                session_bound.add(target.id)

    violations = []

    def visit(node, func):
        current = func
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
            current = node.name
        for child in ast.iter_child_nodes(node):
            if isinstance(child, ast.Call):
                target = child.func
                site = (current or "<module>", child.lineno)
                if ((isinstance(target, ast.Attribute) and target.attr == "urlopen")
                        or (isinstance(target, ast.Name) and target.id == "urlopen")):
                    violations.append((file_name, site[0], site[1]))
                elif ((isinstance(target, ast.Attribute) and target.attr == "build_opener")
                        or (isinstance(target, ast.Name) and target.id == "build_opener")):
                    names = set()
                    for descendant in ast.walk(child):
                        if isinstance(descendant, ast.Name):
                            names.add(descendant.id)
                        elif isinstance(descendant, ast.Attribute):
                            names.add(descendant.attr)
                    if not names & refusing_handler:
                        violations.append((file_name, site[0], site[1]))
                elif (isinstance(target, ast.Attribute) and target.attr in REQUEST_FUNCS
                        and ((isinstance(target.value, ast.Name)
                              and (target.value.id in modules
                                   or target.value.id in session_bound))
                             or (isinstance(target.value, ast.Call)
                                 and _is_session_maker(target.value.func)))
                        and _redirects_allowed(child)):
                    violations.append((file_name, site[0], site[1]))
                elif (isinstance(target, ast.Name) and target.id in verbs
                        and _redirects_allowed(child)):
                    violations.append((file_name, site[0], site[1]))
            visit(child, current)

    visit(tree, "<module>")
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

    def test_dead_handler_does_not_excuse_a_bare_opener(self):
        """A refusing class elsewhere in the file excuses no opener site."""
        mutant = (
            "import urllib.request\n"
            "class _NoRedirect(urllib.request.HTTPRedirectHandler):\n"
            "    def redirect_request(self, *args, **kwargs):\n"
            "        return None\n"
            "def _get(path, token):\n"
            "    opener = urllib.request.build_opener()\n"
            "    request = urllib.request.Request(\n"
            "        'https://api.example' + path,\n"
            "        headers={'Authorization': 'Bearer ' + token})\n"
            "    with opener.open(request, timeout=10) as response:\n"
            "        return response.read()\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            mutant_path = Path(tmp) / "dead_handler.py"
            mutant_path.write_text(mutant)
            violations = find_violations(mutant_path)
        self.assertEqual(len(violations), 1)
        self.assertEqual(violations[0][1], "_get")

    def test_requests_alias_and_from_import_trip_the_gate(self):
        for source, name in (
                ("import requests as rq\n"
                 "def _get(path, token):\n"
                 "    return rq.get('https://api.example' + path,\n"
                 "        headers={'Authorization': 'Bearer ' + token})\n", "alias"),
                ("from requests import get\n"
                 "def _get(path, token):\n"
                 "    return get('https://api.example' + path,\n"
                 "        headers={'Authorization': 'Bearer ' + token})\n", "from_import"),
                ("import requests\n"
                 "def _get(path, token):\n"
                 "    sess = requests.Session()\n"
                 "    return sess.get('https://api.example' + path,\n"
                 "        headers={'Authorization': 'Bearer ' + token})\n", "session"),
        ):
            with self.subTest(style=name):
                with tempfile.TemporaryDirectory() as tmp:
                    mutant_path = Path(tmp) / f"requests_{name}.py"
                    mutant_path.write_text(source)
                    violations = find_violations(mutant_path)
                self.assertEqual(len(violations), 1, name)
                self.assertEqual(violations[0][1], "_get")

    def test_requests_session_factory_spellings_trip_the_gate(self):
        for source, name in (
                ("import requests\n"
                 "def _get(path, token):\n"
                 "    sess = requests.session()\n"
                 "    return sess.get('https://api.example' + path,\n"
                 "        headers={'Authorization': 'Bearer ' + token})\n",
                 "lowercase_factory"),
                ("import requests\n"
                 "class Pinned(requests.Session):\n"
                 "    pass\n"
                 "def _get(path, token):\n"
                 "    return Pinned().get('https://api.example' + path,\n"
                 "        headers={'Authorization': 'Bearer ' + token})\n",
                 "subclass"),
        ):
            with self.subTest(style=name):
                with tempfile.TemporaryDirectory() as tmp:
                    mutant_path = Path(tmp) / f"session_{name}.py"
                    mutant_path.write_text(source)
                    violations = find_violations(mutant_path)
                self.assertEqual(len(violations), 1, name)
                self.assertEqual(violations[0][1], "_get")

    def test_requests_with_redirects_off_passes(self):
        source = (
            "import requests\n"
            "def _get(path, token):\n"
            "    return requests.get('https://api.example' + path,\n"
            "        headers={'Authorization': 'Bearer ' + token},\n"
            "        allow_redirects=False)\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            fixed_path = Path(tmp) / "requests_fixed.py"
            fixed_path.write_text(source)
            self.assertEqual(find_violations(fixed_path), [])

    def test_delegating_handler_does_not_count_as_refusing(self):
        """Refusing one status while delegating the rest still leaks."""
        mutant = (
            "import urllib.request\n"
            "class _Picky(urllib.request.HTTPRedirectHandler):\n"
            "    def redirect_request(self, req, resp, code, msg, headers, new):\n"
            "        if code == 307:\n"
            "            return None\n"
            "        return super().redirect_request(req, resp, code, msg, headers, new)\n"
            "def _get(path, token):\n"
            "    opener = urllib.request.build_opener(_Picky)\n"
            "    request = urllib.request.Request(\n"
            "        'https://api.example' + path,\n"
            "        headers={'Authorization': 'Bearer ' + token})\n"
            "    with opener.open(request, timeout=10) as response:\n"
            "        return response.read()\n"
        )
        with tempfile.TemporaryDirectory() as tmp:
            mutant_path = Path(tmp) / "picky_handler.py"
            mutant_path.write_text(mutant)
            violations = find_violations(mutant_path)
        self.assertEqual(len(violations), 1)
        self.assertEqual(violations[0][1], "_get")

    def test_sweep_covers_token_senders_outside_the_reader_list(self):
        """Authorization files beyond READERS must already be violation-free.

        READERS itself is covered by the allowlist test above; this sweep
        means a new token-sending script (or a new token path in an old one)
        trips the gate before anyone adds it to READERS.
        """
        offenders = []
        for path in sorted(SCRIPTS.glob("*.py")):
            if path.name in READERS:
                continue
            if "Authorization" not in path.read_text():
                continue
            offenders.extend(
                f"{file_name}:{func}:{line}"
                for file_name, func, line in find_violations(path))
        self.assertEqual(offenders, [])

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
