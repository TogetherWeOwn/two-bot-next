# Dashboard custom-response configuration contract

Status: **proposed; design only**, 2026-10-25. Intent review target: 2026-11-01.
No page, endpoint, migration, grant, runtime activation or live write is added.
This file is the single repository source for the proposal and offline fixtures.
Source inventory: `c3af7333df80a766f5d4c229506a3bce53680828` (main baseline).
Source-shipped does not mean deployed, enabled or verified against Discord.

## Decision and alternatives

Guild staff need to find a saved response, inspect it privately, correct it and
pause it without accidentally erasing metadata or misreporting publication.
**Recommend bot-contract-first; no editable page over bulk transfer today.**

- **No-build/current controls:** use `/command-list`, `/command-inspect`,
  `/command`, `/command-disable`, `/command-enable`, `/command-remove`. These
  already cover the underlying capabilities privately for Manage Server staff,
  subject to automation/identity/activation gates. Replacement has the caveat below.
- **Bulk-transfer shortcut:** reuse export/import for deliberate document transfer,
  not per-row editing. Export omits enabled/provenance/revision/publication, and
  import has overwrite/partial-result semantics, not PATCH or CAS. Rejected.
- **Minimal eventual page:** bounded saved-response list, one detail form,
  saved-template preview and explicit enable/disable/delete. No rename, clone,
  prefix search, arbitrary execution, draft render or new placeholder prerequisite.
  Every operation stays unavailable until its accepted bot contract exists.

The web repository was not checked out or modified. Evidence has been requested
from its owning engineer/card for existing pages, APIs, authorization and signer.
Their existence is **unverified**, not asserted absent; reuse accepted evidence
before creating a browser build in that project's own card. The separate
[ticket](dashboard-ticket-config-spec.md) and
[onboarding](dashboard-onboarding-self-roles-spec.md) designs are not this page.

## Source-backed capability inventory

| ID | Shipped source / seam | Page consequence |
| --- | --- | --- |
| `CR-I1` | `automation_commands` primary key `(guild_id,name)`; description, template, nullable text_trigger, enabled, creator/updater/times (`crates/cutover/migrations/0130_custom_commands.sql:15-41`). `StoredCommand` exposes only the first six fields (`crates/core/src/custom_commands.rs:179-190`). | Composite saved ID, not UUID or Discord command ID. Name immutable during that row's life; no rename. Delete/recreate is not the same incarnation. No current revision token or metadata readback. |
| `CR-I2` | `list_commands` includes disabled rows, sorted name ascending; `get_command` scopes exact guild/name (`crates/core/src/custom_command_store.rs:133-156`). Slash pagination partitions that bounded snapshot, not a SQL cursor (`crates/core/src/custom_commands.rs:710-786`). | Show full name, trigger and enabled state, then detail. Default page 1; reject invalid/out-of-range pages, never silently clamp. Read failure is unknown, not empty. |
| `CR-I3` | `/command` define/replace (`crates/core/src/feature_commands.rs:81-110`); service locks guild capacity, validates, writes and audits transactionally (`crates/core/src/custom_command_service.rs:33-84`). | Create/edit reuse domain/service validation, not direct web SQL. No partial update or atomic expected-revision argument exists. |
| `CR-I4` | Omitted description becomes `Custom command`, omitted trigger becomes None (`crates/discord/src/custom_commands.rs:784-791`); put sets enabled=true (`crates/core/src/custom_command_service.rs:72-85`). SQL preserves original creator/time but replaces content/enabled (`crates/core/src/custom_command_store.rs:193-227`). | Today's template-only replacement can reset metadata and resume a paused row. A preserving editor needs a new atomic contract; do not claim the pending omission-preservation work is shipped. |
| `CR-I5` | Disable/enable take only name, commit only a real flip, preserve other fields; missing/no-op has no DML/audit/publish. Enable revalidates saved content; disable can pause invalid content (`crates/core/src/custom_command_service.rs:118-186`; `crates/discord/src/custom_commands.rs:847-911`). | Separate state action, never emulate disable by delete or replacement. An unchanged enable does not repair failed publication. |
| `CR-I6` | Delete by guild/name, audited in service transaction (`crates/core/src/custom_command_store.rs:255-267`; `crates/core/src/custom_command_service.rs:87-104`). | Confirm exact name/guild and last observed state. No bulk delete, DB rollback or deletion of invocation/audit history. |
| `CR-I7` | `/command-inspect` summary/template/preview reads saved rows, including disabled ones. Strict sample arguments bound 500 UTF-16 units but ignored by renderer (`crates/discord/src/custom_commands.rs:650-723`; `crates/core/src/custom_commands.rs:788-792,939-1054`). | Private saved preview only, mention-safe and escaped, truncation labeled. No command.run fact, cooldown/claim consumption, channel message or mutation. No unsaved render is assumed. |
| `CR-I8` | Full-set publisher loads DB rows and merges gated builtins via publish_set; updates snapshot only after successful Discord receipt (`crates/discord/src/custom_commands.rs:133-176`). Manage saves commit before publication (`:831-838,901-908`). | Storage success and registry synchronization are different outcomes. Neither proves a member can invoke successfully. |
| `CR-I9` | Signed automations.export/import are the only automation verbs in the implemented allowlist (`crates/core/src/internal_actions.rs:674-694`). Export is sorted version 1; fields are name/description/template/text_trigger only (`crates/core/src/automation_transfer.rs:29-74`). | No per-definition dashboard list/get/put/remove/set_enabled/preview/publication-status wire contract today. Export is not the page read model. |

### Fields, bounds and literal templates

- **Name:** canonical lowercase `[a-z0-9_-]{1,32}`; show the canonical result
  before create confirmation. Do not silently rename an existing row.
- **Description:** 1–100 UTF-16 units; **template:** 1–2000 UTF-16 units;
  rendered content also at most 2000. Count UTF-16, not bytes/code points.
  SQL CHECKs use character-counting `length()` (`0130_custom_commands.sql:29-34`),
  so SQL alone is insufficient: 51 emoji fit its description check but exceed
  the Rust/UI 100-unit bound. Keep application validation authoritative.
- **text_trigger:** null means slash-only; otherwise `![a-z0-9_-]{1,32}`,
  lowercased at the Discord edge. It adds fixed-`!` execution to the saved
  command, not a configurable guild prefix or an alternative slash type.
- Validation anchors: `crates/core/src/custom_commands.rs:19-32,121-132,172-175,262-285`.
  Names reserve builtin/voice/assistant surfaces even when those features are off
  (`:315-339`). Inspection uses the narrower dispatch reservation set (`:887-902`).
  Capacity counts disabled rows too and reserves the maximal builtin set; updates
  at capacity remain possible (`:288-305`; `custom_command_service.rs:43-49`).
- Trigger ownership is per-guild, case-insensitive, including disabled rows
  (`0130_custom_commands.sql:37-41`; `custom_command_service.rs:56-70`). Existing
  put can store a builtin trigger that execution skips and enable rejects
  (`custom_commands.rs:580-598,648-650,683-693`). Never call that trigger usable;
  preserve the validator difference rather than invent an identical-check claim.
- Only `{user}`, `{username}`, `{server}`, `{channel}` render, in one pass
  (`custom_commands.rs:19,144-170`). Validation rejects unknown lowercase-alpha
  tokens such as `{guild}` or `{args}` (`:118-132`); other brace text such as
  `{User}` stays literal (`:74-88`). The raw renderer's unknown-token literal
  fallback is not permission to bypass validation. `{args}` remains
  [design only](custom-command-args-spec.md). Arguments do not authorize new
  evaluation. No HTML, script, SQL, include, network, or nested substitution.

Pending work is not a prerequisite claimed delivered: clone is open PR
[#1732](https://github.com/TogetherWeOwn/two-bot-next/pull/1732), draft-preview
acceptance is open PR [#1740](https://github.com/TogetherWeOwn/two-bot-next/pull/1740),
and name-prefix filtering/omission-preserving edit are separately owned pending
work at this inventory. Existing prefix **execution** is already implemented
(`crates/core/src/custom_commands.rs:631-693`); it is not name-prefix filtering.
Re-inventory merged changes before any build; do not copy these pending branches.

## Minimal human page, conditional on accepted seams

Private guild-selected **Custom responses** page, no analytics or member data.
List uses existing name-order/page semantics: complete rows, page count, previous/
next controls; no prefix filter dependency. A fresh successful zero-row read alone
shows “No saved custom responses.” Unsupported/read-only/unavailable are separate
states with the precise missing capability; no empty form pretending data loaded.

Detail shows immutable name, description, template, optional trigger and enabled.
Bounded text inputs show UTF-16 counters and field errors linked by accessible
labels; keyboard focus goes to error summary, then the offending field. Save,
preview, disable and delete have distinct accessible names; no color-only state,
keyboard trap, auto-submit on focus, or optimistic success toast. Preserve an
unsent draft privately during errors; no localStorage/offline queue of templates.

Create requires name/template and an explicit review of default description,
null trigger and enabled=true. **Proposed edit semantics:** omitted description,
trigger and enabled preserve the exact saved values; explicit null clears only
trigger, never description/template. Empty text is invalid, not a reset. Template
edit does not resume a disabled row. Changing enabled uses its own action. This
is a missing contract, NOT current put/import behavior. Do not implement a
read-then-unconditional-write adapter: another slash/import writer can race it.

Saved Preview uses the saved definition/revision, authorized viewer and trusted
guild/channel context; missing context reports unavailable, never invented real
member/channel facts. Show an escaped, inert text block and “Preview only; nothing
sent.” Raw and rendered templates are untrusted text, no HTML/Markdown execution,
external-resource load, link auto-fetch or mentions. Saved preview is not a draft
preview: if form input changed, label the preview's saved revision explicitly.
A local form review may show input literally, never claim bot-equivalent rendering.

Disable asks confirmation with name and warning that picker removal may lag;
enable shows the effect and confirms validation. Delete additionally requires
an exact-name confirmation and explains irreversible loss of this definition;
recommend reversible disable first. No deletion of history. On conflict preserve
draft, reload and reconfirm. Cancel sends nothing. Undo is a new deliberate change
against fresh state, not a replay of an old replacement or automatic re-create.

## Authority and signed receiver fences

Current management checks ManageGuild, configured guild/application and automation
gates before lookup (`crates/core/src/router.rs:516-563,705-710`;
`crates/discord/src/custom_commands.rs:193-220,264-280,734-738`). The future web
server must independently prove current session membership and Manage Server on
**reads and writes**, CSRF protection, private/no-store responses, revocation
handling and trusted selected guild. Derive actor_id from that session, not form
input; HMAC service identity is not an admin identity or authorization proof.
Creator/updater/time exist in SQL, but aren't exposed by current reads/export:
show unknown attribution until an accepted metadata read exists. Never log
submitted templates, raw request/Discord bodies, secrets, signatures or member IDs.

The shipped receiver verifies exact bytes, durably burns nonce, then authorizes
flags (`crates/bot/src/internal_action_http.rs:1485-1499`). Import derives caller
from key mapping, requires idempotency and actor snowflake, and pins staging guild
(`:2897-2947`); export pins the same guild (`:2826`). Caller-supplied guild cannot
retarget it. Compare trusted session guild to deployment guild **before signing**
either operation. No production or generic multi-guild route is implied.

The v1 MAC does not bind audience/caller/idempotency headers as v2 proposes;
[v2](internal-action-signing-v2.md) is designed, not implemented. Ignore unsigned
identity/audience headers as authority. Keep private ingress, configured key/
caller mapping, nonce/freshness, feature allowlists and receiver fences. Never
widen them, add a new gateway, obtain a key or enable a flag for this proposal.
The [admin registry](admin-registry.md) is foundation, not a deployed bearer gateway;
closed-gateway PRs are pending, not usable prerequisites. Existing pure scope map
requires read for export and exact admin for import, refuses unmapped actions
(`crates/core/src/internal_actions.rs:2051-2095`); AI holders cannot carry admin
(`docs/admin-registry.md:23-28`). Proposed new verbs must receive an explicit scope/
holder decision, never inherit admin access because they manipulate “only text.”
Recognized live Automations is not cleared by `LIVE_CLEARED_CAPABILITIES`
(`crates/core/src/activation.rs:51-96`). Preserve runtime activation, text-command,
moderation-admission and visibility/erasure fences; a saved enabled flag grants none.

## Concurrency, transfer and honest publication

**Missing W2:** lifecycle-safe revision/precondition checked atomically with the
write under the guild lifecycle lock, across slash, import, toggle, delete and
future page writers. Current SQL has no revision/CAS; timestamps, client hashes,
page generation or a last-read check are not CAS. Acceptance must cover concurrent
writers and delete/recreate ABA. Until then writes stay unsupported, not
last-write-wins behind a “stale” warning. Reuse current transactions/audits; do not
confuse internal idempotency-mismatch 409 with a definition revision conflict.

For existing deliberate transfer reuse automations.import/export as-is. Imports
accept version 1, validate and plan occurrences in order; changed content requires
overwrite=true plus separate overwrite capability. Fresh rows enable, rewrites
preserve enabled and original creator/time. Accepted writes/occurrence audits/
summary commit together; skipped/conflicts may coexist with imported entries
(`docs/internal-actions-receiver.md:220-262`). Omitted import description defaults
to `Imported via automations.import`, trigger to null
(`automation_transfer.rs:111-129`). Export is not a full-fidelity backup of flags,
audit or publication and doesn't imply member invocation works. Do not rewrite
these executors or expose legacy import/migration as part of this page.

Import replay uses the same key and exact body, fresh nonce/time/signature;
new content/actor/intent needs a new key. Original summary is replayed without
reapplying after subsequent edits (`internal_action_http.rs:2933-2974,3005-3033`).
Ambiguous commit/needs_reconciliation means stop and reconcile, not a fresh key
or repeated overwrite. **No current server dry-run flag:** never send
`dry_run:true` to a mutator assuming safety. Proposed dry-run is local shape/
bounds/intent review only: no network, signature, nonce burn, claim, SQL, audit,
Discord call or authorization/freshness claim. Unknown mode/flags fail closed.

Publication requirements for W3:

1. Saved get/write only means persisted definition. Prefix and slash need their
   own runtime gates; a Discord picker entry is not execution success.
2. A page operation must NEVER bulk-replace the guild registry with one row,
   one page, an export subset or an empty set after a failed read. Reuse the
   complete merged builtin+enabled-custom publisher; refuse unknown desired rows.
   The compiled-only commands CLI intentionally excludes DB custom commands and
   can remove them (`docs/command-publish.md:40-47`): not a page repair shortcut.
3. After save-success/publish-failure retain saved state and explain synchronization
   failed/unknown. Do not retry delete/put/toggle to “fix” publication. No-op toggle
   does not resync (`crates/discord/src/custom_commands.rs:901-908`).
4. Existing gateway connection-event retry is bounded behavior, not a page SLA
   (`crates/bot/src/command_runtime.rs:1224-1239`). An authorized executor can
   reconcile the complete registry through existing operational procedures;
   the page may link guidance, not expose restart/deploy or invent a publish verb.
5. W3 must expose authoritative guild/application, desired generation/hash,
   observed generation/hash and receipt/observation time before any
   `publication_confirmed` label. Today's process-local published snapshot is
   not a durable page receipt and later edits invalidate it. Timeout stays unknown;
   no success promise until observed evidence matches. No auto DB rollback after
   a failed Discord write, and no unrelated command deletions on recovery.

## Proposed machine contract and runnable document fixtures

These are proposal-local capability/reason IDs, **not deployed verbs or HTTP
routes**. Potential names below locate missing work exactly; final wire schemas
require accepted W1/W2/W3 and independent protected acceptance tests first.
All IDs/Discord snowflakes are strings. Proposed page envelope is
`{schema,source_sha,guild_id,definition_id,revision,request_id,reason,retryable,next_action}`;
`definition_id={guild_id,name}`, revision opaque/null (null forbids mutation).
List adds `page,total_pages,items`; detail adds the five saved fields. Successful
reads require authoritative absent versus unavailable distinction. No provenance
or publication field may be fabricated from export. Mutation receipts bind the
exact intent and committed revision; they are separate from publication evidence.

```json
{
  "schema":"dashboard.custom_responses.v1",
  "source_sha":"c3af7333df80a766f5d4c229506a3bce53680828",
  "definition_fields":["name","description","template","text_trigger","enabled"],
  "placeholders":["user","username","server","channel"],
  "bounds_utf16":{"description":100,"template":2000,"rendered":2000},
  "edit_omission":"preserve_requires_atomic_contract",
  "adapter":{
    "envelope":["schema","source_sha","guild_id","definition_id","revision","request_id","reason","retryable","next_action"],
    "definition_id":["guild_id","name"],
    "mutation_receipt":["request_id","intent_id","definition_id","committed_revision","storage_outcome","publication_state"],
    "idempotency":{"key_grammar":"[A-Za-z0-9._:-]{8,200}","requirement":"per_confirmed_mutation_intent","retry":"same_key_exact_body_fresh_nonce","changed_body":"refuse","unknown_commit":"reconcile_same_intent"},
    "dry_run":{"mode":"local_only","reason":"PREVIEW_ONLY","effects":[]}
  },
  "capabilities":[
    {"id":"CR-C1","op":"list","candidate":"automations.list","state":"missing","gate":["W1"]},
    {"id":"CR-C2","op":"read","candidate":"automations.get","state":"missing","gate":["W1"]},
    {"id":"CR-C3","op":"create_edit","candidate":"automations.put","state":"missing","gate":["W1","W2","W3"]},
    {"id":"CR-C4","op":"enable_disable","candidate":"automations.set_enabled","state":"missing","gate":["W1","W2","W3"]},
    {"id":"CR-C5","op":"delete","candidate":"automations.remove","state":"missing","gate":["W1","W2","W3"]},
    {"id":"CR-C6","op":"saved_preview","candidate":"automations.preview","state":"missing","gate":["W1"]},
    {"id":"CR-C7","op":"publication_read","candidate":"automations.publication_status","state":"missing","gate":["W1","W3"]},
    {"id":"CR-C8","op":"transfer_export","candidate":"automations.export","state":"source_shipped","gate":[]},
    {"id":"CR-C9","op":"transfer_import","candidate":"automations.import","state":"source_shipped","gate":[]}
  ],
  "reasons":{
    "PREVIEW_ONLY":{"retryable":false,"next_action":"confirm_or_edit"},
    "EMPTY":{"retryable":false,"next_action":"create_when_supported"},
    "UNSUPPORTED":{"retryable":false,"next_action":"use_existing_controls"},
    "READ_ONLY":{"retryable":false,"next_action":"review_missing_gate"},
    "INVALID_INPUT":{"retryable":false,"next_action":"correct_field"},
    "NOT_FOUND":{"retryable":false,"next_action":"refresh_list"},
    "STALE":{"retryable":false,"next_action":"reload_then_reconfirm"},
    "UNAUTHORIZED":{"retryable":false,"next_action":"reauthenticate_or_stop"},
    "FOREIGN_GUILD":{"retryable":false,"next_action":"select_managed_guild"},
    "UNAVAILABLE":{"retryable":true,"next_action":"retry_read"},
    "COMMIT_UNKNOWN":{"retryable":false,"next_action":"reconcile_same_intent"},
    "SAVED_UNVERIFIED":{"retryable":false,"next_action":"inspect_publication"},
    "SAVED_PUBLICATION_FAILED":{"retryable":false,"next_action":"reconcile_full_registry"}
  },
  "fixtures":[
    {"id":"CR-F01","input":"all saved fields plus disabled row; metadata omitted","expect":"inventory read preserves fields; no invented provenance or revision","gate":"W1"},
    {"id":"CR-F02","input":"replace template; omitted description/trigger; disabled row","expect":"baseline resets metadata and enables; future preserving edit blocked until atomic contract","gate":"W2"},
    {"id":"CR-F03","input":"null trigger; empty fields; UTF-16 boundaries; protected names; duplicate trigger","expect":"explicit clear distinct from omission; exact validators and capacity preserved","gate":"W2"},
    {"id":"CR-F04","input":"many maximal names/triggers; page 0/duplicate/noninteger/out-of-range; empty page 1","expect":"whole rows once in name order; bounded pages; only successful zero read is EMPTY","gate":"W1"},
    {"id":"CR-F05","input":"four known tokens; unknown {args}/{guild}; literal {User}; mentions; HTML; expansion overflow","expect":"known single-pass render; unknown lowercase tokens refuse validation; other brace text literal; escaped private saved preview; zero execution effects","gate":"W1"},
    {"id":"CR-F06","input":"revoked/nonadmin session; wrong guild; bad MAC; replay nonce; unmapped scope; AI admin","expect":"no data disclosure or command mutation; trusted-guild check before signing","gate":"W1"},
    {"id":"CR-F07","input":"two edits/toggle/import/slash writers; delete/recreate under stale revision","expect":"atomic stale refusal preserves draft and new incarnation; current APIs cannot claim CAS","gate":"W2"},
    {"id":"CR-F08","input":"same import key/body after later edit; changed body; ambiguous terminal","expect":"original summary once; mismatch refuses; unknown reconciles without forced retry","gate":"W2"},
    {"id":"CR-F09","input":"save commits; Discord PUT fails or receipt times out; repeat no-op toggle","expect":"saved remains; publication failed/unknown; no second mutation or false sync claim","gate":"W3"},
    {"id":"CR-F10","input":"one-page subset; unknown rows; empty desired after failed DB read; builtin/custom union","expect":"zero partial/empty replacement; all unrelated commands preserved on reconciliation","gate":"W3"},
    {"id":"CR-F11","input":"same local dry-run twice; unknown mode or wrong flag combination","expect":"deterministic review; zero network/sign/nonce/claim/SQL/audit/Discord effects; invalid mode refuses","gate":"W1"},
    {"id":"CR-F12","input":"disable/enable no-op and delete missing; reserved trigger on enable","expect":"preserve content/provenance; no-op has no write/audit/publish; enable validates; delete confirms","gate":"W2"}
  ]
}
```

On unavailable read retain last observation with timestamp and **stale** label;
disable mutations until a fresh supported read. Do not treat `retryable` as a
background write queue. `STALE` is a future adapter reason, not today's wire CAS.
Local dry-run returns PREVIEW_ONLY; W1 must define request bounds, error mapping,
list revision/pagination and scope checks; W2 must define per-intent durable
idempotency plus exact-body replay for new mutations before they can be exposed.
No new endpoint is authorized merely by giving a candidate name above.

Run from repository root (Python stdlib only):

```sh
python3 -m doctest -v docs/dashboard-custom-responses-spec.md
python3 scripts/check_docs_index.py --json
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_docs_path_conformance.py' -v
```

```pycon
>>> import json, pathlib, re
>>> doc = pathlib.Path("docs/dashboard-custom-responses-spec.md").read_text()
>>> fence = "`" * 3
>>> contract = json.loads(re.search(fence + r"json\n(.*?)\n" + fence, doc, re.S).group(1))
>>> assert contract["schema"] == "dashboard.custom_responses.v1"
>>> assert re.fullmatch(r"[0-9a-f]{40}", contract["source_sha"])
>>> domain = pathlib.Path("crates/core/src/custom_commands.rs").read_text()
>>> known = re.findall(r'"([a-z]+)"', domain.split("pub const TEMPLATE_PLACEHOLDERS", 1)[1].split(" = ", 1)[1].split(";", 1)[0])
>>> assert contract["placeholders"] == known == ["user", "username", "server", "channel"]
>>> assert contract["definition_fields"] == ["name", "description", "template", "text_trigger", "enabled"]
>>> assert all(int(re.search(r"pub const " + symbol + r": usize = (\d+)", domain).group(1)) == contract["bounds_utf16"][field] for field, symbol in [("description", "MAX_DESCRIPTION_CHARS"), ("template", "MAX_TEMPLATE_CHARS"), ("rendered", "MAX_RENDERED_CHARS")])
>>> assert contract["edit_omission"] == "preserve_requires_atomic_contract"
>>> adapter = contract["adapter"]
>>> assert set(adapter["envelope"]) == {"schema", "source_sha", "guild_id", "definition_id", "revision", "request_id", "reason", "retryable", "next_action"}
>>> assert adapter["definition_id"] == ["guild_id", "name"] and "committed_revision" in adapter["mutation_receipt"] and "publication_state" in adapter["mutation_receipt"]
>>> assert re.fullmatch(adapter["idempotency"]["key_grammar"], "fixture:intent-01") and not re.fullmatch(adapter["idempotency"]["key_grammar"], "bad key")
>>> assert adapter["idempotency"]["retry"] == "same_key_exact_body_fresh_nonce" and adapter["idempotency"]["unknown_commit"] == "reconcile_same_intent"
>>> assert adapter["dry_run"] == {"mode": "local_only", "reason": "PREVIEW_ONLY", "effects": []}
>>> caps = contract["capabilities"]; fixtures = contract["fixtures"]
>>> assert len(caps) == len({c["id"] for c in caps}) == 9
>>> assert len(fixtures) == len({f["id"] for f in fixtures}) == 12
>>> src = pathlib.Path("crates/core/src/internal_actions.rs").read_text()
>>> verbs = set(re.findall(r'"([a-z_.]+)"', src.split("pub const IMPLEMENTED_ACTIONS", 1)[1].split("];", 1)[0]))
>>> assert {c["candidate"] for c in caps if c["state"] == "source_shipped"} == {"automations.import", "automations.export"}
>>> assert all((c["candidate"] in verbs) == (c["state"] == "source_shipped") for c in caps)
>>> assert all(c["gate"] and set(c["gate"]) <= {"W1", "W2", "W3"} for c in caps if c["state"] == "missing")
>>> assert all(f["input"] and f["expect"] and f["gate"] in {"W1", "W2", "W3"} for f in fixtures)
>>> assert all(isinstance(r["retryable"], bool) and r["next_action"] for r in contract["reasons"].values())
>>> refs = re.findall(r'(crates/[a-zA-Z0-9_./-]+\.(?:rs|sql)):(\d+)(?:-(\d+))?', doc)
>>> assert refs and all(pathlib.Path(p).is_file() and 1 <= int(a) <= int(b or a) <= len(pathlib.Path(p).read_text().splitlines()) for p, a, b in refs)
>>> print(json.dumps({"schema": contract["schema"], "capabilities": len(caps), "fixtures": len(fixtures), "status": "pass"}, sort_keys=True))
{"capabilities": 9, "fixtures": 12, "schema": "dashboard.custom_responses.v1", "status": "pass"}

```

This runs document/schema/allowlist/path consistency, **not CR-F01–F12 runtime
acceptance**. The fixture rows are planned positive/negative oracles for the Test
Author, not executed tests or an implementation of these rules. No protected
acceptance files change. Future handoffs need an immutable branch/SHA and path/
hash manifest, behavior-specific RED on the named baseline and unchanged controls;
cache/compile/environment failure is not RED. Use mock Discord and disposable
test DB only, never staging/production DB or a live signed mutation.

## Exact-revision intent gate

| Gate | Evidence needed before build cards, not an implementation request |
| --- | --- |
| `W1` | Web-owned page/API/signer evidence, trusted session/guild/actor checks, bounded list/detail/saved-preview read contracts. Explicit scope/holder and data-exposure disposition for each new verb; no bearer-gateway or signing-v2 assumption. |
| `W2` | Accepted omission/default/null semantics, lifecycle-safe revision across every writer, atomic audits, per-intent idempotency and concurrency/ABA fixtures. Reuse services, not import-as-editor or client-side CAS. |
| `W3` | Complete-registry publication/readback and partial-failure recovery contract, preservation fixtures and truthful saved/observed states. No page-level partial bulk PUT or production activation. |

Bind CEO intent acceptance and recorded host-intent alignment to the exact task
spec revision, repository commit and this file's SHA-256. Request CISO disposition
for genuinely new signed verbs/scope/holder or data boundaries before build cards;
no self-granted read/write access. A pending request or elapsed review date is not
acceptance. Default **no-build** if alignment/evidence is missing. Future browser
and bot implementation require their own cards and protected acceptance handoffs.
This is a design gate, not a routine deployment-approval hop.

Review metric: nine capability states evidenced, twelve fixture oracles decided,
zero page operations mislabeled deployed or publication-confirmed. Review by
2026-11-01; no live experiment, spend, deployment or activation is authorized.
Unverified: web implementation/guards, deployed flags/keys/gates, actual Discord
registry, runtime availability and production behavior. ACT: publish this proposed
contract. WHY: avoid destructive metadata loss and false delivery claims.
UNDO: revert documentation; no operational state changed.
