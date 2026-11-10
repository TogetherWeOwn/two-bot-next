# ADR 0002: Finish the preview-before-action milestone before later milestones

- **Status:** Accepted
- **Date:** 2026-11-09 (accepted in principle 2026-11-08, applied 2026-11-09)
- **Deciders:** Owner (decision D1, 2026-11-02; the binding merge rule);
  Chief of Staff (decision D-2, 2026-11-08, accepted in principle);
  applied by the Engineering Lead (producer vs merge-only edges split
  before the 2026-11-09 review)

## Context

The preview-before-action milestone (M2) started a small number of
work items before the later milestone (M3) specs existed. M3 builds were
ready to start while M2 work items were still blocked on staging
recovery. Merging M3 first would put newer, less-qualified behavior in
front of older unfinished risk-reduction work.

## Decision

- M3 drafts may be built independently, but they may not merge until the
  M2 work items they follow have merged or been explicitly cut.
- Real producer dependencies (a PR that needs another PR's code) stay as
  merge blockers. Merge-order-only edges are replaced by this single
  sequencing rule, so the checkable form is easier to keep honest.
- No safety hold is removed by this decision. Both milestones still pass
  through the same security, usability, database, CI and release gates,
  and the production merge requirements in
  `CONTRIBUTING.md#production-merge-evidence` continue to apply.

## Why

Finishing the oldest unfinished risk-reduction work first keeps merged
history in a safe order. One explicit rule is easier to audit than many
ad-hoc blockers.

## Consequences

The M2 items are defined by their accepted specifications:

- [Lockdown preview](../lockdown-preview-spec.md)
- [Automod verdict preview](../automod-preview-spec.md)
- [Warning history](../warning-history-spec.md)
- [Custom-command arguments](../custom-command-args-spec.md)

Inclusion in this list is not proof of implementation or production
activation; see [plans/current-state-2026-11.md](../plans/current-state-2026-11.md).
Reversing this decision needs a dated entry here and a matching update
to the current-state plan in the same change.

While [ADR 0003](0003-m3-deferred.md) defers the later-milestone items,
the D1 gate is moot: nothing merges from that scope until it is
commissioned again, at which point D1 re-applies unchanged.
