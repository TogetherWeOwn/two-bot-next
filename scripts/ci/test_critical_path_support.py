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


def metadata(rows):
    packages = []
    for package, directory in PACKAGES.items():
        targets = sorted({(r["kind"], r["target"]) for r in rows if r["package"] == package})
        packages.append({
            "id": package, "name": package, "version": "0.4.0",
            "manifest_path": f"/fixture/crates/{directory}/Cargo.toml",
            "dependencies": [],
            "targets": [dict(name=name, kind=[kind], crate_types=["lib" if kind == "lib" else "bin"],
                             test=True, doctest=kind in ("lib", "doc"),
                             src_path=f"/fixture/crates/{directory}/{kind}/{name}.rs",
                             required_features=[], edition="2024")
                        for kind, name in targets if kind != "doc"],
        })
    return {"packages": packages, "workspace_members": list(PACKAGES),
            "workspace_default_members": list(PACKAGES), "version": 1,
            "workspace_root": "/fixture", "target_directory": "/fixture/target",
            "resolve": {"nodes": [{"id": p, "deps": [], "dependencies": [], "features": []}
                                  for p in PACKAGES]}}


def selections(argv, rows):
    """Independent, deliberately narrow Cargo/libtest selector oracle.

    Unknown flags fail the fixture instead of silently treating them as filters.
    It covers the baseline command shapes, not arbitrary Cargo/nextest semantics.
    """
    args = list(argv)
    if args and args[0] in ("test", "llvm-cov", "run"):
        args.pop(0)
    if "--" in args:
        split = args.index("--")
        cargo, libtest = args[:split], args[split + 1:]
    else:
        cargo, libtest = args, []
    packages, kinds, targets, filters, skips, features = [], set(), [], [], [], []
    excluded = set()
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
    packages = set(packages or PACKAGES) - excluded
    kinds = kinds or {"lib", "bin", "test"}
    chosen = []
    for row in rows:
        if row["package"] not in packages or row["kind"] not in kinds:
            continue
        patterns = [target for kind, target in targets if kind == row["kind"]]
        if patterns and not any(fnmatch.fnmatchcase(row["target"], target) for target in patterns):
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
        feature_set = tuple(sorted(f if "/" in f else row["package"] + "/" + f
                                   for f in features))
        chosen.append((row["package"], row["kind"], row["target"], row["name"],
                       feature_set, threads, "--ignore-run-fail" in cargo))
    return chosen


def ledger(records, rows, *, environments=False):
    result = Counter()
    for record in records:
        for unit in selections(record["argv"], rows):
            key = unit
            if environments:
                key += (tuple((k, record.get("env", {}).get(k)) for k in ENV_KEYS),)
            result[key] += 1
    return result


class Sandbox:
    def __init__(self, rows=None):
        self.temp = tempfile.TemporaryDirectory(prefix="critical-path-acceptance-")
        self.root = Path(self.temp.name)
        self.rows = rows if rows is not None else inventory()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "calls.jsonl"
        (self.root / "inventory.json").write_text(json.dumps(self.rows))
        self.env = {
            "PATH": str(self.bin) + os.pathsep + os.defpath, "HOME": str(self.root),
            "RUNNER_TEMP": str(self.root), "GITHUB_WORKSPACE": str(self.root),
            "GITHUB_ENV": str(self.root / "github-env"),
            "GITHUB_OUTPUT": str(self.root / "github-output"),
            "PAPERCLIP_RUN_SCRATCH_DIR": str(self.root / "two-bot-check-scratch"),
            "ACCEPTANCE_HELPERS": str(Path(__file__).parent),
            "ACCEPTANCE_INVENTORY": str(self.root / "inventory.json"),
            "ACCEPTANCE_LOG": str(self.log), "PYTHONDONTWRITEBYTECODE": "1",
            "PYTHONPATH": os.pathsep.join(str(Path(p).resolve()) for p in sys.path if p),
        }
        self.executable("cargo", '''import json, os, sys
from pathlib import Path
sys.path.insert(0, os.environ['ACCEPTANCE_HELPERS'])
from test_critical_path_support import ENV_KEYS, metadata, selections
args = sys.argv[1:]
rows = json.loads(Path(os.environ['ACCEPTANCE_INVENTORY']).read_text())
if args and args[0] == 'metadata':
    print(json.dumps(metadata(rows)))
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
units = selections(args, rows)
if '--list' in args:
    for unit in units:
        print(unit[3] + ': test')
    print(str(len(units)) + ' tests, 0 benchmarks')
    raise SystemExit(0)
effective_env = {k: os.environ[k] for k in ENV_KEYS if k in os.environ}
if 'PAPERCLIP_RUN_SCRATCH_DIR' in effective_env:
    effective_env['PAPERCLIP_RUN_SCRATCH_DIR'] = effective_env['PAPERCLIP_RUN_SCRATCH_DIR'].replace(os.environ['RUNNER_TEMP'], '$RUNNER_TEMP')
with open(os.environ['ACCEPTANCE_LOG'], 'a') as out:
    out.write(json.dumps({'argv': args, 'env': effective_env}) + '\\n')
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
