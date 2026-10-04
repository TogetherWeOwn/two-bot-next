"""Lock the Rust CI lanes' disk-hygiene surface; no GitHub or PyYAML needed.

Two consecutive main `check` failures died in the `cargo test --workspace
--test '*'` link step on hosted overflow runners: the job-container overlay
ran out of space (98 MB left), then `rust-lld` bus error. Unit tests passed
in the same jobs; infra disk-fit, not a code regression.

The old single `check` job is now four parallel lanes (`check` lints,
`rust-tests`, `ignored-db-stores` and `ignored-db-runtime` test); each runs in
its own job container, so each keeps the guards (TOG-12134):
- every lane pins dev/test debuginfo to `line-tables-only`: smaller linked
  binaries, backtraces stay useful.
- every lane disables dev/test incremental compilation: a cold CI link never
  reuses incremental artifacts, so don't write them.
- every lane's job-container prerequisites step removes `/var/lib/apt/lists`:
  the package lists stay on the overlay otherwise.
- in the `check` lane the `cargo-deny` step runs before the toolchain install
  and every compile/link step: the deny image ships its own toolchain and
  rustup-syncs the pinned one (a second ~1 GB toolchain download), so it
  must run while the disk is at its freest (CHANGES on PR #232, 07:31Z).
- the `rust-tests` integration step still runs the full
  `--workspace --test '*'` graph: coverage must not be narrowed as a
  substitute for freeing disk.

Toolchain ordering guard: pip 23 probes `rustc --version` from the workspace
with a 0.5 s timeout for its user agent. If the pinned toolchain still lacks
the components named in rust-toolchain.toml, the rustup proxy starts
installing them and the timeout kills it half-way, leaving untracked
`bin/cargo-fmt`; the later `cargo fmt` then fails with a file conflict in
roughly half the runs. Every lane that runs pip installs the components
before its first pip run.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github/workflows/check.yml"

FULL_INTEGRATION_RUN = "cargo test --workspace --test '*' --features two-bot-core/db --locked"


def job_body_lines(text, job_key):
    """Raw lines of one top-level job block (blank lines dropped)."""
    lines = text.splitlines()
    start = next(
        i for i, line in enumerate(lines)
        if re.match(rf"^  {re.escape(job_key)}:\s*$", line)
    )
    body = []
    for line in lines[start + 1:]:
        if not line.strip():
            continue
        if re.match(r"^  \S", line):
            break
        body.append(line)
    return body


def child_block(body, header, header_indent):
    """Lines under `header` until a line at or above its indent."""
    start = next(i for i, line in enumerate(body) if line.strip() == header)
    depth = len(body[start]) - len(body[start].lstrip())
    assert depth == header_indent, f"{header} moved (indent {depth})"
    taken = []
    for line in body[start + 1:]:
        if len(line) - len(line.lstrip()) <= depth:
            break
        taken.append(line)
    return taken


def step_body(body, step_name):
    """Raw text of one `- name:` step until the next same-level `- ` item."""
    start = next(
        i for i, line in enumerate(body)
        if line.strip() == f"- name: {step_name}"
    )
    taken = []
    for line in body[start + 1:]:
        if re.match(r"^      - ", line):
            break
        taken.append(line)
    return "\n".join(taken)


LANES = ("check", "rust-tests", "ignored-db-stores", "ignored-db-runtime")
PREREQUISITES = "Job-container prerequisites"


def step_names(body):
    return [
        line.strip()[len("- name: "):]
        for line in body
        if line.strip().startswith("- name: ")
    ]


class CheckDiskHygieneTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = WORKFLOW.read_text()
        cls.jobs = {lane: job_body_lines(cls.text, lane) for lane in LANES}
        cls.envs = {lane: child_block(body, "env:", 4) for lane, body in cls.jobs.items()}

    def assertEnv(self, lane, key, value):
        line = next(
            (line for line in self.envs[lane] if line.strip().startswith(f"{key}:")),
            None,
        )
        self.assertIsNotNone(line, f"{lane} job env must pin {key}")
        self.assertEqual(line.split(":", 1)[1].strip().strip("'\""), value)

    def test_debuginfo_is_line_tables_only(self):
        for lane in LANES:
            with self.subTest(lane=lane):
                self.assertEnv(lane, "CARGO_PROFILE_DEV_DEBUG", "line-tables-only")
                self.assertEnv(lane, "CARGO_PROFILE_TEST_DEBUG", "line-tables-only")

    def test_incremental_compilation_off(self):
        for lane in LANES:
            with self.subTest(lane=lane):
                self.assertEnv(lane, "CARGO_PROFILE_DEV_INCREMENTAL", "false")
                self.assertEnv(lane, "CARGO_PROFILE_TEST_INCREMENTAL", "false")

    def test_prerequisites_drop_apt_lists(self):
        for lane, body in self.jobs.items():
            with self.subTest(lane=lane):
                name = next(name for name in step_names(body) if name.startswith(PREREQUISITES))
                self.assertIn("rm -rf /var/lib/apt/lists", step_body(body, name))

    def test_integration_coverage_not_narrowed(self):
        integration = step_body(
            self.jobs["rust-tests"],
            "cargo test (integration, including website acceptance and backup round trip)",
        )
        self.assertIn(FULL_INTEGRATION_RUN, integration)

    def test_deny_runs_before_toolchain_and_compiles(self):
        names = step_names(self.jobs["check"])
        self.assertIn("cargo-deny", names)
        deny_at = names.index("cargo-deny")
        for heavy in (
            "Install Rust toolchain",
            "cargo fmt --check",
            "cargo clippy -D warnings",
        ):
            self.assertIn(heavy, names, f"expected step {heavy!r} in the check lane")
            self.assertLess(
                deny_at, names.index(heavy),
                f"cargo-deny must run before {heavy!r} (disk is freest early)",
            )
        # The test lanes never run cargo-deny: it belongs to the lint lane.
        for lane in LANES[1:]:
            self.assertNotIn("cargo-deny", step_names(self.jobs[lane]))

    def test_toolchain_components_installed_before_first_pip_run(self):
        install = "Install pinned Rust toolchain components"
        for lane, body in self.jobs.items():
            with self.subTest(lane=lane):
                first_pip = next(
                    (i for i, line in enumerate(body)
                     if re.search(r"(?:-m pip\b|^\s*pip3? )", line)),
                    None,
                )
                if first_pip is None:
                    continue  # lane never invokes pip; nothing to guard
                install_at = next(
                    (i for i, line in enumerate(body)
                     if line.strip() == f"- name: {install}"),
                    None,
                )
                self.assertIsNotNone(install_at, f"missing step {install!r} in {lane}")
                self.assertLess(
                    install_at, first_pip,
                    f"pip probes `rustc --version`; finish the toolchain install first ({lane})",
                )
                step = step_body(body, install)
                self.assertRegex(step, r"(?m)^\s+run: rustup toolchain install --no-self-update$")
                self.assertRegex(step, r"(?m)^\s+if: needs\.job-inputs\.outputs\.rust != 'false'$")


if __name__ == "__main__":
    unittest.main()
