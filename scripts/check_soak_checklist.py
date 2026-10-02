#!/usr/bin/env python3
"""Offline parity/checklist coverage and rendered-document guard (stdlib only)."""

import argparse
from collections import Counter
import json
from pathlib import Path
import re

ROOT = Path(__file__).resolve().parents[1]
CONFIG_ROW = (7, ("Config / env catalogue",))
PARITY_SECTIONS = tuple(range(1, 9))
# §12 additions and §13 non-replay ledger obligations (parity §11 B4 gate).
DELTA_SECTIONS = (12, 13)
SECTIONS = PARITY_SECTIONS + DELTA_SECTIONS
DISPOSITION_HEADERS = ("Disposition", "Next boundary / disposition")
LEDGER_HEADER = ("Legacy commit", "Area", "Change", "Status", "Disposition")
LEDGER_STATUSES = ("ported", "carded", "gap", "dropped")
CARD = re.compile(r"\bTOG-\d+\b")


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


def table_rows(markdown, sections, last_headers):
    """Body rows of the tables in numbered `sections`, failing closed.

    Returns ``(rows, seen)``: rows are ``(section, headers, cells)`` and seen
    holds every requested section heading present in the document.
    """
    section = None
    headers = None
    rows = []
    seen = set()
    for line in markdown.splitlines():
        heading = re.match(r"^## (\d+)\. ", line)
        if re.match(r"^##\s", line):
            section = int(heading[1]) if heading else None
            headers = None
            if section in sections:
                seen.add(section)
            continue
        if section not in sections:
            continue
        stripped = line.strip()
        if not stripped or "|" not in stripped:
            headers = None
            continue
        cells = split_cells(stripped)
        if all(re.fullmatch(r":?-+:?", c) for c in cells):
            continue
        if headers is None:
            if cells and cells[-1] in last_headers:
                # A bordered or borderless header opens table scope.
                headers = cells
            elif not stripped.startswith("|") and looks_mapped(cells[-1]):
                # Fail closed: a borderless mapped row separated from its
                # table by a blank line must not silently skip coverage
                # either. Border the row or reword the prose.
                raise ValueError(
                    f"§{section}: unsupported borderless table row: {stripped[:60]}")
            elif stripped.startswith("|"):
                raise ValueError(
                    f"§{section}: parity table must end in {' or '.join(last_headers)}")
            # Borderless prose carrying pipes is ordinary prose, not a table.
            continue
        if not stripped.startswith("|"):
            # Fail closed: a GFM body row without its leading border must
            # not silently skip coverage while a table is active.
            raise ValueError(
                f"§{section}: table row without leading border: {stripped[:60]}")
        if len(cells) != len(headers):
            raise ValueError(f"§{section}: malformed table row: {cells}")
        rows.append((section, headers, cells))
    return rows, seen


def parity_rows(markdown):
    """Read Map tables in §§1–8; exclude only wholly DROP-mapped rows."""
    rows = []
    table, seen_sections = table_rows(markdown, PARITY_SECTIONS, ("Map",))
    for section, _, cells in table:
        mapping = cells[-1].replace("*", "").strip()
        if not mapping:
            raise ValueError(f"§{section}: unmapped row: {cells}")
        # A DROP prefix can still carry mapped work, including replacement
        # owners in parentheses. Semicolons also occur within drop reasons.
        if mapped_owner(mapping) or not re.match(r"^DROP\b", mapping):
            rows.append((section, cells[:-1]))
    if seen_sections != set(PARITY_SECTIONS):
        raise ValueError("Expected all parity sections 1–8")
    # §7 is a prose catalogue, not a table. Still require explicit coverage.
    config = re.search(r"^## 7\. .*?\n(.*?)(?=^## 8\.)", markdown, re.M | re.S)
    if not config or not all(word in config[1] for word in ("env_only", "cold", "hot")):
        raise ValueError("§7 must describe env_only/cold/hot config classes")
    rows.append(CONFIG_ROW)
    counts = Counter(section for section, _ in rows)
    if set(counts) != set(PARITY_SECTIONS):
        raise ValueError("Every parity section must have mapped coverage")
    if len(rows) != len(set(rows)):
        raise ValueError("Duplicate parity source rows")
    return rows


def unlink(cell):
    """Render a source cell as a heading: keep link text, drop targets."""
    return re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", cell)


def owner_cards(cell):
    """TOG cards a disposition cell names, in first-mention order."""
    return tuple(dict.fromkeys(CARD.findall(cell)))


def delta_rows(markdown):
    """Map §12 rows and §13 non-dropped ledger rows to their owner cards.

    Keys copy every source cell except the disposition, like ``parity_rows``.
    §12 keeps the existing DROP rule; §13 excludes ``dropped`` ledger rows
    (reasoned drops, including all history-rewrite replays), whose features
    stay mapped in §§1–8. Every retained row must name an owner card, except
    ``ported`` ledger rows, which cite source evidence instead.
    """
    rows = {}
    table, seen = table_rows(markdown, DELTA_SECTIONS, DISPOSITION_HEADERS)
    if seen != set(DELTA_SECTIONS):
        raise ValueError("Expected parity sections 12 and 13")
    for section, headers, cells in table:
        disposition = cells[-1].replace("*", "").strip()
        if not disposition:
            raise ValueError(f"§{section}: row without disposition: {cells}")
        owners = owner_cards(disposition)
        if section == 13:
            if headers != LEDGER_HEADER:
                raise ValueError(f"§13: ledger columns must be {' | '.join(LEDGER_HEADER)}")
            status = cells[3]
            if status not in LEDGER_STATUSES:
                raise ValueError(f"§13: unknown ledger status {status!r}: {cells[0]}")
            if status == "dropped":
                continue
            if status != "ported" and not owners:
                raise ValueError(f"§13: {status} row needs an owner card: {cells[0]}")
        elif re.match(r"^DROP\b", disposition) and not mapped_owner(disposition):
            continue
        elif not owners:
            raise ValueError(f"§12: row needs an owner card: {cells[0][:60]}")
        key = (section, cells[:-1])
        if key in rows:
            raise ValueError("Duplicate parity source rows")
        rows[key] = owners
    counts = Counter(section for section, _ in rows)
    if set(counts) != set(DELTA_SECTIONS):
        raise ValueError("§12 and §13 must each have retained coverage")
    return rows


def validate(markdown, checklist):
    owners = delta_rows(markdown)
    expected = set(parity_rows(markdown)) | set(owners)
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
        if entry["status"] == "waived" and not entry.get("approver", "").strip():
            raise ValueError(f"{entry['id']}: waiver requires approver")
        if entry["status"] == "automated" and not entry.get("verification", "").strip():
            raise ValueError(f"{entry['id']}: automated requires verification")
        # Voice-sensitive rows are cross-links, never a second voice procedure.
        voice = any("voice" in c.lower() for c in key[1])
        voice = voice or (key[0] == 3 and "ShardResume" in key[1][0])
        if voice and "TOG-10119" not in entry.get("reference", ""):
            raise ValueError(f"{entry['id']}: voice coverage must reference TOG-10119")
        if key[0] in DELTA_SECTIONS:
            owner = entry.get("owner")
            if (not isinstance(owner, list) or not owner
                    or not all(isinstance(card, str) and CARD.fullmatch(card) for card in owner)):
                raise ValueError(f"{entry['id']}: §{key[0]} entry requires owner cards")
            # The owner list tracks the disposition; a moved card is stale.
            if owners.get(key) and tuple(owner) != owners[key]:
                raise ValueError(
                    f"{entry['id']}: stale owner {owner}; parity names {list(owners[key])}")
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
    for section in SECTIONS:
        lines += [f"## {section}. {checklist['sections'][str(section)]}", ""]
        for entry in checklist["entries"]:
            if entry["parity"]["section"] != section:
                continue
            row = entry["parity"]["row"]
            # §12 cites evidence links and §13 an area list; label by name.
            if section == 12:
                label = unlink(row[0])
            elif section == 13:
                label = f"{unlink(row[0])} — {row[2]}"
            else:
                label = " — ".join(row[:2])
            lines += [f"### {entry['id']}: {label}", "",
                      f"- **Method:** `{entry['status']}` (not an execution verdict).",
                      f"- **Action:** {entry['action']}",
                      f"- **Expected:** {entry['expected']}",
                      f"- **Evidence:** {entry['evidence']}"]
            if entry.get("owner"):
                lines.append("- **Owner:** " + ", ".join(
                    f"[{card}](/TOG/issues/{card})" for card in entry["owner"]))
            for field in ("verification", "reason", "approver", "reference"):
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
