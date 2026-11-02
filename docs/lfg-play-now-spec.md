# Proposed member play-now posts

Status: **Declined: no build** (owner decision 2026-11-01). This doc stays as
the record; reopen only with a named host problem. Original proposal: written
2026-10-26. Not implemented, published or approved for a pilot. Merging this
page approves documentation, not a new command, permission, schema or deploy.
All numbers below are proposed pilot defaults, not existing runtime settings.

## Intent and recommendation

A member who wants company tonight should be able to post one small pickup
session without asking staff to create it. Keep scheduled staff LFG events
unchanged. Recommend a separate `/playnow` surface, one approved channel per
guild, a short lifetime and a single optional notification at creation.

**Consent choice:** game interest is not notification consent. Do not mention
the existing game role. Send at most five explicit user mentions in the one
post, selected only from current holders of that game's role who separately
opted into play-now notifications. Here, “game ping” means that bounded audience,
not an unrestricted Discord role mention. No DMs, recipient fan-out messages,
reminders, bump action, `@everyone` or `@here`.

This choice and the numeric limits are engineering recommendations for review,
not decisions already made by members or staff. If literal role mentions are a
requirement, return for a consent design revision; do not silently substitute
them. No voice rooms, threads, event creation, waitlist, backup host, attendance
inference or dashboard are included in v1.

## Existing seams, not new guarantees

Use the [LFG domain](lfg-port.md) and
[LFG interaction contract](lfg-interaction-contract.md) as the baseline.
The existing staff create path, persisted signups and component validation are
reusable seams, not proof that member creation or these safety gates exist.
[Send admission](discord-send-admission.md) governs REST transport; it is not a
community-notification consent or bot-noise budget. Event reminder preferences
remain separate; opting into one feature must never opt into the other.

Source baseline: `c838e5125` (repository inspection, not a live-system claim).

| Existing behavior | Evidence | Proposed difference |
| --- | --- | --- |
| Staff commands require Manage Events | `crates/core/src/feature_commands.rs:455-532` | Member creation is new, separately gated authority; no permission drop. |
| Game picker’s “Apply game roles” menu | `crates/discord/src/onboarding_messages.rs:89-107`; role mapping in `crates/core/src/onboarding.rs:197-230` | Interest/access roles are eligibility only, never inferred ping consent. |
| Join/Leave uses one string select with seat buckets | `crates/core/src/lfg.rs:569-603`; `crates/discord/src/lfg_interactions.rs:584-595` | One Player bucket; retain self-only, guild/message-bound validation. Seat buckets are not Discord roles. |
| Signups stop at start; sweep closes after 15 minutes | [Current lifecycle](lfg-port.md#bounded-lifecycle-maintenance), backed by `crates/core/src/lfg_store.rs` | Member kind accepts `now` and joins until explicit expiry; do not reuse the staff cutoff unchanged. |
| Current creates/edits suppress mentions; nonce/history recovery is bounded | [Current delivery](lfg-port.md) | An explicitly gated member notification effect, not a global change to mention policy. |

Before building, recheck these seams against the accepted head. Current `LfgPost`
and its sweep do not supply host-presence handling, play-now expiry, consent or
durable per-host/game/recipient limits. The member-kind discriminator and time
policy must be used by both mutation and sweep paths: never fake a future start
to evade today's cutoff or change scheduled staff posts. Current failed unbound
posts may close after five minutes; the proposal's unknown-send reservation
instead lasts to member expiry. Future slices must supply runtime acceptance
tests for these differences. The matrix below is a design oracle only, not
evidence that runtime tests have passed. Existing ready-check, backup-host,
thread and voice-room proposals are not implemented prerequisites.

## Member flow and post contract

Proposed command: `/playnow post game:<configured game> seats:<1..7>
starts:<now|15m|30m> notify:<false|true>`. `notify` defaults to **false**.
Only an existing guild member who can view and send in the approved channel,
is not a bot or timed out, and currently holds the selected game's configured
role can create. Reject unknown games, caller-supplied role/channel IDs and
unconfigured or inaccessible targets. No free-text title, link or mention input
in v1. Staff enable games explicitly; onboarding alone must not enable this.
Proposed configuration is one versioned guild play-now record containing the
channel, IANA timezone, human-space channel set, enabled games with canonical
interest-role IDs, and guild/game kill gates. Only Manage Events staff may
change it. Missing record/field means disabled; no guessed environment default.
Builders choose additive storage/locking primitives, with guild/game/host budget
serialization and unique interaction claims; no schema choice is approved here.

Let `created_at` be the durable accepted creation time. `start_at` is that time
plus 0, 15 or 30 minutes; `expires_at = start_at + 30 minutes`. The command has
one confirmation showing the game, **additional** seats, start, expiry and
whether a ping can be attempted. Recheck all gates at confirmation; a preview
reserves nothing. Duplicate delivery of that confirmation returns the original
receipt rather than creating another session.

One public message reads, for example:

> Member pickup · Example Game · hosted by Example Member
>
> Need 3 more · starts in 15 minutes · signups close 30 minutes after start
>
> 0/3 additional players · Join / Leave / Close / Report
>
> Notifications: off, or up to 5 opted-in players notified once

Host attribution is non-pinging text. The host is implicitly playing and cannot
join their own guest roster: `seats` counts additional players, not the host.
Use one fixed “Player” slot with that capacity and reuse the existing LFG
join/leave interaction grammar and validation, rather than introducing a second
unsigned signup protocol. A future format discriminator must bind member-post
kind, host, game, channel, message, revision and expiry to durable state; a
client-supplied host or message ID grants no authority.

Join requires a current non-bot guild member with access to the channel, the
same game role and a live post. It does **not** require notification opt-in.
Leave remains available to an existing signup even if they lost their game
role. Replies are ephemeral; only the saved post changes. Joining means
interest, not presence or attendance. Staff `/lfg*` commands and their
permissions do not change. Public styling must say “Member pickup,” never
“official event.” A proposed onboarding link may open help for this flow only
after the feature is approved and implemented; the current link is not a post.

## Notification consent and recipient selection

A proposed `/playnow notifications game:<game> enabled:<true|false>` preference
is guild- and game-scoped, default off. Enabling requires current game-role
membership and shows: “Mention me in occasional member pickup posts for this
game; at most twice a day. Discord notification settings still apply.” A
separate proposed `/playnow notifications-off` subcommand disables every
play-now game preference.
Neither action changes game-interest roles, signup status, event reminders or
other notification preferences. Store preferences as new purpose-specific
`(guild, game, member)` records with generation and disabled tombstone; recipient
budgets are separate `(guild, member)` history shared across games. Do not
extend signup or event-reminder consent implicitly. Schema names/transactions
remain builder choices; role-loss and opt-out advance the generation. Anyone can turn their own preferences off,
including after losing a role; no public acknowledgement or DM is sent.

Select at most five candidates from opted-in preference rows, least recently
notified first with stable user-ID tie-break, excluding the host and exhausted
recipient cooldowns. Read at most six rows (five plus a bounded lookahead), take
the first five valid candidates, and stop. Do not enumerate a role's entire
membership, paginate or refill rejected candidates in a loop. If those six are
ineligible, send silently even if a seventh member might have qualified. Revalidate current guild membership, game role and channel visibility
for those candidates immediately before dispatch. Missing/stale facts or a
failed read exclude that candidate. Loss of a game role clears that game's
opt-in when observed; reacquisition requires fresh opt-in. There is no automatic
migration of onboarding choices into preferences.

The durable recipient list may shrink, never expand after confirmation. At the
send boundary, serialize withdrawal and sending on the relevant preference
records: withdrawal committed before dispatch admission excludes the member;
withdrawal after an admitted send cannot retract an already issued Discord
notification. Reject stale preference generations. Persist the dispatch claim
before the external send, with one logical notification effect per session.
Do not hold a database transaction open across Discord HTTP. A sender admitted
before withdrawal may still reach Discord afterward; preference confirmation
must disclose this unavoidable in-flight boundary, not promise retroactive
revocation. Discord role/access changes also race the final membership read;
no cross-system atomicity is claimed. A known change before dispatch suppresses
the mention; strict atomic role-membership guarantees would require a revised
product contract, not an assertion that a cache provides them.

The create message uses `allowed_mentions` with automatic parsing off, only
those explicit user IDs, no roles and no reply-author ping. Every subsequent
edit uses an empty allowed-mention set and removes the transient notification
line; signup/leave, reopen, close, reports and restart never ping. Never retry a
ping by editing a message. If permission, freshness, budgets or consent fail,
create a **silent** post only if the independent creation/noise gates pass,
and tell the host exactly why no notification was attempted. Zero eligible
recipients is a successful silent post, not an error and not permission to
mention the broader game role.

## Pilot limits and abuse controls

Limits use durable rolling windows scoped to the guild. They apply across
restarts, concurrent confirmations, close/recreate attempts and uncertain sends.
No staff bypass on the member surface; staff retain their separate workflow.

| Scope | Proposed hard limit |
| --- | --- |
| Host | One active member post; two accepted posts/24 h; same-game creation cooldown 6 h |
| Game | One active member post; one notification attempt/4 h; two notification attempts/24 h |
| Guild | Four accepted member posts/24 h; at least 30 min between creations |
| Recipient | One notification attempt/6 h and two/24 h across all games; at most five recipients/post |
| Public effects | One creation and at most three coalesced roster/state edits/post, including one reserved terminal edit; no automatic extra messages |

An accepted attempt consumes its window even on an uncertain external result;
close, deletion, role changes or consent toggles do not refund budgets. Proven
pre-dispatch validation refusals consume no creation or notification budget.
Creation and notification reservations must be atomic with their respective
claims; concurrent hosts cannot overspend a game/guild/recipient limit. A
silent accepted post consumes host/game-creation/guild post budgets normally.
A ping refused before dispatch admission, including zero recipients, consumes
no game/recipient notification windows. At admission, reserve the game window
and each actual recipient's window together; uncertain admitted attempts keep
all those reservations, even if the send later fails.

Quiet hours: **22:00–09:00 in the explicitly configured guild IANA timezone**,
including DST. Both the confirmation instant and the entire half-open interval
`[created_at, expires_at)` must be outside quiet hours (expiry exactly at 22:00
is allowed); refuse any overlap, including a start window crossing into them.
Recheck at dispatch; do not send a delayed creation inside quiet hours. Never
queue until morning. Missing/invalid timezone
or an unresolved local-time conversion fails closed. Existing signup/leave,
terminal cleanup, consent withdrawal and reporting still work during quiet
hours. Transport rate limits/backoff still apply; do not retry past expiry or
turn a 429 into another notification effect.

Bot noise must remain **strictly below 20%** of human-space message volume,
including all bots, not only play-now. Before any new post, use complete durable
rolling-24-hour counts `H` (human messages) and `B` (bot messages) for the guild's
staff-defined human-space channel set. Require `(B + 1) / (H + B + 1) < 0.20`;
when `H = 0`, counts are stale/incomplete, or another bot's volume is unknown,
refuse creation. Reserve against concurrent TWO bot sends through a shared
admission budget, incorporating observed other-bot messages into `B`. Without
complete counts and shared admission for TWO writers, the pilot stays off; a
per-feature counter is not proof of the guild-wide budget. Unrelated bots are
not transaction participants: this is admission-time evidence, not a promise
that another bot cannot subsequently breach the ratio. Any observed aggregate
breach disables the pilot. Coalesced edits do not create message-volume entries
but have the separate three-edit cap.
No public refusal message is a fallback. Baseline values are not assumed here.

Report uses an ephemeral confirmation and a bounded durable staff record keyed
by post/reporter; one report per reporter/post, no public reply, ping or DM.
Reuse the existing staff moderation intake only if it can preserve that binding
and bound. Otherwise that intake is an explicit prerequisite. Staff with
`ManageEvents` can close a member post or disable play-now per game or guild;
server checks, not button visibility, enforce it. Disable advances a gate
generation and cancels never-admitted sends; recheck that generation at dispatch
admission and each join. A disabled gate stops new admissions immediately;
existing leave, close, reporting and opt-out remain available. As with consent,
a send already admitted may be in flight and cannot be recalled. Tell a racing
host “cancelled before dispatch” or “already admitted; delivery may occur,”
never promise an unsend. Cleanup marks existing posts closed without deleting history.
Unrecognized/unauthorized controls have zero public effects. No arbitrary
member text or recipient list belongs in logs or the staff report receipt.

## Lifecycle and failure truth

| Situation | Required behavior |
| --- | --- |
| Accepted creation | Persist host/game/times, capacity, interaction idempotency key, budgets and delivery state before dispatch; return one receipt. Pending send expires after 60 s or session expiry, whichever is sooner. |
| Full | Atomic capacity check admits only the remaining seats; further joins get an ephemeral “Full.” No waitlist or full notification. Leave reopens seats until expiry without re-pinging. |
| Host leaves | Host cannot use guest Leave; offer Close. Host leaving the guild or losing channel access/game role closes the session when detected. No succession or host transfer in v1. |
| Start | Start time is informational, not a reminder or automatic close. `now` renders “started” immediately and accepts joins until expiry; no hidden grace period. |
| Expiry or manual close | At `now >= expires_at`, reject new joins even before cleanup runs. Close is terminal; retain signup history, disable controls and use the reserved terminal edit. Staff/host Close requires current authority. |
| Member leaves guild | Remove the member from active guest seats when observed; revalidate the roster on mutation/recovery. Never infer that every saved signup is still present. |
| Duplicate/racing action | One creation per confirmation key; serialize join/leave/close with post revision and unique signup identity. Close/expiry wins against a later join; stale controls never resurrect a post. |
| Definite create failure | Record failed delivery; no success claim and no automatic replacement ping. A new user request still faces consumed attempt limits. |
| Timeout/ambiguous create | Mark `delivery_unknown`; keep its active-post reservation and notification budgets until expiry. Reconcile by a deterministic message nonce/receipt if supported; otherwise staff inspect. Never resend blindly. |
| Restart | Restore durable sessions, windows, preferences and claims; expire overdue rows first. Resume only never-dispatched creates within the 60 s send deadline, with fresh consent/permissions and no widened audience. A provably never-admitted create past that deadline is failed/closed and releases only its active slot, not creation-attempt windows. An admitted/unknown send keeps its slot to session expiry. No catch-up ping. |
| Deleted message / failed edit | Mark unavailable or projection stale; domain close/expiry remains authoritative. No new public replacement message. Reconcile within the edit cap; exhausted terminal repair needs staff action, not unbounded retries. |

Observe host/guest membership changes through gateway events when available,
recheck the host and affected member before every mutation, and reuse a bounded
60-second lifecycle pass with fresh membership/access reads. Unavailable reads
refuse joins/sends and mark validation unknown, not “member departed”; safe
leave/close remains available. Known host loss closes the post. Do not promise
instant detection while disconnected; overdue expiry is still checked on every
interaction. The shared staff sweep must distinguish the member time policy.

Coalesce roster changes rather than one edit per click; once the two nonterminal
edit slots are spent, show the last known roster timestamp and direct members
to an ephemeral roster read. Join/Leave receipts report durable truth even if
the public projection is stale. Reserve the third edit for terminal state;
failed or ambiguous edit attempts count against the cap. If terminal projection
repair cannot be confirmed, record it for staff; do not claim disabled buttons
were observed. A stale visible button must still fail closed server-side.

Future storage must integrate member erasure/retention without removing replay
or rate-limit fences early. Proposed identifiable session/recipient/report
retention is 30 days; retain only non-identifying aggregate counters afterward.
Erasure and notification-tombstone design need security review before a pilot;
this page introduces no tables, data collection or deletion job.

## Design test matrix (future acceptance, not executed)

| ID | Scenario | Expected oracle |
| --- | --- | --- |
| PN-01 | Two confirmations for one host; duplicate interaction; two hosts for one game | Exactly one winning reservation, one post and at most one ping effect; loser receives a private reason. |
| PN-02 | Ping spam across games, hosts, restarts and close/recreate | Host/game/guild/recipient windows all hold; no refunded budgets or broader fallback mentions. |
| PN-03 | Game-role holder never opted in; opted-in member loses role/access | Never-selected member gets no ping; known loss suppresses candidate, no refill loop or role mention. |
| PN-04 | Consent withdrawn before admission, after admission, and while post is open | Before admission excludes; in-flight boundary disclosed; later changes/edits never re-ping. Game interest and event preferences remain unchanged. |
| PN-05 | Expired post before cleanup; close races final seat; forged/cross-message ID | Zero late/unauthorized writes or public effects; capacity never oversubscribes; closed state cannot reopen. |
| PN-06 | Full then leave; host presses Leave; host departs guild | Seat reopens silently; host offered Close; departure closes with no transfer/room creation. |
| PN-07 | Quiet-hours boundary/DST, absent timezone, stale counts, zero humans, 20% exactly | Creation refused privately; no deferred send. Strict noise threshold and aggregate atomic reservation proven with concurrent bot send. |
| PN-08 | 429, definite failure, timeout after Discord accepted, restart before/after claim | Bounded deadline, truthful failed/unknown receipts, no duplicated notification; windows and active reservation survive. |
| PN-09 | Staff disables game/guild; repeated reports; unauthorized close | No new create/ping/join, safe exit controls work, one bounded report per reporter, zero authority escalation. |
| PN-10 | Roster burst, failed edit, deleted message, erasure | Two nonterminal plus reserved terminal edit cap; private roster remains truthful; no replacement spam, personal data follows approved retention and fences survive. |

## No-build alternatives

**Lower `/lfg` permissions:** cheapest code change, rejected for this experiment.
Lowering only create permissions broadens creation authority, does not
distinguish unofficial pickups, and provides no notification consent,
per-host/game caps or short-lived member lifecycle. It would not by itself
lower edit/close permissions; owner-scoped member controls still need design. Reusing Join/Leave is desirable; dropping the staff permission
boundary is not equivalent to this design. Keep the current permissions.

**Human-only channel template:** accepted fallback and first demand check. A
pinned “game / players needed / start / finished” template lets members post
without a bot ping or new infrastructure. Staff can count demand for a week;
if it works adequately, do not build automation. No permission changes or new
staff-created recurring work are authorized by this proposal.

**Dedicated opt-in ping roles:** not selected for v1. They separate notification
consent from interest, but require role lifecycle/reconciliation, and stale
notification-role membership can ping people who no longer hold the game role.
A single bounded mention list has a smaller inspectable audience and no extra
roles. Cost: only five eligible members are reached per post, not the whole
interested role. That is deliberate; no claim of guaranteed Discord delivery.

## Measure, review and stop

Design review date: **2026-11-02**. Do not start implementation or create
executable build cards before the explicit intent-alignment decision after
that review. Any later approved pilot has a named community host and exact
start timestamp `T0`; review at **T0 + 7 days**, not seven days after this doc
merges. No launch or metric baseline is claimed today.

Record guild-scoped weekly aggregates: accepted member posts, distinct human
hosts, additional seats requested, unique first-time guest joins per session,
seats occupied at terminal state, and fill rate = terminal occupied guest seats
/ requested guest seats (zero requested denominator is N/A). Repeated
leave/rejoin cannot inflate joins. Count failed/unknown sends, ping attempts,
reports and consent withdrawals separately. Signup is not actual play or
attendance, and bot posts do not count as human community activity. Keep
metrics low-cardinality; no member IDs, usernames or game free text in labels.

Proposed 7-day decision rule, evaluated in order:

1. **Stop immediately:** any confirmed nonconsensual/broader-role ping,
   duplicated notification, rate-cap bypass, or shared noise-budget breach.
   Disable creation/pings/joins, preserve safe exits and evidence; investigate
   before a fresh approval. Missing budget/consent telemetry also disables the
   pilot rather than establishing success.
2. **End this experiment:** fewer than three accepted member posts, fewer than
   two distinct hosts, fill rate below 50%, or any unresolved abuse report after
   seven days. Revert to human-only template; do not end the community/project.
3. **Continue at the same caps:** all thresholds met and no unresolved safety
   incident. “Scale” means propose one additional configured game/channel for
   explicit review, not automatically raise ping/recipient/noise limits.

## Future slices, only after alignment

Each row is a separate proposed card of **at most one engineering day**, not
an instruction to start it now. Test Author owns acceptance tests; builders
must not change those tests. A slice stays dark until its dependencies pass.
If a slice cannot meet that bound, split it before admission.

| Slice | Dependency / small deliverable | Test plan |
| --- | --- | --- |
| A: policy core | Accepted spec; pure validation, time windows, limits and typed refusal reasons; no Discord publication | PN-01/02/07, boundary and property fixtures, fixed clocks and DST cases |
| B: consent and state | A; durable preference generations, post/recipient budgets, idempotency, CAS, erasure fences; no sends | PN-01/02/04/05/10, isolated disposable DB races and restart/retention fixtures |
| C: shared budget prerequisite | B; first prove existing guild-wide counts/reservations can satisfy noise admission; no new telemetry platform | PN-07, concurrent TWO-writer reservation plus observed other-bot volume, incomplete data fails closed; if unavailable, stop rather than enable |
| D: silent creation | B/C; gated command confirmation, game/member reads, fixed-slot post with notifications off | PN-01/03/05/07/09, fake Discord request bodies, private refusals and role-loss boundaries |
| E: optional single ping | D; bounded recipient reads, withdrawal linearization, allowed mentions and uncertain-send fence | PN-02/03/04/08, controlled before/after-admission races, request capture, timeout/restart faults |
| F: lifecycle and controls | D/E; bound existing Join/Leave to member kind, capacity, expiry, Close/Report and kill gates | PN-05/06/09/10, concurrent final seat, cross-post controls, report and edit budgets |
| G: pilot evidence | F plus approved erasure/security and staff-intake seams; staging-only E2E, rollback/disable rehearsal, documented counters | PN-01..10 end-to-end in staging; no production tests, publish or pilot without its approved execution contract |

No build card may claim safe pilot completion using policy-only tests. Approval
must name the channel/game/timezone, community host, notification-boundary
tradeoff, shared-noise evidence, report intake and rollback executor. Unknown
prerequisites keep the feature off; they are not grounds for widening staff
permissions or shipping a ping-only shortcut.
