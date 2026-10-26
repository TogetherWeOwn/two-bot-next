# Invites: private member referral standing

Status: **Proposed — design only**, 2026-10-26; review by **2026-11-02**.
Merge is not acceptance, intent alignment, build approval or activation.
No runtime, schema, command-publication, permission or deployment change is
included. Tier perks (spotlight post, flair, role grant) are not designed
here beyond naming the existing engine tiers; a role grant needs its own
slice and gates.

## 1. Decision and baseline

Consumer: a member who invited a friend and wants to know whether it
counted. Hypothesis: visible, private credit for friends who join **and
stay** produces more invites than today's invisible attribution, and more
credited referrals move weekly-active humans (the community scorecard's
`weekly_active_humans`, `crates/core/src/community.rs:539,1210,1575`).
Demand is **unproven**; §7 carries the kill rule.

A complete referral-reward engine exists and has never been wired.
`evaluate_referrals` (`crates/core/src/referral_rewards.rs:221`) has only
test callers; its module doc leaves wiring `ReferralInput` and delivering
`Notice`s "with the caller" (:24-25). Verified at this head:

- Tiers are experiential only, closed in the type system (no
  cash-equivalent kind): spotlight at 1, cosmetic at 3, role at 5 credited
  referrals (`:35-37,116-122,154-175`); minimum tenure default 7 days
  (`:32`). Direct and 2nd-hop bonus credits sum toward tiers (:296-310).
- The engine already rejects self-referrals, duplicate/leave-rejoin joins,
  temporary memberships (left before tenure) and unproven attribution —
  aggregate `ambiguous:*`, `vanity`, `unknown` never credit an individual
  (:19-22,84-96,325-367). Young or quiet joins stay `Pending` and
  re-evaluable (`:77-82,351-360`). First attribution wins (:326).
- Live join attribution already feeds the funnel: the gateway snapshots
  invite counters per join and files `invite:CODE` exactly when one code
  grew (`crates/discord/src/pipeline.rs:780-805`); `on_join` writes
  `metadata["inviterId"]` only for that singly-grown case
  (`crates/core/src/handlers.rs:615-653`); bots file no funnel row
  (:635-637). `referral_counts` already aggregates those rows per inviter
  as raw evidence, explicitly not credit
  (`crates/core/src/invites.rs:180-228`).
- No member ever sees a count: `docs/commands.md` has no invites surface.
- No referral ledger exists: no migration under `crates/store/migrations`
  and no store module names referrals. Standing today can only be derived
  from funnel facts.
- One existing DM seam can deliver a private notice with mentions
  disabled: `direct_notice` (`crates/discord/src/voice_rooms.rs:1206-1228`).
- Privacy already governs the inputs: a suppressed inviter's `inviterId`
  is scrubbed from join metadata (`crates/core/src/privacy.rs:108-129`),
  membership lifecycle rows persist unconditionally while activity-family
  rows are toggle/opt-out gated (:13-23).

Two baseline corrections to the engine's own copy, decided here:

1. `referred_in_copy` and `second_hop_copy` embed raw member snowflakes
   (`referral_rewards.rs:180-208`). §4's counts-only rule forbids shipping
   that text verbatim; a build slice renders member copy from verdicts and
   tiers, or amends those functions with tests. Engine `Notice.text` is
   never delivered as-is.
2. A `Pending` verdict carries no inviter (`:105`). Pending counts per
   member are therefore a caller-side join of input rows to verdicts by
   `member_id` (§3), not an engine output. Any build that renders pending
   counts straight from `ReferralReport` is wrong.

| Alternative | Member effect / cost | Proposed disposition |
| --- | --- | --- |
| A. No build; engine stays unwired | Invites stay invisible; zero new surface, data or noise | Standing fallback until acceptance; choose it if §7's baseline shows no referral joins to amplify |
| B. Operator-only report from `referral_counts` + manual shout-outs | Staff see raw counts; members still see nothing; shout-outs are public and manual | Interim measurement only (§7 baseline); not the member loop |
| C. Private `/invites` standing + bounded credit/tier DMs (this spec) | Members see own credited/pending counts and tier distance; one coalesced DM per credit batch; needs one read handler, later a notice ledger | Submit for intent review |
| D. Public leaderboard | Status race; farm incentive; small-community pressure; inference about who invited whom | Excluded in v1 (§4); revisit only at a §7 scale review |

## 2. Proposed member surface

One new slash command, `/invites`, no options, guild-only, reply always
ephemeral ("only you can see this"). It reads **the invoker's own**
standing only: no `member` option (unlike `/rank`), no staff override in
v1. It creates nothing in Discord — members still make invite links with
Discord's own UI — and the reply says so.

Fields, in order, all counts — never names, IDs or dates of other members:

- **Credited referrals**: total, split direct / 2nd-hop bonus when bonus
  > 0.
- **Pending**: count of the invoker's attributed joins still waiting,
  split by plain-language bucket: "joined recently" (`TenureTooShort`) /
  "not active here yet" (`AwaitingEngagement`). No per-person dates: with
  a pending count of 1 a date would identify the friend's join day.
- **Current tier**: engine title, or "No tier yet".
- **Next tier**: title, threshold, and distance ("1 more credited
  referral" / "N more"); at the top tier, "Top tier reached".
- **Referred-in line**, only when the invoker themselves joined through a
  credited edge: "You were referred in — pass it on." The inviter is not
  named (§4).
- **What counts**, fixed copy: a referral credits when a friend joins
  through *your* invite link, stays at least 7 days, and takes part (a
  message, voice, or accepting the rules). Only a friend's first join can
  count. Inviting yourself never counts. Joins through the server's
  public/vanity link, or that the bot could not attribute to one link,
  cannot credit anyone. Leaving and rejoining does not count twice.

Rejected joins are **not displayed**, not even as a count: the fixed copy
explains the rules generically instead. A "didn't count: N" figure would
confirm farm verdicts to the farmer and generate support disputes the bot
cannot adjudicate from counts alone. Operators keep the funnel report
(alternative B) for disputes.

Bounds: the reply is one ephemeral message, well inside the 2,000-char
limit by construction (counts and fixed copy only; §6 pins the shape). A
member with no attributed joins gets the same layout with zeros and the
what-counts copy — that is the activation path, not an error. Failure of
the underlying reads returns one honest "standing is unavailable right
now" ephemeral and no partial counts; a count the bot could not compute
completely is never rendered as zero.

## 3. Wiring plan: feeding the engine from existing attribution

No new tracking. Every input already persists for the funnel; the caller
seam is a projection from funnel facts to `ReferralInput`
(`referral_rewards.rs:61-75`), run per guild:

| `ReferralInput` field | Existing source | Projection rule |
| --- | --- | --- |
| `joins[].member_id, joined_at_ms` | `member_join` funnel rows (`handlers.rs:642-653`) | Only rows carrying `metadata["inviterId"]` enter; others cannot credit anyone and are excluded |
| `joins[].source` | row `source` | Must be `invite:CODE` (the live writer pairs it with `inviterId`); a mismatched row is excluded and logged as data-quality, never guessed |
| `joins[].exact` | — (not persisted) | `exact := inviterId present`. The live writer sets `inviter_id` only when a single code grew for that join event (`pipeline.rs:800-804`), so presence is the persisted form of single-code proof. Known corner: two joins through one code between snapshots attribute the growth to the first processed join and file the second unattributed — under-credit, never over-credit. Accepted; see §5 |
| `code_inviter` | the same rows | Synthesized as `CODE -> inviterId`, first (chronological) row wins per code. Current `invite_snapshots` lose deleted codes, so rows are the only complete source. A code later recreated by a different inviter collides in this map: first-seen wins and the conflict is logged; Discord codes are random and `rotate_code` preserves the inviter (`invites.rs:438-478`), so a genuine collision needs manual review, not silent re-credit |
| `standings[].left_at_ms` | `member_leave` rows (`funnel.rs:45-46`) | First leave after the member's attributed join; absent = still present |
| `standings[].engaged` | funnel rows for that member | Any of `gate_cleared`, `first_message`, `first_voice_session` (`funnel.rs:23-39`). `gate_cleared` persists unconditionally (`privacy.rs:19-23`), so a privacy-opted-out member can still credit their inviter through the rules gate without any analytics row (§4) |
| `known_referrals`, `prior_credits` | the credit ledger, once §7 slice 3 lands | Until then: empty, with all joins evaluated as one chronological batch (input order = `occurred_at`, so first-attribution-wins means the earliest join). After banking: prior edges/totals load from the ledger, and "unbanked" means every join whose member has no credited edge — **including previously rejected or pending joins**, re-fed in chronological order. Only credits bank; rejections never do. Anything narrower breaks RF-04: fed alone, a genuine rejoin after a rejected temporary first stay would credit, because cross-batch duplicate detection sees only credited edges (`:64-75,325-327`) |
| `now_ms`, `min_tenure_days` | clock; engine default | `MIN_TENURE_DAYS_DEFAULT` (7) unchanged in v1; per-guild tuning is out of scope |

Sequencing keeps one source of truth for counts:

- **Standing read = compute-on-read.** `/invites` projects and evaluates
  on demand, rendering from `verdicts` + `direct_credits` +
  `bonus_credits` + `tier_for_total`, ignoring `notices`. To bound the
  read it fetches only: rows with `inviterId = invoker`, rows with
  `inviterId` in the set of members the invoker was credited for (2nd-hop
  candidates), the invoker's own `member_join` row (referred-in state),
  and leave/engagement rows for those member IDs. A proposed hard cap of
  5,000 projected rows returns "unavailable" instead of a partial count;
  the build slice pins the real bound with fixtures.
- **Notices = a daily evaluator job plus a small ledger** (§7 slice 3),
  because compute-on-read has nowhere to record what was already sent. The job runs
  under the existing community-jobs supervisor (`docs/community-jobs.md`),
  evaluates uncredited joins with banked priors, inserts credit rows
  (`ON CONFLICT DO NOTHING` on `(guild_id, member_id)` for direct credit —
  first attribution wins in storage too), and claims notice rows before
  sending (§4). Counts on `/invites` never read the ledger, so an erased
  fact drops out of standing even if a notice was already sent; notices
  are history, standing is current truth.
- **Retention caveat (build gate).** Compute-on-read assumes funnel rows
  outlive the longest pending referral (7 days + engagement wait). The
  build slice must verify actual funnel retention before shipping the
  read; if rows purge sooner than credits bank, the ledger becomes the
  banked-priors source first and the read evaluates only the unbanked
  tail. This spec invents no retention period.

## 4. Notices, anti-farm and privacy decisions

**Notices.** When a referral credits or a tier unlocks, the recipient
gets **exactly one DM per daily batch**, coalescing every event that
batch produced for them (direct credits + bonus credits + any tier
unlock) into one counts-only message via the `direct_notice` seam
(mentions disabled). v1 sends no other referral DM:

- No referred-in DM to the new member. It would arrive ≥7 days after
  joining, cold, and its engine text names the inviter. The contagion
  framing lives in the `/invites` reply instead (§2).
- No separate 2nd-hop DM; bonus credits appear in the same coalesced
  count line when they land, and in standing thereafter.
- No channel posts: spotlight *delivery* (a public shout-out) is a tier
  perk, out of scope. The tier-unlock DM names the tier earned, nothing
  more.

Delivery semantics are exactly-once credits, at-most-once notices:
insert a notice row `(guild_id, recipient_id, notice_key)` — keys are
`credit:<member_id>`, `bonus:<member_id>`, `tier:<kind>`, with a unique
constraint on the triple and the same `ON CONFLICT DO NOTHING` discipline
as credit rows — in the same transaction as the credit rows, then send
one DM per recipient covering all unclaimed keys, then mark them sent.
Claim before send gives at-most-once delivery: a crash mid-batch can
lose a celebration, never duplicate one. DMs closed or recipient no longer in
the guild: mark undeliverable, never retry; standing still shows the
credit. Bot-noise budget: notices are DMs, not channel messages, and are
bounded at one per member per day in the worst case (daily batch), only
on credit events — far inside the community's low-bot-noise constraint
without touching any channel's human/bot ratio.

**Counts only.** Neither `/invites` nor any DM ever names, mentions or
dates another member. Residual inference is named honestly: an inviter
who handed one friend one link will know who a pending count of 1 refers
to. The bot adds no confirmation beyond what sharing a personal link
already implies, and shows nothing to anyone else. This is also why v1
has no leaderboard (alternative D): ranking counts adds farm incentive
and social pressure for zero additional member information.

**Anti-farm.** The engine carries the v1 rules (self, duplicate/rejoin,
temporary, unproven — §1). Decisions on what remains:

- **Alt accounts** (one human, several Discord accounts) are not
  detectable from attribution and this spec adds no detection: no IP or
  device signals exist for the bot, and an account-age gate was
  considered and deferred — it penalizes exactly the new-to-Discord
  friends referrals are meant to bring. The standing mitigations are
  cost (each alt must stay 7 days and engage), worthlessness (tiers are
  experiential; the type system admits no cash equivalent), and no
  leaderboard to win. Suspicious bursts are an operator review on the
  existing funnel report; v1 has no auto-punishment and no credit
  revocation path for suspected alts.
- **Publicly posted personal codes** credit their creator. That is
  attribution working, not farming; strangers who stay 7 days and engage
  are the growth loop.
- **Code churn** gains nothing: dedupe keys on the joining member, not
  the code, so ten codes and one friend still yield at most one credit.
- Tier thresholds make farming scale poorly by design: spotlight at 1 is
  deliberately cheap (activation); the role tier needs five distinct
  members retained and engaged.

**Privacy and erasure.**

| Event | Standing / notice consequence |
| --- | --- |
| Inviter runs `/privacy opt-out` | Existing scrub removes their `inviterId` from join metadata (`privacy.rs:108-129`), so compute-on-read credits vanish; ledger credit/notice rows for that inviter must join the erasure plan in the same build slice that creates them. No further notices |
| Referred member opts out / is deleted | Their stored activity is deleted by the existing flow; if the `member_join` row goes, the credit drops out of recomputed standing (counts can go down — standing is current truth). Already-sent DMs are not unsent; nothing new is sent about them |
| Referred member opted out *before* joining activity | Gated activity rows never persist, but `gate_cleared` does; engagement can still be proven via the rules gate alone (§3). No new tracking is added to compensate |
| Member leaves (no erasure) | A stay that earned credit keeps it (engine: genuine-then-left still credits); a leaver before tenure is rejected permanently — a later rejoin does not resurrect it (first stay decides; accepted limitation, §5) |
| Inviter leaves | Banked credits survive in facts/ledger; notices are only attempted for current members; standing is readable again if they rejoin |
| Member is banned without erasure | Facts stand; credit already earned stands. Moderation interplay (e.g. raid joins) is out of scope: unproven burst attribution already files `ambiguous`/unknown and credits nobody |

Attribution edges are retained exactly as long as their source facts:
funnel `member_join` metadata under existing funnel retention, snapshot
rows under the existing snapshot lifecycle, and — once built — ledger
rows under the erasure plan. No separate referral retention clock is
introduced.

## 5. Deterministic future scenario matrix

The build slices turn each row into a fixture or test; IDs are stable
references for acceptance criteria. "Engine" rows are already pinned by
`referral_rewards.rs` tests; "projection" and "delivery" rows are the new
surface this spec adds.

| ID | Scenario | Expected |
| --- | --- | --- |
| RF-01 | Friend joins via invoker's code, stays 10d, sends a message | 1 direct credit; spotlight tier; one DM naming counts only |
| RF-02 | Friend joins, day 6, engaged | Pending "joined recently"; no credit, no DM; credits when re-evaluated after day 7 |
| RF-03 | Friend joins, day 30, never engaged | Pending "not active here yet"; never credits early; no DM |
| RF-04 | Friend joins, leaves day 3, rejoins day 20 via same code, stays | Rejected temporary on first stay; rejoin is a duplicate; 0 credits forever (documented limitation) |
| RF-05 | Friend joins, stays 25d engaged, then leaves | Credits; leaving afterwards does not revoke |
| RF-06 | Invoker joins their own code on an alt, stays, engages | Self-referral only if same account; true alt credits — accepted residual (§4), visible only as a count |
| RF-07 | Six friends join in one snapshot window; two codes grew | `ambiguous` files; 0 credits; nobody notified |
| RF-08 | Two friends join through one code between snapshots | First processed join carries `inviterId`, second files unattributed: at most 1 credit (under-credit corner, §3) |
| RF-09 | Friend joins via vanity/public link | No attribution; 0 credits; what-counts copy explains |
| RF-10 | Credited friend later refers their own friend, who credits | Middle member: direct +1; invoker: bonus +1; both count toward tiers; bonus folds into invoker's next coalesced DM |
| RF-11 | Invoker banks 4 credits; 5th lands | Tier crosses to role in that batch only; exactly one tier DM; standing shows "Top tier reached" |
| RF-12 | Repeat credit inside the same tier | Credit count grows; no tier DM (engine: milestone fires once per tier) |
| RF-13 | Inviter opts out after 2 credits | Scrubbed metadata + erased ledger rows; standing recomputes without them; no DM |
| RF-14 | Referred member is erased after crediting | Edge removed; invoker's credited count drops by 1 on next read; sent DMs unchanged; tier shown is recomputed truth |
| RF-15 | Referred member is privacy-opted-out; only `gate_cleared` persists | Engaged via rules gate; credits after tenure; no analytics row created |
| RF-16 | Daily batch credits 12 joins for one invoker | One DM total for that invoker in that batch, counts only; 12 credit rows, 12 notice keys marked sent |
| RF-17 | Evaluator crashes after claiming notices, before send | At-most-once: those keys never re-send; credits stand; next batch sends nothing stale |
| RF-18 | Recipient has DMs closed | Notice marked undeliverable; no retry; standing still shows the credit |
| RF-19 | Same batch re-run (restart, replay) | `ON CONFLICT DO NOTHING` yields 0 new credits, 0 new notice keys, 0 DMs |
| RF-20 | Code deleted, same string recreated by another member, both eras have joins | First-seen inviter wins in the synthesized map; conflict logged; second era's joins credit first-seen inviter until an operator reviews — never silently split |
| RF-21 | Bot account joins via a code | No funnel join row exists; nothing to credit |
| RF-22 | Member with zero attributed joins runs `/invites` | Zeros + what-counts copy; not an error |
| RF-23 | Projection read fails mid-way | One "unavailable" ephemeral; no partial counts; no DM side effects |

## 6. Offline oracle

Pins the standing arithmetic and reply spine. The Python mirrors the
engine's thresholds (`referral_rewards.rs:35-37,156-175`) so a threshold
or title change fails here until both move together.

```pycon
>>> TIERS = [(1, "Member spotlight"), (3, "Profile flair"), (5, "Referrer status role")]
>>> def standing(total):
...     cur = next((t for m, t in reversed(TIERS) if total >= m), "No tier yet")
...     nxt = next(((m, t) for m, t in TIERS if total < m), None)
...     if nxt is None:
...         return cur, "Top tier reached"
...     return cur, f"{nxt[0] - total} more to {nxt[1]}"
>>> standing(0), standing(1), standing(2), standing(4), standing(5), standing(9)
(('No tier yet', '1 more to Member spotlight'), ('Member spotlight', '2 more to Profile flair'), ('Member spotlight', '1 more to Profile flair'), ('Profile flair', '1 more to Referrer status role'), ('Referrer status role', 'Top tier reached'), ('Referrer status role', 'Top tier reached'))

```

## 7. Smallest future slices, metric and kill rule

Build only after intent alignment at the 2026-11-02 review; each slice
is ≤1 day and independently revertible. The monetization gate is
untouched: tiers stay experiential, never paid.

1. **Baseline (no member surface).** Run `referral_counts` over stored
   funnel history and record credited-candidate joins/week for the last
   8 full weeks. If the median is 0, there is nothing to amplify:
   recommend alternative A and stop. Doubles as the before-picture for
   the metric below.
2. **Acceptance tests** for the funnel→`ReferralInput` projection and
   standing rendering (RF-01…RF-15, RF-20…RF-23), written against this
   spec by the test author; then **`/invites` compute-on-read handler**
   plus commands-doc regeneration and the normal command-publish gates.
3. **Credit + notice ledger and daily evaluator job** (RF-16…RF-19):
   migration, erasure-plan rows for both new tables, DM delivery through
   the existing seam, counts-only copy rendered from verdicts (§1
   correction 1).
4. **Metric exposure**: credited referrals/week (direct and bonus split)
   beside `weekly_active_humans` on the community scorecard, plus guard
   counts: rejections by reason, notice outcomes (sent / undeliverable /
   claimed-unsent), `/invites` reads.
5. **Tier perks are not in these slices.** Spotlight posting, flair and
   especially the referrer role grant (role hierarchy, revocation on
   recompute-drop per RF-14) need their own spec and gates.

**Metric and review.** Primary: credited referrals/week versus the slice-1
baseline; secondary: movement in weekly-active humans; guardrails: DM
undeliverable rate and any member complaints, rejected share of attributed
joins (a rising share suggests farming pressure). Credits lag joins by at
least 7 days, so reviews run **weekly starting 14 days after launch**;
a 7-day review would judge an empty pipe.

**Kill / scale rule.** At the 6-week review: if credited referrals
average <1/week across weeks 3–6 and `/invites` reads are near zero,
remove the notice job and the command (the engine returns to unwired;
ledger rows erase with the feature). If credited referrals sustain
≥2/week for 3 consecutive weeks, open the tier-perk spec (slice 5) and
revisit the leaderboard exclusion with that data — not before.
