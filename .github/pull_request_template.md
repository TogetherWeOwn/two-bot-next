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


## Notes for reviewers (optional)

<!-- Risks, follow-ups, or anything the reviewer should look at first. -->

- For vote-kick, template assistant or RSVP/attendance wiring: the applicable [security requirement IDs](https://github.com/TogetherWeOwn/two-bot-next/blob/main/docs/command-wiring-security.md) and exact-head regression tests.
