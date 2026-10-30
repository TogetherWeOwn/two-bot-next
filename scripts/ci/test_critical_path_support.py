"""Offline fixtures for the critical-path acceptance tests; never run real Cargo/DBs.

The recorder is below the selection boundary: real workflow shells/planners and
partition runners execute, but Cargo returns a synthetic workspace inventory.
Expected executions come from the frozen pre-partition commands, not the planner.
"""

import fnmatch
import json
import os
import subprocess
import sys
import tempfile
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
BOOTSTRAP = "postgres://agent_test:@agent-testdb:5432/two_bot_test_ci"
EXACT = "postgres://agent_test@agent-testdb:5432/agent_test"
EMPTY_PASSWORD = "postgres://agent_test:@agent-testdb:5432/agent_test"
ENV_KEYS = (
    "TWO_TEST_DATABASE_URL", "TWO_RSVP_RUNTIME_TEST_DATABASE_URL",
    "TWO_BOT_TEST_DATABASE_URL", "TEST_DATABASE_URL", "MEMBER_TESTDB",
    "AUTOMOD_CI_TESTDB", "TWO_LFG_TESTDB_CI", "TWO_DATABASE_TLS",
    "TWO_CUSTOM_COMMAND_TEST_CI", "TWO_AUTOMATION_TRANSFER_TEST_CI",
    "TWO_GATEWAY_TEST_DATABASE_URL", "PGPASSWORD", "PGPASSFILE", "PGOPTIONS",
    "PAPERCLIP_RUN_SCRATCH_DIR",
)
PACKAGES = {
    "two-bot-core": "core", "two-bot-discord": "discord", "two-bot": "bot",
    "two-bot-cutover": "cutover", "two-bot-store": "store",
    "two-bot-testsupport": "testsupport", "two-bot-next": "root",
}
# The remainder retains its public id; these are three package-wide libtest
# selections, not three binary-only sweeps or a new partition framework.
IGNORED_BOT_LEGS = (
    "sweep-ignored-bot-gateway", "sweep-ignored-bot-self-role", "sweep-ignored-bot",
)


def sweep_package(leg_id):
    return "bot" if leg_id in IGNORED_BOT_LEGS else leg_id.rsplit("-", 1)[1]


def workflow():
    import yaml

    return yaml.safe_load((ROOT / ".github/workflows/check.yml").read_text())["jobs"]


def inventory():
    rows = []

    def add(package, kind, target, name, ignored=False):
        rows.append(dict(package=package, kind=kind, target=target,
                         name=name, ignored=ignored))

    for package in PACKAGES:
        target = package.replace("-", "_")
        for kind in ("lib", "test", "doc"):
            for name, ignored in (("ordinary", False), ("ordinary_ignored", True),
                                  ("property_smoke", False)):
                add(package, kind, target if kind != "test" else "ordinary_contract",
                    name, ignored)
    for name, ignored in (("ordinary", False), ("ordinary_ignored", True),
                          ("property_smoke", False)):
        add("two-bot", "bin", "two-bot", name, ignored)
    for prefix in (
        "onboarding_", "raid_runtime_", "join_risk_runtime_", "containment_worker_",
        "containment_runtime_evidence_", "fail_open_containment_acceptance::",
        "gateway_tests::", "command_runtime_tests::", "feed_jobs::",
        "scorecard_marks_only_captured_streams", "member_join_fact_round_trip_dedupes",
        "configured_database_reports_seeded_family_frontier",
        "hotpath_settings_test_support::queries::observer_sees_actual_rows_and_rejects_unbounded_control",
        "settings_jobs::tests::ops_overrides_acceptance_tests::",
        "ticket_runtime::cursor_budget", "voice_rooms::tests::name_filter_acceptance",
    ):
        kind = "lib" if prefix.startswith("voice_rooms") else "bin"
        target = "two_bot" if kind == "lib" else "two-bot"
        for suffix, ignored in (("normal", False), ("ignored", True)):
            add("two-bot", kind, target, prefix + suffix, ignored)
    targets = {
        "two-bot-core": ("stream_live_store", "member_moderation_db", "feeds_store"),
        "two-bot-discord": (
            "mutation_429_proves_no_effect_channel", "channel_moderation_runtime",
            "internal_channel_moderation", "internal_events", "community_facts_capture"),
        "two-bot-store": ("starboard_store", "starboard_api", "starboard_config_updated_at"),
        "two-bot-cutover": (
            "raid_list_db", "reengagement_list_db", "game_activity_report_db", "settings_db",
            "voice_rooms_store", "voice_config_store", "voice_create_admission_store",
            "legacy_copy_db", "legacy_verify_db", "legacy_tls_fence", "staging_migrate_db",
            "invite_store", "member_list_catchup"),
        "two-bot": ("containment_worker_soak", "alive", "restore_drill"),
    }
    for package, names in targets.items():
        for target in names:
            add(package, "test", target, "ordinary", False)
            add(package, "test", target, "ordinary_ignored", True)
    add("two-bot-core", "lib", "two_bot_core", "channel_moderation::normal")
    add("two-bot-core", "lib", "two_bot_core", "channel_moderation::ignored", True)
    add("two-bot-core", "test", "future_unlisted_contract", "new_target_is_not_lost")
    add("two-bot-core", "test", "future_unlisted_contract", "new_ignored_target_is_not_lost", True)
    return rows


def dependency(name, *, features=(), kind=None, rename=None, optional=False):
    return dict(name=name, source=None, req="^0.4.0", kind=kind, rename=rename,
                optional=optional, uses_default_features=True, features=list(features),
                target=None, registry=None,
                path=f"/fixture/crates/{PACKAGES.get(name, name)}")


def feature_state(graph, selected, requested=(), *, defaults=True, all_features=False):
    """Small independent resolver for local, unconditional test dependencies.

    Unlike a raw-flag ledger this unifies features of all selected roots, then
    projects only observable dependency features onto each test. External,
    platform and build dependencies are outside this synthetic fixture's scope;
    unsupported graph shapes fail rather than pretending to model Cargo fully.
    """
    packages = {p["name"]: p for p in graph["packages"]}
    by_id = {p["id"]: p for p in graph["packages"]}
    if not set(selected).issubset(packages):
        raise ValueError("unknown selected fixture package")
    roots = {packages[name]["id"] for name in selected}
    enabled = {key: set() for key in by_id}
    active, optional, edges = set(roots), set(), {key: set() for key in by_id}

    def add(package_id, feature):
        if feature not in by_id[package_id].get("features", {}):
            raise ValueError(f"unknown fixture feature {by_id[package_id]['name']}/{feature}")
        enabled[package_id].add(feature)

    def deps(package_id):
        for dep in by_id[package_id]["dependencies"]:
            if dep.get("target") is not None or dep.get("kind") == "build":
                raise ValueError("unmodelled conditional/build dependency")
            if dep.get("kind") == "dev" and package_id not in roots:
                continue
            yield dep

    def forward(package_id, alias, feature=None, *, weak=False):
        matches = [dep for dep in deps(package_id) if (dep.get("rename") or dep["name"]) == alias]
        if not matches:
            raise ValueError(f"unknown fixture dependency alias {alias}")
        for dep in matches:
            if weak and (package_id, alias) not in optional and dep.get("optional"):
                continue
            optional.add((package_id, alias))
            if (feature is not None and dep.get("optional")
                    and by_id[package_id].get("features", {}).get(alias) == ["dep:" + alias]):
                add(package_id, alias)
            target_id = packages[dep["name"]]["id"]
            active.add(target_id)
            if feature is not None:
                add(target_id, feature)

    for root in roots:
        declared = by_id[root].get("features", {})
        if defaults and "default" in declared:
            add(root, "default")
        if all_features:
            enabled[root].update(declared)
    for flag in requested:
        owners = []
        for root in roots:
            if "/" in flag:
                owner, feature = flag.split("/", 1)
                if owner == by_id[root]["name"]:
                    owners.append((root, feature, None))
                elif any((dep.get("rename") or dep["name"]) == owner for dep in deps(root)):
                    owners.append((root, feature, owner))
            elif flag in by_id[root].get("features", {}):
                owners.append((root, flag, None))
        if not owners:
            raise ValueError(f"no selected package accepts fixture feature {flag}")
        for root, feature, alias in owners:
            if alias is None:
                add(root, feature)
            else:
                forward(root, alias, feature)

    previous = None
    while previous != (active, enabled, optional):
        previous = (set(active), {key: set(value) for key, value in enabled.items()}, set(optional))
        for package_id in list(active):
            for feature in list(enabled[package_id]):
                for expansion in by_id[package_id]["features"][feature]:
                    if expansion.startswith("dep:"):
                        forward(package_id, expansion[4:])
                    elif "/" in expansion:
                        alias, value = expansion.split("/", 1)
                        forward(package_id, alias.removesuffix("?"), value, weak=alias.endswith("?"))
                    else:
                        add(package_id, expansion)
            for dep in deps(package_id):
                alias = dep.get("rename") or dep["name"]
                if dep.get("optional") and (package_id, alias) not in optional:
                    continue
                target = packages[dep["name"]]
                target_id = target["id"]
                active.add(target_id)
                edges[package_id].add(target_id)
                if dep.get("uses_default_features", True) and "default" in target.get("features", {}):
                    add(target_id, "default")
                for feature in dep.get("features", []):
                    add(target_id, feature)
    projected = {}
    for root in roots:
        closure, pending = set(), [root]
        while pending:
            current = pending.pop()
            if current not in closure:
                closure.add(current)
                pending.extend(edges[current])
        projected[by_id[root]["name"]] = tuple(sorted(
            by_id[key]["name"] + "/" + feature for key in closure for feature in enabled[key]))
    return enabled, projected


def metadata(rows):
    # Internal normal/dev edges and feature forwarding mirror the manifests.
    # Leaf features replace external dependencies, not workspace relationships.
    edges = {
        "two-bot-core": [dependency("two-bot-testsupport", kind="dev")],
        "two-bot-discord": [dependency("two-bot-core"),
                            dependency("two-bot-discord", features=("test-support",), kind="dev"),
                            dependency("two-bot-cutover", kind="dev"),
                            dependency("two-bot-store", kind="dev"),
                            dependency("two-bot-testsupport", kind="dev")],
        "two-bot": [dependency("two-bot-core", features=("db",)),
                    dependency("two-bot-discord", features=("db",)),
                    dependency("two-bot-cutover"), dependency("two-bot-store"),
                    dependency("two-bot-discord", features=("test-support",), kind="dev"),
                    dependency("two-bot-testsupport", kind="dev")],
        "two-bot-cutover": [dependency("two-bot-core", features=("db",)),
                            dependency("two-bot-discord"), dependency("two-bot-store"),
                            dependency("two-bot-testsupport", kind="dev")],
        "two-bot-store": [dependency("two-bot-core", features=("db",)),
                          dependency("two-bot-discord", kind="dev")],
    }
    features = {"two-bot-core": {"default": [], "db": []},
                "two-bot-discord": {"default": [], "db": ["two-bot-core/db"],
                                    "db-tests": ["db"], "test-support": []}}
    directories = PACKAGES | {r["package"]: PACKAGES.get(r["package"], r["package"]) for r in rows}
    packages = []
    for package, directory in directories.items():
        targets = sorted({(r["kind"], r["target"]) for r in rows if r["package"] == package})
        packages.append({
            "id": package, "name": package, "version": "0.4.0",
            "manifest_path": f"/fixture/crates/{directory}/Cargo.toml",
            "dependencies": edges.get(package, []), "features": features.get(package, {}),
            "targets": [dict(name=name, kind=[kind], crate_types=["lib" if kind == "lib" else "bin"],
                             test=True, doctest=kind == "lib",
                             src_path=f"/fixture/crates/{directory}/{kind}/{name}.rs",
                             **{"required-features": next((r.get("required_features", []) for r in rows
                                if (r["package"], r["kind"], r["target"]) == (package, kind, name)), [])},
                             edition="2021")
                        for kind, name in targets if kind != "doc"],
        })
    graph = {"packages": packages, "workspace_members": list(directories),
             "workspace_default_members": list(directories), "version": 1,
             "workspace_root": "/fixture", "target_directory": "/fixture/target"}
    enabled, _ = feature_state(graph, directories, ["two-bot-core/db"])
    graph["resolve"] = {"nodes": [{"id": p["id"], "features": sorted(enabled[p["id"]]),
        "dependencies": sorted({dep["name"] for dep in p["dependencies"]}),
        "deps": [{"name": (dep.get("rename") or dep["name"]).replace("-", "_"),
                  "pkg": dep["name"], "dep_kinds": [{"kind": dep["kind"], "target": None}]}
                 for dep in p["dependencies"]]} for p in packages]}
    return graph


def offline_integration_fixture():
    """Representative metadata, not a real-workspace coverage qualification."""
    targets = (
        ("two-bot-core", "leveling_store"),
        ("two-bot-core", "future_unlisted_contract"),
        ("two-bot-discord", "internal_channel_moderation"),
        ("two-bot-store", "invite_uses_saturation_acceptance"),
        ("two-bot-cutover", "raid_list_db"),
        ("two-bot", "containment_worker_soak"),
    )
    return metadata([dict(package=package, kind="test", target=target,
                          name="ordinary", ignored=False)
                     for package, target in targets])


def strict_cargo_metadata_run(fixture):
    """Answer only metadata; unexpected subprocess access fails, never skips."""
    payload = json.dumps(fixture)
    expected = ["cargo", "metadata", "--format-version", "1", "--locked",
                "--features", "two-bot-core/db"]

    def run(argv, **kwargs):
        if list(argv) != expected:
            raise AssertionError(f"unexpected subprocess use in offline test: {argv!r}")
        return subprocess.CompletedProcess(argv, 0, stdout=payload, stderr="")

    return run


def selections(argv, rows, graph=None):
    """Independent, deliberately narrow Cargo/libtest selector oracle.

    Unknown flags fail the fixture instead of silently treating them as filters.
    It covers the baseline command shapes, not arbitrary Cargo/nextest semantics.
    """
    args = list(argv)
    if args[:2] == ["llvm-cov", "test"]:
        del args[:2]
    elif args and args[0] in ("test", "llvm-cov", "run"):
        args.pop(0)
    if "--" in args:
        split = args.index("--")
        cargo, libtest = args[:split], args[split + 1:]
    else:
        cargo, libtest = args, []
    packages, kinds, targets, filters, skips, features = [], set(), [], [], [], []
    excluded = set()
    defaults, all_features = True, False
    threads, ignored, include_ignored, exact = None, False, False, False
    i = 0
    while i < len(cargo):
        word = cargo[i]
        if word in ("-p", "--package", "--test", "--bin", "--example", "--features", "--exclude"):
            value = cargo[i + 1]
            i += 2
            if word in ("-p", "--package"):
                packages.append(value)
            elif word == "--exclude":
                excluded.add(value)
            elif word == "--features":
                features.extend(value.replace(",", " ").split())
            else:
                kind = {"--test": "test", "--bin": "bin", "--example": "example"}[word]
                kinds.add(kind)
                targets.append((kind, value))
            continue
        if word in ("--lib", "--bins", "--tests", "--doc"):
            if word == "--tests":
                kinds.update(("lib", "bin", "test"))
            else:
                kinds.add({"--lib": "lib", "--bins": "bin", "--doc": "doc"}[word])
        elif word.startswith("--package="):
            packages.append(word.split("=", 1)[1])
        elif word.startswith("--test="):
            kinds.add("test")
            targets.append(("test", word.split("=", 1)[1]))
        elif word.startswith("--features="):
            features.extend(word.split("=", 1)[1].replace(",", " ").split())
        elif word == "--no-default-features":
            defaults = False
        elif word == "--all-features":
            all_features = True
        elif word not in ("--workspace", "--locked", "--offline", "--no-report", "--ignore-run-fail"):
            if word.startswith("-"):
                raise ValueError(f"unmodelled Cargo option {word!r}")
            filters.append(word)
        i += 1
    if "--list" in libtest:
        include_ignored = True
    i = 0
    while i < len(libtest):
        word = libtest[i]
        if word in ("--skip", "--test-threads"):
            value = libtest[i + 1]
            i += 2
            if word == "--skip":
                skips.append(value)
            else:
                threads = value
            continue
        if word == "--ignored":
            ignored = True
        elif word == "--include-ignored":
            include_ignored = True
        elif word == "--exact":
            exact = True
        elif word.startswith("--test-threads="):
            threads = word.split("=", 1)[1]
        elif word not in ("--nocapture", "--list", "--format=terse"):
            if word.startswith("-"):
                raise ValueError(f"unmodelled libtest option {word!r}")
            filters.append(word)
        i += 1
    graph = graph if graph is not None else metadata(rows)
    by_name = {p["name"]: p for p in graph["packages"]}
    packages = set(packages or by_name) - excluded
    enabled, projected = feature_state(graph, packages, features, defaults=defaults, all_features=all_features)
    kinds = kinds or {"lib", "bin", "test"}
    chosen = []
    for row in rows:
        if row["package"] not in packages or row["kind"] not in kinds:
            continue
        patterns = [target for kind, target in targets if kind == row["kind"]]
        if patterns and not any(fnmatch.fnmatchcase(row["target"], target) for target in patterns):
            continue
        package = by_name[row["package"]]
        target = next((t for t in package["targets"]
                       if t["name"] == row["target"] and row["kind"] in t["kind"]), {})
        required = target.get("required-features", row.get("required_features", []))
        if not set(required).issubset(enabled[package["id"]]):
            if patterns:
                raise ValueError(f"explicit fixture target {row['target']} requires disabled features")
            continue
        if row["ignored"] and not (ignored or include_ignored):
            continue
        if ignored and not row["ignored"]:
            continue
        matches = lambda f: row["name"] == f if exact else f in row["name"]
        if filters and not any(matches(f) for f in filters):
            continue
        if any(matches(s) for s in skips):
            continue
        chosen.append((row["package"], row["kind"], row["target"], row["name"],
                       projected[row["package"]], threads, "--ignore-run-fail" in cargo))
    return chosen


def ledger(records, rows, *, environments=False, graph=None):
    result = Counter()
    for record in records:
        for unit in selections(record["argv"], rows, graph):
            key = unit
            if environments:
                key += (tuple((k, record.get("env", {}).get(k)) for k in ENV_KEYS),)
            result[key] += 1
    return result


class Sandbox:
    def __init__(self, rows=None, graph=None):
        self.temp = tempfile.TemporaryDirectory(prefix="critical-path-acceptance-")
        self.root = Path(self.temp.name)
        self.rows = rows if rows is not None else inventory()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        # Pre-create the shared fixture scratch root. Focused acceptance tests
        # may delete it to prove runners prepare the root themselves, but
        # unrelated fixtures must not depend on a sibling target creating it.
        (self.root / "two-bot-check-scratch").mkdir()
        self.log = self.root / "calls.jsonl"
        (self.root / "inventory.json").write_text(json.dumps(self.rows))
        self.graph = graph if graph is not None else metadata(self.rows)
        (self.root / "metadata.json").write_text(json.dumps(self.graph))
        self.env = {
            "PATH": str(self.bin) + os.pathsep + os.defpath, "HOME": str(self.root),
            "RUNNER_TEMP": str(self.root), "GITHUB_WORKSPACE": str(self.root),
            "GITHUB_ENV": str(self.root / "github-env"),
            "GITHUB_OUTPUT": str(self.root / "github-output"),
            "PAPERCLIP_RUN_SCRATCH_DIR": str(self.root / "two-bot-check-scratch"),
            "ACCEPTANCE_HELPERS": str(Path(__file__).parent),
            "ACCEPTANCE_INVENTORY": str(self.root / "inventory.json"),
            "ACCEPTANCE_METADATA": str(self.root / "metadata.json"),
            "ACCEPTANCE_LOG": str(self.log), "PYTHONDONTWRITEBYTECODE": "1",
            "PYTHONPATH": os.pathsep.join(str(Path(p).resolve()) for p in sys.path if p),
        }
        self.executable("cargo", '''import json, os, sys
from pathlib import Path
sys.path.insert(0, os.environ['ACCEPTANCE_HELPERS'])
from test_critical_path_support import ENV_KEYS, selections
args = sys.argv[1:]
rows = json.loads(Path(os.environ['ACCEPTANCE_INVENTORY']).read_text())
graph = json.loads(Path(os.environ['ACCEPTANCE_METADATA']).read_text())
if args and args[0] == 'metadata':
    print(json.dumps(graph))
    raise SystemExit(0)
if args[:2] == ['llvm-cov', 'report']:
    if os.environ.get('EMPTY_REPORT') != '1':
        text = 'SF:crates/core/src/lib.rs\\nDA:1,1\\nend_of_record\\n'
        if '--output-path' in args:
            Path(args[args.index('--output-path') + 1]).write_text(text)
        else:
            print(text, end='')
    raise SystemExit(int(os.environ.get('REPORT_EXIT', '0')))
if not args or args[0] not in ('test', 'llvm-cov', 'run'):
    raise SystemExit('unexpected Cargo invocation: ' + repr(args))
try:
    units = selections(args, rows, graph)
except ValueError as error:
    print('error: ' + str(error), file=sys.stderr)
    raise SystemExit(101)
if '--list' in args:
    for unit in units:
        print(unit[3] + ': test')
    print(str(len(units)) + ' tests, 0 benchmarks')
    raise SystemExit(0)
effective_env = {k: os.environ[k] for k in ENV_KEYS if k in os.environ}
if 'PAPERCLIP_RUN_SCRATCH_DIR' in effective_env:
    effective_env['PAPERCLIP_RUN_SCRATCH_DIR'] = effective_env['PAPERCLIP_RUN_SCRATCH_DIR'].replace(os.environ['RUNNER_TEMP'], '$RUNNER_TEMP')
with open(os.environ['ACCEPTANCE_LOG'], 'a') as out:
    out.write(json.dumps({'argv': args, 'env': effective_env,
                         'effective_paperclip_run_scratch_dir': os.environ.get('PAPERCLIP_RUN_SCRATCH_DIR')}) + '\\n')
needle = os.environ.get('FAIL_NEEDLE')
if needle and needle in ' '.join(args):
    raise SystemExit(41)
print('test result: ok. ' + str(len(units)) + ' passed')
''')
        self.executable("cargo-llvm-cov", "import os, sys\nos.execv(os.path.join(os.path.dirname(__file__), 'bin/cargo'), ['cargo', *sys.argv[1:]])\n", directory=self.root)
        for name in ("rustup", "curl", "sha256sum", "tar"):
            self.executable(name, "raise SystemExit(0)\n")

    def close(self):
        self.temp.cleanup()

    def executable(self, name, body, directory=None):
        path = (directory or self.bin) / name
        path.write_text(f"#!{sys.executable}\n" + body)
        path.chmod(0o755)
        return path

    def run(self, shell, env=None, cwd=None):
        return subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", shell],
                              cwd=cwd or ROOT, env=self.env | (env or {}), text=True,
                              capture_output=True, timeout=30)

    def records(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def clear(self):
        if self.log.exists():
            self.log.unlink()


def resolved_env(job, step, sandbox, matrix=None):
    env = {}
    for key, value in (job.get("env", {}) | step.get("env", {})).items():
        value = str(value)
        value = value.replace("${{ runner.temp }}", str(sandbox.root))
        for field, replacement in (matrix or {}).items():
            value = value.replace("${{ matrix." + field + " }}", str(replacement))
        if "${{" in value:
            continue
        env[key] = value
    return env


def coverage_step(jobs):
    steps = jobs["coverage-shard"]["steps"]
    return next(s for s in steps if "SHARD_LEGS" in s.get("env", {}))


def run_coverage(sandbox, leg_ids, **extra):
    jobs = workflow()
    step = coverage_step(jobs)
    env = resolved_env(jobs["coverage-shard"], step, sandbox)
    env.update(SHARD_LEGS=" ".join(leg_ids), SHARD_INDEX="0")
    return sandbox.run(step["run"], env | extra)


def assertions_message(proc):
    return f"exit={proc.returncode}\n{proc.stdout}\n{proc.stderr}"
