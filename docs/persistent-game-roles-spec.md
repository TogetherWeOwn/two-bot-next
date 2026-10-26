# Onboarding: persistent game roles on rejoin

Status: **Proposed; design only**. Written 2026-10-26. No code, migrations,
role writes, settings or runtime tests are added. Merge publishes a proposal,
not permission to build or to restore any role. This indexed file is the
editable contract; a future acceptance decision must bind its exact revision.

## 1. Decision, alternatives and scope

Consumer: a returning member who left (or was removed and re-invited) and
today must re-open the game picker and re-tick every game to see the same
rooms again. Hypothesis: handing back the game roles they last saved makes a
return feel like a continuation, at the cost of one bounded automatic
role-grant path. Demand is unmeasured; no usage claim is made.

**Fixed scope.** Restore covers only the ten game-picker catalog keys
(`GAME_PICKS`, `crates/core/src/onboarding.rs:216`), resolved to the current
catalog role id at restore time. Never restored: moderation or staff roles
(any state, no exceptions), level-reward roles, platform picks
(`PLATFORM_PICKS` are not in the menu and `current_game_keys` scans games
only), self-role panel roles, event roles, `@everyone`, managed or
integration roles, and anything granted outside the picker by a human or
another bot. If a role cannot be reached through the picker today, restore
must not reach it either: restore grants a member nothing they could not
grant themselves with one picker submission.

| Option | Value and cost | Disposition |
| --- | --- | --- |
| A. No build: returners re-tick the picker | No new grant path or store; returners who never re-tick stay roleless and roomless | Default until acceptance; retain if the boundary below is not accepted |
| B. Offer only: welcome menu opens pre-ticked from a saved set, member submits to re-grant | Reuses the picker write path unchanged; no automatic grant; still needs the desired-state store in §2, and the value is lost for anyone who does not click | Fallback if the automatic-grant boundary in §4–§5 is not accepted |
| C. Automatic restore from a picker-written desired-state snapshot (§2–§6) | The member value arrives without a click; adds one snapshot store, one bounded add-only grant pass on the welcome path, and the safety/abuse rules fixed below | **Recommend for spec review**, not build |
| D. Restore from the latest `game_roles_selected` funnel row | No new store, but a deliberately cleared selection is indistinguishable from an unchanged one (§2), so it resurrects roles the member removed | Reject: wrong data, not merely stale data |
| E. Snapshot held roles when a goodbye is processed | Mirrors Discord truth including manual grants, but member-remove processing may have no authoritative role list (cache miss reads as unknown, never empty), so it would either guess or keep a second source of truth | Reject as the source; the picker snapshot already mirrors held game roles at every save |
| F. Restore every recorded role family (platform, self-roles, rewards) | One mechanism, four different trust stories; reward and panel state have their own earn/consent semantics | Reject: scope stays game roles only |

Mode scope for C: **legacy onboarding mode only**. Session mode suppresses
all role writes structurally (`game_picker_allowed` /
`level_role_writes_allowed`), so restore there would contradict the mode.
Anchor mode keeps the picker live but its welcome attaches nothing and its
copy is a frozen one-message shape ([onboarding-port.md](onboarding-port.md));
restore with no place to put the §6 notice is a silent grant, so anchor is
excluded until its copy contract is separately revised. Dry run suppresses
restore exactly as it suppresses picker role writes.

## 2. Verified baseline: the funnel rows are not desired state

Inspected repository head: `92d85042527b756062bfc9e19c8d77aed0cee0b3`.

- Granted game roles are recorded as funnel events:
  `record_game_selected` (`crates/core/src/onboarding_store.rs:218`) and the
  in-transaction variants write `game_roles_selected` rows whose metadata is
  `{"picks": [<catalog keys>]}` (`game_selected_row`,
  `crates/core/src/onboarding.rs:1367`). Keys, not role ids — a restore
  source must therefore resolve through the current catalog, never replay
  stored role ids.
- **A clear writes no row.** A raw empty submission removes held game roles
  and replies, before either recorder call
  (`adjudicate_game_select`, `crates/core/src/onboarding.rs:638`). So does a
  stale-menu answer. The latest row after a clear still names the cleared
  picks. Any design that restores "the latest recorded selection" restores
  roles the member deliberately removed. This kills option D.
- **A partial failure writes a subset under the same event type.**
  `record_game_partial_in_transaction` records only the grants that already
  flipped, with the same metadata shape as a whole-answer save. Funnel rows
  are a grant history for analytics, not a state projection; nothing in a
  row distinguishes "whole answer" from "partial".
- Reply-failed submissions retract their rows (`retract_picker_rows`), while
  the Discord role flips already happened. Even the funnel's own truth is
  "delivered selections", deliberately not "held roles".
- Leaving changes nothing recorded: a processed goodbye (any mode) deletes
  only the `onboarding_prompted` marker so a rejoin is welcomed again
  (`clear_onboarding_prompt`, `crates/core/src/onboarding_store.rs:181`).
  Today that welcome's menu opens from the member's **fresh** role list,
  which for a returner is empty.
- Retention does not age these rows out: the retention job deletes only
  voice-session event types from `events`
  (`crates/cutover/src/retention.rs:597`); `game_roles_selected` rows persist
  until opt-out purge or erasure ([privacy.md](privacy.md)).
- Role writes already have their seams: the picker serializes whole-answer
  writes per member (`{guild}:{member}:game_picker` advisory lock,
  `crates/bot/src/onboarding.rs:1021`) and applies them one at a time through
  the shared paced executor (`set_member_role`,
  `crates/discord/src/executor.rs:3310`), which performs **no** hierarchy
  check itself — Discord refuses what hierarchy forbids and the caller turns
  that into the role-failure reply. The preview path already implements the
  fresh-read manageability check restore needs (bot holds Manage Roles or
  Administrator; target exists, is unmanaged, sits below the bot's highest
  role; `crates/bot/src/onboarding.rs:1272`).

**Consequence.** C needs one new desired-state record per `(guild, member)`:
saved catalog keys, `updated_at`, and a last-restore stamp. It is written by
the picker, in the same transaction discipline as today's funnel writes:

| Picker outcome | Snapshot write |
| --- | --- |
| Whole-answer success | The recorded selected set (the member's new whole answer) |
| Clear (raw empty submission) | Empty set, `updated_at` advanced — "cleared" becomes durable truth distinct from "never picked" |
| Partial failure | `current_game_keys` over a fresh post-loop member read; if that read fails, snapshot unchanged (never guess) |
| Stale menu, dry run, total role failure, reply failure | Unchanged. On reply failure the snapshot already mirrors the Discord flips that did land; unlike the funnel it is **not** retracted, because it mirrors held roles, not delivered replies |
| Suppressed (opted-out) pair | Nothing, under the same suppression gate and shared lock order as `record_game_selected`; the Discord role effect is unchanged, exactly as today |

No backfill from funnel rows (§2, option D). A member with no snapshot —
including everyone on the day this ships — restores nothing; their next
picker save creates the snapshot. Manual role changes by staff or other
tools do not update the snapshot; the next picker submission re-converges
it. That accepted residual is bounded by §1: whatever restore re-grants,
the member could re-tick themselves.

## 3. Opt-out, erasure and expiry semantics

The snapshot is per-user persistence, so it inherits every rule in
[privacy.md](privacy.md) before it may exist:

- **Suppression first.** A suppressed `(guild, member)` pair gets zero
  snapshot writes (§2) and zero restore reads or role writes. The restore
  gate runs under the same table-lock discipline as the other onboarding
  seams, so an opt-out landing concurrently either wins before the read or
  waits for it — never purges a snapshot that a restore is mid-grant from
  and then lets the grant finish unnoticed: the grant pass re-checks
  suppression after its lock and stops without further writes.
- **Opt-out purges the snapshot.** The build adds the snapshot to the
  `opt_out_member` purge set (`crates/cutover/src/privacy.rs:78`) alongside
  `events`. An opted-out member is also never welcomed (the prompt guard
  refuses), and restore must not become a backdoor role effect for them.
  Opting back in restores nothing, matching the existing rule that opt-in
  does not resurrect purged rows; the member re-picks.
- **Erasure covers the snapshot.** The build adds a `(guild_id, member_id)`
  entry to the member-erasure manifest, so both the operator
  `erase-member` path and the queued `/privacy delete` intake delete it.
  Erasure while the member is away means a later rejoin restores nothing.
  Restore adds no Discord-side erasure obligation: a departed member holds
  no roles to remove, and erasure today does not reach into Discord.
- **Guild departure.** The snapshot must be removed by the same guild-purge
  path that removes other per-member guild rows when the bot leaves a
  guild; the build slice proves coverage with a fixture rather than
  assuming the manifest reaches it.
- **Expiry.** A snapshot is restorable for **365 days after `updated_at`**;
  at or after that instant the read treats it as absent (lazy expiry), and
  the retention slice physically deletes expired rows. Reads, restores and
  rejoins never extend the clock — only a new picker save does. Rationale:
  the underlying funnel rows already persist indefinitely, so a bounded
  snapshot is strictly less retention for the same facts; a member returning
  after a year re-ticks once. **Open to the acceptance decision:** 180 days
  if reviewers want the tighter bound; the mechanic is identical.

## 4. Restore eligibility and role-hierarchy safety

Restore runs inside the existing onboarding welcome job — no new scheduler,
service or HTTP client — under the same per-member picker lock, so it can
never interleave with a picker submission.

**Eligibility (all required, fail closed on any unknown):** configured
guild; legacy mode; not dry run; human member (`decide_prompt`'s bot skip
precedes everything); the stay's first prompt-eligible trigger — an ungated
join, or a membership-screening `pending: true -> false` transition — so a
still-gated member is never restored before clearing screening; a snapshot
exists, is unexpired, and the pair is not suppressed; and **at most one
restore per `(guild, member)` per rolling 24 hours** (§5). Restore
eligibility is independent of whether a landing channel is postable: with
no welcome post the roles still restore silently and the §6 notice is
simply never sent (never a DM instead).

**Per-role safety, from fresh reads, before each grant.** One fresh member
read, one fresh guild-role list, one fresh bot-member read — the preview
path's evidence shape. A pick is restored only if every check passes;
failures skip **that pick** and never abort the rest, except the bot-wide
permission failure, which skips the whole pass before any write:

| Check | Refusal when |
| --- | --- |
| Catalog | Key not in the current `GAME_PICKS` (retired/renamed key, platform key, smuggled value). Never resolve by role name |
| Existence | The catalog role id is absent from the fresh guild role list (deleted role). Never recreate a role |
| Bot permission | Bot holds neither Manage Roles nor Administrator → whole pass skipped, zero write attempts |
| Managed / everyone | Role is managed, or its id equals the guild id (`@everyone`) |
| Hierarchy | Role position **equal to or above** the bot's highest role (equal refuses, matching [moderation-hierarchy-order.md](moderation-hierarchy-order.md)); Administrator does not bypass hierarchy |
| Escalation bits | Role's permission mask carries Administrator or Manage Roles. The picker grants these same roles today without a runtime mask check (preflight covers it offline); an *automatic* grant adds this one guard so a role re-purposed since cataloguing cannot be auto-granted. A role failing only this check is a loud operator signal, not a silent skip |
| Already held | Not a refusal: no write, counts as present |

Duplicate snapshot keys dedupe and grants run in catalog order; more than
ten keys (impossible through the picker) truncate to the catalog — dedupe
and catalog resolution already bound the pass to **at most 10 writes**.
Writes are single-attempt through the existing paced executor (110 ms
lane): a Discord refusal or error on one pick marks it unrestored and the
pass continues; no retry loop. The check-then-write window (a role moved
or deleted between read and write) is accepted — Discord is the final
enforcer, exactly as for the picker today. Restore is **add-only**: it
never removes a held game role that is absent from the snapshot (the
member may hold roles from Discord-side onboarding); the picker remains
the only remove-to-match path. Restore failure of any kind never blocks,
delays beyond the shared worker budget, or retracts the welcome.

Every skip is counted in logs/metrics by fixed reason code
(`catalog`, `missing`, `managed`, `hierarchy`, `escalation_bits`,
`bot_permission`, `discord_refused`) with no member content; the public
copy names none of them (§6).

## 5. Leave/rejoin cycling and abuse rules

What cycling could buy is small by construction — the roles are self-serve
through the picker without leaving at all — so the rules bound cost and
surprise, not a privilege boundary:

- **Once per stay, once per day.** Restore runs at most once per stay (the
  §4 first eligible trigger) and at most once per `(guild, member)` per
  rolling 24 hours, stamped on the snapshot when a pass completes. A
  second rejoin inside the window gets the ordinary welcome and picker
  with fresh-role defaults; manual re-ticking always remains available,
  so the cooldown costs a genuine returner at most one picker submission.
  `now == last_restore + 24 h` is eligible again (the window is
  `[last, last + 24 h)`).
- **No amplification beyond the welcome.** A cycle already costs one
  welcome post today (the prompt guard clears on goodbye). Restore adds
  zero messages (§6) and at most 10 already-paced role writes, once a day.
  It writes no funnel rows: a restore is not a selection, and writing
  `game_roles_selected` would double-count the member's original picks in
  the funnel and corrupt the very record §2 relies on staying honest.
- **Staff removals.** If staff removed a game role as a soft sanction,
  restore (or the picker, today) can return it. That is inherent to
  self-serve roles and is stated plainly rather than engineered around:
  game roles must never be used as a sanction or access-control mechanism
  for anything that matters — moderation state, timeouts, bans and gated
  areas must not key off picker roles. Restore also never interacts with
  bans: a banned member cannot rejoin to trigger it, and a kick followed
  by a rejoin restores only picker roles.
- **Raids and new accounts.** Accounts without a snapshot restore nothing,
  so bulk first-joins add zero role writes. Returning accounts still pass
  membership screening and the existing anti-raid surfaces first; restore
  adds no path around either, and per-member cooldown plus the shared
  pacing lane bound the write rate.
- **Crash and replay.** The durable welcome job may redeliver. Restore is
  idempotent by construction (already-held picks are no-ops) and the
  24-hour stamp is written only after the write loop finishes, so a crash
  mid-pass leaves the member eligible and the retry re-attempts only the
  missing picks. Discord acceptance versus the stamp commit is the same
  cross-system ambiguity the welcome marker already carries; a duplicate
  add is a harmless no-op, and no exactly-once claim is made.

## 6. Rejoin notice copy and bot-noise budget

Restore runs **before** the welcome menu defaults are built, so the
existing fresh-role defaults open the menu ticked with the restored roles
and post-grant routing stays truthful. The notice rides inside the one
welcome post that already exists.

**Noise budget (hard bounds):** 0 additional messages of any kind (no
second post, no DM, no thread, no edit after the fact); at most **1 added
line** inside the existing welcome post; 0 additional pings — the welcome
already mentions exactly the member, and restored picks are rendered as
plain-text catalog **labels**, never role mentions (a role mention would
ping every holder); at most 10 role-write calls and 1 snapshot read. When
zero picks are restored (no snapshot, expired, all skipped, cooldown),
the welcome is byte-identical to today's — no line, no marker.

**Copy.** Today's legacy welcome is:

```text
<@member> welcome to TWO.

Pick what you play below and I will open the right channels for you.
You can change this any time, and there is an intro thread in <#…> if you want one.
```

With at least one pick restored, exactly one line is inserted after the
greeting, labels in catalog order, comma-separated:

```text
<@member> welcome to TWO.
Welcome back — I put your game roles back (Shooters, Horror).

Pick what you play below and I will open the right channels for you.
You can change this any time, and there is an intro thread in <#…> if you want one.
```

The line names only picks that were actually granted or already held;
skipped picks (§4) are visible only as unticked menu entries, never named
publicly — hierarchy and catalog failures are operator facts, not member
copy. The copy does not claim channels were opened: routing still happens
through the member's own picker submission, and restore records no
`channel_routed` row. Suppressed members get no line because they get no
welcome and no restore (§3).

## 7. Design matrix and offline oracle

All rows are **planned acceptance cases, not runtime evidence**. The
future Test Author pins them against the built callers (disposable
database, recording Discord double, injected clock); here they fix the
intended outcome of each interaction the card asks about.

| Case | Setup | Required outcome |
| --- | --- | --- |
| PR-01 | Saved {shooters, horror}, leaves, rejoins ungated | Both roles granted once, before menu defaults; welcome carries the §6 line naming both labels; no other post |
| PR-02 | Saved, then **cleared all** in the picker, leaves, rejoins | Empty snapshot: zero writes, no notice line, byte-identical welcome |
| PR-03 | Picker partial failure (1 of 3 grants landed), leaves, rejoins | Snapshot mirrors the post-loop fresh held set; only that set restores; never the unlanded picks |
| PR-04 | No snapshot (never picked, or pre-feature member with only funnel rows) | Zero writes; funnel rows never consulted |
| PR-05 | **Opt-out** after saving, then rejoin | Snapshot purged at opt-out; zero writes, no welcome, no notice; opt-in later restores nothing |
| PR-06 | Opt-out lands while a restore pass is queued | Suppression re-check stops the pass; at most the already-sent single writes stand (they were lawful when sent); no notice |
| PR-07 | **Erasure** (operator or `/privacy delete`) while away, then rejoin | Snapshot deleted by the manifest; zero writes |
| PR-08 | Snapshot older than the §3 expiry at rejoin | Treated as absent; zero writes; rejoin does not extend the clock |
| PR-09 | One catalog role **deleted** on Discord, one intact | Intact pick restored; deleted pick skipped (`missing`), unnamed in copy, never recreated |
| PR-10 | Saved role moved **equal to/above** the bot's highest role | Skipped (`hierarchy`); other picks restore; Administrator on the bot does not change this |
| PR-11 | Saved role became managed, or snapshot value equals the guild id | Skipped (`managed`); zero writes for that pick |
| PR-12 | Bot lost Manage Roles and Administrator | Whole pass skipped before any write attempt; welcome still posts without the line |
| PR-13 | Saved role re-purposed with Administrator/Manage Roles permissions | Skipped (`escalation_bits`) and logged loudly; picker behaviour unchanged (separate preflight concern) |
| PR-14 | Snapshot holds a retired key, a platform key, duplicates, a moderation role id | Each drops at catalog resolution; duplicates grant once; catalog order preserved; ≤10 writes |
| PR-15 | **Rapid cycling**: restore, leave, rejoin 1 hour later | Second rejoin: zero restore writes (24 h cooldown), ordinary welcome + picker; manual submission still works |
| PR-16 | Rejoin exactly at `last_restore + 24 h`; and one second before | At the boundary: eligible. Before: cooldown applies |
| PR-17 | Gated rejoin (`pending: true`), gate clears later | No writes while pending; restore runs on the gate-clear trigger, once |
| PR-18 | Bot account joins with a (fixture) snapshot | Zero writes; the bot skip precedes every check |
| PR-19 | Session mode; anchor mode; dry run in legacy mode | Zero writes in all three; welcome behaviour unchanged from today |
| PR-20 | No postable landing channel (welcome suppressed) | Roles restore silently; no DM or substitute notice anywhere |
| PR-21 | One grant refused by Discord mid-pass (403), others succeed | Refused pick skipped and unnamed; notice names only granted/already-held labels; welcome unaffected |
| PR-22 | Welcome job redelivered after a crash mid-pass | Re-run grants only the still-missing picks; stamp written once the loop completes; no second notice line |
| PR-23 | Member already holds a saved role via Discord-side onboarding | No write for it; it counts as present and may be named in the line; held non-snapshot game roles are never removed |
| PR-24 | Picker submission races the restore pass | The shared per-member lock serializes them; final state equals the later whole answer; no interleaved half-sets |
| PR-25 | Foreign guild join; snapshot exists only in the configured guild | Zero writes; guild isolation holds on every read |

Offline document check, from the repository root (no credentials, network
or database). The doctest exercises a pure reference model of the §3–§5
decision rules and the matrix's completeness; it proves the **specified
rules are internally consistent**, not any SQL, REST or deployed behaviour:

```sh
python3 -m doctest -v docs/persistent-game-roles-spec.md
python3 scripts/check_docs_index.py --json
```

```pycon
>>> import pathlib, re
>>> text = pathlib.Path("docs/persistent-game-roles-spec.md").read_text()
>>> rows = re.findall(r"^\| (PR-\d{2}) ", text, re.M)
>>> assert rows == [f"PR-{n:02}" for n in range(1, 26)]
>>> CATALOG = {"shooters": "r1", "survival": "r2", "horror": "r3"}
>>> EXPIRY_S, COOLDOWN_S = 365 * 24 * 3600, 24 * 3600
>>> def plan(now, snap, held, roles, bot):
...     # snap: None or (keys, updated_at, last_restore); roles: id -> (managed, pos, perms)
...     # bot: (highest_pos, can_manage); returns (writes, present, skipped_reason_codes)
...     if snap is None:
...         return [], [], []
...     keys, updated, last = snap
...     if now >= updated + EXPIRY_S:
...         return [], [], []
...     if last is not None and now < last + COOLDOWN_S:
...         return [], [], []
...     if not bot[1]:
...         return [], [], ["bot_permission"]
...     writes, present, skipped, seen = [], [], [], set()
...     for key in keys:
...         if key in seen:
...             continue
...         seen.add(key)
...         rid = CATALOG.get(key)
...         if rid is None:
...             skipped.append("catalog"); continue
...         role = roles.get(rid)
...         if role is None:
...             skipped.append("missing"); continue
...         managed, pos, perms = role
...         if managed:
...             skipped.append("managed"); continue
...         if pos >= bot[0]:
...             skipped.append("hierarchy"); continue
...         if perms & 0x8 or perms & 0x10000000:  # Administrator | Manage Roles
...             skipped.append("escalation_bits"); continue
...         (present if rid in held else writes).append(key)
...     return writes, present, skipped
>>> SAFE = {"r1": (False, 3, 0), "r2": (False, 4, 0), "r3": (False, 5, 0)}
>>> BOT = (9, True)
>>> snap = (["shooters", "horror"], 1000, None)
>>> assert plan(2000, snap, set(), SAFE, BOT) == (["shooters", "horror"], [], [])
>>> assert plan(2000, snap, {"r1"}, SAFE, BOT) == (["horror"], ["shooters"], [])
>>> assert plan(2000, ([], 1000, None), set(), SAFE, BOT) == ([], [], [])  # cleared
>>> assert plan(2000, None, set(), SAFE, BOT) == ([], [], [])
>>> assert plan(1000 + EXPIRY_S, snap, set(), SAFE, BOT) == ([], [], [])  # expired at boundary
>>> assert plan(1000 + EXPIRY_S - 1, snap, set(), SAFE, BOT)[0] == ["shooters", "horror"]
>>> recent = (["shooters"], 1000, 5000)
>>> assert plan(5000 + COOLDOWN_S - 1, recent, set(), SAFE, BOT) == ([], [], [])
>>> assert plan(5000 + COOLDOWN_S, recent, set(), SAFE, BOT)[0] == ["shooters"]
>>> assert plan(2000, snap, set(), SAFE, (9, False)) == ([], [], ["bot_permission"])
>>> assert plan(2000, (["shooters"], 1000, None), set(), {}, BOT)[2] == ["missing"]
>>> assert plan(2000, (["pc", "shooters", "shooters"], 1000, None), set(), SAFE, BOT) == (["shooters"], [], ["catalog"])
>>> assert plan(2000, (["horror"], 1000, None), set(), {"r3": (True, 5, 0)}, BOT)[2] == ["managed"]
>>> assert plan(2000, (["horror"], 1000, None), set(), {"r3": (False, 9, 0)}, BOT)[2] == ["hierarchy"]
>>> assert plan(2000, (["horror"], 1000, None), set(), {"r3": (False, 5, 0x8)}, BOT)[2] == ["escalation_bits"]
>>> print("25 planned cases; reference model only; no roles restored")
25 planned cases; reference model only; no roles restored

```

## 8. Review gates, future build slices and undo

**Disposition: DESIGN_ONLY, no grant.** Before any build card or protected
test handoff: a recorded acceptance of this exact revision (option C over
A/B; legacy-only mode scope; the 365-day expiry versus 180; the automatic
grant with its §4 checks including the escalation-bits guard; the 24-hour
cooldown; the one-line notice and zero-message budget), plus an independent
security disposition of the automatic-grant boundary. A merged spec, a
quiet deadline or a routing acknowledgement is not acceptance; a material
revision rebinds the decision. If the automatic boundary is rejected, fall
back to option B (which still needs §2–§3) or keep A.

Only **after** those gates, cut each slice to at most one working day;
split again rather than dropping a control. None is created here:

| Slice | Output / exit; dependency |
| --- | --- |
| 0. Acceptance record | Exact-revision decision on §1–§6 dispositions above; feeds slices 1–5. No code |
| 1. Protected oracles | Test Author's acceptance tests for PR-01…PR-25 in bounded groups, meaningful RED against the real callers (disposable DB, recording Discord double, injected clock); slice 0 |
| 2. Snapshot store | Additive migration + store: picker-transaction writes per the §2 table (including clear and partial), suppression gate, opt-out purge entry, erasure-manifest entry, expiry read; no restore consumer yet (dark); slice 1 |
| 3. Restore planner | Pure eligibility/catalog/hierarchy/cooldown planner and notice renderer with unit tests over the §7 model; consumes only slice 2's read type; slice 2 |
| 4. Welcome-path wiring | Trigger/order/stamp in the existing welcome job, paced add-only writes under the picker lock, notice line, reason-code metrics; slices 2–3 |
| 5. Retention, docs, staging | Expired-snapshot retention entry, guild-purge coverage fixture, operator-guide and privacy updates, staging leave/rejoin synthetic; slice 4. Split retention from staging if either outgrows a day |

Design metric for the acceptance review: every row of PR-01…PR-25 and
every disposition listed above has an explicit accepted / change /
rejected record, or C stays no-build. That measures design completeness,
not demand or engagement. Unverified by this document: deployed mode and
permissions, real returner demand, live guild role positions, and every
runtime behaviour proposed above.

ACT: publish this indexed Proposed specification. WHY: fix the restore
scope, privacy semantics, hierarchy safety, cycling rules and noise budget
before any automatic role grant is considered, and record why the existing
funnel rows cannot be its source. UNDO: reviewed docs revert; no runtime,
role, schema or data change exists. A future build must ship the snapshot
dark (slice 2) before any restore runs, and disabling restore later must
keep opt-out purge, erasure and expiry working rather than delete the
snapshot early. No live operation occurred.
