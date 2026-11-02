# LFG signup reminders: one quiet pre-start heads-up

Status: **Declined: no build** (owner decision 2026-11-01). This doc stays as
the record; reopen only with a named host problem. Original proposal: written
2026-10-26. **Choose no build now.** This proposal adds no commands, sends,
schedules, preferences, migrations or runtime behavior. Merge records a design,
not acceptance, host consent, security disposition or activation. This is the
sole editable repo spec; decision records must bind its exact commit and
SHA-256.

## 1. Decision, evidence and alternatives

Problem hypothesis: signed-up members no-show game nights they would attend
with one quiet heads-up before start. No TWO no-show frequency, reminder demand
or benefit measurement is established. No live audience, campaign or experiment
was used. Keep the current silence until explicit host intent and independent
security/consent disposition justify anything more.

Sources read 2026-10-26: [Apollo reminders](https://docs.apollo.fyi/reminders)
documents default opt-in attendance reminders with a 15-minute default interval,
delivered in an event thread or optionally the event channel, never a DM;
[Sesh's manual](https://sesh.fyi/manual/) documents opt-in DM reminders chosen
by the attendee after RSVP plus organizer-set channel notifications with
start-relative fire times. These support workflow comparisons, **not** a claim
that either product implements this exact LFG reminder or that TWO needs one.

| Option | Incremental benefit / uncertainty | Decision |
| --- | --- | --- |
| A. No build: retain current silence | No additional noise, member data or send state; signups can still no-show | **Selected now**; lowest cost with unproven demand |
| B. One quiet pre-start reminder in the post's channel | One heads-up per opted-in post; still not guaranteed to reach players and visible to non-signups | Conditional reopening contract below, **not selected for build** |
| C. Per-user schedules, DM reminders or member-level mute | Could target individuals; needs new personal choices, consent/DM infrastructure, delivery/erasure policy and fanout | Not selected; signup/RSVP/reminder consent is not consent to this |

Scope is exactly **one LFG post**, identified by `(guild_id, lfg_id)`. Do not
bridge to website events, RSVP rows, recurring schedules, series, threads or
inferred attendance. New commands beyond a create-time reminder flag, per-user
reminder preferences, LFG create/edit/close/find changes, Discord-event reminder
changes, migrations, spend, production activation and implementation are out of
scope. Silence is **unknown**, never absence or a reason to remove a signup.

## 2. Source map: observed baseline, not future capabilities

Inspected source: `fb11f134ac002a8a6fa6e9982c7b1de03734fbb1`. Source inspection
only; no deployment, message, live member list or database was inspected.

| Seam | Evidence and limit |
| --- | --- |
| No LFG reminder exists | `scheduled_jobs.rs` has no `lfg` reference; `website_jobs.rs:401` only registers the `lfg_sweep` job. No `remind` reference exists in `crates/core/src/lfg.rs`, `lfg_store.rs`, `lfg_list.rs`, `crates/discord/src/lfg_interactions.rs`, `lfg_list.rs` or `crates/bot/src/lfg_jobs.rs` |
| Event-reminder sibling | [Tick](../crates/bot/src/scheduled_jobs.rs#L157-L177) runs schedules then [reminders](../crates/bot/src/scheduled_jobs.rs#L226-L278): fixed [15-minute lead](../crates/core/src/event_reminder_store.rs#L16-L16), [<=2 events/tick](../crates/core/src/event_reminder_store.rs#L18-L18), permanent [`event:reminder:<guild>:<event>` fence](../crates/core/src/event_reminder_store.rs#L76-L124) with NULL actor, [10,000 attempts/guild](../crates/core/src/event_reminder_store.rs#L20-L20). Attempt precedes rendering/POST; results are separate |
| Quiet hours do not exist | The [reminder-preference proposal](event-reminder-preferences-spec.md) withdrew per-member quiet hours: do not implement. There is no quiet-hours store, schedule or enforcement to interoperate with; see section 4 |
| LFG identity/storage | [Schema](../crates/cutover/migrations/0170_lfg.sql#L12-L42): ID, guild, channel, nullable message, start, open/closed, creator, roles (slots 1-99), signups. No reminder flag, notice preference or cancellation cause |
| Signup/leave | [Signup](../crates/core/src/lfg_store.rs#L503-L580) is one advisory-locked transaction; [leave](../crates/core/src/lfg_store.rs#L583-L590) deletes separately; [signups list in join order](../crates/core/src/lfg_store.rs#L477-L497). Signup/leave need no elevated permission; see [LFG domain](lfg-port.md) |
| Post roster is public | [`lfg_body_text`](../crates/core/src/lfg.rs#L537-L559) renders signup identities on the post. A reminder names nobody new, but must still not copy the roster into audits or new stores |
| Edit authority | [Router](../crates/core/src/router.rs#L734-L753): LFG commands sit behind the announcements gate with Manage Events required. Creator identity alone is **not** management authority |
| Mention suppression | [POST builder](../crates/discord/src/executor.rs#L3140-L3187) and [refresh builder](../crates/discord/src/executor.rs#L2938-L2977) suppress mentions independently; rendered mentions do not ping under the closed allowed-mentions policy |
| Past-start sweep | [`lfg_sweep`](../crates/bot/src/lfg_jobs.rs#L14-L35) runs every 60 s under the announcements gate and closes posts [15 min past start](../crates/discord/src/lfg_interactions.rs#L78-L83), sharing [<=20 rows](../crates/discord/src/lfg_interactions.rs#L677-L727) per visit with closed-message refresh repairs. Those retries edit only the saved closed message; they do not authorize reminder POSTs. Close is shared expiry/orphan state, not cancellation proof |
| Opt-out scope | [Privacy](privacy.md) member opt-out is analytics-only; reminder SQL reads no preference store. Leaving a signup is the only current per-post exclusion |
| Existing erasure | [Manifest](../crates/cutover/src/member_erasure_plan.json#L19-L32) removes creator-owned posts and signup identities; new retained identifiers need their own disposition, not an assumed exception |

No live post, permission, deployment, external shared-database writer or host
workflow was inspected. If required writer/erasure integration cannot be small
and coherent, stop this proposal instead of weakening existing fences.

## 3. Conditional B: trigger, audience, channel and authority

Everything in sections 3-7 is a **future acceptance contract**, not a new API or
available command.

- **Trigger:** one fixed T-minus **15 minutes**, matching the existing event
  lead; the first tick inside the window admits. No per-user schedules, custom
  offsets, snooze or second ping. A post whose start moves before admission
  simply moves its window; a move after admission never creates a second fire.
- **Audience:** current signups at admission time. Read a candidate snapshot,
  then revalidate the full ordered roster inside admission (event-reminder
  pattern: `user_id`-ordered array equality plus exact count, bounded to the
  maximum accepted roster of 20 roles × 99 slots = 1,980 signups; sibling
  order, not LFG join order). A join or leave between the candidate read and
  admission changes the roster, so that admission attempt refuses with zero
  POST and the post stays undecided; the next regular tick re-snapshots from
  scratch (no catch-up job, no delayed queue). Only changes after a committed
  admission hit the miss/in-flight limits: signup after committed admission
  misses this fire; withdrawal after committed admission cannot recall an
  in-flight message. Zero signups at admission means no send.
- **Channel:** one short message in the post's saved guild text channel,
  addressed to nobody: no user/role/everyone/here mention, no roster, no
  attendee count, no title, no DM, thread, webhook or fallback channel.
  Consent analysis: DMs need per-recipient opt-in consent infrastructure that
  does not exist (Sesh DMs are attendee-chosen; Apollo defaults to thread or
  channel). A channel message inherits post visibility, which signups already
  accepted by joining a public post, but it **cannot honor a person's desire
  not to see it**, even if they leave the signup or opt out of analytics.
  If individual exclusion or per-recipient authorization proof is required,
  reject B and keep A.
- **Organizer authority:** per-post opt-in at creation, **default off**, stored
  as post policy. The policy is fixed at creation: this proposal adds no
  operation to change or revoke it later, and any future change operation
  would require current channel-resolved Manage Events plus its own design.
  If consent disposition requires revocability before fire, design that
  operation first or keep A. Unknown/disabled policy means **no send**. Existing announcements
  enablement is guild routing consent, not this purpose's consent. The policy
  persists with the post like the post itself: organizer departure does not
  cancel it, but destination access is revalidated at every admission (section
  4). A bot, service key, saved component or signup cannot supply organizer
  intent.

Fixed body (render Discord-local full dates, not ambiguous clock-only or
relative-only values; UTC instants included here to make fixtures reproducible):

- Reminder: `Starting soon: <t:1793493000:F>. Details in the original post:
  <validated Discord jump link>.` Instant: 2026-11-01T00:30:00+00:00, firing
  for a 00:45 start. No title (a title edit after admission must not stale the
  message), no count (signup races must not stale it), no roster.
- Quiet case: post not opted in, zero signups, or window not yet reached. No
  message, no audit row beyond existing tick health, no queue entry.
- Suppressed case: opted in but closed, edited start now outside the window,
  deleted, message unbound, destination unreadable or policy unknown at
  admission. Terminal no-send for this fire; the reason is recorded without
  member identities.

One fixed-format message <=1,000 UTF-16 units; invalid/oversized content
refuses, never truncates a timestamp, splits a burst or substitutes a roster.
Closed mentions on every output: `allowed_mentions` has empty `parse`, `users`,
`roles` and `replied_user=false`; no reply reference or link embed. Suppress
embeds and request notification suppression, but do not promise client silence.

## 4. Quiet bounds, races, restart and scheduler cost

**Quiet-hours interplay:** there is no quiet-hours design to interoperate with;
the withdrawn proposal must not be revived as hidden state. "Quiet" here is
purely behavioral: channel-only, mention-free, one fire per opted-in post per
lifetime, hard caps below. No time-of-day suppression, timezone collection or
daily rule is created. If a host later requires time-of-day suppression, that
needs its own accepted design; do not smuggle it into this one.

Proposed hard bounds for B: <=1 reminder per post lifetime, <=6 per guild
per rolling hour, and one shared per-tick reminder admission allowance: the
existing event-reminder phase and this LFG phase together admit at most 2
per tick, event reminders keeping their current priority, so an LFG post
admits only from unused allowance (never 2 + 2 in one tick). Unknown/refused post-admission attempts consume
quota. No delayed catch-up, retry queue or automatic send after restart. At
most 10,000 retained reminder decisions per guild under a dedicated action
name; at capacity stop rather than recycle replay guards. Caps are part of
requested host intent, not current runtime constants.

Admission must atomically compare current open status, start instant, channel
and message binding, check caps, and commit one unique attempt keyed by
`(guild_id, lfg_id, purpose=lfg_prestart_reminder)`. Only the process that
knows it newly committed admission may make one wire attempt. Unknown DB
commit result authorizes no send. After committed admission, state is
`attempted -> sent | failed | uncertain`; persist `sent` only with a validated
Discord message ID. Lost response, crash before/after wire, timeout, 429/5xx,
unavailable receipt store or restart leaves a terminal decided post: **never
re-arm or automatically resend**, even if history finds nothing. Disable
transport retries for this effect. `sent` means Discord accepted, not read.

- **Edit before fire:** start edit moves the window; title/role edits do not
  disqualify. Admission re-reads start/channel/binding, so a stale candidate
  snapshot cannot send to a moved post.
- **Closed/cancelled/deleted before fire:** closed status, deleted row or NULL
  (unbound) message at admission refuses with zero POST. Expiry/orphan closure
  must never be described as organizer cancellation.
- **Signup/withdrawal races:** the admission roster comparison is the
  serialization point. Join or leave between the candidate read and admission
  mismatches the roster, so that attempt refuses with zero POST and the post
  stays undecided for the next regular tick to re-snapshot. Join after a
  committed admission misses; leave after a committed admission cannot recall
  an in-flight message. The message names nobody, so no race outcome leaks or
  misattributes identity; disclose the race instead of holding DB locks across
  HTTP. Sustained churn can defer admission past the window; the post then
  stays quiet under `out_of_window`, with no catch-up.
- **Restart/double-fire:** the permanent fence makes duplicate ticks, duplicate
  workers and crash-replay return the saved decision with **no new POST**.
  Backup/restore must preserve decided attempts before enabling any sender;
  uncertainty after restore fails closed, never infers delivery absence.
- **Permission loss:** bot cannot view/read/send at admission, channel foreign
  or unsupported, source message deleted, or binding changed all refuse with
  zero POST and no fallback destination.
- **AI-presence budget:** at most one short message per opted-in post per
  lifetime, inside the card-stated community <20% AI-presence goal. No
  measurement is claimed; a future build must show reminder volume against
  that goal before activation.

Bounded scheduler cost: reuse the existing supervised tick (same pattern as
the event-reminder phase) or the 60-second `lfg_sweep` visit; no new timer,
task or queue. Per tick: one bounded candidate query plus admission
transactions only within the shared allowance above (combined with the
event-reminder phase, at most two per tick), each comparing a bounded ordered
user-ID array plus an exact count. No per-signup fanout, no roster materialization beyond the bound,
no lock held across Discord HTTP. Existing tick budgets, sweep row limits and
LFG message budgets are unchanged.

## 5. Audit, retention, privacy and preserved fences

Keep reminder state separate from scheduled-event reminders. Never
delete/rekey/reset `event:reminder:<guild>:<event>` after an LFG start edit,
close, erasure or mirror refresh; an LFG reminder must not create or replay a
scheduled-event reminder, and vice versa.

A adds no retained data. B would add a small amount of linkable state and needs
independent CISO disposition:

- The attempt fence carries NULL actor and only the post target, mirroring the
  event-reminder attempt fact. Keep it through member erasure, post deletion
  and backup/restore. Do not repurpose it to retain member identities or
  delete it to resend. **Actor-free is not a claim of anonymity**: LFG IDs can
  encode interaction IDs and guild/post/channel/message links can identify.
- Per-post opt-in policy is organizer/post metadata, not a member preference;
  it is removed with the post under existing post erasure. No member-level
  mute, timezone, schedule or suppression marker is created.
- Audits and diagnostics must never contain signup user IDs, the roster, the
  title or rendered content. Unknown visibility is `UNAVAILABLE`, never
  evidence of no audience. Zero signups does not prove zero readers.
- Erasure cannot recall Discord messages, including ones already admitted or
  in flight. Deleted posts never recreate; delayed inputs must reauthorize a
  present post.

Preserved fences: announcements gating, Manage Events management authority,
mention suppression, message-size budgets, sweep lifecycle semantics, the
permanent event-reminder fence, analytics-only opt-out scope and existing
erasure executors. If minimal retained facts cannot satisfy erasure and
no-replay together, keep A.

## 6. Agent handoff

All outcomes reply privately (or, for tick effects, to logs/audits only). No
endpoint/command is added by this document. The future adapter contract below
is illustrative JSON; its IDs are synthetic:

```json
{"version":1,"family":"lfg","purpose":"lfg_prestart_reminder","post_id":"lfg-123","guild_id":"456","dry_run":true,"outcome":"preview","reason":"in_window","next_action":"wait_for_fire","sent":false}
```

| Outcome / reason | Meaning and next_action |
| --- | --- |
| `quiet / not_opted_in` | Post policy off or unknown; `none` |
| `quiet / out_of_window` | Window not reached or start passed unadmitted; `none` |
| `quiet / no_signups` | Zero signups at admission; `none` |
| `suppressed / post_closed` | Closed before admission; terminal, `none` |
| `suppressed / post_deleted` | Row gone before admission; terminal, `none` |
| `suppressed / message_unbound` | NULL/changed message binding; terminal, `none` |
| `suppressed / destination_refused` | Channel unreadable/foreign/unsupported or access lost; `none` |
| `suppressed / RATE_LIMITED` | Tick/hour/retention cap hit; no delayed job, `none` |
| `sent / accepted` | Discord accepted the channel message; reading is not known; `none` |
| `failed / attempted_not_delivered` | Known refusal after admission; decided, no retry; `inspect_decision_no_resend` |
| `uncertain / delivery_unknown` | A message may have been sent; do not resend; `inspect_decision_no_resend` |
| `replay / decided` | Return original decision and receipt if known, including uncertainty; `none` |

Every outcome includes stable post/guild IDs when disclosure is authorized and
`outcome`, `reason`, `next_action`, `sent` (true/false/null for uncertain).
Redact IDs in denied responses. Handoff also carries exact spec commit/hash
and protected file hashes from the check below; a changed hash requires source
reinspection, not automatic approval.

## 7. Deterministic offline acceptance matrix

These are **planned assertions**, not executed runtime tests. Future fixtures
use fake UTC clocks, synthetic guilds/actors/channels, deterministic post
stores, controllable permission/policy reads and a recording one-shot Discord
transport. Store races/restarts require independent connections to disposable
test DBs only. Each case asserts JSON reason/next_action plus exact SQL/HTTP
effect counts.

| Case | Required result |
| --- | --- |
| M01 trigger window / DST boundary | First tick inside T-15 admits; equal instants across offsets/DST admit identically; no personal timezone collection |
| M02 signup after snapshot | Join between candidate read and admission mismatches the roster; that attempt refuses with zero POST; post undecided, next regular tick re-snapshots; no catch-up job |
| M03 withdraw after snapshot | Leave before admission mismatches the roster; that attempt refuses, next tick re-snapshots (zero signups then means no send); leave after committed admission cannot recall in-flight; neither leaks identity |
| M04 edit before fire | Start edit moves window; title/role edits do not disqualify; stale snapshot cannot send to moved post |
| M05 close/cancel/delete before fire | Closed/deleted/unbound at admission refuses with zero POST; expiry never described as organizer cancellation |
| M06 no quiet-hours rule | No time-of-day suppression, timezone store or daily schedule exists; quiet is behavioral bounds only |
| M07 restart / duplicate workers | Retained fence returns saved decision; <=1 wire attempt per post lifetime; no automatic startup send |
| M08 double-fire prevention | Same key retry returns saved outcome; no second admission, POST or re-arm after sent/failed/uncertain |
| M09 per-post / per-guild caps | Exact limits and one-over: <=1/post, <=6/guild/hour, shared <=2/tick across event-reminder and LFG admissions combined; terminal attempts consume quota |
| M10 permission loss | Lost bot view/read/send, foreign channel, deleted source or changed binding refuses; no fallback destination |
| M11 organizer policy | Default off refuses; opt-in needs Manage Events; unknown policy refuses; signup cannot enable; policy fixed at creation (no later change/revoke operation exists in this proposal) |
| M12 opt-out / leave | Leave removes future eligibility; no member mute exists; channel visibility to leavers is disclosed |
| M13 DM / mention refusal | No DM, mention, roster, count, title or thread even when requested; fixed format only |
| M14 roster privacy | No signup IDs, roster, title or content in audits/diagnostics; denied responses redact IDs |
| M15 erasure / restore | Erasure removes post policy with post, keeps actor-free fence; restore preserves decisions or sender stays disabled |
| M16 reminder isolation | LFG fire/move/close/restart never alters the permanent event-reminder fence or creates a second reminder |

## 8. Smallest future slice, check recipe and decision gate

If this design is ever accepted AND host-aligned, the smallest sizing sketch
(not a commitment, estimate or build authorization) is: one post-policy flag
with default-off creation wiring, one permanent attempt fence reusing the
existing audit table, one bounded candidate-plus-admission step on the existing
tick, one fixed-format renderer with closed mentions, and offline fixture tests
for the matrix above. Anything needing per-user state, DMs, quiet-hours
storage or a new timer is a different, larger proposal.

From the repo root, offline (Python standard library only):

```sh
python3 scripts/check_docs_index.py --json
python3 - <<'PY'
from pathlib import Path
import hashlib, json, re, subprocess
root = Path.cwd().resolve()
spec = root / "docs/lfg-reminder-spec.md"
text = spec.read_text()
links = re.findall(r"\]\(([^)\s]+)\)", text)
checked = 0
for link in links:
    if "://" in link or link.startswith("#"):
        continue
    name, _, anchor = link.partition("#")
    path = (spec.parent / name).resolve()
    assert path.is_relative_to(root) and path.is_file(), link
    if anchor.startswith("L"):
        match = re.fullmatch(r"L(\d+)(?:-L(\d+))?", anchor)
        assert match, link
        start, end = int(match[1]), int(match[2] or match[1])
        assert 1 <= start <= end <= len(path.read_text().splitlines()), link
    checked += 1
cases = re.findall(r"^\| (M\d{2}) ", text, re.M)
assert cases == [f"M{i:02}" for i in range(1, 17)], cases
protected = ["AGENTS.md", "SECURITY.md", "CONSTRAINTS.md",
             "crates/core/src/lfg_store.rs",
             "crates/core/src/lfg.rs",
             "crates/core/src/router.rs",
             "crates/discord/src/lfg_interactions.rs",
             "crates/discord/src/executor.rs",
             "crates/core/src/event_reminder_store.rs",
             "crates/cutover/src/member_erasure_plan.json"]
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
print(json.dumps({"outcome": "documentation_checked",
    "reason": "offline_paths_and_matrix_only", "next_action": "wait_for_disposition",
    "spec_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
    "spec_sha256": sha(spec), "relative_links_checked": checked,
    "matrix_rows": len(cases),
    "protected_sha256": {p: sha(root / p) for p in protected}}, sort_keys=True))
PY
```

This checks discovery, paths/line ranges and matrix inventory, **not** source
semantics, runtime behavior, vendor pages or approval. Before push run
`git diff --check` and `context-pack --base origin/main`; keep exact-head CI,
independent review and squash auto-merge. No live messages, recipients, DB
writes, production tests/migrations, host automation or new spend are authorized.

Before **any build card**, request CEO/host intent alignment on this exact spec:
retain A, or establish an actual host use case for B and explicitly accept its
channel audience, individual-opt-out limit, default-off policy and caps.
Independently request CISO accept/change/reject disposition on broadcast
consent, current access checks, retained linkable replay facts, erasure/restore
and uncertainty policy. Neither review substitutes for the other; no
self-approval. Unresolved/rejected audience, privacy or anti-noise rules at
the 2026-11-02 review mean **recommend A, no build**, not passive consent. Any
revision invalidates earlier exact-spec acceptance. No public campaign, live
experiment or activation follows from approving docs.

ACT: write one indexed design with no-build selected. WHY: unproven demand and
consent/individual-exclusion limits unresolved. UNDO: reviewed docs revert only;
no operational state changes. Never undo by erasing replay facts or sending
missed reminders.
