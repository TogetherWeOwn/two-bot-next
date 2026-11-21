"""Synthetic orchestration regressions; no live reader or workflow wiring."""
import copy
import unittest
from unittest import mock

from test_attestation_policy import ATTESTATION, BODY, metadata, policy
from test_attestation_snapshot import inventory


class Reader:
    def __init__(self, data, rows=None):
        self.pr = copy.deepcopy(data["pull_request"])
        self.rows = rows if rows is not None else [{"status": "modified", "filename": "docs/a.md"}]
        self.calls = []
        self.mutate = {}

    def __call__(self, route):
        self.calls.append(route)
        index = len(self.calls)
        if index in self.mutate:
            value = self.mutate[index]
            if isinstance(value, Exception):
                raise value
            return copy.deepcopy(value)
        if "/files?" in route:
            return copy.deepcopy(self.rows)
        if "/pulls/" in route:
            return copy.deepcopy(self.pr)
        return {"object": {"sha": "a" * 40}}


def evaluate(data, reader):
    return inventory.evaluate_bracketed(data, "org/repo", "a" * 40, reader, policy)


class Orchestration(unittest.TestCase):
    def assert_refused(self, data, reader):
        with self.assertRaises(inventory.InventoryError) as error:
            evaluate(data, reader)
        self.assertEqual(str(error.exception), "Attestation evaluation refused")
        self.assertIsNone(error.exception.__cause__)
        self.assertTrue(error.exception.__suppress_context__)

    def test_fork_and_rename_use_both_paths_and_bracket_policy(self):
        data = metadata(BODY + ATTESTATION)
        reader = Reader(data, [{"status": "renamed", "filename": "docs/new.md",
                                "previous_filename": "sql/old.sql"}])
        result = evaluate(data, reader)
        self.assertEqual(result, {"head_sha": "b" * 40, "base_sha": "a" * 40,
                                  "file_count": 1, "path_count": 2})
        self.assertEqual(reader.calls, ["repos/org/repo/pulls/1",
                                      "repos/org/repo/git/ref/heads/main",
                                      "repos/org/repo/pulls/1/files?per_page=100&page=1",
                                      "repos/org/repo/pulls/1",
                                      "repos/org/repo/git/ref/heads/main"])
        self.assert_refused(metadata(), Reader(metadata(), reader.rows))

    def test_stale_head_base_and_metadata_before_and_after(self):
        for call in (1, 4):
            for field in ("head", "base", "title", "body", "updated_at", "changed_files"):
                data = metadata()
                reader = Reader(data)
                stale = copy.deepcopy(reader.pr)
                if field in ("head", "base"):
                    stale[field]["sha"] = "c" * 40
                elif field == "changed_files":
                    stale[field] = 2
                else:
                    stale[field] = "different"
                reader.mutate[call] = stale
                with self.subTest(call=call, field=field):
                    self.assert_refused(data, reader)
        for call in (2, 5):
            data = metadata()
            reader = Reader(data)
            reader.mutate[call] = {"object": {"sha": "c" * 40}}
            with self.subTest(call=call):
                self.assert_refused(data, reader)

    def test_identity_inventory_reader_and_policy_refuse_with_constant_error(self):
        data = metadata()
        data["number"] = 2
        reader = Reader(data)
        self.assert_refused(data, reader)
        self.assertEqual(reader.calls, [])
        for count in (0, 3001, True):
            data = metadata()
            data["pull_request"]["changed_files"] = count
            self.assert_refused(data, Reader(data))
        for rows in ([], [{"status": "renamed", "filename": "docs/a.md"}],
                     [{"status": "modified", "filename": "../SENTINEL"}]):
            data = metadata()
            self.assert_refused(data, Reader(data, rows))
        for call in range(1, 6):
            data = metadata()
            reader = Reader(data)
            reader.mutate[call] = RuntimeError("SENTINEL")
            with self.subTest(call=call):
                self.assert_refused(data, reader)
        data = metadata()
        with mock.patch.object(policy, "evaluate", side_effect=RuntimeError("SENTINEL")):
            self.assert_refused(data, Reader(data))

    def test_policy_mutation_cannot_replace_baseline(self):
        data = metadata()
        reader = Reader(data)
        original = inventory.evaluate_metadata

        def mutate(event, repository, sha, paths, module):
            result = original(event, repository, sha, paths, module)
            event["pull_request"]["head"]["sha"] = "c" * 40
            return result

        with mock.patch.object(inventory, "evaluate_metadata", side_effect=mutate):
            self.assertEqual(evaluate(data, reader)["head_sha"], "b" * 40)
        self.assertEqual(data["pull_request"]["head"]["sha"], "b" * 40)


if __name__ == "__main__":
    unittest.main()
