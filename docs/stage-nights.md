# Stage-channel game nights — Proposed

Status: **Proposed, design only**, 2026-10-27. Review **2026-11-02**.
**Recommend no build.** No authorized community host has described unmanaged
audio at TWO's scale, and open voice plus existing moderation is the alternative
to prove first. This page authorizes no command, schema, migration, Stage creation
or management, live experiment, activation or new permission. Merge accepts
documentation, not host intent or a security boundary. A negative finding is the
expected outcome of this card; it does not condemn the voice feature.

## 1. Decision and evidence

Hypothesis: if large game nights or AMAs outgrow open voice, a host could run
structured speaker/listener audio (Discord Stage) instead of crosstalk. Nothing
in this repo or in the assignment shows that threshold has been reached.

| Option | Benefit / cost | Recommendation |
| --- | --- | --- |
| A: no build; per-event open voice channel plus existing room moderation | Already designed and partly built; no new Discord authority, state or retention | **Choose now** |
| B: bot-managed Stage instance for one event | Speaker/listener separation for very large sessions; adds a Community-server dependency, new moderator-level authority, hand-raise handling and a second lifecycle | Conditional design only (§3-§5) |
| C: auto-start on event start, auto-promote speakers, auto-moderate stage chat, recording | Silent broad authority and member-visible effects | Excluded |

On 2026-11-02 retain B only if an authorized community host names a concrete,
repeated session where open voice with the moderation in §2 already failed (not
merely "might be nicer"), the guild is a Community server, and a build owner
accepts the boundaries below. Otherwise **kill B and keep A**. Stage is not a
prerequisite of any other roadmap work. No demand experiment, test Stage or
recurring host work is authorized. Design success is an evidenced need and an
accepted contract, not Stage count or event attendance.

## 2. Source trace: zero Stage management, existing alternative

Source inspected at `c02672cc4001bb92c4692d7782becd6e96a6c623`; not deployment
verification.

- **No Stage lifecycle code.** `ChannelType::GuildStageVoice` appears only as a
  channel *kind* for room-sibling name checks, channel inventory and permission
  copy (`crates/bot/src/voice_rooms.rs:4801,5618,10764,11151`,
  `crates/bot/src/voice_room_plan.rs:195`, `crates/core/src/voice_config.rs:178`).
  No handler for stage-instance create/update/delete or request-to-speak exists;
  the parity table lists stage as explicitly unsubscribed (`docs/parity.md:209`).
  A guild Stage channel can carry a name template (`docs/voice-rooms.md:183`);
  that renames it and does not start a session.
- **Rooms exclude Stage by design.** The LFG room is one voice channel "never
  stage" (`docs/lfg-voice-room-spec.md:41,113`).
- **Alternative A exists in part.** Per-event voice claims are keyed
  `UNIQUE (event_id, instance_starts_at)` and claimed before any Discord call
  (`crates/cutover/src/event_voice.rs:1-20`); a 404 channel is marked `orphaned`
  with one retry and occupied rooms are never deleted
  (`crates/bot/src/event_voice_channels.rs:1-10`); creation is fail-closed on a
  required category and Manage Channels/View Channel preflight
  (`crates/bot/src/event_voice_category.rs:1-12`,
  `docs/configuration.md:289`). Join/leave presence spans are wired into the
  voice worker (`crates/bot/src/voice_rooms.rs:10016`). I found **no non-test
  caller** of `claim_channel` in `crates/bot/src`: the creating ticker is a
  separate, not-yet-wired slice (`event_voice_category.rs` header). A is therefore
  designed, not proven live; finishing it is cheaper than any Stage work.
- **Moderation A already has.** Owner/admin room controls
  ([room controls](voice-room-controls-core.md),
  [ownership](voice-ownership-core.md)), private rooms and user limits, and
  [vote-kick](voice-vote-kick-core.md) for ordinary participants. These are
  room-level, not per-speaker turn-taking; that gap is the only thing B adds.
- **Gateway.** Voice states are already received (`GUILD_VOICE_STATES`,
  `crates/discord/src/intents.rs:29-35`). Hand-raise state would arrive on those
  voice-state updates; delivery of stage-instance events through the `GUILDS`
  intent is believed but **must be re-verified** at build time.

External facts (Discord documentation, not independently executable here):
- API reference: creating, modifying or deleting a Stage instance "requires the
  user to be a moderator of the Stage channel", defined as holding
  `MANAGE_CHANNELS`, `MUTE_MEMBERS` and `MOVE_MEMBERS`; topic is 1-120
  characters; privacy level `GUILD_ONLY` (2) is current, `PUBLIC` (1) is
  deprecated; a Stage instance may reference a `guild_scheduled_event_id`.
- Help-center summary (read only through a search summary; the page returned 403
  to this agent, so **unverified**): Stage channels require a Community server;
  audience up to 10,000; no stated cap on audio-only speakers, with one Go Live
  stream and up to five video participants; non-moderators raise a hand or are
  invited, and a moderator approves. A third-party source claims 50 speakers and
  contradicts this. **The exact Community requirement and speaker cap must be
  re-read from Discord's current official page before any build; this page does
  not assert a cap.**

## 3. Conditional contract for B (only if §1 gate passes)

Stage management stays distinct from room ownership. A Stage is never a managed
voice room: it has no owner, creator-channel, name template, vote-kick or
ownership-transfer semantics, and nothing here changes
[voice room ownership](voice-ownership-core.md) or moderation.

- **Authority.** Every start, promote, demote and close re-checks, at action
  time: authenticated application, configured guild, the voice feature gate, an
  explicit stage-night gate (new, default off, same activation/STOP rules as the
  voice features), the actor's current guild membership, the actor's *current*
  moderator permissions on that Stage channel (the three above) and the bot's
  own current permissions. Actor IDs in a request are never authority. Missing,
  stale or unavailable facts deny. The bot never grants itself or anyone
  `MANAGE_CHANNELS`, `MUTE_MEMBERS` or `MOVE_MEMBERS`; absence is
  `PERMISSION_DENIED` with an operator next action, not an attempted grant.
- **Identity.** `stage_session_id` (random, immutable), `event_id` +
  `instance_starts_at` (the existing event-instance key), `guild_id`,
  `channel_id`, Discord `stage_instance_id` (recorded only after observation),
  `host_generation` (advances on every host transfer, including A to B to A) and
  `roster_revision` (advances on every promote/demote). One active session per
  `(guild_id, channel_id)`; one per `(event_id, instance_starts_at)`. Never
  identify a session by topic, host alone, list offset or "latest".
- **Host.** Exactly one recorded host, who must hold moderator permission when
  acting. Transfer is explicit target acceptance with both pinned
  `host_generation`s; permission loss pauses promotion until a new host is
  confirmed; it never silently reassigns to an administrator.
- **Speaker boundary.** Promotion needs: target is a current non-bot guild member
  present in that Stage channel, has *requested* to speak or accepted an invite,
  is not ineligible (suspended, opted out, timed out), and the session is `open`.
  A hand-raise is a request, not consent to be heard by default and not a role.
  At most `MAX_SPEAKERS` (engineering proposal 25, **below and independent of**
  whatever Discord's current cap is; the lower bound wins). Demotion moves a
  speaker back to audience and removes only that effect; it never kicks,
  mutes-server-wide or touches roles. No role restoration, no persistent
  "speaker" role, no priority speaker, no bulk promote.
- **Hand-raise handling.** Observed from voice-state updates. Queue is derived,
  never trusted from a client: ordered by Discord's request timestamp, capped at
  50 pending, cleared on leave, demote, close or restart (rebuilt from the next
  voice-state snapshot; a restart never promotes from a stale queue).
- **Finite lifecycle.** `planned → opening → open → closing → closed`, plus
  terminal `abandoned`. Open requires the scheduled event start window and a
  host; maximum duration 4 h from `open`, no extension without a new explicit
  host action; a missed close is closed by the next reconcile, which ends the
  Stage only when no host action is in flight. Events deleted, rescheduled or
  completed close the session (`closing` first). A deleted Stage channel moves
  the session to `abandoned`; the bot never recreates the channel.
- **Replies are truthful.** Private ephemeral, mention-safe (no user-supplied
  text interpolated unescaped; allowed mentions closed). States: `SAVED`
  (session row committed, no Discord call), `REQUESTED` (Discord call accepted),
  `OBSERVED` (a read after the call showed the state, with timestamp),
  `PENDING` (admitted, outcome unknown), `UNAVAILABLE` (read failed). Never
  "saved" for "done", never "no sessions" for a read failure. Dry-run, preview
  and cancel have zero durable writes and zero Discord calls.

## 4. State and race matrix

Design oracles only, **not executed behavior tests**. Any future tests use
synthetic identities, fixed clocks and recording Discord doubles and disposable
DB only, never staging or production.

| ID | Scenario | Required oracle |
| --- | --- | --- |
| S01 | Two events overlap on one Stage channel | Second `open` refused `CHANNEL_BUSY`; no merge, no takeover; first unaffected |
| S02 | Same event, new occurrence, prior session unclosed | New `instance_starts_at` is a new session; old one must be closed or abandoned first |
| S03 | Host transfer A→B→A with a stale approval from the first A | `host_generation` mismatch → `STALE_REVIEW`; no effect |
| S04 | Host loses moderator permission mid-session | Promotion paused; session stays open; new host required; no silent admin takeover |
| S05 | Bot loses `MUTE_MEMBERS` | Promote/demote return `PERMISSION_DENIED`, distinguishable from a transport failure; no hidden retry |
| S06 | Stage channel deleted while `open` | Session → `abandoned`; no recreate; no further promote; audit has IDs only |
| S07 | Missed close (host gone, bot restarted) | Reconcile closes after max duration or event end; at most one close attempt in flight |
| S08 | Restart after `closed` | Closed session never re-opens or re-promotes; replay returns original result |
| S09 | Restart while `opening` or `closing` | Reconcile reads first; unknown Discord outcome stays `PENDING`, never assumed |
| S10 | Two moderators promote the same member concurrently | One roster CAS wins; loser `CONFLICT` with original result; speaker count unchanged |
| S11 | Concurrent promotions at `MAX_SPEAKERS - 1` | Never exceeds the cap; loser `CAP_REACHED` |
| S12 | Promote target who left or lowered their hand | `NOT_ELIGIBLE`; no effect |
| S13 | No-effect cancel and duplicate cancel | Zero writes/calls; harmless repeat |
| S14 | Discord rate limit, timeout or partial response on promote | `PENDING`/`UNAVAILABLE`, not `OBSERVED`; no automatic redispatch; read-only reconcile |
| S15 | Member opt-out or erasure during a session | Remove from queue; stop new promotion; erase per §5; in-flight Discord effect disclosed |

## 5. Retention, erasure, audit and agent envelope

- **Stored.** A session row (IDs, state, generations, timestamps, topic hash
  only if needed) and per-promotion receipts (IDs, revision, reason code). No
  topic text beyond a short operator label, no chat, audio, transcript, member
  names, avatars or hand-raise history. Session rows: 30 days after `closed`
  or `abandoned`, then deleted; receipts 24 h after settle. Unresolved
  `PENDING` rows have no TTL until reconciled, which is a **liveness STOP**: B is
  not built without a bounded reconciliation protocol for them.
- **PII-safe audit.** Audit records stable IDs, action, reason code and times
  only. No display name, topic text, message content or raw Discord error body in
  logs, metrics labels or audit. Member erasure and guild removal must cover the
  new rows through the existing erasure manifest
  ([privacy](privacy.md)); this is new wiring that needs CISO disposition and is
  not asserted to exist.
- **Agent envelope** (illustrative, not an API). Default `dry_run=true`; mutation
  needs an explicit typed operation, `dry_run=false` and the acting moderator's
  exact confirmation. Missing or contradictory mode is `INVALID_INPUT`, zero
  effects. Unknown fields, duplicate keys and wrong scalar types are refused.

```json
{"version":1,"operation":"promote","dry_run":true,"stage_session_id":"stage-example","host_generation":"3","roster_revision":"7","target_member_id":"123"}
```

```json
{"version":1,"stage_session_id":"stage-example","outcome":"preview","reason":"REVIEW_REQUIRED","retryable":false,"next_action":"moderator_confirm","effects":{"db_writes":0,"discord_calls":0}}
```

| reason | outcome / retryable / next_action |
| --- | --- |
| `REVIEW_REQUIRED` | preview / false / moderator_confirm |
| `SAVED` | saved / false / none |
| `REQUESTED`, `OBSERVED` | requested, observed / false / inspect_stage |
| `PERMISSION_DENIED` | refused / false / operator_fix_permissions |
| `CHANNEL_BUSY`, `CAP_REACHED`, `NOT_ELIGIBLE` | refused / false / none |
| `STALE_REVIEW`, `CONFLICT` | refused / false / new_review, inspect_original_operation respectively |
| `PENDING` | unknown / false / reconcile_read_only |
| `UNAVAILABLE` | unknown_read / true / retry_read_only |
| `INVALID_INPUT` | refused / false / correct_request |

## 6. Offline check and exact-spec gate

Run from the repo root **after committing**; standard library only, no network,
Discord or database. It checks the committed document's link inventory and finite
matrix, not behavior, demand, Discord limits or security acceptance.

```sh
set -e
python3 scripts/check_docs_index.py --json
python3 - <<'PY'
from pathlib import Path
import hashlib, json, re, subprocess, sys
p = Path('docs/stage-nights.md')
try:
    commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    blob = subprocess.check_output(['git', 'show', f'{commit}:{p.as_posix()}'])
    if blob != p.read_bytes():
        raise ValueError('DIRTY_SPEC')
    text = blob.decode('utf-8')
    cases = re.findall(r'^\| (S\d{2}) \|', text, re.M)
    if cases != [f'S{i:02}' for i in range(1, 16)]:
        raise ValueError('MATRIX_INCOMPLETE')
    root = Path.cwd().resolve()
    links = re.findall(r'\]\(([^)\s]+)\)', text)
    local = [x for x in links if '://' not in x and not x.startswith('#')]
    for link in local:
        target = (p.parent / link.split('#')[0]).resolve()
        if not target.is_relative_to(root) or not target.is_file():
            raise ValueError('MISSING_PATH')
    print(json.dumps({'version': 1, 'outcome': 'documentation_checked',
        'reason': 'LINKS_AND_MATRIX_ONLY', 'next_action': 'exact_spec_disposition',
        'spec_commit': commit, 'spec_sha256': hashlib.sha256(blob).hexdigest(),
        'matrix_rows': len(cases), 'relative_links_checked': len(local),
        'runtime_verified': False}, sort_keys=True))
except (OSError, ValueError, subprocess.CalledProcessError):
    print(json.dumps({'version': 1, 'outcome': 'failed',
        'reason': 'DOC_CONTRACT_FAILED', 'next_action': 'fix_paths_or_matrix'}))
    sys.exit(1)
PY
```

The recommendation is **no build**, so no exact-spec CEO or host-intent routing
is requested and none is implied. Route the exact spec commit and SHA-256 to the
CEO for host-intent alignment, and separately obtain the CISO disposition for §5,
**only if** the 2026-11-02 review finds a named host and a concrete unmet
use case. No security acceptance is recorded here, and a spec merge is not
permission. Missing evidence, an unresolved reconciliation protocol or rejected
boundaries on 2026-11-02 means **kill B**, not passive approval. No
implementation or test tasks, Stage, new role, schema, production test or live
activation follow this PR. Finishing alternative A's event-voice ticker is
separate, already-scoped work and does not depend on this page.
