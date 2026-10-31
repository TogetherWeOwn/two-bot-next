"""Protected acceptance: bind staging-MCP proof to the request it answers.

QA-owned acceptance suite for the build `staging MCP prove-out: reject
malformed replies and uncorroborated fence success`. The Builder owns
`scripts/staging_mcp_proveout.py`; this file only reads it and never edits
builder-owned files (owner pairing rule). Corrections to this file belong to
the Test Author; the Builder must not edit it.

Pinned contract (offline, localhost only, no staging calls, no secrets in
fixtures; `do_post` is patched because the URL fence refuses plain-http
localhost targets; patched token/URL are dummy values never sent):

- Every authenticated RPC stage (initialize, tools/list, each tools/call,
  unknown-tool refusal) validates `jsonrpc == "2.0"`, an `id` that matches
  the request exactly (same JSON number, ids 1-8 in probe order), and
  exactly one well-formed `result`/`error` (both present, neither present,
  or a malformed `error` fails). Malformed/unbound envelopes never pass,
  even when nested fields look right.
- Tool results validate the emitted content contract: `content` must be a
  list of content blocks (empty list allowed for backward compatibility;
  each non-empty entry a dict with string `type`), `isError: true` always
  fails, and null/missing/non-list content fails. `get_audit` keeps its
  bounded `{available, rows, truncated}` structured-excerpt check. All
  existing response caps (`over the body cap` / `over the size cap`) and
  bearer redaction are preserved.
- Generic `-32000` is never proved fencing on its own. Without explicit
  expectation (`STAGING_MCP_EXPECT_FENCE=1`) every container-backed
  `-32000` (timeout, oversize, malformed upstream, fence) fails with
  `FENCE_UNCORROBORATED` and a non-proved disposition. With the explicit
  expectation, container-backed `-32000` passes as the expected fence.
  `get_config` is Worker-side and its `-32000` always fails, even with
  the expectation set. The expectation flag never touches the staging
  host fence, auth probes, or any production path.
- Failures carry stable stage IDs (the existing probe names), fixed
  reason codes in brackets, and a `next:` action:
  `ENVELOPE_VERSION`, `ENVELOPE_ID`, `ENVELOPE_FRAMING`,
  `CONTENT_INVALID`, `FENCE_UNCORROBORATED`. The JSON artifact carries a
  top-level `proof_disposition` (`proved` iff exit 0, otherwise one of
  `failed`/`unverified`/`unavailable`; fence-uncorroborated is never
  `proved`). Exit status matches the artifact: 0 iff `failures == []`
  and disposition `proved`, 1 on any probe failure, 2 on pre-flight
  refusal. Failures never echo untrusted remote bytes.
- Stage IDs (probe order, rpc ids 1-8 then auth): `initialize`,
  `tools/list`, `tools/call get_status`, `tools/call get_gateway`,
  `tools/call get_backfill_progress`, `tools/call get_config`,
  `tools/call get_audit`, `tools/call unknown`, `wrong bearer`,
  `no bearer`.

Failing-first proof: on the pre-fix tree every `*_fails` test below fails
because the script returns exit 0 with no failures for the malformed
battery (wrong version/id, result+error, null/malformed content,
isError:true, generic -32000), and every `proof_disposition` / reason-code
test fails because the fields do not exist; the `*_passes` controls (valid
script, corroborated fence, audit excerpt, caps, redaction, auth fences)
pass. No live MCP calls; no raw remote text in failures.

Offline command (from the repo root):
  PYTHONDONTWRITEBYTECODE=1 python3 -m unittest scripts.ci.test_staging_mcp_proveout_envelope_acceptance -v
"""

import io
import json
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import staging_mcp_proveout as proveout
from scripts.ci.test_staging_mcp_proveout import shipped_tools

BEARER = "test-bearer-envelope-acceptance-0000"
EXPECT_FENCE_ENV = "STAGING_MCP_EXPECT_FENCE"

EXPECTED_STAGES = (
    ["initialize", "tools/list"]
    + [f"tools/call {t}" for t in proveout.EXPECTED_TOOLS]
    + ["tools/call unknown", "wrong bearer", "no bearer"]
)

CONTAINER_TOOLS = sorted(proveout.CONTAINER_BACKED)

REASON_VERSION = "ENVELOPE_VERSION"
REASON_ID = "ENVELOPE_ID"
REASON_FRAMING = "ENVELOPE_FRAMING"
REASON_CONTENT = "CONTENT_INVALID"
REASON_FENCE = "FENCE_UNCORROBORATED"

NON_PROVED = ("failed", "unverified", "unavailable")


def valid_envelope(kind, rpc_id):
    """A main()-satisfying response for probe `kind` (offline fixture)."""
    headers = {"content-type": "application/json"}
    if kind == "initialize":
        return 200, headers, json.dumps({
            "jsonrpc": "2.0", "id": rpc_id,
            "result": {"protocolVersion": "2025-06-18"},
        })
    if kind == "tools/list":
        return 200, headers, json.dumps({
            "jsonrpc": "2.0", "id": rpc_id,
            "result": {"tools": shipped_tools()},
        })
    if kind.startswith("tools/call "):
        tool = kind.split(" ", 1)[1]
        if tool == "get_audit":
            structured = {"available": True, "rows": [], "truncated": False}
        else:
            structured = {"available": True, "rows": [], "truncated": False}
        # Non-audit tools ignore structuredContent; audit requires the
        # bounded excerpt. Content blocks are valid MCP text blocks.
        result = {
            "content": [{"type": "text", "text": "ok"}],
            "structuredContent": structured,
        }
        return 200, headers, json.dumps({
            "jsonrpc": "2.0", "id": rpc_id, "result": result,
        })
    if kind == "unknown":
        return 200, headers, json.dumps({
            "jsonrpc": "2.0", "id": rpc_id,
            "error": {"code": -32601, "message": "unknown tool"},
        })
    if kind in ("wrong bearer", "no bearer"):
        return 401, {"www-authenticate": "Bearer"}, "{}"
    raise AssertionError(f"unknown probe kind {kind}")


PROBE_SCRIPT = (
    ["initialize", "tools/list"]
    + [f"tools/call {t}" for t in proveout.EXPECTED_TOOLS]
    + ["unknown", "wrong bearer", "no bearer"]
)


def valid_script():
    return [valid_envelope(kind, rpc_id + 1)
            for rpc_id, kind in enumerate(PROBE_SCRIPT)]


def run_main(script, env_extra=None):
    """Run main() against a scripted do_post; never touches the network."""
    calls = []

    def fake_do_post(url, payload, token_or_none, timeout=25):
        calls.append((url, payload, token_or_none))
        item = script.pop(0)
        if isinstance(item, Exception):
            raise item
        return item

    env = {
        proveout.TOKEN_ENV: BEARER,
        proveout.URL_ENV: proveout.DEFAULT_URL,
    }
    env.pop(EXPECT_FENCE_ENV, None)
    if env_extra:
        env.update(env_extra)
    # Prove offline: forbid real transport construction.
    with tempfile.TemporaryDirectory() as tmp:
        output = str(Path(tmp) / "receipt.json")
        argv = ["staging_mcp_proveout.py", "--output", output]
        with mock.patch.object(proveout, "do_post", fake_do_post), \
                mock.patch.object(sys, "argv", argv), \
                mock.patch.dict(os.environ, env, clear=True), \
                mock.patch("urllib.request.build_opener",
                           side_effect=AssertionError("real network forbidden")):
            buf = io.StringIO()
            with redirect_stdout(buf):
                code = proveout.main()
        stdout = buf.getvalue()
        raw = Path(output).read_bytes()
        artifact = json.loads(raw.decode("utf-8"))
    assert script == [], "main() must run all ten probes"
    return code, stdout, artifact, raw, calls


class ProofContractHelpers(unittest.TestCase):
    def assert_false_proof_fails(self, index, body_dict_or_text, reason,
                                 sentinel="SENTINEL-envelope-acceptance"):
        """A malformed battery entry must fail closed with code+next action."""
        if isinstance(body_dict_or_text, dict):
            body = json.dumps(body_dict_or_text)
        else:
            body = body_dict_or_text
        script = valid_script()
        script[index] = (200, {"content-type": "application/json"}, body)
        code, _stdout, artifact, _raw, _calls = run_main(script)
        stage = EXPECTED_STAGES[index]
        self.assertEqual(
            code, 1,
            f"{stage}: malformed battery must exit 1, got {code} "
            f"with failures {artifact.get('failures')!r}",
        )
        failures = artifact.get("failures", [])
        self.assertEqual(
            len(failures), 1,
            f"{stage}: exactly one failure expected, got {failures!r}",
        )
        failure = failures[0]
        self.assertTrue(
            failure.startswith(stage + ":"),
            f"{stage}: failure must name its stage, got {failure!r}",
        )
        self.assertIn(
            f"[{reason}]", failure,
            f"{stage}: failure must carry fixed reason [{reason}], "
            f"got {failure!r}",
        )
        self.assertIn(
            "next:", failure,
            f"{stage}: failure must carry a next action, got {failure!r}",
        )
        self.assertNotIn(
            sentinel, failure,
            f"{stage}: failures must never echo remote bytes",
        )
        self.assertNotIn(
            BEARER, failure,
            f"{stage}: failures must never carry the bearer",
        )
        disposition = artifact.get("proof_disposition")
        self.assertIn(
            disposition, NON_PROVED,
            f"{stage}: disposition must be non-proved, got {disposition!r}",
        )
        return artifact


class EnvelopeVersionTests(ProofContractHelpers):
    def test_wrong_version_fails_every_authenticated_stage(self):
        for index in range(8):
            with self.subTest(stage=EXPECTED_STAGES[index]):
                rpc_id = index + 1
                # Nested fields are valid; only the version is wrong.
                _, _, body = valid_script()[index]
                envelope = json.loads(body)
                envelope["jsonrpc"] = "1.0"
                envelope["note"] = "SENTINEL-envelope-acceptance"
                self.assert_false_proof_fails(index, envelope, REASON_VERSION)

    def test_missing_version_fails(self):
        for index in (0, 1, 2, 7):
            with self.subTest(stage=EXPECTED_STAGES[index]):
                _, _, body = valid_script()[index]
                envelope = json.loads(body)
                envelope.pop("jsonrpc", None)
                envelope["note"] = "SENTINEL-envelope-acceptance"
                self.assert_false_proof_fails(index, envelope, REASON_VERSION)

    def test_null_version_fails(self):
        _, _, body = valid_script()[0]
        envelope = json.loads(body)
        envelope["jsonrpc"] = None
        self.assert_false_proof_fails(0, envelope, REASON_VERSION)


class EnvelopeIdTests(ProofContractHelpers):
    def test_wrong_id_fails_every_authenticated_stage(self):
        for index in range(8):
            with self.subTest(stage=EXPECTED_STAGES[index]):
                _, _, body = valid_script()[index]
                envelope = json.loads(body)
                envelope["id"] = 999
                envelope["note"] = "SENTINEL-envelope-acceptance"
                self.assert_false_proof_fails(index, envelope, REASON_ID)

    def test_missing_id_fails(self):
        for index in (0, 2, 7):
            with self.subTest(stage=EXPECTED_STAGES[index]):
                _, _, body = valid_script()[index]
                envelope = json.loads(body)
                envelope.pop("id", None)
                self.assert_false_proof_fails(index, envelope, REASON_ID)

    def test_null_id_fails(self):
        _, _, body = valid_script()[1]
        envelope = json.loads(body)
        envelope["id"] = None
        self.assert_false_proof_fails(1, envelope, REASON_ID)

    def test_string_id_does_not_match_numeric_request(self):
        _, _, body = valid_script()[0]
        envelope = json.loads(body)
        envelope["id"] = "1"
        self.assert_false_proof_fails(0, envelope, REASON_ID)


class EnvelopeFramingTests(ProofContractHelpers):
    def test_result_and_error_fails_every_authenticated_stage(self):
        for index in range(8):
            with self.subTest(stage=EXPECTED_STAGES[index]):
                rpc_id = index + 1
                if index == 7:
                    # Unknown-tool refusal with an extra result still fails.
                    envelope = {
                        "jsonrpc": "2.0", "id": rpc_id,
                        "result": {"content": []},
                        "error": {"code": -32601, "message": "x"},
                    }
                elif index == 0:
                    envelope = {
                        "jsonrpc": "2.0", "id": rpc_id,
                        "result": {"protocolVersion": "2025-06-18"},
                        "error": {"code": -32603, "message": "x"},
                    }
                elif index == 1:
                    envelope = {
                        "jsonrpc": "2.0", "id": rpc_id,
                        "result": {"tools": shipped_tools()},
                        "error": {"code": -32603, "message": "x"},
                    }
                else:
                    envelope = {
                        "jsonrpc": "2.0", "id": rpc_id,
                        "result": {"content": [{"type": "text", "text": "ok"}]},
                        "error": {"code": -32000, "message": "x"},
                    }
                envelope["note"] = "SENTINEL-envelope-acceptance"
                self.assert_false_proof_fails(index, envelope, REASON_FRAMING)

    def test_missing_result_and_error_fails(self):
        for index in (0, 2, 7):
            with self.subTest(stage=EXPECTED_STAGES[index]):
                rpc_id = index + 1
                envelope = {"jsonrpc": "2.0", "id": rpc_id}
                self.assert_false_proof_fails(index, envelope, REASON_FRAMING)

    def test_malformed_error_fails_unknown_refusal(self):
        rpc_id = 8
        for error in (None, "x", ["x"], True, 1, {}, {"message": "x"},
                      {"code": "-32601"}, {"code": True}, {"code": None}):
            with self.subTest(error=error):
                if error is None:
                    # error:null with no result is neither result nor error.
                    envelope = {"jsonrpc": "2.0", "id": rpc_id, "error": None}
                else:
                    envelope = {"jsonrpc": "2.0", "id": rpc_id, "error": error}
                self.assert_false_proof_fails(7, envelope, REASON_FRAMING)

    def test_wrong_error_code_fails_unknown_refusal(self):
        envelope = {"jsonrpc": "2.0", "id": 8,
                    "error": {"code": -32000, "message": "x"}}
        script = valid_script()
        script[7] = (200, {"content-type": "application/json"},
                     json.dumps(envelope))
        code, _s, artifact, _r, _c = run_main(script)
        self.assertEqual(code, 1)
        self.assertEqual(len(artifact["failures"]), 1)
        self.assertTrue(artifact["failures"][0].startswith("tools/call unknown:"))

    def test_error_null_with_valid_result_is_success(self):
        # Control: explicit JSON null error alongside a valid result is the
        # standard success framing and must keep passing.
        _, _, body = valid_script()[0]
        envelope = json.loads(body)
        envelope["error"] = None
        script = valid_script()
        script[0] = (200, {"content-type": "application/json"},
                     json.dumps(envelope))
        code, _s, artifact, _r, _c = run_main(script)
        self.assertEqual(artifact["failures"], [])
        self.assertEqual(code, 0)


class ToolContentTests(ProofContractHelpers):
    TOOL_INDICES = list(range(2, 2 + len(proveout.EXPECTED_TOOLS)))

    def _content_envelope(self, rpc_id, content, extra=None, tool="get_status"):
        result = {"content": content}
        if tool == "get_audit":
            result["structuredContent"] = {
                "available": True, "rows": [], "truncated": False}
        if extra:
            result.update(extra)
        return {"jsonrpc": "2.0", "id": rpc_id, "result": result}

    def test_null_content_fails_every_tool(self):
        for index in self.TOOL_INDICES:
            with self.subTest(stage=EXPECTED_STAGES[index]):
                tool = PROBE_SCRIPT[index].split(" ", 1)[1]
                envelope = self._content_envelope(index + 1, None, tool=tool)
                self.assert_false_proof_fails(index, envelope, REASON_CONTENT)

    def test_malformed_content_fails(self):
        for index in (2, 6):
            for content in ("x", {"type": "text"}, True, 1, ["x"], [None],
                            [{"text": "missing-type"}], [{"type": 1, "text": "x"}]):
                with self.subTest(stage=EXPECTED_STAGES[index], content=content):
                    tool = PROBE_SCRIPT[index].split(" ", 1)[1]
                    envelope = self._content_envelope(index + 1, content, tool=tool)
                    self.assert_false_proof_fails(index, envelope, REASON_CONTENT)

    def test_missing_content_fails(self):
        rpc_id = 3
        envelope = {"jsonrpc": "2.0", "id": rpc_id, "result": {}}
        self.assert_false_proof_fails(2, envelope, REASON_CONTENT)

    def test_is_error_true_fails_even_with_valid_content(self):
        for index in self.TOOL_INDICES:
            with self.subTest(stage=EXPECTED_STAGES[index]):
                tool = PROBE_SCRIPT[index].split(" ", 1)[1]
                envelope = self._content_envelope(
                    index + 1, [{"type": "text", "text": "ok"}],
                    extra={"isError": True}, tool=tool)
                self.assert_false_proof_fails(index, envelope, REASON_CONTENT)

    def test_valid_content_passes(self):
        for content in ([], [{"type": "text", "text": "ok"}],
                        [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]):
            with self.subTest(content=content):
                script = valid_script()
                for index in self.TOOL_INDICES:
                    tool = PROBE_SCRIPT[index].split(" ", 1)[1]
                    envelope = self._content_envelope(index + 1, content, tool=tool)
                    script[index] = (200, {"content-type": "application/json"},
                                     json.dumps(envelope))
                code, _s, artifact, _r, _c = run_main(script)
                self.assertEqual(artifact["failures"], [])
                self.assertEqual(code, 0)

    def test_audit_excerpt_still_enforced(self):
        # Control: the bounded excerpt check survives the content contract.
        rpc_id = 7
        bad = {"jsonrpc": "2.0", "id": rpc_id, "result": {
            "content": [{"type": "text", "text": "ok"}],
            "structuredContent": {"implemented": False}}}
        script = valid_script()
        script[6] = (200, {"content-type": "application/json"}, json.dumps(bad))
        code, _s, artifact, _r, _c = run_main(script)
        self.assertEqual(code, 1)
        self.assertTrue(artifact["failures"][0].startswith("tools/call get_audit:"))


class FenceCorroborationTests(ProofContractHelpers):
    def _fence_script(self, message="container request failed"):
        script = valid_script()
        for tool in CONTAINER_TOOLS:
            index = EXPECTED_STAGES.index(f"tools/call {tool}")
            rpc_id = index + 1
            envelope = {"jsonrpc": "2.0", "id": rpc_id,
                        "error": {"code": -32000, "message": message}}
            script[index] = (200, {"content-type": "application/json"},
                             json.dumps(envelope))
        return script

    def test_generic_32000_without_expectation_fails(self):
        for tool in CONTAINER_TOOLS:
            with self.subTest(tool=tool):
                index = EXPECTED_STAGES.index(f"tools/call {tool}")
                envelope = {"jsonrpc": "2.0", "id": index + 1,
                            "error": {"code": -32000,
                                      "message": "SENTINEL-envelope-acceptance"}}
                self.assert_false_proof_fails(index, envelope, REASON_FENCE)

    def test_timeout_oversize_malformed_labels_still_fail_without_expectation(self):
        for message in ("timeout waiting for container",
                        "container body oversize",
                        "malformed upstream JSON",
                        "container request failed"):
            with self.subTest(message=message):
                script = self._fence_script(message=message)
                code, _s, artifact, _r, _c = run_main(script)
                self.assertEqual(
                    code, 1,
                    f"message {message!r} must not prove fencing without "
                    f"expectation, got {artifact.get('failures')!r}",
                )
                self.assertTrue(
                    any(f"[{REASON_FENCE}]" in f for f in artifact["failures"]),
                    f"failures must carry [{REASON_FENCE}], "
                    f"got {artifact['failures']!r}",
                )
                self.assertIn(artifact.get("proof_disposition"), NON_PROVED)

    def test_corroborated_fence_with_expectation_passes(self):
        script = self._fence_script()
        code, _s, artifact, _r, _c = run_main(
            script, env_extra={EXPECT_FENCE_ENV: "1"})
        self.assertEqual(artifact["failures"], [])
        self.assertEqual(code, 0)
        self.assertEqual(artifact.get("proof_disposition"), "proved")

    def test_unset_or_empty_expectation_does_not_corroborate(self):
        for value in (None, "", "0", "false"):
            with self.subTest(value=value):
                script = self._fence_script()
                extra = {} if value is None else {EXPECT_FENCE_ENV: value}
                code, _s, artifact, _r, _c = run_main(script, env_extra=extra)
                self.assertEqual(code, 1)
                self.assertIn(artifact.get("proof_disposition"), NON_PROVED)

    def test_get_config_32000_always_fails_even_with_expectation(self):
        index = EXPECTED_STAGES.index("tools/call get_config")
        envelope = {"jsonrpc": "2.0", "id": index + 1,
                    "error": {"code": -32000, "message": "x"}}
        script = valid_script()
        script[index] = (200, {"content-type": "application/json"},
                         json.dumps(envelope))
        for extra in ({}, {EXPECT_FENCE_ENV: "1"}):
            with self.subTest(extra=extra):
                code, _s, artifact, _r, _c = run_main(
                    list(script), env_extra=extra)
                self.assertEqual(code, 1)
                self.assertTrue(
                    artifact["failures"][0].startswith("tools/call get_config:"))

    def test_normal_success_without_expectation_passes(self):
        code, _s, artifact, _r, _c = run_main(valid_script())
        self.assertEqual(artifact["failures"], [])
        self.assertEqual(code, 0)
        self.assertEqual(artifact.get("proof_disposition"), "proved")


class ProofDispositionTests(unittest.TestCase):
    def test_stage_ids_are_stable(self):
        _code, _s, artifact, _r, _c = run_main(valid_script())
        self.assertEqual(
            [p["name"] for p in artifact["probes"]], EXPECTED_STAGES)

    def test_disposition_matches_exit_status(self):
        code, _s, artifact, _r, _c = run_main(valid_script())
        self.assertEqual(code, 0)
        self.assertEqual(artifact.get("proof_disposition"), "proved")
        self.assertEqual(artifact.get("failures"), [])
        # One malformed stage flips both the exit and the disposition.
        script = valid_script()
        script[0] = (200, {"content-type": "application/json"},
                     json.dumps({"jsonrpc": "1.0", "id": 1,
                                 "result": {"protocolVersion": "2025-06-18"}}))
        code, _s, artifact, _r, _c = run_main(script)
        self.assertEqual(code, 1)
        self.assertIn(artifact.get("proof_disposition"), NON_PROVED)
        self.assertEqual(len(artifact["failures"]), 1)

    def test_staging_host_fence_survives_expectation_flag(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = str(Path(tmp) / "proveout.json")
            argv = ["staging_mcp_proveout.py", "--output", out]
            env = {proveout.TOKEN_ENV: BEARER,
                   proveout.URL_ENV: "https://two-bot-next-staging.evil.example/mcp",
                   EXPECT_FENCE_ENV: "1"}
            with mock.patch.object(sys, "argv", argv), \
                    mock.patch.dict(os.environ, env, clear=True), \
                    mock.patch("urllib.request.build_opener") as opener, \
                    mock.patch.object(proveout, "do_post") as do_post:
                self.assertEqual(proveout.main(), 2)
            opener.assert_not_called()
            do_post.assert_not_called()
            self.assertFalse(Path(out).exists())

    def test_response_cap_and_redaction_survive(self):
        # Control: over-cap still maps to the fixed cap verdict, and the
        # bearer never appears in stdout, artifact or failures.
        script = valid_script()
        script[0] = proveout.BodyTooLarge("response body over the size cap")
        code, stdout, artifact, raw, _calls = run_main(script)
        self.assertEqual(code, 1)
        self.assertTrue(any("over the body cap" in f for f in artifact["failures"]))
        self.assertNotIn(BEARER, stdout)
        self.assertNotIn(BEARER.encode(), raw)
        self.assertNotIn(BEARER, json.dumps(artifact))


if __name__ == "__main__":
    unittest.main()
