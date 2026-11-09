# Roadmap

Last reviewed: 2026-11-09 (UTC).

This is the public direction for Owen, the Together We Own Discord bot. It is
maintained by the repository maintainers as a summary, not a live task tracker.
Priorities, work ordering and forecasts are maintained separately; this page
does not create or change accepted scope. Intentions are not delivery
commitments: no release date or feature availability is promised here.

For what is available, start with the [README](../README.md),
[command publication guide](command-publish.md) and [parity matrix](parity.md).
A specification, a merged change and a published command are not interchangeable
proofs that a feature is active in production.

## Direction

### Reliability before broader features

Keep the always-on bot dependable: recover gateway sessions safely, make failures
observable, and qualify changes with staging and rollback evidence. Voice-room
correctness and safe recovery take precedence over expanding the feature set.
This roadmap neither clears an incident or release hold nor authorizes a deploy;
the [production merge requirements](../CONTRIBUTING.md#production-merge-evidence)
continue to apply.

### Member control and reusable responses

Make voice rooms easier for members to inspect and manage without staff
intervention. Improve saved responses through predictable editing, cloning and
activation behavior, with existing state preserved. These are product themes,
not a claim that every related runtime path is complete or enabled.

### Preview before you act (M2)

The accepted scope is defined in these distinct repository specifications, not
repeated as a second acceptance checklist here:

- [Automod verdict preview](automod-preview-spec.md): a private staff preview for
  synthetic input, without moderation effects or a member-facing testing oracle.
  Its technical and demand gates remain in force.
- [Warning history](warning-history-spec.md): a private, bounded, metadata-only
  staff read for one user in the current guild, with reasons withheld. Intent
  acceptance is not a historical-data grant or production activation.
- [Custom-command arguments](custom-command-args-spec.md): one literal, bounded,
  non-recursive `{args}` substitution with output-safety controls. This is not a
  general template language or a claim that argument support has shipped.

Each linked specification retains its own scope, acceptance criteria and safety
conditions. Inclusion here does not satisfy its implementation, privacy,
publication or release gates.

## Deferred direction and non-goals

Later milestone (M3) specifications are deferred, no-build work, not current
implementation demand or delivery commitments. Keeping a proposal in the
[documentation map](README.md) preserves its design history; it does not make it
an active feature. Reconsideration needs an attributable member or host need and
an explicit scope decision, not merely the existence of a specification.

This roadmap does not commission another feature theme, a broader moderation
oracle, reason-bearing warning search/export, or an expanded template language.
It does not resume deferred proposals, widen permissions or live-guild scope,
or replace security and release review.

## Feedback and upkeep

Use the repository's [issue forms](https://github.com/TogetherWeOwn/two-bot-next/issues/new/choose)
for public feature requests, documentation corrections and non-sensitive bugs.
Describe the member or staff problem rather than assuming a proposed command
will ship. Report vulnerabilities privately through [SECURITY.md](../SECURITY.md),
not in a public issue.

Maintainers review this summary when accepted direction changes and during
regular roadmap review. Specifications remain the source for their distinct
accepted contracts; [releases](https://github.com/TogetherWeOwn/two-bot-next/releases)
record shipped history. Changes to this page alone authorize no implementation.
