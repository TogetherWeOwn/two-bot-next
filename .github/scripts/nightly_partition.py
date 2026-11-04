#!/usr/bin/env python3
"""Partition the original nightly libtest inventory, not packages or test prefixes.

Cargo still resolves/builds the entire all-feature workspace with its default
selection (including ordinary examples). Doctests run separately through Cargo.
Only libtest execution is partitioned; its original substring skips are applied
by libtest during discovery, before exact target-qualified assignment.
"""
import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

PARTITIONS = 6
JOBS = {"changes", "sweep-plan", "sweep-partition", "sweep-guarded", "sweep-docs"}
ROOT = Path(__file__).resolve().parents[2]
CARGO = ["cargo", "test", "--workspace", "--all-features", "--locked"]


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, ensure_ascii=True).encode()).hexdigest()


def partition(inventory):
    identities = []
    if not isinstance(inventory, dict):
        raise ValueError("inventory must be target-qualified")
    for target, names in sorted(inventory.items()):
        if not isinstance(target, str) or not target or not isinstance(names, list):
            raise ValueError("invalid target inventory")
        if any(not isinstance(name, str) or not name or "\n" in name for name in names):
            raise ValueError("invalid test identity")
        if len(set(names)) != len(names):
            raise ValueError("duplicate test identity within a target")
        identities.extend([target, name] for name in sorted(names))
    # Hash ordering avoids clustering slow module prefixes; round-robin keeps
    # counts within one test, as in the shipped coverage planner. No timing claim
    # follows from test counts: executing PR legs must still be measured.
    identities.sort(key=lambda identity: (digest(identity), identity))
    parts = [identities[i::PARTITIONS] for i in range(PARTITIONS)]
    if not all(parts):
        raise ValueError("missing or empty expected partition")
    return parts


def parse_list(output):
    names = []
    for line in output.splitlines():
        if not line:
            continue
        match = re.fullmatch(r"(.+): test", line)
        if not match:
            raise ValueError("unknown libtest inventory output")
        names.append(match[1])
    if len(names) != len(set(names)):
        raise ValueError("duplicate libtest inventory identity")
    return sorted(names)


def check_execution(output, expected, results):
    # Rust 1.99.1's ConsoleTestState::write_log_result writes only status/name
    # records here, not captured output or inherited child stdout/stderr. Its
    # supported (deprecated) --logfile flag avoids parsing mixed console bytes.
    completed = []
    for line in results.splitlines():
        match = re.fullmatch(r"ok (.+)", line)
        if not match:
            raise ValueError("libtest result log contains a failed, ignored or unknown record")
        completed.append(match[1])
    # The serial harness closes with its summary after test/child diagnostics.
    # Earlier summary-looking lines are diagnostics, not extra harness results.
    lines = output.rstrip().splitlines()
    summary = re.match(
        r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured;",
        lines[-1] if lines else "",
    )
    if (not expected or len(set(completed)) != len(completed)
            or sorted(completed) != sorted(expected)
            or summary is None or summary.groups() != (str(len(expected)), "0", "0", "0")):
        raise ValueError("execution differs from non-empty expected inventory")


def validate_args(args):
    if args[:2] != ["--include-ignored", "--test-threads=1"]:
        raise ValueError("nightly must include ignored tests and remain serial")
    rest = args[2:]
    if len(rest) % 2 or any(rest[i] != "--skip" or not rest[i + 1] for i in range(0, len(rest), 2)):
        raise ValueError("only original substring skips may follow nightly execution flags")
    return args


def command_json(args):
    return json.loads(subprocess.check_output(args, cwd=ROOT, text=True))


def target_key(package, target):
    return "|".join([package["name"], ",".join(target["kind"]), target["name"]])


def discover(test_args):
    metadata = command_json(["cargo", "metadata", "--no-deps", "--all-features", "--locked", "--format-version=1"])
    packages = {p["id"]: p for p in metadata["packages"] if p["id"] in metadata["workspace_members"]}
    expected = {}
    for package in packages.values():
        for target in package["targets"]:
            if target["test"]:
                if target["kind"] not in [["lib"], ["bin"], ["test"], ["example"], ["proc-macro"]]:
                    raise ValueError("unknown Cargo test target kind")
                key = target_key(package, target)
                if key in expected:
                    raise ValueError("duplicate Cargo test target")
                expected[key] = package
    # Do not substitute --lib/--bins/--tests or --all-targets: default cargo test
    # also compiles ordinary examples without turning them into test harnesses.
    built = subprocess.run(CARGO + ["--no-run", "--message-format=json"], cwd=ROOT, text=True, stdout=subprocess.PIPE)
    built.check_returncode()
    targets = {}
    for line in built.stdout.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-message":
            print(message["message"].get("rendered", ""), file=sys.stderr)
        if (message.get("reason") != "compiler-artifact" or not message["profile"]["test"]
                or not message.get("executable") or message["package_id"] not in packages):
            continue
        package = packages[message["package_id"]]
        key = target_key(package, message["target"])
        if key not in expected or key in targets:
            raise ValueError("unknown or duplicate compiled test target")
        manifest = Path(package["manifest_path"])
        executable = Path(message["executable"])
        env = dict(os.environ, CARGO_MANIFEST_DIR=str(manifest.parent), CARGO_MANIFEST_PATH=str(manifest))
        # Cargo's runtime search path and package working directory matter for
        # integration fixtures. These are local freshly built paths, never paths
        # deserialized from the uploaded plan.
        libraries = [executable.parent, Path(metadata["target_directory"]) / "debug/deps"]
        env["LD_LIBRARY_PATH"] = os.pathsep.join(map(str, libraries)) + os.pathsep + env.get("LD_LIBRARY_PATH", "")
        targets[key] = {"executable": str(executable), "cwd": manifest.parent, "env": env}
    if targets.keys() != expected.keys():
        raise ValueError("missing compiled Cargo test target")
    inventory, unfiltered = {}, {}
    for key, target in sorted(targets.items()):
        base = [target["executable"], "--list", "--format=terse"]
        options = {"cwd": target["cwd"], "env": target["env"], "text": True, "timeout": 60}
        unfiltered[key] = parse_list(subprocess.check_output(base + ["--include-ignored"], **options))
        inventory[key] = parse_list(subprocess.check_output(base + test_args, **options))
        if not set(inventory[key]).issubset(unfiltered[key]):
            raise ValueError("filtered inventory contains unknown tests")
    return inventory, unfiltered, targets


def validate_plan(plan, sha):
    if plan.get("version") != 1 or plan.get("sha") != sha:
        raise ValueError("plan version or exact source SHA mismatch")
    if plan.get("partitions") != partition(plan["inventory"]):
        raise ValueError("partition union is not the original complete/disjoint inventory")


def run_partition(plan, index, test_args, receipt, sha):
    validate_plan(plan, sha)
    if type(index) is not int or not 0 <= index < PARTITIONS:
        raise ValueError("unknown partition")
    start = time.monotonic()
    inventory, unfiltered, targets = discover(test_args)
    if (inventory != plan["inventory"] or unfiltered != plan["unfiltered"]
            or test_args != plan["test_args"]):
        raise ValueError("local discovery differs from original plan")
    selected = plan["partitions"][index]
    executed, failures = [], []
    for key, target in sorted(targets.items()):
        names = [name for target_id, name in selected if target_id == key]
        if not names:
            continue
        # Exact negative filters allow one serial harness invocation per target
        # without substring collisions, shell word splitting or per-test process
        # isolation. Exclude quarantined/rerouted tests as well as sibling parts.
        args = [target["executable"], "--include-ignored", "--test-threads=1", "--exact", "--color=never"]
        for name in unfiltered[key]:
            if name not in names:
                args.extend(["--skip", name])
        if sum(len(arg.encode()) + 1 for arg in args) > 1_000_000:
            raise ValueError("libtest argument budget exceeded")
        actual = parse_list(subprocess.check_output(args + ["--list", "--format=terse"], cwd=target["cwd"], env=target["env"], text=True, timeout=60))
        if actual != sorted(names):
            raise ValueError("exact selected inventory differs before execution")
        print(f"::group::{key}: {len(names)} tests (partition {index})", flush=True)
        # Fresh per-invocation evidence cannot reuse another target's results.
        with tempfile.TemporaryDirectory(prefix="nightly-libtest-") as scratch:
            results = Path(scratch) / "results.log"
            output = []
            # Stream diagnostics unchanged; only the result file supplies names.
            # The event-bound workflow timeout remains the outer process-tree cap.
            with subprocess.Popen(args + ["--logfile", str(results)], cwd=target["cwd"], env=target["env"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT) as process:
                for line in process.stdout:
                    sys.stdout.buffer.write(line)
                    sys.stdout.buffer.flush()
                    output.append(line)
                code = process.wait()
            print("::endgroup::", flush=True)
            try:
                if code:
                    raise ValueError(f"libtest exit {code}")
                check_execution(b"".join(output).decode("utf-8", errors="replace"), names, results.read_text())
            except (ValueError, OSError) as error:
                failures.append(f"{key}: {error}")
            else:
                executed.extend([key, name] for name in names)
    if failures:
        raise ValueError("; ".join(failures))
    if sorted(executed) != sorted(selected):
        raise ValueError("partition execution is incomplete")
    receipt.parent.mkdir(parents=True, exist_ok=True)
    receipt.write_text(json.dumps({"version": 1, "sha": sha, "partition": index,
        "plan_digest": digest(plan), "executed": selected,
        "elapsed_seconds": round(time.monotonic() - start, 2)}, indent=2) + "\n")


def aggregate(needs, plan, receipts, sha):
    if set(needs) != JOBS or needs["changes"].get("result") != "success":
        raise ValueError("selector or expected nightly dependency missing/failed")
    selected = needs["changes"].get("outputs", {}).get("rust") != "false"
    if not selected:
        if any(needs[job].get("result") != "skipped" for job in JOBS - {"changes"}):
            raise ValueError("deselected nightly dependency did not skip cleanly")
        return
    if any(needs[job].get("result") != "success" for job in JOBS):
        raise ValueError("selected nightly dependency failed, cancelled or skipped")
    validate_plan(plan, sha)
    files = sorted(receipts.glob("partition-*.json"))
    if {p.name for p in files} != {f"partition-{i}.json" for i in range(PARTITIONS)}:
        raise ValueError("missing or unknown partition receipt")
    for index in range(PARTITIONS):
        receipt = json.loads((receipts / f"partition-{index}.json").read_text())
        if (receipt.get("version") != 1 or receipt.get("sha") != sha
                or receipt.get("partition") != index or receipt.get("plan_digest") != digest(plan)
                or receipt.get("executed") != plan["partitions"][index]):
            raise ValueError("partition receipt does not prove its exact expected execution")
        print(f"partition {index}: {len(receipt['executed'])} tests; {receipt.get('elapsed_seconds', 'unknown')} seconds")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["plan", "run", "docs", "aggregate"])
    parser.add_argument("--plan", type=Path, default=Path("nightly-plan.json"))
    parser.add_argument("--partition", type=int)
    parser.add_argument("--receipt", type=Path)
    parser.add_argument("--receipts", type=Path, default=Path("nightly-receipts"))
    parser.add_argument("--needs", type=Path)
    # -- separates the verbatim original libtest arguments from this CLI.
    argv = sys.argv[1:]
    boundary = argv.index("--") if "--" in argv else len(argv)
    args = parser.parse_args(argv[:boundary])
    test_args = argv[boundary + 1:]
    sha = os.environ["GITHUB_SHA"]
    if args.mode == "aggregate":
        needs = json.loads(args.needs.read_text())
        selected = needs.get("changes", {}).get("outputs", {}).get("rust") != "false"
        plan = json.loads(args.plan.read_text()) if selected else None
        aggregate(needs, plan, args.receipts, sha)
    elif args.mode == "docs":
        plan = json.loads(args.plan.read_text())
        validate_plan(plan, sha)
        subprocess.run(CARGO + ["--doc", "--no-fail-fast", "--"] + validate_args(plan["test_args"]), cwd=ROOT, check=True)
    else:
        if args.mode == "plan":
            validate_args(test_args)
            inventory, unfiltered, _ = discover(test_args)
            plan = {"version": 1, "sha": sha, "inventory": inventory, "unfiltered": unfiltered,
                    "test_args": test_args, "partitions": partition(inventory)}
            args.plan.write_text(json.dumps(plan, indent=2) + "\n")
            print(f"Planned {sum(map(len, inventory.values()))} target-qualified tests in {PARTITIONS} non-empty partitions")
        else:
            plan = json.loads(args.plan.read_text())
            run_partition(plan, args.partition, validate_args(plan["test_args"]), args.receipt, sha)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
        print(f"nightly partition failed: {error}", file=sys.stderr)
        sys.exit(1)
