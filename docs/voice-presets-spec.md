# Member-owned voice presets — Proposed

Status: **Proposed, design only**, 2026-10-26. Review **2026-11-02**.
**Recommend no build now.** No authorized community-host use case beyond current
explicit room/creator settings has been established. This page authorizes no
command, schema, production migration, live experiment, activation or role
restoration. Merge accepts documentation, not host intent or a security boundary.
Numeric limits below are engineering proposals, not current settings.

## 1. Decision and evidence

Hypothesis: a returning owner could explicitly reuse a room name and user limit
without repeating two manual changes. [Wrenchy](https://wrenchy.org/features)
advertises saved voice presets (assignment evidence, not independently verified
here); vendor capability is not evidence of TWO demand.

| Option | Benefit / cost | Recommendation |
| --- | --- | --- |
| A: current explicit room controls and creator defaults | No member catalog, retained room text or new recovery surface; two manual settings may suffice | **Choose now** |
| B: self-owned, guild-local explicit save/review/apply | Reuses two settings across sessions; adds personal storage, ownership fencing and uncertain-effect handling | Conditional design only |
| C: auto-apply on join, copied access/roles/privacy/bitrate, other owners' rooms | Broader authority and silent future effects | Excluded |

On 2026-11-02 retain B only if at least one authorized community host describes a
concrete repeated-session use case that A cannot adequately meet, and safe
ownership/erasure fits the boundaries below. Otherwise **defer B and retain A**.
This kills the hypothesis, not the voice feature or community project. Do not
create implementation or acceptance-test cards automatically. No live demand
experiment or recurring host work is authorized. Design success is an evidenced
need and an accepted safe contract, not preset count or bot activity.

## 2. Existing seams, not claimed guarantees

Source inspected at `5db6ecccd7c67202ecea6d76f9ae65ceab2e3654`; not deployment
verification. Start with [room controls](voice-room-controls-core.md),
[name handling](voice-name-panel.md), [ownership](voice-ownership-core.md),
[creator defaults](voice-creator-defaults-acceptance.md) and
[privacy](privacy.md). Guild/creator configuration is not a member preset catalog:
[configuration model](../crates/core/src/voice_config.rs) stores shared creator
settings. The conditional contract below is **new**, including any additional
revision/fence needed across existing mutation paths. A pure ownership helper
or existing row lock does not prove end-to-end transfer/apply safety.

Current runtime routes `/name`, `/limit [count]` and `/unlimit`
(`crates/bot/src/voice_rooms.rs:14410-14442`). Manual controls permit owner/admin;
B deliberately requires self-ownership and current room presence instead. Current
component ownership checks are not an occupancy guarantee. Creator defaults
apply during room planning (`crates/bot/src/voice_room_plan.rs:299-312`): absent
limit inherits the creator channel; explicit 0 is unlimited. B stores an observed
integer, never that inheritance sentinel or the guild export's ambiguous 0.
The existing owner-grant journal (`crates/cutover/src/voice_rooms.rs:278-430`)
fences permission grants, not general name/limit operations or preset revisions.

Known build prerequisites, not a catalog-only estimate:
- Limit writes currently use `RoomAction::SetUserLimit`
  (`crates/bot/src/voice_rooms_limit.rs:267`); names use stored overrides and the
  rename coalescer (`crates/bot/src/voice_name_panel.rs:1122-1150,1319-1404`).
  There is no claimed combined two-field dispatch seam. B requires a reviewed
  combined effect preserving canonical policy/pacing; otherwise revise this spec.
- Fence coverage must enumerate slash/panel name+limit, automatic template
  refresh, transfer/claim, empty-room/guild deletion and member erasure paths,
  including queued work and restart recovery. P04 must prove no bypass with
  recording doubles and cross-worker races before B can be enabled.
- Current [opt-out](privacy.md#member-opt-out-per-user-per-guild) deliberately
  excludes operational voice-room rows. B proposes a **new optional personal
  preference** class, distinct from those rows. Its catalog suppression/purge is
  new wiring requiring CISO disposition, not existing opt-out behavior.

## 3. Small, private member flow

Proposed controls (not registered commands): **Save**, **List**, **Review apply**,
**Confirm**, **Cancel**, **Delete**. Guild-only, ephemeral replies; no DMs, public
catalog, sharing, importing, automatic join hook, role restoration or dashboard.
Save captures only the current observed literal room name and numeric user limit;
no template evaluation, privacy flag, access overwrite, role, member list, bitrate,
creator ID or source-room identity survives in the saved preset. Apply changes
only those two fields of the explicitly reviewed current room. It does not alter
creator defaults, room ownership, access, visibility or occupancy. A user limit
is not an access-control guarantee and does not evict occupants.

- Every save/apply review and confirmation checks authenticated application,
  configured guild, voice feature/activation/STOP gates, current non-bot guild
  membership, privacy eligibility, current presence in that managed voice room,
  requester view/connect access and **current recorded ownership by that member**.
  Current bot view/manage-channel capability must also be known. Deny on missing,
  stale or unavailable facts; an administrator cannot act for a different owner
  through this member surface. Caller-provided member/owner IDs are never authority.
- Scope every lookup by `(guild_id, authenticated_member_id)`. Opaque IDs are
  locators, not capabilities. Foreign/missing/deleted/unauthorized resources share
  `NOT_AVAILABLE`, without disclosing their names, existence, versions or counts.
  List/delete only require current self identity/membership and privacy gates,
  not ownership of a still-existing room; erasure/opt-out remains available after
  loss of room ownership or feature disable through the existing privacy path.
- At most **3 presets/member/guild**, **300/guild**, **10 rows/page**; no staff
  bypass or cross-guild reuse. Atomic quotas cover concurrent saves. Labels are
  1..32 Unicode scalars, <=128 UTF-8 bytes, trimmed and exact-case unique within
  that member/guild; labels are display-only, never selectors or IDs. Reject
  C0/C1 controls, mention syntax (`@`, `<`, `>`) and empty/whitespace-only labels.
- Name: 1..100 Unicode scalars, <=400 UTF-8 bytes, no controls, no whitespace-only
  value or mention syntax; never silently truncate. Save and apply must pass the
  current canonical voice-name policy as well, including literal-name classification
  (reject all recognized template syntax) and guild uniqueness, excluding the
  target room itself on apply. Save validates source observation; apply rechecks
  uniqueness against its destination. A policy rewrite/filter would require a
  changed-name preview and new confirmation, not silent substitution.
  Limit: JSON integer **0..99**, where 0 means unlimited, never missing/unknown;
  reject bool, float, string, negative, overflow and absent fields. Stricter
  current Discord/guild constraints win; unknown constraints refuse.
- Save review states exactly what personal data persists, scope, expiry and how
  to delete/opt out. Confirm is explicit storage consent, not permission to apply
  now or on a future join. No implicit consent from changing a room manually.
- Saving a new label creates a new random preset ID, revision 1. A duplicate
  label never overwrites silently: show old/new name and limit privately, preset
  ID and expected revision; explicit **Overwrite** increments revision via CAS.
  Delete/recreate allocates a new ID, even for the same label. Expired rows cannot
  be resurrected or renewed by replay.
- Apply review shows stable room/preset IDs, versions, current observed name/limit,
  requested name/limit, effective validation, and “Only name and user limit will
  change; access and ownership stay unchanged.” Show the actual public-room-name
  visibility consequence. Confirm pins that exact diff; a changed observation,
  ownership generation, control revision, preset revision or privacy epoch refuses
  `STALE_REVIEW`, not a best-effort apply to another/current room.
- Reviews expire in **5 minutes**. Cancel, preview and `dry_run=true` have **zero
  durable writes, audits, reservations, queue admissions or Discord mutations**.
  Duplicate cancel is harmless. A review token is an authenticated, expiring
  binding, not a durable pending operation; restart may invalidate unconfirmed
  reviews and asks for a new review. Do not log/store token bodies or room text.

Names/labels are untrusted literal text. Escape Markdown, suppress automatic and
reply-author mentions, and never interpolate into component IDs. Replies <=2,000
UTF-16 units; pack whole rows with ID/version/status and page footer. At most 10
rows/page, never truncate IDs or silently omit rows. Use a private attachment or
one-record view if a complete old/new diff cannot fit. No attachment on denial;
no raw text in logs, audit or metric labels. Unknown catalog/read failure is
`UNAVAILABLE`, never “no saved presets.” “Saved” means catalog commit only;
“requested” is not “applied”; “observed matching” is a timestamped read, not proof
that this operation caused the change or that the room still matches now.

## 4. Identity, atomicity and restart contract

Stable identities: guild/member Discord IDs; random immutable `preset_id`;
`preset_revision`; `room_id = (guild_id, channel_id, room_generation)`;
`ownership_generation` advances on every transfer/claim, even away-and-back;
`control_revision` advances on every managed name/limit mutation;
`privacy_epoch`; random `review_id` and `operation_id` (the stable handoff item ID).
Never identify a room by name, owner alone, a list offset or “latest.” Versions
are opaque monotonic strings, not timestamps. Return them only after authorization.

All bot-side name/limit writes, transfer/claim, deletion, erasure and preset
apply admission must share reviewed room/member fences; B cannot be wired only
in a new handler while existing slash/panel/gateway paths bypass them. At
confirmation, re-read authoritative preset and room facts, reauthorize and compare
all pinned versions plus exact observed name/limit. In one durable transaction,
CAS the versions and claim the operation before releasing the transaction.
No transaction stays open across HTTP. Transfer/delete/new control writes are
refused while an admitted room operation is unresolved; there is no automatic
transfer timeout or lease expiry that releases an uncertain write.

Dispatch once through the existing canonical name/limit policy, rename pacing
and Discord transport limits, using one PATCH containing exactly those two fields
if the future adapter supports it. The resulting literal must also become the
room's durable literal `custom_name` override, so later template refresh cannot
clobber it; never treat preset text as a template. That intent is operation-bound,
not a settled override before outcome reconciliation. Future local override
settlement and dispatch recovery must agree; success may not leave Discord and
stored override disagreeing. Existing Restore template remains an explicit manual
control after the operation settles; catalog expiry/delete does not clear that
room override. Partial dispatch through separate paths is not accepted silently;
revise the spec if one effect cannot be supported.
Read/revalidate immediately before dispatch; known revocation cancels a
provably never-dispatched operation. The bot-side admission point orders racing
opt-out/transfer; it cannot make Discord REST atomic with database state or
external administrator edits. An admitted request can reach Discord after an
opt-out, disconnect or external edit. Disclose that boundary. **Strict external
stale-state/ownership CAS is not available through a preflight read.** If the
host requires that stronger guarantee, choose A, not a lock-based promise.

| State/event | Required outcome / permitted next effect |
| --- | --- |
| Preview/cancel/expired review | No operation exists; zero writes and PATCHes; re-review to act |
| Valid save/overwrite | One catalog CAS commit; `SAVED`, no room PATCH |
| Valid apply admission | Durable `admitted` receipt; one room reservation; not yet applied |
| Same review + same canonical input | Unique review claim returns original operation/result after reauthorization; never redispatch |
| Same review + changed input | `CONFLICT`; no effect; a fresh ID cannot evade pinned CAS |
| Known pre-dispatch refusal | `REFUSED`; release reservation only with proof no dispatch was attempted |
| Successful PATCH response | `REQUEST_ACCEPTED`; bounded fresh GET may report `OBSERVED_MATCH` with observation time, never perpetual convergence |
| Known rejection/no acceptance | `FAILED`; no hidden compensating PATCH or retry; current transport guarantees must justify classification |
| Timeout, partial/unknown delivery, crash after admission | `UNKNOWN`; freeze room mutations/transfer, no automatic redispatch or rollback |
| Restart | Catalog survives; reconcile admitted/unknown receipts read-only first; never re-admit a consumed review |
| Readback matches without acceptance proof | `OBSERVED_MATCH`, dispatch outcome still unknown; matching alone does not release a potentially in-flight request |
| Concurrent erase/opt-out | Advance privacy epoch and stop admission; never-dispatched claims cancel; admitted boundary disclosed and unresolved fences preserved |

No automatic retry on 429, timeout or process crash for B, even though setting
two values looks idempotent: a late retry could clobber a newer owner's settings.
An unknown operation needs authorized reconciliation proving the original request
cannot still execute before releasing the fence. Neither a bounded GET miss,
a desired-state match, a new operation ID, an elapsed lease nor a restart is that
proof. Current seams are not claimed to provide it. **Liveness STOP, independent
of retention:** do not implement B without an accepted bounded reconciliation or
terminal room-retirement protocol; one uncertain request must not introduce an
indefinite room-management outage. No such protocol is established by this spec.
If finite recovery and erasure cannot coexist, B is deferred. Existing manual
controls remain A; B must not be partially enabled without shared-path coverage.

## 5. Storage, expiry and privacy disposition

B requires new durable member-scoped catalog and operation state; guild creator
settings and funnel/event history are not a safe substitute or a source from
which to reconstruct erased presets. No schema, grants or migration is added here.

| Proposed record | Lifetime / deletion |
| --- | --- |
| Preset | 90 days from explicit save/overwrite; list/apply does not refresh expiry. Expiry enforced on every read/admission even before cleanup; delete text on expiry in a bounded maintenance pass within 24 h. |
| Unconfirmed review | Signed/encrypted expiring token or process memory only; 5 min, no durable text copy or audit. Restart may invalidate it. |
| Settled operation receipt | <=24 h; codes/IDs/versions and review expiry only, no saved-name/label snapshot. After expiry the original review always refuses; expired IDs cannot create new work. |
| Admitted/unknown operation | Minimal identifiers/versions and recovery fence; no automatic TTL that forgets uncertainty. Any transient desired-text recovery copy must have an independently accepted deletion bound. **Retention unresolved; build STOP.** |

A confirmation atomically validates its embedded expiry with server time and
claims the review ID in the durable operation receipt (unique guild/member/review
key, not a token-body copy); retained claims outlive the review window. Only
**confirmation**, never preview, writes that receipt; process-memory review does
not mean a process-memory consumed-claim set. Receipt expiry therefore cannot
revive a review. Clock uncertainty refuses. Future decoding
must authenticate tokens, bind guild/member/operation/diff/versions/expiry,
reject tampering/unknown fields, and never trust a client-provided digest alone.
At most one unresolved operation/room, 3/member and 300/guild; saturation refuses
new B admission, not fence eviction. B enrollment must bound retained room
fences too; inability to fit these bounds without losing uncertainty keeps B off.

Privacy opt-out disables saving/applying, deletes catalog entries and invalidates
reviews via the privacy epoch; opting back in does not restore them. Self-delete
is a versioned CAS on one preset; guild deletion and member erasure must cover
catalog, labels, operation/review metadata and any recovery copy in the canonical
erasure manifest/maintenance protocol. Actual guild removal must use the existing
retention/manifest purge; temporary `unavailable=true` is not deletion, and rejoin
cancels pending guild purge (`crates/bot/src/gateway.rs:870-884`,
`crates/cutover/src/retention.rs:1057-1091`). Member erasure currently matches room
owner or original creator; new catalog/receipt identities need explicit coverage,
not only a room-row cascade. Deleting a preset does not revert a room
that was already changed; disclose the remaining Discord-visible name. A catalog
expiry/delete invalidates unadmitted reviews. No hidden hash archive, role grant,
member-content log or “backup preset” survives as an erasure workaround.

**CISO disposition: not accepted or waived by this proposal.** Before B build,
obtain exact-revision disposition for new personal text, linkable IDs/receipts,
consent/opt-out race, admitted effects versus prompt erasure, finite unknown-state
recovery, backup erasure, restore and rollback. Do not assert the current erasure
manifest already covers B. Erasure must not silently drop an unresolved external
effect fence; if prompt deletion and safe recovery cannot coexist, retain A.
Restores keep B disabled until privacy epochs, erased-subject handling, revisions
and consumed/unknown claims are reconciled. Rollback disables B; do not deploy a
writer that ignores its fences or repopulate presets from a stale snapshot.
No new permission, secret, external provider or spend is proposed.

## 6. Agent handoff: illustrative JSON, not an available API

One repo source: this document. No executable live flags or alternate agent
mutation route. Future agents can inspect/preview; they cannot impersonate member
consent or substitute a supplied owner ID. Default mode is preview; mutation
requires explicit typed operation, `dry_run=false` and the member's exact review
confirmation. Missing/contradictory mode is `INVALID_INPUT`, zero effects.
Envelopes <=8 KiB, depth <=4; duplicate-aware decode before map materialization,
reject duplicate/unknown fields, unsupported versions and wrong scalar types.
IDs/revisions are bounded strings; no unbounded text or raw remote errors in replies.

```json
{"version":1,"operation":"preview_apply","dry_run":true,"preset_id":"preset-example","preset_revision":"2","room_id":{"guild_id":"456","channel_id":"789","room_generation":"4"},"ownership_generation":"9","control_revision":"12"}
```

```json
{"version":1,"operation_id":null,"review_id":"review-example","preset_id":"preset-example","preset_revision":"2","room_id":{"guild_id":"456","channel_id":"789","room_generation":"4"},"outcome":"preview","reason":"REVIEW_REQUIRED","retryable":false,"next_action":"member_confirm_exact_diff","effects":{"catalog_writes":0,"room_patches":0}}
```

Every authorized result carries version, stable relevant IDs/revisions, outcome,
reason, retryable, next_action and known effect counts. Unknown effects use `null`,
not zero; denials redact resource identifiers and diffs. Proposed finite reasons:

| reason | outcome / retryable / next_action |
| --- | --- |
| `REVIEW_REQUIRED` | preview / false / member_confirm_exact_diff |
| `SAVED`, `DELETED` | saved, deleted / false / none |
| `REQUEST_ACCEPTED`, `OBSERVED_MATCH` | requested, observed / false / inspect_current_room |
| `CANCELLED`, `REVIEW_EXPIRED` | no_effect / false / none, new_review respectively |
| `INVALID_INPUT`, `QUOTA_REACHED` | refused / false / correct_request, delete_own_preset respectively |
| `NOT_AVAILABLE`, `PRIVACY_DISABLED` | refused / false / none, use_privacy_controls respectively |
| `STALE_REVIEW`, `CONFLICT` | refused / false / new_review, inspect_original_operation respectively |
| `UNAVAILABLE` | unknown_read / true / retry_read_only |
| `FAILED`, `UNKNOWN` | failed, unknown / false / inspect_original_operation, preserve_fence respectively |

## 7. Finite future acceptance matrix

Design oracles only, **not executed behavior tests**. Future independent tests
use synthetic identities, fixed clocks, recording Discord doubles and disposable
DB races, never production/staging tests. Each row checks stable reason,
next_action and exact durable writes/PATCHes, including negative controls.

| ID | Scenario | Required oracle |
| --- | --- | --- |
| P01 | Save own live room, apply to explicitly reviewed later own room | Name/limit only; separate storage and public-name consent; no automatic join effect |
| P02 | Foreign/gone room or preset, absent membership, admin non-owner | Same non-disclosing refusal; zero mutations |
| P03 | Transfer away/back, room replaced, same channel with new generation | All stale reviews refuse; no retargeting by current name/owner |
| P04 | Transfer/claim/delete/manual rename races admitted apply | Shared paths serialize or refuse; no late apply to a new bot-recorded owner |
| P05 | Overwrite/apply race; delete/recreate same label | Preset CAS holds; old ID/revision cannot select new content |
| P06 | Name/label Unicode/control/mention and limit 0/99/one-over/wrong types | Exact byte/scalar bounds and canonical policy; no truncation or access inference |
| P07 | Concurrent quota fills, saturated unresolved room/member/guild | Hard bounds atomic; no hidden eviction of uncertainty |
| P08 | Cancel, preview, dry-run, expiry and restart before confirmation | Zero durable writes/audits/claims/PATCHes; expired token never revived |
| P09 | Same review identical/changed input, crash before/after claim, receipt expiry | One admission; replay returns original; no resend or TTL resurrection |
| P10 | Timeout, 429, rejected PATCH, partial result and matching readback | Truthful accepted/failed/unknown/observed; no rollback, retry or evidence-free fence release |
| P11 | Opt-out/erasure before admission versus during admitted request | Epoch orders admission; disclose in-flight boundary; no catalog resurrection or unsafe fence deletion |
| P12 | 90-day expiry, overwrite renewal, guild deletion, restore/old binary | Enforce expiry on reads, erase text, preserve accepted recovery rules; B stays disabled on uncertainty |
| P13 | Missing/stale permissions/facts, lookup failure and empty list | Unknown != empty, no false saved/applied, no ID existence oracle |
| P14 | Forged/expired/cross-guild token, duplicate JSON keys and wrong mode | Fail closed with zero effects; client IDs grant no authority |
| P15 | Long private diff/list, Markdown and mentions, logs/metrics | Whole bounded rows and complete review; mentions closed; no personal text labels |

## 8. Offline documentation check and exact-spec gate

Run from repo root **after committing**; Python standard library, no network,
Discord or DB. It checks the committed document's link inventory and finite matrix,
not behavior, demand, API availability or security acceptance.

```sh
python3 scripts/check_docs_index.py --json
python3 - <<'PY'
from pathlib import Path
import hashlib, json, re, subprocess, sys
p = Path('docs/voice-presets-spec.md')
try:
    commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    blob = subprocess.check_output(['git', 'show', f'{commit}:{p.as_posix()}'])
    if blob != p.read_bytes():
        raise ValueError('DIRTY_SPEC')
    text = blob.decode('utf-8')
    cases = re.findall(r'^\| (P\d{2}) \|', text, re.M)
    if cases != [f'P{i:02}' for i in range(1, 16)]:
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

Before **any build/test card**, route the exact spec commit + SHA-256 to the CEO
for host-intent alignment: retain A, or name the authorized unmet host use case
and explicitly accept B's two-field scope, public-name visibility and unavoidable
in-flight/external-edit boundary. Separately obtain the CISO disposition in §5;
no security acceptance is recorded here. A revision invalidates prior exact-spec
acceptance. Missing evidence, unresolved retention/recovery or rejected boundaries
on 2026-11-02 means **defer B**, not passive approval. These are new product/design
boundaries, not a routine deployment approval hop. No implementation/test tasks,
new roles, schema execution, production tests or live activation follow this PR.
