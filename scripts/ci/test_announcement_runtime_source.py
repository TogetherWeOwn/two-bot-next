"""Hermetic Git-object/source predicates; no remote, runtime or build authority."""

from dataclasses import replace
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import announcement_runtime_source as source
from announcement_runtime_schema import Refusal
from announcement_runtime_policy import runtime_preflight


H1, H2, H3 = "1" * 40, "2" * 40, "3" * 40
D1, D2 = "a" * 64, "b" * 64


def manifest(entries=None, commit=H1, tree=H2):
    if entries is None:
        entries = (source.Entry(b"src/app.rs", "100644", H3, 1, D1),)
    return source.Manifest(commit, tree, tuple(entries))


def topology():
    return {
        "candidate": source.CANDIDATE, "recorded_base": source.RECORDED_BASE,
        "control": H1, "merge_bases_cb": [source.MERGE_BASE],
        "merge_bases_ct": [source.MERGE_BASE], "current_main": H1,
        "ancestor_pairs": frozenset((source.MERGE_BASE, sha) for sha in (
            source.CANDIDATE, source.RECORDED_BASE, H1)),
        "current_pr": {
            "repository_id": "1396224242", "full_name": "TogetherWeOwn/two-bot-next",
            "head_repository_id": "1396224242", "base_repository_id": "1396224242",
            "number": "1639", "state": "open",
            "head_ref": "feat/announcement-command-live-dispatch", "base_ref": "main",
            "head_sha": source.CANDIDATE, "head_tree": source.CANDIDATE_TREE,
            "base_sha": H1,
        },
    }


class ManifestTests(unittest.TestCase):
    def test_literal_manifest_encoding_and_digest(self):
        m = manifest()
        expected = ('{"commit":"' + H1 + '","entries":[{"mode":"100644","object_id":"' +
                    H3 + '","path_hex":"7372632f6170702e7273","sha256":"' + D1 +
                    '","size":1}],"schema":"two.bot.announcement-source-manifest.v1",'
                    '"tree":"' + H2 + '"}').encode("ascii")
        self.assertEqual(m.digest(), hashlib.sha256(expected).hexdigest())
        self.assertEqual(m.record()["entries"][0]["path_hex"], b"src/app.rs".hex())

    def test_add_delete_modify_mode_and_non_runtime_paths_are_not_filtered(self):
        before = manifest([
            source.Entry(b"Cargo.lock", "100644", H1, 10, D1),
            source.Entry(b"old.txt", "100644", H1, 10, D1),
            source.Entry(b"script.sh", "100644", H1, 10, D1),
            source.Entry(b"src/app.rs", "100644", H1, 10, D1),
        ])
        after = manifest([
            source.Entry(b"Cargo.lock", "100644", H2, 11, D2),
            source.Entry(b"build.rs", "100644", H3, 1, D1),
            source.Entry(b"script.sh", "100755", H1, 10, D1),
            source.Entry(b"src/app.rs", "100644", H1, 10, D1),
        ], commit=H2, tree=H3)
        delta = source.complete_delta(before, after)
        changes = delta["changes"]
        self.assertEqual([bytes.fromhex(x["path_hex"]) for x in changes],
                         [b"Cargo.lock", b"build.rs", b"old.txt", b"script.sh"])
        self.assertIsNone(changes[1]["before"])
        self.assertIsNone(changes[2]["after"])
        self.assertEqual(changes[3]["before"]["mode"], "100644")
        self.assertEqual(changes[3]["after"]["mode"], "100755")
        self.assertEqual(delta["before_commit"], H1)
        self.assertEqual(delta["after_commit"], H2)

    def test_byte_paths_are_not_normalized(self):
        m = manifest([source.Entry(path, "100644", H3, 0, D1)
                      for path in (b"a b", b"caf\xc3\xa9", b"raw-\xff")])
        source.validate_manifest(m)
        self.assertEqual([x["path_hex"] for x in m.record()["entries"]],
                         ["612062", "636166c3a9", "7261772dff"])

    def test_unsafe_paths_refuse_before_rendering_values(self):
        for path in (b"", b"/absolute", b"../escape", b"a/../b", b"./a", b"a//b",
                     b"a/.git/config", b".GiT/config", b"a\\b", b"a\nb", b"a\0b",
                     b"a\tb", b"a\x7fb", b"x" * (source.PATH_BYTES_LIMIT + 1)):
            with self.subTest(path=path), self.assertRaisesRegex(Refusal, "source_path"):
                source.validate_manifest(manifest([replace(manifest().entries[0], raw_path=path)]))

    def test_unknown_modes_symlinks_submodules_missing_and_duplicate_objects_refuse(self):
        for mode in ("120000", "160000", "040000", "100664", "100600"):
            with self.subTest(mode=mode), self.assertRaisesRegex(Refusal, "source_mode"):
                source.validate_manifest(manifest([replace(manifest().entries[0], mode=mode)]))
        for entries in ([], [manifest().entries[0]] * 2,
                        [replace(manifest().entries[0], raw_path=b"z"), manifest().entries[0]],
                        [replace(manifest().entries[0], raw_path=b"src"), manifest().entries[0]]):
            with self.subTest(entries=entries), self.assertRaises(Refusal):
                source.validate_manifest(manifest(entries))

    def test_hash_count_and_size_bounds(self):
        for entry in (replace(manifest().entries[0], object_id="1" * 39),
                      replace(manifest().entries[0], sha256="A" * 64),
                      replace(manifest().entries[0], size=True),
                      replace(manifest().entries[0], size=-1),
                      replace(manifest().entries[0], size=source.OBJECT_LIMIT + 1)):
            with self.subTest(entry=entry), self.assertRaises(Refusal):
                source.validate_manifest(manifest([entry]))
        with patch.object(source, "ENTRY_LIMIT", 1), self.assertRaises(Refusal):
            source.validate_manifest(manifest([replace(manifest().entries[0], raw_path=p)
                                               for p in (b"a", b"b")]))
        with patch.object(source, "TREE_BYTES_LIMIT", 0), self.assertRaises(Refusal):
            source.validate_manifest(manifest())

    def test_no_delta_is_distinct_from_missing_manifest(self):
        self.assertEqual(source.complete_delta(manifest(), manifest())["changes"], [])
        with self.assertRaises(Refusal):
            source.complete_delta(manifest(), manifest([]))


class TopologyTests(unittest.TestCase):
    def test_matching_claims_never_enable_runtime(self):
        source.check_topology_claims(**topology())
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_wrong_foreign_closed_retargeted_pr_and_tree_refuse(self):
        changes = {"repository_id": "2", "full_name": "foreign/two-bot-next",
                   "head_repository_id": "2", "base_repository_id": "2", "number": "1",
                   "state": "closed", "head_ref": "different", "base_ref": "staging",
                   "head_sha": H2, "head_tree": H2, "base_sha": H2, "extra": "extension"}
        for key, value in changes.items():
            args = topology()
            args["current_pr"][key] = value
            with self.subTest(field=key), self.assertRaisesRegex(Refusal, "pr_identity"):
                source.check_topology_claims(**args)
        for key in topology()["current_pr"]:
            args = topology()
            del args["current_pr"][key]
            with self.subTest(missing=key), self.assertRaises(Refusal):
                source.check_topology_claims(**args)

    def test_main_movement_does_not_select_new_control(self):
        args = topology()
        args["current_main"] = H2
        with self.assertRaisesRegex(Refusal, "control_drift"):
            source.check_topology_claims(**args)

    def test_singleton_merge_base_required_for_both_pairs(self):
        for key in ("merge_bases_cb", "merge_bases_ct"):
            for bases in ([], [H2], [source.MERGE_BASE, H2],
                          [source.MERGE_BASE, source.MERGE_BASE], {source.MERGE_BASE}):
                args = topology()
                args[key] = bases
                with self.subTest(key=key, bases=bases), self.assertRaisesRegex(Refusal, "topology"):
                    source.check_topology_claims(**args)

    def test_all_three_ancestry_relations_required(self):
        for pair in topology()["ancestor_pairs"]:
            args = topology()
            args["ancestor_pairs"] -= {pair}
            with self.subTest(pair=pair), self.assertRaisesRegex(Refusal, "topology"):
                source.check_topology_claims(**args)

    def test_substituted_candidate_base_control_and_symbolic_refs_refuse(self):
        for key, value in (("candidate", H1), ("recorded_base", H1),
                           ("control", source.CANDIDATE), ("control", "main"),
                           ("candidate", "--help"), ("recorded_base", "HEAD")):
            args = topology()
            args[key] = value
            with self.subTest(key=key, value=value), self.assertRaises(Refusal):
                source.check_topology_claims(**args)


class LocalInventoryTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.repo = Path(self.directory.name)
        self.env = {"PATH": "/usr/bin:/bin", "HOME": self.directory.name,
                    "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
                    "GIT_AUTHOR_NAME": "Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
                    "GIT_COMMITTER_NAME": "Fixture",
                    "GIT_COMMITTER_EMAIL": "fixture@example.invalid"}
        self.git("init", "--quiet")
        (self.repo / "Cargo.lock").write_bytes(b"locked\0binary\xff\n")
        (self.repo / "build.rs").write_text('panic!("MUST NOT EXECUTE");\n')
        (self.repo / "old.txt").write_bytes(b"old\n")
        self.first = self.commit()
        (self.repo / "old.txt").unlink()
        (self.repo / "new.txt").write_bytes(b"new\n")
        (self.repo / "build.rs").chmod(0o755)
        self.second = self.commit()

    def git(self, *args):
        return subprocess.check_output(["/usr/bin/git", "-C", str(self.repo), *args],
                                       env=self.env, stderr=subprocess.DEVNULL, timeout=30).strip()

    def commit(self):
        self.git("add", "--all")
        self.git("commit", "--quiet", "-m", "Synthetic source fixture")
        return self.git("rev-parse", "HEAD").decode("ascii")

    def test_actual_git_blobs_include_binary_modes_and_all_files_without_checkout(self):
        inventory = source.LocalSourceInventory(self.repo)
        first = inventory.manifest(self.first)
        second = inventory.manifest(self.second)
        self.assertEqual([x.raw_path for x in first.entries],
                         [b"Cargo.lock", b"build.rs", b"old.txt"])
        binary = first.entries[0]
        self.assertEqual(binary.sha256, hashlib.sha256(b"locked\0binary\xff\n").hexdigest())
        self.assertEqual(binary.object_id, self.git("rev-parse", self.first + ":Cargo.lock").decode())
        self.assertEqual(first.entries[1].mode, "100644")
        self.assertEqual(second.entries[1].mode, "100755")
        self.assertEqual(inventory.merge_bases(self.first, self.second), [self.first])
        inventory.require_ancestor(self.first, self.second)
        with self.assertRaisesRegex(Refusal, "topology"):
            inventory.require_ancestor(self.second, self.first)
        changes = source.complete_delta(first, second)["changes"]
        self.assertEqual([bytes.fromhex(x["path_hex"]) for x in changes],
                         [b"build.rs", b"new.txt", b"old.txt"])
        self.assertFalse((self.repo / "target").exists())

    def test_worktree_and_untracked_changes_cannot_rewrite_object_manifest(self):
        inventory = source.LocalSourceInventory(self.repo)
        before = inventory.manifest(self.first)
        (self.repo / "Cargo.lock").write_text("uncommitted tampering")
        (self.repo / "generated.txt").write_text("not an approved packaging input")
        self.assertEqual(inventory.manifest(self.first), before)

    def test_candidate_hooks_filters_and_inherited_git_environment_do_not_execute(self):
        marker = self.repo / "executed"
        payload = self.repo / "candidate-script"
        payload.write_text('#!/bin/sh\ntouch "' + str(marker) + '"\n')
        payload.chmod(0o755)
        self.git("config", "core.fsmonitor", str(payload))
        self.git("config", "filter.candidate.smudge", str(payload))
        self.git("config", "core.pager", str(payload))
        self.git("config", "alias.ls-tree", "!" + str(payload))
        with patch.dict(os.environ, {"GIT_DIR": "/missing", "GIT_WORK_TREE": "/missing",
                                     "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "core.pager",
                                     "GIT_CONFIG_VALUE_0": str(payload)}):
            self.assertEqual(len(source.LocalSourceInventory(self.repo).manifest(self.first).entries), 3)
        self.assertFalse(marker.exists())

    def test_symlink_and_submodule_modes_refuse(self):
        (self.repo / "link").symlink_to("Cargo.lock")
        linked = self.commit()
        with self.assertRaisesRegex(Refusal, "source_mode"):
            source.LocalSourceInventory(self.repo).manifest(linked)
        (self.repo / "link").unlink()
        self.git("add", "--all")
        self.git("update-index", "--add", "--cacheinfo", "160000," + self.first + ",submodule")
        self.git("commit", "--quiet", "-m", "Synthetic gitlink")
        with self.assertRaisesRegex(Refusal, "source_mode"):
            source.LocalSourceInventory(self.repo).manifest(self.git("rev-parse", "HEAD").decode())

    def test_shallow_replace_and_graft_history_refuse(self):
        shallow = self.repo / ".git/shallow"
        shallow.write_text(self.first + "\n")
        with self.assertRaisesRegex(Refusal, "source_incomplete_history"):
            source.LocalSourceInventory(self.repo)
        shallow.unlink()
        self.git("replace", self.first, self.second)
        with self.assertRaisesRegex(Refusal, "source_replaced_history"):
            source.LocalSourceInventory(self.repo)
        self.git("replace", "-d", self.first)
        (self.repo / ".git/info/grafts").write_text(self.second + "\n")
        with self.assertRaisesRegex(Refusal, "source_grafted_history"):
            source.LocalSourceInventory(self.repo)

    def test_promisor_included_config_refuses_without_lazy_fetch(self):
        included = self.repo / "included-config"
        included.write_text('[remote "origin"]\n promisor = yes\n')
        self.git("config", "include.path", str(included))
        with self.assertRaisesRegex(Refusal, "source_incomplete_history"):
            source.LocalSourceInventory(self.repo)

    def test_missing_objects_and_symbolic_revisions_refuse(self):
        inventory = source.LocalSourceInventory(self.repo)
        for commit in ("f" * 40, "HEAD", "--help", self.git("rev-parse", "HEAD^{tree}").decode()):
            with self.subTest(commit=commit), self.assertRaises(Refusal):
                inventory.manifest(commit)

    def test_expired_local_budget_refuses(self):
        inventory = source.LocalSourceInventory(self.repo)
        inventory._deadline = 0
        with self.assertRaisesRegex(Refusal, "source_timeout"):
            inventory.manifest(self.first)

    def test_output_bounds_and_spawn_failure_are_sanitized(self):
        inventory = source.LocalSourceInventory(self.repo)
        with self.assertRaisesRegex(Refusal, "source_output_size"):
            inventory._git("cat-file", "blob", self.first + ":Cargo.lock", limit=1)
        with patch.object(source.subprocess, "Popen", side_effect=OSError("PRIVATE_SENTINEL")):
            with self.assertRaises(Refusal) as context:
                inventory.manifest(self.first)
        self.assertEqual(str(context.exception), "announcement evidence refused: source_git")

    def test_tree_and_object_bounds_refuse_not_truncate(self):
        for name, value in (("ENTRY_LIMIT", 2), ("OBJECT_LIMIT", 2), ("TREE_BYTES_LIMIT", 2)):
            inventory = source.LocalSourceInventory(self.repo)
            with patch.object(source, name, value), self.subTest(bound=name), self.assertRaises(Refusal):
                inventory.manifest(self.first)

    def raw_object(self, kind, data):
        return subprocess.check_output(
            ["/usr/bin/git", "-C", str(self.repo), "hash-object", "--literally", "-t", kind,
             "-w", "--stdin"], input=data, env=self.env,
            stderr=subprocess.DEVNULL, timeout=30).strip().decode("ascii")

    def raw_commit(self, tree):
        data = (f"tree {tree}\nauthor Fixture <fixture@example.invalid> 1 +0000\n"
                "committer Fixture <fixture@example.invalid> 1 +0000\n\nFixture\n").encode()
        return self.raw_object("commit", data)

    def test_raw_noncanonical_modes_refuse_before_git_normalization(self):
        blob = self.git("rev-parse", self.first + ":Cargo.lock").decode()
        for mode in (b"100664", b"100600", b"100777", b"0100644"):
            tree = self.raw_object("tree", mode + b" file\0" + bytes.fromhex(blob))
            # Demonstrate the actual normalizing reader, not an Entry mutation.
            displayed = self.git("ls-tree", tree)
            self.assertIn(displayed.split()[0], (b"100644", b"100755"))
            for nested in (False, True):
                selected = self.raw_object("tree", b"40000 nested\0" + bytes.fromhex(tree)) if nested else tree
                with self.subTest(mode=mode, nested=nested), self.assertRaisesRegex(Refusal, "source_mode"):
                    source.LocalSourceInventory(self.repo).manifest(self.raw_commit(selected))

    def test_raw_tree_order_duplicates_truncation_and_empty_directories_refuse(self):
        blob = bytes.fromhex(self.git("rev-parse", self.first + ":Cargo.lock").decode())
        leaf = b"100644 file\0" + blob
        for data in (leaf + leaf, b"100644 z\0" + blob + b"100644 a\0" + blob,
                     leaf[:-1], b"100644 bad/name\0" + blob, b"040000 directory\0" + blob):
            with self.subTest(data=data), self.assertRaises(Refusal):
                source._validate_raw_tree(data)
        empty = self.raw_object("tree", b"")
        root = self.raw_object("tree", b"40000 empty\0" + bytes.fromhex(empty))
        with self.assertRaisesRegex(Refusal, "source_tree"):
            source.LocalSourceInventory(self.repo).manifest(self.raw_commit(root))

    def test_commit_tree_and_blob_size_checked_before_body_read(self):
        inventory = source.LocalSourceInventory(self.repo)
        blob = bytes.fromhex(self.git("rev-parse", self.first + ":Cargo.lock").decode())
        tree = b"".join(b"100644 file-" + f"{i:04d}".encode() + b"\0" + blob for i in range(64))
        for kind, data in (("commit", b"message" * 256), ("tree", tree), ("blob", b"x" * 2048)):
            object_id = self.raw_object(kind, data)
            with patch.object(source, "OBJECT_LIMIT", 1024), patch.object(
                    inventory, "_git", wraps=inventory._git) as calls:
                with self.subTest(kind=kind), self.assertRaisesRegex(Refusal, "source_size"):
                    inventory._object(kind, object_id)
                self.assertEqual(calls.call_args_list[0].args, ("cat-file", "-s", object_id))
                self.assertFalse(any(call.args == ("cat-file", kind, object_id)
                                     for call in calls.call_args_list))

    def test_git_child_has_address_space_and_cpu_limits(self):
        inventory = source.LocalSourceInventory(self.repo)
        with patch.object(source.subprocess, "Popen", wraps=subprocess.Popen) as spawn:
            inventory.manifest(self.first)
        self.assertTrue(spawn.call_args_list)
        for call in spawn.call_args_list:
            argv = call.args[0]
            self.assertEqual(argv[:5], ["/usr/bin/prlimit", "--as=536870912", "--cpu=30", "--", "/usr/bin/git"])
            self.assertNotIn("GIT_CONFIG_COUNT", call.kwargs["env"])

    def test_comparisons_bind_every_frozen_tree_including_merge_base(self):
        first_tree = self.git("rev-parse", self.first + "^{tree}").decode()
        second_tree = self.git("rev-parse", self.second + "^{tree}").decode()
        with patch.multiple(source, CANDIDATE=self.second, CANDIDATE_TREE=second_tree,
                            RECORDED_BASE=self.first, RECORDED_BASE_TREE=first_tree,
                            MERGE_BASE=self.first, MERGE_BASE_TREE=first_tree):
            comparisons = source.collect_source_comparisons(
                self.repo, control=self.first, served_before=self.first)
            self.assertEqual(comparisons["manifests"][self.first].tree, first_tree)
            with patch.object(source, "MERGE_BASE_TREE", "0" * 40):
                with self.assertRaisesRegex(Refusal, "source_identity"):
                    source.collect_source_comparisons(
                        self.repo, control=self.first, served_before=self.first)

    def test_wrong_frozen_candidate_objects_refuse(self):
        with self.assertRaises(Refusal):
            source.collect_source_comparisons(self.repo, control=self.second, served_before=self.first)
        with self.assertRaises(Refusal):
            source.collect_source_comparisons(self.repo, control="unbound", served_before=self.first)


if __name__ == "__main__":
    unittest.main()
