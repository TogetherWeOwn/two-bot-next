"""Shared read-only schema evidence validation and bounded CLI outcomes."""

import argparse
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

I64_MAX = 2**63 - 1
SOURCE_SHA = re.compile(r"[0-9a-f]{40}")
# Stable terminal vocabulary; never derive diagnostics or actions from input.
RESULTS = {
    "schema_clean": ("clean", "proceed_with_deploy", 0),
    "schema_drift": ("drift", "run_reviewed_migration_then_retry", 1),
    "recovery_waived": ("waived", "continue_guard_validated_recovery", 0),
    "arguments_invalid": ("refused", "correct_gate_arguments", 2),
    "source_sha_invalid": ("refused", "supply_canonical_source_sha", 2),
    "binding_refused": ("refused", "repair_plan_binding_and_pins", 2),
    "manifest_invalid": ("refused", "rebuild_reviewed_plan_runner_and_retry", 2),
    "plan_process_failed": ("error", "inspect_plan_runner_failure_and_retry", 3),
    "plan_execution_failed": ("error", "repair_plan_runner_execution_and_retry", 3),
    "plan_timeout": ("error", "investigate_plan_timeout_and_retry", 3),
    "manifest_write_failed": ("error", "repair_evidence_output_and_retry", 3),
    "gate_internal_error": ("error", "inspect_gate_implementation_before_retry", 3),
}


# Strings are skipped whole, so brackets inside them never count.
JSON_TOKEN = re.compile(r'"(?:[^"\\]|\\.)*"|[\[\]{}]')
# CPython's old default recursion limit, which bounded the C JSON decoder.
MAX_JSON_DEPTH = 1000


def _loads_bounded(raw, **kwargs):
    """json.loads with a lexical nesting bound that raises RecursionError.

    Newer CPython sizes the decoder's recursion guard from the real C stack
    and decodes 10,000 levels, so the bound no longer depends on the
    interpreter: existing RecursionError refusals keep working unchanged.
    """
    text = raw.decode("utf-8", "replace") if isinstance(raw, (bytes, bytearray)) else raw
    depth = 0
    for match in JSON_TOKEN.finditer(text):
        token = match.group()
        if token in "[{":
            depth += 1
            if depth > MAX_JSON_DEPTH:
                raise RecursionError("JSON nesting exceeds the decoder bound")
        elif token in "]}":
            depth -= 1
    return json.loads(raw, **kwargs)


class Refused(ValueError):
    """A bounded, non-secret refusal; never include rejected input in errors."""

    code = "manifest_invalid"


class GateFailure(Refused):
    """An outcome selected from RESULTS, not from exception text or wire data."""

    def __init__(self, code):
        super().__init__(code)
        self.code = code


class GateArgumentParser(argparse.ArgumentParser):
    def error(self, message):
        # argparse's default error includes rejected arguments and values.
        raise GateFailure("arguments_invalid")


def require(condition, message):
    if not condition:
        raise Refused(message)


def validate_source_sha(source_sha):
    if not isinstance(source_sha, str) or SOURCE_SHA.fullmatch(source_sha) is None:
        raise GateFailure("source_sha_invalid")


def unique_object(pairs):
    """Refuse duplicate fields at every depth, including escaped key spellings."""
    result = {}
    for key, value in pairs:
        require(key not in result, "plan manifest has duplicate fields")
        result[key] = value
    return result


def invalid_constant(value):
    # Python's JSON decoder otherwise accepts NaN and Infinity (not JSON).
    raise Refused("plan manifest is not valid JSON")


def finite_float(raw):
    """Refuse exponent-overflow wire numbers; json would decode them as inf."""
    try:
        value = float(raw)
    except ValueError:
        raise Refused("plan manifest is not valid JSON") from None
    require(math.isfinite(value), "plan manifest is not valid JSON")
    return value


def pending_versions(manifest, key="pending_before"):
    """Return the strictly-ascending pending list; refuse malformed manifests."""
    require(isinstance(manifest, dict), "plan manifest is not an object")
    pending = manifest.get(key)
    require(isinstance(pending, list), "pending versions are invalid")
    require(all(type(v) is int and 0 < v <= I64_MAX for v in pending),
            "pending version is outside the positive i64 range")
    require(pending == sorted(set(pending)), "pending versions are not strictly ascending")
    return pending


def validate_manifest(manifest, source_sha, migration_target, pins):
    """Bind the existing v1 report to this read-only request, not to itself.

    Pins must come from verify_binding's independently verified, trimmed vars.
    Host/branch matching follows that verifier; database matching stays exact.
    No hash/ledger/provenance redesign is implied by this consumer contract.
    """
    require(isinstance(manifest, dict), "plan manifest is not an object")
    require(type(manifest.get("runner_version")) is int
            and manifest["runner_version"] == 1, "plan runner version is unsupported")
    require(manifest.get("mode") == "plan", "plan mode is invalid")
    require(manifest.get("migration_target") == migration_target, "plan migration target is invalid")
    require(manifest.get("source_sha") == source_sha, "plan source SHA does not match")
    require(manifest.get("role") == "two_bot_migrator_ro", "plan role is not read-only")
    for key in ("applied_count", "applied_store_count"):
        require(type(manifest.get(key)) is int and manifest[key] == 0,
                "plan applied counts must be zero")
    target = manifest.get("target")
    require(isinstance(target, dict), "plan target is not an object")
    require(all(isinstance(target.get(key), str) for key in ("host", "database", "branch_id")),
            "plan target fields are invalid")
    host, database, branch = pins
    require(target["host"].lower() == host.lower()
            and target["database"] == database
            and target["branch_id"].lower() == branch.lower(),
            "plan target does not match the verified identity")
    return pending_versions(manifest), pending_versions(manifest, "pending_store_before")


def verified_pins(verify_binding, url, host, database, branch):
    try:
        return verify_binding(url, host, database, branch)
    except Refused:
        raise GateFailure("binding_refused") from None


def run_plan(argv, run):
    """Decode only the final report line; never expose child stdout/stderr."""
    try:
        proc = run(
            argv, capture_output=True, text=True, check=False, timeout=600,
        )
    except subprocess.TimeoutExpired:
        raise GateFailure("plan_timeout") from None
    except UnicodeError:
        raise Refused("plan manifest is not valid JSON") from None
    except (OSError, subprocess.SubprocessError):
        raise GateFailure("plan_execution_failed") from None
    if proc.returncode != 0:
        raise GateFailure("plan_process_failed")
    require(isinstance(proc.stdout, str), "plan manifest is not valid JSON")
    try:
        return _loads_bounded(proc.stdout.strip().splitlines()[-1],
                          object_pairs_hook=unique_object, parse_constant=invalid_constant,
                          parse_float=finite_float)
    except Refused:
        raise
    except (ValueError, IndexError, RecursionError):
        # Includes JSONDecodeError and Python's integer digit cap.
        raise Refused("plan manifest is not valid JSON") from None


def write_manifest(path, manifest):
    """Publish validated evidence atomically; a failed write publishes nothing.

    The only removable file is this call's owned temporary sibling. Never
    truncate the destination before serialization, flushing and close succeed.
    """
    temporary = None
    try:
        text = json.dumps(manifest, sort_keys=True, allow_nan=False) + "\n"
        destination = Path(path)
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", delete=False,
                                         dir=destination.parent, prefix=".schema-gate-") as handle:
            temporary = handle.name
            handle.write(text)
        os.replace(temporary, destination)
    except (OSError, TypeError, ValueError, RecursionError):
        raise GateFailure("manifest_write_failed") from None
    finally:
        if temporary is not None and os.path.exists(temporary):
            try:
                os.unlink(temporary)
            except OSError:
                pass  # No rejected values or filesystem diagnostics in CLI output.


def result(code):
    verdict, next_action, exit_code = RESULTS[code]
    print(json.dumps({"verdict": verdict, "code": code, "next_action": next_action}, sort_keys=True))
    return exit_code


def cli(main):
    """Keep every CLI failure inside a fixed vocabulary, including parser errors."""
    try:
        return main()
    except Refused as error:
        code = error.code
    except Exception:
        # Unexpected faults are not a clean verdict or permission to expose data.
        code = "gate_internal_error"
    print(f"schema gate failed: {code}", file=sys.stderr)
    return result(code)
