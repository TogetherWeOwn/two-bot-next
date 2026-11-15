"""Mocked CLI adapter regressions; no workflow or live metadata reads."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

from test_attestation_policy import metadata, policy
from test_attestation_orchestration import Reader


SCRIPTS = Path(__file__).resolve().parents[2] / ".github" / "scripts"
sys.path.insert(0, str(SCRIPTS))
spec = importlib.util.spec_from_file_location("inactive_gate", SCRIPTS / "base_attestation_gate.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class CliAdapter(unittest.TestCase):
    def invoke(self, raw, env=None, reader=None, module=policy, args=None):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "event.json"
            path.write_bytes(raw)
            output, errors = io.StringIO(), io.StringIO()
            context = {"GITHUB_EVENT_NAME": "pull_request_target", "GITHUB_REPOSITORY": "org/repo",
                       "GITHUB_SHA": "a" * 40, "GH_TOKEN": "SENTINEL-TOKEN"}
            if env:
                context.update(env)
            with mock.patch.dict(os.environ, context, clear=True), \
                    mock.patch.object(gate, "RestReader", return_value=reader) as factory, \
                    mock.patch.object(gate, "load_policy", return_value=module), \
                    contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                code = gate.main(args if args is not None else ["--event", str(path)])
            return code, output.getvalue(), errors.getvalue(), factory

    def test_delegates_valid_event_to_bracketed_evaluator(self):
        data = metadata()
        reader = Reader(data)
        code, output, errors, factory = self.invoke(json.dumps(data).encode(), reader=reader)
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(output)["head_sha"], "b" * 40)
        self.assertEqual(errors, "")
        self.assertEqual(len(reader.calls), 5)
        factory.assert_called_once_with("org/repo", "SENTINEL-TOKEN")

    def test_malformed_input_and_context_fail_closed(self):
        valid = json.dumps(metadata()).encode()
        cases = [(b'{"x":', None, None), (b'{"x":1,"x":2}', None, None),
                 (b'{"x":NaN}', None, None), (b'\xff', None, None),
                 (b' ' * (gate.MAX_EVENT_BYTES + 1), None, None),
                 (valid, {"GITHUB_EVENT_NAME": "pull_request"}, None),
                 (valid, {"GITHUB_SHA": "SENTINEL"}, None),
                 (valid, None, ["--SENTINEL"]) ]
        for raw, env, args in cases:
            with self.subTest(raw=raw[:20], env=env, args=args):
                code, output, errors, _ = self.invoke(raw, env, Reader(metadata()), args=args)
                self.assertEqual((code, output, errors), (1, "", gate.REFUSAL + "\n"))

    def test_reader_policy_and_missing_token_fail_without_diagnostics(self):
        data = metadata()
        raw = json.dumps(data).encode()
        reader = Reader(data)
        reader.mutate[1] = RuntimeError("SENTINEL")
        code, output, errors, _ = self.invoke(raw, reader=reader)
        self.assertEqual((code, output, errors), (1, "", gate.REFUSAL + "\n"))
        failing = mock.Mock()
        failing.evaluate.side_effect = RuntimeError("SENTINEL")
        code, output, errors, _ = self.invoke(raw, reader=Reader(data), module=failing)
        self.assertEqual((code, output, errors), (1, "", gate.REFUSAL + "\n"))
        with mock.patch.dict(os.environ, {"GITHUB_EVENT_NAME": "pull_request_target",
                                         "GITHUB_REPOSITORY": "org/repo"}, clear=True):
            with tempfile.NamedTemporaryFile() as event, contextlib.redirect_stderr(io.StringIO()) as errors:
                event.write(raw)
                event.flush()
                self.assertEqual(gate.main(["--event", event.name]), 1)
                self.assertEqual(errors.getvalue(), gate.REFUSAL + "\n")


if __name__ == "__main__":
    unittest.main()
