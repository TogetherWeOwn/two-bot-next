#!/usr/bin/env python3
"""Plan coverage shards from changed files (two-bot-next CI sharding).

Emits the coverage leg matrix: package partitions of the two workspace
sweeps plus dedicated guarded ignored/DB replays, matching the coverage-shard
job in check.yml, and how to partition them into N shards.

Fail-closed: any planning error (unreadable diff, malformed metadata,
unknown paths) widens to the full set with ``widened: true``. ``main``
always exits 0 on planning inputs; only output write failures exit 1.
"""

import argparse
import json
import shlex
import subprocess
import sys
from pathlib import Path

from cargo_feature_selection import workspace_feature_args

ROOT = Path(__file__).resolve().parents[2]

ALL_CRATES = [
    "two-bot-core",
    "two-bot-discord",
    "two-bot",
    "two-bot-cutover",
    "two-bot-store",
    "two-bot-testsupport",
    "two-bot-next",
]

# Command templates retain the baseline skip lists. Resolve sweep templates
# with resolve_sweep_leg(leg, metadata) before comparing or copying commands:
# effective feature flags depend on the selected workspace dependency graph.
FULL_LEGS = [
    {
        "id": "sweep-nonignored",
        "command": (
            "cargo llvm-cov --workspace --lib --bins --tests "
            "--features two-bot-core/db --locked --ignore-run-fail --no-report -- "
            "--skip cli_write_failure_and_crash_never_publish_prune_or_upload_a_partial_dump "
            "--skip seeded_aggregates_render_exactly_guild_scoped_and_read_only "
            "--skip uses_one_above_i32_max_saturates_and_write_proceeds "
            "--skip uses_u64_max_saturates_at_i32_max "
            "--skip saturated_write_warns_with_identifiers_and_no_raw_counter "
            "--skip normal_uses_round_trip_byte_identical "
            "--skip exact_i32_max_writes_without_saturation_warn "
            "--skip settings_jobs::tests::ops_overrides_acceptance_tests::"
        ),
        "supplemental_commands": [
            "cargo llvm-cov --no-report -p two-bot --bin two-bot "
            "--features two-bot-core/db --locked -- "
            "settings_jobs::tests::ops_overrides_acceptance_tests:: "
            "--test-threads=1 --skip regenerate_reference_docs"
        ],
        "crates": list(ALL_CRATES),
    },
    {
        "id": "sweep-ignored",
        "command": (
            "cargo llvm-cov --workspace --lib --bins --tests "
            "--features two-bot-core/db --locked --ignore-run-fail "
            "--no-report -- --ignored --test-threads=1 --skip regenerate_reference_docs "
            "--skip flags_are_guild_scoped_half_open_deduplicated_and_activity_protected "
            "--skip list_is_guild_scoped_selector_shaped_and_read_only "
            "--skip claim_is_owned_once_then_in_flight "
            "--skip failed_send_releases_the_claim_for_the_retry "
            "--skip delivered_rows_are_never_reclaimed "
            "--skip delivered_rows_expire_only_past_the_horizon_with_the_fence_intact "
            "--skip fresh_claim_cannot_be_reclaimed "
            "--skip stale_unposted_claim_is_taken_over_atomically "
            "--skip started_claim_is_never_reclaimed_even_when_stale "
            "--skip started_send_that_provably_failed_releases_for_the_retry "
            "--skip reclaiming_a_missing_key_claims_it_fresh "
            "--skip database::actions_persist_rows_and_preserve_other_events_and_guilds "
            "--skip parity_mismatch_cursor_and_read_only_acceptance "
            "--skip legacy_verify_policy_gates_the_real_test_endpoints "
            "--skip real_sqlx_runner_cases "
            "--skip post_failure_unreadable_ledger_is_unknown_not_empty "
            "--skip post_failure_readable_ledger_keeps_array_output "
            "--skip post_apply_unreadable_ledger_is_unknown_not_empty "
            "--skip store_failure_keeps_committed_cutover_evidence "
            "--skip connection_count_sees_other_roles_from_plan_login "
            "--skip single_delete_429_releases_attempt_for_same_key_retry "
            "--skip bulk_delete_429_releases_attempt_for_same_key_retry "
            "--skip channel_overwrite_429_releases_attempt_and_retires_only_its_seed "
            "--skip slowmode_429_releases_attempt_for_same_and_distinct_keys "
            "--skip unlock_429_releases_lane_but_preserves_recovery_seed "
            "--skip lockdown_429_preserves_prior_seed_while_releasing_the_lane "
            "--skip slowmode_5xx_and_unexpected_statuses_stay_fenced "
            "--skip slowmode_timeout_stays_fenced "
            "--skip cancelled_lockdown_stays_fenced_with_seed_retained "
            "--skip audit_log_pagination_limit_1_to_100_before_after_covers_45d_window "
            "--skip audit_log_requests_ride_rate_limit_client_never_direct_rest "
            "--skip audit_cursor_persists_per_window_and_resumes_mid_window "
            "--skip audit_rows_hold_ids_timestamps_enums_only "
            "--skip audit_429_retry_after_pauses_without_losing_place "
            "--skip bans_fetch_applies_blocklist_and_counts_everyone "
            "--skip invite_rows_hold_metadata_only "
            "--skip onboarding_rows_hold_ids_enums_metadata_only "
            "--skip bans_invites_onboarding_resume_continues_from_persisted_cursors "
            "--skip event_users_paginate_through_rate_limit_client_with_max_100 "
            "--skip reaction_users_paginate_through_rate_limit_client "
            "--skip pins_list_through_rate_limit_client_persists_metadata_only "
            "--skip blocklisted_users_skipped_for_per_user_rows_while_aggregates_keep_counting "
            "--skip persisted_rows_hold_ids_timestamps_enums_only "
            "--skip resume_continues_from_persisted_cursors_per_family"
        ),
        "crates": list(ALL_CRATES),
    },
    {
        "id": "core-stream_live_store",
        "command": (
            "cargo llvm-cov --no-report -p two-bot-core --features db "
            "--locked --test stream_live_store -- --ignored "
            "--test-threads=1 --skip regenerate_reference_docs"
        ),
        "crates": ["two-bot-core"],
    },
    {
        "id": "discord-mutation_429_proves_no_effect_channel",
        "command": (
            "cargo llvm-cov --no-report -p two-bot-discord --features db "
            "--locked --test mutation_429_proves_no_effect_channel -- "
            "--include-ignored --test-threads=1 --skip regenerate_reference_docs"
        ),
        "crates": ["two-bot-discord"],
    },
    {
        "id": "core-channel_moderation",
        "command": (
            "cargo llvm-cov --no-report -p two-bot-core --features db "
            "--locked -- channel_moderation --include-ignored "
            "--test-threads=1 --skip regenerate_reference_docs"
        ),
        "crates": ["two-bot-core"],
    },
    {
        "id": "discord-channel_moderation_runtime",
        "command": (
            "env -u PGPASSWORD -u PGPASSFILE -u PGOPTIONS cargo llvm-cov "
            "--no-report -p two-bot-discord --features db --locked "
            "--test channel_moderation_runtime -- --include-ignored "
            "--test-threads=1 --skip regenerate_reference_docs"
        ),
        "crates": ["two-bot-discord"],
    },
    {
        "id": "discord-internal_channel_moderation",
        "command": (
            "cargo llvm-cov --no-report -p two-bot-discord --features db "
            "--locked --test internal_channel_moderation -- "
            "--include-ignored --test-threads=1 --skip regenerate_reference_docs"
        ),
        "crates": ["two-bot-discord"],
    },
    {
        "id": "core-feeds_store",
        "command": (
            "cargo llvm-cov --no-report -p two-bot-core --features db "
            "--test feeds_store --locked -- --ignored --test-threads=1 "
            "--skip regenerate_reference_docs"
        ),
        "crates": ["two-bot-core"],
    },
    {
        "id": "cutover-legacy_verify_db",
        "command": (
            "cargo llvm-cov --no-report -p two-bot-cutover "
            "--test legacy_verify_db --locked -- --ignored "
            "--test-threads=1 --skip regenerate_reference_docs"
        ),
        "crates": ["two-bot-cutover"],
    },
    {
        "id": "cutover-legacy_tls_fence",
        "command": (
            "cargo llvm-cov --no-report -p two-bot-cutover "
            "--test legacy_tls_fence --locked -- --ignored "
            "--test-threads=1 --skip regenerate_reference_docs"
        ),
        "crates": ["two-bot-cutover"],
    },
    {
        "id": "cutover-staging_migrate_db",
        "command": (
            "cargo llvm-cov --no-report -p two-bot-cutover "
            "--test staging_migrate_db --locked -- --ignored "
            "--test-threads=1 --skip regenerate_reference_docs"
        ),
        "crates": ["two-bot-cutover"],
    },
    {
        "id": "discord-internal_events",
        "command": (
            "cargo llvm-cov --no-report -p two-bot-discord --features db "
            "--locked --test internal_events -- --include-ignored "
            "--test-threads=1 --skip regenerate_reference_docs"
        ),
        "crates": ["two-bot-discord"],
    },
    {
        "id": "cutover-raid_list_db",
        "command": (
            "cargo llvm-cov --no-report -p two-bot-cutover "
            "--test raid_list_db --locked -- --ignored --test-threads=1 "
            "--skip regenerate_reference_docs"
        ),
        "crates": ["two-bot-cutover"],
    },
    {
        "id": "cutover-reengagement_list_db",
        "command": (
            "cargo llvm-cov --no-report -p two-bot-cutover "
            "--test reengagement_list_db --locked -- --ignored "
            "--test-threads=1 --skip regenerate_reference_docs"
        ),
        "crates": ["two-bot-cutover"],
    },
    {
        "id": "cutover-game_activity_report_db",
        "command": (
            "cargo llvm-cov --no-report -p two-bot-cutover "
            "--test game_activity_report_db --locked -- --test-threads=1 "
            "--skip regenerate_reference_docs"
        ),
        "crates": ["two-bot-cutover"],
    },
    {
        "id": "core-member_moderation_db",
        "command": (
            "cargo llvm-cov --no-report -p two-bot-core --features db "
            "--locked --test member_moderation_db -- --ignored "
            "--test-threads=1 --skip regenerate_reference_docs"
        ),
        "crates": ["two-bot-core"],
    },
    {
        "id": "store-invite_uses_saturation_acceptance",
        "command": (
            "cargo llvm-cov --no-report -p two-bot-store "
            "--features two-bot-core/db --locked "
            "--test invite_uses_saturation_acceptance -- --test-threads=1 "
            "--skip regenerate_reference_docs"
        ),
        "crates": ["two-bot-store"],
    },
]

# Package partitions keep Cargo's automatic target discovery: a newly added
# integration target is covered without adding its name here. The remainder
# owns root/testsupport and any future workspace member, so widening cannot
# accidentally omit a package that is not yet in the path classifier.
SWEEP_PACKAGES = {
    "core": "two-bot-core",
    "discord": "two-bot-discord",
    "bot": "two-bot",
    "cutover": "two-bot-cutover",
    "store": "two-bot-store",
    "other": None,
}


def _expand_sweeps(legs):
    expanded = []
    for leg in legs:
        if leg["id"] not in ("sweep-nonignored", "sweep-ignored"):
            expanded.append(leg)
            continue
        for key, package in SWEEP_PACKAGES.items():
            selector = (f"-p {package}" if package else "--workspace" + "".join(
                f" --exclude {name}" for name in SWEEP_PACKAGES.values()
                if name is not None
            ))
            part = dict(leg, id=f"{leg['id']}-{key}",
                        command=leg["command"].replace("--workspace", selector, 1),
                        crates=[package] if package else ["two-bot-next", "two-bot-testsupport"])
            # Environment-mutating ops acceptance belongs only to the bot's
            # nonignored sweep, never every package or every matrix runner.
            part.pop("supplemental_commands", None)
            if package == "two-bot" and "supplemental_commands" in leg:
                part["supplemental_commands"] = leg["supplemental_commands"]
            if leg["id"] == "sweep-ignored" and package == "two-bot":
                # Keep package-wide target discovery and substring skip semantics.
                # Ordered exclusions make nested group names single-owner too.
                for suffix, filters in (
                    ("gateway", "gateway_tests::"),
                    ("self-role", "self_role_runtime:: --skip gateway_tests::"),
                ):
                    expanded.append(dict(part, id=f"{part['id']}-{suffix}",
                                         command=part["command"] + " " + filters,
                                         sweep_package=key))
                part["command"] += " --skip gateway_tests:: --skip self_role_runtime::"
            expanded.append(part)
    return expanded


FULL_LEGS = _expand_sweeps(FULL_LEGS)
ALWAYS_ON_LEGS = ["sweep-nonignored-other"]

_CRATE_DIRS = {
    "core": "two-bot-core",
    "discord": "two-bot-discord",
    "bot": "two-bot",
    "cutover": "two-bot-cutover",
    "store": "two-bot-store",
    "testsupport": "two-bot-testsupport",
}

_PLANNER_FILES = frozenset(
    {
        "scripts/ci/plan-coverage-shards.py",
        "scripts/ci/merge-coverage-shards.py",
    }
)


def classify_path(path):
    """Classify a repo-relative path for shard planning."""
    text = str(path)
    # Widening triggers (full set): workspace-wide or planner-owned.
    if text in ("Cargo.lock", "Cargo.toml"):
        return "widen"
    if text.endswith("/Cargo.toml") or text.endswith("/Cargo.lock"):
        return "widen"
    if text == "rust-toolchain.toml" or text.startswith("rust-toolchain"):
        return "widen"
    if text.endswith("build.rs"):
        return "widen"
    if text.startswith(".cargo/"):
        return "widen"
    if text == ".github/workflows/check.yml":
        return "widen"
    if text in _PLANNER_FILES:
        return "widen"
    if "/migrations/" in text:
        return "widen"
    # Docs: never affect coverage.
    if text.startswith("docs/") or text.endswith(".md"):
        return "docs"
    # Crate paths.
    if crate_for_path(text) is not None:
        return "crate"
    return "unknown"


def crate_for_path(path):
    """Workspace package for a repo-relative path, or None."""
    text = str(path)
    # Widen triggers never map to a crate, even under crates/.
    if text.endswith("build.rs") or "/migrations/" in text:
        return None
    if text.endswith("/Cargo.toml") or text.endswith("/Cargo.lock"):
        return None
    parts = text.split("/")
    if len(parts) >= 2 and parts[0] == "crates":
        return _CRATE_DIRS.get(parts[1])
    if len(parts) >= 2 and parts[0] == "src":
        return "two-bot-next"
    return None


def reverse_deps_from_metadata(metadata):
    """Reverse dependents from ``cargo metadata`` JSON.

    Returns {package: set(direct workspace dependents)}. Raises ValueError
    on malformed metadata. Dev-only edges are included (conservative:
    test deps affect coverage).
    """
    if not isinstance(metadata, dict):
        raise ValueError("cargo metadata must be an object")
    packages = metadata.get("packages")
    resolve = metadata.get("resolve")
    if not isinstance(packages, list) or not isinstance(resolve, dict):
        raise ValueError("cargo metadata missing packages/resolve")
    nodes = resolve.get("nodes")
    if not isinstance(nodes, list) or not nodes:
        raise ValueError("cargo metadata missing resolve.nodes")
    package_names = set()
    id_to_name = {}
    for pkg in packages:
        if not isinstance(pkg, dict) or "name" not in pkg:
            raise ValueError("cargo metadata package missing name")
        name = pkg["name"]
        if not isinstance(name, str) or not name:
            raise ValueError("cargo metadata package name must be a string")
        package_names.add(name)
        pkg_id = pkg.get("id")
        if isinstance(pkg_id, str) and pkg_id:
            id_to_name[pkg_id] = name
    if not package_names:
        raise ValueError("cargo metadata has no packages")
    # Workspace members when available; else all listed packages (the
    # acceptance stub has no workspace_members and lists only workspace).
    workspace_names = set(package_names)
    members = metadata.get("workspace_members")
    if isinstance(members, list) and members:
        workspace_names = set()
        for mid in members:
            name = id_to_name.get(mid)
            if isinstance(name, str):
                workspace_names.add(name)
        if not workspace_names:
            raise ValueError("cargo metadata workspace_members unresolvable")
    reverse = {name: set() for name in workspace_names}
    resolved_workspace = set()
    for node in nodes:
        if not isinstance(node, dict):
            raise ValueError("cargo metadata node must be an object")
        node_id = node.get("id")
        deps = node.get("deps")
        if not isinstance(node_id, str) or not isinstance(deps, list):
            raise ValueError("cargo metadata node missing id/deps")
        node_name = id_to_name.get(node_id)
        if node_name is None:
            # Acceptance stub: id is "<name> <version> (...)".
            first = node_id.split(" ", 1)[0]
            if first in package_names:
                node_name = first
        if node_name is None or node_name not in workspace_names:
            continue
        resolved_workspace.add(node_name)
        for dep in deps:
            if not isinstance(dep, dict):
                raise ValueError("cargo metadata dep must be an object")
            dep_name = None
            pkg_ref = dep.get("pkg")
            if isinstance(pkg_ref, str):
                dep_name = id_to_name.get(pkg_ref)
            if dep_name is None:
                candidate = dep.get("name")
                if isinstance(candidate, str) and candidate in package_names:
                    dep_name = candidate
            if dep_name is None or dep_name not in workspace_names:
                continue
            reverse[dep_name].add(node_name)
    if resolved_workspace != workspace_names:
        raise ValueError("cargo metadata missing workspace dependency nodes")
    return reverse


def sweep_feature_flags(key, metadata):
    """Resolve the full-workspace feature closure for a sweep's package set."""
    reverse = reverse_deps_from_metadata(metadata)
    package = SWEEP_PACKAGES[key]
    selected = [package] if package else sorted(set(reverse) - set(SWEEP_PACKAGES.values()))
    return workspace_feature_args(metadata, selected)


def resolve_sweep_leg(leg, metadata):
    """Turn a sweep template into the command actually run for this graph."""
    if not leg["id"].startswith(("sweep-nonignored-", "sweep-ignored-")):
        return dict(leg)
    key = leg.get("sweep_package", leg["id"].rsplit("-", 1)[1])
    tokens = shlex.split(leg["command"])
    at = tokens.index("--features")
    tokens[at:at + 2] = sweep_feature_flags(key, metadata)
    package = SWEEP_PACKAGES[key]
    crates = [package] if package else sorted(
        set(reverse_deps_from_metadata(metadata)) - set(SWEEP_PACKAGES.values())
    )
    return dict(leg, command=shlex.join(tokens), crates=crates)


def _full_plan(shard_count, reason):
    legs = [leg["id"] for leg in FULL_LEGS]
    return {
        "legs": legs,
        "shards": _partition(legs, shard_count),
        "widened": True,
        "reason": reason,
    }


def validate_plan(value):
    """Reject partial or duplicate ownership before publishing a CI matrix."""
    if not isinstance(value, dict):
        raise ValueError("coverage plan must be an object")
    legs, shards = value.get("legs"), value.get("shards")
    known = {leg["id"] for leg in FULL_LEGS}
    if (not isinstance(legs, list) or not legs
            or any(not isinstance(leg, str) or leg not in known for leg in legs)
            or len(set(legs)) != len(legs)):
        raise ValueError("coverage plan has empty, duplicate or unknown legs")
    if not isinstance(shards, list) or not shards:
        raise ValueError("coverage plan has no shards")
    if any(not isinstance(shard, list) for shard in shards):
        raise ValueError("coverage plan shard must be a list")
    flat = [leg for shard in shards for leg in shard]
    if (any(not isinstance(leg, str) or leg not in known for leg in flat)
            or len(flat) != len(set(flat)) or set(flat) != set(legs)):
        raise ValueError("coverage plan does not assign every selected leg exactly once")


def _partition(legs, shard_count):
    try:
        count = int(shard_count)
    except (TypeError, ValueError):
        count = 4
    if count < 1:
        count = 1
    shards = [[] for _ in range(count)]
    for idx, leg_id in enumerate(legs):
        shards[idx % count].append(leg_id)
    return shards


def plan(changed_files, shard_count, *, event_name, ref, metadata):
    """Plan coverage legs/shards. Never raises; errors widen to full set."""
    try:
        return _plan_inner(
            changed_files,
            shard_count,
            event_name=event_name,
            ref=ref,
            metadata=metadata,
        )
    except Exception as exc:  # fail closed
        return _full_plan(shard_count, f"planner error fails closed: {exc}")


def _plan_inner(changed_files, shard_count, *, event_name, ref, metadata):
    # Scheduled runs and pushes to main always run the full set.
    if event_name == "schedule":
        return _full_plan(shard_count, "schedule always runs the full set")
    if event_name == "push" and ref == "refs/heads/main":
        return _full_plan(shard_count, "push to main always runs the full set")
    if not isinstance(changed_files, list):
        return _full_plan(shard_count, "changed files unreadable; failing closed")
    ordered = sorted(str(p) for p in changed_files)
    wideners = [p for p in ordered if classify_path(p) == "widen"]
    if wideners:
        return _full_plan(
            shard_count,
            f"workspace-wide change fails closed: {wideners[0]}",
        )
    unknowns = [p for p in ordered if classify_path(p) == "unknown"]
    if unknowns:
        return _full_plan(
            shard_count, f"unknown path fails closed: {unknowns[0]}"
        )
    changed_crates = set()
    for path in ordered:
        if classify_path(path) == "crate":
            crate = crate_for_path(path)
            if crate is not None:
                changed_crates.add(crate)
    if not changed_crates:
        legs = list(ALWAYS_ON_LEGS)
        return {
            "legs": legs,
            "shards": _partition(legs, shard_count),
            "widened": False,
            "reason": "no Rust crate changes; running always-on legs",
        }
    try:
        reverse = reverse_deps_from_metadata(metadata)
    except Exception as exc:
        return _full_plan(
            shard_count, f"cargo metadata unreadable fails closed: {exc}"
        )
    # Changed crates plus transitive reverse dependents.
    affected = set(changed_crates)
    queue = list(changed_crates)
    while queue:
        current = queue.pop()
        for dependent in reverse.get(current, ()):
            if dependent not in affected:
                affected.add(dependent)
                queue.append(dependent)
    selected = []
    for leg in FULL_LEGS:
        crates = leg["crates"]
        if leg["id"].startswith(("sweep-nonignored-", "sweep-ignored-")) and leg["id"].endswith("-other"):
            # New workspace members live in the remainder, including their
            # ignored tests when a changed dependency reaches them.
            crates = set(reverse) - set(SWEEP_PACKAGES.values())
        if leg["id"] in ALWAYS_ON_LEGS:
            selected.append(leg["id"])
        elif any(c in affected for c in crates):
            selected.append(leg["id"])
    return {
        "legs": selected,
        "shards": _partition(selected, shard_count),
        "widened": False,
        "reason": (
            f"changed crates {sorted(changed_crates)} affect "
            f"{sorted(affected)}; {len(selected)} legs"
        ),
    }


def _changed_from_diff(base_ref, head_ref):
    proc = subprocess.run(
        ["git", "-C", str(ROOT), "diff", "--name-only", base_ref, head_ref],
        capture_output=True,
        text=True,
        timeout=120,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"git diff exited {proc.returncode}")
    return [line.strip() for line in proc.stdout.splitlines() if line.strip()]


def _metadata_from_cargo():
    try:
        proc = subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--locked", "--features", "two-bot-core/db"],
            cwd=str(ROOT),
            capture_output=True,
            text=True,
            timeout=180,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if proc.returncode != 0:
        return None
    try:
        return json.loads(proc.stdout)
    except ValueError:
        return None


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base-ref", default="origin/main")
    parser.add_argument("--head-ref", default="HEAD")
    parser.add_argument("--shards", type=int, default=4)
    parser.add_argument("--event", default="pull_request")
    parser.add_argument("--ref", default="refs/pull/1/merge")
    parser.add_argument("--output")
    parser.add_argument("--sweep-features", choices=SWEEP_PACKAGES)
    parser.add_argument("--describe-sweeps", action="store_true")
    args = parser.parse_args(argv)
    if args.describe_sweeps:
        metadata = _metadata_from_cargo()
        try:
            legs = [resolve_sweep_leg(leg, metadata) for leg in FULL_LEGS]
        except (ValueError, TypeError, KeyError):
            print("::error::could not resolve coverage command descriptors", file=sys.stderr)
            return 1
        print(json.dumps(legs, indent=2))
        return 0
    if args.sweep_features:
        try:
            flags = sweep_feature_flags(args.sweep_features, _metadata_from_cargo())
        except (ValueError, TypeError, KeyError):
            print("::error::could not resolve coverage feature graph", file=sys.stderr)
            return 1
        print(" ".join(flags))
        return 0
    try:
        changed = _changed_from_diff(args.base_ref, args.head_ref)
    except Exception:
        result = _full_plan(args.shards, "unreadable diff fails closed to full set")
        return _write_output(args.output, result)
    metadata = _metadata_from_cargo()
    result = plan(
        changed,
        args.shards,
        event_name=args.event,
        ref=args.ref,
        metadata=metadata,
    )
    return _write_output(args.output, result)


def _write_output(output, result):
    text = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if not output:
        sys.stdout.write(text)
        return 0
    try:
        Path(output).write_text(text)
    except OSError as exc:
        print(f"::error::could not write shard plan: {exc}", flush=True)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
