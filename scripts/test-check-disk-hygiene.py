"""Lock the check-job disk-hygiene surface; no GitHub or PyYAML needed.

Two consecutive main `check` failures died in the `cargo test --workspace
--test '*'` link step on hosted overflow runners: the job-container overlay
ran out of space (98 MB left), then `rust-lld` bus error. Unit tests passed
in the same jobs; infra disk-fit, not a code regression.

Guards (TOG-12134):
- the `check` job pins dev/test debuginfo to `line-tables-only`: smaller
  linked test binaries, backtraces stay useful.
- the `check` job disables dev/test incremental compilation: a cold CI link
  never reuses incremental artifacts, so don't write them.
- the job-container prerequisites step removes `/var/lib/apt/lists`: the
  package lists stay on the overlay otherwise.
- the `cargo-deny` step runs before the toolchain install and every
  compile/link step: the deny image ships its own toolchain and
  rustup-syncs the pinned one (a second ~1 GB toolchain download), so it
  must run while the disk is at its freest (CHANGES on PR #232, 07:31Z).
- the integration step still runs the full `--workspace --test '*'` graph:
  coverage must not be narrowed as a substitute for freeing disk.

Toolchain ordering guard: pip 23 probes `rustc --version` from the workspace
with a 0.5 s timeout for its user agent. If the pinned toolchain still lacks
the components named in rust-toolchain.toml, the rustup proxy starts
installing them and the timeout kills it half-way, leaving untracked
`bin/cargo-fmt`; the later `cargo fmt` then fails with a file conflict in
roughly half the runs. The components are installed before the first pip run.
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


class CheckDiskHygieneTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = WORKFLOW.read_text()
        cls.check = job_body_lines(cls.text, "check")
        cls.env = child_block(cls.check, "env:", 4)

    def assertEnv(self, key, value):
        line = next(
            (line for line in self.env if line.strip().startswith(f"{key}:")),
            None,
        )
        self.assertIsNotNone(line, f"check job env must pin {key}")
        self.assertEqual(line.split(":", 1)[1].strip().strip("'\""), value)

    def test_debuginfo_is_line_tables_only(self):
        self.assertEnv("CARGO_PROFILE_DEV_DEBUG", "line-tables-only")
        self.assertEnv("CARGO_PROFILE_TEST_DEBUG", "line-tables-only")

    def test_incremental_compilation_off(self):
        self.assertEnv("CARGO_PROFILE_DEV_INCREMENTAL", "false")
        self.assertEnv("CARGO_PROFILE_TEST_INCREMENTAL", "false")

    def test_prerequisites_drop_apt_lists(self):
        prereq = step_body(self.check, "Job-container prerequisites (loopback forward to agent-testdb)")
        self.assertIn("rm -rf /var/lib/apt/lists", prereq)

    def test_integration_coverage_not_narrowed(self):
        integration = step_body(
            self.check, "cargo test (integration, including website acceptance and backup round trip)"
        )
        self.assertIn(FULL_INTEGRATION_RUN, integration)

    def test_deny_runs_before_toolchain_and_compiles(self):
        names = [
            line.strip()[len("- name: "):]
            for line in self.check
            if line.strip().startswith("- name: ")
        ]
        self.assertIn("cargo-deny", names)
        deny_at = names.index("cargo-deny")
        for heavy in (
            "Install Rust toolchain",
            "cargo fmt --check",
            "cargo clippy -D warnings",
            "cargo test (unit and binary, including RSVP store)",
            "cargo test (integration, including website acceptance and backup round trip)",
        ):
            self.assertIn(heavy, names, f"expected step {heavy!r} in check job")
            self.assertLess(
                deny_at, names.index(heavy),
                f"cargo-deny must run before {heavy!r} (disk is freest early)",
            )

    def test_toolchain_components_installed_before_first_pip_run(self):
        install = "Install pinned Rust toolchain components"
        install_at = next(
            (i for i, line in enumerate(self.check) if line.strip() == f"- name: {install}"),
            None,
        )
        self.assertIsNotNone(install_at, f"missing step {install!r}")
        first_pip = next(
            (i for i, line in enumerate(self.check)
             if re.search(r"(?:-m pip\b|^\s*pip3? )", line)),
            None,
        )
        self.assertIsNotNone(first_pip, "pip step moved or renamed; update this guard")
        self.assertLess(
            install_at, first_pip,
            "pip probes `rustc --version`; finish the toolchain install first",
        )
        body = step_body(self.check, install)
        self.assertRegex(body, r"(?m)^\s+run: rustup toolchain install --no-self-update$")
        self.assertRegex(body, r"(?m)^\s+if: needs\.job-inputs\.outputs\.rust != 'false'$")


if __name__ == "__main__":
    unittest.main()
