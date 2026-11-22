# Current-state plan — November 2026

**Snapshot date:** 2026-11-10 (UTC)
**How to read this file:** every item is marked proposed, approved,
implemented or verified. Accepted means the specification is accepted;
it does not mean the behavior is implemented, merged or active in
production. Check each specification's own status header and gates
before building or publishing anything.

## Source reconciliation (2026-11-21 UTC)

Source reviewed at `main` commit `be4867a25210602613277ac04e109695ca2e175c`.
Use these findings when resuming work from the November 10 snapshot below.

- **Lockdown preview:** Confirm/Cancel merged through the existing mutation
  path (#1938), with [component acceptance tests](../../crates/discord/tests/channel_moderation_lockdown_preview_confirm_acceptance.rs).
  The command is still dark in source. The remaining [spec §8](../lockdown-preview-spec.md)
  slice covers registry, help and docs integration plus synthetic staging
  evidence. Publication and activation require their applicable review and
  release gates.
- **AutoMod verdict preview:** The bounded store (#2267), audit commit gate
  (#2264), and Discord handler (#2270) are on main. The dry-run and repeat
  isolation regression (#2342) and conditional staging-guild registration
  (#2343) also landed. [Spec §9, slice 4](../automod-preview-spec.md#9-future-build-slices-each-1-day-cut-ready-not-created-here) calls for the
  exact-head Actions proof and a read-only registry receipt from the
  qualified staging artifact. Check the serving SHA and registry before
  treating publication as verified. The scope is staging only.
- **Warning history and `{args}`:** Check the
  [history-read gates](../warning-history-spec.md) before building warning
  history. For [`{args}`](../custom-command-args-spec.md), resume release
  qualification from the merged build slices.
- **Operational evidence to confirm:** Current staging recovery and drill
  status, AutoMod registry read-back, `{args}` staging or production promotion,
  and any release-hold clearance require current operator or release evidence.

## Approved scope: preview before you act (M2)

Accepted 2026-11-01 by the owner. Each build re-checks the spec's
`file:line` seams on current `main` first. The spec's technical,
publication and release gates still apply at their respective steps.

| Item | Specification | Status of the spec | What remains |
|---|---|---|---|
| Lockdown preview | [Lockdown preview](../lockdown-preview-spec.md) | Implemented, dark and unpublished — planner, executor snapshot/roles reads and dark slash with pending store merged (#1920, #1925, #1929) | Publication/activation behind its gates (spec §8), plus the open confirm/cancel build step (#1938, open). Direct lockdown stays unchanged. |
| Automod verdict preview | [Automod verdict preview](../automod-preview-spec.md) | Approved — private staff preview for synthetic input, no moderation effects | Build the dry-run preview command and its rate/audit limits. Not a member-facing oracle. |
| Warning history | [Warning history](../warning-history-spec.md) | Approved — private, bounded, metadata-only staff read, reasons withheld | Independent history-read gates still open; no historical-data grant follows from acceptance alone. |
| Custom-command arguments | [Custom-command arguments](../custom-command-args-spec.md) | Implemented — one literal, bounded, non-recursive `{args}` substitution with output-safety controls; both build slices merged (#1970, #1974) | Gated release; the shipped spec status records the merges, not production activation. Not a general template language. |

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
  history (W4) and `{args}` gated release (W5; builds merged).
  See [handoff](../handoff.md) for the blocker and next step at snapshot.
- **Not started:** any commissioned M3 build, because M3 items are
  deferred and undecided, and later-milestone merges are gated on M2
  (see [ADR 0002](../adr/0002-m2-m3-sequencing.md)).

## How this plan is verified

- Before building: re-check the spec's `file:line` seams on current
  `main`; confirm the spec's technical gates and kill rule still hold.
- Before merging: run the pre-push checks in
  [`CONTRIBUTING.md`](../../CONTRIBUTING.md#pre-push-checklist), meet
  the applicable production merge requirements in
  [`CONTRIBUTING.md`](../../CONTRIBUTING.md#production-merge-evidence), and
  link the exact-head CI results in the PR body. Require owner
  approval for the full PR head SHA if the merge would deploy to production or
  if production deployment cannot be ruled out.
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
