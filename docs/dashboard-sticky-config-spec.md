# Dashboard sticky configuration contract

Status: **Proposed; design only**, 2026-10-26. Intent review by 2026-11-02.
Source inventory: `e1868d97d3519cae3f2ed91726e83e98f77dff0d`.
This is the repository source of truth for a future single-channel editor, not
an available page, endpoint, permission, schema migration or activation. Nothing
here authorizes a build or changes a sticky. Source-shipped is not deployed.

## Decision and no-build alternative

Staff should privately read one channel's saved sticky, review a body/debounce
edit and deliberately remove its configuration, without being told it was posted.
Recommend **contract-first, no editable dashboard over today's store**. There is
no safe read-then-call-put adapter: put resets the delivery fence and delete has
no stale-edit precondition. A warning cannot repair those concurrency gaps.

Smallest eventual surface: select one authorized channel, read its existing row,
edit body and debounce together, preview inert text, or confirm removal. No guild
listing, create/clone/move, enabled toggle, post-now, reset-fence or retry-delivery
button. Absent means use existing `/sticky` in that channel, subject to existing
gates; disabled rows remain read-only. No default is silently written on load.

**No-build today:** use `/sticky body:<text> debounce:<seconds>` and
`/sticky-remove` in the intended current channel with Manage Server authority.
Save affects subsequent eligible activity, not an immediate post. Reconfiguration
is also a recovery action today: inspect possible orphan posts before using it
on an uncertain sticky, following [sticky recovery](sticky-debounce-acceptance.md#uncertain-delivery-recovery).
Removal does not guarantee every Discord copy disappears. These are current
source controls, not a claim they are enabled in any particular deployment.

Private inspection and unsaved Discord preview retain their separate owners:
open PRs [#1674](https://github.com/TogetherWeOwn/two-bot-next/pull/1674) and
[#1761](https://github.com/TogetherWeOwn/two-bot-next/pull/1761) at this inventory.
They are not assumed merged or usable web APIs. Re-inventory before future work;
do not duplicate their runtime/tests. No web repository was inspected or changed;
its session, channel resolver, signer and pages are **unverified**, not absent.

## Source-backed controls and consumers

| ID / control | Current source, validator, writer and audit | Proposed consequence |
| --- | --- | --- |
| `SC-C1` read existing row | `sticky_messages` key `(guild_id,channel_id)`, creator/updater/time, enabled and post/claim columns: `crates/cutover/migrations/0150_sticky_messages.sql:23-40`; fence added in `crates/cutover/migrations/0454_sticky_delivery_uncertainty.sql:28-29`. `get_sticky` scopes exact guild/channel and returns body/debounce/enabled/last ID/time only (`crates/core/src/sticky.rs:382-429`). No audit on this read. | Composite ID, not a new UUID. No current revision, claim/fence or provenance readback. Never infer fence=false from omitted fields. A private bounded read contract is missing. |
| `SC-C2` body | `/sticky` required string, max 2000 (`crates/core/src/feature_commands.rs:246-251`); strict decoder rejects missing, duplicate or wrong-type options before lookup/audit (`crates/bot/src/command_runtime.rs:2974-3038`). `validate_body` checks 1–2000 UTF-16 units and sendable effective text (`crates/core/src/sticky.rs:79-89`). | Keep exact text, show UTF-16 counter, no implicit trim/template evaluation. Browser/JSON byte bounds do not replace domain validation. |
| `SC-C3` debounce | Optional slash integer 1–300; omitted normalizes to 5 (`feature_commands.rs:252-257`; `sticky.rs:91-104`). Passed as seconds to put. | Editor submits both body and explicit integer debounce; omission/null is invalid, not reset-to-5. No PATCH/omission-preservation claim about current slash put. |
| `SC-C4` save edit | `sticky_set` validates, calls `put_sticky` with enabled=true and invoker, then separately audits create/update ok/rejected (`command_runtime.rs:1927-2028`). Conflict upsert preserves last ID/time and creator/time but **clears claim_token, claimed_at and delivery_uncertain** (`sticky.rs:431-473`). | No CAS, preserving disabled edit, per-intent replay or atomic write+audit today. Future edit must preserve enabled/provenance/post/claim/fence state, refuse busy/uncertain and never call this upsert as an editor. |
| `SC-C5` removal | Delete by exact guild/channel returns prior message ID, no revision check (`sticky.rs:599-624`). Runtime deletes row, best-effort deletes that ID, separately audits delete ok/absent/rejected (`command_runtime.rs:2031-2087`). | Confirm exact channel and incarnation/revision; separate config removal from cleanup result. Missing row is not proof no copies exist. No bulk delete, history purge or automatic recreation. |
| `SC-C6` preview/dry-run | No sticky preview in this baseline's command definitions. Shared text safety counts UTF-16, neutralizes mass mentions then truncates, preserving non-mention joiners (`crates/core/src/message_safety.rs:14-106`). `post_message` uses the shared sender and usable receipt (`crates/discord/src/executor.rs:2841-2859`); default allowed_mentions disables parsing (`:1097-1102`). | Proposed local draft review is not a Discord send or proof of bot-equivalent rendering. Reuse accepted preview work when available, not an invented signed verb. |
| `SC-C7` delivery observation | Accepted nonbot same-guild activity: lookup, pure debounce precheck, atomic claim, post, record, retire previous best-effort, audit (`command_runtime.rs:1737-1925`). Claim arms fence; token-gated record/release disarms; ambiguous cleanup re-arms (`sticky.rs:475-597`). | No automatic live effect added by a page. Successful save only changes future input to this consumer. Last ID/time is historical evidence, not current-message existence or body equality. |

**Authority as shipped:** ManageGuild/Manage Server on both slash definitions;
router configured-guild, automation gate and permission refusal
(`crates/core/src/router.rs:494-516,709-715`). Runtime rejects a mismatched known
application (`command_runtime.rs:1345-1350`), defers before database I/O and
mutates nothing if defer fails (`:1544-1548`). Actor comes from the interaction
(`:2960-2972`), not an editable option. Automation gates/identity activation still
apply; recognized live Automations is not cleared by
`crates/core/src/activation.rs:51-96`. An enabled row grants none of these gates.

**Not settings:** body/debounce/enabled live in the product table, not
`guild_settings`, a hot snapshot or `/ops/config` overlay. No sticky body or
debounce environment key exists in the settings catalogue at this inventory.
`TWO_AUTOMATIONS=1` is the existing shared runtime gate (`sticky.rs:359-366`),
not a new per-sticky setting or a page enable switch. No credentials, environment
values, override edits, restart/deploy or production-target selector belong here.

**Audit limitation:** `audit_sticky` appends IDs/outcomes, not bodies
(`sticky.rs:626-650`); runtime logs audit failure without failing the operation
(`command_runtime.rs:2600-2628`). Put/delete and audit are separate SQL calls,
not a transaction. Do not claim atomic auditing, expose raw reasons/bodies, or
use an audit row as a Discord receipt. The proposed mutation contract requires
atomic configuration+revision+attribution+intent receipt+audit before enablement.

## Private human UX and authority

Future web reads **and** writes require a trusted current session, membership in
the configured guild and Manage Server authority. Also require current visibility
of the exact same-guild channel before disclosing saved content: the proposal
requires View Channel and Read Message History for content-bearing views, not
just a cached channel picker or a service key. This is a proposed read boundary,
not an assertion today's get enforces it. Deleted, foreign or inaccessible
channels give one nondisclosing denial; never enumerate foreign IDs or names.
Revalidate membership, channel identity/type/visibility and relevant bot capability
at read, preview and effect time. Missing facts/Discord outage fail closed, not
permission grants, fabricated channel facts or an empty configuration. Scope the
initial contract to a currently usable guild text channel; other kinds stay
unsupported until explicitly decided. Do not convert or create a channel.

Derive actor ID server-side from the authenticated human; require a canonical
nonzero u64 Discord ID string, never the browser's actor field or the existing
empty-string fallback. Trusted server chooses guild, channel and deployment;
compare all three before signing. HMAC authenticates the service, not human
permission. No direct browser DB access or signing key, new token, or self-grant.
Private responses are no-store; CSRF/session revocation checks precede effects.
No bodies in analytics, URLs, logs, error strings, audit exports or public PR
examples. Synthetic text only in this document. Raw and preview bodies render
as escaped inert text, not HTML/Markdown, embeds, autoloaded URLs or notifications;
role/user/mass mentions never ping. Do not log raw DB/Discord responses.

Accessible form: label body and seconds, announce counters/field errors, focus
error summary, keyboard-operable actions, textual status (not color alone).
Preview, Save configuration and Remove configuration are distinct controls.
Keep an unsent draft in private component memory on failure, never durable
localStorage, an offline queue or an automatic retry. Cancel sends nothing.

| Human state | Required display / action |
| --- | --- |
| Empty | Only a fresh successful absent read: “No saved sticky in this channel.” No dashboard create in this slice. Absence says nothing about old Discord copies. |
| Denied | “You cannot access this channel's sticky.” Hide content and disable actions; no foreign object details. |
| Invalid | Named field and non-echoing correction; no write. Debounce zero is invalid, not unset. |
| Conflict | “Configuration changed. Your draft is kept; reload and confirm again.” Never silently overwrite or rebase the draft. |
| Outage | Keep prior observation visibly stale with time; editing disabled until fresh authorized read. Unknown is not empty. |
| Claimed | “A re-post may be in progress. Configuration changes are unavailable.” Never offer lease expiry as an unlock. |
| Uncertain | “Discord delivery is uncertain. Inspect the channel before recovery.” No reset, re-save or automatic retry shortcut. |
| Saved | “Configuration saved. No post was sent by this save; Discord delivery is not confirmed.” Future eligible activity can consume it. |
| Removed | “Configuration removed. Discord cleanup: confirmed / failed / unknown / not attempted.” No “all messages deleted” claim. |

## Configuration versus delivery state

Maintain independent dimensions, with observation time and source SHA:

- `configuration`: absent, saved_enabled, saved_disabled, unknown. A fresh read
  confirms storage only, not active runtime, currently visible text or delivery.
- `delivery`: unknown, no_recorded_post, last_recorded, claimed, uncertain.
  Non-null last_message_id/time means **last recorded**, not still present or
  matching the latest saved body. Null means no recorded post, not “never sent.”
- `claim_present` and `delivery_uncertain`: true/false/**unknown**, never raw
  claim tokens. They can both be true. A claim arms uncertainty immediately;
  show both facts rather than assuming “claimed” is a safe outcome. Missing
  read fields force unknown and disable writes, not false defaults.
- `effective`: unknown until the actual enabled runtime/gates and a version-bound
  consumer observation can be evidenced. Neither poll timing, save receipt,
  enabled=true nor matching timestamps is an effective/applied acknowledgement.
- `last_confirmed_revision`: null today. No persisted association between the
  posted message and configuration incarnation/revision exists. Any future
  confirmed label needs an exact revision-bound definitive send/record receipt
  and observation time; even that is historical, not a fresh existence check.

Claim lease is 60 s, debounce defaults to 5 and permits 1–300 s. Debounce is
measured from last_posted_at, not a timer reset on each human message: elapsed
boundary plus the next eligible accepted activity can attempt a post
(`sticky.rs:150-171,203-228`). A pure Repost decision is not permission to send.
An armed uncertainty fence survives restart and lease expiry. Only proven no-send
or definitive token-owned completion may release it; obsolete/ambiguous outcomes
hold/re-arm. Previous-copy retirement is best-effort, so duplicates can remain
even after a confirmed replacement. Preserve [existing recovery](sticky-debounce-acceptance.md),
never clear a claim/fence just to show an edit as applied.

## Missing atomic and transport contracts (no implementation permission)

`SC-W1` bounded authorized read/preview: expose one row plus provenance, opaque
lifecycle revision and safe delivery facts; distinguish absent from outage.
Reuse accepted web/session/channel boundaries and private internal ingress.
`settings.get/set` and automations.import/export do not provide sticky CRUD.
`IMPLEMENTED_ACTIONS` has no sticky verbs (`crates/core/src/internal_actions.rs:674-694`).
Proposed read scope is existing **read**; edit/removal require existing exact
**admin**, not moderate (server content/broadcast configuration). This is a
recommendation for CISO disposition, not a mapping already granted: unmapped
verbs refuse even with admin (`internal_actions.rs:2051-2095`). AI token holders
cannot carry admin ([admin foundation](admin-registry.md)); this spec grants no
agent an apply path. Preview local-only requires no token. No live bearer gateway
or signing-v2 implementation is assumed. Reuse [receiver fences](internal-actions-receiver.md)
and [signing boundary](internal-action-signing-v2.md); do not broaden feature/key/
caller/guild allowlists. Existing settings/transfer receivers pin staging, so a
requested guild/flag cannot be treated as safe retargeting. No route is provided
here; unsupported stays unsupported without signing or network calls.

`SC-W2` mutation/lifecycle: an opaque incarnation-safe revision and expected
revision check **in the same transaction** as edit/removal, receipt and audit.
Every writer (slash, future editor and any recovery/import writer) must participate;
client hashes, updated_at equality and a separate SELECT are not CAS. Preserve
creator/time, enabled, last post fields and delivery state on edit; disabled rows
stay read-only in this slice. A claim or uncertain flag must refuse **atomically**
with no configuration/revision/audit-success change. Serialize with claim
acquisition and lifecycle transitions: either the edit wins before claim and the
claim gets new body, or claim wins and edit refuses. Never steal/expire a claim.

Delete confirmation binds composite ID, incarnation/revision and the last
observed message ID. Require an exact-channel confirmation with guild/channel
shown as authorized inert names/IDs; server mints a bounded confirmation binding
the full target and operation. Cancel, stale binding or changed target sends no
mutation. Delete requires the same busy/fence refusal, preserves audit/history
and records configuration removal before any best-effort known-ID cleanup.
Retain a lifecycle tombstone/receipt or equivalent authoritative incarnation
fence so delete/recreate ABA and late old-worker fence/cleanup outcomes cannot
alter a new sticky. Today's claim-blind fence method scopes only guild/channel;
today's token-gated completion is not a full incarnation protocol. Define and
prove that protocol across all writers before exposing removal. Do not implement
an unfenced delete followed by automatic recreation as undo. Undo is a separately
confirmed new intent against fresh state, never DB restore or history deletion.

Durable per-intent idempotency binds actor, deployment, composite ID, incarnation,
revision, operation and exact body bytes. Same key/body replays original receipt
without DML/audit/Discord repeats, even after later edits/removal. Same key with
changed bytes/target/actor refuses. Retry transport only under the accepted
receiver contract with fresh nonce/time/signature and the same intent/key/body;
nonce replay protection is not mutation idempotency. Unknown commit/cleanup stays
unknown: reconcile original intent, never retry with a fresh key or blind remove.
A reread alone cannot prove which intent committed or whether cleanup happened.

`SC-W3` delivery reporting: no post on save; future eligible activity may use
saved input. Removal may invoke existing authorized known-ID cleanup, separately
receipted as confirmed/failed/unknown/not_attempted; configuration success is
never downgraded into a second delete to repair cleanup. No scanning channel
history, deleting unrelated messages or new recovery action is authorized here.
Runtime lacks durable configuration-to-post revision and cleanup receipts today.
No historical-body match or all-copies-gone claim without its own accepted proof.

## Proposed bounded JSON and finite reasons

Examples are **offline proposal objects, not deployable request bodies or verbs**.
No endpoint, credential, target URL, apply command or production flag is given.
Only `mode=local_review` is supported by the document oracle: unknown/apply/dry_run
flags refuse locally with zero effects. A future apply surface must be separately
accepted, typed and authorized, not reached by toggling a boolean on this object.

Proposed future ingress bound: 16,384 UTF-8 bytes of the complete JSON envelope;
reject overbound before processing. Domain body bound remains 1–2000 UTF-16 units,
with visible effective output. Proposed transport refuses decoded U+0000 (SQL
text cannot store it); do not claim today's pure validator enforces that. IDs
are canonical decimal strings, revisions opaque equality tokens including
incarnation, never JavaScript numbers, client-generated hashes or ordering.

```json
{
  "schema":"dashboard.sticky_config.v1",
  "source_sha":"e1868d97d3519cae3f2ed91726e83e98f77dff0d",
  "limits":{"body_utf16":2000,"debounce_min":1,"debounce_max":300,"default_seconds":5,"envelope_bytes":16384},
  "read_example":{
    "request_id":"fixture-read-01",
    "sticky_id":{"guild_id":"100000000000000001","channel_id":"100000000000000002"},
    "revision":null,
    "configuration":{"state":"saved_enabled","body":"Community information","debounce_seconds":5},
    "delivery":{"state":"unknown","last_message_id":null,"last_posted_at_ms":null,"claim_present":null,"delivery_uncertain":null,"last_confirmed_revision":null},
    "reason":"READ_ONLY","retryable":false,"next_action":"await_accepted_contract"
  },
  "review_example":{
    "mode":"local_review","request_id":"fixture-review-01","intent_id":"fixture-intent-01",
    "sticky_id":{"guild_id":"100000000000000001","channel_id":"100000000000000002"},
    "expected_revision":"fixture-incarnation-01:revision-07",
    "operation":"edit","draft":{"body":"Community information","debounce_seconds":10},
    "reason":"PREVIEW_ONLY","retryable":false,"next_action":"review_without_sending","effects":[]
  },
  "remove_target_example":{
    "operation":"remove","intent_id":"fixture-remove-01",
    "sticky_id":{"guild_id":"100000000000000001","channel_id":"100000000000000002"},
    "expected_revision":"fixture-incarnation-01:revision-07",
    "last_observed_message_id":"100000000000000003",
    "confirmation":"fixture-server-bound-target-01"
  },
  "receipt_fields":["request_id","intent_id","sticky_id","committed_revision","configuration_outcome","delivery_state","cleanup_state","reason","retryable","next_action"],
  "reasons":{
    "PREVIEW_ONLY":{"retryable":false,"next_action":"review_without_sending"},
    "EMPTY":{"retryable":false,"next_action":"use_current_channel_controls"},
    "READ_ONLY":{"retryable":false,"next_action":"await_accepted_contract"},
    "UNSUPPORTED":{"retryable":false,"next_action":"use_existing_controls"},
    "DENIED":{"retryable":false,"next_action":"reauthenticate_or_stop"},
    "INVALID_INPUT":{"retryable":false,"next_action":"correct_field"},
    "CONFLICT":{"retryable":false,"next_action":"reload_then_reconfirm"},
    "UNAVAILABLE":{"retryable":true,"next_action":"retry_authorized_read"},
    "CLAIMED":{"retryable":false,"next_action":"wait_then_reread"},
    "DELIVERY_UNCERTAIN":{"retryable":false,"next_action":"inspect_before_authorized_recovery"},
    "COMMIT_UNKNOWN":{"retryable":false,"next_action":"reconcile_original_intent"},
    "SAVED_UNVERIFIED":{"retryable":false,"next_action":"reread_configuration"},
    "REMOVED_CLEANUP_UNKNOWN":{"retryable":false,"next_action":"inspect_cleanup_receipt"}
  }
}
```

A future read adds authoritative observed_at and source/deployment identity;
null revision forbids mutation, and null delivery facts never mean clear.
Error example, sharing the same finite reason table and bounded ID-only envelope:
`{"request_id":"fixture-write-01","intent_id":"fixture-intent-01","revision":null,"reason":"COMMIT_UNKNOWN","retryable":false,"next_action":"reconcile_original_intent"}`.
Receipts never echo body. Replays return the original committed revision, not a
claim the current row still matches; get fresh state before any further edit.
`retryable` on an unavailable read is not permission to queue background writes.

## Acceptance matrix and runnable checks

These are future protected-test oracles, **not executed runtime acceptance**.
Use independent acceptance ownership, mock Discord and disposable test databases
only. Preserve baseline RED proof, unchanged controls and immutable path/hash
handoffs; environment/compile failures are not behavioral RED.

| ID | Input / interleaving | Required oracle |
| --- | --- | --- |
| `SC-F01` | Absent, disabled, existing enabled, unavailable exact-channel read | Distinct states, no invented default/provenance/revision/fence, no auto-create or enabled flip. |
| `SC-F02` | 0/1/2000/2001 UTF-16; 1000/1001 emoji; 2000 multibyte BMP; whitespace/zero-width only; NUL; envelope byte boundary | Match domain bounds and effective text; separate transport limits. No SQL/Discord on invalid; no bytes-as-character-count shortcut. |
| `SC-F03` | Debounce omitted/null/string/bool/fraction/0/1/300/301 | Proposed editor requires explicit integer 1–300; separately prove current slash omission=5. |
| `SC-F04` | Missing/current channel, foreign/deleted/wrong-kind channel; stale picker; permission revoked after load/preview | Recheck trusted membership/admin/visibility at each boundary; no content or signature on denial/outage. No fallback channel. |
| `SC-F05` | Literal markup, links, @everyone/@here with separators, role/user mentions, multilingual joiners, expansion at bound | Inert browser review, no fetch/ping; current shared send safety preserved; local draft display not claimed as Discord receipt. |
| `SC-F06` | Two edits; slash vs page; removal vs edit; delete/recreate ABA | One atomic precondition winner; conflict preserves newer incarnation/content/draft. Existing unconditional put/delete cannot pass this oracle. |
| `SC-F07` | Claim wins before edit/removal, including >60 s with fence; edit wins before claim | Busy/fence refusal without clearing anything; or claim uses new config. No stale worker records/fences/deletes against a new incarnation. |
| `SC-F08` | Same intent/key/body twice, then after later change; key reused with changed actor/target/body | Original receipt once, one configuration transition/audit/cleanup at most; mismatch refusal; no replay overwrite. |
| `SC-F09` | Definite write refusal, DB/audit failure, lost commit response | Refusal has no committed transition; atomic audit failure rolls back; ambiguous commit gives COMMIT_UNKNOWN and same-intent reconciliation, not fresh-key retry. |
| `SC-F10` | Saved config but gates off/no activity; next eligible human message; bot/foreign message; post timeout/unusable ID | Save sends zero posts; consumer retains existing gates/claim order. Unknown delivery remains unknown/fenced, never “applied.” |
| `SC-F11` | Confirmed removal; known-ID cleanup fails/times out; absent replay; confirmation changed/cancelled | Config result separate from cleanup; no history deletion, new incarnation removal or duplicate effect. Absent is not all-copies-gone proof. |
| `SC-F12` | Same local review twice; unknown/apply mode, dry_run or target flag; oversized JSON | Deterministic no-effect review/refusal. Zero network, signing, nonce/idempotency claim, SQL, audit or Discord; no wrong-flag path to live effects. |

Run from repository root, Python stdlib only:

```sh
python3 -m doctest -v docs/dashboard-sticky-config-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

```pycon
>>> import json, pathlib, re
>>> doc = pathlib.Path("docs/dashboard-sticky-config-spec.md").read_text()
>>> fence = "`" * 3
>>> m = json.loads(re.search(fence + r"json\n(.*?)\n" + fence, doc, re.S).group(1))
>>> assert m["schema"] == "dashboard.sticky_config.v1" and re.fullmatch(r"[0-9a-f]{40}", m["source_sha"])
>>> src = pathlib.Path("crates/core/src/sticky.rs").read_text()
>>> constants = dict(re.findall(r"pub const (\w+): (?:u64|usize) = (\d+);", src))
>>> assert [m["limits"][k] for k in ("body_utf16", "debounce_min", "debounce_max", "default_seconds")] == [int(constants[k]) for k in ("MAX_BODY_CHARS", "MIN_DEBOUNCE_SECONDS", "MAX_DEBOUNCE_SECONDS", "DEFAULT_DEBOUNCE_SECONDS")]
>>> units = lambda s: len(s.encode("utf-16-le")) // 2
>>> assert [units(s) for s in ("", "x", "x"*2000, "x"*2001, "😀"*1000, "😀"*1001, "界"*2000)] == [0, 1, 2000, 2001, 2000, 2002, 2000]
>>> assert len(("界"*2000).encode()) == 6000
>>> read = m["read_example"]; review = m["review_example"]; target = m["remove_target_example"]
>>> assert read["revision"] is None and read["delivery"]["claim_present"] is None and read["delivery"]["delivery_uncertain"] is None
>>> assert read["delivery"]["last_confirmed_revision"] is None and read["reason"] == "READ_ONLY"
>>> assert review["sticky_id"] == read["sticky_id"] == target["sticky_id"] and target["expected_revision"] == review["expected_revision"]
>>> assert all(re.fullmatch(r"[1-9][0-9]{0,19}", s) and int(s) <= 2**64-1 for s in review["sticky_id"].values())
>>> assert review["mode"] == "local_review" and review["effects"] == [] and review["reason"] == "PREVIEW_ONLY"
>>> local_result = lambda obj: "PREVIEW_ONLY" if set(obj) == set(review) and obj.get("mode") == "local_review" and obj == review else "UNSUPPORTED"
>>> assert local_result(review) == local_result(json.loads(json.dumps(review))) == "PREVIEW_ONLY"
>>> assert all(local_result(dict(review, mode=mode)) == "UNSUPPORTED" for mode in ("apply", "production", "dry_run", None))
>>> assert local_result(dict(review, dry_run=True)) == local_result(dict(review, target="production")) == "UNSUPPORTED"
>>> assert len(json.dumps(m, ensure_ascii=False).encode()) < m["limits"]["envelope_bytes"]
>>> byte_bound = lambda b: len(b) <= m["limits"]["envelope_bytes"]
>>> assert byte_bound(b"x"*16384) and not byte_bound(b"x"*16385)
>>> assert all(isinstance(v["retryable"], bool) and v["next_action"] for v in m["reasons"].values())
>>> assert all(obj["reason"] in m["reasons"] and all(obj[k] == m["reasons"][obj["reason"]][k] for k in ("retryable", "next_action")) for obj in (read, review))
>>> put = src.split("pub async fn put_sticky(", 1)[1].split("pub async fn claim_sticky_post(", 1)[0]
>>> assert all(s in put for s in ("claim_token      = NULL", "claimed_at       = NULL", "delivery_uncertain = FALSE"))
>>> get = src.split("pub async fn get_sticky(", 1)[1].split("pub async fn put_sticky(", 1)[0]
>>> assert "SELECT guild_id, channel_id, body" in get and "delivery_uncertain" not in get and "claim_token" not in get
>>> actions = pathlib.Path("crates/core/src/internal_actions.rs").read_text().split("pub const IMPLEMENTED_ACTIONS", 1)[1].split("];", 1)[0]
>>> assert "sticky" not in actions
>>> settings = pathlib.Path("crates/core/src/settings.rs").read_text().split("pub const SETTING_CLASSES", 1)[1].split("\n];", 1)[0]
>>> assert "STICKY" not in settings
>>> refs = re.findall(r"(crates/[a-zA-Z0-9_./-]+\.(?:rs|sql)):(\d+)(?:-(\d+))?", doc)
>>> assert refs and all(pathlib.Path(p).is_file() and 1 <= int(a) <= int(b or a) <= len(pathlib.Path(p).read_text().splitlines()) for p, a, b in refs)
>>> assert len(set(re.findall(r"^\| `(SC-F\d+)` \|", doc, re.M))) == 12
>>> print(json.dumps({"schema":m["schema"],"planned_fixtures":12,"status":"pass"}, sort_keys=True))
{"planned_fixtures": 12, "schema": "dashboard.sticky_config.v1", "status": "pass"}

```

This checks document examples, bounds arithmetic, source gaps and paths only.
It is not an implementation validator, a bot render/authorization check, an
execution of SC-F01–F12 or staging PASS. Rust/DB/Discord tests and web/runtime
behavior are not claimed. No protected tests change and no Rust doc include is
added; compiling Rust or changing CI selection is unnecessary for this slice.

## Exact-revision intent/security gate and bounded future slices

Before any build card, record host/CEO intent alignment against the exact issue
spec revision, repository commit and file SHA-256. Missing acceptance defaults
**no-build**, never timeout consent. Record CISO disposition of proposed content
read authority, new verb/scope/holder mapping and lifecycle/cleanup boundary.
Do not turn an existing shared-service signer into unbounded user authority.
Use the established alignment path; a saved request is not accepted intent.

Decision questions: accept read-existing/edit/remove-only scope and no-create/no-
toggle/no-recovery? Accept storage-only copy and refusal during claims/uncertainty?
Accept exact admin write/read content scope with fresh channel visibility and
human attribution, without AI-admin or production activation? Who supplies the
accepted web authorization/channel/signer evidence? These are design decisions,
not a new routine deployment approval hop. Ordinary future authorized deployment
retains existing exact-head gates, staging/health evidence and rollback.

After alignment only, independently tested **bot-side slices, each ≤1 day**:

1. `SC-W1` private one-channel read model: safe missing/unknown facts, provenance,
   explicit authority/scoping contract; reuse separately owned inspection work.
2. `SC-W2a` incarnation/revision plus claim/lifecycle refusal primitives and
   protected concurrency/ABA tests across existing writers; no page writes yet.
3. `SC-W2b` atomic preserving edit, audit and per-intent receipt/replay, using
   W2a; no fence reset or immediate post.
4. `SC-W2c` confirmed removal with incarnation-safe late-worker handling and
   durable separate cleanup result, using W2a; if not ≤1 day, split before start.
5. `SC-W3` version-bound historical delivery/readback evidence; no channel-history
   scanner or recovery mutator. Keep unknown until this is proved.

These are recommendations, not created/authorized build cards. A later browser
card belongs to its own web project after the accepted bot/security contracts;
no second repo or web code in this change. Review metric by 2026-11-02: seven
controls mapped, twelve fixture oracles decided, zero saved=posted claims.
Unverified: actual pages/session guards, deployed flags/keys/activation, current
guild/channel permissions, effective consumer state and real Discord delivery.
ACT: publish proposed contract. WHY: prevent fence resets, stale overwrites and
false delivery claims. UNDO: revert documentation; no operational state changed.
