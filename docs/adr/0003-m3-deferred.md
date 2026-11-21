# ADR 0003: Later-milestone items deferred, undecided

- **Status:** Accepted
- **Date:** 2026-11-10
- **Deciders:** Owner, 2026-11-10; recorded by Product Lead

## Context

Five later-milestone (M3) ideas came from agent research into other
bots, not from a member need. Their specifications were written and
retained, but nobody had asked for the behavior.

## Decision

The following are retained but **deferred, undecided** — nothing is
committed to building them, and they do not start automatically when
the earlier milestone or staging recovery finishes:

- Private agenda of your own Going/Interested events
- Re-use your own past LFG post
- Staff ticket priority
- Add one game night to the calendar
- Leave a giveaway with no refund

They return only if Product or the owner explicitly commissions one.
Existing specifications and research are kept so a future
commissioning starts from history, not from scratch.

## Why

Building unrequested features spends review and production risk on
behavior no member asked for. Deferring undecided items keeps the
roadmap honest: presence of a spec is not a commitment.

## Consequences

- December revisit dates and early-slot triggers for these items are removed.
  Each deferred spec's status header now records this 2026-11-10 decision;
  the superseded 2026-11-01 December and early-slot lines are preserved as
  history in that header's supersession wording and in git history.
- If one is commissioned, every gate still applies: security review,
  usability review, database discipline, CI and release approval.
- Deferred specs stay in the [documentation map](../README.md) as
  design history; they are **no-build** work, not current
  implementation demand.
