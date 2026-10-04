#!/usr/bin/env python3
"""Tests for pr_standards.py. Run: python3 -m unittest discover -s .github/scripts -p 'test_pr_standards.py'"""
import io
import json
import os
import re
import unittest
from contextlib import redirect_stdout

import pr_standards as ps

HERE = os.path.dirname(os.path.abspath(__file__))
TEMPLATE = os.path.join(HERE, "..", "pull_request_template.md")

GOOD_BODY = """
## Thinking Path

> - The service signs every request it forwards
> - A stale signing key makes the forwarded request fail
> - The key was never refreshed after rotation
> - This pull request refreshes the key when a signature check fails

## Linked Issues or Issue Description

Refs: TOG-1234

## What Changed

- Refresh the signing key on a failed signature check

## Verification

- `npm test` passes: 41 passed, 0 failed

## Risks

- Low risk. One extra key fetch on the failure path only.

## Model Used

- Example Model 1.0, 200k context, tool use

## Checklist

- [x] I searched for duplicate or related PRs and linked them above
"""


def env(**kw):
    base = {"EVENT": "pull_request", "TITLE": "fix(auth): refresh the signing key", "BODY": GOOD_BODY,
            "AUTHOR": "agent-one", "HEAD_REF": "fix/refresh-signing-key", "REPO_PRIVATE": "true",
            "REQUIRE_CARD_REF": "true", "PR_STANDARDS_MODE": "error"}
    base.update(kw)
    return base


def levels(findings):
    return sorted((f.level, f.title) for f in findings)


class Title(unittest.TestCase):
    def test_good_titles(self):
        for t in ("fix(auth): refuse expired sudo sessions", "feat: add thing", "chore(deps)!: bump x",
                  "docs(readme): explain setup"):
            self.assertEqual([], ps.header_errors(t), t)

    def test_bad_titles(self):
        for t in ("TOG-12: fix the thing", "fix auth", "Fix(auth): x", "fix(auth): done.", "fix(auth): " + "x" * 100,
                  "fix(auth):x", "wip: stuff"):
            self.assertNotEqual([], ps.header_errors(t), t)

    def test_bad_title_is_always_an_error_even_in_warn_mode(self):
        f = ps.evaluate(env(TITLE="fix auth", PR_STANDARDS_MODE="warn"))
        self.assertIn(("error", "PR title"), levels(f))


class Body(unittest.TestCase):
    def test_good_body_passes(self):
        self.assertEqual([], ps.evaluate(env()))

    def test_empty_body_is_an_error_in_every_mode(self):
        for mode in ("warn", "error"):
            f = ps.evaluate(env(BODY="<!-- nothing -->  ", PR_STANDARDS_MODE=mode))
            self.assertIn(("error", "PR body"), levels(f))

    def test_missing_section_follows_mode(self):
        body = GOOD_BODY.replace("## Risks", "## Notes")
        self.assertIn(("error", "PR template"), levels(ps.evaluate(env(BODY=body))))
        self.assertIn(("warning", "PR template"), levels(ps.evaluate(env(BODY=body, PR_STANDARDS_MODE="warn"))))
        self.assertNotIn("PR template", [t for _, t in levels(ps.evaluate(env(BODY=GOOD_BODY)))])

    def test_placeholder_thinking_path_does_not_count(self):
        thin = GOOD_BODY.split("## Linked")[0]
        placeholder = ("## Thinking Path\n\n> - This repo does a thing\n> - [Which subsystem is involved]\n"
                       "> - [What problem exists]\n> - This pull request ...\n> - The benefit is ...\n\n")
        body = GOOD_BODY.replace(thin, placeholder)
        msgs = [f.message for f in ps.evaluate(env(BODY=body))]
        self.assertTrue(any("Thinking Path" in m and "at least 3" in m for m in msgs), msgs)

    def test_bare_dash_section_is_empty(self):
        body = GOOD_BODY.replace("- Low risk. One extra key fetch on the failure path only.", "-")
        msgs = [f.message for f in ps.evaluate(env(BODY=body))]
        self.assertTrue(any("Risks" in m for m in msgs), msgs)

    def test_model_placeholder_rejected_and_human_authored_accepted(self):
        bad = GOOD_BODY.replace("- Example Model 1.0, 200k context, tool use", "- <model>")
        self.assertTrue(any("Model Used" in f.message for f in ps.evaluate(env(BODY=bad))))
        ok = GOOD_BODY.replace("- Example Model 1.0, 200k context, tool use", "- None - human-authored")
        self.assertEqual([], ps.evaluate(env(BODY=ok)))

    def test_comment_blocks_do_not_satisfy_a_section(self):
        body = GOOD_BODY.replace("- Refresh the signing key on a failed signature check",
                                 "<!-- - Refresh the signing key on a failed signature check -->")
        msgs = [f.message for f in ps.evaluate(env(BODY=body))]
        self.assertTrue(any("What Changed" in m for m in msgs), msgs)

    def test_light_types_skip_linked_issue_and_dedup_tick(self):
        body = (GOOD_BODY.replace("## Linked Issues or Issue Description\n\nRefs: TOG-1234\n\n", "")
                .replace("- [x] I searched for duplicate or related PRs and linked them above", "- [ ] done")
                + "\nRefs: TOG-9\n")
        self.assertEqual([], ps.evaluate(env(TITLE="docs(readme): explain setup", BODY=body)))
        f = ps.evaluate(env(TITLE="fix(auth): refresh the key", BODY=body))
        self.assertIn(("error", "PR template"), levels(f))
        self.assertIn(("error", "Duplicate search"), levels(f))

    def test_unticked_dedup_box_fails_for_code_types(self):
        body = GOOD_BODY.replace("- [x] I searched", "- [ ] I searched")
        self.assertIn(("error", "Duplicate search"), levels(ps.evaluate(env(BODY=body))))

    def test_bots_and_generated_prs_are_exempt(self):
        f = ps.evaluate(env(AUTHOR="dependabot[bot]", TITLE="chore(deps): bump x", BODY="Bumps x."))
        self.assertEqual([], f)
        f = ps.evaluate(env(AUTHOR="dependabot[bot]", TITLE="Bump x from 1 to 2", BODY="Bumps x."))
        self.assertEqual([("warning", "PR title")], levels(f))
        f = ps.evaluate(env(TITLE="chore(main): release 1.2.0", HEAD_REF="release-please--branches--main",
                            BODY="Automated release PR with the changelog for this version."))
        self.assertEqual([], f)


class CardAndInternalRefs(unittest.TestCase):
    def test_private_repo_requires_card_ref_only_when_asked(self):
        body = GOOD_BODY.replace("Refs: TOG-1234", "Fixes #12")
        self.assertIn(("error", "Card reference"), levels(ps.evaluate(env(BODY=body))))
        self.assertIn(("warning", "Card reference"), levels(ps.evaluate(env(BODY=body, REQUIRE_CARD_REF="false"))))

    def test_public_repo_forbids_internal_refs_in_title_body_and_branch(self):
        pub = dict(REPO_PRIVATE="false")
        self.assertEqual([], ps.evaluate(env(BODY=GOOD_BODY.replace("Refs: TOG-1234", "Fixes #12"), **pub)))
        for k, v in (("TITLE", "fix(auth): TOG-123 refresh the key"), ("HEAD_REF", "TOG-123-refresh-key"),
                     ("BODY", GOOD_BODY)):
            f = ps.evaluate(env(**{k: v}, **pub))
            self.assertIn(("error", "Internal reference"), levels(f), k)

    def test_public_repo_mode_controls_level(self):
        clean = GOOD_BODY.replace("Refs: TOG-1234", "Fixes #12")
        for mode, level in (("warn", "warning"), ("error", "error")):
            f = ps.evaluate(env(BODY=clean, REPO_PRIVATE="false", PR_STANDARDS_MODE=mode, HEAD_REF="TOG-9-x"))
            self.assertEqual([(level, "Internal reference")], levels(f), mode)

    def test_public_repo_catches_instance_links_and_hosts(self):
        for leak in ("see /TOG/issues/TOG-1", "agent://abc", "http://localhost:3100/x", "http://10.0.0.5/x",
                     "http://192.168.1.4", "https://box.tail1234.ts.net/x", "https://workforce.infextion.net/x"):
            body = GOOD_BODY.replace("Refs: TOG-1234", leak)
            f = ps.evaluate(env(BODY=body, REPO_PRIVATE="false"))
            self.assertIn(("error", "Internal reference"), levels(f), leak)

    def test_public_repo_does_not_flag_look_alikes(self):
        body = GOOD_BODY.replace("Refs: TOG-1234", "Fixes #12. Mentions CVE-2026-1234, UTF-8, SHA-256, GPT-4 and 172.32.0.1.")
        self.assertEqual([], ps.evaluate(env(BODY=body, REPO_PRIVATE="false")))

    def test_html_comments_are_not_scanned(self):
        body = GOOD_BODY.replace("Refs: TOG-1234", "Fixes #12\n<!-- do not write TOG-123 or localhost here -->")
        self.assertEqual([], ps.evaluate(env(BODY=body, REPO_PRIVATE="false")))

    def test_prefixes_are_configurable(self):
        body = GOOD_BODY.replace("Refs: TOG-1234", "Refs ACME-77")
        self.assertEqual([], ps.evaluate(env(BODY=body, REPO_PRIVATE="false")))
        f = ps.evaluate(env(BODY=body, REPO_PRIVATE="false", INTERNAL_ID_PREFIXES="ACME"))
        self.assertIn(("error", "Internal reference"), levels(f))


class Push(unittest.TestCase):
    def commits(self, *msgs):
        return json.dumps([{"id": f"{i:040x}", "message": m} for i, m in enumerate(msgs)])

    def test_conventional_squash_commit_passes(self):
        f = ps.evaluate(env(EVENT="push", COMMITS=self.commits("fix(auth): refresh the key (#12)")))
        self.assertEqual([], f)

    def test_bypass_commit_is_an_error_and_merges_are_skipped(self):
        f = ps.evaluate(env(EVENT="push", COMMITS=self.commits("Update stuff", "Merge branch 'x'", 'Revert "y"')))
        self.assertEqual([("error", "Commit on main")], levels(f))

    def test_public_history_internal_refs_follow_mode(self):
        c = self.commits("fix(auth): refresh the key (#12)\n\nRefs: TOG-1234")
        self.assertEqual([("error", "Internal reference")],
                         levels(ps.evaluate(env(EVENT="push", COMMITS=c, REPO_PRIVATE="false"))))
        self.assertEqual([], ps.evaluate(env(EVENT="push", COMMITS=c, REPO_PRIVATE="true")))

    def test_garbage_commits_json_does_not_crash(self):
        self.assertEqual([], ps.evaluate(env(EVENT="push", COMMITS="not json")))


class Template(unittest.TestCase):
    """The shipped template and the gate must not drift apart."""

    def setUp(self):
        with open(TEMPLATE, encoding="utf-8") as fh:
            self.text = fh.read()

    def test_every_required_heading_is_in_the_template(self):
        sections = ps.parse_sections(self.text)
        for heading, _, _ in ps.REQUIRED_SECTIONS:
            self.assertIn(heading.lower(), sections, heading)

    def test_template_carries_the_dedup_checkbox(self):
        self.assertIsNotNone(ps.DEDUP_CHECKBOX.search(ps.strip_comments(self.text)))

    def test_unfilled_template_fails_in_error_mode(self):
        f = ps.evaluate(env(BODY=self.text, PR_STANDARDS_MODE="error"))
        self.assertTrue(any(x.level == "error" and x.title in ("PR template", "Duplicate search") for x in f), f)

    def test_template_has_no_internal_references_outside_comments(self):
        self.assertEqual([], ps.internal_hits(self.text, "TOG|PAP|PAPA"))


class Main(unittest.TestCase):
    def run_main(self, e):
        buf = io.StringIO()
        with redirect_stdout(buf):
            rc = ps.main(e)
        return rc, buf.getvalue()

    def test_exit_code_follows_errors_only(self):
        self.assertEqual(0, self.run_main(env())[0])
        rc, out = self.run_main(env(TITLE="nope"))
        self.assertEqual(1, rc)
        self.assertTrue(out.startswith("::error title=PR title::"), out)
        rc, out = self.run_main(env(BODY=GOOD_BODY.replace("## Risks", "## Notes"), PR_STANDARDS_MODE="warn"))
        self.assertEqual(0, rc)
        self.assertIn("::warning title=PR template::", out)

    def test_annotation_messages_are_escaped(self):
        self.assertEqual("a%25b%0Ac", ps._escape("a%b\nc"))
        _, out = self.run_main(env(TITLE="bad\nTitle"))
        self.assertEqual(1, len(re.findall(r"^::", out, re.M)))


if __name__ == "__main__":
    unittest.main()
