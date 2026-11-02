# LFG session changes: edited post first, notices only by explicit intent

Status: **Declined: no build** (owner decision 2026-11-01). This doc stays as
the record; reopen only with a named host problem. Original proposal: written
2026-10-25. **Choose no build now.** This proposal adds no commands, sends,
subscriptions, settings, migrations or runtime behavior. Merge records a design,
not acceptance, host consent, security disposition or activation. This is the
sole editable repo spec; decision records must bind its exact commit and
SHA-256.

## 1. Decision, evidence and alternatives

Problem hypothesis: players may miss a changed start time or a canceled session
when only the original post changes. No TWO demand, missed-session frequency or
benefit measurement is established. No live audience, campaign or experiment was
used. Keep the existing edited post until explicit host intent and independent
security/consent disposition justify anything more.

Sources read 2026-10-25: [Apollo editing](https://docs.apollo.fyi/modifying_events)
documents start-time/recurrence edits; [Apollo reminders](https://docs.apollo.fyi/reminders)
documents attendee preferences, default opt-in and thread/channel reminders;
[Sesh's manual](https://sesh.fyi/manual/) documents event editing and opt-in DM
reminders under `/create`. These support workflow comparisons, **not** a claim
that either product implements this exact change notice or that TWO needs one.

| Option | Incremental benefit / uncertainty | Decision |
| --- | --- | --- |
| A. No build: retain the edited LFG post | No additional noise, member data or send state; readers can still miss edits | **Selected now**; lowest cost with unproven demand |
| B. Explicit private preview, then staff publication of one channel notice | Makes a material change visible in channel history without a roster; still not guaranteed to reach players and may annoy unrelated readers | Conditional reopening contract below, **not selected for build** |
| C. Opt-in change subscriptions | Could target people who explicitly want updates; needs new personal choices, delivery/erasure policy and fanout | Not selected; signup/RSVP/reminder consent is not consent to this |

Scope exactly **one LFG post**, identified by `(guild_id, lfg_id)`. Do not bridge
to website events, RSVP rows, recurring schedules, series, or inferred attendance.
B uses only the post's existing text channel, never a new/existing thread: the
[thread proposal](lfg-thread-spec.md) remains separately owned and design-only.
Quiet hours, agenda, recap, roles, calendar sync, new permissions/spend, production
activation and implementation are out of scope.

## 2. Source map: observed baseline, not future capabilities

Inspected source: `0b8f8c6427859a606c4be710224dc68649fabe00`. Source inspection
only; no deployment, message, live member list or database was inspected.

| Seam | Evidence and limit |
| --- | --- |
| LFG identity/storage | [Schema](../crates/cutover/migrations/0170_lfg.sql#L10-L39): ID, guild, channel, nullable message, start, open/closed, creator and signups. No semantic revision, cancellation cause, notice preference or website-event ID |
| Edit authority | [Router](../crates/core/src/router.rs#L723-L737): announcements gate and Manage Events for create/edit/close. Creator identity alone is **not** edit authority |
| Start edit | [Store](../crates/core/src/lfg_store.rs#L149-L203): guild/post lock, open-only distinct metadata update; no role/signup rewrite. [Service](../crates/discord/src/lfg_interactions.rs#L416-L471) validates future time; unchanged returns without refresh |
| Visible update | [Service](../crates/discord/src/lfg_interactions.rs#L569-L599): audit and refresh can fail after saved edit; reply discloses that. [Refresh](../crates/discord/src/lfg_interactions.rs#L717-L739) edits the bound message, not a notice; [transport](../crates/discord/src/executor.rs#L2928-L2954) suppresses mentions |
| Close is not cancellation proof | [Manual close](../crates/discord/src/lfg_interactions.rs#L478-L490) and [lifecycle close](../crates/discord/src/lfg_interactions.rs#L680-L714) share closed state. Expiry/orphan closure must never be described as organizer cancellation |
| Audit is not a revision | [Audit](../crates/discord/src/lfg_interactions.rs#L744-L774) has actor/action/outcome and time-based identity, no before/after values; it is separate from the metadata commit |
| Website family, excluded | [Signed upsert/cancel](../crates/bot/src/internal_action_http.rs#L2402-L2499) maps event keys in the configured staging guild; request identity conflict is not an event revision. This path does not establish a human organizer's authority |
| Reminder isolation | [Reminder store](../crates/core/src/event_reminder_store.rs#L38-L123) reads scheduled-event RSVP rows and keeps a permanent guild/event attempt fence, **not** a start-time key. It is not an LFG notice subscription or reusable per-change outbox |
| Existing erasure | [Manifest](../crates/cutover/src/member_erasure_plan.json#L19-L31) removes creator-owned LFG posts and signup identities; new retained links/identifiers need their own disposition, not an assumed exception |

The existing [reminder-preference proposal](event-reminder-preferences-spec.md)
selects no new preference. Do not revive its withdrawn quiet-hours/Off design.
Some older LFG design text predates `/lfg-edit`; the source map above, not its
"only close" wording, describes this head.

## 3. Conditional B: eligible change and revision binding

Everything in sections 3–7 is a **future acceptance contract**, not a new API or
available command. A later accepted build must first supply atomic semantic
revision/provenance persistence; reconstructing old values from audit timestamps,
message history or the scheduled-event mirror is forbidden.

- Assign a monotonically increasing revision per guild/post on meaningful stored
  metadata/status mutations; preserve old/current UTC instants, destination and
  cause. No-op edits do not advance it. Signup changes do not become notices.
  Keep only the undecided baseline/latest snapshots plus <=3 attempt decisions
  per post, not an unbounded history of every edit; IDs are never reused.
- Qualifying reschedule: open session, new start still future at admission, old
  and new Unix **seconds differ**. Offset-only formatting and subsecond changes
  are not material. Title-only/role/signup edits do not qualify.
- Qualifying cancellation: an explicit human cancellation declaration committed
  with `open -> closed`, cause `organizer_cancelled`. Existing `/lfg-close` lacks
  that proof; historical closed rows, expiry, orphan cleanup and post deletion
  are `UNSUPPORTED_CHANGE`, never cancellation. No reopen or series cancellation.
- Bind preview to `(guild_id, lfg_id, from_revision, to_revision, channel_id,
  source_message_id, content_hash, policy_revision, actor_id, expires_at)`.
  Revisions/IDs are strings in handoff JSON, never a mutable title/start key.
  Initial `from_revision` is the retained creation baseline; later it is the
  last notice-decided revision, including failed/uncertain attempts.
- Coalesce all unsent edits into one old-to-latest comparison. Require 60 seconds
  with no eligible edit before preview; there is **no timer-driven publication**.
  A -> B -> A yields no meaningful change; A -> B -> C previews A -> C, not two
  notices. A newer metadata/destination revision invalidates an older preview.
- Private preview expires after 5 minutes. Same actor must explicitly confirm
  the exact preview. A bot, service key, saved component or signup cannot supply
  human intent. Confirm is separately authenticated and reauthorized.

Authority: preserve configured guild, feature, application, activation and human
actor fences; require **current membership plus channel-resolved Manage Events**
and access to the source/destination at preview and confirmation. Do not infer
permission from `created_by` or an agent-supplied actor ID. Missing/foreign/denied
posts share one non-disclosing response. Any unavailable authority check refuses.

## 4. Audience, preferences and honest privacy limits

**B is a channel broadcast, not recipient fanout.** One message, exactly the
post's saved guild text channel, with a confirmed accessible source message.
No channel override, fallback, thread creation/adoption, DM, webhook, member-add,
role/everyone/here/user mention, roster, attendee count, host name or free text.
The only content is change type, old/new time and a fixed-origin Discord jump
link constructed from validated bound IDs. No title, location or private reason.

- A host must explicitly opt this channel into this purpose under a recorded,
  default-off policy revision. Each publication additionally needs the human
  confirmation above. Unknown/disabled policy, or missing approved intent, means
  **no send**. Existing announcements enablement is not this purpose's consent.
- At confirmation/admission, verify destination still belongs to this guild,
  binding is unchanged, source message exists, requester can view/read it and bot
  can view/read/send there using existing permissions. Only guild text channels
  qualify; changed/unknown permissions, deleted messages or unsupported channels
  refuse. Discord enforces access for readers; no cached RSVP list authorizes it.
- **No personal preference is read or created.** RSVP/Going, LFG signup, reminder
  settings and analytics opt-out do not authorize this notice. No recipient
  membership/visibility census or subscriber list is collected. Departed members
  are not contacted or named; a departed requester cannot confirm.
- Consequently B **cannot honor a person's desire not to see a shared channel
  message**, even if they leave the signup or opt out of reminders/analytics.
  Channel/client notification settings remain Discord-controlled; no ping does
  not mean silent. The preview must say this. If individual exclusion or proof
  of current authorization for each recipient is required, reject B and keep A;
  do not quietly add subscriptions, identity markers or a roster filter.
- `NO_AUDIENCE` means no approved destination audience (channel policy off),
  **not** a measured zero viewers. Zero signups does not prove zero readers;
  do not disclose counts or claim everyone was notified. Unknown visibility is
  `UNAVAILABLE`, never evidence of no audience or permission to fall back.

Closed mentions on every output: `allowed_mentions` has empty `parse`, `users`,
`roles` and `replied_user=false`; no reply reference or link embed. Suppress embeds
and request notification suppression, but do not promise client silence.
One fixed-format message <=1,000 UTF-16 units; invalid/oversized content refuses,
never truncates a date, splits a burst or substitutes an attachment.

## 5. Noise, durable decisions and erasure

Proposed hard bounds for B: <=2 reschedule attempts and <=1 cancellation attempt
per post lifetime, >=5 minutes between any attempts for a post, <=4 attempts per
guild per rolling hour, <=2 per actor per rolling 10 minutes. Unknown/refused
post-admission attempts consume quota. No delayed catch-up, retry queue or automatic
send after restart. At most 10,000 retained notice decisions per guild; at capacity
stop rather than recycle replay guards. Caps are part of requested host intent,
not current runtime constants; cancellation does not bypass them.

Admission must atomically compare current revision/binding/policy, check caps,
advance the post's notice-decided baseline and commit one unique attempt keyed by
`(guild_id, lfg_id, from_revision, to_revision, purpose=lfg_change_notice)`.
Concurrent different previews share the same post decision lock; overlapping
ranges cannot both win. Same idempotency key with different binding/content is
`CONFLICT`. Identical retry returns the saved outcome, with **no new POST**.

Only the process that knows it newly committed admission may make one wire
attempt. Unknown DB commit result authorizes no send. After committed admission,
state is `attempted -> sent | failed | uncertain`; persist `sent` only with a
validated Discord message ID. Lost response, crash before/after wire, timeout,
429/5xx, unavailable receipt store or restart leaves a terminal decided range:
**never re-arm or automatically resend**, even if history finds nothing. Disable
transport retries for this effect. `sent` means Discord accepted, not read.
A later genuinely different revision can be separately previewed under remaining
caps; changing title/time formatting or using a new request ID cannot replay a
decided change. A post edit/access revocation after admission cannot recall an
already in-flight message; disclose this race, do not hold DB locks across HTTP.

Keep this state separate from RSVP reminders. Never delete/rekey/reset
`event:reminder:<guild>:<event>` after a start edit, cancellation, erasure or mirror
refresh; an LFG notice must not create or replay a scheduled-event reminder.

A adds no retained data. B would add personal/linkable state and needs independent
CISO disposition: no copied roster/body/title/location or personal preferences;
short-lived preview contains no stored actor token; proposed actor-bearing quota
records expire after 1 hour and are removed on erasure. Retain only the minimum
actor-free post/revision attempt facts for replay protection, including across
post erasure and restore. **Actor-free is not a claim of anonymity**: LFG IDs can
encode interaction IDs and guild/post/channel/message links can be identifying.
No indefinite retention exception, hash-based identity substitute or deletion
exemption is accepted here. If minimal retained facts cannot satisfy erasure and
no-replay together, keep A. Deleted posts never recreate; all delayed requests
must reauthorize a present post. No Discord message deletion/edit is implied by
local erasure. Backup/restore must preserve decided attempts before enabling any
sender; uncertainty after restore fails closed, never infers delivery absence.

## 6. Private UX and agent handoff

Proposed preview is ephemeral to the authorized requester, no write/audit/enqueue,
quota consumption or channel effect. It shows event ID, revisions, destination,
exact fixed-format body, remaining caps and expiry. Warn: "One channel message,
visible to channel readers, including people not signed up. No pings or DMs;
Discord may still notify readers. Delivery is best effort, not proof of reading."

Synthetic body examples (render Discord-local full dates, not ambiguous clock-only
or relative-only values; UTC instants included here to make fixtures reproducible):

- Reschedule: `Session rescheduled. Previously: <t:1793440800:F>. Now:
  <t:1793444400:F>. See original post: <validated Discord jump link>.`
  Instants: 2026-10-31T10:00:00+00:00 -> 2026-10-31T11:00:00+00:00.
- Cancellation: `Session canceled. Previously scheduled: <t:1793440800:F>.
  New start: none; this session is canceled. See original post: <validated
  Discord jump link>.` Never display a fake new timestamp or equate expiry to this.

All outcomes reply privately. No endpoint/command is added by this document.
The future adapter contract below is illustrative JSON; its IDs are synthetic:

```json
{"version":1,"family":"lfg","event_id":"lfg-123","guild_id":"456","from_revision":"7","to_revision":"9","dry_run":true,"outcome":"preview","reason":"start_changed","next_action":"confirm_exact_preview","sent":false}
```

| Outcome / reason | Private response and next_action |
| --- | --- |
| `preview / start_changed` or `organizer_cancelled` | Exact body, limits and warning; `confirm_exact_preview` |
| `noop / no_meaningful_change` | "No start-time change or explicit cancellation to announce"; `none` |
| `refused / NO_AUDIENCE` | "No channel is approved for these notices"; `keep_edited_post` |
| `refused / UNAVAILABLE` | "Cannot verify the post, access or notice policy; nothing was sent" before admission; `retry_preview_when_available` |
| `refused / NOT_AVAILABLE` | Missing, foreign or unauthorized, with no title/roster leak; `none` |
| `refused / UNSUPPORTED_CHANGE` | Close has no cancellation proof, historical revision or series unsupported; `keep_edited_post` |
| `refused / STALE` | "Post or policy changed, or preview expired; preview again"; `preview_latest` |
| `refused / CONFLICT` | "This request differs from its recorded intent"; `inspect_decision` |
| `refused / RATE_LIMITED` | No delayed job; show next possible preview time, never promise delivery; `keep_edited_post` |
| `sent / accepted` | "Discord accepted the channel notice; reading is not known"; `none` |
| `failed / attempted_not_delivered` | Known refusal after admission; decided, no retry; `inspect_decision_no_resend` |
| `uncertain / delivery_unknown` | "A notice may have been sent; do not resend"; `inspect_decision_no_resend` |
| `replay / decided` | Return original decision and receipt if known, including uncertainty; `none` |

Every outcome includes stable IDs/revisions when disclosure is authorized and
`outcome`, `reason`, `next_action`, `sent` (true/false/null for uncertain). Redact
IDs in denied responses. Handoff also carries exact spec commit/hash and protected
file hashes from the check below; a changed hash requires source reinspection,
not automatic approval. Agents may preview but cannot confirm as the human.

## 7. Deterministic offline acceptance matrix

These are **planned assertions**, not executed runtime tests. Future fixtures use
fake UTC clocks, synthetic guilds/actors/channels, deterministic revision stores,
controllable permission/policy reads and a recording one-shot Discord transport.
Store races/restarts require independent connections to disposable test DBs only.
Each case asserts JSON reason/next_action plus exact SQL/HTTP effect counts.

| Case | Required result |
| --- | --- |
| M01 old/new instant, offset/DST boundary | Distinct seconds render two full local timestamps; same instant with another offset is noop; no personal timezone collection |
| M02 cancellation vs expiry | Explicit stored organizer_cancelled qualifies; sweep, orphan, old close and missing cause refuse with zero POST |
| M03 repeated edits / A -> B -> C -> A | Coalesce baseline/latest, require 60 seconds stable, invalidate older previews; net no change means no notice |
| M04 schedule / series / website ID | Reject other families and series; never traverse RSVP or scheduled-message rows |
| M05 same revision / key retry | One admission and <=1 wire attempt; changed content with same key conflicts; new key cannot replay decided range |
| M06 DB/Discord unknown outcome | Unknown commit -> zero POST; lost wire/receipt -> uncertain, zero retry, quota consumed |
| M07 no eligible audience / empty signup | Policy off -> NO_AUDIENCE, zero POST; zero signup does not mean zero readers or create a recipient list |
| M08 opt-outs / preferences | No inferred consent from signup/reminders/analytics; explicit host policy and confirmation required; personal broadcast exclusion unavailable is disclosed |
| M09 departed member / requester | No targeting/naming former signups; requester departure or revoked Manage Events before admission refuses |
| M10 invisible/deleted/moved destination | Refuse missing read/send permission, foreign channel, source deletion, changed binding; no DM/channel/thread fallback |
| M11 cross-guild / malformed / denied | Same non-disclosing NOT_AVAILABLE; forged actor, bad ID, bot actor and unauthorized component cannot publish |
| M12 restart / concurrent confirmations | Retained decision fences prevent repeat POST; overlapping previews race to one decision; no automatic startup send |
| M13 rate/cap/render/ping bounds | Test exact limits and one-over; no split, mention, embed, title or arbitrary content; all terminal attempts count |
| M14 edit after admission / erasure / restore | Disclose in-flight race; deleted post/delayed input cannot recreate; restore preserves decisions or sender stays disabled |
| M15 private dry-run / preview expiry | No persistence/queue/send/quota; expires after 5 minutes; policy/revision change requires new preview |
| M16 reminder isolation | Move/cancel/restart does not alter existing permanent event-reminder fence or create a second reminder |

## 8. One source/check recipe and decision gate

From the repo root, offline (Python standard library only):

```sh
python3 scripts/check_docs_index.py --json
python3 - <<'PY'
from pathlib import Path
import hashlib, json, re, subprocess
root = Path.cwd().resolve()
spec = root / "docs/event-change-notices-spec.md"
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
             "crates/discord/src/lfg_interactions.rs",
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
independent review and squash auto-merge. No live messages, recipients, DB writes,
production tests/migrations, host automation or new spend are authorized.

Before **any build card**, request CEO/host intent alignment on this exact spec:
retain A, or establish an actual host use case for B and explicitly accept its
channel audience, individual-opt-out limit and caps. Independently request CISO
accept/change/reject disposition on broadcast consent, current access checks,
retained linkable replay facts, erasure/restore and uncertainty policy. Neither
review substitutes for the other; no self-approval. Unresolved/rejected audience,
privacy or anti-noise rules at the 2026-10-31 review mean **recommend A, no build**,
not passive consent. Any revision invalidates earlier exact-spec acceptance.
No public campaign, live experiment or activation follows from approving docs.
