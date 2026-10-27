"""Guard Rust toolchain/image parity: one pin, every consumer.

`rust-toolchain.toml` is the single source of truth (currently 1.99.1).
Every `dtolnay/rust-toolchain` step in `check.yml`, `nightly.yml`,
`pipeline-benchmark.yml` and `reusable-rust-nextest.yml` must request that
exact channel (the fuzz-compile job alone uses pinned nightly), every covered
Dockerfile (`Dockerfile` and `crates/voice-probe/Dockerfile`) must build
`FROM rust:<major>.<minor>-trixie`, and the selected workflow job containers
must use the matching pinned Rust trixie image. Runs offline via the existing
`Source-level CI gate regressions` discover step in check.yml.

Hermetic fixtures: drifted synthetic roots fail, the pinned tree passes.
"""

import re
import tempfile
import tomllib
import unittest
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ("check.yml", "nightly.yml", "pipeline-benchmark.yml", "reusable-rust-nextest.yml")
CONTAINER_WORKFLOWS = {
    "deploy-staging.yml": ("schema-gate", "roles-gate"),
    "staging-migrate.yml": ("plan", "apply"),
    "deploy-production.yml": ("schema-gate", "roles-gate"),
    "production-migrate.yml": ("plan", "apply"),
    "nightly.yml": ("sweep",),
}
DOCKERFILES = ("Dockerfile", "crates/voice-probe/Dockerfile")
# libFuzzer needs nightly; pinned date, not floating stable.
FUZZ_TOOLCHAIN = "nightly-2026-10-01"
MATCH_COMMENT = "Match rust-toolchain.toml"


class UniqueKeyLoader(yaml.BaseLoader):
    def construct_mapping(self, node, deep=False):
        mapping = {}
        for key_node, value_node in node.value:
            key = self.construct_object(key_node, deep=deep)
            if key in mapping:
                raise ValueError(f"duplicate workflow key: {key}")
            mapping[key] = self.construct_object(value_node, deep=deep)
        return mapping


def read_channel(root):
    return tomllib.loads((root / "rust-toolchain.toml").read_text())["toolchain"]["channel"]


def expected_image_tag(channel):
    major, minor, _ = channel.split(".")
    return f"{major}.{minor}-trixie"


def workflow_toolchain_errors(root):
    """Every installer step pins the channel, except fuzz-compile nightly."""
    errors = []
    try:
        channel = read_channel(root)
    except Exception as exc:
        return [f"rust-toolchain.toml unreadable: {exc}"]
    if not re.fullmatch(r"\d+\.\d+\.\d+", channel):
        return [f"rust-toolchain.toml channel {channel!r} is not an exact x.y.z pin"]
    for name in WORKFLOWS:
        path = root / ".github/workflows" / name
        try:
            workflow = yaml.load(path.read_text(), Loader=UniqueKeyLoader)
        except Exception as exc:
            errors.append(f"{name} unreadable: {exc}")
            continue
        for job_id, job in (workflow.get("jobs") or {}).items():
            for step in job.get("steps", []) or []:
                if not str(step.get("uses", "")).startswith("dtolnay/rust-toolchain@"):
                    continue
                pinned = (step.get("with") or {}).get("toolchain")
                if job_id == "fuzz-compile":
                    if pinned != FUZZ_TOOLCHAIN:
                        errors.append(f"{name}#{job_id}: expected {FUZZ_TOOLCHAIN}, got {pinned!r}")
                    continue
                if pinned != channel:
                    errors.append(f"{name}#{job_id}: expected {channel!r}, got {pinned!r}")
    return errors


def _rust_stages(text):
    """Every `FROM [flags] rust:<tag>[@sha256:<digest>]` stage, in file order.

    Tag and digest are read from the SAME line in one pass, so a later rust
    stage can never supply the digest for an earlier stage's tag (or vice
    versa) once this file grows a second rust stage. Optional `FROM` flags
    such as `--platform=$BUILDPLATFORM` are skipped so flagged rust stages
    stay covered instead of silently invisible to the tag and digest checks.
    """
    return re.findall(r"(?m)^FROM\s+(?:--\S+\s+)*rust:([^\s@]+)(?:@sha256:([0-9a-f]{64}))?", text)


def dockerfile_parity_errors(root):
    """Every rust stage in each covered Dockerfile builds FROM rust:<major>.<minor>-trixie."""
    errors = []
    try:
        channel = read_channel(root)
    except Exception as exc:
        return [f"rust-toolchain.toml unreadable: {exc}"]
    if not re.fullmatch(r"\d+\.\d+\.\d+", channel):
        return [f"rust-toolchain.toml channel {channel!r} is not an exact x.y.z pin"]
    expected = expected_image_tag(channel)
    for name in DOCKERFILES:
        path = root / name
        try:
            text = path.read_text()
        except Exception as exc:
            errors.append(f"{name} unreadable: {exc}")
            continue
        stages = _rust_stages(text)
        if not stages:
            errors.append(f"{name}: no FROM rust:<tag> builder found")
        for index, (tag, _digest) in enumerate(stages):
            if tag != expected:
                errors.append(f"{name}: stage {index} expected rust:{expected}, got rust:{tag}")
    return errors


def container_image_errors(root):
    """Selected job containers use the pinned Rust trixie image tag and digest.

    The tag must match rust-toolchain.toml and the digest must match the
    shared builder digest of the covered Dockerfiles, so a digest-only roll
    on either side fails.
    """
    errors = []
    try:
        expected = expected_image_tag(read_channel(root))
    except Exception as exc:
        return [f"rust-toolchain.toml unreadable: {exc}"]
    builder_digests = {}
    for name in DOCKERFILES:
        try:
            stages = _rust_stages((root / name).read_text())
        except Exception:
            stages = []
        if not stages or not stages[0][1]:
            errors.append(f"{name}: no pinned rust builder digest found")
            continue
        stage_digests = {digest for _, digest in stages if digest}
        if len(stage_digests) > 1:
            detail = ", ".join(sorted(f"sha256:{d[:12]}" for d in stage_digests))
            errors.append(f"{name}: rust stages disagree on builder digest: {detail}")
            continue
        builder_digests[name] = stages[0][1]
    if len(set(builder_digests.values())) > 1:
        detail = ", ".join(f"{n}@sha256:{d[:12]}" for n, d in sorted(builder_digests.items()))
        errors.append(f"builder digests disagree: {detail}")
        return errors
    expected_digest = next(iter(builder_digests.values()), None)
    for name, job_ids in CONTAINER_WORKFLOWS.items():
        path = root / ".github/workflows" / name
        try:
            workflow = yaml.load(path.read_text(), Loader=UniqueKeyLoader)
        except Exception as exc:
            errors.append(f"{name} unreadable: {exc}")
            continue
        jobs = workflow.get("jobs") or {}
        for job_id in job_ids:
            container = (jobs.get(job_id) or {}).get("container") or {}
            image = container.get("image") if isinstance(container, dict) else container
            match = re.fullmatch(r"rust:([^@\s]+)@sha256:([0-9a-f]{64})", str(image or ""))
            if not match:
                errors.append(f"{name}#{job_id}: expected pinned rust:{expected}@sha256 image, got {image!r}")
            elif match.group(1) != expected:
                errors.append(f"{name}#{job_id}: expected rust:{expected}, got rust:{match.group(1)}")
            elif expected_digest is not None and match.group(2) != expected_digest:
                errors.append(
                    f"{name}#{job_id}: container digest sha256:{match.group(2)[:12]} "
                    f"does not match builder digest sha256:{expected_digest[:12]}"
                )
    return errors


def toolchain_parity_errors(root):
    return workflow_toolchain_errors(root) + dockerfile_parity_errors(root) + container_image_errors(root)


PINNED_WORKFLOW = """\
name: fixture
on:
  pull_request:
jobs:
  job:
    runs-on: ubuntu-latest
    steps:
      - uses: dtolnay/rust-toolchain@7e38f4b43b4db5c8dd498af069a4f6196df1d067 # master
        with:
          toolchain: "{toolchain}" # Match rust-toolchain.toml; floating stable can drift.
"""

PINNED_DIGEST = "a" * 64
PINNED_DOCKERFILE = "FROM rust:{tag}@sha256:{digest} AS builder\n"
PINNED_CONTAINER_IMAGE = "rust:{tag}@sha256:{digest}"


def make_fixture_root(parent, channel="1.99.1", workflow_toolchain="1.99.1", docker_tag="1.99-trixie", container_tag="1.99-trixie", docker_digest=PINNED_DIGEST, container_digest=PINNED_DIGEST):
    root = Path(parent) / "root"
    (root / ".github/workflows").mkdir(parents=True)
    (root / "rust-toolchain.toml").write_text(
        f'[toolchain]\nchannel = "{channel}"\ncomponents = ["rustfmt", "clippy"]\nprofile = "minimal"\n'
    )
    for name in WORKFLOWS:
        (root / ".github/workflows" / name).write_text(
            PINNED_WORKFLOW.format(toolchain=workflow_toolchain)
        )
    for name, job_ids in CONTAINER_WORKFLOWS.items():
        path = root / ".github/workflows" / name
        if not path.exists():
            path.write_text("name: fixture\njobs:\n")
        job_text = "\n".join(
            f"  {job_id}:\n    container:\n      image: {PINNED_CONTAINER_IMAGE.format(tag=container_tag, digest=container_digest)}"
            for job_id in job_ids
        )
        with path.open("a") as fixture:
            fixture.write(job_text + "\n")
    for name in DOCKERFILES:
        target = root / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(PINNED_DOCKERFILE.format(tag=docker_tag, digest=docker_digest))
    return root


class ToolchainParityTests(unittest.TestCase):
    def test_pinned_tree_passes(self):
        self.assertEqual(toolchain_parity_errors(ROOT), [])

    def test_all_rust_builder_dockerfiles_are_covered(self):
        self.assertEqual(DOCKERFILES, ("Dockerfile", "crates/voice-probe/Dockerfile"))
        for name in DOCKERFILES:
            self.assertTrue((ROOT / name).is_file(), name)

    def test_pinned_fixture_passes(self):
        with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
            root = make_fixture_root(scratch)
            self.assertEqual(toolchain_parity_errors(root), [])

    def test_floating_stable_workflow_fails(self):
        with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
            root = make_fixture_root(scratch, workflow_toolchain="stable")
            errors = workflow_toolchain_errors(root)
            self.assertTrue(errors, "floating stable must fail")
            self.assertTrue(all("stable" in error for error in errors))

    def test_wrong_version_workflow_fails(self):
        with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
            root = make_fixture_root(scratch, workflow_toolchain="1.100.0")
            errors = workflow_toolchain_errors(root)
            self.assertTrue(errors, "1.100.0 against a 1.99.1 pin must fail")

    def test_each_pinned_workflow_drift_fails_independently(self):
        # Independently enumerated: dropping a WORKFLOWS entry must fail here
        # instead of silently shrinking the fixture inventory and the drift
        # loop together.
        expected = ("check.yml", "nightly.yml", "pipeline-benchmark.yml", "reusable-rust-nextest.yml")
        self.assertEqual(WORKFLOWS, expected)
        for drifted in expected:
            with self.subTest(workflow=drifted):
                with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
                    root = make_fixture_root(scratch)
                    (root / ".github/workflows" / drifted).write_text(
                        PINNED_WORKFLOW.format(toolchain="1.100.0")
                    )
                    errors = workflow_toolchain_errors(root)
                    self.assertEqual(len(errors), 1, errors)
                    self.assertIn(drifted, errors[0])

    def test_drifted_dockerfile_fails(self):
        with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
            root = make_fixture_root(scratch, docker_tag="1.100-trixie")
            errors = dockerfile_parity_errors(root)
            self.assertEqual(len(errors), len(DOCKERFILES))
            self.assertTrue(all("1.100-trixie" in error for error in errors))

    def test_drifted_container_image_fails(self):
        with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
            root = make_fixture_root(scratch, container_tag="1.99-bookworm")
            errors = container_image_errors(root)
            self.assertEqual(len(errors), sum(map(len, CONTAINER_WORKFLOWS.values())))
            self.assertTrue(all("1.99-bookworm" in error for error in errors))
            wired = toolchain_parity_errors(root)
            self.assertTrue(wired, "container drift must fail through the CI entry point")
            self.assertTrue(any("1.99-bookworm" in error for error in wired))

    def test_digest_only_drifted_container_image_fails(self):
        with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
            root = make_fixture_root(scratch, container_digest="b" * 64)
            errors = container_image_errors(root)
            self.assertEqual(len(errors), sum(map(len, CONTAINER_WORKFLOWS.values())))
            self.assertTrue(all("digest" in error for error in errors))
            wired = toolchain_parity_errors(root)
            self.assertTrue(wired, "digest-only drift must fail through the CI entry point")
            self.assertTrue(any("digest" in error for error in wired))

    def test_digest_only_drifted_builder_fails(self):
        with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
            root = make_fixture_root(scratch, docker_digest="b" * 64)
            errors = container_image_errors(root)
            self.assertEqual(len(errors), sum(map(len, CONTAINER_WORKFLOWS.values())))
            self.assertTrue(all("digest" in error for error in errors))

    def test_multistage_second_rust_tag_drift_fails(self):
        with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
            root = make_fixture_root(scratch)
            (root / "Dockerfile").write_text(
                PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=PINNED_DIGEST)
                + f"FROM rust:1.100-trixie@sha256:{PINNED_DIGEST} AS extra\n"
            )
            errors = dockerfile_parity_errors(root)
            self.assertEqual(len(errors), 1, errors)
            self.assertIn("1.100-trixie", errors[0])
            wired = toolchain_parity_errors(root)
            self.assertTrue(wired, "second-stage tag drift must fail through the CI entry point")
            self.assertTrue(any("1.100-trixie" in error for error in wired))

    def test_multistage_second_rust_digest_drift_fails(self):
        with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
            root = make_fixture_root(scratch)
            (root / "Dockerfile").write_text(
                PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=PINNED_DIGEST)
                + f"FROM rust:1.99-trixie@sha256:{'b' * 64} AS extra\n"
            )
            errors = container_image_errors(root)
            self.assertEqual(len(errors), 1, errors)
            self.assertIn("disagree", errors[0])
            wired = toolchain_parity_errors(root)
            self.assertTrue(wired, "second-stage digest drift must fail through the CI entry point")

    def test_multistage_later_digest_does_not_satisfy_builder(self):
        with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
            root = make_fixture_root(scratch)
            (root / "Dockerfile").write_text(
                "FROM rust:1.99-trixie AS builder\n"
                + f"FROM rust:1.99-trixie@sha256:{PINNED_DIGEST} AS extra\n"
            )
            errors = container_image_errors(root)
            self.assertEqual(len(errors), 1, errors)
            self.assertIn("no pinned rust builder digest", errors[0])

    def test_platform_flag_rust_stage_is_checked(self):
        with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
            root = make_fixture_root(scratch)
            (root / "Dockerfile").write_text(
                f"FROM --platform=$BUILDPLATFORM rust:1.99-trixie@sha256:{PINNED_DIGEST} AS builder\n"
            )
            self.assertEqual(toolchain_parity_errors(root), [])
            (root / "Dockerfile").write_text(
                f"FROM --platform=$BUILDPLATFORM rust:1.100-trixie@sha256:{PINNED_DIGEST} AS builder\n"
            )
            errors = dockerfile_parity_errors(root)
            self.assertEqual(len(errors), 1, errors)
            self.assertIn("1.100-trixie", errors[0])
            wired = toolchain_parity_errors(root)
            self.assertTrue(wired, "flagged-stage tag drift must fail through the CI entry point")
            self.assertTrue(any("1.100-trixie" in error for error in wired))

    def test_each_container_image_is_covered(self):
        for name, job_ids in CONTAINER_WORKFLOWS.items():
            with self.subTest(workflow=name):
                self.assertTrue(job_ids, name)
                self.assertTrue((ROOT / ".github/workflows" / name).is_file(), name)

    def test_each_dockerfile_drift_fails_independently(self):
        for drifted in DOCKERFILES:
            with self.subTest(drifted=drifted):
                with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
                    root = make_fixture_root(scratch)
                    (root / drifted).write_text(PINNED_DOCKERFILE.format(tag="1.100-trixie", digest=PINNED_DIGEST))
                    errors = dockerfile_parity_errors(root)
                    self.assertEqual(len(errors), 1, errors)
                    self.assertIn(drifted, errors[0])

    def test_probe_builder_revert_fails_independently(self):
        with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
            root = make_fixture_root(scratch)
            (root / "crates/voice-probe/Dockerfile").write_text(
                "FROM rust:1.98-trixie@sha256:a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546 AS builder\n"
            )
            errors = dockerfile_parity_errors(root)
            self.assertEqual(len(errors), 1, errors)
            self.assertIn("crates/voice-probe/Dockerfile", errors[0])
            wired = toolchain_parity_errors(root)
            self.assertTrue(wired, "probe builder revert must fail through the CI entry point")
            self.assertTrue(any("voice-probe" in error for error in wired))

    def test_loose_channel_fails(self):
        with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
            root = make_fixture_root(scratch, channel="1.99")
            self.assertTrue(toolchain_parity_errors(root), "a looser channel must fail")

    def test_pinned_workflows_carry_the_match_comment(self):
        channel = read_channel(ROOT)
        for name in WORKFLOWS:
            with self.subTest(workflow=name):
                text = (ROOT / ".github/workflows" / name).read_text()
                pinned = [line for line in text.splitlines() if f'toolchain: "{channel}"' in line]
                self.assertTrue(pinned, f"{name} has no pinned toolchain line")
                self.assertTrue(all(MATCH_COMMENT in line for line in pinned), f"{name} lost the match comment")
        self.assertNotIn("toolchain: stable", "\n".join(
            (ROOT / ".github/workflows" / name).read_text() for name in WORKFLOWS))


if __name__ == "__main__":
    unittest.main()
