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


# --- Bounded Dockerfile instruction walker -----------------------------------
#
# This guard covers exactly two files (`DOCKERFILES`), so it deliberately
# accepts a documented subset of the Docker language and fails closed on
# everything else. Silent omission is the one outcome that is never allowed:
# a stage the walker cannot classify is an error, not an unchecked stage.
#
# Accepted: ordinary instructions, `#` comments, blank lines, single physical
# FROM lines, and the default backslash line continuation used by RUN/LABEL/
# HEALTHCHECK. Refused explicitly: escape directives (active directive region
# only; a late `escape=` comment is an ordinary comment), a BOM, vertical
# tab/form feed/NUL separators, heredocs, continued FROM instructions and
# non-`docker.io`/`library`/`rust` image references.

_DEFAULT_ESCAPE = "\\"
_DIRECTIVE = re.compile(r"(?im)^[ \t]*#[ \t]*([A-Za-z][A-Za-z0-9_.-]*)[ \t]*=[ \t]*(.*?)[ \t]*\r?$")
_UNSUPPORTED_SEPARATORS = ("\x00", "\x0b", "\x0c")
_HEREDOC = re.compile(r"<<-?[ \t]*(['\"]?)[A-Za-z0-9_.-]+\1")
_RUST_REFERENCE = re.compile(r"rust(?::[^@\s]+)?(?:@[^\s]*)?")
_STAGE_FLAG = re.compile(r"--\S+")


def _refuse(detail):
    return ValueError(f"unsupported Dockerfile syntax; {detail}")


def _logical_instructions(text):
    """`(instruction, continued, malformed)` for every Dockerfile instruction.

    Continuations are resolved with Docker's own rule: a logical line
    continues only when its physical line ends in an ODD number of escape
    characters. An even run carries an escaped backslash, so the instruction
    ends there and a following FROM is a real stage instead of data joined
    onto an earlier RUN. A continued logical line that reaches EOF without a
    final instruction is reported as malformed rather than silently dropped.
    """
    pending = ""
    continued = False
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or line.lstrip().startswith("#"):
            continue
        trailing = len(line) - len(line.rstrip(_DEFAULT_ESCAPE))
        if trailing % 2:
            pending += line[: len(line) - 1] + " "
            continued = True
            continue
        yield pending + stripped, continued, False
        pending = ""
        continued = False
    if pending.strip():
        yield pending, continued, True


def _active_directives(text):
    """Parser directives from Docker's initial directive region only.

    Docker stops reading directives at the first instruction or any comment
    that is not a `key = value` parser directive, so `# escape=` after a FROM
    is an ordinary comment and must not change how the file parses.
    """
    directives = []
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        if not stripped.startswith("#"):
            break
        match = _DIRECTIVE.fullmatch(line)
        if not match:
            break
        directives.append((match.group(1).lower(), match.group(2)))
    return directives


def _normalize_reference(reference):
    """Strip the registry/image-name prefixes the two covered files never need."""
    body = reference
    for host in ("docker.io/library/", "index.docker.io/library/", "library/"):
        if body.startswith(host):
            return body[len(host):]
    return body


def _stage_reference(reference, original):
    """Normalize an accepted image reference; refuse anything outside the subset."""
    body = _normalize_reference(reference)
    if not _RUST_REFERENCE.fullmatch(body):
        raise _refuse(f"unnormalizable image reference {original!r}; name it as rust:<tag>[:@sha256:<digest>]")
    name, _, digest = body.partition("@")
    tag = name.partition(":")[2]
    pinned = re.fullmatch(r"sha256:([0-9a-f]{64})", digest)
    return tag, (pinned.group(1) if pinned else None)


def _rust_stages(text):
    """Every `FROM [flags] rust:<tag>[@sha256:<digest>]` stage, in file order.

    Tag and digest are read from the SAME instruction, so a later rust stage
    can never supply the digest for an earlier stage's tag (or vice versa)
    once this file grows a second rust stage. Optional `FROM` flags such as
    `--platform=$BUILDPLATFORM` are skipped so flagged rust stages stay
    covered instead of silently invisible to the tag and digest checks.
    """
    if text.startswith("\ufeff"):
        raise _refuse("byte order mark before the first instruction; save the file as plain UTF-8")
    if "\r" in text.replace("\r\n", ""):
        raise _refuse("bare carriage return in the source; use LF or CRLF line endings")
    for character in _UNSUPPORTED_SEPARATORS:
        if character in text:
            raise _refuse(f"vertical tab, form feed or NUL separator {character!r} before an instruction")
    for key, value in _active_directives(text):
        if key == "escape":
            if value != _DEFAULT_ESCAPE:
                raise _refuse("unsupported Docker escape directive; use the default backslash escape")
            continue
        raise _refuse(f"unsupported Docker parser directive #{key}={value}; keep the default grammar")
    stages = []
    for instruction, continued, malformed in _logical_instructions(text):
        if _HEREDOC.search(instruction):
            raise _refuse("heredoc instruction content is not covered by this guard")
        keyword = instruction.split(None, 1)
        # Docker joins continued lines without a separator, so `FR\` + `OM`
        # is a real FROM even though the joined keyword here reads `FR OM`.
        if continued and "".join(instruction.split()).upper().startswith("FROM"):
            raise _refuse(
                "unsupported Docker FROM continuation; keep each FROM on one "
                "physical line (escape= and multi-row images are not supported)"
            )
        if not keyword or keyword[0].upper() != "FROM":
            if malformed:
                raise _refuse("line continuation runs past the end of the file")
            continue
        if continued or malformed:
            raise _refuse(
                "unsupported Docker FROM continuation; keep each FROM on one "
                "physical line (escape= and multi-row images are not supported)"
            )
        tokens = keyword[1].split() if len(keyword) > 1 else []
        while tokens and _STAGE_FLAG.fullmatch(tokens[0]):
            tokens.pop(0)
        if not tokens:
            raise _refuse("FROM instruction without an image reference")
        original = tokens[0]
        normalized = _normalize_reference(original)
        if "$" in normalized:
            raise _refuse(
                f"variable-backed image reference {original!r}; write the Rust image "
                "literally so its pin is visible to this guard"
            )
        if not re.fullmatch(r"rust(?::.*)?", normalized):
            # Aliased earlier stages (`FROM builder AS again`) carry no pin of
            # their own; only Rust images are this guard's business.
            continue
        stages.append(_stage_reference(normalized, original))
    return stages


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
        try:
            stages = _rust_stages(text)
        except ValueError as exc:
            errors.append(f"{name}: {exc}")
            continue
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
        except ValueError as exc:
            errors.append(f"{name}: {exc}")
            continue
        except Exception:
            stages = []
        if not stages or not stages[0][1]:
            errors.append(f"{name}: no pinned rust builder digest found")
            continue
        for index, (_tag, digest) in enumerate(stages[1:], start=1):
            if digest is None:
                errors.append(f"{name}: stage {index} has no pinned rust builder digest")
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


def continued_from_fixtures(reference):
    # Valid Docker logical instructions only. Comments/blank physical lines
    # do not end a continuation; the escape directive precedes every stage.
    backslash = chr(92)
    return (
        ("plain", "", f"FROM {backslash}\n rust:{reference} AS extra\n"),
        ("mixed-case-platform", "",
         f"  FrOm --platform=$BUILDPLATFORM {backslash}\n rust:{reference} AS extra\n"),
        ("continued-platform", "",
         f"FROM {backslash}\n --platform=linux/amd64 {backslash}\n rust:{reference} AS extra\n"),
        ("crlf", "", f"FROM {backslash}\r\n rust:{reference} AS extra\r\n"),
        ("comment-and-blank", "",
         f"FROM {backslash}\n # continuation comment\n\n rust:{reference} AS extra\n"),
        ("explicit-backslash", f"# escape={backslash}\n",
         f"FROM {backslash}\n rust:{reference} AS extra\n"),
        ("backtick", "# escape=`\n", f"FROM `\n rust:{reference} AS extra\n"),
    )


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
                    path = root / ".github/workflows" / drifted
                    original = path.read_text()
                    scalar = 'toolchain: "1.99.1"'
                    self.assertEqual(original.count(scalar), 1)
                    path.write_text(original.replace(scalar, 'toolchain: "1.100.0"', 1))
                    errors = workflow_toolchain_errors(root)
                    self.assertEqual(len(errors), 1, errors)
                    self.assertIn(drifted, errors[0])
                    self.assertEqual(
                        toolchain_parity_errors(root),
                        [f"{drifted}#job: expected '1.99.1', got '1.100.0'"],
                    )

    def test_extra_workflow_installer_drift_fails_through_aggregate(self):
        with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
            root = make_fixture_root(scratch)
            path = root / ".github/workflows/pipeline-benchmark.yml"
            original = path.read_text()
            self.assertIn('toolchain: "1.99.1" # Match rust-toolchain.toml', original)
            path.write_text(original + (
                '      - uses: dtolnay/rust-toolchain@master\n'
                '        with:\n'
                '          toolchain: "1.98.0"\n'
            ))
            self.assertEqual(
                toolchain_parity_errors(root),
                ["pipeline-benchmark.yml#job: expected '1.99.1', got '1.98.0'"],
            )

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

    def test_probe_later_unpinned_stage_fails_through_aggregate(self):
        for instruction in ("FROM", "from", "FrOm"):
            for flags in ("", "--platform=$BUILDPLATFORM ", "--platform=linux/amd64 "):
                with self.subTest(instruction=instruction, flags=flags):
                    with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
                        root = make_fixture_root(scratch)
                        probe = root / "crates/voice-probe/Dockerfile"
                        probe.write_text(probe.read_text() + f"{instruction} {flags}rust:1.99-trixie AS extra\n")
                        errors = toolchain_parity_errors(root)
                        self.assertTrue(
                            any("crates/voice-probe/Dockerfile" in error and
                                ("digest" in error or "pinned" in error) for error in errors),
                            errors,
                        )

    def test_probe_lowercase_and_flagged_wrong_tag_fails_through_aggregate(self):
        for instruction in ("from", "FrOm", "FROM"):
            for flags in ("", "--platform=$BUILDPLATFORM ", "--platform=linux/amd64 "):
                with self.subTest(instruction=instruction, flags=flags):
                    with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
                        root = make_fixture_root(scratch)
                        probe = root / "crates/voice-probe/Dockerfile"
                        probe.write_text(probe.read_text() +
                            f"{instruction} {flags}rust:1.98-trixie@sha256:{PINNED_DIGEST} AS extra\n")
                        self.assertIn(
                            "crates/voice-probe/Dockerfile: stage 1 expected rust:1.99-trixie, got rust:1.98-trixie",
                            toolchain_parity_errors(root),
                        )

    def test_probe_malformed_digests_fail_through_aggregate(self):
        malformed = ("a" * 65, "a" * 63, "a" * 63 + "g", "a" * 64 + "suffix")
        for digest in malformed:
            for later in (False, True):
                with self.subTest(digest=digest, later=later):
                    with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
                        root = make_fixture_root(scratch)
                        probe = root / "crates/voice-probe/Dockerfile"
                        valid = PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=PINNED_DIGEST)
                        self.assertEqual(toolchain_parity_errors(root), [])
                        bad = PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=digest)
                        probe.write_text((valid if later else "") + bad)
                        errors = toolchain_parity_errors(root)
                        self.assertTrue(
                            any("crates/voice-probe/Dockerfile" in error and
                                ("digest" in error or "pinned" in error) for error in errors),
                            errors,
                        )

    def test_probe_only_builder_digest_disagreement_fails_through_aggregate(self):
        with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
            root = make_fixture_root(scratch)
            self.assertEqual(toolchain_parity_errors(root), [])
            (root / "crates/voice-probe/Dockerfile").write_text(
                PINNED_DOCKERFILE.format(tag="1.99-trixie", digest="b" * 64)
            )
            self.assertEqual(toolchain_parity_errors(root), [
                "builder digests disagree: Dockerfile@sha256:aaaaaaaaaaaa, "
                "crates/voice-probe/Dockerfile@sha256:bbbbbbbbbbbb"
            ])

    def assert_continuation_diagnostics(self, errors, name, expected=()):
        self.assertIsInstance(errors, list)
        if expected:
            self.assertTrue(errors, f"{name}: continued Rust stage must not disappear")
        for error in errors:
            self.assertIsInstance(error, str)
            self.assertTrue(error.startswith(f"{name}: "), errors)
        # A deliberately unsupported form may fail closed, even when pinned.
        # A missing-stage/digest error is NOT a valid pinned-form rejection.
        def unsupported(error):
            return (("unsupported" in error.lower() or "not supported" in error.lower())
                    and ("continuation" in error.lower() or "escape" in error.lower()))
        self.assertTrue(all(unsupported(error) or
                            (bool(expected) and all(term in error for term in expected))
                            for error in errors), errors)

    def assert_continued_stage_policy(self, reference, expected=()):
        # Independent scope: dropping a covered Dockerfile must not also drop
        # these fixtures. Each keeps a pinned first stage so omission is visible.
        for name in ("Dockerfile", "crates/voice-probe/Dockerfile"):
            for form, directive, later in continued_from_fixtures(reference):
                with self.subTest(dockerfile=name, form=form):
                    with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
                        root = make_fixture_root(scratch)
                        self.assertEqual(toolchain_parity_errors(root), [])
                        first = PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=PINNED_DIGEST)
                        # write_bytes preserves the CRLF case instead of text
                        # IO normalizing it before the production reader runs.
                        (root / name).write_bytes((directive + first + later).encode())
                        self.assert_continuation_diagnostics(
                            toolchain_parity_errors(root), name, expected)

    def test_correctly_pinned_continued_stage_passes_or_explicitly_refuses(self):
        self.assert_continued_stage_policy(f"1.99-trixie@sha256:{PINNED_DIGEST}")

    def test_continued_later_unpinned_stage_fails_through_aggregate(self):
        self.assert_continued_stage_policy("1.99-trixie", ("stage 1", "digest"))

    def test_continued_later_wrong_tag_fails_through_aggregate(self):
        self.assert_continued_stage_policy(
            f"1.98-trixie@sha256:{PINNED_DIGEST}",
            ("stage 1", "expected rust:1.99-trixie", "got rust:1.98-trixie"))

    def test_continued_later_digest_disagreement_fails_through_aggregate(self):
        self.assert_continued_stage_policy(
            "1.99-trixie@sha256:" + "b" * 64,
            ("disagree", "sha256:aaaaaaaaaaaa", "sha256:bbbbbbbbbbbb"))

    def assert_docker_contract(self, source, expected=()):
        # Keep scope independent of the scanner and fixture inventories. Only
        # file-specific diagnostics count: unrelated workflow failures do not.
        for name in ("Dockerfile", "crates/voice-probe/Dockerfile"):
            with self.subTest(dockerfile=name):
                with tempfile.TemporaryDirectory(prefix="toolchain-parity-") as scratch:
                    root = make_fixture_root(scratch)
                    self.assertEqual(toolchain_parity_errors(root), [])
                    (root / name).write_bytes(source.encode("utf-8"))
                    errors = toolchain_parity_errors(root)
                    self.assertIsInstance(errors, list)
                    if not expected:
                        self.assertEqual(errors, [])
                    else:
                        self.assertTrue(errors, f"{name}: expected explicit rejection")
                        for error in errors:
                            self.assertIsInstance(error, str)
                            self.assertTrue(error.startswith(f"{name}: "), errors)
                            self.assertTrue(all(term in error for term in expected), errors)

    def test_even_escape_run_preserves_later_unpinned_stage(self):
        first = PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=PINNED_DIGEST)
        for newline in ("\n", "\r\n"):
            with self.subTest(newline=repr(newline)):
                source = first + "RUN printf '%s' " + chr(92) * 2 + "\n"
                source += "FROM rust:1.99-trixie AS extra\n"
                self.assert_docker_contract(source.replace("\n", newline), ("stage 1", "digest"))

    def test_even_escape_run_preserves_later_wrong_tag(self):
        first = PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=PINNED_DIGEST)
        source = first + "RUN printf '%s' " + chr(92) * 2 + "\n"
        source += f"FROM rust:1.98-trixie@sha256:{PINNED_DIGEST} AS extra\n"
        self.assert_docker_contract(source, ("stage 1", "expected rust:1.99-trixie", "got rust:1.98-trixie"))

    def test_variable_builder_is_not_supplied_by_from_looking_run_data(self):
        source = "ARG BASE\nFROM ${BASE} AS builder\n"
        source += "RUN printf '%s' \"" + chr(92) + "\n"
        source += f"FROM rust:1.99-trixie@sha256:{PINNED_DIGEST} AS decoy\"\n"
        self.assert_docker_contract(source, ("unsupported", "variable-backed image reference"))

    def test_from_looking_run_data_does_not_create_a_stage(self):
        first = PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=PINNED_DIGEST)
        for reference in ("1.99-trixie", f"1.98-trixie@sha256:{'b' * 64}"):
            with self.subTest(reference=reference):
                source = first + "RUN printf '%s' \"" + chr(92) + "\n"
                source += f"FROM rust:{reference} AS decoy\"\n"
                self.assert_docker_contract(source)

    def test_inactive_escape_comment_is_ordinary_comment(self):
        first = PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=PINNED_DIGEST)
        self.assert_docker_contract(first + "# escape=`\n")

    def test_active_backtick_escape_directive_is_explicitly_refused(self):
        first = PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=PINNED_DIGEST)
        self.assert_docker_contract("# escape=`\n" + first, ("unsupported", "escape"))

    def test_first_line_bom_is_explicitly_refused(self):
        first = PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=PINNED_DIGEST)
        self.assert_docker_contract(chr(0xfeff) + first, ("unsupported", "byte order mark"))

    def test_noncanonical_separators_are_explicitly_refused(self):
        first = PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=PINNED_DIGEST)
        for separator in ("\x0b", "\x0c"):
            with self.subTest(separator=repr(separator)):
                source = first + f"FROM{separator}rust:1.99-trixie AS extra\n"
                self.assert_docker_contract(source, ("unsupported", "separator"))

    def test_split_from_keyword_is_explicitly_refused(self):
        first = PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=PINNED_DIGEST)
        for split in ("FROM" + chr(92) + "\n ", "FR" + chr(92) + "\nOM ",
                      "F" + chr(92) + "\n# comment\nROM ", "fro" + chr(92) + "\nm "):
            for reference in ("1.99-trixie", f"1.98-trixie@sha256:{PINNED_DIGEST}"):
                with self.subTest(split=split, reference=reference):
                    source = first + split + f"rust:{reference} AS extra\n"
                    self.assert_docker_contract(source, ("unsupported", "FROM continuation"))

    def test_canonical_rust_image_prefixes_pass_and_still_check_later_stages(self):
        first = PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=PINNED_DIGEST)
        for prefix in ("", "docker.io/library/", "library/"):
            with self.subTest(prefix=prefix):
                pinned = first.replace("rust:", prefix + "rust:")
                self.assert_docker_contract(pinned)
                self.assert_docker_contract(
                    first + f"FROM {prefix}rust:1.99-trixie AS extra\n", ("stage 1", "digest"))
                self.assert_docker_contract(
                    first + f"FROM {prefix}rust:1.98-trixie@sha256:{PINNED_DIGEST} AS extra\n",
                    ("stage 1", "expected rust:1.99-trixie", "got rust:1.98-trixie"))

    def test_heredoc_content_is_explicitly_refused(self):
        first = PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=PINNED_DIGEST)
        for instruction in ("RUN <<EOF\nprintf ok\nEOF\n",
                            "RUN <<'EOF'\nFROM rust:1.99-trixie AS decoy\nEOF\n"):
            with self.subTest(instruction=instruction):
                self.assert_docker_contract(first + instruction, ("unsupported", "heredoc"))

    def test_supported_ordinary_instruction_continuations_pass(self):
        first = PINNED_DOCKERFILE.format(tag="1.99-trixie", digest=PINNED_DIGEST)
        for instruction, argument in (("RUN printf '%s'", "ok"),
                                      ("LABEL purpose=fixture", "continuation=ok"),
                                      ("HEALTHCHECK CMD printf", "ok")):
            for newline in ("\n", "\r\n"):
                with self.subTest(instruction=instruction, newline=repr(newline)):
                    source = "# ordinary comment\n\n" + first
                    source += instruction + " " + chr(92) + "\n# continued comment\n\n " + argument + "\n"
                    self.assert_docker_contract(source.replace("\n", newline))

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
