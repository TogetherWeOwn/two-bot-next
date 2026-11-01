# Move a feed relay without moving its history

Status: **Declined: no build** (owner decision 2026-11-01). This doc stays as
the record; reopen only with a named host problem. Original proposal: written
2026-10-25. No command, endpoint, schema, permission, live read/write, service
or spend is introduced. Merge records a proposal, not security acceptance or
host intent. This is the sole editable repository spec; decision records bind
its commit and SHA-256. Stop before any implementation or protected-test cards.

## Decision and alternatives

Staff reorganizing channels may want future feed posts elsewhere while keeping
already-delivered items deduplicated. Demand and frequency are **unmeasured**;
this design makes no production or benefit claim.

| Option | Consequence | Recommendation |
| --- | --- | --- |
| A. No build; keep the current destination | No new disclosure or delivery transition; channel reorganization remains manual | **Default now** until exact intent and independent security disposition |
| B. Remove then add | Refuses pending recovery, but successful deletion cascades settled history; a new ID can replay old items | Not a history-preserving move; do not recommend as a repair |
| C. One destination-only move | Preserves identity/history; needs two-channel authority, stale-snapshot fencing and honest uncertainty | Smallest conditional future option; specify below, do not build now |

C changes only `channel_id` plus required concurrency/audit metadata. Preserve
`id`, `guild_id`, source, kind, creator, creation time, enabled state and every
settled delivery row byte-for-byte. No source editor, bulk move, replay/backfill,
message copy/edit/delete, dashboard or new admin gateway. Existing messages stay
in the old channel; a move never makes them visible in the new one. Future unseen
items can still originate from older feed entries; this is not a time cutoff.

## Observed source and limits

Inspected baseline: `c3af7333df80a766f5d4c229506a3bce53680828`. Repository inspection
is not deployed-state evidence. No feed, channel, membership or live DB was read.

| Current seam | Evidence and limit |
| --- | --- |
| Authority and surface | [Planner](../crates/core/src/feeds.rs#L88-L165) has Add/Remove/Inspect/List only, configured guild + Manage Server; Add uses invoking channel, enabled=true. [Router](../crates/core/src/router.rs#L750-L787) applies announcements/permission gates. No move, preview, generation or intent API |
| Object/permission gap | [Feed transport](../crates/discord/src/executor.rs#L3081-L3165) posts without the proposed two-channel preflight. The separate [access model](../crates/discord/src/channel_access.rs#L10-L80) is not a complete feed authority path or read-history proof |
| Schema and collision | [Schema](../crates/cutover/migrations/0180_feeds.sql#L2-L31) has relay enabled/timestamps, unique guild/channel/kind/source and cascading ledger. No generation, move receipt or per-delivery destination. A target tuple already occupied by another relay must refuse `CONFLICT`, never merge/delete either ledger |
| Enable/pause | [Store](../crates/core/src/feeds_store.rs#L25-L85) inserts only; enabled-only poll reads exclude disabled rows. [Recovery](../crates/core/src/feeds_store.rs#L188-L288) also requires enabled. Pause/resume is a [separate open proposal](https://github.com/TogetherWeOwn/two-bot-next/pull/1720), not baseline support; its final enabled/recovery semantics need reinspection before build |
| Removal vs claim | [Store](../crates/core/src/feeds_store.rs#L87-L173) refuses all pending, otherwise cascades settled history. Shared READ COMMITTED advisory fence has 5 s lock/15 s statement timeout, no network |
| Stale admission | [Fresh claim](../crates/core/src/feeds_store.rs#L212-L247) rechecks guild/ID/channel/enabled under that fence, then commits pending before sending; no generation/source comparison. [Settlement](../crates/core/src/feeds_store.rs#L290-L328) is guild/item/token-fenced, not destination-fenced |
| Recovery destination | [Queue](../crates/core/src/feeds_store.rs#L253-L285) takes channel from the supplied relay, **not a stored delivery channel**; [runtime](../crates/bot/src/feed_jobs.rs#L431-L485) reconciles before fetch. Repointing a pending relay would misdirect recovery |
| Delivery uncertainty | [Runtime](../crates/bot/src/feed_jobs.rs#L516-L724) keeps ambiguous sends pending; history checks are bounded and a missing match is not absence. [Ledger](../crates/core/src/feeds_store.rs#L175-L207) never reclaims delivered rows; expired pending takeover is not a new-send authorization |
| Baseline test seams | [Store tests](../crates/core/tests/feeds_store.rs#L175-L436) cover claims, tokens and restart; [runtime races](../crates/bot/src/feed_jobs_tests.rs#L1985-L2394) hold the advisory fence and observe DB waiters for removal, **not move acceptance** |

Use [feed operations](feed-supervisor.md) for existing behavior. Source-sample and
dashboard proposals are separate inputs, not authority to fetch content or edit
a destination. Global announcements-off is a tick gate, not proof an in-flight
send drained ([job](../crates/bot/src/feed_jobs.rs#L239-L266)).

## Conditional authority and non-disclosure contract

A destination ID is a selector, **not permission proof**. Every read, preview,
confirmation and retry authenticates the real current guild member; derive actor
identity from that session/interaction, never a supplied actor field. Preserve
existing guild/application, feature, activation and human-actor gates. Require
Manage Server **and** current channel-resolved access below; creator identity
or a service credential is insufficient. Do not widen any bot/actor permission.

- Scope relay lookup to `(trusted_guild_id, relay_id)` before returning details.
  Both saved old channel and proposed new channel must exist in that same guild,
  be supported GuildText channels and have authoritative current object facts.
  No threads, forums, DMs, webhook targets, fallback channel or permission repair.
- Actor must have View Channel, Read Message History and Send Messages in **both**
  channels, even on a no-op; bot must have the same effective capabilities in both.
  No network probe post is allowed as validation. Inaccessible/deleted old channel
  is a refusal, not permission to abandon its recovery evidence.
- **Do not broaden or otherwise change the audience.** Initial C requires equal
  canonical, fully resolved role/member overwrite sets (including allow/deny
  bitfields) under the same current guild-role context, and equal NSFW status.
  Compare authoritative leaf-channel facts, not category IDs, names, an admin's
  own access, `@everyone` alone, or two booleans named private. Future resolver
  must use trusted Discord channel type/NSFW/overwrite data, current guild role
  permissions, actor/bot memberships and guild-owner/admin bypass semantics;
  prove Read Message History separately (the cited helper lacks it). Sort exact
  `(overwrite_type, target_id, allow, deny)` tuples; reject duplicate/unknown
  targets/bits or incomplete role context. Do not equate absent and explicit
  deny overwrites or inherit a category list in place of the leaf's actual list.
  Exact equality intentionally refuses some equivalent audiences; it must never
  accept merely equal send permissions. Private-to-public and different private
  audiences refuse even when the actor can send in both. No reader census/list.
- Re-read all authority/object/audience facts at preview and immediately before
  commit, outside the DB transaction. Bind them to authenticated actor, both
  channels, current relay generation and exact proposed target; expire a preview
  after 5 minutes. A detected permission/membership change requires a fresh
  preview, never automatic confirmation. Each validation phase is one attempt,
  <=6 read-only Discord requests total and <=5 seconds overall, no retries or
  permission-changing calls; exhausted/partial/unknown facts refuse. Later send
  checks use the same bounds inside the existing relay deadline, never extending
  it. Extra read traffic/latency is unmeasured and part of the intent decision.
- Discord ACL reads and SQL cannot be atomic. A permission change **after** the
  final read can race commit/send; equality proves a snapshot, not future audience
  equivalence. Before each later wire send re-check destination guild/type and
  bot view/send authority; a failure/unknown result authorizes no send. Ordinary
  later channel ACL changes remain Discord administration, not a promise made by
  a move receipt. Independent disposition must explicitly accept this residual
  race; if strict instantaneous audience equality is required, retain A. No DB
  lock across Discord calls, no claims of eliminating this external race.

Same-guild does not mean disclosed-to-all-staff. Missing/foreign/unauthorized
relay or channel shares `NOT_AVAILABLE`, with no source, old/new channel, creator,
item, pending count or existence hint. Reauthorize **before returning a cached
receipt**. Log only fixed reason/correlation classes, not feed URLs/bodies,
channel names, permission lists or raw Discord/SQL errors. Authorized previews
show IDs and inert channel names, never mentions, source samples or item history.

## One atomic transition and the delivery fence

**Future prerequisite, not current support:** a persisted monotonic relay
`generation`, incremented by every destination/enabled mutation under the same
admission fence. IDs are never reused. An expected old channel alone cannot detect
A -> B -> A. A hash of the current row, timestamps and audit IDs are not a durable
version. If generation and stale-poll integration cannot be accepted, retain A;
do not silently call today's store versioned.

1. Private preview reads current relay/generation and performs the above checks;
   no claim, audit, enqueue, fetch, lease renewal, nonce or channel effect. It may
   read bounded DB/Discord metadata; offline dry-run below performs no IO.
2. Same actor confirms the exact `(guild, relay, generation, old, new)` within
   5 minutes. Revalidate authority first. Require a fresh intent/idempotency key;
   store an exact-binding hash, not raw source or permission payloads.
3. After reauthorization, begin one short transaction on the **same guild/feed
   advisory fence** as admission/removal, with existing timeouts. Look up the
   exact intent receipt: matching binding replays its original outcome, conflicting
   binding refuses. Receipt lookup precedes CAS because a saved move advanced
   generation. No receipt + expired/invalid confirmation proof -> `INTENT_EXPIRED`,
   not fresh execution. Read current guild-scoped row; expected generation/old-channel
   mismatch is `CONFLICT`. Feature/activation checks remain mandatory. Conflict
   masks later state reasons deliberately: re-preview does not authorize a save.
4. Under that fence refuse if **any pending row of this relay in this guild**
   exists: fresh/expired lease, accepted-unknown, recovery queued, claimed but not
   sent, disabled relay, or outside the recovery page. Use `EXISTS` over
   `feed_deliveries JOIN feed_relays ON id=feed_id WHERE feed_id=$relay AND
   guild_id=$guild AND state='pending'`, without lease/enabled/channel filtering.
   No other relay blocks it. No match limit other than existence, no 20-row page.
   Timeout is `UNAVAILABLE`, never guessed empty. This precedes same-destination
   success: pending plus old=new still refuses.
5. If authorized, current and no pending: old=new is `UNCHANGED`, no relay update
   or generation advance. Otherwise update only destination and generation.
   In the same commit persist minimal actor-attributed old/new ID receipt and
   idempotency outcome. Audit failure rolls back the move. Refusals do not mutate
   relay/ledger or reserve an intent. No delivery row update, delete or rekey.
6. Commit and return `SAVED`, **configuration only**, not sent/applied. SQL/network
   failure before known commit is `UNAVAILABLE`; lost commit response is
   `COMMIT_UNKNOWN`, never success or assumed rollback. Inspect the same intent
   after reauthorization; do not compensate, issue a different key or remove/add.

Generation/intent persistence and atomic audit are **future schema prerequisites,
not present or migration-approved here**. Proposed receipt key is unique on
`(guild_id, relay_id, intent_id)`, bound to authenticated actor/old/new/generation.
Keep linkable actor/channel receipt at most 24 hours; member erasure removes it
and invalidates that actor's outstanding preview proofs, rather than retaining
an identity hash.
No indefinite PII exemption. A future integrity-bound preview proof has a 5-minute
admission expiry and is never minted by offline dry-run; an unknown/erased intent
without a still-valid proof cannot execute. Receipt replay within 24 hours may
outlive preview expiry; after receipt expiry return `INTENT_EXPIRED` before CAS,
never treat the old key as fresh. Same key/exact binding returns original `SAVED`
or `UNCHANGED`, never repeats audit/DML; different binding conflicts. A **fresh**
no-op returns `UNCHANGED`; it must not claim a previous move. Replay is a private
historical read, not a move/no-op admission, and cannot redirect pending work.
Reading current destination alone cannot prove which caller saved it. If receipt,
audit or commit fails atomically, all DML rolls back; an unknown commit permits
only same-intent inspection, not another execution. This is a proposed bounded
policy for independent security acceptance, not a claim of existing support.

**Race order:** claim first -> pending exists -> move refuses, and that send stays
bound to its old destination. Move first -> generation/destination changes -> a
poll snapshot of the old generation cannot admit even if the destination later
returns to the old value. Fresh admission must compare saved enabled state,
source/kind, destination **and generation** under the fence. Discard stale fetched
items; the next ordinary poll reads a fresh relay. Do not repoint stale work to
the new channel, trigger an immediate poll, or clear item keys to force replay.

Recovery keeps the old relay binding, nonce, item key and owner-token fence.
**There is no per-delivery channel column today:** the all-pending refusal keeps
the relay's destination unchanged until recovery finishes, so historical pending
work cannot acquire the new destination. It may settle only using the existing
authoritative receipt path; no-match, expiry or a move request is not absence proof. Never redirect,
auto-settle, release or delete uncertain sends to unblock a move. Restarts and
restore preserve ledger/generation/intent fences or keep the sender disabled
pending reconciliation. A paused relay with no pending work stays paused after
a permitted move. A disabled relay **with pending work refuses indefinitely**:
baseline recovery requires enabled, and no baseline resume command exists. Do not
unpause by SQL or add a move-specific recovery bypass; a separately accepted
pause/recovery path must resolve that condition before C can cover it, otherwise
retain A for that relay. Move is neither pause, resume, drain nor recovery. Safe
undo is another authorized CAS move with no pending work, not SQL rollback,
history deletion or message recall.

## Private UX and proposed agent contract

No surface is registered by this proposal. A later accepted adapter must advertise
its exact supported operation and schema in help/tool discovery, not add fields
to existing commands and assume they work. All replies are private/ephemeral,
<=2,000 UTF-16 units, closed allowed-mentions and escaped inert text; never public
fallback, raw item/title, source URL, token, attachment or paginated history.

- Preview: relay ID, observed generation, old -> new channel IDs, enabled/paused
  state, expiry and warning: “Future unseen items only. Old posts stay put.
  Nothing sent or saved. Unresolved delivery or detected access changes block
  saving; later permission changes can race the final snapshot.”
- Saved: same old/new IDs and committed generation, “Destination saved; no post
  sent. History retained. Poll/application timing is not confirmed.”
- Unchanged: “Destination unchanged; nothing sent.” Refused: fixed reason and
  next action below. Unknown commit: “Save outcome unknown; inspect this intent.
  Do not submit a different move.” No delivery-success claim in any move receipt.

Illustrative **future** JSON, with synthetic IDs (not an executable endpoint):

```json
{"schema":"feed.move.v1","guild_id":"100","relay_id":"relay-1","expected_generation":"7","old_channel_id":"200","new_channel_id":"201","intent_id":"move-1","dry_run":true,"code":"PREVIEW","retryable":false,"next_action":"confirm_exact_preview","sent":false}
```

IDs/generations are strings; validate canonical nonzero Discord IDs and bounded
relay IDs using existing parsers. Unknown keys, source/kind/enabled edits, batch
inputs and an unbound actor refuse. Every result has `schema,code,retryable,
next_action,sent`; authorized results also carry exact IDs/generation/intent.
Denied results omit them. Failure maps to a nonzero adapter exit; do not invent
current HTTP statuses. `retryable=true` permits another preflight, never blind
mutation or delivery retry. False does not forbid the explicitly prescribed
inspection/read, including same-intent inspection after an unknown commit.

| Code | retryable | next_action |
| --- | --- | --- |
| `PREVIEW` | false | `confirm_exact_preview` |
| `SAVED` / `UNCHANGED` | false | `none` |
| `NOT_AVAILABLE` | false | `check_authorized_context` |
| `AUDIENCE_MISMATCH` | false | `keep_current_destination` |
| `PENDING_DELIVERY` | false | `inspect_existing_recovery_no_resend` |
| `CONFLICT` / `PREVIEW_EXPIRED` | false | `reread_then_reconfirm` |
| `UNAVAILABLE` | true | `retry_preview_when_available` |
| `COMMIT_UNKNOWN` | false | `inspect_same_intent_no_new_move` |
| `INTENT_EXPIRED` | false | `reread_then_new_preview` |
| `INVALID_INPUT` | false | `fix_input` |

Offline dry-run is a deterministic decision over supplied **synthetic or already
authorized** snapshots. No DB/Discord/feed/network call, audit, idempotency record,
claim, nonce, scheduling or send. `PREVIEW` means model eligibility only, not
current permission proof. A production confirmation must never accept a client
snapshot/dry-run output as authorization. Live preview is separately read-only.

## Runnable offline contract matrix

Run from the repo root, Python standard library only:

```sh
python3 -m doctest -v docs/feed-move-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

This embedded model tests the **proposed oracle**, including permission changes
and same-guild non-disclosure. It is not production implementation, protected
acceptance evidence, a DB race test or proof of Discord authorization.

```pycon
>>> from copy import deepcopy
>>> import json
>>> base = dict(guild="100", relay="relay-1", channel="200", generation="7", enabled=True, source="fixture", kind="rss", creator="300", settled=["item-1"], pending=[])
>>> access = dict(member=True, manage=True, actor_old=True, actor_new=True, bot_old=True, bot_new=True, same_guild=True, exists=True, audience_equal=True)
>>> actions = dict(PREVIEW="confirm_exact_preview", SAVED="none", UNCHANGED="none", NOT_AVAILABLE="check_authorized_context", AUDIENCE_MISMATCH="keep_current_destination", PENDING_DELIVERY="inspect_existing_recovery_no_resend", CONFLICT="reread_then_reconfirm", UNAVAILABLE="retry_preview_when_available", INTENT_EXPIRED="reread_then_new_preview")
>>> def authorized(facts):
...     return all(facts.get(k) is True for k in ("member", "manage", "actor_old", "actor_new", "bot_old", "bot_new", "same_guild", "exists"))
>>> def emit(code, row=None, expected="7", old="200", new="201"):
...     out = dict(schema="feed.move.v1", code=code, retryable=code == "UNAVAILABLE", next_action=actions[code], sent=False)
...     if row is not None:
...         out.update(guild_id=row["guild"], relay_id=row["relay"], generation=row["generation"], expected_generation=expected, old_channel_id=old, new_channel_id=new, intent_id="move-1")
...     return out
>>> def decide(row, facts, expected="7", old="200", new="201", dry=True, available=True):
...     snapshot = deepcopy(row)  # Internal fixture state, never a denied response.
...     code = "PREVIEW"
...     if not authorized(facts): return emit("NOT_AVAILABLE"), snapshot
...     if not available: code = "UNAVAILABLE"
...     elif facts.get("audience_equal") is not True: code = "AUDIENCE_MISMATCH"
...     elif row["generation"] != expected or row["channel"] != old: code = "CONFLICT"
...     elif row["pending"]: code = "PENDING_DELIVERY"
...     elif new == old: code = "UNCHANGED"
...     elif not dry:
...         snapshot["channel"], snapshot["generation"] = new, str(int(expected) + 1)
...         code = "SAVED"
...     return emit(code, snapshot, expected, old, new), snapshot
>>> def case(row=base, facts=access, **kw): return decide(row, facts, **kw)[0]["code"]
>>> assert case(new="200", dry=False) == "UNCHANGED"                                      # M01
>>> result, moved = decide(base, access, dry=False)
>>> assert result["code"] == "SAVED" and moved["channel"] == "201"                         # M02
>>> assert all(moved[k] == base[k] for k in base if k not in ("channel", "generation"))
>>> assert all(case(dict(base, pending=[p]), dry=False) == "PENDING_DELIVERY" for p in ("fresh", "expired", "uncertain", "queued"))  # M03
>>> assert case(dict(base, pending=["uncertain"]), new="200") == "PENDING_DELIVERY"
>>> assert case(dict(base, pending=["winner"]), dry=False) == "PENDING_DELIVERY"             # M04 claim wins
>>> assert case(moved, expected="7", old="200") == "CONFLICT"                                # M05 move wins
>>> assert case(dict(base, generation="9"), expected="7") == "CONFLICT"                     # M06 ABA
>>> paused = dict(base, enabled=False)
>>> assert decide(paused, access, dry=False)[1]["enabled"] is False                         # M07
>>> assert case(facts=dict(access, same_guild=False)) == "NOT_AVAILABLE"                    # M08
>>> assert case(facts=dict(access, audience_equal=False)) == "AUDIENCE_MISMATCH"             # M09
>>> for key in ("member", "manage", "actor_old", "actor_new", "bot_old", "bot_new"):
...     assert case(facts=dict(access, **{key: False})) == "NOT_AVAILABLE"                   # M10
>>> assert case() == "PREVIEW" and case(facts=dict(access, actor_old=False), dry=False) == "NOT_AVAILABLE"  # M11 revoke after preview
>>> denied, unchanged = decide(base, dict(access, actor_old=False), new="200", dry=False)
>>> assert denied == emit("NOT_AVAILABLE") and "relay_id" not in denied and unchanged == base  # M12 hidden old channel, even no-op
>>> assert case(facts=dict(access, exists=False)) == "NOT_AVAILABLE"                        # M13 deleted channel
>>> assert case(available=False, dry=False) == "UNAVAILABLE"                                # M14
>>> preview, snapshot = decide(base, access)
>>> assert snapshot == base and preview == decide(base, access)[0] and not preview["sent"]  # M15 dry-run, no mutations
>>> recovery = dict(base, pending=[{"nonce":"n", "owner":"o"}])
>>> assert decide(recovery, access, dry=False)[1] == recovery                               # M16 recovery and relay channel untouched
>>> def inspect_receipt(facts, receipt, binding, expired=False):
...     if not authorized(facts): return emit("NOT_AVAILABLE")
...     if receipt is None or expired: return emit("INTENT_EXPIRED", base)
...     if receipt["binding"] != binding: return emit("CONFLICT", base)
...     return deepcopy(receipt["response"])  # No CAS, audit, DML or send on replay.
>>> binding = ("100", "relay-1", "300", "7", "200", "201")
>>> receipt = dict(binding=binding, response=result)
>>> assert inspect_receipt(access, receipt, binding) == result and result["code"] == "SAVED"  # M17 original saved, not fresh no-op
>>> assert inspect_receipt(access, receipt, binding[:-1] + ("202",))["code"] == "CONFLICT"   # M18 key/body mismatch
>>> assert inspect_receipt(access, receipt, binding, expired=True)["code"] == "INTENT_EXPIRED"  # M19 expiry precedes stale CAS
>>> assert inspect_receipt(dict(access, actor_old=False), receipt, binding) == emit("NOT_AVAILABLE")  # M20 revoke before replay
>>> assert set(denied) == {"schema", "code", "retryable", "next_action", "sent"}
>>> assert all(isinstance(result[k], str) for k in ("guild_id", "relay_id", "generation", "old_channel_id", "new_channel_id", "intent_id"))
>>> print(json.dumps({"schema":"feed.move.oracle.v1","cases":20,"outcome":"pass","scope":"model_only"}, sort_keys=True))
{"cases": 20, "outcome": "pass", "schema": "feed.move.oracle.v1", "scope": "model_only"}

```

Future **independent protected fixtures**, only after acceptance: use the existing
disposable DB helper and independent pools for M03–M06/M16, with barriers around
the advisory fence (not sleeps); assert exact relay/ledger/audit counts and
destinations. Include >20 pending rows and expired leases, enabled changes,
restart/restore, timeout/rollback and commit-unknown injection. Mock current
permissions twice for M08–M13, including role/overwrite changes and different
private audiences; assert zero disclosed fields and zero sends on denial.
Recording loopback Discord transport must observe zero POST from move/preview,
old destination for a winning claim, zero sends for stale generation and no
settled-item replay after the next poll. Add receipt expiry, key/body mismatch,
reauthorization on cached receipts, full private message bounds/mentions and
preview expiry. No staging/production DB or live channel/source requests.

## Disposition, exact revision and stop rule

**Independent CISO boundary disposition: not obtained.** The proposal's author
cannot approve their own request; an adversarial drafting review or ordinary
CodeRabbit or Devin Review is not that disposition. Request independent accept/change/reject
on audience equivalence/residual ACL race, current two-channel authority,
linkable receipt retention/erasure and delivery/recovery invariants. No new
credential, permission or runtime authority is granted by this proposal.

**CEO/host intent alignment: not obtained.** Route one exact-revision decision
brief through the President & COO using the existing authorized intent path,
not an owner approval hop. Bind task-document revision, repo commit and file
SHA-256. Require explicit recorded alignment for C's scope, same-audience limit,
paused behavior, no replay, unknown outcomes and privacy limits before **any
build or test-author card**. Do not treat queued review, docs merge or silence as
acceptance; a changed spec invalidates prior exact-revision acceptance.

Review on 2026-11-01: metric is 20/20 oracle cases with agreed future acceptance
assertions plus explicit independent disposition and intent alignment, not moved
relays or measured demand. Missing/rejected/unsafe conditions -> **A, no build**;
no passive approval or new live experiment. Unverified: deployed state, actual
objects/ACLs, demand, runtime move behavior and future receipt retention policy.

ACT: publish a source-backed proposal and executable offline model only.
WHY: preserve history without turning a destination change into replay or
cross-audience disclosure. UNDO: revert documentation; no live state changed.
