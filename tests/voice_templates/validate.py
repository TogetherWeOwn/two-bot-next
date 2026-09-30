#!/usr/bin/env python3
"""Validate fixture integrity, not renderer conformance. Standard library only."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import sys

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
DAYS = "Monday Tuesday Wednesday Thursday Friday Saturday Sunday".split()
MONTHS = ("January February March April May June July August September October "
          "November December").split()


class FixtureError(ValueError):
    pass


def require(ok, message):
    if not ok:
        raise FixtureError(message)


def fields(value, required, optional=()):
    require(type(value) is dict, "expected object")
    require(set(required) <= value.keys(), f"missing fields: {set(required) - value.keys()}")
    require(value.keys() <= set(required) | set(optional), "unknown fields")


def text(value, empty=False):
    require(type(value) is str and (empty or bool(value)), "expected string")


def integer(value, low, high):
    require(type(value) is int and low <= value <= high, "integer outside range")


def boolean(value):
    require(type(value) is bool, "expected boolean")


def array(value):
    require(type(value) is list, "expected array")


def string_array(value, empty=False):
    array(value)
    for item in value:
        text(item)
    require(empty or bool(value), "empty array")
    require(len(value) == len(set(value)), "duplicate array entries")


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON key: {key}")
        result[key] = value
    return result


def reject_constant(value):
    raise FixtureError(f"non-JSON constant: {value}")


def load(path):
    return json.loads(Path(path).read_text(encoding="utf-8"),
                      object_pairs_hook=unique_object, parse_constant=reject_constant)


def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True,
                      separators=(",", ":"), allow_nan=False)


def validate_context(context):
    fields(context, ["channel_kind", "number", "limit", "private", "seed", "owner_id",
                     "original_creator_name", "members", "clock", "settings"])
    require(context["channel_kind"] in ["temporary", "standalone", "stage"], "channel kind")
    integer(context["number"], 1, 999999)
    integer(context["limit"], 0, 99)
    boolean(context["private"])
    for key in ["seed", "owner_id", "original_creator_name"]:
        text(context[key])
    array(context["members"])
    ids = set()
    parties = {}
    for member in context["members"]:
        fields(member, ["id", "display_name", "nick", "roles", "game", "live_discord",
                        "live_external", "stream_title", "party"])
        for key in ["id", "display_name"]:
            text(member[key])
        require(member["id"] not in ids, "duplicate member ID")
        ids.add(member["id"])
        for key in ["nick", "game", "stream_title"]:
            if member[key] is not None:
                text(member[key])
        string_array(member["roles"], empty=True)
        boolean(member["live_discord"])
        boolean(member["live_external"])
        party = member["party"]
        if party is not None:
            fields(party, ["id", "size", "maximum", "state", "details"])
            text(party["id"])
            integer(party["size"], 0, 999999)
            if party["maximum"] is not None:
                integer(party["maximum"], party["size"], 999999)
            text(party["state"], empty=True)
            text(party["details"], empty=True)
            require(party["id"] not in parties or parties[party["id"]] == party,
                    "inconsistent snapshots of same party")
            parties[party["id"]] = party
    # Empty standalone channels can remember their last owner. Temporary rooms
    # can transiently have no members; the lifecycle is outside this corpus.
    require(not ids or context["owner_id"] in ids, "owner absent from populated snapshot")
    clock = context["clock"]
    fields(clock, ["weekday", "month", "hour", "timezone"])
    require(clock["weekday"] in DAYS and clock["month"] in MONTHS, "English civil clock")
    integer(clock["hour"], 0, 23)
    text(clock["timezone"])
    settings = context["settings"]
    fields(settings, ["no_game", "aliases", "named_lists", "force_single_game", "include_inactive"])
    text(settings["no_game"])
    boolean(settings["force_single_game"])
    boolean(settings["include_inactive"])
    for key in ["aliases", "named_lists"]:
        require(type(settings[key]) is dict, "settings map")
        for name, value in settings[key].items():
            text(name)
            if key == "aliases":
                text(value)
            else:
                string_array(value, empty=True)


def trimmed_output(value):
    # Truncation follows trim, so a 100-character prefix may end in whitespace.
    return value.lstrip() == value and (len(value) == 100 or value.rstrip() == value)


def spec_inventory(spec):
    """Derive token/keyword/style inventory independently of coverage.json."""
    v5 = spec.split("## V5:", 1)[1].split("## V7:", 1)[0]
    tokens = set(re.findall(r"@@[a-z_]+@@", v5))
    tokens.update(["##", "$#", "$0#", "$00#", "+#"])
    conditions = spec.split("| Activity and streaming |", 1)[1].split(
        "- `FULL` requires", 1)[0]
    keywords = set(re.findall(r"\b[A-Z][A-Z_]+(?::id)?\b", conditions))
    style_block = spec.split("- Case:", 1)[1].split("- **Accept when:** every mode", 1)[0]
    # Parenthetical explanations can span lines; strip them before splitting
    # the mode inventory, including upper/caps aliases and the <N>w family.
    style_block = re.sub(r"\([^)]*\)", "", style_block, flags=re.S)
    for prefix in ["- Words and spacing:", "- Novelty:", "- Unicode fonts:"]:
        style_block = style_block.replace(prefix, ",")
    styles = {item.strip().strip(".` ") for item in re.split(r"[,/]", style_block)}
    return ({"token:" + token for token in tokens}
            | {"condition:" + keyword for keyword in keywords}
            | {"style:" + style for style in styles}
            | {"compare:" + op for op in ["<", ">", "<=", ">=", "=", "!="]}
            | {"plural:members", "plural:others", "plural:party", "random:choice",
               "random:list", "state:resting"}
            | {"rule:" + rule for rule in ["order", "trim", "truncate", "fallback",
               "unknown-condition", "unknown-style", "nested", "optional-else",
               "style-chain", "name-in-condition", "non-numeric", "default-template"]})


def feature_used(key, template):
    if key.startswith("token:"):
        return key[6:] in template
    if key.startswith("condition:"):
        word = key[10:]
        pattern = re.escape(word.replace(":id", ":"))
        if word.endswith(":id"):
            pattern += r"[a-zA-Z0-9_-]+"
        return bool(re.search(r"\{\{\s*" + pattern + r"(?=\W|$)", template))
    if key.startswith("compare:"):
        return " " + key[8:] + " " in template
    if key.startswith("style:"):
        mode = key[6:]
        pattern = r"\d+w" if mode == "<N>w" else re.escape(mode)
        return bool(re.search(r'""(?:[a-z0-9]+\+)*' + pattern + r"(?=[:+])", template))
    if key.startswith("plural:"):
        separator = {"members": "/", "others": "\\", "party": "|"}[key[7:]]
        return bool(re.search(r"<<[^<>]*" + re.escape(separator) + r"[^<>]*>>", template))
    if key == "random:choice":
        return bool(re.search(r"\[\[(?!list:)[^\[\]]*\]\]", template))
    if key == "random:list":
        return "[[list:" in template
    if key == "state:resting":
        return "__" in template and "/" in template
    return True  # Finalization and evaluation-order rules are narrative contracts.


def validate(corpus, coverage, spec_bytes):
    fields(corpus, ["version", "spec", "contexts", "cases", "stability_groups"])
    require(type(corpus["version"]) is int and corpus["version"] == 1, "corpus version")
    fields(corpus["spec"], ["path", "sha256"])
    require(corpus["spec"]["path"] == "docs/voice-rooms.md", "spec path")
    require(corpus["spec"]["sha256"] == hashlib.sha256(spec_bytes).hexdigest(), "spec drift")
    require(type(corpus["contexts"]) is dict and corpus["contexts"], "context map")
    for name, context in corpus["contexts"].items():
        text(name)
        validate_context(context)
    fields(coverage, ["version", "features", "ambiguities"])
    require(type(coverage["version"]) is int and coverage["version"] == 1, "coverage version")
    array(coverage["features"])
    array(coverage["ambiguities"])
    feature_map = {}
    for feature in coverage["features"]:
        fields(feature, ["id", "literal", "section", "case_ids"])
        text(feature["id"])
        text(feature["literal"])
        require(feature["id"] not in feature_map, "duplicate feature ID")
        require(feature["section"] in ["V5", "V6"], "coverage section")
        require(feature["literal"] in spec_bytes.decode(), "coverage literal absent from spec")
        string_array(feature["case_ids"])
        feature_map[feature["id"]] = feature
    require(set(feature_map) == spec_inventory(spec_bytes.decode()), "feature inventory mismatch")
    ambiguity_map = {}
    for ambiguity in coverage["ambiguities"]:
        fields(ambiguity, ["id", "question", "case_ids"])
        text(ambiguity["id"])
        text(ambiguity["question"])
        require(ambiguity["id"] not in ambiguity_map, "duplicate ambiguity ID")
        string_array(ambiguity["case_ids"])
        ambiguity_map[ambiguity["id"]] = ambiguity
    array(corpus["cases"])
    case_map, signatures, exact_signatures = {}, {}, set()
    counts = {"exact": 0, "invariant": 0, "deferred": 0}
    for case in corpus["cases"]:
        fields(case, ["id", "input", "context", "expected", "covers"])
        text(case["id"])
        text(case["input"], empty=True)
        text(case["context"])
        require(case["id"] not in case_map, "duplicate case ID")
        require(case["context"] in corpus["contexts"], "missing context")
        string_array(case["covers"])
        for key in case["covers"]:
            require(key in feature_map, "unknown coverage feature")
            require(feature_used(key, case["input"]), "feature not exercised by input")
        expected = case["expected"]
        require(type(expected) is dict, "expected object")
        kind = expected.get("kind")
        require(type(kind) is str and kind in counts, "unknown expected kind")
        if kind == "exact":
            fields(expected, ["kind", "output"])
            text(expected["output"])
            require(len(expected["output"]) <= 100, "output exceeds 100 characters")
            require(trimmed_output(expected["output"]), "untrimmed output")
        elif kind == "invariant":
            fields(expected, ["kind", "nonempty", "max_characters", "stable_for_same_context"],
                   ["allowed_outputs", "casefold_equals"])
            require(expected["nonempty"] is True, "nonempty invariant required")
            require(type(expected["max_characters"]) is int and expected["max_characters"] == 100,
                    "100-character invariant required")
            require(expected["stable_for_same_context"] is True, "stability invariant required")
            if "allowed_outputs" in expected:
                string_array(expected["allowed_outputs"])
                require(all(len(s) <= 100 and trimmed_output(s) for s in expected["allowed_outputs"]),
                        "invalid allowed output")
            if "casefold_equals" in expected:
                target = expected["casefold_equals"]
                text(target)
                require(target.casefold() == target, "case-folded target required")
                if "allowed_outputs" in expected:
                    require(any(s.casefold() == target for s in expected["allowed_outputs"]),
                            "contradictory invariant constraints")
        else:
            fields(expected, ["kind", "ambiguity_id"])
            text(expected["ambiguity_id"])
            require(expected["ambiguity_id"] in ambiguity_map, "missing ambiguity")
        signature = canonical([case["input"], corpus["contexts"][case["context"]]])
        require(signature not in signatures or signatures[signature] == expected,
                "conflicting expectations for identical input/context")
        signatures[signature] = expected
        if kind == "exact":
            exact_signatures.add(signature)
        counts[kind] += 1
        case_map[case["id"]] = case
    # Do not let unresolved, invariant or duplicated probes inflate the minimum.
    require(len(exact_signatures) >= 150, "fewer than 150 distinct exact golden cases")
    array(corpus["stability_groups"])
    require(bool(corpus["stability_groups"]), "missing cross-rename stability groups")
    group_ids = set()
    for group in corpus["stability_groups"]:
        fields(group, ["id", "case_ids"])
        text(group["id"])
        require(group["id"] not in group_ids, "duplicate stability group ID")
        group_ids.add(group["id"])
        string_array(group["case_ids"])
        require(len(group["case_ids"]) >= 2, "stability group needs two cases")
        require(set(group["case_ids"]) <= case_map.keys(), "missing stability case")
        rows = [case_map[id] for id in group["case_ids"]]
        contexts = [corpus["contexts"][row["context"]] for row in rows]
        require(all(row["expected"]["kind"] == "invariant" for row in rows),
                "stability group requires invariant cases")
        require(all(row["input"] == rows[0]["input"] and row["expected"] == rows[0]["expected"]
                    for row in rows), "stability inputs or expectations differ")
        require(all(context["seed"] == contexts[0]["seed"] for context in contexts),
                "stability seeds differ")
        require(len({canonical(context) for context in contexts}) >= 2,
                "stability group must change non-seed context")
    for key, feature in feature_map.items():
        actual = {case["id"] for case in case_map.values() if key in case["covers"]}
        require(set(feature["case_ids"]) == actual, "coverage reverse index mismatch")
    for key, ambiguity in ambiguity_map.items():
        actual = {case["id"] for case in case_map.values()
                  if case["expected"].get("ambiguity_id") == key}
        require(set(ambiguity["case_ids"]) == actual, "ambiguity reverse index mismatch")
    digest = hashlib.sha256(canonical([corpus, coverage]).encode()).hexdigest()
    return {"cases": len(case_map), "distinct_inputs": len(signatures), **counts,
            "features": len(feature_map), "ambiguities": len(ambiguity_map), "sha256": digest}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", type=Path, default=HERE / "corpus.json")
    parser.add_argument("--coverage", type=Path, default=HERE / "coverage.json")
    parser.add_argument("--spec", type=Path, default=ROOT / "docs/voice-rooms.md")
    args = parser.parse_args()
    try:
        first = validate(load(args.corpus), load(args.coverage), args.spec.read_bytes())
        second = validate(load(args.corpus), load(args.coverage), args.spec.read_bytes())
        require(first == second, "non-deterministic validation result")
    except (ValueError, OSError, TypeError, KeyError, IndexError) as error:
        print(f"INVALID: {error}", file=sys.stderr)
        return 1
    print(json.dumps(first, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
