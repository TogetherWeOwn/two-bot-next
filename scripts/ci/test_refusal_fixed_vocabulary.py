"""Pin fixed-vocabulary refusals in the database-binding gate paths.

`fix(ops): keep malformed database bindings inside sanitized refusals`
moved the port read inside fixed-message refusals and removed an
interpolated unknown query key. Nothing stops the next error path from
echoing rejected input again, so
`ast-grep-rules/refusal-fixed-vocabulary.yml` flags interpolation,
`str()`/`repr()`/`ascii()`/`.join()`/`format()`/`%` rendering
(including `%(name)s` named specifiers and parenthesised operands),
`+` concatenation and bare-variable messages inside `Refused`,
`ReadFailed` and `require` refusal constructors in the four covered
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
trips at the construction site. The trigger runs against the message
argument plus its constructor prefix (`Refused(`, `ReadFailed(`, `,
`)`), never a require condition, so
`require(str(port).isdigit(), "port must be numeric")` stays clean
while `Refused(done.stderr)` trips exactly as the rule's whole-call
alternatives do (verified with the pinned binary). The
YAML rule cannot scope that far in ast-grep 0.50 (`has` sees direct
children only and never unifies an outer $MSG; verified with probe
rules), so it still matches the whole call and a computed-condition
shape must never join its valid fixtures. `Refused`/`ReadFailed` take
exactly one message argument: a second argument stringifies into a tuple
that `scripts/staging_events_read.py:371/374` prints, so the pin asserts
the single-argument shape (mirroring the `require` arity assert) and
treats any extra argument, `*args` splat or `**kwargs` as flagged. The ast-grep `Refused($MSG)` /
`ReadFailed($MSG)` patterns only bind single-argument calls, so the YAML
still misses that family and this pin decides it. This pin decides CI.
Known gaps, documented rather than
gated: `raise Refused(message)` funnels a pre-built variable (the
`require` helper relay itself is an explicit allowlist entry; the
`require` call sites are covered); a computed or bare-attribute require
message neither gate sees (`require(cond, url.strip())`,
`require(ok, done.stderr)`, `require(ok, parsed.hostname)` trip neither
the rule nor this pin); control flow the rule cannot see
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
    "scripts/ci/schema_gate_common.py",
    "scripts/staging_events_read.py",
]

RULE_ID = "refusal-fixed-vocabulary"

# Pinned literals: the YAML must carry exactly these patterns. A deliberate
# rule change updates both sides in one reviewed diff.
EXPECTED_BROAD = (
    r"(\{|\.format\s*\(|%\s*[\w\(]|str\s*\(|repr\s*\(|ascii\s*\(|\.join\s*\(|\+"
    r"|Refused\(\s*[a-z_]|ReadFailed\(\s*[a-z_]"
    r"|,\s*[a-z_][A-Za-z0-9_]*\s*\)\s*$|^[a-z_][A-Za-z0-9_]*$)"
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


def first_call(snippet):
    """Outermost refusal constructor call in one snippet.

    `ast.walk` is breadth-first, so the first `Refused`/`ReadFailed`/
    `require` call is the outermost one, never a rendering helper nested
    inside its message.
    """
    tree = ast.parse(snippet.strip())
    for node in ast.walk(tree):
        if isinstance(node, ast.Call) and func_id(node.func) in (
            "Refused",
            "ReadFailed",
            "require",
        ):
            return node
    raise AssertionError(f"no refusal call in snippet: {snippet!r}")


def message_segment(text, node, where=""):
    """Source of the refusal message argument, or "" when it has none.

    `Refused`/`ReadFailed` carry the message in their single argument (or
    a `msg=` keyword; the ast-grep `Refused($MSG)` pattern binds that
    too, verified with the pinned binary) and `require` in its second;
    anything else (for example a computed require condition) is
    deliberately invisible to the trigger. A second `Refused`/`ReadFailed`
    argument stringifies into a tuple the readers print, and the
    single-argument ast-grep patterns miss it, so out-of-contract arity
    raises here and the callers map it to flagged.
    """
    if func_id(node.func) == "require":
        assert len(node.args) == 2, (
            f"{where}: require takes two arguments; "
            "the ast-grep pattern require($COND, $MSG) would miss this shape"
        )
        target = node.args[1]
    elif func_id(node.func) in ("Refused", "ReadFailed"):
        if any(isinstance(arg, ast.Starred) for arg in node.args) or any(
            kw.arg is None for kw in node.keywords
        ):
            raise AssertionError(
                f"{where}: Refused/ReadFailed take exactly one message "
                "argument, not *args/**kwargs; "
                "the ast-grep patterns Refused($MSG)/ReadFailed($MSG) would "
                "miss this shape"
            )
        assert len(node.args) + len(node.keywords) == 1, (
            f"{where}: Refused/ReadFailed take exactly one argument; "
            "the ast-grep patterns Refused($MSG)/ReadFailed($MSG) would "
            "miss this shape"
        )
        if node.args:
            target = node.args[0]
        elif node.keywords:
            target = node.keywords[0].value
        else:
            target = None
    elif node.args:
        target = node.args[0]
    elif node.keywords:
        target = node.keywords[0].value
    else:
        target = None
    if target is None:
        return ""
    return ast.get_source_segment(text, target) or ""


def scope_probes(node, message):
    """Constructor-scoped strings that replicate the rule's whole-call match.

    The YAML applies the trigger regex to the whole call, so its
    `Refused\\(\\s*[a-z_]` / `ReadFailed\\(\\s*[a-z_]` /
    `,\\s*[a-z_]...$` alternatives see the constructor spelling that a
    bare message can never contain: `done.stderr` has no `Refused(` in
    it, so the message-only check stays clean where the rule flags.
    Re-testing those same constructor prefixes here (and only the
    prefixes, never a require condition) keeps the pin's verdict
    identical to the rule's, so `require(str(port).isdigit(), "port
    must be numeric")` stays clean.
    """
    if func_id(node.func) == "require":
        return [f", {message})"]
    return [f"{func_id(node.func)}({message}"]


def refusal_statements(path):
    """(lineno, call segment, message segment, scope probes) for refusals.

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
            "require",
        ):
            segment = ast.get_source_segment(text, node)
            try:
                message = message_segment(
                    text, node, where=f"{path}:{node.lineno}"
                )
                probes = scope_probes(node, message)
            except AssertionError:
                # Out-of-contract arity (for example a two-argument
                # Refused): the single-argument ast-grep patterns miss it
                # and str(error) prints the tuple, so force a trip. The
                # "+" guarantees the broad trigger matches; no allowlist
                # entry contains it.
                message = f"{segment} + extra-arg"
                probes = [message]
            found.append(
                (
                    node.lineno,
                    segment,
                    message,
                    probes,
                )
            )
    return sorted(found, key=lambda entry: entry[:3])


def classify(segment, message, broad, exempts, probes=()):
    """Return flagged/exempted/clean for one refusal call.

    The trigger runs against the message argument plus the
    constructor-scoped probes; the exemptions run against the whole
    call (entries like `Refused\\(\\s*message\\s*\\)` name the
    constructor itself).
    """
    if not broad.search(message) and not any(
        broad.search(probe) for probe in probes
    ):
        return "clean"
    if any(rx.search(segment) for rx in exempts):
        return "exempted"
    return "flagged"


def snippet_verdict(snippet):
    """classify() for one fixture or mutant snippet.

    Out-of-contract arity (a second argument, or *args/**kwargs
    splatting) raises in message_segment(); map it to flagged so those
    shapes trip the pin even though the single-argument ast-grep patterns
    miss them. A snippet with no refusal call at all still raises from
    first_call() so a typo'd constructor name can never read as flagged.
    """
    broad, exempts = broad_and_exempts()
    # first_call stays outside the try: a snippet with no refusal call
    # (for example a typo'd constructor name) must raise loudly, never
    # read as "flagged". Only out-of-contract arity maps to flagged.
    node = first_call(snippet)
    try:
        message = message_segment(snippet.strip(), node)
    except AssertionError:
        return "flagged"
    return classify(
        snippet, message, broad, exempts, scope_probes(node, message)
    )


def canon(segment):
    """Indentation-insensitive statement text for the exact allowlist."""
    return "\n".join(line.strip() for line in segment.strip().splitlines())


def scan_tree():
    broad, exempts = broad_and_exempts()
    flagged, exempted = [], {}
    for rel in COVERED:
        path = ROOT / rel
        for lineno, segment, message, probes in refusal_statements(path):
            verdict = classify(segment, message, broad, exempts, probes)
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
        ]
    ),
    "scripts/ci/production_schema_gate.py": sorted(
        [
            "require(pin and \"@\" not in pin and \"/\" not in pin,\n"
            'f"{label} is empty or not a bare name")',
        ]
    ),
    "scripts/ci/schema_gate_common.py": ["Refused(message)"],
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
        stripped = snippet.strip()
        self.assertTrue(
            re.match(
                r"(raise\s+(Refused|ReadFailed)\s*\(|require\s*\()",
                stripped,
            ),
            f"fixture snippet outside the rule scope: {snippet!r}",
        )
        return snippet_verdict(snippet)

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
        'raise Refused(f"plan refused the gate: {proc.stderr}")',
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
        'require(port is not None, key)',
        'raise Refused("bad %(key)s" % keymap)',
        'raise ReadFailed(done.stderr)',
        'raise Refused(parsed.hostname)',
        'raise ReadFailed(output.decode())',
        'raise Refused(error.args[0])',
        'raise Refused(output[:80])',
        'raise Refused(host.upper())',
        'raise Refused(msg=host)',
        # A second argument stringifies into a tuple the readers print;
        # the single-argument ast-grep patterns miss it, so the pin trips
        # on arity (pin-only: no YAML fixture change).
        'raise Refused("refusing: the database host must be a bare name", host)',
        'raise ReadFailed("psql failed", done.stderr)',
        # A *splat smuggles the tuple through the same hole.
        "raise Refused(*error.args)",
        # A **splat smuggles keywords through the same hole. Uppercase on
        # purpose: a lowercase name still trips the broad lowercase-name
        # pattern without the kw.arg branch, so only the uppercase form
        # pins that half of the splat check.
        "raise Refused(**KWARGS)",
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
        # A computed require condition with a fixed message stays clean:
        # the trigger never sees the condition.
        'require(str(port).isdigit(), "port must be numeric")',
    ]

    def test_interpolated_mutants_trip(self):
        for snippet in self.INTERPOLATED:
            with self.subTest(snippet=snippet):
                self.assertEqual(snippet_verdict(snippet), "flagged")

    def test_snippets_without_a_refusal_call_raise(self):
        # A typo'd constructor (or any snippet with no refusal call) must
        # raise from first_call(), never read as "flagged": silent
        # flagged verdicts let misspelled mutants pass without a signal.
        for snippet in (
            'raise Refuse(f"x {host}")',
            "x = 1",
            "foo(bar)",
        ):
            with self.subTest(snippet=snippet):
                with self.assertRaises(AssertionError):
                    snippet_verdict(snippet)

    def test_fixed_mutants_stay_clean(self):
        for snippet in self.FIXED:
            with self.subTest(snippet=snippet):
                self.assertIn(
                    snippet_verdict(snippet), ("clean", "exempted")
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
            entries = refusal_statements(mutant)
            for lineno, segment, message, probes in entries:
                verdict = classify(
                    segment, message, broad, exempt_list, probes
                )
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
            entries = refusal_statements(mutant)
            for lineno, segment, message, probes in entries:
                verdict = classify(
                    segment, message, broad, exempt_list, probes
                )
                if verdict == "flagged":
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
                if isinstance(node, ast.Call) and (
                    func_id(node.func) == "format"
                    or getattr(node.func, "attr", "") == "format"
                ):
                    # `"...".format(x)` is an Attribute call, which func_id
                    # reports as ""; match it too so formatted outputs trip.
                    self.fail("classify_failure formats a message")
                if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Mod):
                    self.fail("classify_failure %-formats a message")


if __name__ == "__main__":
    unittest.main()