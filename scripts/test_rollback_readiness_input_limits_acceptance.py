"""Protected offline acceptance contract for rollback-readiness input budgets.

Breaks caught: missing cumulative gzip budget, cap-after-parse/read-all file
handling, character-count instead of byte-count limits, exclusive boundaries,
and unvalidated createdAt text reaching verdict output. No production changes.

The Builder supplies DECOMPRESSED_CAP, SEQUENCES_FILE_CAP and CONFIG_FILE_CAP
module constants. Tests lower them to 8 KiB to exercise real gzip/files cheaply;
create=True lets the unimplemented baseline reach behavioral assertions rather
than fail importing a missing constant. Production defaults are not prescribed.
Fixed refusal messages below are the output contract, never parser/input text.
"""

import builtins
import contextlib
import gzip
import io
import json
import os
import tempfile
import unittest
from unittest import mock

from test_rollback_readiness_probe import CONFIG, Fixture, LIVE, READY, STAGING, archive_lines, manifest, probe


CAP = 8192
ARCHIVE_NAME = "two-funnel-20261002T041700Z.ndjson.gz"
STREAM_REASON = "archive exceeds decompressed byte cap"
SEQUENCES_REASON = "sequences file exceeds byte cap"
CONFIG_REASON = "wrangler config exceeds byte cap"
TIMESTAMP_REASON = "manifest has invalid createdAt timestamp"
CANARY = "INPUT_CONTENT_MUST_NOT_REACH_VERDICT"


class RollbackInputLimitsAcceptanceTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.fx = Fixture(tmp.name)
        self.archive = self.fx.backups / ARCHIVE_NAME
        for name in ("DECOMPRESSED_CAP", "SEQUENCES_FILE_CAP", "CONFIG_FILE_CAP"):
            patch = mock.patch.object(probe, name, CAP, create=True)
            patch.start()
            self.addCleanup(patch.stop)
        guard = mock.patch.object(
            probe.urllib.request.OpenerDirector, "open",
            side_effect=AssertionError("acceptance tests must never use the network"))
        guard.start()
        self.addCleanup(guard.stop)

    def run_probe(self):
        out, err = io.StringIO(), io.StringIO()
        requested = []

        def fetch(url):
            requested.append(url)
            return READY

        with mock.patch.dict(os.environ, {}, clear=True), contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = probe.main(self.fx.argv(), fetch_fn=fetch)
        self.assertEqual(err.getvalue(), "")
        self.assertEqual(requested, [STAGING + "/readyz"])
        return code, out.getvalue().splitlines()

    def assert_passes(self):
        code, lines = self.run_probe()
        self.assertEqual(code, 0, "\n".join(lines))
        self.assertEqual(len(lines), 5, "verdict must remain one line per check")
        for name, line in zip(("manifest", "sequences", "readyz", "deploy-config"), lines):
            self.assertTrue(line.startswith(f"PASS {name}: "), line)
        self.assertEqual(lines[-1], "rollback readiness: 4/4 checks passed")

    def assert_refuses(self, check, reason):
        code, lines = self.run_probe()
        self.assertEqual(code, 1, "oversized/invalid input was accepted")
        self.assertEqual(len(lines), 5, "input must not inject verdict lines")
        names = ("manifest", "sequences", "readyz", "deploy-config")
        for name, line in zip(names, lines):
            if name == check:
                self.assertEqual(line, f"FAIL {name}: {reason}")
            elif check == "manifest" and name == "sequences":
                self.assertEqual(line, "FAIL sequences: no valid backup manifest to compare against")
            else:
                self.assertTrue(line.startswith(f"PASS {name}: "), line)
        passed = 2 if check == "manifest" else 3
        self.assertEqual(lines[-1], f"rollback readiness: {passed}/4 checks passed")
        self.assertNotIn(CANARY, "\n".join(lines))
        self.assertNotIn("Traceback", "\n".join(lines))

    def write_archive(self, raw):
        self.archive.write_bytes(gzip.compress(raw, mtime=0))

    def sized_archive(self, size):
        lines = archive_lines(manifest())
        raw = b"\n".join(lines) + b"\n"
        self.assertLessEqual(len(raw), size)
        # Whitespace in a row line preserves both JSON and end/count semantics.
        lines[1] += b" " * (size - len(raw))
        return b"\n".join(lines) + b"\n"

    def sequences_bytes(self, size, *, unicode=False):
        rows = [dict(row) for row in LIVE]
        rows[0]["note"] = CANARY + ("é" * 100 if unicode else "")
        raw = json.dumps(rows, ensure_ascii=False).encode("utf-8")
        self.assertLessEqual(len(raw), size)
        return raw + b" " * (size - len(raw))

    def config_bytes(self, size, *, unicode=False):
        raw = CONFIG.encode() + ("\n# " + CANARY + ("é" * 100 if unicode else "") + "\n").encode()
        self.assertLessEqual(len(raw), size)
        return raw + b" " * (size - len(raw))

    def expanded_archive(self):
        head = manifest()
        head["tables"][0]["count"] = 1024
        rows = [json.dumps({"kind": "row", "table": "events", "data": {
            "id": "42", "guild_id": CANARY}}).encode()] * 1024
        raw = b"\n".join([json.dumps(head).encode(), *rows,
                            archive_lines(manifest())[3], b'{"kind":"end","rows":1025}']) + b"\n"
        self.assertGreater(len(raw), CAP * 8)
        self.assertLess(max(map(len, raw.splitlines())), CAP)
        self.write_archive(raw)
        self.assertLess(self.archive.stat().st_size, CAP // 4)

    def test_many_short_lines_trip_cumulative_decompressed_budget(self):
        self.expanded_archive()
        self.assert_refuses("manifest", STREAM_REASON)

    def test_stream_budget_stops_consuming_before_eof(self):
        self.expanded_archive()
        real_open = probe.gzip.open
        consumed = []
        self_test = self

        class MeteredGzip:
            def __init__(self, stream):
                self.stream = stream

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                self.stream.close()

            def take(self, method, size):
                self_test.assertGreater(size, 0, "gzip read must be bounded")
                self_test.assertLessEqual(size, CAP + 1025)
                data = method(size)
                consumed.append(len(data))
                self_test.assertLessEqual(sum(consumed), CAP + 1025,
                                          "stream continued consuming past its budget")
                return data

            def read(self, size=-1):
                return self.take(self.stream.read, size)

            def readline(self, size=-1):
                return self.take(self.stream.readline, size)

            def __iter__(self):
                return self

            def __next__(self):
                line = self.readline(1025)
                if not line:
                    raise StopIteration
                return line

        def opened(*args, **kwargs):
            return MeteredGzip(real_open(*args, **kwargs))

        with mock.patch.object(probe, "LINE_CAP", 1024), mock.patch.object(probe.gzip, "open", side_effect=opened):
            self.assert_refuses("manifest", STREAM_REASON)
        self.assertTrue(consumed, "stream reader bypassed budget observation")

    def test_decompressed_exact_cap_passes(self):
        raw = self.sized_archive(CAP)
        self.assertEqual(len(raw), CAP)
        self.write_archive(raw)
        self.assert_passes()

    def test_decompressed_cap_plus_one_refuses_including_end_and_newlines(self):
        raw = self.sized_archive(CAP + 1)
        self.assertEqual(len(raw), CAP + 1)
        self.write_archive(raw)
        self.assert_refuses("manifest", STREAM_REASON)

    def test_per_line_cap_remains_enforced_below_total_budget(self):
        # Both lines are individually valid JSON; only the row exceeds LINE_CAP.
        raw = self.sized_archive(4096)
        self.write_archive(raw)
        with mock.patch.object(probe, "LINE_CAP", 1024):
            with self.assertRaises(probe.ProbeError):
                probe.read_manifest(self.archive)

    def test_sequences_below_and_at_byte_cap_pass(self):
        for size in (CAP - 1, CAP):
            with self.subTest(size=size):
                raw = self.sequences_bytes(size)
                self.assertEqual(len(raw), size)
                self.fx.sequences.write_bytes(raw)
                self.assert_passes()

    def test_sequences_over_cap_refuse_before_json_decode(self):
        for raw in (self.sequences_bytes(CAP + 1), b"!" + b" " * CAP):
            with self.subTest(valid_json=raw.startswith(b"[")):
                self.fx.sequences.write_bytes(raw)
                decoded = []
                real_loads = probe.json.loads

                def loads(value, *args, **kwargs):
                    text = value.decode() if isinstance(value, bytes) else value
                    if text.startswith("[") or text.startswith("!"):
                        decoded.append(True)
                    return real_loads(value, *args, **kwargs)

                with mock.patch.object(probe.json, "loads", side_effect=loads):
                    self.assert_refuses("sequences", SEQUENCES_REASON)
                self.assertEqual(decoded, [], "over-cap sequences reached JSON decode")

    def test_sequences_limit_counts_utf8_bytes_not_characters(self):
        raw = self.sequences_bytes(CAP + 1, unicode=True)
        self.assertLess(len(raw.decode("utf-8")), CAP)
        self.fx.sequences.write_bytes(raw)
        self.assert_refuses("sequences", SEQUENCES_REASON)

    def test_config_below_and_at_byte_cap_pass(self):
        for size in (CAP - 1, CAP):
            with self.subTest(size=size):
                self.fx.config.write_bytes(self.config_bytes(size))
                self.assertEqual(self.fx.config.stat().st_size, size)
                self.assert_passes()

    def test_config_over_cap_refuses_before_toml_decode(self):
        for raw in (self.config_bytes(CAP + 1), b"[" + b" " * CAP):
            with self.subTest(valid_toml=raw.startswith(b"[vars]")):
                self.fx.config.write_bytes(raw)
                with mock.patch.object(probe.tomllib, "load", wraps=probe.tomllib.load) as load, mock.patch.object(
                        probe.tomllib, "loads", wraps=probe.tomllib.loads) as loads:
                    self.assert_refuses("deploy-config", CONFIG_REASON)
                self.assertEqual(load.call_count + loads.call_count, 0,
                                 "over-cap config reached TOML decode")

    def test_config_limit_counts_utf8_bytes_not_characters(self):
        raw = self.config_bytes(CAP + 1, unicode=True)
        self.assertLess(len(raw.decode("utf-8")), CAP)
        self.fx.config.write_bytes(raw)
        self.assert_refuses("deploy-config", CONFIG_REASON)

    def test_file_readers_never_request_unbounded_or_oversized_reads(self):
        for path, read in ((self.fx.sequences, lambda: probe.load_live_sequences(str(self.fx.sequences))),
                           (self.fx.config, lambda: probe.check_deploy_config(self.fx.config, probe.DEFAULT_ENVS, []))):
            with self.subTest(file=path.name):
                requested = []
                real_builtin_open, real_io_open = builtins.open, io.open

                class BoundedReader:
                    def __init__(self, stream):
                        self.stream = stream

                    def __enter__(self):
                        return self

                    def __exit__(self, *exc):
                        self.stream.close()

                    def read(self, size=-1):
                        requested.append(size)
                        self_test.assertGreater(size, 0, "input file read without a byte bound")
                        self_test.assertLessEqual(size, CAP + 1)
                        self_test.assertIn("b", self.stream.mode, "file must be bounded in bytes, not characters")
                        return self.stream.read(size)

                    def __getattr__(self, name):
                        return getattr(self.stream, name)

                self_test = self

                def open_with(real_open):
                    def opened(file, *args, **kwargs):
                        stream = real_open(file, *args, **kwargs)
                        return BoundedReader(stream) if file == path or file == str(path) else stream
                    return opened

                with mock.patch.object(builtins, "open", side_effect=open_with(real_builtin_open)), mock.patch.object(
                        io, "open", side_effect=open_with(real_io_open)):
                    read()
                self.assertTrue(requested, "file reader bypassed bounded read observation")

    def test_non_timestamp_created_at_refuses_without_echo(self):
        for value in ("", "not-a-timestamp", CANARY + "\nPASS manifest: forged",
                      "2026-99-99T25:61:61Z", "2026-10-02", "2026-10-02T04:17:00Z trailing"):
            with self.subTest(value=value):
                self.fx.write_archive(ARCHIVE_NAME, archive_lines(manifest(createdAt=value)))
                self.assert_refuses("manifest", TIMESTAMP_REASON)

    def test_overlong_created_at_refuses_below_line_and_stream_caps(self):
        for value in ("2026-10-02T04:17:00Z" + CANARY * 40,
                      "2026-10-02T04:17:00." + "1" * 2048 + "Z"):
            with self.subTest(length=len(value)):
                raw = b"\n".join(archive_lines(manifest(createdAt=value))) + b"\n"
                self.assertLess(len(raw), CAP)
                self.write_archive(raw)
                self.assert_refuses("manifest", TIMESTAMP_REASON)

    def test_non_string_created_at_refuses(self):
        for value in (None, 42, True, [], {}):
            with self.subTest(value=value):
                self.fx.write_archive(ARCHIVE_NAME, archive_lines(manifest(createdAt=value)))
                code, lines = self.run_probe()
                self.assertEqual(code, 1)
                self.assertTrue(lines[0].startswith("FAIL manifest: "))
                self.assertEqual(lines[-1], "rollback readiness: 2/4 checks passed")

    def test_supported_dump_versions_and_timestamp_controls_pass(self):
        for version in probe.DUMP_VERSIONS:
            for timestamp in ("2026-10-02T04:17:00Z", "2026-10-02T04:17:00.123456Z",
                              "2026-10-02T04:17:00+00:00"):
                with self.subTest(version=version, timestamp=timestamp):
                    self.fx.write_archive(ARCHIVE_NAME, archive_lines(manifest(version=version, createdAt=timestamp)))
                    self.assert_passes()


if __name__ == "__main__":
    unittest.main()
