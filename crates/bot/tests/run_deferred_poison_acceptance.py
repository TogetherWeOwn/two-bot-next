"""Run protected deferred-poison acceptance with test wiring in a scratch copy.

No repository runtime function is written. Builder must register both child
modules in ordinary CI after consuming the protected tests. Optional mutants
change only a disposable run-owned snapshot, never Builder's checkout/ref.
"""

import argparse
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
BOT = Path("crates/bot/src/gateway.rs")
DISCORD = Path("crates/discord/src/leveling_runtime.rs")
REGISTRATIONS = {
    BOT: ('deferred_poison_acceptance', '\n#[cfg(test)]\n#[path = "gateway_deferred_poison_acceptance_tests.rs"]\nmod deferred_poison_acceptance;\n'),
    DISCORD: ('deferred_worker_poison', '\n#[cfg(feature = "test-support")]\n#[path = "../tests/support/deferred_worker_poison.rs"]\npub mod deferred_worker_poison;\n'),
}


def replace_once(text, old, new):
    if text.count(old) != 1:
        raise ValueError("mutant anchor changed or is ambiguous; refusing")
    return text.replace(old, new, 1)


def mutate(workspace, mutant):
    if mutant == "collect-default":
        path = workspace / BOT
        text = path.read_text()
        start = text.index("        let requests = requests_or_hold_cursor(\n")
        end = text.index("        // Voice after the cache update", start)
        old = text[start:end]
        new = replace_once(old, "let requests = requests_or_hold_cursor(\n            match disposition", "let requests = match disposition")
        new = replace_once(new, "            },\n            &mut dispatch_error,\n        );\n", "            }.unwrap_or_default();\n")
        path.write_text(replace_once(text, old, new))
    elif mutant == "facts-warn-only":
        path = workspace / BOT
        path.write_text(replace_once(path.read_text(),
            "if !hold_cursor_on_facts_poison(&error, &mut dispatch_error) {", "if true {"))
    elif mutant in ("config-enable-ok", "config-refresh-ok"):
        path = workspace / DISCORD
        text = path.read_text()
        signature = "    pub fn " + ("enable_with_config(" if mutant == "config-enable-ok" else "refresh_config(")
        start = text.index(signature)
        end = text.index("\n    }", start)
        old = text[start:end]
        new = replace_once(old,
            "let mut state = self.0.lock().map_err(|_| CommunityFactsError::Poisoned)?;",
            "let Ok(mut state) = self.0.lock() else { return Ok(()); };")
        path.write_text(replace_once(text, old, new))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--suite", choices=("worker", "supervisor", "discord"), default="worker")
    parser.add_argument("--controls", action="store_true")
    parser.add_argument("--mutant", choices=("collect-default", "facts-warn-only", "config-enable-ok", "config-refresh-ok"))
    args = parser.parse_args()
    if args.controls and args.suite != "worker":
        parser.error("--controls selects only the independent worker controls")
    if args.mutant and ((args.mutant.startswith("config-") and args.suite != "discord") or
                        (not args.mutant.startswith("config-") and args.suite != "worker")):
        parser.error("select the matching worker/discord suite for this mutant")
    scratch = os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("PAPERCLIP_SCRATCH_DIR")
    if not scratch:
        parser.error("set a run-owned Paperclip scratch directory; no fallback")
    scratch = Path(scratch).resolve(strict=True)
    originals = {path: (ROOT / path).read_bytes() for path in REGISTRATIONS}
    paths = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=ROOT,
    ).decode().split("\0")
    with tempfile.TemporaryDirectory(prefix="deferred-worker-tests-", dir=scratch) as tmp:
        workspace = Path(tmp)
        for name in filter(None, paths):
            relative = Path(name)
            if relative.is_absolute() or ".." in relative.parts:
                raise ValueError("snapshot path escapes workspace")
            source = ROOT / relative
            if not source.is_file():
                continue
            destination = workspace / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, destination, follow_symlinks=False)
        for path, (module, registration) in REGISTRATIONS.items():
            if f"mod {module};" not in originals[path].decode():
                (workspace / path).write_bytes(originals[path] + registration.encode())
        if args.mutant:
            mutate(workspace, args.mutant)
        if args.suite == "discord":
            target = ["-p", "two-bot-discord", "--features", "db", "--lib"]
            test_filter = "leveling_runtime::deferred_capture_poison_acceptance::"
        else:
            target = ["-p", "two-bot", "--bin", "two-bot"]
            test_filter = ("gateway::tests::gateway_poisoned_collect_holds_checkpoint_without_requests"
                           if args.suite == "supervisor" else "gateway::deferred_poison_acceptance::")
            if args.controls:
                test_filter += "healthy_"
        command = ["python3", "scripts/cargo_cache.py", "run", "--", "test", *target,
                   test_filter, "--", "--nocapture", "--test-threads=1"]
        for path, original in originals.items():
            print(f"Input {path} SHA-256: {hashlib.sha256(original).hexdigest()}", flush=True)
        print("Snapshot mutant:", args.mutant or "none (test registration only)", flush=True)
        print("Command:", " ".join(command), flush=True)
        result = subprocess.run(command, cwd=workspace, check=False)
    for path, original in originals.items():
        if (ROOT / path).read_bytes() != original:
            raise RuntimeError("author workspace input changed during verification")
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
