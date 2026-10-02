"""Rollback-readiness probe regressions: local fixtures only, no network or databases."""

import contextlib
import gzip
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import rollback_readiness_probe as probe  # noqa: E402

STAGING = "https://two-bot-next-staging.example-sub.workers.dev"
READY = (200, json.dumps({"components": [["process", "ready"], ["gateway", "ready"]]}).encode())
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

    def test_parked_staging_gateway_is_a_truthful_response(self):
        parked = (503, json.dumps({"components": [["process", "ready"], ["gateway", "starting"]]}).encode())
        code, results, _ = self.probe(fetch=self.fetch(parked))
        self.assertEqual(code, 0)
        self.assertIn("gateway starting (parked, not E2E approval)", results["PASS readyz"])

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
                "readyz", "without the bot's process/gateway breakdown"),
            "readyz broken deploy": (
                lambda: None, {"fetch": (500, b"error code: 1101")},
                "readyz", "/readyz 500 without"),
            "readyz no response": (
                lambda: None, {"fetch": TimeoutError("timed out")},
                "readyz", "did not respond (TimeoutError)"),
            "readyz gateway down at 200": (
                lambda: None,
                {"fetch": (200, b'{"components":[["process","ready"],["gateway","down"]]}')},
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
            "unknown version": (lines(version=5), None, "manifest version 5 is not one of (3, 4)"),
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


if __name__ == "__main__":
    unittest.main()
