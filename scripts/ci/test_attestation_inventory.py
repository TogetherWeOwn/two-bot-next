"""Focused inventory regressions adapted from the base attestation gate tests."""
import importlib.util
from pathlib import Path
import unittest


spec = importlib.util.spec_from_file_location(
    "attestation_inventory", Path(__file__).resolve().parents[2] / ".github/scripts/attestation_inventory.py"
)
inventory = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inventory)


class Inventory(unittest.TestCase):
    def test_bounded_pages_and_previous_paths(self):
        rows = [{"filename": f"src/{i}.rs", "status": "modified"} for i in range(150)]
        rows[0] = {"filename": "docs/new", "status": "renamed", "previous_filename": "src/old"}
        rows[1]["previous_filename"] = "src/copied"
        calls = []

        def api(route):
            calls.append(route)
            page = int(route.rsplit("=", 1)[1])
            return rows[(page - 1) * 100:page * 100]

        paths = inventory.changed_paths(api, "/pulls/1", 150)
        self.assertEqual(calls, ["/pulls/1/files?per_page=100&page=1", "/pulls/1/files?per_page=100&page=2"])
        self.assertEqual(paths, sorted({r["filename"] for r in rows} | {"src/old", "src/copied"}))

    def test_count_and_page_integrity(self):
        for count in (0, -1, True, 3001, "1", None):
            with self.subTest(count=count), self.assertRaises(inventory.InventoryError):
                inventory.changed_paths(lambda _: [], "/pulls/1", count)
        for page in (None, {}, [], [{"filename": "a", "status": "added"}] * 2):
            with self.subTest(page=page), self.assertRaises(inventory.InventoryError):
                inventory.changed_paths(lambda _: page, "/pulls/1", 1)
        duplicate = {"filename": "a", "status": "modified"}
        with self.assertRaises(inventory.InventoryError):
            inventory.changed_paths(lambda _: [duplicate, duplicate], "/pulls/1", 2)

    def test_invalid_entries_and_paths(self):
        for row in (None, {}, {"filename": "a", "status": []},
                    {"filename": "a", "status": "unknown"},
                    {"filename": "a", "status": "renamed"},
                    {"filename": "a", "status": "renamed", "previous_filename": "a"}):
            with self.subTest(row=row), self.assertRaises(inventory.InventoryError):
                inventory.changed_paths(lambda _: [row], "/pulls/1", 1)
        for path in ("", " a", "a\n", "a\x00b", "a\x7fb", "a\u0085b", "a b",
                     "a\\b", "/a", "a/../b", "./a", "a//b", "a/"):
            for field in ("filename", "previous_filename"):
                row = {"filename": "new", "status": "renamed", "previous_filename": "old"}
                row[field] = path
                with self.subTest(path=path, field=field), self.assertRaises(inventory.InventoryError):
                    inventory.changed_paths(lambda _: [row], "/pulls/1", 1)
        for path in (".github/workflows/ci.yml", "a b/c d", "dir/.hidden", "a..b", "unicode/é.rs"):
            self.assertEqual(inventory.inventory_path(path), path)


if __name__ == "__main__":
    unittest.main()
