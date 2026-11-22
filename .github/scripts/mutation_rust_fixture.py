"""Actions-only end-to-end scope and verdict fixtures for cargo-mutants."""

import os
import subprocess
import tempfile
from pathlib import Path

from mutation_rust import measure, verdict
from mutation_scope import added_lines, git, pr_diff

OLD_SOURCE = """pub fn eligible(value: i32) -> bool {
    let positive = value > 0;
    positive && value > 3
}

pub fn legacy(value: i32) -> bool {
    value != 10
}
"""
NEW_SOURCE = OLD_SOURCE.replace("value > 3", "value >= 3")
STRONG_TEST = """
#[test]
fn boundary() {
    for (value, expected) in [(1, false), (2, false), (3, true), (4, true)] {
        assert_eq!(eligible(value), expected);
    }
}
"""
WEAK_TEST = """
#[test]
fn unrelated() {
    assert!(legacy(9));
}
"""


def commit(root, message):
    git(root, "add", ".")
    git(root, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
        "commit", "-qm", message)
    return git(root, "rev-parse", "HEAD").strip()


def main():
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise SystemExit("mutation fixtures run only in GitHub Actions")
    with tempfile.TemporaryDirectory(dir=os.environ["RUNNER_TEMP"]) as scratch:
        output = Path(scratch)
        root = output / "crate"
        (root / "src").mkdir(parents=True)
        git(root, "init", "-q")
        (root / "Cargo.toml").write_text('[package]\nname = "mutation-fixture"\nversion = "0.1.0"\nedition = "2021"\n')
        source = root / "src/lib.rs"
        source.write_text(OLD_SOURCE)
        subprocess.run(["cargo", "generate-lockfile", "--offline"], cwd=root, check=True, timeout=30)
        (root / "uncovered.rs").write_text("pub fn untouched() -> bool { true }\n")
        base = commit(root, "baseline")
        git(root, "mv", "uncovered.rs", "src/uncovered.rs")
        for label, test, status, hard_code in (
            ("strong", STRONG_TEST, "killed", 0), ("weak", WEAK_TEST, "survived", 1),
        ):
            source.write_text(NEW_SOURCE + test)
            head = commit(root, label)
            _, patch = pr_diff(root, base, head)
            assert "src/uncovered.rs" not in added_lines(patch)
            patch_file = output / "fixture.diff"
            patch_file.write_text(patch)
            try:
                report = measure(root, patch_file, output)
            except (ValueError, subprocess.SubprocessError):
                baseline_log = output / "mutants.out/log/baseline.log"
                if baseline_log.is_file():
                    print(baseline_log.read_text()[-4000:])
                raise
            assert report["out_of_scope"] > 0, report
            assert {(row["file"], row["line"]) for row in report["results"]} == {("src/lib.rs", 3)}, report
            assert {row["status"] for row in report["results"]} == {status}, report
            assert verdict(report, "warn") == 0 and verdict(report, "hard") == hard_code
            print(f"Rust fixture {label}: {len(report['results'])} {status} on line 3; HARD exit {hard_code}")
        source.write_text(NEW_SOURCE + '\n#[test]\nfn broken() { assert!(!eligible(3)); }\n')
        head = commit(root, "broken baseline")
        _, patch = pr_diff(root, base, head)
        patch_file.write_text(patch)
        try:
            measure(root, patch_file, output)
        except ValueError as error:
            assert "exit 4" in str(error), error
            print("Rust fixture: failing baseline is an infrastructure error")
        else:
            raise AssertionError("failing Rust baseline was accepted")


if __name__ == "__main__":
    main()
