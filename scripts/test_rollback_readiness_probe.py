"""Rollback-readiness probe regressions: local fixtures only, no network or databases."""

import contextlib
import gzip
import io
import json
import os
from pathlib import Path
import re
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import rollback_readiness_probe as probe  # noqa: E402

STAGING = "https://two-bot-next-staging.example-sub.workers.dev"
def readyz_body(*components):
    return json.dumps({"components": [list(c) for c in components], "jobs": {},
                       "build_revision": "unknown", "build_id": "unknown"}).encode()


# The bot's real /readyz bodies (crates/bot/src/server.rs), one per state the gate must classify.
READYZ = {
    "ready": (200, readyz_body(("process", "ready"), ("gateway", "ready"),
                               ("database", "ready"), ("token_invalid", "ready"))),
    "parked": (503, readyz_body(("process", "ready"), ("gateway", "starting"),
                                ("database", "ready"), ("token_invalid", "ready"))),
    "database down": (503, readyz_body(("process", "ready"), ("gateway", "ready"),
                                       ("database", "down"), ("token_invalid", "ready"))),
    "token invalid": (503, readyz_body(("process", "ready"), ("gateway", "ready"),
                                       ("database", "ready"), ("token_invalid", "down"))),
    "gateway and database down": (503, readyz_body(("process", "ready"), ("gateway", "down"),
                                                   ("database", "down"), ("token_invalid", "ready"))),
}
READY = READYZ["ready"]
CONFIG = """\
[vars]
BOT_PORT = "8080"

[env.staging.vars]
BOT_PORT = "8080"
TWO_GUILD_NAME = "TWO Staging"

[env.production.vars]
BOT_PORT = "8080"
TWO_GUILD_NAME = "TogetherWeOwn"
"""


def manifest(**overrides):
    obj = {
        "kind": "manifest", "version": 4, "createdAt": "2026-10-02T04:17:00Z",
        "tables": [
            {"name": "events", "columns": ["id", "guild_id"], "columnTypes": ["bigint", "text"], "count": 2},
            {"name": "rsvp_events", "columns": ["id"], "columnTypes": ["bigint"], "count": 1},
        ],
        "eventsSequence": 42, "schemaMigrations": ["1", "2"], "sequences": {"rsvp_events": 7},
    }
    obj.update(overrides)
    return obj


def archive_lines(head):
    rows = [{"kind": "row", "table": "events", "data": {"id": "41", "guild_id": "g"}},
            {"kind": "row", "table": "events", "data": {"id": "42", "guild_id": "g"}},
            {"kind": "row", "table": "rsvp_events", "data": {"id": "7"}}]
    return [json.dumps(head).encode(), *(json.dumps(r).encode() for r in rows),
            json.dumps({"kind": "end", "rows": 3}).encode()]


LIVE = [
    {"table": "events", "column": "id", "sequence": "public.events_id_seq", "last_value": 42, "readable": True},
    {"table": "rsvp_events", "column": "id", "sequence": "public.rsvp_events_id_seq",
     "last_value": None, "readable": True},
]


class Fixture:
    def __init__(self, root):
        self.root = Path(root)
        self.backups = self.root / "backups"
        self.backups.mkdir()
        self.sequences = self.root / "sequences.json"
        self.config = self.root / "wrangler.toml"
        self.write_archive("two-funnel-20261002T041700Z.ndjson.gz", archive_lines(manifest()))
        self.write_live(LIVE)
        self.config.write_text(CONFIG)

    def write_archive(self, name, lines, *, raw=None, mtime=2_000_000_000):
        path = self.backups / name
        path.write_bytes(raw if raw is not None else gzip.compress(b"\n".join(lines) + b"\n"))
        os.utime(path, (mtime, mtime))
        return path

    def write_live(self, rows):
        self.sequences.write_text(json.dumps(rows))

    def argv(self, *extra):
        return ["--backup-dir", str(self.backups), "--sequences", str(self.sequences),
                "--staging-url", STAGING, "--config", str(self.config), *extra]


class RollbackReadinessProbeTests(unittest.TestCase):
    def setUp(self):
        # Any real HTTP is a test failure: the probe only gets fake fetches here.
        guard = mock.patch.object(probe.urllib.request.OpenerDirector, "open",
                                  side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        self.fresh()

    def fresh(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.fx = Fixture(tmp.name)
        self.requested = []

    def fetch(self, response=READY):
        def fake(url):
            self.requested.append(url)
            if isinstance(response, Exception):
                raise response
            return response
        return fake

    def probe(self, *extra, fetch=None, env=None):
        out = io.StringIO()
        with mock.patch.dict(os.environ, env or {}, clear=True), contextlib.redirect_stdout(out):
            code = probe.main(self.fx.argv(*extra), fetch_fn=fetch or self.fetch())
        lines = out.getvalue().splitlines()
        return code, {line.split(":", 1)[0]: line for line in lines[:-1]}, lines[-1]

    def assert_only_failure(self, name, reason, code, results):
        self.assertEqual(code, 1)
        self.assertEqual(set(results), {f"{'FAIL' if n == name else 'PASS'} {n}"
                                        for n in ("manifest", "sequences", "readyz", "deploy-config")})
        self.assertIn(reason, results[f"FAIL {name}"])

    def snapshot(self):
        return {p: (p.stat().st_mtime_ns, p.read_bytes())
                for p in sorted(self.fx.root.rglob("*")) if p.is_file()}

    def test_all_pass(self):
        before = self.snapshot()
        code, results, summary = self.probe()
        self.assertEqual(code, 0, results)
        self.assertEqual(set(results), {"PASS manifest", "PASS sequences", "PASS readyz",
                                        "PASS deploy-config"})
        self.assertIn("two-funnel-20261002T041700Z.ndjson.gz v4, 2 tables", results["PASS manifest"])
        self.assertIn("covers all 2 live sequences", results["PASS sequences"])
        self.assertIn("staging, production declare TWO_GUILD_NAME", results["PASS deploy-config"])
        self.assertEqual(summary, "rollback readiness: 4/4 checks passed")
        self.assertEqual(self.requested, [STAGING + "/readyz"])
        self.assertEqual(self.snapshot(), before, "the probe must never write")

    def readyz(self, status, body):
        _, results, _ = self.probe(fetch=self.fetch((status, body)))
        return next(line for key, line in results.items() if key.endswith(" readyz"))

    def test_every_real_server_state_is_accepted(self):
        for label in ("ready", "parked"):
            with self.subTest(label):
                self.assertTrue(self.readyz(*READYZ[label]).startswith("PASS readyz"))

    def test_database_and_token_faults_are_never_parked(self):
        for label, component in (("database down", "database down"),
                                 ("token invalid", "token_invalid down"),
                                 ("gateway and database down", "database down")):
            with self.subTest(label):
                line = self.readyz(*READYZ[label])
                self.assertTrue(line.startswith("FAIL readyz"), line)
                self.assertIn(component, line)

    def test_parked_staging_gateway_is_a_truthful_response(self):
        self.assertIn("gateway starting (parked, not E2E approval)", self.readyz(*READYZ["parked"]))

    def test_readyz_is_classified_by_component_name_and_status(self):
        process, gateway = ("process", "ready"), ("gateway", "ready")
        database, token = ("database", "ready"), ("token_invalid", "ready")
        accepted = {
            "503 with gateway starting": (503, readyz_body(process, ("gateway", "starting"), database, token)),
            "503 with gateway down": (503, readyz_body(process, ("gateway", "down"), database, token)),
        }
        rejected = {
            "200 without database or token_invalid": (200, readyz_body(process, gateway),
                                                      "lacks database, token_invalid"),
            "200 with gateway starting": (200, readyz_body(process, ("gateway", "starting"), database, token),
                                          "unexpected components"),
            "200 with database down": (200, readyz_body(process, gateway, ("database", "down"), token),
                                       "unexpected components"),
            "200 with token invalid down": (200, readyz_body(process, gateway, database, ("token_invalid", "down")),
                                            "unexpected components"),
            "200 with an unknown ready component": (200, readyz_body(process, gateway, database, token,
                                                                    ("voice", "ready")),
                                                   "carries an unknown component name"),
            "200 with an unknown component down": (200, readyz_body(process, gateway, database, token,
                                                                    ("voice", "down")),
                                                   "carries an unknown component name"),
            "500 with a complete body": (500, readyz_body(process, gateway, database, token),
                                         "/readyz 500 with unexpected"),
            "503 with every component ready": (503, readyz_body(process, gateway, database, token),
                                               "unexpected components"),
            "503 with process down": (503, readyz_body(("process", "down"), ("gateway", "down"), database, token),
                                      "unexpected components"),
            "503 with an unknown component down": (503, readyz_body(process, ("gateway", "starting"), database,
                                                                    token, ("voice", "down")),
                                                   "carries an unknown component name"),
            "503 with an unknown ready component": (503, readyz_body(process, gateway, database, token,
                                                                    ("voice", "ready")),
                                                   "carries an unknown component name"),
            "503 with gateway starting and token invalid down": (503, readyz_body(
                process, ("gateway", "starting"), database, ("token_invalid", "down")), "unexpected components"),
            "503 with gateway and database down": (503, readyz_body(
                process, ("gateway", "down"), ("database", "down"), token), "unexpected components"),
            "503 with database and token invalid down": (503, readyz_body(
                process, gateway, ("database", "down"), ("token_invalid", "down")), "unexpected components"),
            "503 without database or token_invalid": (503, readyz_body(process, ("gateway", "starting")),
                                                      "lacks database, token_invalid"),
            "missing process": (200, readyz_body(gateway, database, token), "lacks process"),
            "missing gateway": (200, readyz_body(process, database, token), "lacks gateway"),
            "empty breakdown": (200, readyz_body(), "lacks process, gateway, database, token_invalid"),
            "duplicate component": (200, readyz_body(process, gateway, gateway), "repeats a component name"),
            "status outside ready, starting, down": (200, readyz_body(process, ("gateway", "ok")),
                                                     "without the bot's component breakdown"),
            "row of the wrong length": (200, readyz_body(("process",)), "without the bot's component breakdown"),
            "ownership refusal carrying components": (503, json.dumps(
                {"error": "ownership_fenced", "components": [list(process), list(gateway)]}).encode(),
                "without the bot's component breakdown"),
            "non-JSON body": (503, b"<html>not the bot</html>", "without the bot's component breakdown"),
        }
        for label, (status, body) in accepted.items():
            with self.subTest(label):
                self.assertTrue(self.readyz(status, body).startswith("PASS readyz"))
        for label, (status, body, reason) in rejected.items():
            with self.subTest(label):
                line = self.readyz(status, body)
                self.assertTrue(line.startswith("FAIL readyz"), line)
                self.assertIn(reason, line)

    def test_unknown_component_refused_without_echoing_data(self):
        sentinel = "SENTINEL_unknown_rollback_abc123"
        for status, extra in ((200, (sentinel, "ready")),
                              (503, (sentinel, "down")),
                              (200, (sentinel, "starting"))):
            with self.subTest(status=status, extra=extra):
                body = readyz_body(("process", "ready"), ("gateway", "ready"),
                                   ("database", "ready"), ("token_invalid", "ready"),
                                   extra)
                line = self.readyz(status, body)
                self.assertTrue(line.startswith("FAIL readyz"), line)
                self.assertIn("carries an unknown component name", line)
                self.assertNotIn(sentinel, line)

    def test_newest_archive_by_mtime_skips_decoys(self):
        self.fx.write_archive("two-funnel-zz-older.ndjson.gz", [b"not json"], mtime=1_000_000_000)
        self.fx.write_archive("two-funnel-20261002T051700Z.ndjson.gz", archive_lines(manifest()),
                              mtime=2_050_000_000)
        self.fx.write_archive(".two-funnel-x.ndjson.gz.tmp", [b"not json"], mtime=2_100_000_000)
        self.fx.write_archive("two-funnel-x.ndjson.gz.partial", [b"not json"], mtime=2_100_000_000)
        self.fx.write_archive("not-two-funnel.ndjson.gz", [b"not json"], mtime=2_100_000_000)
        code, results, _ = self.probe()
        self.assertEqual(code, 0, results)
        self.assertIn("two-funnel-20261002T051700Z.ndjson.gz v4", results["PASS manifest"])

    def test_current_dump_version_5_manifest_passes(self):
        self.fx.write_archive("two-funnel-20261002T051700Z.ndjson.gz", archive_lines(manifest(version=5)),
                              mtime=2_050_000_000)
        code, results, _ = self.probe()
        self.assertEqual(code, 0, results)
        self.assertIn("two-funnel-20261002T051700Z.ndjson.gz v5", results["PASS manifest"])

    def test_current_dump_version_6_manifest_passes(self):
        self.fx.write_archive("two-funnel-20261002T051700Z.ndjson.gz", archive_lines(manifest(version=6)),
                              mtime=2_060_000_000)
        code, results, _ = self.probe()
        self.assertEqual(code, 0, results)
        self.assertIn("two-funnel-20261002T051700Z.ndjson.gz v6", results["PASS manifest"])

    def test_current_dump_version_7_manifest_passes(self):
        self.fx.write_archive("two-funnel-20261002T051700Z.ndjson.gz", archive_lines(manifest(version=7)),
                              mtime=2_070_000_000)
        code, results, _ = self.probe()
        self.assertEqual(code, 0, results)
        self.assertIn("two-funnel-20261002T051700Z.ndjson.gz v7", results["PASS manifest"])

    def test_current_dump_version_8_manifest_passes(self):
        self.fx.write_archive("two-funnel-20261002T051700Z.ndjson.gz", archive_lines(manifest(version=8)),
                              mtime=2_080_000_000)
        code, results, _ = self.probe()
        self.assertEqual(code, 0, results)
        self.assertIn("two-funnel-20261002T051700Z.ndjson.gz v8", results["PASS manifest"])

    def test_current_dump_version_9_manifest_passes(self):
        self.fx.write_archive("two-funnel-20261002T051700Z.ndjson.gz", archive_lines(manifest(version=9)),
                              mtime=2_090_000_000)
        code, results, _ = self.probe()
        self.assertEqual(code, 0, results)
        self.assertIn("two-funnel-20261002T051700Z.ndjson.gz v9", results["PASS manifest"])

    def test_current_dump_version_10_manifest_passes(self):
        self.fx.write_archive("two-funnel-20261002T051700Z.ndjson.gz", archive_lines(manifest(version=10)),
                              mtime=2_100_000_000)
        code, results, _ = self.probe()
        self.assertEqual(code, 0, results)
        self.assertIn("two-funnel-20261002T051700Z.ndjson.gz v10", results["PASS manifest"])

    def test_current_dump_version_11_manifest_passes(self):
        self.fx.write_archive("two-funnel-20261002T051700Z.ndjson.gz", archive_lines(manifest(version=11)),
                              mtime=2_110_000_000)
        code, results, _ = self.probe()
        self.assertEqual(code, 0, results)
        self.assertIn("two-funnel-20261002T051700Z.ndjson.gz v11", results["PASS manifest"])

    def test_current_dump_version_12_manifest_passes(self):
        self.fx.write_archive("two-funnel-20261002T051700Z.ndjson.gz", archive_lines(manifest(version=12)),
                              mtime=2_120_000_000)
        code, results, _ = self.probe()
        self.assertEqual(code, 0, results)
        self.assertIn("two-funnel-20261002T051700Z.ndjson.gz v12", results["PASS manifest"])

    def test_current_dump_version_13_manifest_passes(self):
        self.fx.write_archive("two-funnel-20261002T051700Z.ndjson.gz", archive_lines(manifest(version=13)),
                              mtime=2_130_000_000)
        code, results, _ = self.probe()
        self.assertEqual(code, 0, results)
        self.assertIn("two-funnel-20261002T051700Z.ndjson.gz v13", results["PASS manifest"])

    def test_current_dump_version_14_manifest_passes(self):
        self.fx.write_archive("two-funnel-20261002T051700Z.ndjson.gz", archive_lines(manifest(version=14)),
                              mtime=2_140_000_000)
        code, results, _ = self.probe()
        self.assertEqual(code, 0, results)
        self.assertIn("two-funnel-20261002T051700Z.ndjson.gz v14", results["PASS manifest"])

    def test_current_dump_version_15_manifest_passes(self):
        self.fx.write_archive("two-funnel-20261002T051700Z.ndjson.gz", archive_lines(manifest(version=15)),
                              mtime=2_150_000_000)
        code, results, _ = self.probe()
        self.assertEqual(code, 0, results)
        self.assertIn("two-funnel-20261002T051700Z.ndjson.gz v15", results["PASS manifest"])

    def test_current_dump_version_16_manifest_passes(self):
        self.fx.write_archive("two-funnel-20261002T051700Z.ndjson.gz", archive_lines(manifest(version=16)),
                              mtime=2_160_000_000)
        code, results, _ = self.probe()
        self.assertEqual(code, 0, results)
        self.assertIn("two-funnel-20261002T051700Z.ndjson.gz v16", results["PASS manifest"])

    def assert_invalid_created_at(self, value):
        self.fx.write_archive("two-funnel-20261002T041700Z.ndjson.gz",
                              archive_lines(manifest(createdAt=value)))
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code, results, summary = self.probe()
        self.assertEqual(err.getvalue(), "")
        self.assertEqual(code, 1)
        self.assertEqual(set(results), {"FAIL manifest", "FAIL sequences", "PASS readyz",
                                        "PASS deploy-config"})
        self.assertEqual(results["FAIL manifest"],
                         "FAIL manifest: manifest has invalid createdAt timestamp")
        self.assertEqual(results["FAIL sequences"],
                         "FAIL sequences: no valid backup manifest to compare against")
        self.assertEqual(summary, "rollback readiness: 2/4 checks passed")
        self.assertNotIn(value, "\n".join(results.values()))

    def test_unicode_digit_created_at_refuses_without_echo(self):
        timestamp = "2026-10-02T04:17:00.123456789+23:59"
        for digits in ("٠١٢٣٤٥٦٧٨٩", "０１２３４５６７８９"):
            translation = str.maketrans("0123456789", digits)
            for match in re.finditer(r"[0-9]+", timestamp):
                value = (timestamp[:match.start()] + match[0].translate(translation)
                         + timestamp[match.end():])
                with self.subTest(value=value):
                    self.assert_invalid_created_at(value)

    def test_out_of_range_created_at_offsets_refuse_without_echo(self):
        for offset in ("+24:00", "-24:00", "+23:60", "-23:60"):
            with self.subTest(offset=offset):
                self.assert_invalid_created_at("2026-10-02T04:17:00" + offset)

    def test_invalid_created_at_fraction_lengths_refuse_without_echo(self):
        for fraction in ("", "1234567890"):
            for zone in ("Z", "+23:59", "-23:59"):
                value = f"2026-10-02T04:17:00.{fraction}{zone}"
                with self.subTest(value=value):
                    self.assertLess(len(value), probe.CREATED_AT_MAX_LEN)
                    self.assert_invalid_created_at(value)

    def test_created_at_ascii_fraction_and_offset_boundaries_pass(self):
        for fraction in ("", ".1", ".123456789"):
            for zone in ("Z", "+00:00", "-00:00", "+23:59", "-23:59"):
                value = f"2026-10-02T04:17:00{fraction}{zone}"
                with self.subTest(value=value):
                    self.fx.write_archive("two-funnel-20261002T041700Z.ndjson.gz",
                                          archive_lines(manifest(createdAt=value)))
                    code, results, summary = self.probe()
                    self.assertEqual(code, 0, results)
                    self.assertIn(f"createdAt {value}, eventsSequence", results["PASS manifest"])
                    self.assertEqual(summary, "rollback readiness: 4/4 checks passed")

    def test_decompressed_reader_stops_at_cap_plus_one(self):
        cap = 16
        for prefix in (b"", b"x" * (cap - 2) + b"\n"):
            with self.subTest(prefix_bytes=len(prefix)), mock.patch.object(probe, "DECOMPRESSED_CAP", cap):
                raw = io.BytesIO(prefix + b"x" * 64)
                stream = probe.BudgetedStream(raw)
                if prefix:
                    self.assertEqual(probe.read_line(stream), prefix)
                with self.assertRaisesRegex(probe.ProbeError, "^archive exceeds decompressed byte cap$"):
                    probe.read_line(stream)
                self.assertEqual(raw.tell(), cap + 1)
                self.assertEqual(stream.total, cap + 1)

    def test_single_failure_fails_only_that_check(self):
        missing_guild = CONFIG.replace('TWO_GUILD_NAME = "TogetherWeOwn"\n', "")
        cases = {
            "live allocator past manifest": (
                lambda: self.fx.write_live([{**LIVE[0], "last_value": 43}, LIVE[1]]),
                {}, "sequences", "events.id (live 43 > manifest 42, would rewind)"),
            "sequence with no manifest high-water": (
                lambda: self.fx.write_live([*LIVE, {"table": "events", "column": "seq2",
                                                    "sequence": "public.events_seq2_seq",
                                                    "last_value": 1, "readable": True}]),
                {}, "sequences", "events.seq2 (no manifest high-water)"),
            "sequence on a table outside the backup": (
                lambda: self.fx.write_live([*LIVE, {"table": "rollback_journal", "column": "id",
                                                    "sequence": "public.rollback_journal_id_seq",
                                                    "last_value": 3, "readable": True}]),
                {}, "sequences", "rollback_journal.id (table not in the backup)"),
            "unreadable live sequence": (
                lambda: self.fx.write_live([{**LIVE[0], "last_value": None, "readable": False}, LIVE[1]]),
                {}, "sequences", "no SELECT/USAGE on public.events_id_seq"),
            "empty live result": (
                lambda: self.fx.write_live([]), {}, "sequences", "returned no sequences"),
            "live result has the wrong shape": (
                lambda: self.fx.sequences.write_text('{"events": 42}'),
                {}, "sequences", "is not a JSON array"),
            "readyz ownership refusal": (
                lambda: None, {"fetch": (503, b'{"error":"not owner"}')},
                "readyz", "without the bot's component breakdown"),
            "readyz broken deploy": (
                lambda: None, {"fetch": (500, b"error code: 1101")},
                "readyz", "/readyz 500 without"),
            "readyz no response": (
                lambda: None, {"fetch": TimeoutError("timed out")},
                "readyz", "did not respond (TimeoutError)"),
            "readyz gateway down at 200": (
                lambda: None,
                {"fetch": (200, readyz_body(("process", "ready"), ("gateway", "down"),
                                            ("database", "ready"), ("token_invalid", "ready")))},
                "readyz", "unexpected components"),
            "production env lacks TWO_GUILD_NAME": (
                lambda: self.fx.config.write_text(missing_guild),
                {}, "deploy-config", "[env.production.vars] lacks TWO_GUILD_NAME"),
            "top-level TWO_ var not repeated": (
                lambda: self.fx.config.write_text(CONFIG.replace("[vars]\n", '[vars]\nTWO_AUTOMOD = "1"\n')),
                {}, "deploy-config", "lacks TWO_AUTOMOD"),
            "empty TWO_ value": (
                lambda: self.fx.config.write_text(CONFIG.replace('"TWO Staging"', '""')),
                {}, "deploy-config", "[env.staging.vars] lacks TWO_GUILD_NAME"),
            "missing env section": (
                lambda: None, {"argv": ("--env", "staging", "--env", "canary")},
                "deploy-config", "[env.canary] is missing"),
            "extra required key": (
                lambda: None, {"argv": ("--require", "TWO_MODERATION")},
                "deploy-config", "lacks TWO_MODERATION"),
            "int-digit overflow in wrangler config": (
                lambda: self.fx.config.write_text(CONFIG.replace("[vars]\n", "[vars]\nDIGITS = " + "1" * 4301 + "\n")),
                {}, "deploy-config", "is not valid TOML"),
            "deeply nested wrangler config": (
                lambda: self.fx.config.write_text("a = " + "[" * 1000 + "]" * 1000 + "\n"),
                {}, "deploy-config", "is not valid TOML"),
        }
        for label, (arrange, opts, name, reason) in cases.items():
            with self.subTest(label):
                self.fresh()
                arrange()
                fetch = self.fetch(opts["fetch"]) if "fetch" in opts else None
                code, results, summary = self.probe(*opts.get("argv", ()), fetch=fetch)
                self.assert_only_failure(name, reason, code, results)
                self.assertEqual(summary, "rollback readiness: 3/4 checks passed")

    def test_standalone_settings_sequence_without_mark_fails(self):
        head = manifest(tables=[
            {"name": "events", "columns": ["id", "guild_id"], "columnTypes": ["bigint", "text"], "count": 2},
            {"name": "rsvp_events", "columns": ["id"], "columnTypes": ["bigint"], "count": 1},
            {"name": "guild_settings", "columns": ["version"], "columnTypes": ["bigint"], "count": 0},
        ])
        self.fx.write_archive("two-funnel-20261002T041700Z.ndjson.gz", archive_lines(head),
                              mtime=2_000_000_000)
        self.fx.write_live([*LIVE, {"table": "guild_settings", "column": "version",
                                    "sequence": "public.guild_settings_version_seq",
                                    "last_value": 500, "readable": True}])
        code, results, _ = self.probe()
        self.assert_only_failure("sequences", "guild_settings.version (no manifest high-water)",
                                 code, results)

    def test_malformed_staging_url_fails_check_without_crashing(self):
        code, results, _ = self.probe("--staging-url", "https://[::1")
        self.assert_only_failure("readyz", "refusing: not the two-bot-next-staging", code, results)

    def test_malformed_manifest_fails_manifest_and_sequences(self):
        def lines(**overrides):
            return archive_lines(manifest(**overrides))

        good = archive_lines(manifest())
        cases = {
            "not gzip": (None, b"plain text, not gzip", "not a complete gzip archive"),
            "truncated gzip": (None, gzip.compress(b"\n".join(good))[:-12], "not a complete gzip archive"),
            "empty archive": ([], None, "is empty"),
            "manifest not JSON": ([b"{manifest", *good[1:]], None, "manifest line is not JSON"),
            "row before manifest": (good[1:], None, "first line is not a manifest"),
            "unknown version": (lines(version=17), None, "manifest version 17 is not one of (3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16)"),
            "boolean version": (lines(version=True), None, "manifest version True"),
            "no createdAt": (lines(createdAt=None), None, "no createdAt timestamp"),
            "negative eventsSequence": (lines(eventsSequence=-1), None, "invalid eventsSequence"),
            "float eventsSequence": (lines(eventsSequence=42.0), None, "invalid eventsSequence"),
            "migrations not strings": (lines(schemaMigrations=[1]), None, "invalid schemaMigrations"),
            "sequences not an object": (lines(sequences=[7]), None, "invalid sequences"),
            "negative sequence mark": (lines(sequences={"rsvp_events": -1}), None,
                                       "rsvp_events has an invalid high-water mark"),
            "no tables": (lines(tables=[]), None, "no table list"),
            "duplicate table": (lines(tables=[manifest()["tables"][0]] * 2), None, "events is duplicated"),
            "bad row count": (lines(tables=[{**manifest()["tables"][0], "count": "2"}]), None,
                              "events has an invalid row count"),
            "no end marker": (good[:-1], None, "has no end marker: truncated"),
            "end rows mismatch": ([*good[:-1], b'{"kind":"end","rows":4}'], None,
                                  "end marker says 4 rows, manifest counts 3"),
        }
        for label, (archive, raw, reason) in cases.items():
            with self.subTest(label):
                self.fresh()
                self.fx.write_archive("two-funnel-20261002T041700Z.ndjson.gz", archive, raw=raw)
                code, results, summary = self.probe()
                self.assertEqual(code, 1)
                self.assertIn(reason, results["FAIL manifest"])
                self.assertEqual(results["FAIL sequences"],
                                 "FAIL sequences: no valid backup manifest to compare against")
                self.assertIn("PASS readyz", results)
                self.assertIn("PASS deploy-config", results)
                self.assertEqual(summary, "rollback readiness: 2/4 checks passed")

    def test_missing_archive_and_inputs_fail_closed(self):
        for name in os.listdir(self.fx.backups):
            (self.fx.backups / name).unlink()
        argv = ["--config", str(self.fx.config)]
        out = io.StringIO()
        with mock.patch.dict(os.environ, {}, clear=True), contextlib.redirect_stdout(out):
            self.assertEqual(probe.main(argv, fetch_fn=self.fetch()), 1)
        text = out.getvalue()
        self.assertIn("FAIL manifest: no backup directory given", text)
        self.assertIn("FAIL readyz: no staging Worker URL given", text)
        self.assertEqual(self.requested, [])
        code, results, _ = self.probe()
        self.assertIn("no two-funnel-*.ndjson.gz archive", results["FAIL manifest"])

    def test_environment_defaults_are_read(self):
        argv = ["--sequences", str(self.fx.sequences), "--config", str(self.fx.config)]
        out = io.StringIO()
        env = {"TWO_BACKUP_DIR": str(self.fx.backups), "STAGING_WORKER_URL": STAGING + "/"}
        with mock.patch.dict(os.environ, env, clear=True), contextlib.redirect_stdout(out):
            code = probe.main(argv, fetch_fn=self.fetch())
        self.assertEqual(code, 0, out.getvalue())

    def test_refuses_every_non_staging_origin_without_a_request(self):
        for url in ("https://two-bot-next-production.example-sub.workers.dev",
                    "https://two-bot-next.example-sub.workers.dev",
                    "https://bot.togetherweown.com",
                    "http://two-bot-next-staging.example-sub.workers.dev",
                    "https://two-bot-next-staging.example-sub.workers.dev:8443",
                    "https://user@two-bot-next-staging.example-sub.workers.dev",
                    "https://two-bot-next-staging.example-sub.workers.dev.evil.test",
                    "https://two-bot-next-staging.example-sub.workers.dev/readyz",
                    "https://two-bot-next-staging.example-sub.workers.dev/?x=1"):
            with self.subTest(url):
                self.requested.clear()
                with mock.patch.dict(os.environ, {}, clear=True), contextlib.redirect_stdout(io.StringIO()):
                    results = probe.run(probe.parse_args(self.fx.argv("--staging-url", url)), self.fetch())
                readyz = next(r for r in results if r.name == "readyz")
                self.assertFalse(readyz.ok)
                self.assertIn("refusing: not the two-bot-next-staging workers.dev origin", readyz.reason)
                self.assertEqual(self.requested, [])

    def test_default_fetch_never_follows_redirects(self):
        handler = probe._NoRedirect()
        self.assertIsNone(handler.redirect_request(None, None, 302, "Found", {}, "https://elsewhere"))

    def test_rejects_non_two_require_key(self):
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            probe.parse_args(["--require", "DATABASE_URL"])

    def test_sequences_query_is_read_only(self):
        query = probe.SEQUENCES_QUERY.upper()
        self.assertTrue(query.lstrip().startswith("SELECT"))
        for verb in ("INSERT", "UPDATE", "DELETE", "ALTER", "SETVAL", "NEXTVAL", "TRUNCATE",
                     "CREATE", "DROP", "GRANT"):
            self.assertNotIn(verb, query)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(probe.main(["--print-sequences-query"]), 0)
        self.assertEqual(out.getvalue(), probe.SEQUENCES_QUERY)

    def test_repository_deploy_config_declares_required_keys(self):
        self.assertIn("TWO_GUILD_NAME",
                      probe.check_deploy_config(probe.DEFAULT_CONFIG, probe.DEFAULT_ENVS, []))


class ServerComponentDriftTests(unittest.TestCase):
    def test_gate_and_fixtures_name_exactly_the_components_the_server_serves(self):
        source = (probe.ROOT / "crates/bot/src/server.rs").read_text()
        served = set()
        for fn in ("readiness_report", "with_token_state"):
            match = re.search(rf"^fn {fn}\(.*?^\}}$", source, re.M | re.S)
            self.assertIsNotNone(match, f"server.rs no longer defines fn {fn}; update this drift guard")
            served.update(re.findall(r'"([a-z_]+)"\.to_owned\(\)', match.group(0)))
        fixtures = {name for _, body in READYZ.values() for name, _ in json.loads(body)["components"]}
        self.assertEqual(fixtures, served)
        self.assertEqual(set(probe.READYZ_REQUIRED), served)


class UserAgentTests(unittest.TestCase):
    def test_fetch_sends_an_explicit_user_agent(self):
        seen = []

        class Response(io.BytesIO):
            status = 200

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

        class Opener:
            def open(self, request, timeout=None):
                seen.append(request.get_header("User-agent"))
                return Response(b"{}")

        with mock.patch.object(probe.urllib.request, "build_opener", return_value=Opener()):
            probe.fetch("https://two-bot-next-staging.example-sub.workers.dev/readyz")
        self.assertEqual(seen, [probe.USER_AGENT])
        self.assertFalse(seen[0].startswith("Python-urllib"))


class DeepNestingRefusalTests(unittest.TestCase):
    """Excessively nested JSON fails closed, never aborting the probe.

    Fixtures stay under READYZ_BODY_CAP so the byte cap cannot mask the
    decoder backstop; shallow valid/malformed controls prove the decode
    entry points still classify ordinary input.
    """

    ARRAY_DEPTH = 10000
    OBJECT_DEPTH = 10000

    def setUp(self):
        guard = mock.patch.object(probe.urllib.request.OpenerDirector, "open",
                                  side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.fx = Fixture(tmp.name)
        self.requested = []

    def deep_array(self, depth=ARRAY_DEPTH):
        body = b"[" * depth + b"]" * depth
        self.assertLessEqual(len(body), probe.READYZ_BODY_CAP)
        return body

    def deep_object(self, depth=OBJECT_DEPTH):
        body = b'{"a":' * depth + b"0" + b"}" * depth
        self.assertLessEqual(len(body), probe.READYZ_BODY_CAP)
        return body

    def fetch(self, response):
        def fake(url):
            self.requested.append(url)
            if isinstance(response, Exception):
                raise response
            return response
        return fake

    def test_readyz_state_refuses_deeply_nested_array_and_object(self):
        for body in (self.deep_array(), self.deep_object()):
            with self.subTest(length=len(body)):
                with self.assertRaises(probe.ProbeError) as caught:
                    probe.readyz_state(200, body)
                self.assertIn("without the bot's component breakdown", str(caught.exception))

    def test_shallow_valid_and_malformed_readyz_controls(self):
        self.assertEqual(
            probe.readyz_state(*READY),
            {"process": "ready", "gateway": "ready",
             "database": "ready", "token_invalid": "ready"})
        with self.assertRaises(probe.ProbeError):
            probe.readyz_state(200, b"{not json")

    def test_manifest_line_overflow_refuses_as_not_json(self):
        deep = self.deep_array()
        self.fx.write_archive("two-funnel-20261002T041700Z.ndjson.gz",
                              [deep, *archive_lines(manifest())[1:]])
        with self.assertRaises(probe.ProbeError) as caught:
            probe.read_manifest(self.fx.backups / "two-funnel-20261002T041700Z.ndjson.gz")
        self.assertIn("manifest line is not JSON", str(caught.exception))

    def test_end_marker_overflow_refuses_as_truncated(self):
        deep = self.deep_array()
        lines = [*archive_lines(manifest())[:-1], deep]
        self.fx.write_archive("two-funnel-20261002T041700Z.ndjson.gz", lines)
        with self.assertRaises(probe.ProbeError) as caught:
            probe.read_manifest(self.fx.backups / "two-funnel-20261002T041700Z.ndjson.gz")
        self.assertIn("has no end marker", str(caught.exception))

    def test_live_sequences_overflow_refuses_as_not_json(self):
        deep = self.deep_array()
        self.fx.sequences.write_bytes(deep)
        with self.assertRaises(probe.ProbeError) as caught:
            probe.load_live_sequences(str(self.fx.sequences))
        self.assertIn("is not JSON", str(caught.exception))

    def int_digit_body(self):
        body = b'{"components":[],"n":' + b"1" * 4301 + b"}"
        self.assertLessEqual(len(body), probe.READYZ_BODY_CAP)
        return body

    def test_readyz_state_refuses_int_digit_overflow(self):
        with self.assertRaises(probe.ProbeError) as caught:
            probe.readyz_state(200, self.int_digit_body())
        self.assertIn("without the bot's component breakdown", str(caught.exception))

    def test_manifest_line_int_digit_overflow_refuses_as_not_json(self):
        self.fx.write_archive("two-funnel-20261002T041700Z.ndjson.gz",
                              [b"1" * 4301, *archive_lines(manifest())[1:]])
        with self.assertRaises(probe.ProbeError) as caught:
            probe.read_manifest(self.fx.backups / "two-funnel-20261002T041700Z.ndjson.gz")
        self.assertIn("manifest line is not JSON", str(caught.exception))

    def test_end_marker_int_digit_overflow_refuses_as_truncated(self):
        lines = [*archive_lines(manifest())[:-1], b"1" * 4301]
        self.fx.write_archive("two-funnel-20261002T041700Z.ndjson.gz", lines)
        with self.assertRaises(probe.ProbeError) as caught:
            probe.read_manifest(self.fx.backups / "two-funnel-20261002T041700Z.ndjson.gz")
        self.assertIn("has no end marker", str(caught.exception))

    def test_live_sequences_int_digit_overflow_refuses_as_not_json(self):
        self.fx.sequences.write_bytes(b"1" * 4301)
        with self.assertRaises(probe.ProbeError) as caught:
            probe.load_live_sequences(str(self.fx.sequences))
        self.assertIn("is not JSON", str(caught.exception))

    def test_live_sequences_invalid_utf8_refuses_as_not_json(self):
        self.fx.sequences.write_bytes(b"\xff\xfe[]")
        with self.assertRaises(probe.ProbeError) as caught:
            probe.load_live_sequences(str(self.fx.sequences))
        self.assertIn("is not JSON", str(caught.exception))

    def test_probe_main_with_int_digit_readyz_fails_closed_without_traceback(self):
        out = io.StringIO()
        with mock.patch.dict(os.environ, {}, clear=True), contextlib.redirect_stdout(out):
            code = probe.main(self.fx.argv(), fetch_fn=self.fetch((200, self.int_digit_body())))
        text = out.getvalue()
        self.assertEqual(code, 1)
        self.assertIn("FAIL readyz: /readyz 200 without the bot's component breakdown", text)
        self.assertIn("rollback readiness: 3/4 checks passed", text)
        self.assertNotIn("Traceback", text)
        self.assertEqual(self.requested, [STAGING + "/readyz"])

    def test_probe_main_with_deeply_nested_readyz_fails_closed_without_traceback(self):
        deep = self.deep_array()
        out = io.StringIO()
        with mock.patch.dict(os.environ, {}, clear=True), contextlib.redirect_stdout(out):
            code = probe.main(self.fx.argv(), fetch_fn=self.fetch((200, deep)))
        text = out.getvalue()
        self.assertEqual(code, 1)
        self.assertIn("FAIL readyz: /readyz 200 without the bot's component breakdown", text)
        self.assertIn("rollback readiness: 3/4 checks passed", text)
        self.assertNotIn("Traceback", text)
        self.assertNotIn("[[[[[", text)
        self.assertEqual(self.requested, [STAGING + "/readyz"])


class BodyCapRefusalTests(unittest.TestCase):
    """Oversized /readyz bodies are refused before JSON parsing (M4.42)."""

    def setUp(self):
        guard = mock.patch.object(probe.urllib.request.OpenerDirector, "open",
                                  side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        self.fresh()

    def fresh(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.fx = Fixture(tmp.name)
        self.requested = []

    def fetch(self, response=READY):
        def fake(url):
            self.requested.append(url)
            if isinstance(response, Exception):
                raise response
            return response
        return fake

    def probe(self, *extra, fetch=None, env=None):
        out = io.StringIO()
        with mock.patch.dict(os.environ, env or {}, clear=True), contextlib.redirect_stdout(out):
            code = probe.main(self.fx.argv(*extra), fetch_fn=fetch or self.fetch())
        lines = out.getvalue().splitlines()
        return code, {line.split(":", 1)[0]: line for line in lines[:-1]}, lines[-1]

    def padded(self, value, size):
        raw = json.dumps(value).encode()
        self.assertLessEqual(len(raw), size, "fixture must fit the target size")
        return raw + b" " * (size - len(raw))

    def valid_readyz(self, **extra):
        payload = {"components": [["process", "ready"], ["gateway", "ready"],
                                  ["database", "ready"], ["token_invalid", "ready"]],
                   "jobs": {}, "build_revision": "unknown", "build_id": "unknown"}
        payload.update(extra)
        return payload

    def test_exact_cap_valid_input_passes(self):
        body = self.padded(self.valid_readyz(), probe.READYZ_BODY_CAP)
        self.assertEqual(len(body), probe.READYZ_BODY_CAP)
        self.assertEqual(json.loads(body)["build_revision"], "unknown")
        code, results, _ = self.probe(fetch=self.fetch((200, body)))
        self.assertEqual(code, 0, results)
        self.assertIn("PASS readyz", results)

    def test_valid_json_padded_past_cap_refuses_before_parsing(self):
        sentinel = "SENTINEL_BODY_rollback_padded"
        body = self.padded(self.valid_readyz(note=sentinel), probe.READYZ_BODY_CAP + 1)
        self.assertEqual(len(body), probe.READYZ_BODY_CAP + 1)
        self.assertEqual(json.loads(body)["note"], sentinel)
        code, results, _ = self.probe(fetch=self.fetch((200, body)))
        self.assertEqual(code, 1, results)
        self.assertIn("/readyz answered over the body cap", results["FAIL readyz"])
        self.assertNotIn(sentinel, results["FAIL readyz"])

    def test_cap_length_valid_prefix_with_hidden_tail_refuses(self):
        prefix = self.padded(self.valid_readyz(), probe.READYZ_BODY_CAP)
        self.assertEqual(json.loads(prefix)["build_revision"], "unknown")
        code, results, _ = self.probe(fetch=self.fetch((200, prefix + b"X")))
        self.assertEqual(code, 1, results)
        self.assertIn("/readyz answered over the body cap", results["FAIL readyz"])

    def test_httperror_oversize_enforces_the_same_bound(self):
        body = self.padded(self.valid_readyz(), probe.READYZ_BODY_CAP + 1)
        code, results, _ = self.probe(fetch=self.fetch((503, body)))
        self.assertEqual(code, 1, results)
        self.assertIn("/readyz answered over the body cap", results["FAIL readyz"])

    def test_transport_reads_bounded_cap_plus_one_both_paths(self):
        import urllib.error
        seen = []

        class Response(io.BytesIO):
            status = 200

            def read(self, n=-1):
                seen.append(n)
                return super().read(n)

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

        class OkOpener:
            def open(self, request, timeout=None):
                return Response(b"{}")

        with mock.patch.object(probe.urllib.request, "build_opener",
                               return_value=OkOpener()):
            probe.fetch(STAGING + "/readyz")
        self.assertEqual(seen, [probe.READYZ_BODY_CAP + 1])
        seen.clear()

        class ErrorOpener:
            def open(self, request, timeout=None):
                fp = Response(b"x")
                raise urllib.error.HTTPError(request.full_url, 503,
                                             "Service Unavailable", {}, fp)

        with mock.patch.object(probe.urllib.request, "build_opener",
                               return_value=ErrorOpener()):
            status, _ = probe.fetch(STAGING + "/readyz")
        self.assertEqual(status, 503)
        self.assertEqual(seen, [probe.READYZ_BODY_CAP + 1])


class DuplicateJsonKeyRefusalTests(unittest.TestCase):
    """Duplicate JSON object keys refuse at every probe decode site.

    Plain json.loads keeps the last duplicate, so a repeated manifest
    ``version`` or ``components`` could mask a stale backup or a down
    gateway. Every object at any depth is strict with fixed vocabulary
    that never echoes keys or values.
    """

    ARCHIVE = "two-funnel-20261002T041700Z.ndjson.gz"
    READY_PAIR = ('[["process", "ready"], ["gateway", "ready"], '
                  '["database", "ready"], ["token_invalid", "ready"]]')
    DOWN_PAIR = ('[["process", "ready"], ["gateway", "down"], '
                 '["database", "ready"], ["token_invalid", "ready"]]')

    def setUp(self):
        guard = mock.patch.object(probe.urllib.request.OpenerDirector, "open",
                                  side_effect=AssertionError("network access in a unit test"))
        guard.start()
        self.addCleanup(guard.stop)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.fx = Fixture(tmp.name)

    def archive_path(self):
        return self.fx.backups / self.ARCHIVE

    def fetch(self, response=READY):
        def fake(url):
            if isinstance(response, Exception):
                raise response
            return response
        return fake

    def test_manifest_top_level_duplicate_refused_both_orders(self):
        head = json.dumps(manifest()).encode()
        marker = "SENTINEL_manifest_dup_abc123"
        variants = [
            head[:-1] + (', "version": "%s"}' % marker).encode(),
            b'{"version": "%s", ' % marker.encode() + head[1:],
        ]
        for dup in variants:
            with self.subTest(dup=dup[:44]):
                self.fx.write_archive(self.ARCHIVE,
                                      [dup, *archive_lines(manifest())[1:]])
                with self.assertRaises(probe.ProbeError) as caught:
                    probe.read_manifest(self.archive_path())
                self.assertEqual(str(caught.exception),
                                 f"{self.ARCHIVE} manifest line carries duplicate JSON object keys")
                self.assertNotIn(marker, str(caught.exception))

    def test_manifest_nested_duplicate_refused(self):
        raw = json.dumps(manifest()).replace('"count": 2', '"count": 2, "count": 3', 1)
        self.assertIn('"count": 2, "count": 3', raw)
        self.fx.write_archive(self.ARCHIVE,
                              [raw.encode(), *archive_lines(manifest())[1:]])
        with self.assertRaises(probe.ProbeError) as caught:
            probe.read_manifest(self.archive_path())
        self.assertEqual(str(caught.exception),
                         f"{self.ARCHIVE} manifest line carries duplicate JSON object keys")

    def test_manifest_escaped_spelling_counts_as_duplicate(self):
        raw = json.dumps(manifest()).replace('"version": 4',
                                             '"version": 4, "\\u0076ersion": 5', 1)
        self.assertIn("\\u0076ersion", raw)
        self.fx.write_archive(self.ARCHIVE,
                              [raw.encode(), *archive_lines(manifest())[1:]])
        with self.assertRaises(probe.ProbeError) as caught:
            probe.read_manifest(self.archive_path())
        self.assertEqual(str(caught.exception),
                         f"{self.ARCHIVE} manifest line carries duplicate JSON object keys")

    def test_end_marker_duplicate_refused(self):
        lines = [*archive_lines(manifest())[:-1], b'{"kind": "end", "rows": 3, "rows": 4}']
        self.fx.write_archive(self.ARCHIVE, lines)
        with self.assertRaises(probe.ProbeError) as caught:
            probe.read_manifest(self.archive_path())
        self.assertEqual(str(caught.exception),
                         f"{self.ARCHIVE} end marker carries duplicate JSON object keys")

    def test_live_sequences_duplicate_refused(self):
        text = json.dumps(LIVE).replace('"readable": true',
                                        '"readable": true, "readable": false', 1)
        self.assertIn('"readable": true, "readable": false', text)
        self.fx.sequences.write_text(text)
        with self.assertRaises(probe.ProbeError) as caught:
            probe.load_live_sequences(str(self.fx.sequences))
        self.assertEqual(str(caught.exception),
                         f"{self.fx.sequences} carries duplicate JSON object keys")

    def test_live_sequences_escaped_spelling_counts_as_duplicate(self):
        text = json.dumps(LIVE).replace('"readable": true',
                                        '"readable": true, "\\u0072eadable": false', 1)
        self.assertIn("\\u0072eadable", text)
        self.fx.sequences.write_text(text)
        with self.assertRaises(probe.ProbeError) as caught:
            probe.load_live_sequences(str(self.fx.sequences))
        self.assertEqual(str(caught.exception),
                         f"{self.fx.sequences} carries duplicate JSON object keys")

    def test_readyz_top_level_components_duplicate_refused(self):
        for status, first, second in ((200, self.DOWN_PAIR, self.READY_PAIR),
                                      (200, self.READY_PAIR, self.DOWN_PAIR),
                                      (200, self.READY_PAIR, self.READY_PAIR),
                                      (503, self.DOWN_PAIR, self.READY_PAIR)):
            with self.subTest(status=status, first_down=first == self.DOWN_PAIR):
                raw = ('{"components": %s, "jobs": {}, '
                       '"build_revision": "unknown", "build_id": "unknown", '
                       '"components": %s}' % (first, second)).encode()
                with self.assertRaises(probe.ProbeError) as caught:
                    probe.readyz_state(status, raw)
                self.assertEqual(str(caught.exception),
                                 f"/readyz {status} carries duplicate JSON object keys")

    def test_readyz_nested_duplicate_refused(self):
        raw = ('{"components": %s, "jobs": {"counter": {"parked": true, "parked": false}}, '
               '"build_revision": "unknown", "build_id": "unknown"}' % self.READY_PAIR).encode()
        with self.assertRaises(probe.ProbeError) as caught:
            probe.readyz_state(200, raw)
        self.assertEqual(str(caught.exception),
                         "/readyz 200 carries duplicate JSON object keys")

    def test_readyz_escaped_spelling_counts_as_duplicate(self):
        raw = ('{"components": %s, "jobs": {"a": 1, "\\u0061": 2}, '
               '"build_revision": "unknown", "build_id": "unknown"}' % self.READY_PAIR).encode()
        with self.assertRaises(probe.ProbeError) as caught:
            probe.readyz_state(200, raw)
        self.assertEqual(str(caught.exception),
                         "/readyz 200 carries duplicate JSON object keys")

    def test_duplicate_manifest_fails_manifest_and_sequences_end_to_end(self):
        head = json.dumps(manifest()).encode()
        dup = head[:-1] + b', "version": 5}'
        self.fx.write_archive(self.ARCHIVE, [dup, *archive_lines(manifest())[1:]])
        out = io.StringIO()
        with mock.patch.dict(os.environ, {}, clear=True), contextlib.redirect_stdout(out):
            code = probe.main(self.fx.argv(), fetch_fn=self.fetch())
        text = out.getvalue()
        self.assertEqual(code, 1, text)
        self.assertIn("FAIL manifest: two-funnel-20261002T041700Z.ndjson.gz manifest line "
                      "carries duplicate JSON object keys", text)
        self.assertIn("FAIL sequences: no valid backup manifest to compare against", text)
        self.assertIn("PASS readyz", text)
        self.assertIn("PASS deploy-config", text)

    def test_duplicate_readyz_fails_only_readyz_end_to_end(self):
        raw = ('{"components": %s, "jobs": {}, "build_revision": "unknown", "build_id": "unknown", '
               '"components": %s}' % (self.DOWN_PAIR, self.READY_PAIR)).encode()
        out = io.StringIO()
        with mock.patch.dict(os.environ, {}, clear=True), contextlib.redirect_stdout(out):
            code = probe.main(self.fx.argv(), fetch_fn=self.fetch((200, raw)))
        text = out.getvalue()
        self.assertEqual(code, 1, text)
        self.assertIn("FAIL readyz: /readyz 200 carries duplicate JSON object keys", text)
        self.assertIn("PASS manifest", text)

    def test_unique_nested_documents_still_pass_end_to_end(self):
        head = manifest()
        head["note"] = {"a": 1, "b": [1, 2, {"c": 3}]}
        self.fx.write_archive(self.ARCHIVE, archive_lines(head))
        raw = ('{"components": %s, "jobs": {"counter": {"parked": false, "running": true}}, '
               '"build_revision": "unknown", "build_id": "unknown"}' % self.READY_PAIR).encode()
        out = io.StringIO()
        with mock.patch.dict(os.environ, {}, clear=True), contextlib.redirect_stdout(out):
            code = probe.main(self.fx.argv(), fetch_fn=self.fetch((200, raw)))
        text = out.getvalue()
        self.assertEqual(code, 0, text)
        self.assertIn("rollback readiness: 4/4 checks passed", text)


def load_tests(loader, tests, pattern):
    # Keep protected acceptance coverage on the existing CI and direct-script routes.
    import test_rollback_readiness_input_limits_acceptance as input_limits

    tests.addTests(loader.loadTestsFromModule(input_limits))
    return tests


if __name__ == "__main__":
    unittest.main()
