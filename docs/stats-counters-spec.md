# Server-stats counter channels — Proposed

Status: **Proposed, design only**, 2026-10-27. Review **2026-11-05**.
**Recommend no build now.** The website already serves the member count, and no
authorized host request for it in the channel list is on record. This page authorizes no
command, schema, migration, live experiment, activation or new permission.
Merge accepts documentation, not host intent or a security boundary. Numbers
below are engineering proposals, not current settings.

## 1. Decision and evidence

Hypothesis: a host may want live community size visible in the channel list,
which the website cannot do. Bots such as ServerStats advertise the pattern; that
is vendor capability, not evidence of TWO demand.

| Option | Benefit / cost | Recommendation |
| --- | --- | --- |
| A: website only | `web_v1.live_counts` already serves the bot-excluded human count every 60 s ([interval](../crates/core/src/community_snapshots.rs#L48), [writer](../crates/core/src/website_store.rs#L133)); no new Discord writes | **Choose now** |
| B: one counter, `stats.human_members.v1` | Ambient count in Discord; one renamed channel, one state row | Conditional design only |
| C: B plus `stats.voice_humans.v1` | Adds humans now in voice; a snapshot that moves faster than the rename budget, so it is the weakest counter | Conditional, last |
| D: online count, per-role, message activity, custom templates, per-member stats | Needs a privileged presence intent, new storage or a template surface | Excluded |

On 2026-11-05 retain B only if an authorized host names an in-Discord audience
the website cannot reach and accepts the staleness in §2. Otherwise **defer and
keep A**. This kills the hypothesis, not the community project. Create no
implementation or test cards automatically. If a build is later chosen, B ships
alone first; C needs its own evidence.

## 2. Rename budget, cadence and staleness

Discord limits renames of one channel to **2 per 10 minutes**. This is not in
Discord's documented rate-limit tables and is not reported in response headers;
the source is [discord-api-docs issue 1900](https://github.com/discord/discord-api-docs/issues/1900)
(2020) and Discord's own [community post](https://support.discord.com/hc/en-us/community/posts/20757990318999-Increase-renaming-channel-API)
(2024). This repo already builds on it ([constant](../crates/core/src/voice_rooms.rs#L43-L45),
[voice note](voice-rooms.md#discord-api-notes-all-slices)). It was not re-measured
here: no live experiment is authorized, so a future build must treat the figure
as unverified and fail closed on any 429.

Design budget (checked by the offline check in §7):

```json
{"discord_renames_per_window":2,"window_s":600,"min_rename_spacing_s":660,"admin_headroom":1,"evaluate_every_s":60,"stale_after_s":1800}
```

- Evaluate each counter every 60 s from the same bot-excluded reading the website
  uses ([`build_counter_reading`](../crates/core/src/community_snapshots.rs#L492)).
  Rename only when the formatted value changed and at least **660 s** have passed
  since the last rename attempt. That is at most one rename in any 10-minute
  window and keeps one slot for an admin's manual rename. Normal lag is at most
  about 12 minutes.
- Fixed formats, no templates: `Members: 1,234` and `In voice: 12`. Plain ASCII
  digits with `,` grouping. Names are always under 100 characters.
- **Staleness label.** If the reading is older than 1800 s (source read failed,
  roster truncated as in [website_jobs](../crates/bot/src/website_jobs.rs#L1147), rename blocked), the
  next permitted rename appends a space and `(stale)` to `last_value`, and it
  has priority over a value update. A stale rename reuses `last_value` only to
  mark it stale, never as a fresh number. The label clears on the next fresh
  applied value. For fresh value updates the bot never writes a value it did not
  read this cycle.
- **Residual, disclosed.** The label itself needs a rename. If a 429, permission
  loss or deletion blocks that too, the channel keeps an old number unlabeled.
  State `stale_unlabeled` (a counter state, not an outcome) then raises a staff alert (log and metric by counter ID,
  never by guild-visible text). Repeated `stale_unlabeled` is evidence for the
  defer rule, not something to hide.
- 429: obey `Retry-After` (persisted as `retry_after_until`, §4), extend the
  spacing, no burst retry. Three consecutive
  failures move the counter to `blocked` and alert staff; recovery is a manual
  `resume` command.
- Counters use the bot-excluded human count (raid-window accounts excluded, as the
  website does). The voice counter must filter bots from live voice state. The
  existing [`occupants`](../crates/bot/src/voice_rooms.rs#L1895) helper already
  counts humans only, but a build must re-verify that against its own inputs. It
  also skips counter channels themselves. No bot traffic is ever counted. I could not find a doc
  named the "AI-presence rule"; this is the rule applied as the card states it.

## 3. Setup, ownership and teardown

Default off; a guild has no counters until an admin enables them. Proposed
subcommands (not registered): `setup`, `status`, `resume`, `teardown`.

- **Admin only**: configured guild plus Manage Server, the same gate as the
  [feeds planner](feed-move-spec.md). Caller-supplied IDs are never authority.
- **Configured-guild fence** is checked at every setup, every cycle and before
  every PATCH. If the guild is no longer the configured one, the counter becomes
  `fenced_off`: no renames, no reads, no deletion of the channel or row. Unfencing
  and re-fencing are not replays; re-fencing resumes only after `resume`.
- **Bind an existing channel, do not create one.** The admin picks an empty voice
  channel. Refuse a voice-room creator or managed room, a channel already bound,
  or one where the bot lacks View and Manage Channels (it already holds both for
  voice rooms; **no new permission or intent**). `status` warns if `@everyone` can
  Connect. One counter per channel, at most two channels per guild.
- **Identity.** Stable `counter_id` (`stats.human_members.v1`,
  `stats.voice_humans.v1`) and `(guild_id, channel_id, channel_generation)`.
  Generation advances on every re-bind. Never find the channel by name.
- **Manual rename** (read-back or channel-update shows a name other than the last
  one the bot applied): state `drifted`. The bot does **not** rename back, since
  that would spend the budget and fight an admin. It pauses until `resume` or
  `teardown`. **Precedence:** when an admin rename both uses a budget slot and
  shows up as drift, `drifted` wins over `DEFERRED_BUDGET`/`RATE_LIMITED`; the
  slot is still counted, so after `resume` the bot waits the full spacing.
- **Manual delete**: state `orphaned`. No auto-recreate; an admin runs `setup`
  again, which creates a new generation.
- **Teardown** stops writes, deletes the state row, and leaves the channel and its
  last name in place; the admin deletes or renames it. Replay of `teardown` or a
  repeated `setup` is idempotent.

## 4. Storage, restart and erasure

One new row per counter (no schema is added here): `guild_id`, `counter_id`,
`channel_id`, `channel_generation`, `state`, `state_version`, `last_value`,
`last_applied_name`, `last_attempt_at`, `last_success_at`, `retry_after_until`.
**No member ID, name,
role or raw voice list is stored**; the voice count is an in-memory integer.

- **Restart/replay.** The desired name is recomputed from the source, never queued.
  `last_attempt_at` is written before the PATCH, so a crash mid-refresh counts the
  rename as spent: state `uncertain`, wait the full spacing, then read the channel
  back. A matching name settles to `active` and reports `APPLIED` (an outcome,
  not a state); no automatic second PATCH within spacing. A 429 stores its
  `Retry-After` deadline in `retry_after_until`, and the refresh flow sends no
  PATCH before it, even after a restart; `last_attempt_at` still enforces the
  fixed spacing. A restart cannot create an extra rename.
- **Retention/erasure.** Row deleted at teardown and by the existing guild-purge
  queue on true guild removal ([privacy](privacy.md#retention-and-explicit-exceptions));
  a rejoin cancels the purge. Member erasure has nothing to erase beyond what the
  counts already aggregate; the count is not a per-member record.
- **CISO disposition: not recorded or waived here.** Needed before any build, and
  expected to be light: no PII, no new permission or intent, one admin command,
  one bot-initiated rename of an admin-bound channel. The CISO should confirm that
  reading. No new spend or provider.

## 5. Agent handoff: illustrative JSON, not an available API

One repo source: this document. No live flags. Future agents may read `status`;
they cannot run `setup` or `teardown` for an admin.

```json
{"version":1,"counter_id":"stats.human_members.v1","channel_id":"789","channel_generation":"2","outcome":"stale","reason":"STALE_UNLABELED","retryable":false,"next_action":"staff_resume_or_teardown","effects":{"renames":0,"db_writes":0}}
```

Envelopes are at most 8 KiB, depth 4; reject duplicate or unknown fields. Unknown
effects are `null`, not zero.

| reason | outcome / retryable / next_action |
| --- | --- |
| `APPLIED`, `NO_CHANGE` | applied, no_effect / false / none |
| `DEFERRED_BUDGET` | deferred / true / wait_spacing |
| `RATE_LIMITED` | deferred / true / honor_retry_after |
| `STALE_LABELED`, `STALE_UNLABELED` | stale / false / none, staff_resume_or_teardown |
| `DRIFTED`, `ORPHANED` | paused / false / resume, setup |
| `PERMISSION_LOST`, `BLOCKED` | paused / false / restore_permission, resume |
| `FENCED_OFF`, `NOT_ADMIN` | refused / false / none |
| `UNCERTAIN` | unknown / true / read_back |
| `INVALID_INPUT` | refused / false / correct_request |

## 6. Finite state/test matrix

States: `off`, `active`, `stale_unlabeled`, `drifted`, `orphaned`, `blocked`,
`uncertain`, `fenced_off`. Outcomes such as `APPLIED` are not states.
Design oracles only, **not executed tests**. Future tests use synthetic IDs, fixed
clocks and recording Discord doubles, never production or staging.

| ID | Scenario | Required oracle |
| --- | --- | --- |
| S01 | Default guild, no setup | `off`; zero reads, renames and rows |
| S02 | Non-admin or foreign guild runs setup | `NOT_ADMIN`/`FENCED_OFF`; zero effects |
| S03 | Setup on a managed room, bound channel or without Manage Channels | `INVALID_INPUT`; no row, no rename |
| S04 | Count changes every minute for an hour | At most one rename per 660 s, so at most one in any 10-minute window |
| S05 | Rename budget exhausted by a 429 or the bot's own spacing | `RATE_LIMITED`/`DEFERRED_BUDGET`; honor `retry_after_until`, also after restart; no burst |
| S06 | Source read fails or roster truncated past 1800 s | `(stale)` suffix at next slot on `last_value`, never a guessed number |
| S07 | Rename blocked while stale | `STALE_UNLABELED`; staff alert; no hidden retry loop |
| S08 | Admin renames the channel (also consumes a slot) | `drifted` wins over budget deferral; no rename-back until `resume`; full spacing after `resume` |
| S09 | Admin deletes the channel | `orphaned`; no auto-recreate; new generation on re-setup |
| S10 | Guild unfenced, then re-fenced | `fenced_off`; zero PATCHes; resume only by `resume` |
| S11 | Crash between row write and PATCH, or after PATCH | `uncertain`; full spacing; read back; no double rename |
| S12 | Bot joins the guild or a voice room | Neither count changes |
| S13 | Bot loses View/Manage Channels | `PERMISSION_LOST`; no retries beyond backoff; staff alert |
| S14 | Teardown then replay; guild removal then rejoin | Row gone, idempotent; purge cancelled on rejoin; channel untouched |

## 7. Offline documentation check and exact-spec gate

Run from repo root **after committing**; Python standard library, no network,
Discord or DB. It checks this document's local links, matrix rows and budget
arithmetic, not behavior, demand or security acceptance.

```sh
python3 scripts/check_docs_index.py --json
python3 - <<'PY'
from pathlib import Path
import hashlib, json, re, subprocess, sys
p = Path('docs/stats-counters-spec.md')
try:
    commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    blob = subprocess.check_output(['git', 'show', f'{commit}:{p.as_posix()}'])
    if blob != p.read_bytes():
        raise ValueError('DIRTY_SPEC')
    text = blob.decode('utf-8')
    cases = re.findall(r'^\| (S\d{2}) \|', text, re.M)
    if cases != [f'S{i:02}' for i in range(1, 15)]:
        raise ValueError('MATRIX_INCOMPLETE')
    b = json.loads(re.search(r'^\{"discord_renames_per_window".*\}$', text, re.M).group(0))
    ours = -(-b['window_s'] // b['min_rename_spacing_s'])
    if ours > b['discord_renames_per_window'] - b['admin_headroom']:
        raise ValueError('BUDGET_EXCEEDED')
    root = Path.cwd().resolve()
    local = [x for x in re.findall(r'\]\(([^)\s]+)\)', text) if '://' not in x and not x.startswith('#')]
    for link in local:
        target = (p.parent / link.split('#')[0]).resolve()
        if not target.is_relative_to(root) or not target.is_file():
            raise ValueError('MISSING_PATH')
    print(json.dumps({'version': 1, 'outcome': 'documentation_checked',
        'reason': 'LINKS_MATRIX_BUDGET_ONLY', 'next_action': 'exact_spec_disposition',
        'spec_commit': commit, 'spec_sha256': hashlib.sha256(blob).hexdigest(),
        'matrix_rows': len(cases), 'relative_links_checked': len(local),
        'runtime_verified': False}, sort_keys=True))
except (OSError, ValueError, AttributeError, KeyError, subprocess.CalledProcessError):
    print(json.dumps({'version': 1, 'outcome': 'failed',
        'reason': 'DOC_CONTRACT_FAILED', 'next_action': 'fix_paths_matrix_or_budget'}))
    sys.exit(1)
PY
```

Before **any build or test card**, route the exact spec commit and SHA-256 to the
CEO for host-intent alignment: keep A, or name the authorized in-Discord audience
and accept B's staleness, drift and `stale_unlabeled` residual. Obtain the CISO
disposition in §4 separately; none is recorded here. A revision invalidates prior
acceptance. Missing evidence on 2026-11-05 means **defer**, not passive approval.
No implementation tasks, schema execution, production tests or activation follow
this PR.
