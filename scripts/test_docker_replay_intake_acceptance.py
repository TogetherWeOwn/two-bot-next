"""Protected offline acceptance for confined replay and both verifier layers.

All mutated paths stay inside a disposable fixture. The manifest gate's fetch
is a local witness, never Cargo; CLI mutations change only COPY, so no mutated
RUN text is passed to a shell. Run this module normally and with python -O.
"""

import contextlib
import importlib.util
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
DOCKERFILES = ("Dockerfile", "crates/voice-probe/Dockerfile")
SPEC = importlib.util.spec_from_file_location(
    "docker_replay_intake_gate", ROOT / "scripts/check-docker-manifests.py")
manifests = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(manifests)

_WRITE_WATCH = None


def _audit_writes(event, args):
    """Observe real filesystem writes, including ones erased by temp cleanup."""
    if _WRITE_WATCH is None or event not in ("open", "os.mkdir"):
        return
    if event == "open" and not args[2] & (
            os.O_WRONLY | os.O_RDWR | os.O_CREAT | os.O_TRUNC | os.O_APPEND):
        return
    if not isinstance(args[0], (str, bytes, os.PathLike)):
        return
    fixture, scratch, writes = _WRITE_WATCH
    path = Path(os.fsdecode(args[0])).resolve()
    if not path.is_relative_to(fixture):
        return
    # Creating the empty temporary layer itself is not replay. Creating its
    # contents is: even a late refusal must leave no earlier replay writes.
    if event == "os.mkdir" and path.parent == scratch:
        return
    writes.append((event, str(path.relative_to(fixture))))


sys.addaudithook(_audit_writes)


@contextlib.contextmanager
def watch_writes(fixture, scratch):
    global _WRITE_WATCH
    writes = []
    previous = _WRITE_WATCH
    _WRITE_WATCH = (fixture.resolve(), scratch.resolve(), writes)
    try:
        yield writes
    finally:
        _WRITE_WATCH = previous


def make_fixture(parent, with_scripts=False):
    """Copy the committed manifests and Dockerfiles, not application sources."""
    root = parent / "root"
    members = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["members"]
    files = ["Cargo.toml", "Cargo.lock", *DOCKERFILES,
             *(f"{member}/Cargo.toml" for member in members)]
    for name in files:
        dest = root / name
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / name, dest)
    if with_scripts:
        # Copy the script tree so a shared parser/helper can be used without
        # requiring any particular helper name or module layout.
        shutil.copytree(ROOT / "scripts", root / "scripts",
                        ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
    return root


def snapshot(root):
    return {str(path.relative_to(root)):
            path.read_bytes() if path.is_file() else None
            for path in sorted(root.rglob("*"))}


class DockerReplayConfinementAcceptanceTests(unittest.TestCase):
    def check_refusal(self, instruction, absolute=None, diagnostic=None):
        for name in DOCKERFILES:
            with self.subTest(dockerfile=name):
                with tempfile.TemporaryDirectory(prefix="docker-replay-acceptance-") as tmp:
                    fixture = Path(tmp)
                    root = make_fixture(fixture)
                    scratch = fixture / "scratch"
                    scratch.mkdir()
                    outside = (fixture if absolute else scratch) / "outside"
                    outside.mkdir()
                    (outside / "sentinel").write_bytes(b"original sentinel\n")
                    (outside / "Cargo.toml").write_bytes(b"original manifest sentinel\n")
                    before = snapshot(outside)
                    if absolute is None:
                        extra = instruction
                    else:
                        target = str(outside) if absolute else "../outside"
                        if instruction == "COPY":
                            extra = f"COPY Cargo.toml {shlex.quote(target + '/')}"
                        elif instruction == "mkdir":
                            extra = f"RUN mkdir -p {shlex.quote(target + '/created')}"
                        else:
                            extra = f"RUN echo overwritten > {shlex.quote(target + '/sentinel')}"
                    docker = (root / name).read_text()
                    self.assertIn("COPY . .", docker)
                    # Put the bad instruction last to catch validation that
                    # starts replaying a valid prefix before inspecting it.
                    mutated = docker.replace("COPY . .", extra + "\nCOPY . .", 1)
                    fetched = []
                    error = None
                    with watch_writes(fixture, scratch) as writes:
                        with mock.patch.object(
                                subprocess, "run",
                                side_effect=AssertionError("gate must not launch processes")):
                            try:
                                manifests.check_tree(
                                    mutated, root, scratch_dir=str(scratch),
                                    fetch=lambda dest: fetched.append(dest))
                            except Exception as caught:
                                error = caught
                    problems = []
                    if error is None:
                        problems.append("invalid instruction was accepted")
                    elif not isinstance(error, manifests.Refusal):
                        problems.append(f"escaped {type(error).__name__}, not Refusal")
                    elif not str(error) or (diagnostic and not re.search(diagnostic, str(error))):
                        problems.append("Refusal lacks fixed diagnostic vocabulary")
                    if snapshot(outside) != before:
                        problems.append("out-of-layer sentinels/tree changed")
                    if writes:
                        problems.append(f"{len(writes)} replay writes before refusal; first={writes[0]}")
                    if fetched:
                        problems.append("fetch reached before refusal")
                    self.assertEqual(problems, [], f"{extra}: " + "; ".join(problems))

    def test_absolute_copy_destination_refuses_before_replay(self):
        self.check_refusal("COPY", absolute=True)

    def test_parent_copy_destination_refuses_before_replay(self):
        self.check_refusal("COPY", absolute=False)

    def test_absolute_mkdir_target_refuses_before_replay(self):
        self.check_refusal("mkdir", absolute=True)

    def test_parent_mkdir_target_refuses_before_replay(self):
        self.check_refusal("mkdir", absolute=False)

    def test_absolute_echo_target_refuses_before_replay(self):
        self.check_refusal("echo", absolute=True)

    def test_parent_echo_target_refuses_before_replay(self):
        self.check_refusal("echo", absolute=False)

    def test_empty_mkdir_operand_refuses_before_replay(self):
        self.check_refusal('RUN mkdir -p ""', diagnostic=r"mkdir|empty")

    def test_quoted_or_escaped_redirection_is_not_a_shell_operator(self):
        for operator in ("'>'", '\">\"', r"\>"):
            with self.subTest(operator=operator):
                self.check_refusal(
                    f"RUN echo '' {operator} src/lib.rs",
                    diagnostic=r"echo|redirection|unsupported")

    def test_committed_layers_accept_unquoted_redirection(self):
        for name in DOCKERFILES:
            with self.subTest(dockerfile=name):
                with tempfile.TemporaryDirectory(prefix="docker-replay-control-") as tmp:
                    fixture = Path(tmp)
                    root = make_fixture(fixture)
                    scratch = fixture / "scratch"
                    scratch.mkdir()
                    fetched = []

                    def witness(dest):
                        self.assertTrue((dest / "Cargo.toml").is_file())
                        self.assertTrue((dest / "src/lib.rs").is_file())
                        self.assertTrue((dest / "crates/voice-probe/src/main.rs").is_file())
                        self.assertTrue(dest.resolve().is_relative_to(scratch.resolve()))
                        fetched.append(dest)

                    with watch_writes(fixture, scratch) as writes:
                        with mock.patch.object(
                                subprocess, "run",
                                side_effect=AssertionError("gate must not launch processes")):
                            manifests.check_tree(
                                (root / name).read_text(), root,
                                scratch_dir=str(scratch), fetch=witness)
                    self.assertEqual(len(fetched), 1)
                    self.assertTrue(writes, "write observer must detect a real replay")


def run_verifier(root):
    scratch = root / "scratch"
    scratch.mkdir()
    env = os.environ | {"PAPERCLIP_RUN_SCRATCH_DIR": str(scratch),
                        "RUNNER_TEMP": str(scratch), "PYTHONDONTWRITEBYTECODE": "1"}
    optimized = ["-O"] if sys.flags.optimize else []
    return subprocess.run(
        [sys.executable, *optimized, str(root / "scripts/test-docker-deps.py")],
        cwd=root, env=env, capture_output=True, text=True, timeout=120)


class DockerCopyIntakeAcceptanceTests(unittest.TestCase):
    def check_bad_copy(self, instruction, vocabulary):
        for name in DOCKERFILES:
            with self.subTest(dockerfile=name, instruction=instruction):
                with tempfile.TemporaryDirectory(prefix="docker-copy-intake-") as tmp:
                    root = make_fixture(Path(tmp), with_scripts=True)
                    path = root / name
                    docker = path.read_text()
                    self.assertIn("COPY . .", docker)
                    path.write_text(docker.replace(
                        "COPY . .", instruction + "\nCOPY . .", 1))
                    proc = run_verifier(root)
                problems = []
                if proc.returncode == 0:
                    problems.append("malformed COPY exited 0")
                if not re.search(r"Refusal: .*" + vocabulary, proc.stderr):
                    problems.append("missing fixed Refusal diagnostic")
                if "PASS" in proc.stdout or "PASS" in proc.stderr:
                    problems.append("printed PASS for a rejected verifier invocation")
                self.assertEqual(problems, [], "; ".join(problems))

    def test_copy_arity_refuses_in_either_layer_without_any_pass(self):
        for instruction in ("COPY foo", "COPY"):
            self.check_bad_copy(instruction, "COPY")

    def test_copy_quoting_refuses_in_either_layer_without_any_pass(self):
        for instruction in ("COPY 'unterminated ./", 'COPY "unterminated ./'):
            self.check_bad_copy(instruction, "malformed quoting")

    def test_actual_entrypoint_accepts_both_committed_layers(self):
        with tempfile.TemporaryDirectory(prefix="docker-copy-control-") as tmp:
            root = make_fixture(Path(tmp), with_scripts=True)
            proc = run_verifier(root)
        self.assertEqual(proc.returncode, 0, proc.stderr[-2000:])
        passes = [line for line in proc.stdout.splitlines() if "PASS" in line]
        self.assertEqual(len(passes), 2, proc.stdout)
        self.assertIn("Dockerfile", passes[0])
        self.assertIn("crates/voice-probe/Dockerfile", passes[1])


if __name__ == "__main__":
    unittest.main()
