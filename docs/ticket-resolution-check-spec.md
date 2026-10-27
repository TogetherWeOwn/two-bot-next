# Tickets: a staff resolution check is a question, never Close

Status: **Proposed — design only; recommends no-build**. Written 2026-10-27;
review by 2026-11-03. Merge publishes neither a command nor build approval. No
runtime, schema, permission, credential, spend or retention change here.

## Decision and alternatives

Consumer: a member whose ticket may be finished and the staff member deciding
whether to close it. Benefit is a hypothesis; no failure of the current approach
was inspected (no ticket contents were read), and demand is unmeasured.

| Option | Value and cost | Disposition |
| --- | --- | --- |
| A. No build; staff asks in the ticket | Zero new state, command or permission; the member answers in their own words and staff keeps the decision | **Recommend** |
| B. One staff-asked check with a structured answer | Machine-readable answer and a visible "still needs help"; needs durable state, a bot-authored channel message with controls, expiry and race handling | Smallest conditional model, built only on evidence |
| C. Auto-close on silence/expiry, member Close, or a new staff gate | Faster apparent closure but turns silence or a click into destructive authority | **Excluded** |

A message the bot posts into the ticket channel is as visible to staff and opener
as a staff-typed message, and Discord cannot hide it from other staff. B's only
extra value over A is a button answer and a stored flag that no workflow reads.
That is not shown to fix a failure A leaves open. Ticket Tool's
[commands manual](https://docs.tickettool.xyz/general/commands) lists
`$closeRequest` ("Sends the Close Ask Message"); its
[options page](https://docs.tickettool.xyz/dashboard/panel-configs/ticket-options)
ties that message to a Two Step Ticket feature with Confirm/Cancel Close.
Neither excerpt states who may answer or what silence does, so it is precedent
for the idea only, not evidence of benefit or of safe semantics.

Out of scope and unchanged: staff Confirm/Cancel of their own destructive intent,
the opener's own resolution signal, claim handoff, inactivity closure, transcript
delivery. This file does not restate them and has no dependency on them.

## Verified baseline

Head `7046b8eda953a20d129291accd23f6b24a5a0ad0`; repository facts, not production
state. A future build must recheck them at its own head.

- `crates/core/src/tickets.rs:20-24` defines only Open, Claim and Close; the
  `Ticket` record at `:73-83` has no request or answer field.
- `crates/bot/src/ticket_runtime.rs:1040-1065` sends authorized Close straight
  into the lifecycle. `:1245-1262` is its order: begin close, freeze writes,
  capture, save transcript, then cleanup. A check must never reach this path.
- `crates/bot/src/ticket_claim_close_truth_acceptance_tests.rs:429-485` pins one
  DELETE and one history capture per close. A check adds zero of either.

## Rules that hold for any structured form

1. The answer is an **assertion for staff** only. It never closes, deletes,
   captures or exports a transcript, changes access or transfers a claim.
2. Silence, expiry, decline, a missing opener and a departed opener are **never
   consent** and never start a timer, close or staff gate. Staff Close keeps
   its configured-role authorization and capture-before-delete receipt unchanged.
3. Pings, DMs, public sends and transcript text are not produced. The one
   channel message is bound to the ticket and carries no member content.

## Conditional model (only if the review recommends B)

One marker per ticket incarnation, monotonic, never a queue:

| Marker | Meaning | Next |
| --- | --- | --- |
| `none / 0` | no check | `asked / 1` |
| `asked / 1` | a named staff member asked; no answer | `resolved / 2` or `needs_help / 2` |
| `resolved / 2`, `needs_help / 2` | opener's single answer, final for this check | none |

Expiry is a **derived display** (`asked` older than a fixed 48 h, injected
clock); it writes nothing, is not a state and answers nothing. An answer after
that window is refused as `expired`. One check per incarnation; to ask again,
staff talks in the ticket (A). No re-ask, withdraw, reset or staff override.

Binding and freshness, checked under the same guild-scoped ticket row lock as
close, inside a five-second metadata proof (no Discord I/O inside the lock):

- Capture `(guild_id, ticket_id, opener_id, channel_id)` before any wait. The
  `ticket_id` is a runtime-generated incarnation ID; callers cannot supply or
  reuse it, and a recreated channel is a new ticket with marker `none`.
- Ask: actor is current staff (existing predicate) with fresh channel access and
  the ticket is `open`. The asker's ID is not stored; only `asked_at`.
- Answer: actor equals the current opener, with fresh ViewChannel + SendMessages.
  Staff, collaborators and administrators cannot answer for them.
- `creating`, `closing`, `cleanup_pending` and `closed` refuse as unavailable
  and never invoke recovery. Close first wins; ask first never blocks Close.
- Stale, forged or foreign controls resolve to a generic unavailable result
  before any marker, opener or ticket ID is revealed, and are never routed to
  Close. The custom ID carries only an opaque ticket ID and revision.
- Restart reloads the committed marker; a lost reply leaves state intact; lost
  commit acknowledgement reads as unknown, never as success.
- Retention: the marker and two timestamps are personal data on the parent row,
  removed by member erasure. Transcript purge does not remove parent rows
  (`crates/cutover/src/tickets.rs:338-372`), so no 90-day ceiling is claimable
  for them. That open lifetime gap is a further reason for A, and not a license
  to add a timer.

## Truthful examples

Private replies are plain text, at most 300 characters, mentions disabled.

| Outcome / reason / next_action | Copy |
| --- | --- |
| `asked / asked / await_opener` | "Staff asked whether you still need help. Your ticket stays open." (to staff) |
| `saved / resolved / await_staff_decision` | "Thanks. Staff have not closed anything; they will decide." |
| `saved / needs_help / await_staff_reply` | "Noted: you still need help. Your ticket stays open." |
| `no_op / already_answered / none` | "Your answer is already recorded. Your ticket stays open." |
| `refused / expired / talk_in_ticket` | "This check has ended. Tell staff here if you still need help." |
| `unavailable / context_unavailable / contact_staff` | "This check is unavailable here. Ask staff if you need help." |

```json
{"schema":"tickets.resolution_check.v1","mode":"diagnostic","guild_id":"g1","ticket_generation":"0123456789abcdef0123456789abcdef","revision":"2","outcome":"saved","reason":"needs_help","retryable":false,"next_action":"await_staff_reply"}
```

IDs and revision are strings; generation and revision are null for
unavailable or foreign context. The sample has no actor, opener, channel or
content field and is a handoff shape, not authority.

## Threat and privacy disposition

Boundaries: authenticated interaction, current channel and actor proof, locked
ticket row, one bounded channel message. Spoofed actor or selector refuses;
forged or stale controls cannot reach Close; opener and ticket are re-proven at
the lock; the monotonic marker bounds spam and replay. Residual: a Discord
permission change after the last read; the channel message is seen by all staff
with access and cannot be unsent here; the personal-data lifetime gap above.
This is the author's own assessment, **not** the independent security
disposition, which is still required before any build.

## Zero-destructive-effect matrix

Fixture: configured g1, open T1/c1, opener A, staff S, marker 0; synthetic IDs,
injected clocks, disposable test DB only. For every row assert zero channel
DELETE, transcript read or write, permission or role mutation, claim transfer,
DM, mention, close call and job; only the named state changes.

| Case | Order and required result |
| --- | --- |
| ask | S asks 0 -> asked/1, one message, lifecycle and claimant unchanged |
| repeat_ask | S asks again -> refused, no second message |
| answer_resolved | A answers resolved 1 -> resolved/2, ticket stays open |
| answer_needs_help | A answers needs_help 1 -> needs_help/2, ticket stays open |
| repeat_answer | A answers twice -> one saved, one already_answered |
| foreign_requester | Other member or staff answers -> unavailable, no detail, no write |
| non_staff_ask | Member asks -> refused, no marker |
| access_loss | A or S loses access before the lock -> unavailable, no write |
| expiry | Answer after 48 h -> expired, marker 1 unchanged, ticket open |
| silence | No answer ever -> state unchanged, no close or timer |
| decline | Dismiss or ignore the message -> no state, no close |
| stale_control | Old or forged custom ID -> unavailable, never routed to Close |
| recreated | Delete T1; c1 now T2; held T1 control refuses; T2 starts at 0 |
| close_first | Close holds the row first -> unavailable, marker unchanged |
| ask_then_close | Ask commits, staff Close -> unchanged capture-before-delete |
| concurrent_answer | Two connections answer -> exactly one saved |
| opener_departure | A leaves the guild -> answer refuses; no close, no timer |
| staff_departure | S leaves after asking -> check persists, still just a question |
| restart | Restart after commit before reply -> marker reloaded, no replay |
| commit_unknown | Drop COMMIT ack -> unknown, status reads state only |
| erasure | Member erasure removes marker; foreign guild untouched |
| overload | Saturate shared admission -> bounded refusal, no write |

Runnable document consistency, not implemented security acceptance:

```sh
python3 -m doctest -v docs/ticket-resolution-check-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

```pycon
>>> import json, pathlib
>>> text = pathlib.Path("docs/ticket-resolution-check-spec.md").read_text()
>>> rows = text.split("| Case |", 1)[1].split("\n\n", 1)[0].splitlines()[2:]
>>> cases = [row.split("|")[1].strip() for row in rows]
>>> assert len(cases) == len(set(cases)) == 22
>>> assert {"silence", "expiry", "decline", "close_first", "recreated", "restart"} <= set(cases)
>>> # Abstract oracle: answer only from `asked`; nothing here can close.
>>> ask = {0: 1, 1: None, 2: None}
>>> answer = {0: None, 1: 2, 2: 2}
>>> assert ask[0] == 1 and ask[1] is None and answer[0] is None
>>> assert answer[1] == answer[2] == 2
>>> assert all(v in (None, 1, 2) for v in list(ask.values()) + list(answer.values()))
>>> sample = json.loads(text.split("```json\n", 1)[1].split("\n```", 1)[0])
>>> assert sample["schema"] == "tickets.resolution_check.v1" and sample["retryable"] is False
>>> assert not {"actor_id", "opener_id", "channel_id", "content"} & sample.keys()
>>> print(json.dumps({"cases": len(cases), "outcome": "checked", "reason": "offline_oracle_only", "next_action": "seek_exact_intent_and_boundary_disposition"}, sort_keys=True))
{"cases": 22, "next_action": "seek_exact_intent_and_boundary_disposition", "outcome": "checked", "reason": "offline_oracle_only"}

```

This checks the proposed model and case inventory only. Future caller tests must
prove SQL serialization, binding, non-disclosure, erasure and zero destructive
wire effects. A passing doctest is not runtime readiness, demand or approval.

## Review and undo

**Decision: no-build (A).** B is recommended only if, by 2026-11-03, an evidenced
ticket failure shows staff asking in chat did not work and structured feedback
would have fixed it, and the lifetime gap has an accepted policy. Absent that,
A stands and nothing follows expiry. Any recommended future exact spec goes
through the existing CEO/host path with independent boundary review; no build
card, new authority, spend or live experiment is created here. A routing
acknowledgement or document merge is not intent alignment.

ACT: publish the proposed no-build decision and offline checks. WHY: let staff
ask whether help is finished without laundering silence into closure. UNDO:
revert the docs commit; no live data changed.
