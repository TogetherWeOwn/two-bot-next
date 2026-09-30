"""Negative fixture controls. No renderer, network, DB, or third-party packages."""

import copy
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import validate


class CorpusTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.corpus = validate.load(validate.HERE / "corpus.json")
        cls.coverage = validate.load(validate.HERE / "coverage.json")
        cls.spec = (validate.ROOT / "docs/voice-rooms.md").read_bytes()

    def test_corpus_and_determinism(self):
        first = validate.validate(self.corpus, self.coverage, self.spec)
        second = validate.validate(copy.deepcopy(self.corpus), copy.deepcopy(self.coverage), self.spec)
        self.assertEqual(first, second)
        self.assertGreaterEqual(first["exact"], 150)
        self.assertEqual(first["features"], len(validate.spec_inventory(self.spec.decode())))
        # Key order, indentation and Unicode escaping must not change the digest.
        roundtrip = json.loads(json.dumps(self.corpus, sort_keys=True, ensure_ascii=True))
        self.assertEqual(first, validate.validate(roundtrip, self.coverage, self.spec))

    def assert_bad(self, mutate, message):
        corpus, coverage = copy.deepcopy(self.corpus), copy.deepcopy(self.coverage)
        mutate(corpus, coverage)
        with self.assertRaisesRegex(validate.FixtureError, message):
            validate.validate(corpus, coverage, self.spec)

    def test_malformed_schema(self):
        mutations = [
            (lambda c, _: c.update(version=True), "corpus version"),
            (lambda c, _: c.update(unknown=True), "unknown fields"),
            (lambda c, _: c["cases"][0].pop("expected"), "missing fields"),
            (lambda c, _: c["cases"][0].update(context="missing"), "missing context"),
            (lambda c, _: c["cases"][0].update(input=42), "expected string"),
            (lambda c, _: c["cases"][0]["expected"].update(kind=[]), "unknown expected kind"),
            (lambda c, _: c["contexts"]["solo"].update(limit=True), "integer outside range"),
            (lambda c, _: c["contexts"]["solo"].update(limit=100), "integer outside range"),
            (lambda c, _: c["contexts"]["solo"].update(seed=""), "expected string"),
            (lambda c, _: c["contexts"]["solo"]["clock"].update(hour=24), "integer outside range"),
            (lambda c, _: c["contexts"]["solo"]["clock"].update(month="Sept"), "English civil clock"),
            (lambda c, _: c["contexts"]["solo"].update(owner_id="absent"), "owner absent"),
            (lambda c, _: c["contexts"]["solo"]["members"].append(c["contexts"]["solo"]["members"][0]), "duplicate member"),
            (lambda c, _: c["contexts"]["party-4"]["members"][0]["party"].update(maximum=1), "integer outside range"),
            (lambda c, _: c["cases"][0]["expected"].update(output=""), "expected string"),
            (lambda c, _: c["cases"][0]["expected"].update(output="x" * 101), "exceeds 100"),
            (lambda c, _: c["cases"][0]["expected"].update(output=" untrimmed "), "untrimmed"),
            (lambda c, _: c["cases"][1].update(id=c["cases"][0]["id"]), "duplicate case"),
            (lambda c, _: c["cases"][0]["covers"].append("token:@@missing@@"), "unknown coverage"),
        ]
        for mutate, message in mutations:
            with self.subTest(message=message):
                self.assert_bad(mutate, message)

    def test_coverage_and_ambiguity_controls(self):
        mutations = [
            (lambda _, x: x["features"].pop(), "inventory mismatch"),
            (lambda _, x: x["features"][0]["case_ids"].pop(), "reverse index mismatch"),
            (lambda _, x: x["features"][0]["case_ids"].append("missing"), "reverse index mismatch"),
            (lambda _, x: x["features"].append(copy.deepcopy(x["features"][0])), "duplicate feature"),
            (lambda _, x: x["features"][0].update(literal="not in the spec"), "literal absent"),
            (lambda c, _: c["cases"][0].update(input="plain literal"), "not exercised"),
            (lambda c, _: c["cases"][0]["covers"].append(c["cases"][0]["covers"][0]), "duplicate array"),
            (lambda _, x: x["ambiguities"][0]["case_ids"].pop(), "ambiguity reverse"),
            (lambda _, x: x["ambiguities"].pop(0), "missing ambiguity"),
            (lambda _, x: x["ambiguities"].append(copy.deepcopy(x["ambiguities"][0])), "duplicate ambiguity"),
        ]
        for mutate, message in mutations:
            with self.subTest(message=message):
                self.assert_bad(mutate, message)

    def test_minimum_not_inflated_by_deferred_cases(self):
        def mutate(corpus, _):
            corpus["cases"] = [c for c in corpus["cases"] if c["expected"]["kind"] != "exact"]
        self.assert_bad(mutate, "fewer than 150 distinct exact")

    def test_duplicates_do_not_inflate_minimum(self):
        def mutate(corpus, _):
            first = corpus["cases"][0]
            corpus["cases"] = []
            for i in range(150):
                row = copy.deepcopy(first)
                row["id"] = f"duplicate-{i}"
                corpus["cases"].append(row)
        self.assert_bad(mutate, "fewer than 150 distinct exact")

    def test_conflicting_expectations(self):
        def mutate(corpus, _):
            row = copy.deepcopy(corpus["cases"][0])
            row["id"] = "conflicting"
            row["expected"]["output"] = "different"
            corpus["cases"].append(row)
        self.assert_bad(mutate, "conflicting expectations")

    def test_stability_groups(self):
        def different_seed(corpus, _):
            row = next(c for c in corpus["cases"] if c["id"] == "random-list-stable-rename")
            corpus["contexts"][row["context"]]["seed"] = "different-seed"
        mutations = [
            (lambda c, _: c.update(stability_groups=[]), "missing cross-rename"),
            (lambda c, _: c["stability_groups"][0]["case_ids"].append("missing"), "missing stability case"),
            (different_seed, "stability seeds differ"),
            (lambda c, _: c["stability_groups"][0].update(case_ids=["random-choice-stable"]), "needs two cases"),
            (lambda c, _: c["stability_groups"].append(copy.deepcopy(c["stability_groups"][0])), "duplicate stability"),
        ]
        for mutate, message in mutations:
            with self.subTest(message=message):
                self.assert_bad(mutate, message)

    def test_invariant_schema(self):
        def mutate(corpus, _):
            row = next(c for c in corpus["cases"] if c["expected"]["kind"] == "invariant")
            row["expected"]["stable_for_same_context"] = False
        self.assert_bad(mutate, "stability invariant required")

    def validate_added_case(self, template, expected, covers):
        corpus, coverage = copy.deepcopy(self.corpus), copy.deepcopy(self.coverage)
        corpus["contexts"]["regression-control"] = copy.deepcopy(corpus["contexts"]["solo"])
        corpus["contexts"]["regression-control"]["seed"] = "regression-control"
        case = {"id": "regression-control", "input": template, "context": "regression-control",
                "expected": expected, "covers": covers}
        corpus["cases"].append(case)
        for feature in coverage["features"]:
            if feature["id"] in covers:
                feature["case_ids"].append(case["id"])
        return validate.validate(corpus, coverage, self.spec)

    def test_trim_before_truncate_boundary(self):
        # V5 trims before truncation: cutting just before y exposes whitespace.
        template = "x" * 99 + " y"
        output = "x" * 99 + " "
        for kind in ["exact", "invariant"]:
            for whitespace in [" ", "\t", "\n"]:
                with self.subTest(kind=kind, whitespace=whitespace):
                    expected = {"kind": "exact", "output": output[:-1] + whitespace}
                    if kind == "invariant":
                        expected = {"kind": kind, "nonempty": True, "max_characters": 100,
                                    "stable_for_same_context": True,
                                    "allowed_outputs": [output[:-1] + whitespace]}
                    self.validate_added_case(template[:-2] + whitespace + "y", expected,
                                             ["rule:trim", "rule:truncate"])
            for invalid in ["x" * 98 + " ", " " + "x" * 99, " " * 100, "x" * 100 + " "]:
                with self.subTest(kind=kind, invalid=repr(invalid)):
                    expected = {"kind": "exact", "output": invalid}
                    if kind == "invariant":
                        expected = {"kind": kind, "nonempty": True, "max_characters": 100,
                                    "stable_for_same_context": True, "allowed_outputs": [invalid]}
                    with self.assertRaises(validate.FixtureError):
                        self.validate_added_case(template, expected, ["rule:truncate"])

    def test_mixed_random_blocks(self):
        template = "[[den/crew]] [[list:rooms]]"
        self.assertTrue(validate.feature_used("random:choice", template))
        self.assertTrue(validate.feature_used("random:list", template))
        self.assertFalse(validate.feature_used("random:choice", "[[list:rooms]]"))
        self.assertFalse(validate.feature_used("random:choice", "unfinished [[den/crew"))
        expected = {"kind": "invariant", "nonempty": True, "max_characters": 100,
                    "stable_for_same_context": True}
        self.validate_added_case(template, expected, ["random:choice", "random:list"])

    def test_compatible_invariant_constraints(self):
        for outputs, target in [(["ROOM NAME", "Room Name"], "room name"),
                                (["den", "CREW"], "crew"), (["Straße"], "strasse"),
                                (["ß" * 100], "ss" * 100),
                                (["x" * 99 + " "], "x" * 99 + " ")]:
            with self.subTest(outputs=outputs):
                expected = {"kind": "invariant", "nonempty": True, "max_characters": 100,
                            "stable_for_same_context": True, "allowed_outputs": outputs,
                            "casefold_equals": target}
                self.validate_added_case('""rand:room name""', expected, ["style:rand"])

    def test_contradictory_invariant_constraints(self):
        for outputs, target, message in [(["den"], "room name", "contradictory invariant"),
                                         (["ROOM NAME"], "Room Name", "case-folded target"),
                                         (["Straße"], "straße", "case-folded target")]:
            with self.subTest(outputs=outputs, target=target):
                expected = {"kind": "invariant", "nonempty": True, "max_characters": 100,
                            "stable_for_same_context": True, "allowed_outputs": outputs,
                            "casefold_equals": target}
                with self.assertRaisesRegex(validate.FixtureError, message):
                    self.validate_added_case('""rand:room name""', expected, ["style:rand"])

    def test_spec_drift(self):
        with self.assertRaisesRegex(validate.FixtureError, "spec drift"):
            validate.validate(self.corpus, self.coverage, self.spec + b"\nChanged")

    def test_spec_inventory_detects_new_token_and_style(self):
        for old, new in [(b"@@num@@`: humans", b"@@new_counter@@ @@num@@`: humans"),
                         (b"- Novelty: `uwu`, `usd` (upside down).", b"- Novelty: `uwu`, `usd`, `newstyle`.")]:
            spec = self.spec.replace(old, new)
            self.assertTrue(spec != self.spec, "spec mutation must replace a real fragment")
            corpus = copy.deepcopy(self.corpus)
            corpus["spec"]["sha256"] = hashlib.sha256(spec).hexdigest()
            with self.assertRaisesRegex(validate.FixtureError, "inventory mismatch"):
                validate.validate(corpus, self.coverage, spec)

    def test_strict_json(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "invalid.json"
            for raw in ['{"version":1,"version":2}', '{"x":NaN}', '{"x":Infinity}', '{broken']:
                path.write_text(raw)
                with self.subTest(raw=raw), self.assertRaises(ValueError):
                    validate.load(path)

    def test_malformed_fixture_cli_deliberately_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "malformed.json"
            malformed = copy.deepcopy(self.corpus)
            malformed["cases"][0].pop("expected")
            path.write_text(json.dumps(malformed))
            result = subprocess.run([sys.executable, str(validate.HERE / "validate.py"),
                                     "--corpus", str(path)], capture_output=True, text=True, check=False)
            self.assertEqual(result.returncode, 1)
            self.assertIn("INVALID: missing fields", result.stderr)
            self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main()
