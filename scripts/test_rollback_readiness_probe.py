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
            "200 with an unknown ready component": (200, readyz_body(process, gateway, database, token,
                                                                     ("voice", "ready"))),
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
            "200 with an unknown component down": (200, readyz_body(process, gateway, database, token,
                                                                    ("voice", "down")),
                                                   "unexpected components"),
            "500 with a complete body": (500, readyz_body(process, gateway, database, token),
                                         "/readyz 500 with unexpected"),
            "503 with every component ready": (503, readyz_body(process, gateway, database, token),
                                               "unexpected components"),
            "503 with process down": (503, readyz_body(("process", "down"), ("gateway", "down"), database, token),
                                      "unexpected components"),
            "503 with an unknown component down": (503, readyz_body(process, ("gateway", "starting"), database,
                                                                    token, ("voice", "down")),
                                                   "unexpected components"),
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
            "unknown version": (lines(version=6), None, "manifest version 6 is not one of (3, 4, 5)"),
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


if __name__ == "__main__":
    unittest.main()
