# Events: host-written play expectations — Decision

Status: **Decision, design only: no build.** Written 2026-10-27; review
**2026-11-03**. No product or runtime command is added (section 8 adds only an
offline documentation check); no storage, migration, RSVP field, permission,
send or runtime test is added, and no outreach, experiment or activation is
authorized.
Merge accepts documentation, not host intent or a privacy boundary. This file is
the single repository contract; review requests bind its commit and SHA-256.

## 1. Decision and evidence

Decision `EPX-1`: hosts explain what a game night expects (experience level,
voice use, pace, where to ask) in the **existing event description**, using the
optional template in section 4. Add nothing to runtime or schema.

Hypothesis: members decide whether to join more easily when the host says up
front what the night expects. No TWO demand is validated; this card is a
proposal, not a request from a host.

[sesh's manual](https://sesh.fyi/manual/) (read 2026-10-27) documents a
free-text event description ("Limited to 4096 characters"), Custom RSVP Options,
and edit/delete limited to event
managers and the creator. That is vendor capability evidence of
organizer-written text, not of TWO demand, and it shows no accessibility feature.

| Option | Value and cost | Disposition |
| --- | --- | --- |
| A: host convention in the existing description (section 4) | Zero code, schema, storage or new disclosure; reuses the host's current edit path | **Choose now** |
| B: structured fields (for example three enumerated expectations) | Consistent rendering and filtering; needs fields, a validator and a mirror/schema change for what four short lines already carry | Not justified; conditional slice in section 7 only |
| C: member needs, preferences, accommodations or health collected by RSVP, form or inference | Personal and sensitive data, retention and erasure duties, profiling risk | **Excluded** |

## 2. Verified baseline

Inspected repository head: `1a87869f8`. Facts only; none is permission to widen.

- `crates/core/src/scheduled_events.rs:31,33-41,93-112,152-172`: the mirror keeps one
  optional `description`, counted in Unicode scalars (`chars().count()`) with a
  **1,000 cap**. One violation refuses the **whole snapshot**, never a clipped
  field, and the poll cadence is 10 minutes.
- `crates/core/src/internal_actions.rs:82-85,1261-1275`: `event.upsert` accepts an
  optional `description` of at most 1,000 **UTF-16 units** (`description_too_long`).
  The field must be non-empty when present, so the action cannot send an empty
  string. `crates/discord/src/internal_events.rs:126-138` omits an absent
  description from the PATCH body instead of sending null. Clearing a
  description is therefore not expressible through this action; the host edits it
  in Discord. Whether Discord treats a present-and-null field as a clear was not
  tested here.
- `crates/discord/src/internal_events.rs:134`: events are created with
  `privacy_level: 2`, guild-only. `crates/cutover/migrations/0300_website_contract.sql:120-135`
  mirrors `description` as plain nullable text beside a status; only scheduled
  and active rows reach the contract views.
- `crates/core/src/rsvp.rs:36-67`: RSVP stays Going / Interested / Declined with no
  free text. Authority to write an event is the existing `event.upsert` `moderate`
  admin-API scope (`internal_actions.rs:2056-2061`) and the host's own Discord edit rights. This decision adds none.
- Related specs own the neighbouring seams and are not changed here:
  [calendar handoff](event-calendar-handoff-spec.md) already **excludes** the
  description because it can carry links, mentions or private detail;
  [native publish](event-native-publish-spec.md) shows a description whole in a
  400-unit preview; [agenda](event-agenda-spec.md) and
  [seat limits](rsvp-capacity-spec.md) are separate held proposals.
- [Reminders](event-reminders.md) use the event name, not the description.

No live guild, Discord edit behavior, permission or deployment was inspected.

## 3. Boundary

- **Organizer-authored, public only where the event is already visible.** The text
  is the host's words about the event. Do not copy it to a place visible to more
  members than the event itself, and do not derive anything about a member from
  it.
- **No member needs.** No per-member diagnosis, disability or medical data,
  private accommodation, questionnaire, profiling, RSVP answer or inferred
  preference. A member who needs something asks the host through a channel the
  host already named. The bot stores nothing about the ask.
- **No guarantee.** A label ("all levels", "voice optional") describes the host's
  intent for that night. It does not promise accessibility, suitability for any
  condition, safety or acceptance. Do not write "accessible", "safe for" or
  "suitable for" followed by a condition.
- **Inert text.** Current paths store and forward the description verbatim and do
  not parse it. Any future bot message that quotes it must apply the closed
  outbound mention policy (as [reminders](event-reminders.md) do) and treat links,
  Markdown and control characters as data.
- **Truthful states.** No description means "not provided", never "no
  expectations" or "beginner friendly". A mirror value is as old as its last poll
  (up to 10 minutes plus a failed poll), so a mirrored description is
  "as of last sync", and an unread mirror is **unknown**, not empty.
- **Unchanged:** agendas, event mirrors, roles, publication, reminders and RSVP.

## 4. Optional host template

Four short lines, plain text, no mentions or links. About 190 UTF-16 units, so it
leaves most of the 1,000 for the rest of the description.

```text
Experience: all levels welcome | some experience helpful
Voice: optional | required | not used
Pace: relaxed | steady | fast; breaks about every hour
Questions: ask the host before the event
```

Pick one value per line and delete the others; omit any line the host does not
want to state. It is a convention. The bot does not parse, require or validate it.

## 5. Agent handoff: illustrative JSON, not an available API

One repo source: this document. A future agent may only classify text it was
given; it never reads a member's data and never changes an event. Illustrative:

```json
{"version":1,"decision_id":"EPX-1","event_id":"100000000000000001","outcome":"shown_as_written","reason":"HOST_TEXT","retryable":false,"next_action":"none","effects":{"writes":0}}
```

| reason | outcome / retryable / next_action |
| --- | --- |
| `HOST_TEXT` | shown_as_written / false / none |
| `NOT_PROVIDED` | not_provided / false / ask_host |
| `MIRROR_UNREAD`, `MIRROR_STALE` | unknown / true / open_event_in_discord |
| `DESCRIPTION_TOO_LONG` | refused / false / shorten_description |
| `EVENT_NOT_ACTIVE` | no_effect / false / none |

Unknown is `unknown`, never `not_provided`. A reply carries no text from another
event, member or private channel.

## 6. Finite example and test matrix

Design oracles only, **not executed behavior tests**. Event IDs are synthetic.

| ID | Scenario | Required oracle |
| --- | --- | --- |
| X01 | Description absent | `NOT_PROVIDED`; no inferred level, voice rule or pace |
| X02 | Section 4 template filled in | `HOST_TEXT`; shown whole; not parsed or required |
| X03 | Exactly 1,000 and 1,001 UTF-16 units | 1,000 accepted; 1,001 `description_too_long` |
| X04 | 600 astral characters (1,200 UTF-16 units, 600 scalars) | Action refuses; the mirror counts scalars and would accept; hosts count UTF-16 |
| X05 | One mirrored description over 1,000 scalars | Existing behavior: whole snapshot refused, last good rows stay; keep text well under the cap |
| X06 | `@everyone`, `<@id>`, links, Markdown, zero-width and right-to-left controls | Inert data; no parse, no ping, no link preview from the bot |
| X07 | Description edited after the mirror poll | Value is "as of last sync"; stale is `MIRROR_STALE`, never "current" |
| X08 | Event cancelled, completed or deleted | `EVENT_NOT_ACTIVE`; no text surfaced from a dead event |
| X09 | Event in a private channel | Text stays where the event is visible; never copied wider |
| X10 | Host writes "accessible" or "suitable for" a condition | Out of convention; no label is a guarantee; host guidance only |
| X11 | Member asks for an accommodation | Pointed to the host's named channel; no RSVP field, no stored record |
| X12 | Host needs to clear the text | `event.upsert` cannot (non-empty or omitted); host edits in Discord |
| X13 | Recurring or copied event | Each event carries its own text; a stale copy is the host's to fix |
| X14 | Non-host tries to edit the text | Only existing event authority; nothing new is granted |
| X15 | Mirror read fails or times out | `MIRROR_UNREAD`, `unknown`, retryable; never `not_provided` |

## 7. Review by 2026-11-03 and the one conditional slice

On 2026-11-03 retain B only if an authorized community host names a specific
repeated problem that 1,000 characters of host-written text cannot solve (for
example a need to filter or sort events by an expectation). Otherwise **record
no-build and keep A**. This kills the structured-field hypothesis, not events.
No build or behavior-test card follows this PR, no host is contacted, and no
experiment is run.

If B is ever justified, describe exactly one slice of at most one day: in the
[native publish](event-native-publish-spec.md) preview, offer the section 4
lines as a host-typed insert that is still stored in the existing `description`,
with no new column, RSVP field or member input. Before any build, route the exact
spec commit and SHA-256 to the CEO for host-intent alignment, and obtain an
independent privacy-boundary disposition from the CISO. No security acceptance
is recorded here, and a revision invalidates a prior exact-spec acceptance.

## 8. Offline documentation check

Run from the repo root **after committing**. Python standard library only; no
network, Discord or database. It checks the committed file's relative links, the
finite matrix and that the section 4 template fits the description cap and
contains no mention, bare or Markdown link, or control character. It does not
check behavior, demand
or privacy acceptance.

```sh
python3 scripts/check_docs_index.py --json
python3 - <<'PY'
from pathlib import Path
import hashlib, json, re, subprocess, sys
p = Path('docs/event-play-spec.md')
try:
    commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    blob = subprocess.check_output(['git', 'show', f'{commit}:{p.as_posix()}'])
    if blob != p.read_bytes():
        raise ValueError('DIRTY_SPEC')
    text = blob.decode('utf-8')
    cases = re.findall(r'^\| (X\d{2}) \|', text, re.M)
    if cases != [f'X{i:02}' for i in range(1, 16)]:
        raise ValueError('MATRIX_INCOMPLETE')
    root = Path.cwd().resolve()
    links = re.findall(r'\]\(([^)\s]+)\)', text)
    local = [x for x in links if '://' not in x and not x.startswith('#')]
    for link in local:
        target = (p.parent / link.split('#')[0]).resolve()
        if not target.is_relative_to(root) or not target.is_file():
            raise ValueError('MISSING_PATH')
    block = re.search(r'```text\n(.*?)```', text, re.S).group(1)
    units = len(block.encode('utf-16-le')) // 2
    if units > 1000 or re.search(r'[@<>]|https?:|\[[^\]\n]*\]\([^)\n]*\)|[\x00-\x08\x0b-\x1f\u200b-\u200f\u202a-\u202e]', block):
        raise ValueError('TEMPLATE_INVALID')
    print(json.dumps({'version': 1, 'outcome': 'documentation_checked',
        'reason': 'LINKS_MATRIX_TEMPLATE_ONLY', 'next_action': 'exact_spec_disposition',
        'spec_commit': commit, 'spec_sha256': hashlib.sha256(blob).hexdigest(),
        'matrix_rows': len(cases), 'relative_links_checked': len(local),
        'template_utf16_units': units, 'runtime_verified': False}, sort_keys=True))
except (OSError, ValueError, AttributeError, subprocess.CalledProcessError):
    print(json.dumps({'version': 1, 'outcome': 'failed',
        'reason': 'DOC_CONTRACT_FAILED', 'next_action': 'fix_paths_matrix_or_template'}))
    sys.exit(1)
PY
```
