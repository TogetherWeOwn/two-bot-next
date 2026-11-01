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
        body = GOOD_BODY.replace("## Verification", "## Notes")
        self.assertIn(("error", "PR template"), levels(ps.evaluate(env(BODY=body))))
        self.assertIn(("warning", "PR template"), levels(ps.evaluate(env(BODY=body, PR_STANDARDS_MODE="warn"))))
        self.assertNotIn("PR template", [t for _, t in levels(ps.evaluate(env(BODY=GOOD_BODY)))])

    def test_optional_prose_sections_are_not_required(self):
        # Pipeline review 2026-10-10: only the linked card/issue and the test evidence are required.
        body = ("## Linked Issues or Issue Description\n\nRefs: TOG-1234\n\n"
                "## Verification\n\n- `npm test` passes: 41 passed, 0 failed\n")
        self.assertEqual([], ps.evaluate(env(BODY=body)))

    def test_bare_dash_section_is_empty(self):
        body = GOOD_BODY.replace("- `npm test` passes: 41 passed, 0 failed", "-")
        msgs = [f.message for f in ps.evaluate(env(BODY=body))]
        self.assertTrue(any("Verification" in m for m in msgs), msgs)

    def test_comment_blocks_do_not_satisfy_a_section(self):
        body = GOOD_BODY.replace("- `npm test` passes: 41 passed, 0 failed",
                                 "<!-- - `npm test` passes: 41 passed, 0 failed -->")
        msgs = [f.message for f in ps.evaluate(env(BODY=body))]
        self.assertTrue(any("Verification" in m for m in msgs), msgs)

    def test_light_types_skip_linked_issue(self):
        body = (GOOD_BODY.replace("## Linked Issues or Issue Description\n\nRefs: TOG-1234\n\n", "")
                .replace("- [x] I searched for duplicate or related PRs and linked them above", "- [ ] done")
                + "\nRefs: TOG-9\n")
        self.assertEqual([], ps.evaluate(env(TITLE="docs(readme): explain setup", BODY=body)))
        f = ps.evaluate(env(TITLE="fix(auth): refresh the key", BODY=body))
        self.assertIn(("error", "PR template"), levels(f))
        self.assertNotIn("Duplicate search", [t for _, t in levels(f)])

    def test_unticked_dedup_box_is_not_required(self):
        # Pipeline review 2026-10-10: the duplicate-search tick is optional prose now.
        body = GOOD_BODY.replace("- [x] I searched", "- [ ] I searched")
        self.assertEqual([], ps.evaluate(env(BODY=body)))

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

    def test_public_branch_ids_match_in_any_case_and_nested_paths(self):
        clean = GOOD_BODY.replace("Refs: TOG-1234", "Fixes #12")
        for ref in ("TOG-123-x", "tog-123-x", "Tog-123-x", "qa/tog-123-example", "test/pap-123-example",
                    "fix/Papa-7-slug", "feat/area/PAP-9-slug", "fix/slug-tog-5"):
            f = ps.evaluate(env(BODY=clean, REPO_PRIVATE="false", PR_STANDARDS_MODE="warn", HEAD_REF=ref))
            self.assertEqual([("warning", "Internal reference")], levels(f), ref)
            self.assertIn("branch name", f[0].message, ref)

    def test_public_branch_look_alikes_and_clean_slugs_pass(self):
        clean = GOOD_BODY.replace("Refs: TOG-1234", "Fixes #12")
        for ref in ("fix/refresh-signing-key", "feat/voice/tog-roster", "fix/pap-smear", "fix/stog-123",
                    "fix/utf-8-input", "chore/tog_123"):
            f = ps.evaluate(env(BODY=clean, REPO_PRIVATE="false", HEAD_REF=ref))
            self.assertEqual([], f, ref)

    def test_public_branch_all_fixed_patterns_follow_mode(self):
        # FIXED_INTERNAL is now consumed only by the branch/commit gates (title/body
        # go through strict_hits/other_hits), so the branch gate must pin every
        # non-ticket-id pattern. Each ref carries only its own pattern.
        clean = GOOD_BODY.replace("Refs: TOG-1234", "Fixes #12")
        refs = ("fix/x-agent://abc", "fix/x-(/TOG/issues/abc)", "fix/x-localhost-y",
                "fix/x-10.0.0.5-y", "fix/x-box.tail1234.ts.net-y",
                "fix/x-workforce.infextion.net-y")
        for mode, level in (("warn", "warning"), ("error", "error")):
            for ref in refs:
                f = ps.evaluate(env(BODY=clean, REPO_PRIVATE="false",
                                    PR_STANDARDS_MODE=mode, HEAD_REF=ref))
                self.assertEqual([(level, "Internal reference")], levels(f), (mode, ref))
                self.assertIn("branch name", f[0].message, (mode, ref))

    def test_public_title_body_strict_subset_always_errors(self):
        # Tracker id, instance UI link and internal host fail the title/body even in warn mode.
        # Each fixture carries only its own pattern, so demoting one strict pattern fails this test.
        # Hostnames are case-insensitive and instance links stay links inside inline code.
        clean = GOOD_BODY.replace("Refs: TOG-1234", "Fixes #12")
        for leak in ("TOG-123", "(/TOG/issues/abc)", "`/TOG/issues/abc`",
                     "https://workforce.infextion.net/x", "https://WORKFORCE.INFEXTION.NET/x"):
            for where in ("TITLE", "BODY"):
                body = clean if where == "TITLE" else GOOD_BODY.replace("Refs: TOG-1234", leak)
                title = "fix(auth): refresh the key " + leak if where == "TITLE" else "fix(auth): refresh the key"
                f = ps.evaluate(env(TITLE=title, BODY=body, REPO_PRIVATE="false",
                                    PR_STANDARDS_MODE="warn"))
                self.assertIn(("error", "Internal reference"), levels(f), (where, leak))

    def test_public_title_body_advisory_subset_follows_mode(self):
        # Agent links, localhost, private nets and tailnet names stay advisory in warn mode
        for leak in ("agent://abc", "http://localhost:3100/x", "http://10.0.0.5/x",
                     "https://box.tail1234.ts.net/x"):
            body = GOOD_BODY.replace("Refs: TOG-1234", leak)
            f = ps.evaluate(env(BODY=body, REPO_PRIVATE="false", PR_STANDARDS_MODE="warn"))
            self.assertEqual([("warning", "Internal reference")], levels(f), leak)
            f = ps.evaluate(env(BODY=body, REPO_PRIVATE="false", PR_STANDARDS_MODE="error"))
            self.assertEqual([("error", "Internal reference")], levels(f), leak)

    def test_private_repo_allows_lowercase_ids_in_branch(self):
        self.assertEqual([], ps.evaluate(env(HEAD_REF="qa/tog-123-example")))

    def test_public_prose_ticket_ids_stay_case_sensitive(self):
        body = GOOD_BODY.replace("Refs: TOG-1234", "Fixes #12. The tog-123 spelling is plain text here.")
        self.assertEqual([], ps.evaluate(env(BODY=body, REPO_PRIVATE="false")))

    def test_public_repo_catches_instance_links_and_hosts(self):
        for leak in ("(/TOG/issues/abc)", "agent://abc", "http://localhost:3100/x", "http://10.0.0.5/x",
                     "http://192.168.1.4", "https://box.tail1234.ts.net/x", "https://workforce.infextion.net/x"):
            body = GOOD_BODY.replace("Refs: TOG-1234", leak)
            f = ps.evaluate(env(BODY=body, REPO_PRIVATE="false"))
            self.assertIn(("error", "Internal reference"), levels(f), leak)

    def test_public_repo_does_not_flag_look_alikes(self):
        body = GOOD_BODY.replace("Refs: TOG-1234", "Fixes #12. Mentions CVE-2026-1234, UTF-8, SHA-256, GPT-4 and 172.32.0.1.")
        self.assertEqual([], ps.evaluate(env(BODY=body, REPO_PRIVATE="false")))

    def test_html_comments_are_not_scanned_for_advisory(self):
        body = GOOD_BODY.replace("Refs: TOG-1234", "Fixes #12\n<!-- do not write localhost here -->")
        self.assertEqual([], ps.evaluate(env(BODY=body, REPO_PRIVATE="false")))

    def test_strict_hits_inside_html_comments_still_fail(self):
        # Comment text is still stored and served, so the fail-closed subset scans raw text.
        for leak in ("TOG-123", "(/TOG/issues/abc)", "https://workforce.infextion.net/x"):
            body = GOOD_BODY.replace("Refs: TOG-1234", f"Fixes #12\n<!-- hidden {leak} -->")
            self.assertIn(("error", "Internal reference"),
                          levels(ps.evaluate(env(BODY=body, REPO_PRIVATE="false", PR_STANDARDS_MODE="warn"))),
                          leak)

    def test_prefixes_are_configurable(self):
        body = GOOD_BODY.replace("Refs: TOG-1234", "Refs ACME-77")
        self.assertEqual([], ps.evaluate(env(BODY=body, REPO_PRIVATE="false")))
        f = ps.evaluate(env(BODY=body, REPO_PRIVATE="false", INTERNAL_ID_PREFIXES="ACME"))
        self.assertIn(("error", "Internal reference"), levels(f))


FULL_ATTESTATION = """
## Attestation

risk_tier: T0

### Claims

- C1: rollback refuses an unqualified DELETE

### Tests

- C1 | test=scripts/test_rollback.py::test_refuses_bare_delete | failed_before=not-run | proof=python3 scripts/test_rollback.py 5 OK

### High-risk checklist

- [x] D1: no secret sinks; redacted evidence only
- [x] D2: no auth paths changed
- [x] D3: no migrations shipped
- [x] D4: bounded inputs and caps
- [x] D5: no concurrency hazard
- [x] D6: pinned actions only
- [x] D7: failures propagate honestly
- [x] D8: malformed shapes covered
"""


class AttestationFloor(unittest.TestCase):
    T0_FILES = ".github/workflows/deploy-production.yml"

    def test_t0_path_without_attestation_fails(self):
        f = ps.evaluate(env(BODY=GOOD_BODY, CHANGED_FILES=self.T0_FILES))
        self.assertIn(("error", "Attestation"), levels(f))

    def test_t0_path_with_full_attestation_passes(self):
        f = ps.evaluate(env(BODY=GOOD_BODY + FULL_ATTESTATION, CHANGED_FILES=self.T0_FILES))
        self.assertEqual([], f)

    def test_t1_path_without_attestation_passes(self):
        body = GOOD_BODY
        for changed in ("src/main.rs\ndocs/guide.md", ""):
            self.assertEqual([], ps.evaluate(env(BODY=body, CHANGED_FILES=changed)))

    def test_leftover_placeholders_fail(self):
        base = GOOD_BODY + FULL_ATTESTATION
        bodies = [
            base.replace("- [x] D1: no secret sinks; redacted evidence only",
                         "- [x] D1: <secret sinks, redaction and log/artifact exposure>"),
            base.replace("risk_tier: T0", "risk_tier: <T0|T1|T2>"),
            base.replace("- C1: rollback refuses an unqualified DELETE",
                         "- C1: <behavior guaranteed by this change>"),
        ]
        for body in bodies:
            f = ps.evaluate(env(BODY=body, CHANGED_FILES=self.T0_FILES))
            self.assertIn(("error", "Attestation"), levels(f), body[-200:])

    def test_rename_from_a_t0_path_counts(self):
        # Renames carry both paths; the old T0 path keeps the floor at T0.
        # Deleting a T0 file is still T0.
        for changed in ("scripts/renamed.py\ncrates/cutover/src/parse.rs",
                        "migrations/0001_init.sql"):
            f = ps.evaluate(env(BODY=GOOD_BODY, CHANGED_FILES=changed))
            self.assertIn(("error", "Attestation"), levels(f), changed)

    def test_t0_gate_is_an_error_in_warn_mode(self):
        f = ps.evaluate(env(BODY=GOOD_BODY, CHANGED_FILES=self.T0_FILES,
                            PR_STANDARDS_MODE="warn"))
        self.assertIn(("error", "Attestation"), levels(f))

    def test_understated_tier_fails_and_higher_tier_is_fine(self):
        base = GOOD_BODY + FULL_ATTESTATION
        f = ps.evaluate(env(BODY=base.replace("risk_tier: T0", "risk_tier: T1"),
                            CHANGED_FILES=self.T0_FILES))
        self.assertIn(("error", "Attestation"), levels(f))
        # Declaring T0 on a T1 path is allowed.
        self.assertEqual([], ps.evaluate(env(BODY=base, CHANGED_FILES="src/main.rs")))

    def test_t0_path_matching_follows_the_template(self):
        t0 = ("migrations/0001.sql", "sql/web_v1.sql", "db/schema.sql",
              "src/auth.rs", "crates/core/src/session_controller.rs",
              "crates/cutover/src/parse.rs", ".github/workflows/deploy-production.yml",
              "wrangler.toml", "config/wrangler.production.json",
              "migrations/notes.md")
        for path in t0:
            self.assertTrue(ps.is_t0_path(path), path)
        not_t0 = ("src/main.rs", "src/authors.py", "crates/discord/src/voices.rs",
                  "src/roleplay.ts", "docs/auth-guide.md", "docs/wrangler.md",
                  ".github/workflows/check.yml", "README.md")
        for path in not_t0:
            self.assertFalse(ps.is_t0_path(path), path)


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

    def test_public_history_all_fixed_patterns_follow_mode(self):
        # Same pin as the branch gate: every non-ticket-id entry in FIXED_INTERNAL
        # must warn/error on push. Each message carries only its own pattern.
        leaks = ("agent://abc", "(/TOG/issues/abc)", "http://localhost:3100/x",
                 "http://10.0.0.5/x", "https://box.tail1234.ts.net/x",
                 "https://workforce.infextion.net/x")
        for leak in leaks:
            c = self.commits(f"fix(auth): refresh the key (#12)\n\nSee {leak}")
            for mode, level in (("warn", "warning"), ("error", "error")):
                f = ps.evaluate(env(EVENT="push", COMMITS=c, REPO_PRIVATE="false",
                                    PR_STANDARDS_MODE=mode))
                self.assertEqual([(level, "Internal reference")], levels(f), (mode, leak))

    def test_garbage_commits_json_does_not_crash(self):
        self.assertEqual([], ps.evaluate(env(EVENT="push", COMMITS="not json")))

    def test_bot_long_title_is_a_warning_not_an_error(self):
        long_title = ("ci(deps): bump dtolnay/rust-toolchain from "
                      "7e38f4b43b4db5c8dd498af069a4f6196df1d067 to "
                      "e2a55d2ffb04f378e9626c28d38b36d230d1e12f (#808)")
        self.assertGreater(len(long_title), 100)
        commits = json.dumps([{"id": "0" * 40, "message": long_title,
                               "author": {"username": "dependabot[bot]", "name": "dependabot[bot]"}}])
        f = ps.evaluate(env(EVENT="push", COMMITS=commits))
        self.assertEqual([("warning", "Commit on main")], levels(f))
        # Same long title from a human is still an error.
        commits = json.dumps([{"id": "0" * 40, "message": long_title}])
        f = ps.evaluate(env(EVENT="push", COMMITS=commits))
        self.assertEqual([("error", "Commit on main")], levels(f))


class Template(unittest.TestCase):
    """The shipped template and the gate must not drift apart."""

    def setUp(self):
        with open(TEMPLATE, encoding="utf-8") as fh:
            self.text = fh.read()

    def test_every_required_heading_is_in_the_template(self):
        sections = ps.parse_sections(self.text)
        for heading, _, _ in ps.REQUIRED_SECTIONS:
            self.assertIn(heading.lower(), sections, heading)

    def test_unfilled_template_fails_in_error_mode(self):
        f = ps.evaluate(env(BODY=self.text, PR_STANDARDS_MODE="error"))
        self.assertTrue(any(x.level == "error" and x.title == "PR template" for x in f), f)

    def test_template_has_no_internal_references_outside_comments(self):
        self.assertEqual([], ps.internal_hits(self.text, "TOG|PAP|PAPA"))
        # The strict subset scans raw text (including comments), so the shipped
        # template must carry no strict hit at all.
        self.assertEqual([], ps.strict_hits(self.text, "TOG|PAP|PAPA"))


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
        rc, out = self.run_main(env(BODY=GOOD_BODY.replace("## Verification", "## Notes"), PR_STANDARDS_MODE="warn"))
        self.assertEqual(0, rc)
        self.assertIn("::warning title=PR template::", out)

    def test_annotation_messages_are_escaped(self):
        self.assertEqual("a%25b%0Ac", ps._escape("a%b\nc"))
        _, out = self.run_main(env(TITLE="bad\nTitle"))
        self.assertEqual(1, len(re.findall(r"^::", out, re.M)))


if __name__ == "__main__":
    unittest.main()
