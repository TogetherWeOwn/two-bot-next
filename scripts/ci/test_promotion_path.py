from copy import deepcopy
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
from urllib.parse import unquote

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[2] / ".github/scripts"), *sys.path]):
    from promotion_path import InventoryError, LABELS, MAX_FILES, promotion_path, update


REPO = "example/bot"
YES, NO = LABELS


def row(path, status="modified", **extra):
    return {"filename": path, "status": status, **extra}


class FixtureAPI:
    def __init__(self, *rows):
        self.rows = list(rows)
        self.pr = {"number": 12, "state": "open", "changed_files": len(rows), "updated_at": "before",
                   "base": {"ref": "main", "sha": "b" * 40, "repo": {"full_name": REPO}},
                   "head": {"sha": "a" * 40}}
        self.refs = {"main": "b" * 40, "release/next": "c" * 40}
        self.definitions = set(LABELS)
        self.labels = {"keep-me", NO}
        self.writes = []
        self.pages = []
        self.on_page = self.on_write = lambda: None

    def __call__(self, method, route, body=None, *, tolerate=()):
        suffix = unquote(route.removeprefix(f"/repos/{REPO}"))
        if method != "GET":
            self.writes.append((method, suffix))
            if (method, suffix) == ("POST", "/labels"):
                self.definitions.add(body["name"])
                return None
            if (method, suffix) == ("POST", "/issues/12/labels"):
                self.labels.update(body["labels"])
            elif method == "DELETE" and suffix.startswith("/issues/12/labels/"):
                self.labels.discard(suffix.rsplit("/", 1)[-1])
            else:
                raise AssertionError((method, suffix))
            self.pr["updated_at"] = "label-write"
            self.on_write()
        elif suffix == "/pulls/12":
            return deepcopy(dict(self.pr, labels=[{"name": name} for name in self.labels]))
        elif suffix.startswith("/git/ref/heads/"):
            return {"object": {"sha": self.refs[suffix.removeprefix("/git/ref/heads/")]}}
        elif suffix.startswith("/pulls/12/files?per_page=100&page="):
            page = int(suffix.split("=")[-1])
            self.pages.append(page)
            self.on_page()
            return self.rows[(page - 1) * 100:page * 100]
        elif suffix.startswith("/labels/"):
            name = suffix.rsplit("/", 1)[-1]
            if name in self.definitions:
                return {"name": name}
            if 404 not in tolerate:
                raise AssertionError("Missing repository label definition")
        else:
            raise AssertionError((method, suffix))

    def run(self):
        return update(self, REPO, 12)


class PromotionPathTests(unittest.TestCase):
    def test_bot_policy_paths(self):
        for path in (".github/workflows/check.yml", ".github/actions/build/action.yml", "Dockerfile",
                     "deploy/two-bot-next-backup.service", "nested/worker/wrangler.production.jsonc",
                     "scripts/bot_promotion.py", "scripts/worker_container_rollback.py",
                     "crates/cutover/src/bin/staging_migrate.rs", "crates/store/src/migrations.rs",
                     "crates/cutover/migrations/1000_x.sql", "crates/store/migrations/0001_x.sql", "migrations.lock"):
            self.assertTrue(promotion_path(path), path)
        for path in ("docs/production-deploy.md", "docs/wrangler.md", "crates/bot/src/help.rs",
                     "crates/store/tests/migrations.rs", "scripts/test_bot_promotion_copy_adapter.py",
                     "scripts/ci/flake_ledger.py", "wrangler/src/redirect.ts", "crates/fake/migrations/1.sql"):
            self.assertFalse(promotion_path(path), path)

    def test_rename_in_both_directions_and_deletion(self):
        for change in (row("notes.txt", "renamed", previous_filename="migrations.lock"),
                       row("migrations.lock", "renamed", previous_filename="notes.txt"),
                       row("crates/store/migrations/1.sql", "removed")):
            with self.subTest(change=change):
                self.assertEqual(FixtureAPI(change).run()[-1], YES)

    def test_label_flip_retarget_creation_and_idempotence(self):
        api = FixtureAPI(row("Dockerfile"))
        api.definitions.clear()
        self.assertEqual(api.run(), ("a" * 40, "b" * 40, "main", YES))
        self.assertEqual(api.labels, {"keep-me", YES})
        self.assertEqual(api.writes[0], ("POST", "/labels"))
        api.pr["base"].update(ref="release/next", sha="c" * 40)
        api.rows = [row("docs/notes.md")]
        self.assertEqual(api.run(), ("a" * 40, "c" * 40, "release/next", NO))
        self.assertEqual(api.labels, {"keep-me", NO})
        api.writes.clear()
        self.assertEqual(api.run()[-1], NO)
        self.assertEqual(api.writes, [])

    def test_all_pages_used_and_truncation_refuses(self):
        api = FixtureAPI(*(row(f"docs/{i}.md") for i in range(100)), row("Dockerfile"))
        self.assertEqual(api.run()[-1], YES)
        self.assertEqual(api.pages, [1, 2])
        api.rows.pop()
        with self.assertRaises(InventoryError):
            api.run()
        self.assertEqual(api.labels, {"keep-me", YES})

    def test_incomplete_malformed_or_capped_never_publishes(self):
        cases = ((MAX_FILES, [row("README.md")]), (0, []),
                 (1, [row("README.md", "renamed")]),
                 (2, [row("README.md"), row("README.md")]), (1, []))
        for count, rows in cases:
            with self.subTest(count=count, rows=rows):
                api = FixtureAPI(*rows)
                api.pr["changed_files"] = count
                with self.assertRaises(InventoryError):
                    api.run()
                self.assertEqual(api.writes, [])

    def test_head_and_base_moves_refuse_publication(self):
        api = FixtureAPI(row("Dockerfile"))
        api.on_page = lambda: api.pr["head"].update(sha="d" * 40)
        with self.assertRaises(InventoryError):
            api.run()
        self.assertEqual(api.writes, [])
        api.on_page = lambda: api.refs.update(main="e" * 40)
        with self.assertRaises(InventoryError):
            api.run()
        self.assertEqual(api.writes, [])

    def test_change_during_label_write_returns_no_current_evidence(self):
        api = FixtureAPI(row("README.md"))
        api.labels = {"keep-me", YES}
        api.on_write = lambda: api.pr["head"].update(sha="d" * 40)
        with self.assertRaises(InventoryError):
            api.run()
        self.assertIn("keep-me", api.labels)


if __name__ == "__main__":
    unittest.main()
