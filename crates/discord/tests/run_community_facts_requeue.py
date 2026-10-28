"""Run protected facts recovery acceptance against the real drain, offline.

Only test files are committed. In a run-owned scratch snapshot, bind the external
record_fact function to the scoped store double and register its child module.
Normally no drain, capture, classification, error, or state implementation changes.
An explicit --mutant injects exactly one regression ONLY into that scratch copy;
exit 75 is cache refusal, not an observed assertion failure.
Builder must keep the same cfg(test) binding/registration so ordinary library CI
runs these protected tests. Non-test builds keep the real store import.
--workspace generates the ignored command reference inside the scratch snapshot
before compiling bot tests; Discord-only modes do not need that prerequisite.
Generation and verification override any inherited REFERENCE_DOCS_ROOT with the snapshot.

Commands (controller Cargo builds always use the bounded cache wrapper):
  python3 crates/discord/tests/run_community_facts_requeue.py
  python3 crates/discord/tests/run_community_facts_requeue.py --controls
  python3 crates/discord/tests/run_community_facts_requeue.py --clippy
  python3 crates/discord/tests/run_community_facts_requeue.py --workspace
  python3 crates/discord/tests/run_community_facts_requeue.py --mutant rule-false-ack
  python3 crates/discord/tests/run_community_facts_requeue.py --mutant join-false-ack
  python3 crates/discord/tests/run_community_facts_requeue.py --mutant drop-count-assignment
  python3 crates/discord/tests/run_community_facts_requeue.py --controls --mutant poisoned-zero-counts
"""

import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
SOURCE = Path("crates/discord/src/leveling_runtime.rs")
TEST = Path("crates/discord/tests/support/community_facts_requeue.rs")
MODULE = "community_facts_requeue_acceptance"
ORIGINAL_IMPORT = "message_fact, record_fact, rules_accepted_fact,"
DOUBLE_IMPORT = f"use {MODULE}::record_fact;"
REGISTRATION = (
    '\n#[cfg(test)]\n#[path = "../tests/support/community_facts_requeue.rs"]\n'
    f"mod {MODULE};\n"
)
BINDING = (
    f"\n#[cfg(test)]\n{DOUBLE_IMPORT}\n"
    "#[cfg(not(test))]\nuse two_bot_core::community_store::record_fact;\n"
)


MUTANTS = {
    "rule-false-ack": (
        "            if record_fact(&pool, &fact).await? {\n"
        "                inserted += 1;\n"
        "            }\n"
        "            guard.rules_done += 1;",
        "            if record_fact(&pool, &fact).await? {\n"
        "                inserted += 1;\n"
        "                guard.rules_done += 1;\n"
        "            }",
    ),
    "join-false-ack": (
        "            if record_fact(&pool, &fact).await? {\n"
        "                inserted += 1;\n"
        "            }\n"
        "            guard.joins_done += 1;",
        "            if record_fact(&pool, &fact).await? {\n"
        "                inserted += 1;\n"
        "                guard.joins_done += 1;\n"
        "            }",
    ),
    "drop-count-assignment": (
        "        *dropped += overflow as u64;",
        "        *dropped = overflow as u64;",
    ),
    "poisoned-zero-counts": (
        "    pub fn dropped_counts(&self) -> Result<(u64, u64, u64), CommunityFactsError> {\n"
        "        let state = self.0.lock().map_err(|_| CommunityFactsError::Poisoned)?;",
        "    pub fn dropped_counts(&self) -> Result<(u64, u64, u64), CommunityFactsError> {\n"
        "        let Ok(state) = self.0.lock() else {\n"
        "            return Ok((0, 0, 0));\n"
        "        };",
    ),
}


def mutate(instrumented, name):
    before, after = MUTANTS[name]
    text = instrumented.decode()
    if text.count(before) != 1:
        raise ValueError(f"mutant {name}: expected exactly one mutation site")
    return text.replace(before, after, 1).encode()


def instrument(original):
    text = original.decode()
    if ORIGINAL_IMPORT in text:
        if text.count(ORIGINAL_IMPORT) != 1 or DOUBLE_IMPORT in text:
            raise ValueError("ambiguous store binding; do not modify runtime bodies")
        text = text.replace(ORIGINAL_IMPORT, "message_fact, rules_accepted_fact,", 1)
        text += BINDING
    elif DOUBLE_IMPORT not in text:
        raise ValueError("store import drift: use the actual cfg(test) double binding")
    if f"mod {MODULE};" not in text:
        text += REGISTRATION
    return text.encode()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--controls", action="store_true")
    mode.add_argument("--clippy", action="store_true")
    mode.add_argument("--workspace", action="store_true")
    parser.add_argument("--mutant", choices=tuple(MUTANTS), help="one scratch-only regression; never modifies the author workspace")
    args = parser.parse_args()
    if args.mutant and (args.clippy or args.workspace):
        parser.error("mutants run only the protected runtime or healthy controls")
    scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get(
        "PAPERCLIP_SCRATCH_DIR"
    )
    if not scratch:
        parser.error("set a run-owned PAPERCLIP_RUN_SCRATCH_DIR; no /tmp fallback")
    scratch = Path(scratch).resolve(strict=True)
    original = (ROOT / SOURCE).read_bytes()
    instrumented = instrument(original)
    if args.mutant:
        instrumented = mutate(instrumented, args.mutant)
    paths = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=ROOT,
    ).decode().split("\0")
    with tempfile.TemporaryDirectory(prefix="community-facts-tests-", dir=scratch) as tmp:
        workspace = Path(tmp)
        for name in filter(None, paths):
            relative = Path(name)
            if relative.is_absolute() or ".." in relative.parts:
                raise ValueError("workspace path escapes the snapshot")
            source = ROOT / relative
            if not source.is_file():
                continue
            destination = workspace / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, destination, follow_symlinks=False)
        (workspace / SOURCE).write_bytes(instrumented)
        environment = dict(os.environ, REFERENCE_DOCS_ROOT=str(workspace))
        command = ["python3", "scripts/cargo_cache.py", "run", "--"]
        if args.clippy:
            command += ["clippy", "-p", "two-bot-discord", "--features", "db", "--lib", "--tests", "--", "-D", "warnings"]
        elif args.workspace:
            command += ["test", "--workspace", "--features", "two-bot-discord/db", "--locked"]
        else:
            test_filter = MODULE + ("::healthy_control_" if args.controls else "::")
            command += ["test", "-p", "two-bot-discord", "--features", "db", "--lib", test_filter, "--", "--nocapture", "--test-threads=1"]
        print("Runtime SHA-256:", hashlib.sha256(original).hexdigest(), flush=True)
        print("Protected test SHA-256:", hashlib.sha256((ROOT / TEST).read_bytes()).hexdigest(), flush=True)
        if args.mutant:
            print("Scratch-only mutant:", args.mutant, flush=True)
            print("Mutant runtime SHA-256:", hashlib.sha256(instrumented).hexdigest(), flush=True)
        else:
            print("Scratch changes: external store import + cfg(test) registration only", flush=True)
        try:
            if args.workspace:
                prerequisite = ["python3", "scripts/generate_command_reference.py", "--controller"]
                print("Prerequisite:", " ".join(prerequisite), flush=True)
                generated = subprocess.run(prerequisite, cwd=workspace, env=environment, check=False)
                if generated.returncode:
                    return generated.returncode
            print("Command:", " ".join(command), flush=True)
            result = subprocess.run(command, cwd=workspace, env=environment, check=False)
        finally:
            if (ROOT / SOURCE).read_bytes() != original:
                raise RuntimeError("author workspace source changed during verification")
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
