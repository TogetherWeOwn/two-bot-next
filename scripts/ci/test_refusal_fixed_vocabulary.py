"""Pin fixed-vocabulary refusals in the database-binding gate paths.

`fix(ops): keep malformed database bindings inside sanitized refusals`
moved the port read inside fixed-message refusals and removed an
interpolated unknown query key. Nothing stops the next error path from
echoing rejected input again, so
`ast-grep-rules/refusal-fixed-vocabulary.yml` flags interpolation,
`str()`/`repr()`/`ascii()`/`.join()`/`format()`/`%` rendering,
`+` concatenation and bare-variable messages inside `Refused`,
`ReadFailed` and `require` refusal constructors in the three covered
files, with an allowlist for the fixed-vocabulary sites. `sgconfig.yml`
already runs every rule in `ast-grep-rules/` against every fixture in
`ast-grep-rule-tests/` via `ast-grep test`, so no config change is needed.

This suite is the offline half of the pin: it loads the rule's own
patterns from the YAML (single source of truth, pinned to literals
below), proves the current tree is green with an exact allowlist, proves
a scratch-copy mutant reintroducing the interpolated rejected key trips,
and pins the three companions the exemptions rest on (`guild_fence`
literals, the allowlisted-key ordering, `classify_failure` purity).

It is a name-based tripwire, not a proof: it matches the names a
refusal reaches for, not every way to render rejected input. Every
`Refused`/`ReadFailed`/`require` constructor call is collected, not just
`raise` statements, so `error = Refused(f"...{host}..."); raise error`
trips at the construction site. Known gaps, documented rather than
gated: `raise Refused(message)` funnels a pre-built variable (the
`require` helper relay itself is an explicit allowlist entry; the
`require` call sites are covered); control flow the rule cannot see
(for example a `parsed.port` read moved back outside its `try`); and a
mixed line carrying both an allowlisted marker and a new interpolation
stays exempt until the exact allowlist below changes, which fails this
suite on purpose.
"""

import ast
import re
import shutil
import tempfile
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
RULE_PATH = ROOT / "ast-grep-rules/refusal-fixed-vocabulary.yml"
FIXTURE_PATH = ROOT / "ast-grep-rule-tests/refusal-fixed-vocabulary-test.yml"
SGCONFIG_PATH = ROOT / "sgconfig.yml"
SMOKE_PATH = ROOT / "scripts/staging_automation_read_smoke.py"

COVERED = [
    "scripts/ci/staging_schema_gate.py",
    "scripts/ci/production_schema_gate.py",
    "scripts/staging_events_read.py",
]

RULE_ID = "refusal-fixed-vocabulary"

# Pinned literals: the YAML must carry exactly these patterns. A deliberate
# rule change updates both sides in one reviewed diff.
EXPECTED_BROAD = (
    r"(\{|\.format\s*\(|%\s*\w|str\s*\(|repr\s*\(|ascii\s*\(|\.join\s*\(|\+"
    r"|Refused\(\s*[a-z_]|ReadFailed\(\s*[a-z_])"
)
EXPECTED_PATTERNS = (
    "Refused($MSG)",
    "ReadFailed($MSG)",
    "require($COND, $MSG)",
)
EXPECTED_NOTS = (
    r"\{[A-Z][A-Z0-9_]*\}",
    r"\{label\}",
    r"classify_failure\(",
    r"type\([A-Za-z_][A-Za-z0-9_]*\)\.__name__",
    r"has a bad \{key\}",
    r"plan refused the gate",
    r"Refused\(str\(error\)\)",
    r"Refused\(\s*message\s*\)",
)


def load_rule():
    return yaml.safe_load(RULE_PATH.read_text())


def broad_and_exempts():
    rule = load_rule()["rule"]
    broad = re.compile(rule["all"][1]["regex"])
    exempts = [re.compile(entry["regex"]) for entry in rule["not"]["any"]]
    return broad, exempts


def func_id(func):
    return getattr(func, "id", "")


def refusal_statements(path):
    """(lineno, source segment) for every in-scope refusal constructor call.

    Every `Refused`/`ReadFailed`/`require` call is collected wherever it
    sits (`raise`, assignment, `return`), matching the ast-grep rule's
    scope: the rule flags the constructor call itself, so an assignment
    like `error = Refused(f"...{host}...")` trips here even though the
    later `raise error` carries no message.
    """
    text = path.read_text()
    tree = ast.parse(text)
    found = []
    for node in ast.walk(tree):
        if isinstance(node, ast.Call) and func_id(node.func) in (
            "Refused",
            "ReadFailed",
        ):
            found.append((node.lineno, ast.get_source_segment(text, node)))
        elif isinstance(node, ast.Call) and func_id(node.func) == "require":
            assert len(node.args) == 2, (
                f"{path}:{node.lineno}: require takes two arguments; "
                "the ast-grep pattern require($COND, $MSG) would miss this shape"
            )
            found.append((node.lineno, ast.get_source_segment(text, node)))
    return sorted(found)


def classify(segment, broad, exempts):
    """Return flagged/exempted/clean for one refusal statement's source."""
    if not broad.search(segment):
        return "clean"
    if any(rx.search(segment) for rx in exempts):
        return "exempted"
    return "flagged"


def canon(segment):
    """Indentation-insensitive statement text for the exact allowlist."""
    return "\n".join(line.strip() for line in segment.strip().splitlines())


def scan_tree():
    broad, exempts = broad_and_exempts()
    flagged, exempted = [], {}
    for rel in COVERED:
        path = ROOT / rel
        for lineno, segment in refusal_statements(path):
            verdict = classify(segment, broad, exempts)
            if verdict == "flagged":
                flagged.append(f"{rel}:{lineno}: {canon(segment)}")
            elif verdict == "exempted":
                exempted.setdefault(rel, []).append(canon(segment))
    for lines in exempted.values():
        lines.sort()
    return flagged, exempted


# The exact allowlisted statements on the current tree. Any added, removed
# or reworded interpolation fails the suite until the rule and this pin are
# reviewed together.
EXPECTED_EXEMPTED = {
    "scripts/ci/staging_schema_gate.py": sorted(
        [
            'require(\n'
            'pin and "prod" not in lowered and "@" not in pin and "/" not in pin,\n'
            'f"{label} is empty, production-like or not a bare name",\n'
            ")",
            "Refused(\n"
            'f"staging-migrate plan refused the gate (exit {proc.returncode}): "\n'
            'f"{proc.stderr.strip().splitlines()[-1] if proc.stderr.strip() '
            'else \'no detail\'}"\n'
            ")",
            # The require(condition, message) helper relay.
            "Refused(message)",
        ]
    ),
    "scripts/ci/production_schema_gate.py": sorted(
        [
            "require(pin and \"@\" not in pin and \"/\" not in pin,\n"
            'f"{label} is empty or not a bare name")',
            "Refused(\n"
            'f"production-migrate plan refused the gate (exit {proc.returncode}): "\n'
            'f"{proc.stderr.strip().splitlines()[-1] if proc.stderr.strip() '
            'else \'no detail\'}"\n'
            ")",
            # The require(condition, message) helper relay.
            "Refused(message)",
        ]
    ),
    "scripts/staging_events_read.py": sorted(
        [
            "ReadFailed(f\"{classify_failure(done.stderr)} "
            "(psql exit {done.returncode})\")",
            "Refused(f\"refusing: ${MEMBER_ENV} must be 15-22 digits\")",
            "Refused(f\"refusing: no valid staging host pin bound (${HOST_ENV})\")",
            "Refused(f\"refusing: no database login bound (${URL_ENV})\")",
            "Refused(f\"refusing: no fixture member bound (${MEMBER_ENV})\")",
            "Refused(f\"refusing: the database login has a bad {key}\")",
            "Refused(f\"refusing: the database must be {DB_NAME}\")",
            "Refused(f\"refusing: the login must be role {DB_ROLE}\")",
            "Refused(f\"refusing: the server does not report role {DB_ROLE} "
            'on database {DB_NAME}")',
            "Refused(f\"refusing: {label} is not a real UTC instant\")",
            "Refused(f\"refusing: {label} must be a UTC instant like "
            '2026-10-07T14:00:00Z")',
            "Refused(str(error))",
        ]
    ),
}


class RuleShapeTests(unittest.TestCase):
    def test_rule_id_language_files_and_patterns(self):
        rule = load_rule()
        self.assertEqual(rule["id"], RULE_ID)
        self.assertEqual(rule["language"], "python")
        self.assertEqual(rule["severity"], "error")
        self.assertIn("fixed vocabulary", rule["message"])
        self.assertEqual(rule["files"], COVERED)
        body = rule["rule"]
        patterns = [entry["pattern"] for entry in body["all"][0]["any"]]
        self.assertEqual(tuple(patterns), EXPECTED_PATTERNS)

    def test_rule_regexes_match_the_pinned_literals(self):
        rule = load_rule()["rule"]
        self.assertEqual(rule["all"][1]["regex"], EXPECTED_BROAD)
        nots = [entry["regex"] for entry in rule["not"]["any"]]
        self.assertEqual(tuple(nots), EXPECTED_NOTS)
        re.compile(rule["all"][1]["regex"])
        for pattern in nots:
            re.compile(pattern)

    def test_rule_and_fixture_files_exist_with_matching_ids(self):
        self.assertTrue(RULE_PATH.is_file())
        self.assertTrue(FIXTURE_PATH.is_file())
        fixture = yaml.safe_load(FIXTURE_PATH.read_text())
        self.assertEqual(fixture["id"], RULE_ID)
        self.assertTrue(fixture["valid"])
        self.assertTrue(fixture["invalid"])

    def test_sgconfig_runs_rule_and_fixture_dirs_without_changes(self):
        config = yaml.safe_load(SGCONFIG_PATH.read_text())
        self.assertIn("ast-grep-rules", config["ruleDirs"])
        test_dirs = [
            entry.get("testDir") for entry in config.get("testConfigs", [])
        ]
        self.assertIn("ast-grep-rule-tests", test_dirs)


class FixtureConformanceTests(unittest.TestCase):
    def verdict(self, snippet):
        broad, exempts = broad_and_exempts()
        stripped = snippet.strip()
        self.assertTrue(
            re.match(
                r"(raise\s+(Refused|ReadFailed)\s*\(|require\s*\()",
                stripped,
            ),
            f"fixture snippet outside the rule scope: {snippet!r}",
        )
        return classify(snippet, broad, exempts)

    def test_valid_fixtures_pass(self):
        fixture = yaml.safe_load(FIXTURE_PATH.read_text())
        for snippet in fixture["valid"]:
            with self.subTest(snippet=snippet):
                self.assertIn(
                    self.verdict(snippet),
                    ("clean", "exempted"),
                    f"valid fixture would trip the rule: {snippet!r}",
                )

    def test_invalid_fixtures_fail(self):
        fixture = yaml.safe_load(FIXTURE_PATH.read_text())
        for snippet in fixture["invalid"]:
            with self.subTest(snippet=snippet):
                self.assertEqual(
                    self.verdict(snippet),
                    "flagged",
                    f"invalid fixture escapes the rule: {snippet!r}",
                )


class TreeGreenTests(unittest.TestCase):
    def test_no_unallowlisted_interpolation_on_the_tree(self):
        flagged, _ = scan_tree()
        self.assertEqual(
            flagged,
            [],
            "refusal interpolates rejected input; use a fixed-vocabulary "
            "message or extend the rule allowlist with review:\n"
            + "\n".join(flagged),
        )

    def test_allowlist_matches_the_tree_exactly(self):
        _, exempted = scan_tree()
        self.assertEqual(exempted, EXPECTED_EXEMPTED)

    def test_every_exemption_still_applies(self):
        _, exempted = scan_tree()
        fixture = yaml.safe_load(FIXTURE_PATH.read_text())
        pooled = "\n".join(
            [line for lines in exempted.values() for line in lines]
            + [str(snippet) for snippet in fixture["valid"]]
        )
        _, exempts = broad_and_exempts()
        self.assertEqual(len(exempts), len(EXPECTED_NOTS))
        for pattern, rx in zip(EXPECTED_NOTS, exempts):
            with self.subTest(exemption=pattern):
                self.assertTrue(
                    rx.search(pooled),
                    f"allowlist entry matches nothing; remove it: {pattern}",
                )


class MutantTests(unittest.TestCase):
    INTERPOLATED = [
        'raise Refused(f"refusing: the database login may not set {key}")',
        'raise Refused(f"plan database port {port} is refused")',
        'raise Refused(f"the binding port {parsed.port} is refused")',
        'raise Refused(f"the binding port {parts.port} is refused")',
        'raise Refused(f"psql said: {done.stderr}")',
        'raise Refused(f"plan said: {proc.stdout}")',
        'raise Refused(f"bad value {value} for key")',
        'raise Refused(f"bad binding {url}")',
        'raise Refused("bad key {}".format(key))',
        'raise Refused("bad key %s" % key)',
        'raise Refused(f"was: {error}")',
        'raise Refused(str(output))',
        'raise Refused(repr(output))',
        'raise ReadFailed(f"row bytes: {output}")',
        'require(port is None, f"pooled ports are refused: {port}")',
        'raise Refused("refusing: the database login may not set " + key)',
        'raise Refused("refusing: the database login may not set " + repr(key))',
        'raise Refused(key)',
        'raise ReadFailed(output)',
    ]
    FIXED = [
        'raise Refused("refusing: the database login may not set that query parameter")',
        'raise ReadFailed("psql timed out")',
        "Refused(DatabaseError.InvalidPort)",
        "ReadFailed(ReadFailure.Timeout)",
        "Refused(DB_ROLE)",
        "Refused(message)",
        'raise Refused(f"refusing: the database login has a bad {key}")',
        'raise Refused(f"refusing: the login must be role {DB_ROLE}")',
        'raise ReadFailed(f"{classify_failure(done.stderr)} (psql exit {done.returncode})")',
        'raise Refused(f"failed: unexpected {type(error).__name__}; nothing was written")',
        'require(port is None or port == 5432, "pooled ports are refused")',
    ]

    def test_interpolated_mutants_trip(self):
        broad, exempts = broad_and_exempts()
        for snippet in self.INTERPOLATED:
            with self.subTest(snippet=snippet):
                self.assertEqual(classify(snippet, broad, exempts), "flagged")

    def test_fixed_mutants_stay_clean(self):
        broad, exempts = broad_and_exempts()
        for snippet in self.FIXED:
            with self.subTest(snippet=snippet):
                self.assertIn(
                    classify(snippet, broad, exempts), ("clean", "exempted")
                )

    def test_scratch_mutant_with_interpolated_rejected_key_trips(self):
        fixed = (
            'raise Refused("refusing: the database login may not set '
            'that query parameter")'
        )
        regressed = (
            'raise Refused(f"refusing: the database login may not set {key} '
            'query parameter")'
        )
        with tempfile.TemporaryDirectory() as scratch:
            mutant = Path(scratch) / "staging_events_read.py"
            shutil.copy(ROOT / "scripts/staging_events_read.py", mutant)
            text = mutant.read_text()
            self.assertIn(fixed, text)
            mutant.write_text(text.replace(fixed, regressed))
            flagged, exempted = [], {}
            broad, exempt_list = broad_and_exempts()
            for lineno, segment in refusal_statements(mutant):
                verdict = classify(segment, broad, exempt_list)
                if verdict == "flagged":
                    flagged.append((lineno, canon(segment)))
            self.assertEqual(len(flagged), 1)
            lineno, statement = flagged[0]
            self.assertIn("may not set {key}", statement)
            self.assertGreaterEqual(lineno, 215)
            self.assertLessEqual(lineno, 230)

    def test_scratch_assignment_mutant_trips_at_construction(self):
        """`error = Refused(f"...{host}..."); raise error` must trip.

        The constructor call is collected wherever it sits, so building
        the interpolated refusal into a variable trips at the
        construction site even though `raise error` carries no message.
        """
        with tempfile.TemporaryDirectory() as scratch:
            mutant = Path(scratch) / "staging_events_read.py"
            shutil.copy(ROOT / "scripts/staging_events_read.py", mutant)
            text = mutant.read_text()
            probe = (
                "\n\ndef _mutant_probe(host):\n"
                '    error = Refused(f"refusing: the database host {host} '
                'must be a bare name")\n'
                "    raise error\n"
            )
            self.assertNotIn("def _mutant_probe", text)
            mutant.write_text(text + probe)
            flagged = []
            broad, exempt_list = broad_and_exempts()
            for lineno, segment in refusal_statements(mutant):
                if classify(segment, broad, exempt_list) == "flagged":
                    flagged.append((lineno, canon(segment)))
            self.assertEqual(len(flagged), 1)
            _, statement = flagged[0]
            self.assertIn("{host}", statement)


class CompanionTests(unittest.TestCase):
    def guild_fence(self):
        tree = ast.parse(SMOKE_PATH.read_text())
        for node in ast.walk(tree):
            if isinstance(node, ast.FunctionDef) and node.name == "guild_fence":
                return node
        self.fail("guild_fence not found")

    def test_guild_fence_raises_literals_only(self):
        fence = self.guild_fence()
        raises = [
            node
            for node in ast.walk(fence)
            if isinstance(node, ast.Raise)
        ]
        self.assertTrue(raises)
        for node in raises:
            with self.subTest(line=node.lineno):
                self.assertIsInstance(node.exc, ast.Call)
                self.assertEqual(func_id(node.exc.func), "SmokeError")
                self.assertEqual(len(node.exc.args), 1)
                self.assertIsInstance(node.exc.args[0], ast.Constant)

    def test_bad_key_sits_behind_the_unknown_key_guard(self):
        text = (ROOT / "scripts/staging_events_read.py").read_text()
        tree = ast.parse(text)
        target = None
        for node in ast.walk(tree):
            if isinstance(node, ast.FunctionDef) and node.name == "database_target":
                target = ast.get_source_segment(text, node)
        self.assertIsNotNone(target)
        guard = target.index("if key not in URL_PARAMS")
        fixed = target.index("may not set that query parameter")
        bad = target.index("has a bad {key}")
        self.assertLess(guard, fixed)
        self.assertLess(fixed, bad)

    def test_classify_failure_returns_fixed_phrases_only(self):
        text = (ROOT / "scripts/staging_events_read.py").read_text()
        tree = ast.parse(text)
        target = None
        for node in ast.walk(tree):
            if isinstance(node, ast.FunctionDef) and node.name == "classify_failure":
                target = node
        self.assertIsNotNone(target)
        for node in ast.walk(target):
            with self.subTest(node=type(node).__name__):
                self.assertNotIsInstance(node, ast.JoinedStr)
                if isinstance(node, ast.Call) and func_id(node.func) == "format":
                    self.fail("classify_failure formats a message")
                if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Mod):
                    self.fail("classify_failure %-formats a message")


if __name__ == "__main__":
    unittest.main()