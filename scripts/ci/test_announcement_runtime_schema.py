"""Hermetic tests for scripts/announcement_runtime_schema.py.

The receipt comes from announcement_runtime_fixtures.py, a hand-written
synthetic fixture. It is not live evidence and not an authority: the schema
module is imported here only for its constants and entry points, never to
derive the fixture shape. Shape acceptance proves syntax only.
"""

import copy
import datetime
import json
import pathlib
import sys
import traceback
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import announcement_runtime_fixtures as fx
import announcement_runtime_schema as schema

SENTINEL = "S3CRET-sentinel-9f2c"


def encode(value):
    return json.dumps(value, separators=(",", ":")).encode("utf-8")


def get(obj, path):
    for part in path:
        obj = obj[part]
    return obj


def mutated(path, value):
    doc = fx.receipt()
    get(doc, path[:-1])[path[-1]] = value
    return doc


class Base(unittest.TestCase):
    def refused(self, raw, code):
        with self.assertRaises(schema.Refusal) as ctx:
            schema.parse_receipt(raw)
        self.assertEqual(ctx.exception.code, code)
        return ctx.exception

    def refused_doc(self, doc, code):
        return self.refused(encode(doc), code)


class FixtureTests(Base):
    def test_fixture_is_fresh_and_accepted(self):
        self.assertIsNot(fx.receipt(), fx.receipt())
        doc = fx.receipt()
        self.assertEqual(schema.parse_receipt(encode(doc)), doc)
        schema.validate_receipt_shape(copy.deepcopy(doc))

    def test_fixture_does_not_confer_promotion(self):
        self.assertIs(fx.receipt()["promotable"], False)
        self.refused_doc(mutated(["promotable"], True), "enum")

    def test_constants(self):
        self.assertEqual(schema.RECEIPT_LIMIT, 1048576)
        self.assertEqual(schema.TEXT_LIMIT, 4096)
        self.assertEqual(schema.REFERENCE_LIMIT, 256)
        self.assertEqual(schema.LEDGER_LIMIT, 512)
        self.assertEqual(schema.DEPTH_LIMIT, 12)


class FieldTests(Base):
    # Hand-listed object paths and one required key in each.
    OBJECTS = [
        ([], "scope"), (["scope"], "pr"), (["scope", "repository"], "id"),
        (["scope", "candidate"], "tree"), (["scope", "contract"], "revision"),
        (["scope", "source_manifest"], "sha256"), (["control"], "workflow"),
        (["control", "workflow"], "path"), (["control", "installation"], "mapping_ref"),
        (["authority"], "epoch"), (["authority", "decisions", 0], "outcome"),
        (["authority", "decisions", 0, "principal"], "subject_id"),
        (["authority", "reads", 0], "history_sha256"),
        (["authority", "execution"], "nonce_ref"), (["authority", "execution", "run"], "attempt"),
        (["attempt"], "enforcement_epoch"), (["attempt", "checkpoints", 0], "phase"),
        (["attempt", "locks", 0], "fence"), (["build"], "image_digest"),
        (["build", "worker"], "entrypoint"), (["build", "worker", "modules", 0], "size"),
        (["rollout"], "deployment_id"), (["served", "samples", 0], "modules_sha256"),
        (["resources"], "plan"), (["resources", "entries", 0], "owner"),
        (["resources", "entries", 0, "identity"], "cleanup_plan_ref"),
        (["resources", "states", 0], "ref"), (["resources", "states", 1], "value_sha256"),
        (["fixtures"], "consent_ref"), (["fixtures", "setting_boundaries", 0], "row_version"),
        (["gates"], "readiness_clearance"), (["gates", "records", 0], "status"),
        (["observations"], "reconciliation_ref"), (["observations", "cases", 0], "outcome"),
        (["observations", "expected_effects", 0], "sequence"),
        (["observations", "coverage", 0], "result"),
        (["lifecycle"], "in_flight"), (["lifecycle", "setup", 0], "outcome"),
        (["lifecycle", "served_before"], "rollout_ref"), (["result"], "discrepancy_refs"),
    ]

    def test_envelope_unknown_and_missing(self):
        for key in fx.receipt():
            doc = fx.receipt()
            del doc[key]
            with self.subTest(missing=key):
                self.refused_doc(doc, "object_fields")
        doc = fx.receipt()
        doc["extension"] = "x"
        self.refused_doc(doc, "object_fields")
        self.refused(b"[]", "object_fields")
        self.refused(b"null", "object_fields")
        self.refused(b"{}", "object_fields")

    def test_nested_unknown_and_missing(self):
        for path, key in self.OBJECTS:
            with self.subTest(path=path, missing=key):
                doc = fx.receipt()
                del get(doc, path)[key]
                self.refused_doc(doc, "object_fields")
            with self.subTest(path=path, unknown=True):
                doc = fx.receipt()
                get(doc, path)["extension"] = "x"
                self.refused_doc(doc, "object_fields")

    def test_module_and_workflow_paths_cannot_escape(self):
        for path in (["build", "worker", "entrypoint"],
                     ["build", "worker", "modules", 0, "path"],
                     ["control", "workflow", "path"]):
            for bad in ("../escape", "a/../b", "./local", ".", "..", "a/.."):
                with self.subTest(path=path, bad=bad):
                    self.refused_doc(mutated(path, bad), "path")
            for bad in ("/absolute", "a//b", "a\\b", "a/", "https://host/path"):
                with self.subTest(path=path, bad=bad):
                    self.refused_doc(mutated(path, bad), "text_format")

    def test_union_tag_failures(self):
        ident = ["resources", "entries", 0, "identity"]
        self.refused_doc(mutated(ident + ["kind"], "other"), "union_tag")
        self.refused_doc(mutated(ident + ["kind"], 3), "union_tag")
        self.refused_doc(mutated(["resources", "states", 0, "presence"], "gone"), "union_tag")
        doc = fx.receipt()
        doc["resources"]["states"][0] = []
        self.refused_doc(doc, "union_tag")
        # A tag from another variant does not borrow that variant's fields.
        doc = fx.receipt()
        doc["resources"]["states"][0]["version"] = "1"
        self.refused_doc(doc, "object_fields")

    def test_duplicate_keys(self):
        self.refused(b'{"a":1,"a":2}', "duplicate_key")
        self.refused(b'{"a":1,"\\u0061":2}', "duplicate_key")
        self.refused(b'{"\\u00e9":1,"\xc3\xa9":2}', "duplicate_key")
        self.refused(b'{"a\\/b":1,"a/b":2}', "duplicate_key")
        text = encode(fx.receipt()).decode()
        self.refused((text[:-1] + ',"schema":"x"}').encode(), "duplicate_key")
        self.refused((text[:-1] + ',"\\u0073chema":"x"}').encode(), "duplicate_key")
        self.refused((text[:-1] + ',"sch\\u0065ma":"x"}').encode(), "duplicate_key")
        nested = text.replace('"result":{', '"result":{"status":"fail",', 1)
        self.refused(nested.encode(), "duplicate_key")
        nested = text.replace('"result":{', '"result":{"\\u0073tatus":"fail",', 1)
        self.refused(nested.encode(), "duplicate_key")


class ScalarTests(Base):
    COUNT = ["resources", "entries", 0, "expected_count"]

    def test_count_boundaries(self):
        self.assertEqual(schema.parse_receipt(encode(mutated(self.COUNT, 0)))
                         ["resources"]["entries"][0]["expected_count"], 0)
        top = 2**63 - 1
        schema.parse_receipt(encode(mutated(self.COUNT, top)))
        self.refused_doc(mutated(self.COUNT, top + 1), "integer")
        self.refused_doc(mutated(self.COUNT, -1), "integer")
        self.refused_doc(mutated(self.COUNT, 10**20), "integer_size")
        self.refused_doc(mutated(self.COUNT, -(10**19)), "integer_size")

    def test_count_rejects_bool_float_null_string(self):
        for label, literal in (("true", b"true"), ("false", b"false"), ("float", b"1.0"),
                               ("exp", b"1e2"), ("null", b"null"), ("str", b'"1"'),
                               ("list", b"[1]"), ("dict", b"{}")):
            with self.subTest(literal=label):
                text = encode(mutated(self.COUNT, 7)).replace(
                    b'"expected_count":7', b'"expected_count":' + literal, 1)
                self.assertIn(literal, text)
                try:
                    schema.parse_receipt(text)
                except schema.Refusal:
                    pass
                else:
                    self.fail("accepted non-integer count")

    def test_bool_is_not_integer_or_enum_member(self):
        self.refused_doc(mutated(["attempt", "run", "attempt"], True), "integer")
        self.refused_doc(mutated(["gates", "records", 0, "status"], True), "integer")
        self.refused_doc(mutated(["fixtures", "boot_announcements"], 1), "enum")
        self.refused_doc(mutated(["fixtures", "boot_announcements"], False), "enum")
        self.refused_doc(mutated(["promotable"], 0), "enum")
        self.refused_doc(mutated(["promotable"], None), "enum")

    def test_service_id_and_integer_ranges(self):
        for bad in ("0", "01", "", "-1", "1" * 21, 5, None):
            with self.subTest(service_id=bad):
                with self.assertRaises(schema.Refusal):
                    schema.parse_receipt(encode(mutated(["authority", "epoch"], bad)))
        schema.parse_receipt(encode(mutated(["authority", "epoch"], "1" * 20)))
        status = ["gates", "records", 0, "status"]
        schema.parse_receipt(encode(mutated(status, 100)))
        schema.parse_receipt(encode(mutated(status, 599)))
        self.refused_doc(mutated(status, 99), "integer")
        self.refused_doc(mutated(status, 600), "integer")
        self.refused_doc(mutated(["attempt", "run", "attempt"], 0), "integer")
        eff = ["observations", "actual_effects", 0]
        self.refused_doc(mutated(eff + ["attempted_count"], 2), "integer")
        self.refused_doc(mutated(eff + ["accepted_count"], 2), "integer")
        size = ["build", "worker", "modules", 0, "size"]
        schema.parse_receipt(encode(mutated(size, schema.ARCHIVE_LIMIT)))
        self.refused_doc(mutated(size, schema.ARCHIVE_LIMIT + 1), "integer")
        self.refused_doc(mutated(size, 0), "integer")

    def test_discord_resource_ids_fit_nonzero_u64_without_narrowing_service_ids(self):
        identity = {"kind": "discord", "application_id": "1", "guild_id": "2",
                    "object_id": "3", "object_type": "channel",
                    "permissions_sha256": "a" * 64, "membership_sha256": "b" * 64}
        for field in ("application_id", "guild_id", "object_id"):
            for value in (str(2**64), "99999999999999999999"):
                with self.subTest(field=field, oversized=value):
                    doc = fx.receipt()
                    doc["resources"]["entries"][0]["identity"] = {**identity, field: value}
                    self.refused_doc(doc, "discord_id")
            for value in ("1", str(2**64 - 1)):
                with self.subTest(field=field, boundary=value):
                    doc = fx.receipt()
                    doc["resources"]["entries"][0]["identity"] = {**identity, field: value}
                    schema.parse_receipt(encode(doc))
        # Epochs are not Discord snowflakes: retain the existing generic syntax.
        schema.parse_receipt(encode(mutated(["authority", "epoch"], "99999999999999999999")))

    def test_non_json_numbers_and_syntax(self):
        for literal in (b"NaN", b"Infinity", b"-Infinity"):
            with self.subTest(literal=literal):
                self.refused(b'{"a":' + literal + b"}", "non_json_number")
        for raw in (b"{", b'{"a":}', b"{'a':1}", b'{"a":1,}', b"nope", b"\xff\xfe",
                    b'{"a":1} x', b'\xef\xbb\xbf{"a":1}'):
            with self.subTest(raw=raw):
                self.refused(raw, "json")

    def test_unicode(self):
        self.refused(b'{"a":"\\ud800"}', "unicode")
        self.refused(b'{"\\udc00":1}', "unicode")
        self.refused(b'{"a":["\\udfff"]}', "unicode")
        self.refused_doc(mutated(["scope", "task_ref"], "t\u00e9st"), "text_format")
        self.refused_doc(mutated(["scope", "task_ref"], "\uff41bc"), "text_format")
        self.refused_doc(mutated(["scope", "task_ref"], "ref\n"), "text_format")
        self.refused_doc(mutated(["authority", "epoch"], "\u0661"), "text_format")
        self.refused(b'\xc3\x28', "json")

    def test_unresolved_symbols_refused_case_insensitively(self):
        for word in ("unknown", "UNKNOWN", "todo", "tbd", "none", "null", "all", "hold",
                     "unbound"):
            with self.subTest(word=word):
                self.refused_doc(mutated(["scope", "task_ref"], word.lower()),
                                 "unresolved_symbol")
        doc = mutated(["scope", "task_ref"], "unknown-task")
        schema.parse_receipt(encode(doc))

    def test_text_boundary_4096_bytes(self):
        field = ["scope", "task_ref"]
        self.refused_doc(mutated(field, "a" * 4096), "text_format")
        self.refused_doc(mutated(field, "a" * 4097), "text_size")
        # 2048 two-byte characters are exactly 4096 bytes; one more is over.
        self.refused_doc(mutated(field, "\u00e9" * 2048), "text_format")
        self.refused_doc(mutated(field, "\u00e9" * 2049), "text_size")
        self.refused_doc(mutated(field, "\U0001f600" * 1025), "text_size")
        # Keys are bounded too.
        doc = fx.receipt()
        doc["k" * 4097] = 1
        self.refused_doc(doc, "text_size")

    def test_reference_length_boundary(self):
        field = ["scope", "task_ref"]
        schema.parse_receipt(encode(mutated(field, "a" * 96)))
        self.refused_doc(mutated(field, "a" * 97), "text_format")
        self.refused_doc(mutated(field, ""), "text_format")
        self.refused_doc(mutated(field, "Abc"), "text_format")
        self.refused_doc(mutated(field, "1abc"), "text_format")

    def test_reference_256_boundaries(self):
        refs = ["attempt", "job_refs"]
        schema.parse_receipt(encode(mutated(refs, ["r%d" % i for i in range(256)])))
        self.refused_doc(mutated(refs, ["r%d" % i for i in range(257)]), "array_size")
        self.refused_doc(mutated(refs, []), "array_size")
        # An object with more than 256 keys is refused before field checking.
        self.refused_doc(mutated(["scope"], {"k%d" % i: 1 for i in range(257)}),
                         "object_size")
        self.refused_doc(mutated(["scope"], {"k%d" % i: 1 for i in range(256)}),
                         "object_fields")
        flags = ["build", "worker", "compatibility_flags"]
        schema.parse_receipt(encode(mutated(flags, [])))
        self.refused_doc(mutated(flags, ["f%d" % i for i in range(257)]), "array_size")

    def test_ledger_512_boundaries(self):
        for name in ("expected_effects", "actual_effects"):
            path = ["observations", name]
            with self.subTest(name=name):
                schema.parse_receipt(encode(mutated(path, [fx.effect() for _ in range(512)])))
                self.refused_doc(mutated(path, [fx.effect() for _ in range(513)]), "list_size")
        schema.parse_receipt(encode(mutated(["observations", "expected_effects"], [])))
        for name in ("setup", "cleanup"):
            path = ["lifecycle", name]
            with self.subTest(name=name):
                schema.parse_receipt(encode(mutated(path, [fx.transition()] * 512)))
                self.refused_doc(mutated(path, [fx.transition()] * 513), "list_size")
                self.refused_doc(mutated(path, []), "array_size")
        # Lists above the ledger bound fail before any element is typed.
        self.refused_doc(mutated(["attempt", "job_refs"], ["r"] * 513), "list_size")

    def test_depth_boundaries(self):
        self.refused(b"[" * 13 + b"]" * 13, "depth")
        self.refused(b"[" * 40, "depth")
        self.refused(b"[" * 12 + b"]" * 12, "object_fields")
        self.refused(b"[" * 11 + b"1" + b"]" * 11, "object_fields")
        self.refused(b"[" * 12 + b"1" + b"]" * 12, "depth")
        self.refused(b'{"a":' * 13 + b"1" + b"}" * 13, "depth")
        self.refused(b"]", "json")
        self.refused(b"[]]", "json")
        # Brackets and escaped quotes inside strings do not count as depth.
        self.refused(b'{"a":"' + b"[" * 50 + b'"}', "object_fields")
        self.refused(b'{"a":"\\"' + b"[" * 50 + b'"}', "object_fields")
        self.refused(b'{"a":"\\\\"}', "object_fields")
        deep = mutated(["scope", "task_ref"], "x")
        deep["extra"] = json.loads(b"[" * 10 + b"1" + b"]" * 10)
        self.refused_doc(deep, "object_fields")

    def test_receipt_size_1mib(self):
        limit = schema.RECEIPT_LIMIT
        base = encode(fx.receipt())
        exact = base + b" " * (limit - len(base))
        self.assertEqual(len(exact), limit)
        self.assertEqual(schema.parse_receipt(exact), fx.receipt())
        self.refused(exact + b" ", "receipt_size")
        self.refused(b"", "receipt_size")
        for bad in (base.decode(), bytearray(base), memoryview(base), None, 5):
            with self.subTest(type=type(bad).__name__):
                self.refused(bad, "receipt_size")
        self.refused(b" " * (limit + 1), "receipt_size")


class InventoryTests(Base):
    def test_exact_eight_sets(self):
        self.assertEqual(schema.GATE_NAMES, frozenset(fx.GATE_NAMES))
        self.assertEqual(len(schema.GATE_NAMES), 8)
        self.assertEqual(schema.COMMAND_GATES, frozenset(fx.COMMAND_GATES))
        self.assertEqual(len(schema.COMMAND_GATES), 8)
        self.assertEqual(len(fx.GATE_NAMES), len(set(fx.GATE_NAMES)))
        self.assertEqual(len(fx.COMMAND_GATES), len(set(fx.COMMAND_GATES)))

    def test_gate_records(self):
        path = ["gates", "records"]
        doc = fx.receipt()
        doc["gates"]["records"].reverse()
        schema.parse_receipt(encode(doc))
        doc = fx.receipt()
        doc["gates"]["records"][7]["name"] = "roles"
        self.refused_doc(doc, "gate_inventory")
        doc = fx.receipt()
        doc["gates"]["records"][7]["name"] = "extra_gate"
        self.refused_doc(doc, "enum")
        doc = fx.receipt()
        doc["gates"]["records"][0]["name"] = "Schema_Plan"
        self.refused_doc(doc, "enum")
        self.refused_doc(mutated(path, get(fx.receipt(), path)[:7]), "array_size")
        self.refused_doc(mutated(path, get(fx.receipt(), path) * 2), "array_size")
        self.refused_doc(mutated(path, []), "array_size")

    def test_command_gates(self):
        path = ["observations", "command_gates"]
        doc = fx.receipt()
        doc["observations"]["command_gates"].reverse()
        schema.parse_receipt(encode(doc))
        doc = fx.receipt()
        doc["observations"]["command_gates"][7] = "rsvp"
        self.refused_doc(doc, "command_inventory")
        doc = fx.receipt()
        doc["observations"]["command_gates"][0] = "feed-delete"
        self.refused_doc(doc, "enum")
        self.refused_doc(mutated(path, fx.COMMAND_GATES[:7]), "array_size")
        self.refused_doc(mutated(path, fx.COMMAND_GATES + ["rsvp"]), "array_size")
        self.refused_doc(mutated(path, [x.upper() for x in fx.COMMAND_GATES]), "enum")

    def test_gate_result_literals(self):
        path = ["gates", "records", 3, "result"]
        schema.parse_receipt(encode(mutated(path, "failure")))
        for bad in ("pass", "fail", "Success", "SUCCESS", "ok", "", True, None, 1):
            with self.subTest(result=bad):
                self.refused_doc(mutated(path, bad), "enum")

    def test_result_literals_and_consistency(self):
        base = ["result"]
        for bad in ("Pass", "success", "failure", "", None, True):
            with self.subTest(status=bad):
                self.refused_doc(mutated(base + ["status"], bad), "enum")
        pass_with_reason = mutated(base, {"status": "pass", "reason_codes": ["gate"],
                                          "discrepancy_refs": []})
        self.refused_doc(pass_with_reason, "result")
        pass_with_ref = mutated(base, {"status": "pass", "reason_codes": [],
                                       "discrepancy_refs": ["gap-ref"]})
        self.refused_doc(pass_with_ref, "result")
        for status in ("fail", "hold"):
            good = {"status": status, "reason_codes": ["gate", "sample"],
                    "discrepancy_refs": ["gap-ref"]}
            with self.subTest(status=status):
                self.assertEqual(
                    schema.parse_receipt(encode(mutated(base, good)))["result"], good)
                self.refused_doc(mutated(base, dict(good, reason_codes=[])), "result")
                self.refused_doc(mutated(base, dict(good, discrepancy_refs=[])), "result")
                self.refused_doc(mutated(base, dict(good, reason_codes=["gate", "gate"])),
                                 "result")
                self.refused_doc(mutated(base, dict(good, reason_codes=["bogus"])), "enum")
                self.refused_doc(mutated(base, dict(good, reason_codes=["Gate"])), "enum")
        self.assertEqual(len(schema.REASON_CODES), 19)
        every = {"status": "fail", "reason_codes": sorted(schema.REASON_CODES),
                 "discrepancy_refs": ["gap-ref"]}
        schema.parse_receipt(encode(mutated(base, every)))
        self.refused_doc(mutated(base, dict(every, reason_codes=every["reason_codes"] + ["gate"])),
                         "array_size")

    def test_fixed_literals(self):
        self.refused_doc(mutated(["schema"], "two.bot.announcement-candidate-staging.v2"),
                         "enum")
        self.refused_doc(mutated(["purpose"], "promotion"), "enum")
        self.refused_doc(mutated(["scope", "pr"], "1640"), "enum")
        self.refused_doc(mutated(["scope", "pr"], 1639), "enum")
        self.refused_doc(mutated(["scope", "base_ref"], "develop"), "enum")
        self.refused_doc(mutated(["attempt", "event"], "push"), "enum")
        self.refused_doc(mutated(["authority", "decisions", 0, "outcome"], "rejected"), "enum")
        self.refused_doc(mutated(["authority", "decisions", 0, "role"], "dev"), "enum")
        doc = fx.receipt()
        del doc["authority"]["decisions"][1]
        self.refused_doc(doc, "array_size")
        doc = fx.receipt()
        doc["attempt"]["locks"].pop()
        self.refused_doc(doc, "array_size")


class TimestampTests(unittest.TestCase):
    def test_utc_accepts_canonical(self):
        value = schema.utc("2026-01-02T03:04:05Z")
        self.assertEqual(value, datetime.datetime(2026, 1, 2, 3, 4, 5,
                                                  tzinfo=datetime.timezone.utc))
        self.assertEqual(value.utcoffset(), datetime.timedelta(0))
        self.assertEqual(schema.utc("2024-02-29T23:59:59Z").day, 29)

    def test_utc_rejects_non_canonical(self):
        for bad in ("2026-01-02T03:04:05+00:00", "2026-01-02T03:04:05.000Z",
                    "2026-01-02T03:04:05z", "2026-01-02t03:04:05Z", "2026-01-02 03:04:05Z",
                    "2026-01-02T03:04:05", "2026-01-02T03:04:05+0100", "2026-01-02T03:04Z",
                    "2026-01-02T03:04:05Z\n", " 2026-01-02T03:04:05Z", "", "0",
                    "\u0662026-01-02T03:04:05Z", "2026-01-02T03:04:05Z\u200b"):
            with self.subTest(value=bad):
                with self.assertRaises(schema.Refusal) as ctx:
                    schema.utc(bad)
                self.assertEqual(ctx.exception.code, "text_format")

    def test_utc_rejects_impossible_dates(self):
        for bad in ("2026-02-30T00:00:00Z", "2026-13-01T00:00:00Z", "2026-00-10T00:00:00Z",
                    "2026-01-32T00:00:00Z", "2026-01-01T24:00:00Z", "2026-01-01T00:60:00Z",
                    "2026-01-01T00:00:60Z", "2025-02-29T00:00:00Z"):
            with self.subTest(value=bad):
                with self.assertRaises(schema.Refusal) as ctx:
                    schema.utc(bad)
                self.assertEqual(ctx.exception.code, "timestamp")

    def test_utc_rejects_non_strings(self):
        for bad in (None, 0, 1.5, True, b"2026-01-02T03:04:05Z",
                    datetime.datetime(2026, 1, 1)):
            with self.subTest(value=bad):
                with self.assertRaises(schema.Refusal):
                    schema.utc(bad)

    def test_fixture_timestamps_parse_via_utc(self):
        self.assertLess(schema.utc(fx.T0), schema.utc(fx.T1))
        self.assertLess(schema.utc(fx.T2), schema.utc(fx.T3))

    def test_receipt_rejects_calendar_invalid_timestamps(self):
        for path in (["attempt", "started_at"], ["authority", "reads", 0, "read_at"],
                     ["resources", "entries", 0, "expires_at"],
                     ["observations", "cases", 0, "ended_at"]):
            for bad in ("2026-02-30T00:00:00Z", "2026-01-01T24:00:00Z"):
                with self.subTest(path=path, bad=bad), self.assertRaises(schema.Refusal) as ctx:
                    schema.parse_receipt(encode(mutated(path, bad)))
                self.assertEqual(ctx.exception.code, "timestamp")

    def test_all_worker_compatibility_dates_are_calendar_valid(self):
        for path in (["build", "worker", "compatibility_date"],
                     ["lifecycle", "worker_before", "compatibility_date"],
                     ["lifecycle", "worker_after", "compatibility_date"]):
            schema.parse_receipt(encode(mutated(path, "2024-02-29")))
            for bad in ("2026-99-99", "2025-02-29", "2026-04-31", "0000-01-01"):
                with self.subTest(path=path, bad=bad), self.assertRaises(schema.Refusal) as ctx:
                    schema.parse_receipt(encode(mutated(path, bad)))
                self.assertEqual(ctx.exception.code, "date")

    def test_receipt_timestamp_shape(self):
        for bad in ("2026-01-01T00:00:00+00:00", "2026-01-01T00:00:00.5Z"):
            doc = mutated(["attempt", "started_at"], bad)
            with self.assertRaises(schema.Refusal) as ctx:
                schema.parse_receipt(encode(doc))
            self.assertEqual(ctx.exception.code, "text_format")


class NoEchoTests(unittest.TestCase):
    def attempts(self):
        s = SENTINEL
        yield "bad_text", encode(mutated(["scope", "task_ref"], s + "!"))
        yield "bad_enum", encode(mutated(["scope", "pr"], s))
        yield "bad_status", encode(mutated(["result", "status"], s))
        yield "bad_reason", encode(mutated(["result"], {
            "status": "fail", "reason_codes": [s], "discrepancy_refs": ["gap-ref"]}))
        yield "unknown_key", encode(mutated([s], 1))
        yield "unknown_nested_key", encode(mutated(["scope", s], s))
        yield "missing_with_value", encode(mutated(["scope", "extra"], s))
        yield "union_tag", encode(mutated(["resources", "states", 0, "presence"], s))
        yield "gate_name", encode(mutated(["gates", "records", 0, "name"], s))
        yield "command_gate", encode(mutated(["observations", "command_gates", 0], s))
        yield "count_string", encode(mutated(["resources", "entries", 0, "expected_count"], s))
        yield "oversize_text", encode(mutated(["scope", "task_ref"], s + "a" * 5000))
        yield "oversize_key", encode({s + "k" * 5000: 1})
        yield "duplicate_value", ('{"a":"%s","a":"%s"}' % (s, s)).encode()
        yield "duplicate_key", ('{"%s":1,"%s":2}' % (s, s)).encode()
        yield "escaped_duplicate_key", ('{"S3CRET":1,"S3\\u0043RET":"%s"}' % s).encode()
        yield "surrogate", ('{"a":"%s\\ud800"}' % s).encode()
        yield "syntax", ('{"a":"%s",}' % s).encode()
        yield "trailing", ('{"a":1} %s' % s).encode()
        yield "constant", ('{"a":NaN,"b":"%s"}' % s).encode()
        yield "bigint", ('{"a":%s,"b":"%s"}' % ("9" * 25, s)).encode()
        yield "depth", ('{"b":"%s","a":%s}' % (s, "[" * 20)).encode()
        yield "invalid_utf8", s.encode() + b"\xff"
        yield "unresolved", encode(mutated(["scope", "task_ref"], "unknown"))
        yield "inconsistent_pass", encode(mutated(["result"], {
            "status": "pass", "reason_codes": ["gate"], "discrepancy_refs": [s]}))

    def test_refusals_never_echo_sentinel(self):
        count = 0
        for label, raw in self.attempts():
            with self.subTest(case=label):
                with self.assertRaises(schema.Refusal) as ctx:
                    schema.parse_receipt(raw)
                err = ctx.exception
                rendered = "".join(traceback.format_exception(
                    type(err), err, err.__traceback__))
                for text in (str(err), repr(err), rendered, err.code, repr(err.args)):
                    self.assertNotIn(SENTINEL, text)
                    self.assertNotIn("S3CRET", text)
                self.assertEqual(str(err), "announcement evidence refused: " + err.code)
                self.assertRegex(err.code, r"^[a-z_]+$")
                self.assertIsNone(err.__cause__)
                count += 1
        self.assertGreater(count, 20)

    def test_utc_does_not_echo(self):
        with self.assertRaises(schema.Refusal) as ctx:
            schema.utc(SENTINEL)
        self.assertNotIn(SENTINEL, str(ctx.exception))
        self.assertNotIn(SENTINEL, repr(ctx.exception))

    def test_accepted_sentinel_free_input_has_no_side_channel(self):
        # Success returns only the parsed document; nothing is logged.
        doc = fx.receipt()
        self.assertEqual(schema.parse_receipt(encode(doc)), doc)


if __name__ == "__main__":
    unittest.main()
