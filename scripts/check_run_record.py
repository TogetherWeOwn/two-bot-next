#!/usr/bin/env python3
"""Offline validator for staging E2E run records (stdlib only, no network).

Validates a run-record JSON file against
`docs/staging-e2e-run-record.schema.json`, which is the single source of
truth: this script interprets the schema subset the schema file uses
(`type`, `required`, `properties`, `additionalProperties`, `enum`, `const`,
`pattern`, `minLength`, `minimum`, `minItems`, `format: date-time`,
`allOf`/`if`/`then`) instead of duplicating its rules in code.

Two record-specific rules live here, not in the schema:

* records carrying `"mock": true` are worked examples, never evidence. They
  validate only with `--allow-mock`; without it they fail closed.
* verdict consistency: a record with any `commands[].result == "fail"` and
  `verdict.disposition == "PASS"` is rejected. A run with a failed command
  must be `NEEDS WORK`.
* execution window: `window.end_utc` must not precede `window.start_utc`;
  every `commands[].started_utc` must fall inside the window and its
  completion (`started_utc` + `duration_ms`) must not exceed the window end
  under the timestamp-precision policy below. `deployment.first_readyz_utc`
  is deliberately unchecked: a deployment may predate command execution.
  Malformed or incomparable timestamps fail safely with path-only
  diagnostics. This check never enforces the separate UTC-representation
  rule (which suffixes are allowed); it only orders the instants it can
  parse as timezone-aware datetimes.
* public-safety scan: no internal tracker IDs (`TOG-`, `PAP-`) and no
  high-confidence secret markers in any string value or object key. Key
  hits are reported at redacted paths, never echoing the key. This repo
  is public.
* input limits: a record or schema may hold at most `MAX_DOCUMENT_BYTES`
  UTF-8 bytes; larger files are refused with a fixed message before any
  decoding or parsing, so no read is unbounded. A record or schema may
  nest at most `MAX_NESTING_DEPTH`
  container levels (the root container is level 1). Deeper input, and input
  the decoder refuses (recursion overflow or an oversized integer literal),
  is refused with a fixed message. Bytes that are not UTF-8 are refused with
  their byte offset. No diagnostic echoes raw bytes or values.
* condition subset: `if` conditions support `properties` with `const`
  only, the subset the checked-in schema uses. Any other `if` operator
  is refused with a loud validation error instead of being silently
  treated as satisfied.

Timestamp policy for fields named `*_utc` (`window.start_utc`,
`window.end_utc`, `commands[].started_utc`, `deployment.first_readyz_utc`):
they must be timezone-aware UTC: a trailing `Z` or an explicit zero offset
(`+00:00`, `+0000`). Negative zero (`-00:00`, `-0000`) is accepted and
normalized to UTC, matching `datetime.fromisoformat` and RFC 3339's
`-00:00` (unknown local offset, same UTC instant). Timezone-naive values
(no suffix) and nonzero offsets (for example `+03:00`, `+00:30`) are
rejected, as are impossible calendar dates.

Timestamp-precision policy: instants compare as exact parsed datetimes, so
fractional seconds are preserved. The producer emits second-precision
`window` bounds (`%Y-%m-%dT%H:%M:%SZ`), truncating the true clock. A
completion that exceeds a second-precision (fractionless) window end by
less than one second is therefore attributed to truncation and allowed.
A completion at or past end + 1s, or any overrun past a fractional end,
is rejected. Command starts have no allowance: they must lie within
[start, end] exactly.

Usage:
  python3 scripts/check_run_record.py --record <file.json> [--allow-mock]
  [--schema <schema.json>]

Exit 0 when the record is valid, 1 with one error per line otherwise.
"""

import argparse
import datetime
import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_SCHEMA = ROOT / "docs" / "staging-e2e-run-record.schema.json"

# Conservative public-safety scan over every string value in the record.
SECRET_MARKERS = (
    re.compile(r"\bTOG-\d+\b"),
    re.compile(r"\bPAP-\d+\b"),
    re.compile(r"ghp_[A-Za-z0-9]+|gho_[A-Za-z0-9]+|github_pat_[A-Za-z0-9_]+"),
    re.compile(r"xox[bpras]-[A-Za-z0-9-]+"),
    re.compile(r"BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY"),
    re.compile(r"[Dd]iscord[^\"']{0,40}[Tt]oken[^\"']{0,10}[:=]"),
)

DATETIME_RE = re.compile(
    r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}"
    r"(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?$"
)

UTC_OFFSET_RE = re.compile(r"([+-])(\d{2}):?(\d{2})$")


def is_utc_field(path):
    """True when the JSON path names a `*_utc` timestamp field."""
    field = path.rsplit(".", 1)[-1]
    field = re.sub(r"\[.*\]$", "", field)
    return field.endswith("_utc")

FRACTION_RE = re.compile(r"\.\d+")

# A completion past a fractionless window end by less than this is
# second-truncation, not an impossible window (see module docstring).
TRUNCATION_ALLOWANCE = datetime.timedelta(seconds=1)

TYPE_NAMES = {
    "object": dict,
    "array": list,
    "string": str,
    "integer": int,
    "boolean": bool,
}


# Keeps every recursive walk far below the interpreter's recursion limit.
MAX_NESTING_DEPTH = 32
NESTING_MESSAGE = f"nesting depth exceeds the {MAX_NESTING_DEPTH}-level budget"
DECODER_DIGITS_MESSAGE = "integer literal exceeds the decoder digit limit"

# Bounds every document read before decoding: the loader issues one bounded
# read of at most MAX_DOCUMENT_BYTES + 1 (the extra byte witnesses EOF) and
# refuses larger input before UTF-8 decoding or JSON parsing.
MAX_DOCUMENT_BYTES = 1024 * 1024
BYTE_LIMIT_MESSAGE = "document exceeds byte limit"


class DuplicateKeyError(ValueError):
    """A JSON object has repeated keys; diagnostics never include its values."""


class DecoderLimitError(ValueError):
    """Input nests past the budget or exceeds a decoder limit; never echoes content."""


class InvalidUtf8Error(ValueError):
    """Input bytes are not UTF-8; reports the byte offset, never the bytes."""


class DocumentTooLargeError(ValueError):
    """Input exceeds the byte budget; fixed message, never echoes content."""


class _ObjectPairs(list):
    """Keep object pairs distinct from arrays until paths can be assigned."""


def safe_key(key):
    # Untrusted keys can themselves contain a secret; do not echo it in diagnostics.
    if any(marker.search(key) for marker in SECRET_MARKERS):
        return "[redacted-key]"
    return key


def child_path(path, key):
    return f"{path}.{safe_key(key)}"


def exceeds_nesting_budget(value):
    """True when any container nests deeper than MAX_NESTING_DEPTH levels.

    Iterative, so deep or self-referencing input cannot exhaust the stack.
    """
    pending = [(value, 1)]
    while pending:
        node, level = pending.pop()
        if isinstance(node, _ObjectPairs):
            children = [item for _key, item in node]
        elif isinstance(node, dict):
            children = list(node.values())
        elif isinstance(node, list):
            children = node
        else:
            continue
        if level > MAX_NESTING_DEPTH:
            return True
        pending.extend((child, level + 1) for child in children)
    return False


def load_json(text):
    """Decode JSON without losing duplicate keys, including in nested objects."""
    def unique_objects(value, path):
        if isinstance(value, _ObjectPairs):
            result = {}
            for key, item in value:
                field_path = child_path(path, key)
                if key in result:
                    raise DuplicateKeyError(f"{field_path}: duplicate object key")
                result[key] = unique_objects(item, field_path)
            return result
        if isinstance(value, list):
            return [unique_objects(item, f"{path}[{index}]")
                    for index, item in enumerate(value)]
        return value

    try:
        raw = json.loads(text, object_pairs_hook=_ObjectPairs)
    except json.JSONDecodeError:
        raise
    except RecursionError:
        raise DecoderLimitError(NESTING_MESSAGE) from None
    except ValueError:
        # Besides JSONDecodeError, json.loads raises ValueError only for the int digit cap.
        raise DecoderLimitError(DECODER_DIGITS_MESSAGE) from None
    if exceeds_nesting_budget(raw):
        raise DecoderLimitError(NESTING_MESSAGE)
    return unique_objects(raw, "$")


def load_document(path):
    # One bounded read: even a file that was small at stat time can grow,
    # so a post-read length check is too late. The size refusal wins over
    # decoding and parsing, and counts encoded bytes, not characters.
    with Path(path).open("rb") as handle:
        data = handle.read(MAX_DOCUMENT_BYTES + 1)
    if len(data) > MAX_DOCUMENT_BYTES:
        raise DocumentTooLargeError(BYTE_LIMIT_MESSAGE)
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise InvalidUtf8Error(f"invalid UTF-8 at byte offset {exc.start}") from None
    return load_json(text)


def check_format(value, fmt, path, errors):
    if fmt == "date-time":
        if not isinstance(value, str) or not DATETIME_RE.match(value):
            errors.append(f"{path}: not a UTC ISO-8601 date-time")
            return
        try:
            datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
        except ValueError:
            errors.append(f"{path}: invalid calendar date-time")
            return
        if is_utc_field(path):
            check_utc_suffix(value, path, errors)


def check_utc_suffix(value, path, errors):
    """Require a timezone-aware UTC suffix on `*_utc` fields.

    Accepts `Z` and explicit zero offsets; negative zero normalizes to
    UTC. Rejects naive timestamps and nonzero offsets with field-level
    diagnostics that never include the rejected value.
    """
    if value.endswith("Z"):
        return
    match = UTC_OFFSET_RE.search(value)
    if match is None:
        errors.append(
            f"{path}: naive timestamp without timezone; "
            "*_utc fields require Z or an explicit zero offset (+00:00)"
        )
        return
    if match.group(2) != "00" or match.group(3) != "00":
        errors.append(
            f"{path}: non-UTC offset; "
            "*_utc fields require Z or an explicit zero offset (+00:00)"
        )


def condition_holds(instance, condition, path, errors):
    """Evaluate the small `if` subschemas this schema file uses.

    Supports `properties` with `const` only. Any other `if` operator is
    an unsupported condition: record a loud validation error naming the
    operator (never its value) and treat the condition as not holding,
    so `then` is skipped but validation still fails.
    """
    if not isinstance(condition, dict):
        errors.append(
            f"{path}: unsupported condition in if "
            "(supported: an object of properties conditions)"
        )
        return False
    supported = True
    for key in condition:
        if key != "properties":
            errors.append(
                f"{path}: unsupported condition {safe_key(key)!r} in if "
                "(supported: properties)"
            )
            supported = False
    props = condition.get("properties", {})
    if not isinstance(props, dict):
        errors.append(
            f"{path}: unsupported condition in if.properties "
            "(supported: an object of const conditions)"
        )
        return False
    for key, subschema in props.items():
        if not isinstance(subschema, dict):
            errors.append(
                f"{path}: unsupported condition in "
                f"if.properties.{safe_key(key)} "
                "(supported: an object with const)"
            )
            supported = False
            continue
        unknown = [op for op in subschema if op != "const"]
        for operator in unknown:
            errors.append(
                f"{path}: unsupported condition operator "
                f"{safe_key(operator)!r} in "
                f"if.properties.{safe_key(key)} (supported: const)"
            )
        if unknown:
            supported = False
    if not supported:
        return False
    if not isinstance(instance, dict):
        return False
    for key, subschema in props.items():
        if not isinstance(subschema, dict):
            continue
        if "const" in subschema and instance.get(key) != subschema["const"]:
            return False
    return True


def check_instance(instance, schema, path, errors):
    if not isinstance(schema, dict):
        return
    expected = schema.get("type")
    if expected is not None:
        want = TYPE_NAMES.get(expected)
        if want is None:
            errors.append(f"{path}: unknown schema type {expected!r}")
            return
        if expected == "integer" and isinstance(instance, bool):
            errors.append(f"{path}: expected integer, got boolean")
            return
        if not isinstance(instance, want):
            errors.append(
                f"{path}: expected {expected}, "
                f"got {type(instance).__name__}"
            )
            return
    if "const" in schema and instance != schema["const"]:
        errors.append(f"{path}: must equal {schema['const']!r}")
    if "enum" in schema and instance not in schema["enum"]:
        errors.append(
            f"{path}: invalid enum value not one of {schema['enum']!r}; "
            "must be one of the allowed values"
        )
    if isinstance(instance, str):
        if "minLength" in schema and len(instance) < schema["minLength"]:
            errors.append(
                f"{path}: shorter than minLength {schema['minLength']}"
            )
        if "pattern" in schema and not re.search(schema["pattern"], instance):
            errors.append(
                f"{path}: string does not match required pattern "
                f"{schema['pattern']!r}"
            )
    if isinstance(instance, (int, float)) and not isinstance(instance, bool):
        if "minimum" in schema and instance < schema["minimum"]:
            errors.append(f"{path}: below minimum {schema['minimum']}")
    if isinstance(instance, dict):
        for key in schema.get("required", []):
            if key not in instance:
                errors.append(f"{path}: missing required field {safe_key(key)!r}")
        props = schema.get("properties", {})
        for key, value in instance.items():
            if key in props:
                check_instance(value, props[key], child_path(path, key), errors)
            elif schema.get("additionalProperties") is False:
                errors.append(f"{path}: unexpected field {safe_key(key)!r}")
    if isinstance(instance, list):
        if "minItems" in schema and len(instance) < schema["minItems"]:
            errors.append(f"{path}: fewer than minItems {schema['minItems']}")
        item_schema = schema.get("items")
        if isinstance(item_schema, dict):
            for index, value in enumerate(instance):
                check_instance(
                    value, item_schema, f"{path}[{index}]", errors
                )
    if "format" in schema:
        check_format(instance, schema["format"], path, errors)
    for subschema in schema.get("allOf", []):
        if "if" not in subschema:
            continue
        cond = subschema["if"]
        then = subschema.get("then")
        if condition_holds(instance, cond, path, errors):
            if isinstance(then, dict):
                check_instance(instance, then, path, errors)


def scan_public_safety(value, path, errors):
    if isinstance(value, str):
        for marker in SECRET_MARKERS:
            if marker.search(value):
                errors.append(
                    f"{path}: public-safety scan hit "
                    f"{marker.pattern!r}"
                )
                break
    elif isinstance(value, dict):
        for key, item in value.items():
            if isinstance(key, str):
                for marker in SECRET_MARKERS:
                    if marker.search(key):
                        errors.append(
                            f"{child_path(path, key)}: public-safety "
                            f"scan hit {marker.pattern!r}"
                        )
                        break
            scan_public_safety(item, child_path(path, key), errors)
    elif isinstance(value, list):
        for index, item in enumerate(value):
            scan_public_safety(item, f"{path}[{index}]", errors)


def validate(record, schema, allow_mock=False):
    """Return a list of error strings; empty means valid."""
    if exceeds_nesting_budget(record) or exceeds_nesting_budget(schema):
        return [f"$: {NESTING_MESSAGE}"]
    errors = []
    scan_public_safety(record, "$", errors)
    # Schema diagnostics can also echo values (enum, pattern, date-time).
    # Refuse unsafe records first, so those diagnostics never see a secret.
    if errors:
        return errors
    check_instance(record, schema, "$", errors)
    if isinstance(record, dict) and record.get("mock") is True and not allow_mock:
        errors.append(
            "$.mock: worked example labelled mock:true is not evidence; "
            "re-run with --allow-mock only to check the template shape"
        )
    check_verdict_consistency(record, errors)
    check_execution_window(record, errors)
    return errors


def parse_instant(value):
    """Parse an ISO-8601 timestamp to an aware UTC datetime, or None.

    Returns None for non-strings, unparseable values and timezone-naive
    values (no suffix). Naive values are incomparable: ordering them
    against aware bounds would raise, so they fail safely instead.
    Accepts any explicit offset; zone policy belongs to the separate
    UTC-representation rule, not to ordering.
    """
    if not isinstance(value, str):
        return None
    try:
        parsed = datetime.datetime.fromisoformat(value.replace("Z", "+00:00"))
    except (ValueError, OverflowError):
        return None
    if parsed.tzinfo is None:
        return None
    try:
        return parsed.astimezone(datetime.timezone.utc)
    except (ValueError, OverflowError):
        return None


def has_fractional_seconds(value):
    """True when the raw timestamp text carries fractional seconds."""
    return isinstance(value, str) and FRACTION_RE.search(value) is not None


def check_execution_window(record, errors):
    """Reject reversed windows and command timings outside the window.

    Requires window end >= start, every command start within [start, end],
    and every completion (start + duration_ms) within the window end under
    the module timestamp-precision policy. Reports paths only, never
    timestamp values. Ignores `deployment.first_readyz_utc`: a deployment
    may predate command execution.
    """
    if not isinstance(record, dict):
        return
    window = record.get("window")
    commands = record.get("commands")
    if not isinstance(window, dict) or not isinstance(commands, list):
        return
    start_raw = window.get("start_utc")
    end_raw = window.get("end_utc")
    if not isinstance(start_raw, str) or not isinstance(end_raw, str):
        return
    start = parse_instant(start_raw)
    end = parse_instant(end_raw)
    if start is None:
        errors.append(
            "$.window.start_utc: incomparable timestamp for window ordering; "
            "window bounds must be parseable timezone-aware date-times"
        )
    if end is None:
        errors.append(
            "$.window.end_utc: incomparable timestamp for window ordering; "
            "window bounds must be parseable timezone-aware date-times"
        )
    if start is None or end is None:
        return
    if end < start:
        errors.append(
            "$.window.end_utc: window end precedes start; "
            "window end must be >= start"
        )
        return
    end_fractional = has_fractional_seconds(end_raw)
    for index, command in enumerate(commands):
        if not isinstance(command, dict):
            continue
        started_raw = command.get("started_utc")
        if not isinstance(started_raw, str):
            continue
        started = parse_instant(started_raw)
        if started is None:
            errors.append(
                f"$.commands[{index}].started_utc: incomparable timestamp "
                "for window ordering; command starts must be parseable "
                "timezone-aware date-times"
            )
            continue
        if started < start or started > end:
            errors.append(
                f"$.commands[{index}].started_utc: outside execution window; "
                "command invocations must fall within [window start, window end]"
            )
            # Still check completion when the start parses, so a record
            # with both defects reports both; skip only when duration is
            # unusable (schema already reports bad durations).
        duration = command.get("duration_ms")
        if isinstance(duration, bool) or not isinstance(duration, int):
            continue
        if duration < 0:
            continue
        try:
            completion = started + datetime.timedelta(milliseconds=duration)
        except (OverflowError, ValueError):
            errors.append(
                f"$.commands[{index}]: incomparable completion for window "
                "ordering"
            )
            continue
        if completion <= end:
            continue
        if not end_fractional and (completion - end) < TRUNCATION_ALLOWANCE:
            continue
        errors.append(
            f"$.commands[{index}]: completion exceeds window end; "
            "recorded completion (started_utc + duration_ms) must not exceed "
            "the declared window end"
        )


def check_verdict_consistency(record, errors):
    """Reject PASS verdicts over runs containing a failed command.

    Reports paths and counts only, never command values, so a failing
    verdict can never echo a secret-shaped field.
    """
    if not isinstance(record, dict):
        return
    verdict = record.get("verdict")
    commands = record.get("commands")
    if not isinstance(verdict, dict) or not isinstance(commands, list):
        return
    if verdict.get("disposition") != "PASS":
        return
    failed = [
        index for index, command in enumerate(commands)
        if isinstance(command, dict) and command.get("result") == "fail"
    ]
    if failed:
        first = failed[0]
        errors.append(
            "$.verdict.disposition: PASS contradicts "
            f"{len(failed)} failed command(s) "
            f"(first at $.commands[{first}].result); "
            "a run with any result=fail must be NEEDS WORK"
        )


LOAD_ERRORS = (
    OSError,
    json.JSONDecodeError,
    DuplicateKeyError,
    DecoderLimitError,
    DocumentTooLargeError,
    InvalidUtf8Error,
)


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Validate a staging E2E run record against its schema."
    )
    parser.add_argument("--record", required=True, help="Record JSON file.")
    parser.add_argument(
        "--allow-mock",
        action="store_true",
        help="Accept a record labelled mock:true (template shape only).",
    )
    parser.add_argument(
        "--schema",
        default=str(DEFAULT_SCHEMA),
        help="Schema JSON file (default: docs schema).",
    )
    args = parser.parse_args(argv)
    try:
        record = load_document(args.record)
    except LOAD_ERRORS as exc:
        print(f"{args.record}: cannot load record: {exc}")
        return 1
    try:
        schema = load_document(args.schema)
    except LOAD_ERRORS as exc:
        print(f"{args.schema}: cannot load schema: {exc}")
        return 1
    errors = validate(record, schema, allow_mock=args.allow_mock)
    for error in errors:
        print(f"{args.record}: {error}")
    if errors:
        return 1
    print(f"{args.record}: valid staging E2E run record")
    return 0


if __name__ == "__main__":
    sys.exit(main())
