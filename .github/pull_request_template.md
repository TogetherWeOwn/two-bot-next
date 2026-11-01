<!--
Title: Conventional Commits header, e.g. `fix(auth): refuse expired sudo sessions`.
Types: feat fix perf refactor test docs build ci chore revert style security. Max 100 chars, no trailing period.
This repo is public. Do not put internal ticket ids, instance links, localhost or private-network
addresses in the title, the body, a commit message or the branch name. Name the branch after the change,
for example `fix/sudo-window`. Link only public GitHub issues (`Fixes #123`).
Write short, plain sentences. `pr-lint` requires the title, the linked issue (feat/fix/perf/refactor/security)
and Verification; the other sections are optional context for the reviewer.
Full rules: CONTRIBUTING.md, "Pull request standards".
-->

## Linked Issues or Issue Description

<!--
  Required for feat, fix, perf, refactor and security. Pick ONE:
  (A) A public GitHub issue exists: `Fixes #123`, `Closes #123` or `Refs #123`.
  (B) No issue exists: describe the problem here in your own words (what happened, what you expected,
      how to reproduce it, or the motivation and proposed solution for a feature).
-->

-

## What Changed (optional)

<!-- One bullet per logical unit. The reviewer reads the diff; add only what the diff does not show. -->

## Verification

<!-- Commands you ran and their results, so a reviewer can repeat them. -->

-


## Attestation

<!-- SHADOW / WARN only. Fill every field; retain the exact heading/field/bullet syntax.
T0 = critical paths, T1 = standard changes, T2 = docs-only. Declaring a higher risk is OK.
T0 directories at any depth: migrations, sql, db, database, drizzle.
T0 directory/module words: auth, authn, authz, authentication, authorization, oauth,
session, sessions, billing, payment, payments, permission, permissions,
gateway, voice, crypto, vault, secret, secrets,
backup, backups, dump, dumps, grant, grants, role, roles, erasure, ingest.
Words are case-insensitive and split at punctuation, snake_case, kebab-case, and CamelCase
boundaries (auth.rs, gateway_backoff.rs, member-erasure.ts, SessionController.php).
The words also match directory names/descendants. Prose filenames ending .md/.rst/.txt
are not modules, but still inherit any critical directory floor. Substrings such as
names in authors.py, voices/, and roleplay.ts do not match those critical words.
Also T0: .github/workflows/deploy*, crates/cutover/**, and wrangler .toml/.json/.jsonc
configs at any depth (including wrangler.production.toml); prose like docs/wrangler.md
is not a config. Deploy scripts outside these paths still require a declared T0 when critical.
Renames inspect both old and new paths; deleting a T0 file is still T0.
T2 requires only root or docs/ .md/.rst files or explicitly allowlisted text basenames:
README.txt, LICENSE.txt, NOTICE.txt, CHANGELOG.txt, CONTRIBUTING.txt, CODE_OF_CONDUCT.txt,
SECURITY.txt, SUPPORT.txt, AUTHORS.txt (case-insensitive). Critical directory floors win.
Never T2: .github/ files, code, arbitrary .txt, CMakeLists.txt, or requirements/constraints
manifests (including requirements.txt, requirements-dev.txt, and docs/requirements.txt).
Path names are only a risk floor, not proof that other code is non-critical; declare T0
for critical behavior even when its path does not match. Ingest is conservatively T0
because filenames cannot prove whether a body route requires authentication.

Claims: one behavior claim per C-number. For docs, state the documentation-only claim.
Tests: one line per claim/test mapping; repository-relative path, optional ::test_name.
Every claimed test file must be added/modified/renamed in this PR, not removed or merely present
elsewhere in the repository. Inline tests in source files are allowed. Existing unchanged tests
belong in Verification, not this field. Duplicate mapping lines for multiple tests/claims.
failed_before=yes|no|not-run describes what you actually observed. No invented red-before proof.
proof= names the command/result/mutant or other evidence; never executes from this text.
Only T2 may replace all test lines with: - none | reason=Documentation-only; no behavior changed.
The validator checks shape and changed-file references, NOT test execution or truth of claims.

High-risk checklist: keep D1-D8. Replace each placeholder with evidence or
not-applicable plus a concrete reason. For T0 all boxes must be checked; never blindly check them.
Use plain one-line text (no | delimiter inside test fields). Keep attestation unfenced and visible.
-->

risk_tier: <T0|T1|T2>

### Claims

- C1: <behavior guaranteed by this change>

### Tests

- C1 | test=<path/to/test_file>::<test_name> | failed_before=not-run | proof=<command and observed result>

### High-risk checklist

- [ ] D1: <secret sinks, redaction and log/artifact exposure>
- [ ] D2: <authentication, authorization and cross-principal/oracle cases>
- [ ] D3: <migrations, grants, restore, erasure and state integrity>
- [ ] D4: <input bounds, query limits, retries and resource caps>
- [ ] D5: <concurrency, lost writes, replay and single-use effects>
- [ ] D6: <pinned dependencies/actions and test/guard wiring>
- [ ] D7: <failure propagation, non-empty evidence and honest verdicts>
- [ ] D8: <malformed shapes, Unicode, parsing and host validation>

## Notes for reviewers (optional)

<!-- Risks, follow-ups, or anything the reviewer should look at first. -->

- For vote-kick, template assistant, RSVP/attendance or slash-command input-handling wiring: the applicable [security requirement IDs](https://github.com/TogetherWeOwn/two-bot-next/blob/main/docs/command-wiring-security.md) and exact-head regression tests.
