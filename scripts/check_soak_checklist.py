#!/usr/bin/env python3
"""Offline parity/checklist coverage and rendered-document guard (stdlib only)."""

import argparse
from collections import Counter
import json
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]
CONFIG_ROW = (7, ("Config / env catalogue",))


def split_cells(text):
    """Split a table line with optional leading/trailing border pipes."""
    body = text
    if body.startswith("|"):
        body = body[1:]
    if body.endswith("|") and not body.endswith("\\|"):
        body = body[:-1]
    # Escaped pipes are cell content, not table boundaries.
    return tuple(c.strip().replace(r"\|", "|") for c in
                 re.split(r"(?<!\\)\|", body))


def mapped_owner(mapping):
    """Slice/issue owner that keeps a DROP-prefixed row in scope."""
    return re.search(r"\b(?:S\d+|B\d+|NEW-\d+|TOG-\d+)\b", mapping)


def looks_mapped(cell):
    """True when a last-cell value claims parity mapping of any kind."""
    mapping = cell.replace("*", "").strip()
    return bool(re.match(r"^DROP\b", mapping) or mapped_owner(mapping))


def parity_rows(markdown):
    """Read Map tables in §§1–8; exclude only wholly DROP-mapped rows."""
    section = None
    headers = None
    rows = []
    seen_sections = set()
    for line in markdown.splitlines():
        heading = re.match(r"^## (\d+)\. ", line)
        if re.match(r"^##\s", line):
            section = int(heading[1]) if heading else None
            headers = None
            if section in range(1, 9):
                seen_sections.add(section)
            continue
        if section not in range(1, 9):
            continue
        stripped = line.strip()
        if not stripped or "|" not in stripped:
            headers = None
            continue
        cells = split_cells(stripped)
        if all(re.fullmatch(r":?-+:?", c) for c in cells):
            continue
        if headers is None:
            if cells and cells[-1] == "Map":
                # A bordered or borderless header opens table scope.
                headers = cells
            elif not stripped.startswith("|") and looks_mapped(cells[-1]):
                # Fail closed: a borderless mapped row separated from its
                # table by a blank line must not silently skip coverage
                # either. Border the row or reword the prose.
                raise ValueError(
                    f"§{section}: unsupported borderless table row: {stripped[:60]}")
            elif stripped.startswith("|"):
                raise ValueError(f"§{section}: parity table must end in Map")
            # Borderless prose carrying pipes is ordinary prose, not a table.
            continue
        if not stripped.startswith("|"):
            # Fail closed: a GFM body row without its leading border must
            # not silently skip coverage while a table is active.
            raise ValueError(
                f"§{section}: table row without leading border: {stripped[:60]}")
        if len(cells) != len(headers):
            raise ValueError(f"§{section}: malformed table row: {cells}")
        mapping = cells[-1].replace("*", "").strip()
        if not mapping:
            raise ValueError(f"§{section}: unmapped row: {cells}")
        # A DROP prefix can still carry mapped work, including replacement
        # owners in parentheses. Semicolons also occur within drop reasons.
        if mapped_owner(mapping) or not re.match(r"^DROP\b", mapping):
            rows.append((section, cells[:-1]))
    if seen_sections != set(range(1, 9)):
        raise ValueError("Expected all parity sections 1–8")
    # §7 is a prose catalogue, not a table. Still require explicit coverage.
    config = re.search(r"^## 7\. .*?\n(.*?)(?=^## 8\.)", markdown, re.M | re.S)
    if not config or not all(word in config[1] for word in ("env_only", "cold", "hot")):
        raise ValueError("§7 must describe env_only/cold/hot config classes")
    rows.append(CONFIG_ROW)
    counts = Counter(section for section, _ in rows)
    if set(counts) != set(range(1, 9)):
        raise ValueError("Every parity section must have mapped coverage")
    if len(rows) != len(set(rows)):
        raise ValueError("Duplicate parity source rows")
    return rows


def validate(markdown, checklist):
    expected = set(parity_rows(markdown))
    if checklist.get("schema_version") != 1:
        raise ValueError("Unsupported checklist schema_version")
    entries = checklist.get("entries", [])
    actual = []
    ids = []
    for entry in entries:
        ids.append(entry["id"])
        source = entry["parity"]
        key = (source["section"], tuple(source["row"]))
        actual.append(key)
        if entry["status"] not in ("automated", "manual", "waived"):
            raise ValueError(f"{entry['id']}: invalid status")
        for field in ("id", "action", "expected", "evidence"):
            if not isinstance(entry.get(field), str) or not entry[field].strip():
                raise ValueError(f"{entry.get('id')}: missing {field}")
        if entry["status"] == "waived" and not entry.get("reason", "").strip():
            raise ValueError(f"{entry['id']}: waiver requires reason")
        if entry["status"] == "automated" and not entry.get("verification", "").strip():
            raise ValueError(f"{entry['id']}: automated requires verification")
        # Voice-sensitive rows are cross-links, never a second voice procedure.
        voice = any("voice" in c.lower() for c in key[1])
        voice = voice or (key[0] == 3 and "ShardResume" in key[1][0])
        if voice and "TOG-10119" not in entry.get("reference", ""):
            raise ValueError(f"{entry['id']}: voice coverage must reference TOG-10119")
    if len(ids) != len(set(ids)) or len(actual) != len(set(actual)):
        raise ValueError("Duplicate checklist IDs or parity rows")
    missing = expected - set(actual)
    stale = set(actual) - expected
    if missing or stale:
        raise ValueError(f"Coverage mismatch: missing={sorted(missing)!r}; stale={sorted(stale)!r}")
    return Counter(section for section, _ in expected)


def render(checklist):
    lines = ["# Parity soak checklist", "", "<!-- Generated from soak-checklist.json by scripts/check_soak_checklist.py --render. -->", ""]
    lines += checklist["instructions"] + [""]
    for section in range(1, 9):
        lines += [f"## {section}. {checklist['sections'][str(section)]}", ""]
        for entry in checklist["entries"]:
            if entry["parity"]["section"] != section:
                continue
            label = " — ".join(entry["parity"]["row"][:2])
            lines += [f"### {entry['id']}: {label}", "",
                      f"- **Method:** `{entry['status']}` (not an execution verdict).",
                      f"- **Action:** {entry['action']}",
                      f"- **Expected:** {entry['expected']}",
                      f"- **Evidence:** {entry['evidence']}"]
            for field in ("verification", "reason", "reference"):
                if entry.get(field):
                    lines.append(f"- **{field.title()}:** {entry[field]}")
            lines.append("")
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--render", action="store_true", help="Print the generated Markdown")
    args = parser.parse_args()
    checklist = json.loads((ROOT / "docs/soak-checklist.json").read_text())
    try:
        counts = validate((ROOT / "docs/parity.md").read_text(), checklist)
        output = render(checklist)
        if args.render:
            print(output, end="")
        else:
            if (ROOT / "docs/soak-checklist.md").read_text() != output:
                raise ValueError("Markdown drift: regenerate with --render")
            print(f"PASS: {sum(counts.values())}/{sum(counts.values())} non-DROP rows; "
                  + ", ".join(f"§{s}={counts[s]}" for s in sorted(counts)))
    except (ValueError, KeyError, TypeError) as error:
        parser.exit(1, f"FAIL: {error}\n")


if __name__ == "__main__":
    main()
