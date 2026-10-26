# Giveaways: private self-only requirements snapshot

Status: **proposed, design only**. Written 2026-10-25; review by 2026-10-31.
No runtime, command registration, sensitive read, schema or activation changes.
Merge publishes a proposal, not permission to build. This file is the single
repository contract; intent review binds its exact commit and SHA-256.

## 1. Decision and demand uncertainty

Consumer: a member considering one giveaway they can already see. Hypothesis:
a private explanation before entry avoids accidental participation. Demand is
**unvalidated**; no interviews, request counts or engagement benefit are claimed.

| Option | Value / cost | Recommendation |
| --- | --- | --- |
| A. No build; retain current entry refusal | No additional disclosure or read load; attempting entry may enter when requirements pass | **Default now**. Do not describe entry as a preview or tell members to enter merely to check |
| B. Explicit self-only non-entry check | Explains only existing static rules without participation; needs fresh visibility proof not established here | Smallest conditional design to review; no build until all gates below are satisfied |
| C. Other-user checks, lists, DM or richer eligibility report | Introduces identity/existence or enumeration surfaces | Excluded |

[Hex giveaways](https://docs.hex.bot/giveaways), read 2026-10-25, documents
private entry confirmations and missing-requirement explanations, but says
requirements are not rechecked at draw. This is vendor documentation, not TWO
demand or a trust-policy recommendation. Do not copy its level/block/standing
rules or its draw semantics. Staff creation preview is a separate operation.

## 2. Source baseline and limits

Inspected source head: `42e5c8f3587fb43671686ecd0da3439a68dca167`.
Source facts, not production verification:

- `crates/core/src/giveaway.rs:210-293`: `adjudicate_eligibility` requires
  **all** configured roles, then minimum account age, then minimum guild age in
  whole days; it returns the first failure. Requirements default to empty/zero.
  No standing, denylist, level or prize/legal rule is present.
- `crates/discord/src/giveaway.rs:578-657`: entry takes its budget, uses
  interaction-member evidence, maps missing/future join time to zero days,
  calls mutating `admit_entry`, and updates the public count on success.
  Ineligibility gets generic role-or-age text, not a private non-entry report.
- `crates/core/src/giveaway_store.rs:778-854`: admission locks the parent,
  checks lifecycle/deadline/claim/pending message, adjudicates, checks existing
  entry, durable rate and capacity, then inserts. `get_giveaway` (`:203-219`)
  is read-only but returns internal nonce/claim fields: never serialize its row.
- `crates/discord/src/giveaway.rs:926-1011`: draw/reroll rechecks fresh membership
  and requirements before commit; absent members are excluded and transport
  failure aborts. Do not claim Discord checks are atomic with the transaction.
- `crates/core/src/router.rs:716-720,838-842`: `/giveaway` is administrative
  (Manage Server); `gw:` entry components have a separate member path.
- `crates/discord/src/onboarding_permissions.rs:23-108,164-213`: existing
  `MemberAccess` reads guild/roles/self member and checks a channel. It does not
  expose joined-at/human proof, require text-channel history access, or supply
  this proposal's five-second aggregate budget. Unmodified reuse is insufficient.
- [Privacy](privacy.md#member-opt-out-per-user-per-guild) distinguishes analytics
  suppression from erasure. No suppression check was found in the inspected
  giveaway entry/admission/draw seams; erasure separately covers giveaway rows
  (`crates/cutover/src/member_erasure_plan.json:33-46`). Do not invent a giveaway
  opt-out rule or rebuild missing analytics history. Privacy applicability needs
  an explicit decision before B; until then it is unavailable.

No live giveaway, member, permission, deployment or database was probed. Fresh
actor-effective channel visibility and exact applicable privacy-fence reuse
remain **unverified build prerequisites**, not implied by bot access or this doc.

## 3. Authorization before disclosure

Proposed surface: `/giveaway-requirements id:<existing-id>`, one member-facing
read-only command in the configured guild, **not** a relaxation of administrative
`/giveaway`. Publication/default permissions need explicit later acceptance.
No target-member, bulk, search,
autocomplete enumeration, force, DM, public/shareable link or execution option.
Take actor, guild, application and invocation channel only from authenticated
interaction context; a valid-looking snowflake is not authority.

In this order, before showing any giveaway metadata or missing rule:

1. Preserve existing application, configured-guild, feature, activation, actor
   and shared-router admission fences. Bot/foreign/DM invocations keep their
   existing ignore/refusal policy; never add a fallback dispatcher.
2. Admit to the separate bounded read budget (§5); failed private ACK/defer
   stops before data reads. Replies start private, never public-then-edit.
3. Resolve exactly `(configured_guild, giveaway_id)` using a bounded read, not
   prefix matching or an all-giveaway list. Keep the row private until authorized.
4. Prove **fresh current membership and actor-effective access to the bound
   channel**, not just the invoking channel, interaction member cache, a stored
   entry, old message possession, or a successful bot GET. Bind actual channel
   guild/type and current role/overwrite evidence; account for administrator
   and guild-owner semantics through the existing permission resolver. Restrict
   this minimum proposal to ordinary guild text channels; threads and unknown
   channel types are unavailable, not permission to read extra membership.
   Require View Channel and Read Message History, conservatively. No history
   fetch, member list, participant list or other-user lookup is permitted.
5. Preserve every applicable existing privacy/opt-out fence. If its result or
   fresh evidence is absent, stale or unavailable, withhold the answer. Do not
   disclose whether suppression, departure, hidden channel, missing resource
   or backend failure caused withholding. Do not derive guild age from stored
   analytics when fresh `joined_at` is missing.
6. Read/revalidate the same row immediately before presentation: still open,
   before deadline, unclaimed, with no pending draw/announcement state. Require
   unchanged rules/channel/lifecycle across reads and unchanged observed policy
   revision. Never repair a stale row, release a claim or invoke closeout.
   Unknown/corrupt state is unavailable. Evidence has a five-second monotonic
   lifetime, checked before send; timeout or observed invalidation discards it.

A channel/role/member change after external verification cannot be atomic with
Discord delivery. The snapshot explicitly admits that bounded race and grants
no future access. Never reuse the result as entry authority or cache it for
another invocation. If current permitted readers/resolvers cannot prove this
boundary within §5, choose A; **new sensitive reads require a separate independent
boundary decision**, not a silent expansion of this proposal.

## 4. Requirements-only semantics and private UX

Validate the stored requirements schema/bounds without echoing parser errors.
Reuse `adjudicate_eligibility` with current human actor roles (fresh bot=false
proof required), account age derived from their
authenticated Discord ID, and known guild age from fresh `joined_at`. Use the
existing evaluator's ordering and inclusive thresholds; return only its first
safe missing-rule category. Never manufacture `0 days` for missing/future
`joined_at`: when a positive guild-age rule needs that evidence, the answer is
unavailable. Future/invalid account creation evidence is likewise unavailable.
No guild-age requirement means missing join time is irrelevant to that rule.

The categories are **eligible**, **missing_rule**, **unavailable**. Eligible
means only “requirements met”, not admitted, already entered, likely to win,
or draw-eligible. Durable entry still checks deadline, claim/pending state,
existing entry, capacity and rate. Draw retains TWO's fresh-membership behavior;
this check changes neither entry nor draw policy.

All replies: ephemeral; fixed plain text at most 500 Unicode characters;
`allowed_mentions.parse=[]`, no replied-user ping, embeds, attachments, links,
role/member names or raw input. Do not echo title/prize, raw account/join dates,
actual ages, private membership/opt-out facts, participants/counts, audit data,
nonce/seed/winners, or unsafe denial reasons. No list of all missing rules.

| Category / reason | Private text and next action |
| --- | --- |
| eligible / REQUIREMENTS_MET | “Requirements met at this check. This is a requirements-only snapshot. You have not entered by checking; entry has additional checks and draw uses fresh membership.” / choose entry separately |
| missing_rule / ROLE_REQUIRED | “A required role is missing. This is a requirements-only snapshot; checking does not enter you.” / consult the already-visible giveaway rules |
| missing_rule / ACCOUNT_AGE_REQUIRED | “The account-age requirement is not met. This is a requirements-only snapshot; checking does not enter you.” / consult the already-visible rules |
| missing_rule / GUILD_AGE_REQUIRED | “The server-age requirement is not met. This is a requirements-only snapshot; checking does not enter you.” / consult the already-visible rules |
| unavailable / UNAVAILABLE | “Requirements check unavailable. Nothing changed. Try again later from a giveaway you can currently view.” / retry later, never change privacy settings to obtain an answer |

Missing, hidden, deleted, foreign-guild, departed, privacy-withheld, closed,
expired, claimed, pending, stale, malformed and store/Discord-failure cases share
that final category and shape. No per-cause public error, diagnostic ID, resource
metadata or automatic retry. Do not promise identical network timing; admission
and bounded exact-key reads limit probing, not a claim of constant-time secrecy.

Changes between check and entry are ordinary: lose a role or membership, reach
capacity/deadline, or start a draw -> entry/draw decides anew. Existing-entry
status is not read or disclosed; even an already-entered actor gets only the
same static snapshot. Repeating a check never changes entry state.

## 5. Budget, effects, retention and security disposition

Proposed maximums, not measured runtime headroom: **2 checks/member/guild/60 s**,
**20 checks/guild/60 s**, **2 in flight/guild**, **64 limiter keys/process**.
Refuse new keys at capacity, never evict a live bucket to bypass a limit; expire
keys after 60 s. Two exact-key giveaway reads, at most four Discord GETs total
(current self member plus bounded existing permission evidence), and at most
one existing scoped privacy-fence read if required; whole operation five seconds.
No retries, history reads, participant counts or scans. A shared/multi-process
runtime needs an already-approved coordinating limiter or stays unavailable;
per-process multiplication is not an accepted bound. Reuse stricter existing
read/REST admission. **Never consume or reserve the entry-write budget**.

Zero participation/audit/count writes; zero nonce/randomness generation; zero
public message create/edit/refresh; zero scheduling, claiming, drawing or reroll.
The only sends are the private interaction ACK and reply. No schema, credential,
new permission, suppression bypass, spend or persistent report/cache. Limiter
bookkeeping is transient, separate from admission/entry, not a participation log.

Request-scoped roles/ages/proof/result are dropped at completion, by five seconds;
no result history, export, analytics or member-labelled telemetry. Limiter keys
expire within 60 s and must be removable by existing member-erasure handling
before any future activation; if that cannot be proved, no build. Replays after
erasure recompute only permitted fresh evidence, never resurrect projections.
Existing Discord ephemeral-message retention is Discord-controlled, not a bot
promise of deletion. Never log payloads, tokens, IDs, dates, rules or suppression
reasons; only existing bounded label-free availability counters if already allowed.

**CISO disposition: NO BUILD / no new authority granted.** Publishing this
bounded design is acceptable; privacy/visibility implementation is not accepted.
Bot readability is insufficient actor visibility; the fresh resolver, privacy
applicability and erasure path have not been proved here. This is a refusal of
new reads/provisioning, not self-approval of a boundary request. Unblocking
alternative: retain current entry refusal, without calling entry a preview.
Any future change needs independently authorized disposition on exact code and
tests; the author cannot approve their own capability request. No security gate
is waived by merge, demand evidence, or intent alignment.

## 6. Agent contract and runnable offline oracle

Discovery: [documentation map](README.md). No CLI/API/command is registered.
Future help must say “requirements only; self only; does not enter; may become
stale”. Proposed typed envelope, not an executable action:

```json
{"schema":"giveaway.requirements.v1","mode":"dry_run","guild_id":"fixture-guild","giveaway_id":"fixture-draw","actor_id":"fixture-self","outcome":"eligible","reason":"REQUIREMENTS_MET","retryable":false,"next_action":"choose_entry_separately","effects":[]}
```

IDs come from authorized bindings, never user-selected targets. For unavailable,
all three IDs are null, `reason=UNAVAILABLE`, `retryable=true`,
`next_action=retry_from_visible_message`, `effects=[]`. Missing-rule next action
is `read_visible_rules`. Reasons are exactly the five codes above; no sensitive
internal subreason escapes even in JSON. `mode=dry_run` is the only supported
mode; unknown modes/flags fail before any client construction. No execute flag
combination or production selector exists. Idempotent means zero domain effects,
not a guaranteed identical answer after state changes; budgets still apply.

Run from repo root (stdlib only; no credentials/environment, network or DB):

```sh
python3 -m doctest -v docs/giveaway-self-check-spec.md
python3 scripts/check_docs_index.py --json
git diff --check
```

The oracle below is a **design model**, not production logic, a live permission
probe or proof of actual no-effect callers. It checks precedence, thresholds,
unknown ages and collapsed refusals over synthetic evidence. Future protected
acceptance must drive actual dispatcher/evaluator with failing mutation sinks,
REST transcripts and before/after entry/audit/count/nonce/refresh witnesses.
It must cover every matrix case here, including changes between check and entry.
Do not replace core evaluation with this Python model in runtime.

```pycon
>>> import json
>>> def check(*, authorized=True, ready=True, fresh=True,
...           required_roles=("r1",), held_roles=("r1",),
...           account_days=30, joined_days=30, min_account=7, min_guild=7,
...           mode="dry_run"):
...     if mode != "dry_run":
...         raise ValueError("UNSUPPORTED_MODE")
...     reason = "UNAVAILABLE"
...     if authorized and ready and fresh:
...         if account_days is None or account_days < 0:
...             pass
...         elif min_guild > 0 and (joined_days is None or joined_days < 0):
...             pass
...         elif not all(role in held_roles for role in required_roles):
...             reason = "ROLE_REQUIRED"
...         elif account_days < min_account:
...             reason = "ACCOUNT_AGE_REQUIRED"
...         elif min_guild > 0 and joined_days < min_guild:
...             reason = "GUILD_AGE_REQUIRED"
...         else:
...             reason = "REQUIREMENTS_MET"
...     outcome = ("unavailable" if reason == "UNAVAILABLE" else
...                "eligible" if reason == "REQUIREMENTS_MET" else "missing_rule")
...     action = {"unavailable": "retry_from_visible_message",
...               "eligible": "choose_entry_separately",
...               "missing_rule": "read_visible_rules"}[outcome]
...     return {"schema": "giveaway.requirements.v1", "mode": mode,
...             "guild_id": None if outcome == "unavailable" else "fixture-guild",
...             "giveaway_id": None if outcome == "unavailable" else "fixture-draw",
...             "actor_id": None if outcome == "unavailable" else "fixture-self",
...             "outcome": outcome, "reason": reason, "next_action": action,
...             "retryable": outcome == "unavailable", "effects": []}
>>> cases = [
...     ("met", {}, "REQUIREMENTS_MET"),
...     ("role", {"held_roles": ()}, "ROLE_REQUIRED"),
...     ("all_roles_required", {"required_roles": ("r1", "r2")}, "ROLE_REQUIRED"),
...     ("no_rules", {"required_roles": (), "held_roles": (), "min_account": 0, "min_guild": 0}, "REQUIREMENTS_MET"),
...     ("account", {"account_days": 6}, "ACCOUNT_AGE_REQUIRED"),
...     ("guild", {"joined_days": 6}, "GUILD_AGE_REQUIRED"),
...     ("exact_threshold", {"account_days": 7, "joined_days": 7}, "REQUIREMENTS_MET"),
...     ("missing_join", {"joined_days": None}, "UNAVAILABLE"),
...     ("future_join", {"joined_days": -1}, "UNAVAILABLE"),
...     ("no_join_rule", {"joined_days": None, "min_guild": 0}, "REQUIREMENTS_MET"),
...     ("future_account", {"account_days": -1}, "UNAVAILABLE"),
...     ("first_rule", {"held_roles": (), "account_days": 0, "joined_days": 0}, "ROLE_REQUIRED"),
... ]
>>> cases += [(name, {"authorized": False}, "UNAVAILABLE") for name in
...           ("foreign_guild", "hidden_channel", "deleted_channel", "departed_member",
...            "applicable_privacy_fence", "arbitrary_target", "missing_member_state")]
>>> cases += [(name, {"ready": False}, "UNAVAILABLE") for name in
...           ("missing_draw", "closed", "expired", "claimed", "pending", "store_down")]
>>> cases += [("stale_proof", {"fresh": False}, "UNAVAILABLE"),
...           ("rules_changed", {"fresh": False}, "UNAVAILABLE")]
>>> domain = dict(entry=0, audit=0, count=0, nonce=0, refresh=0, schedule=0, draw=0)
>>> before = domain.copy()
>>> for name, evidence, expected in cases:
...     result = check(**evidence)
...     assert result["reason"] == expected, name
...     assert result["effects"] == [] and domain == before, name
...     assert len(json.dumps(result)) < 500, name
...     if expected == "UNAVAILABLE":
...         assert all(result[key] is None for key in ("guild_id", "giveaway_id", "actor_id"))
>>> denied = [check(**evidence) for _, evidence, expected in cases if expected == "UNAVAILABLE"]
>>> assert all(result == denied[0] for result in denied)
>>> assert check()["outcome"] == "eligible"
>>> assert check(held_roles=())["outcome"] == "missing_rule"  # later entry cannot use old check
>>> assert check(ready=False)["outcome"] == "unavailable"  # deadline/claim changed
>>> assert check() == check() and domain == before
>>> check(mode="execute")
Traceback (most recent call last):
    ...
ValueError: UNSUPPORTED_MODE
>>> print(json.dumps({"schema": "giveaway.requirements.oracle.v1", "fixtures": len(cases), "status": "pass"}, sort_keys=True))
{"fixtures": 27, "schema": "giveaway.requirements.oracle.v1", "status": "pass"}

```

## 7. Exact-spec acceptance, review metric and undo

Before any build/acceptance-test cards: record CEO spec acceptance and host
intent alignment through the President/COO, binding this file's exact commit,
SHA-256 and issue-document revision. The President/COO coordinates the CEO and
existing authorized host-intent source; neither this author nor a review receipt
substitutes for either source. If that authority cannot be traced, retain A.
A receipt, queued request, silence, an
expired review date or this author-authored CISO refusal is not acceptance.
Do not ask the owner directly. No new host executor or routine approval hop.
The decision must explicitly accept A (no build) or revise the prerequisites
for B, the requirements-only wording, disclosure/rate limits and no-new-sensitive-
reads rule. Any material change invalidates alignment; bind a fresh revision.

Review target 2026-10-31: all 27 model cases dispositioned, and visibility,
applicable privacy fence, limiter/erasure and exact intent each accepted or
explicitly refused. This measures design completeness, not customer demand.
Kill rule: if proof needs a new sensitive read or a visibility/privacy bypass,
stop B and retain A, not stop giveaways. No timeout auto-approval. A later demand
experiment needs its own metric/date; none is activated by this proposal.

ACT: publish a spec and synthetic oracle for review. WHY: separate explanation
from entry without creating a hidden-resource oracle. UNDO: reviewed docs revert;
no runtime/data changes to undo. No test/migration against production, live
probe, spend, credential placement or feature activation is authorized here.
