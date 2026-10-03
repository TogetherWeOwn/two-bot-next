<!--
Fill in every section. Use short, active sentences. Delete these comments before you submit.
Title: a Conventional Commits header, e.g. `fix(gateway): resume after a dropped session`.
Types: feat fix perf refactor test docs build ci chore revert style security. Max 100 chars, no trailing period.
Never put a secret, token, private URL, or internal card ID (TOG-, PAP-) in the title, body, commits, or branch name.
-->

## Thinking Path

<!--
Walk from the system down to this change, one short bullet per step, like this:
- This repo runs the Discord bot as one always-on container.
- The gateway keeps one session and resumes it after a drop.
- A resume after a long gap replays events the bot already handled.
- This PR drops replayed events older than the stored cursor.
- The benefit is one action per event, not two.
-->

-

## Linked Issues or Issue Description

<!--
Link the issue this PR closes or advances. If there is none, describe the problem here.
Closes #123  (a public GitHub issue), or write the problem here in two or three sentences.
-->

Closes #

## What Changed

<!-- List the changes. Name each file or script that matters, and say what it does now. -->

-

## Verification

<!--
Give the exact commands you ran and their results. Say what you did not run, and why.
Never claim a green run you did not see. For a bug fix, show the test that fails without the fix.
-->

-

## Risks

<!--
What can this break? Name credentials, permissions, hosts, or services it touches.
Name the rollback. Write "None" only if you checked.
-->

-

## Model Used

<!--
Name the provider and the exact model ID, plus the mode (reasoning effort, tool use) if it matters.
If a human wrote every line, write: None — human-authored.
-->

-

## Checklist

- [ ] The PR title is a Conventional Commits header
- [ ] I filled in every section above
- [ ] I ran the tests that cover this change and reported the results above
- [ ] I addressed every review finding, or replied with why it does not apply
- [ ] No secret, token, private URL, or internal card ID (TOG-, PAP-) is in the diff, the title, the body, the commits, or the branch name
- [ ] I credited the contributors whose work this builds on
- [ ] I updated the docs this change makes stale
