# Voice creator configuration copy — Proposed

Status: **Proposed, design only**. Written 2026-10-26; review **2026-11-02**.
No copy command, endpoint, migration, Discord write, live configuration read or
activation is delivered or authorized. Merge accepts documentation for review,
not the feature or host intent. **Keep manual configuration for now.**

## Decision and current alternatives

Hypothesis: an administrator setting up a second lobby could avoid transcribing
saved defaults without replacing a whole guild configuration. Demand and time
saved have not been measured; roadmap reserve is not implementation admission.

| Option | Scope / tradeoff | Decision |
| --- | --- | --- |
| Manual per-creator commands | Existing controls; more steps, but no new copy state or concurrency protocol | Default now; reassess after settings-read lands |
| Guild export/import | Broad configuration transfer with existing private review; not a one-creator copy primitive | Keep its scope; never edit an export to simulate this operation |
| Explicit source-to-existing-target copy | Same guild, stable creator channel IDs, three scalar defaults only; new complete review and transactional freshness needed | Conditional proposal below, not a build authorization |
| Clone/create target, copy permissions or cross-guild import | New identity, authority and lifecycle boundaries | Excluded |

Source inspection baseline: `5db6ecccd7c67202ecea6d76f9ae65ceab2e3654`
(includes the original codec baseline `92d85042527b756062bfc9e19c8d77aed0cee0b3`).
See [creator defaults](voice-creator-defaults-acceptance.md),
[placement](voice-placement-core.md), [config review](voice-config-diff-core.md)
and [apply](voice-config-apply.md). These are source contracts, not evidence of
current deployment or permission to copy real configuration.

The [runtime model](../crates/core/src/voice_rooms.rs#L91-L173) is
`CreatorChannel`; [add_creator](../crates/cutover/src/voice_rooms.rs#L118-L164)
is a full-row upsert, **not** a compare-and-swap or copy authorization.
[Export DTO](../crates/core/src/voice_config.rs#L58-L86) is different: it renames
fields, collapses inherited limit to zero on export and omits companion overrides.
Do not use it as a lossless snapshot for this proposal; preserve `null` versus 0.
Manual `/position`, `/group`, `/defaultlimit` and other configuration controls
remain separate. [PR #1838](https://github.com/TogetherWeOwn/two-bot-next/pull/1838)
proposes private, write-free channel-only `/position`, `/group`, `/textchannels`
reads; inspected open at head `07777b33e8a1ab65b8e6e0cf3f6ed1d5001bc2cb`.
Do not advertise that pending contract as shipped or change its tests/scope here.
No existing upsert, import confirmation or read summary supplies this proposal's
source-and-target concurrency guarantee automatically.

## Exact v1 boundary

**Design recommendation, not a host decision:** replace all three allowlisted
fields on one existing target with the source's saved values. No field selector,
merge inference, names-as-identity, creation, deletion or default reset. Both
IDs must resolve to configured creator voice channels in the same admitted guild.
A channel existing on Discord alone is not a configured creator.

| Saved field | Proposal | Meaning |
| --- | --- | --- |
| `position` | Copy | `RoomPosition::Above` or `Below`, relative to the target creator |
| `group_by_category` | Copy | Boolean; future shared numbering/contiguous placement within target category |
| `default_limit` | Copy | `null` inherits target creator's live limit; integer 0 is unlimited; integers 1..99 set capacity |
| All other fields, including future model fields | Preserve target; never copy by default | Closed allowlist, no struct-wide clone/upsert |

Preserve both creator identities (`guild_id`, `channel_id`), target category/Discord
channel attributes, source configuration and every other creator. The complete
saved-row exclusion inventory at this baseline is `name_template`,
`status_template` (persisted outside `CreatorChannel`), `permission_source`,
`permission_channel_id`, `private_default`, `text_channels`, `text_channel_name`,
`text_viewer_role_id` and `first_room_number`, plus the two identity fields.
Unknown added columns/schema versions fail closed until this inventory is revised.
A versioned schema allowlist classifies secret-bearing fields before selecting
values: never read a new column speculatively to discover whether it is a secret.
These saved-row fields, and only these plus the three copied fields, form the
complete freshness snapshot; external grants/runtime/Discord attributes do not.

**Exclude `first_room_number`:** [live room-name facts](../crates/bot/src/voice_name_panel.rs#L1195-L1206)
read it from the current creator, so copying it cannot honestly promise future
rooms only without expanding the implementation. `first_number` and
`always_private` are export DTO names, not the canonical runtime field names.
Exclude nickname/game aliases, bitrate, logging destinations and guild settings;
permission overwrites, role/member grants, visibility/access policies, viewer roles,
deny/allow lists and guild activation are not in the copy boundary.
Even a source reference that is currently empty is not permission to clear the
target. No permission-source copying without a separate independent CISO boundary
disposition and a new accepted spec; existing `add_creator` behavior grants none.

Never copy runtime rooms, owners, occupants, room numbers already assigned,
member preferences, pending joins/votes, sessions, counters, cooldowns, recovery
fences, import plans, revision/incarnation IDs, audit history, credentials, tokens
or secrets. This is not a snapshot/restore. Copy writes saved defaults only;
existing target rooms and admitted/in-flight room-creation snapshots retain their
old values. A later fresh room admission can use the new defaults. No room rename,
move, permission edit, disconnect, channel creation or refresh fan-out follows.
Saved is not observed/applied; a receipt must say **saved for future rooms**.

Validate the three values under the current canonical placement/default-limit
validators used by the corresponding manual writes, even if old stored values
were once valid; no coercion, clamping or truncation. Pin those exact validators
when the final read contract is admitted. Unknown fields/schema, incomplete reads,
malformed stored values or unknown channel ownership/type/visibility are not
empty/default values: refuse, do not repair or infer. After current authorization,
source=target returns `SAME_CREATOR`, no preview token or writes. An equal allowlist
is `NO_CHANGE` after a fresh complete read, not evidence the creators are identical;
a stale confirmation is refused before considering a no-op.

## Private preview, cancel and confirm

Proposed surface: guild-only administrator action with explicit `source_creator_id`
and `target_creator_id`, both decimal Discord channel-ID strings, never labels.
It must preserve application/configured-guild, feature, activation and halt fences.
Require a current human guild administrator **and** current visibility/access to
both channels. This is deliberately narrower than manual settings' Manage Channels
gate and import's Manage Guild gate. Derive guild Administrator from the complete
ready member/role facts, not channel-overwrite-derived interaction permissions
([existing authority path](../crates/bot/src/voice_rooms.rs#L8418-L8493)).
Reauthorize preview, every page, confirm, cancel and receipt reads;
agent-supplied actor IDs and component contents are not authority. Bot access must
be sufficient for authoritative facts; this action requests no new permissions.
Missing, foreign and unauthorized resources share `NOT_AVAILABLE`, without
existence, names, values, revision or count disclosure. Unknown authority fails
closed; no cached permission fallback.

Preview reads one consistent source/target configuration snapshot plus bounded
channel facts. It performs **zero durable writes**, including audit, idempotency
reservation or recovery state, and no Discord mutation or background work.
Cancel/expiry likewise perform zero configuration/durable writes. Use a signed,
opaque, short-lived preview token bound to actor/application/guild, both creator
IDs and incarnations/revisions, schema/allowlist version, exact proposed values,
review digest and issuance/expiry; never put raw settings in component IDs or logs.
Keep its complete review snapshot and page/cancel state in bounded process memory
(<=2 active previews/actor, <=20/guild; refuse excess, expire after five minutes).
The token is a signed handle/digest, not plaintext configuration in a component ID.
Restart or another worker without that record requires a fresh preview. Token
issuance must fit existing signing authority; provisioning a new credential or
persistent preview store is not silently included in this design.

Private human review must include all three rows, even unchanged ones: field,
source saved value, target before, target after, and changed/unchanged status.
Also list **every non-allowlisted saved creator field** with source and target
values and `preserve target / excluded`, using stable IDs for reference fields.
A grant collection outside the creator model is never fetched or expanded; label
it `not read; excluded and unchanged`, not empty. Never include secret values.
If schema discovery identifies a secret-bearing creator field, refuse preview
with `UNSUPPORTED_SCHEMA` rather than rendering it. Runtime state is not read and
is explicitly excluded. No partial/truncated diff may be confirmed.

Use ephemeral pages <=2,000 UTF-16 units each, <=20 pages and <=32 KiB serialized
UTF-8 review data, with whole fields, page N/count and a single snapshot/digest.
Escape Markdown, close allowed mentions, and neutralize display mentions/control
and bidi characters; show escaped Unicode scalars where needed, preserving exact
underlying values in the validated snapshot. No clickable channel/role/user
mentions. Never split or silently shorten a field; oversized review is
`REVIEW_TOO_LARGE`, with no confirm token. All pages are bound to the same digest;
confirm is available only after the same actor has traversed every page. Such
page-traversal bookkeeping may be bounded in-memory only, expires with the token,
and restart loses it: re-preview, never silently mark pages reviewed.

Expiry is five minutes from snapshot issuance, not extended by paging. Confirm
requires that token plus explicit confirmation of its exact digest; there is no
`force`, name-based fallback or implicit confirm on retry. Revalidate source and
target channel/guild/type/access facts; unknown/unavailable facts refuse. Discord
facts cannot be transactionally locked: the guarantee is saved-state CAS, **not**
atomicity with Discord. Disclose that limitation; no Discord writes are made.

## Freshness, atomicity and replay

**Build prerequisite:** authoritative, monotonic creator revisions and unique
incarnations must cover **every** writer (manual commands, import, delete/recreate,
maintenance and copy). Compare the complete saved source and target, including
excluded fields, not just the three copied values. Content hashes alone miss ABA;
process-local caches and name comparisons are not freshness witnesses. Restores
must invalidate preview tokens using a new generation; old tokens cannot survive
rollback/restart with uncertain revision history. If implementing this across
existing writers exceeds one bounded future slice, defer the copy feature.

Confirm ordering: reauthorize and gather Discord facts **before** acquiring DB
locks; then run one bounded DB transaction. Fence the same guild configuration
as its other writers, lock both rows in stable ID order, recheck locally available
admission/halt state, incarnations/revisions and complete snapshots, then update
**only** the three target fields with a conditional expected-version write. Source,
excluded target fields and unrelated rows stay byte-for-byte unchanged. Persist
the target revision increment and minimal success receipt atomically; that receipt
has unique constraints on both preview identity and actor-scoped request ID and
is the durable consumption record (not a preview reservation). Missing row is
refusal, never INSERT/UPSERT. Read/write failure rolls back everything. No DB lock
spans Discord/network I/O. Observed loss of actor admin/channel access or bot
visibility before commit refuses. No external atomicity or post-commit permission
compensation is promised: a later Discord revocation cannot undo a saved commit.

| Condition | Required result / effects |
| --- | --- |
| Preview, cancel, expired token | No durable/configuration writes, no Discord mutations |
| Source/target edited, removed/recreated, import or restore since preview | `STALE_PREVIEW`; zero writes; invalidate view and re-preview both |
| Two confirms against target revision 7 | One commits revision 8; other is stale, never overwrites revision 8 |
| Source=target / same three values | `SAME_CREATOR` / `NO_CHANGE`; zero writes and no revision bump |
| Same confirmed preview + same request ID + same fingerprint | Reauthorize, return original committed receipt; no second write |
| Same request ID + changed IDs/digest/input while its receipt is retained | `IDEMPOTENCY_CONFLICT`; zero writes |
| Different request IDs for the same preview | At most one success: consumed preview identity and target CAS arbitrate |
| Commit result lost / connection failure | `OUTCOME_UNKNOWN`; inspect original receipt first, never report failure or blindly retry as a new request |

Request IDs are 1..64 ASCII letters/digits/`_`/`-`, scoped to guild and actor;
preview identity is server-generated and single-use. Fingerprint binds operation,
actor, guild, IDs, schema version, snapshot digest and exact copied values. Receipt
contains only IDs, revisions, fingerprint, preview identity, token expiry, commit
time and fixed result codes; no copied text, grants or secrets. Only successful
changing commits create receipts; denials/no-ops never reserve keys or capacity.
Bound retention to 24 hours after commit and <=100 unexpired receipts/guild (no
eviction to make space); `CAPACITY_REACHED` refuses before mutation. A valid token
expires five minutes after issuance; expired tokens never execute, even without
a retained receipt. Matching replay during retention is a **read-only receipt
lookup**, not a new confirm: recheck all current admission/authority/access fences
first, and return `NOT_AVAILABLE` without receipt data if denied. Otherwise return
the original receipt, explicitly `replayed=true`, with zero new writes, even after
token expiry or process restart. After retention, an authenticated original token
returns `EXPIRED`: verify its signed issuance/expiry against server time even when
no receipt exists. A request ID alone cannot look up or execute an operation; no
permanent tombstone is needed to reject that expired token. Invalid signatures or
revoked signing generations fail closed, never execute or disclose a receipt.
Request-ID conflict/replay guarantees last only while the receipt is retained.
After deletion, that actor may reuse the ID **only with a fresh preview**, complete
review and new explicit confirmation, subject to current authority and CAS. That
is a new operation, not recovery/replay of the erased one; the old token remains
expired regardless of the reused ID. Clients recovering an unknown outcome must
not silently substitute a new preview for the original confirmation.
Cancel discards the token/UI and marks any local record cancelled; an already
submitted authorized confirm may have won, so inspect its receipt instead of
claiming rollback. Missing local preview state cannot execute after restart.
Receipt retention/erasure and generation/signing strategy need independent
security disposition before build. Erasure must not accidentally revive a still
valid token: invalidate its scope/generation before deleting replay evidence.

## Agent contract and offline check

This file is the single proposal source. **Illustrative JSON only; no endpoint
or CLI exists.** Preview accepts no writable field overrides; exact source/target
saved values are fetched through the authorized read contract. Unknown/duplicate
keys, bad versions/types, nondecimal/zero/out-of-range IDs, envelope >4 KiB UTF-8
or nesting >4 refuse `INVALID_INPUT`. All failures are bounded constant codes,
never raw DB/Discord errors. Agent preview cannot impersonate human intent.

```json
{"version":1,"operation":"preview","guild_id":"456","source_creator_id":"111","target_creator_id":"222","dry_run":true}
```

```json
{"version":1,"result":"preview","reason":"REVIEW_REQUIRED","next_action":"review_all_pages","source_creator_id":"111","target_creator_id":"222","source_revision":"4","target_revision":"7","changed_fields":["position","default_limit"],"retryable":false,"effects":{"configuration_writes":0,"discord_mutations":0}}
```

Future apply requires `operation=confirm`, `dry_run=false`, signed preview token,
exact digest, request ID and independently authenticated administrator confirmation.
A missing/wrong mode combination never writes. Authorized result envelopes carry
version, result, reason, next_action, retryable, stable IDs and known revisions;
denials omit resource IDs/revisions. A success is `saved / SAVED_FOR_FUTURE_ROOMS /
none`, never `applied_to_rooms`. Error recovery:

| reason | retryable / next_action |
| --- | --- |
| `SAME_CREATOR`, `NO_CHANGE` | false / `none` |
| `NOT_AVAILABLE` | false / `check_authorized_creator_selection` |
| `INVALID_INPUT`, `UNSUPPORTED_SCHEMA` | false / `correct_request`, `use_manual_commands` respectively |
| `STALE_PREVIEW`, `EXPIRED` | false / `preview_both_again` |
| `REVIEW_TOO_LARGE` | false / `use_manual_commands` |
| `IDEMPOTENCY_CONFLICT`, `OUTCOME_UNKNOWN` | false / `inspect_original_receipt` |
| `UNAVAILABLE` | true / `retry_preview_after_recovery`; known no commit only |
| `CAPACITY_REACHED` | false / `wait_for_receipt_expiry_then_preview` |

Offline documentation check from repository root (Python standard library; no
network, DB, tokens or product execution). This checks discovery, links, field
inventory, example shape and fixture inventory **only**, not future behavior:

```sh
python3 scripts/check_docs_index.py --json
python3 - <<'PY'
from pathlib import Path
import hashlib, json, re, sys
p = Path('docs/voice-creator-copy-spec.md')
try:
    text = p.read_text()
    copied = re.findall(r'^\| `([^`]+)` \| Copy \|', text, re.M)
    assert copied == ['position', 'group_by_category', 'default_limit']
    cases = re.findall(r'^\| (F[0-9]{2}) \|', text, re.M)
    assert cases == [f'F{i:02}' for i in range(1, 13)]
    for link in re.findall(r'\]\(([^)]+)\)', text):
        if '://' not in link and not link.startswith('#'):
            assert (p.parent / link.split('#')[0]).is_file()
    examples = [json.loads(s) for s in re.findall(r'```json\n(.*?)\n```', text, re.S)]
    assert len(examples) == 2
    assert examples[0]['operation'] == 'preview' and examples[0]['dry_run'] is True
    assert examples[0]['source_creator_id'] != examples[0]['target_creator_id']
    assert examples[1]['effects'] == {'configuration_writes': 0, 'discord_mutations': 0}
    assert examples[1]['result'] == 'preview'
    print(json.dumps({'version': 1, 'result': 'documentation_checked',
        'reason': 'INVENTORY_ONLY', 'next_action': 'review_exact_spec',
        'spec_sha256': hashlib.sha256(p.read_bytes()).hexdigest(),
        'copy_fields': copied, 'fixtures': len(cases), 'runtime_verified': False}))
except (OSError, ValueError, AssertionError, KeyError):
    print(json.dumps({'version': 1, 'result': 'failed', 'reason': 'DOC_CONTRACT_FAILED',
        'next_action': 'fix_documentation'}))
    sys.exit(1)
PY
```

## Future acceptance fixtures, not tests executed here

Use synthetic IDs G=456, source=111, target=222, third creator=333. Source r4:
`position=Above, group_by_category=true, default_limit=4`; target r7:
`position=Below, group_by_category=true, default_limit=0`. Seed deliberately
different excluded fields, permissions and runtime/pending records for each.
Every fixture asserts private/mention-safe response, exact result/reason/next_action,
configuration write count and zero Discord mutations; no live data.

| ID | Fixture / required oracle |
| --- | --- |
| F01 | Copy baseline: all three values match source; only position/default_limit changed; source/third/excluded values and runtime records unchanged; target revision increments once. Separate null-to-0 and 0-to-null cases preserve inherit versus unlimited; null inherits target's live limit, never source's |
| F02 | Source=111,target=111 -> SAME_CREATOR, zero writes; equal allowlist on distinct IDs -> NO_CHANGE, excluded differences preserved |
| F03 | Missing creator, unknown channel, foreign guild, no membership/admin/access or unknown facts -> NOT_AVAILABLE without existence/values/revisions; no upsert |
| F04 | Concurrent manual edit of target excluded field while confirming -> STALE_PREVIEW, preserve edit; two confirmations serialize to exactly one success |
| F05 | Source edit after preview, including change-then-revert ABA -> STALE_PREVIEW; source values never refetched and silently substituted into old review |
| F06 | Target edit/ABA, delete-recreate with same ID, import or restore -> stale/generation refusal; no blind update or reuse of revision |
| F07 | Existing target room occupied and another creation admitted before commit: all existing/in-flight settings, owners, permissions and occupants unchanged; fresh later admission uses copied defaults |
| F08 | Preview/cancel/expiry/restart halfway through pages -> zero durable writes; no confirm without complete current view; lost commit response -> inspect receipt |
| F09 | Same-key replay (even after token expiry), retained-key/input conflict, alternate key same preview, receipt expiry/erasure and capacity -> at most one commit per preview. Commit at t0; after t0+24h delete its receipt, resend the signed original token+ID -> EXPIRED, zero writes. Reuse that ID with a fresh fully reviewed/confirmed preview -> new operation subject to CAS; then resend the old token again -> EXPIRED, never the new receipt or a second commit |
| F10 | Exact/one-over 4 KiB input, 32 KiB review, 20 pages and 2,000 UTF-16 units/page; a whole unrenderable field refuses; no truncated confirm |
| F11 | Excluded template contains @everyone, Markdown, emoji, combining characters and bidi controls: escaped complete private diff, no ping; raw values preserved, byte/scalar/UTF-16 bounds distinguished |
| F12 | Unknown schema/field, malformed stored scalar, duplicate JSON key, missing ID, invalid mode, DB/read failure, revoked access at confirm: bounded refusal; no defaulting, partial update or raw error leak |

Before any future build, the Test Author must supply immutable tests/manifest,
behavior-specific baseline RED and unchanged positive/negative controls. This
matrix and offline check are **not** those tests and do not prove runtime safety.

## Admission, review date and kill rule

Before **any implementation or test card**, record exact-spec commit + SHA-256
CEO/host intent: a concrete second-lobby need that final manual settings reads do
not adequately solve, plus acceptance of this narrow subset, complete private
review and future-rooms-only limitation. Final settings-read PR contract must be
merged and re-inspected; copy neither changes that owner's scope nor depends on
an unreleased summary as an authoritative read API.

Independently settle genuine CISO boundaries for new receipt retention/erasure,
signing/generation and saved-state authority. Permission-source copying remains
excluded regardless; expanding it requires separate disposition and a new design.
No decision or consent is recorded by this document. Do not introduce a routine
CEO/owner approval hop into already-approved deployment contracts.

Review **2026-11-02**: defer if manual commands meet the need, demand remains
unproven, any boundary is unresolved, or complete diff plus all-writer CAS cannot
fit one bounded future slice. Silence is not acceptance. Measure usefulness by
administrator setup steps/time and transcription errors avoided, not bot activity;
no live measurement is authorized here. Future implementation must use canonical
command descriptors and generated references, preserve all security/activation
fences, pass local checks, same-head CI and independent review, then auto-merge.
This slice changes docs only; no build/test/deploy cards are created.
