# Current-state plan — November 2026

**Snapshot date:** 2026-11-10 (UTC)
**How to read this file:** every item is marked proposed, approved,
implemented or verified. Accepted means the specification is accepted;
it does not mean the behavior is implemented, merged or active in
production. Check each specification's own status header and gates
before building or publishing anything.

## Approved scope: preview before you act (M2)

Accepted 2026-11-01 by the owner. Each build re-checks the spec's
`file:line` seams on current `main` first, and its own technical,
publication and release gates still apply before any build PR merges.

| Item | Specification | Status of the spec | What remains |
|---|---|---|---|
| Lockdown preview | [Lockdown preview](../lockdown-preview-spec.md) | Implemented, dark and unpublished — planner, executor snapshot/roles reads and dark slash with pending store merged (#1920, #1925, #1929) | Publication/activation behind its gates (spec §8), plus the open confirm/cancel build step (#1938, open). Direct lockdown stays unchanged. |
| Automod verdict preview | [Automod verdict preview](../automod-preview-spec.md) | Approved — private staff preview for synthetic input, no moderation effects | Build the dry-run preview command and its rate/audit limits. Not a member-facing oracle. |
| Warning history | [Warning history](../warning-history-spec.md) | Approved — private, bounded, metadata-only staff read, reasons withheld | Independent history-read gates still open; no historical-data grant follows from acceptance alone. |
| Custom-command arguments | [Custom-command arguments](../custom-command-args-spec.md) | Built in this stack — one literal, bounded, non-recursive `{args}` substitution with output-safety controls; build slices merged (#1970, #1974) | Gated release. The spec Status line still reads "merges pending"; open #2002 records it shipped. Not a general template language. |

Acceptance of the intent is recorded in each spec's status header.
Acceptance does not lift any no-build state inside the spec and does
not authorize a command publication, schema change or deploy.

## Deferred scope: later milestone (M3) — no-build

Retained but deferred, undecided (see
[ADR 0003](../adr/0003-m3-deferred.md)). These are proposals, not
commitments, and are not current implementation demand:

- Private agenda of your own Going/Interested events
- Re-use your own past LFG post
- Staff ticket priority
- Add one game night to the calendar
- Leave a giveaway with no refund

Other retained specs in the repository (community, events and
dashboard proposals) keep design history in the
[documentation map](../README.md). Inclusion there preserves history;
it does not make a feature active.

## Implemented vs remaining (at snapshot)

- **Implemented and merged:** voice-room self-service and saved-response
  editing paths listed in the parity matrix
  ([parity](../parity.md)) and publication guide
  ([command-publish](../command-publish.md)); the M2 lockdown preview in
  dark (planner #1920, executor reads #1925, dark slash #1929); and the
  `{args}` build slices (#1970, #1974). A merged change is not by
  itself proof of production activation.
- **Blocked on staging recovery:** automod preview (W3), warning
  history (W4) and `{args}` (W5, built in a stack with merges pending
  per its spec Status line; builds merged in #1970/#1974, status
  correction open as #2002). See [handoff](../handoff.md) for the
  current blocker and next step.
- **Not started:** any commissioned M3 build, because M3 items are
  deferred and undecided, and later-milestone merges are gated on M2
  (see [ADR 0002](../adr/0002-m2-m3-sequencing.md)).

## How this plan is verified

- Before building: re-check the spec's `file:line` seams on current
  `main`; confirm the spec's technical gates and kill rule still hold.
- Before merging: run the pre-push checks in
  [`CONTRIBUTING.md`](../../CONTRIBUTING.md#pre-push-checklist), meet
  the applicable production merge requirements in
  [`CONTRIBUTING.md`](../../CONTRIBUTING.md#production-merge-evidence)
  (including Release Manager confirmation on the exact head, even for
  docs-only changes), and link the exact-head CI results in the PR body.
- Before activating: stage, exercise the behavior once on staging and
  attach evidence.
  Deploy and rollback procedures live in the
  [production deploy runbook](../production-deploy.md) and the
  [operations runbook](../runbook.md); incident and release holds are
  unchanged by this plan.

## Explicit non-goals

- No third feature theme and no new features beyond accepted scope.
- No broader moderation oracle, reason-bearing warning search/export,
  or expanded template language.
- No wider permissions or live-guild scope changes.
- No migration off Paperclip, bulk export or sync plugin; Paperclip
  stays the live work queue (assignments, dependencies, status).
