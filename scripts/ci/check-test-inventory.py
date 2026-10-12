#!/usr/bin/env python3
"""HARD gate: every test target and fuzz target is built or run by some CI job.

A target that no workflow references is an orphan: it compiles (or rots)
without ever executing, so a regression hides until release. The gate fails
with the orphan list; add the target to a lane or record a reasoned exemption
in EXEMPT below.

Coverage rules, in order:
1. Integration targets (``crates/*/tests/*.rs``) whose file contains
   ``#[ignore]`` must be name-referenced in a run context in ``check.yml``
   or ``nightly.yml`` (comments and ``--skip`` tokens are stripped first, so
   a sweep skip alone does not count as coverage).
2. Integration targets without ``#[ignore]`` are blanket-covered by the
   workspace ``--test '*'`` integration run (asserted present).
3. Every fuzz target in ``fuzz/Cargo.toml`` must have a matching
   ``fuzz_targets/<name>.rs`` and the ``fuzz-compile`` job must build them.
4. The workspace lib/bins unit run, the doc-test run and an ``--all-targets``
   build (examples) must each be present.
"""

import argparse
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# Targets no CI job may run, each with the reason it stays unrouted. A reason
# is required: an orphan without one is a regression, not an exemption.
EXEMPT = {
    # Manual regen: rewrites docs/commands.md and docs/configuration.md in the
    # checkout, so CI must never run it (ignored-db-runtime documents this).
    "reference_docs": "manual doc regen; rewrites committed files",
}

WORKSPACE_LIB_BINS = "cargo test --workspace --lib --bins"
WORKSPACE_DOC = "cargo test --workspace --doc"
FUZZ_BUILD = "cargo +nightly-2026-10-01 fuzz build"


def strip_comments_and_skips(text):
    kept = [line for line in text.splitlines()
            if not line.lstrip().startswith("#")]
    return re.sub(r"--skip\s+\S+", "", "\n".join(kept))


def check(root=ROOT):
    errors = []
    try:
        check_yml = (root / ".github/workflows/check.yml").read_text()
        nightly_yml = (root / ".github/workflows/nightly.yml").read_text()
    except OSError as exc:
        return [f"workflows unreadable: {exc}"]
    runnable = strip_comments_and_skips(check_yml + "\n" + nightly_yml)

    for path in sorted(root.glob("crates/*/tests/*.rs")):
        name = path.stem
        has_ignored = bool(re.search(r"#\[ignore", path.read_text(errors="replace")))
        if name in EXEMPT:
            if not EXEMPT[name].strip():
                errors.append(f"{name}: exemption reason is empty")
            continue
        if re.search(r"\b%s\b" % re.escape(name), runnable):
            continue
        if not has_ignored and "--test '*'" in check_yml:
            continue
        errors.append(f"orphaned test target: {name} "
                      f"({'has #[ignore]d tests' if has_ignored else 'not referenced'}; "
                      "route it to a lane or exempt it with a reason)")

    try:
        manifest = tomllib.loads((root / "fuzz/Cargo.toml").read_text())
        bins = [b["name"] for b in manifest.get("bin", []) if "name" in b]
    except (OSError, tomllib.TOMLDecodeError) as exc:
        return errors + [f"fuzz manifest unreadable: {exc}"]
    for name in bins:
        if not (root / "fuzz" / "fuzz_targets" / f"{name}.rs").is_file():
            errors.append(f"fuzz target {name} has no fuzz/fuzz_targets/{name}.rs")
    if FUZZ_BUILD not in check_yml:
        errors.append("fuzz-compile job does not build all fuzz targets "
                      f"(expected {FUZZ_BUILD!r})")

    for marker, label in ((WORKSPACE_LIB_BINS, "workspace lib/bins unit run"),
                          (WORKSPACE_DOC, "workspace doc-test run"),
                          ("--all-targets", "all-targets build (examples)")):
        if marker not in check_yml and marker not in nightly_yml:
            errors.append(f"missing {label} ({marker!r} not found in check/nightly)")
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args()
    errors = check(args.root)
    for error in errors:
        print(f"test inventory: {error}", file=sys.stderr)
    if not errors:
        print("test inventory: every target is routed to a CI job")
    return bool(errors)


if __name__ == "__main__":
    raise SystemExit(main())
