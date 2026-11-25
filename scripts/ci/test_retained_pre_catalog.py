"""Reconstruct catalog candidates in disposable repositories without a database."""

import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from ci.staging_rollback_compatibility import MIGRATION_PATHS, Refused, git, git_environment
import retained_pre_catalog as catalog
from script_intake import decode_json
sys.path.pop(0)


ROOT = Path(__file__).resolve().parents[2]
ORIGINAL = "d8b6c1503fd72a7533ea96979f8c2ddc794e6737"
LANDED = "28990125adbc3a636c6a70e9a99d8dbd9f77fbcd"
FENCE = f"{MIGRATION_PATHS[0]}/0475_attendance_recovery_boundary.sql"
STORE = f"{MIGRATION_PATHS[1]}/0465_pause_resume_marker.sql"
LOCK = "migrations.lock"


class RetainedPreCatalogTests(unittest.TestCase):
    def setUp(self):
        scratch = TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.repo = Path(scratch.name)
        self.git("init", "--object-format=sha1", "--template=")
        objects = git(ROOT, "rev-parse", "--path-format=absolute", "--git-path", "objects")
        (self.repo / ".git/objects/info/alternates").write_bytes(objects)
        self.git("restore", f"--source={ORIGINAL}", "--worktree", "--", *MIGRATION_PATHS, LOCK)
        self.original = self.commit()

        self.fence = self.repo / FENCE
        self.sql = git(ROOT, "show", f"{LANDED}:{FENCE}")
        self.fence.write_bytes(self.sql)
        self.lock = self.repo / LOCK
        original_lock = decode_json(self.lock.read_bytes())
        landed_lock = decode_json(git(ROOT, "show", f"{LANDED}:{LOCK}"))
        original_lock["migrations"][FENCE] = landed_lock["migrations"][FENCE]
        self.lock_bytes = (json.dumps(original_lock, indent=2) + "\n").encode()
        self.lock.write_bytes(self.lock_bytes)
        self.reconstruction = self.commit()

    def git(self, *args):
        return subprocess.run(
            ["git", "--no-replace-objects", "-C", self.repo, "-c", "core.hooksPath=/dev/null",
             "-c", "core.autocrlf=false", "-c", "core.fileMode=true", "-c", "core.excludesFile=/dev/null",
             "-c", "user.name=Catalog Fixture", "-c", "user.email=catalog@example.invalid", *args],
            env={**git_environment(), "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull},
            check=True, capture_output=True, timeout=10,
        ).stdout

    def commit(self):
        self.git("add", "--all")
        self.git("commit", "--no-gpg-sign", "-m", "Disposable catalog")
        return self.git("rev-parse", "HEAD").decode().strip()

    def assert_refused(self, sha, reason):
        with self.assertRaisesRegex(Refused, f"^{reason}$"):
            catalog.verify_retained_catalog(self.repo, sha)

    def test_proves_exact_historical_catalog_from_distinct_commits(self):
        (self.repo / "unrelated.txt").write_text("Independent source identity\n")
        second = self.commit()
        for repo, sha in ((self.repo, self.reconstruction), (self.repo / MIGRATION_PATHS[0], second)):
            with self.subTest(sha=sha):
                self.assertEqual(catalog.verify_retained_catalog(repo, sha), {
                    "source_sha": "058141c29cf135304b060d8fd89aa6e21e44a0c6",
                    "reconstruction_sha": sha,
                    "cutover_tree": "8be4d2baba589a053bacc2523cbdcb760fca4d90",
                    "store_tree": "0a53e81334904b4e467fa0fe9e3095eb87653e8a",
                    "lock_blob": "2e93ede5799fe3806bd2f21cb35a3544f9f23e37",
                    "landed_0475_blob": "36e7d2dd7e129a4823d275d2a8f4e15694259323",
                    "landed_0475_sha384": ("845e9144067a0104232a6c75b4bcfe9066ff8b18bac69e050334f53deea44e6a4d3"
                                          "e12e8185938acbeaf496b7f3ed486"),
                    "cutover_sha256": "13e6dd45635b089790300733647a6fdb2eea587be3564b027c78f2106f1d44f9",
                    "store_sha256": "b72cbabfbe78bb568556acc1a5c938874d12ddace08a54d1ef300aab36c24dce",
                })

    def test_refuses_omitted_altered_and_extra_cutover_sql(self):
        self.assert_refused(self.original, "retained_catalog_tree_mismatch")
        self.fence.write_bytes(self.sql + b"\n")
        self.assert_refused(self.commit(), "retained_catalog_tree_mismatch")
        self.fence.write_bytes(self.sql)
        self.fence.with_name("0476_extra.sql").write_text("SELECT 1;\n")
        self.assert_refused(self.commit(), "retained_catalog_tree_mismatch")

    def test_refuses_substituted_migration_path_mode_or_type(self):
        wrong_path = self.fence.with_name("0475_other_description.sql")
        self.fence.rename(wrong_path)
        self.assert_refused(self.commit(), "retained_catalog_tree_mismatch")
        wrong_path.rename(self.fence)
        self.fence.chmod(0o755)
        self.assert_refused(self.commit(), "retained_catalog_tree_mismatch")
        self.fence.rename(self.repo / "retained.sql")
        self.fence.symlink_to("../../../retained.sql")
        self.assert_refused(self.commit(), "retained_catalog_tree_mismatch")

    def test_refuses_store_ledger_catalog_substitution_at_same_version(self):
        store = self.repo / STORE
        store.write_bytes(store.read_bytes() + b"\n")
        self.assert_refused(self.commit(), "retained_catalog_tree_mismatch")

    def test_refuses_nonmatching_lock_with_exact_migration_trees(self):
        alternatives = {"original": git(ROOT, "show", f"{ORIGINAL}:{LOCK}"),
                        "landed": git(ROOT, "show", f"{LANDED}:{LOCK}"),
                        "different_bytes": self.lock_bytes + b"\n"}
        for label, lock in alternatives.items():
            with self.subTest(lock=label):
                self.lock.write_bytes(lock)
                self.assert_refused(self.commit(), "retained_catalog_lock_mismatch")

    def test_refuses_git_replacement_and_symbolic_source(self):
        self.git("replace", self.original, self.reconstruction)
        self.assert_refused(self.original, "retained_catalog_tree_mismatch")
        self.assert_refused("HEAD", "invalid_source_sha")

    def test_refuses_wrong_cutover_derivation_even_with_a_matching_tree_pin(self):
        self.fence.write_bytes(self.sql + b"\n")
        candidate = self.commit()
        candidate_tree = self.git("rev-parse", f"{candidate}:{MIGRATION_PATHS[0]}").strip()
        with patch.object(catalog, "RETAINED_CUTOVER_TREE", candidate_tree):
            self.assert_refused(candidate, "retained_catalog_tree_mismatch")

    def test_refuses_wrong_lock_derivation_even_with_a_matching_blob_pin(self):
        modified = decode_json(self.lock_bytes)
        modified["migrations"][FENCE]["justification"] = "Different lock entry"
        self.lock.write_text(json.dumps(modified, indent=2) + "\n")
        candidate = self.commit()
        candidate_blob = self.git("rev-parse", f"{candidate}:{LOCK}").strip()
        with patch.object(catalog, "RETAINED_LOCK_BLOB", candidate_blob):
            self.assert_refused(candidate, "retained_catalog_lock_mismatch")


if __name__ == "__main__":
    unittest.main()
