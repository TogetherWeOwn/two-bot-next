# Quiet feed title filtering — proposed contract

Status: **proposed, design only**. Written 2026-10-25; review **2026-11-01**.
**Choose no build now**: prefer an already narrow source, or remove an unwanted
relay through existing controls after resolving pending deliveries. No source
change, sample fetch, filter write, command, runtime, migration or activation is
authorized here. Merge records a proposal, not host intent or security acceptance.

## 1. Decision and evidence

Hypothesis: staff could reduce irrelevant game/news posts with title keywords.
No TWO demand or representative-title quality measurement is established.
[MonitoRSS](https://monitorss.xyz/) advertises filtering by keyword, author or
topic (marketing page read 2026-10-25); this is a comparison, not demand evidence.
This design fetched no feed, article, guild data or live sample.

| Option | Value / cost | Decision |
| --- | --- | --- |
| A. No build; select a narrower source | No extra state, controls or bot volume; source quality may remain insufficient | **Selected now** |
| B. Bounded literal include/exclude on the existing parsed title | Predictable for descriptive titles, not a topic classifier; adds durable skip decisions and privacy/recovery obligations | Conditional contract below, **not authorized for build** |
| C. Regex, author/body scraping, classifier or new provider | More input, cost and failure modes without demonstrated need | Excluded |

A does not authorize removing/re-adding a relay to reset history, changing its
stored source in place, or fetching a source to investigate. At the review date,
retain A unless an actual host use case and separately authorized, bounded title
evidence justify B. If titles are missing, misleading, generic or cannot separate
the wanted items under the literal rules, choose A, not richer matching. A future
fixture evaluation should report both wanted items lost and unwanted items passed;
synthetic examples here prove neither usefulness nor delivery. Success is less
unwanted bot volume, not more bot engagement. No spend or live experiment follows.

## 2. Observed baseline and limits

Original inspection baseline: `c3af7333df80a766f5d4c229506a3bce53680828`.
Current-source links rechecked at `1441cbef76a1c8237a3a91a8944e2d25b66c4ce4`;
not deployment verification. See [feed operations](feed-supervisor.md) and
[domain/store contract](feeds-port.md). The model exposes a parsed title and URL,
not a filter policy. Existing parsing, URL safety, message bounds, 20 POST-attempt
budget and nonce recovery remain mandatory, before and after any future matcher.

- [Model](../crates/core/src/feeds.rs#L65-L85) has no title-filter setting or
  durable filtered outcome. The [parser](../crates/core/src/feeds.rs#L1200-L1215)
  substitutes `Untitled` for missing, empty, repeated or literal JS-trim-whitespace-only
  title fields, losing presence evidence. Numeric references such as `&#32;` stay
  present as literal text: [decoding](../crates/core/src/feeds.rs#L1484-L1494) does
  **not** convert them to whitespace. An actual title `Untitled` is indistinguishable
  through `FeedItem.title` alone.
  [Scalar extraction](../crates/core/src/feeds.rs#L1283-L1311) rejects repeated
  title fields and omits nested element text. B needs a separate presence/usability
  signal before fallback; this design does not implement it.
- [Poller](../crates/bot/src/feed_jobs.rs): recovery and new XML candidates are
  distinct; an in-memory relay scheduling cursor is **not an item checkpoint**.
- [Store](../crates/core/src/feeds_store.rs) and
  [schema](../crates/cutover/migrations/0180_feeds.sql): existing delivery claims
  arbitrate `(feed_id, item_key)`; delivered/pending evidence must not be reused
  as a skipped marker or deleted to apply a filter.
- [Erasure manifest](../crates/cutover/src/member_erasure_plan.json#L164-L171)
  deletes creator-owned feeds/deliveries directly. The
  [shared replay guards](../crates/cutover/src/member_erasure.rs#L140-L159)
  **do not cover feed pending rows**: member/guild erasure does not inherit the
  feed-remove refusal. This is an existing gap, not a claimed protection. B must
  reconcile this before adoption. See [privacy contract](privacy.md); titles,
  keywords and hashed item identities are not anonymous by default.
- Pause/resume was separate work at the original baseline;
  [PR #1720](https://github.com/TogetherWeOwn/two-bot-next/pull/1720) is now merged
  on main, preserving relay/delivery history without title filtering. Deployment
  was not verified here. Source preview, inspection, paging and dashboard designs
  do not implement or authorize matching.

## 3. Conditional B: exact matcher v1

These are **proposed rules**, not existing commands or Rust APIs. One policy per
saved relay; `null` means no filter. An object with both lists empty is invalid;
disabling uses `null`, not an inferred match-all keyword. Relays **never enrolled
in B** keep current delivery behavior with no policy and report `NO_FILTER`,
never `MATCHED`. Once enrolled, disabling matching preserves decision/capacity
fences; it does not return the relay to an untracked legacy path.

- Input is **only the existing parser's extracted title before Discord rendering
  and before its `Untitled` fallback**, with explicit presence/usability metadata.
  Missing, repeated/ambiguous or normalized-empty title fields are missing, never
  a match for `untitled`. A genuinely supplied `Untitled` may match. If metadata
  cannot distinguish these, B is refused; do not guess from the fallback string.
  Entity decoding remains exactly the existing parser's decoding. Do not fetch a
  URL, reconstruct child markup, strip HTML, interpret Markdown, decode another
  entity layer, match URL/body/author, or use rendered/escaped post text. Partial
  text exposed by the parser is not proof of the whole title.
- Normalize both title and each keyword: replace every maximal run of the fixed
  whitespace set below with one ASCII space; trim that space; map **ASCII A–Z
  only** to a–z. Other Unicode scalar values remain exact. No NFC/NFKC, locale,
  full Unicode case folding, accent stripping, stemming or zero-width removal.
  Thus `CAFÉ` does not match `café`, and `café` does not match `café`.
  This deliberate limitation must be visible in private preview.
- Whitespace set (Unicode White_Space, frozen here rather than library-dependent):
  U+0009–000D, U+0020, U+0085, U+00A0, U+1680, U+2000–200A, U+2028,
  U+2029, U+202F, U+205F, U+3000. U+200B and U+FEFF are **not** whitespace.
  Reject other C0/C1 control characters in policy input; in a filtered title
  classify them as `TITLE_UNUSABLE`. No-filter processing stays unchanged.
- A keyword is a nonempty literal **substring**, including punctuation/spaces;
  not a token boundary, regex, glob or boolean expression. `war` matches `award`;
  `.*` means the two literal characters. No comma/space splitting: arrays contain
  phrases. Limits apply to the entire title, not a truncated prefix.
- Bounds: <=8 include phrases, <=8 exclude phrases, <=16 total. Each raw phrase
  <=128 UTF-8 bytes and <=64 Unicode scalars; normalized phrase 1..64 scalars.
  Sum of raw phrase bytes <=1,024. Reject duplicate normalized phrases within or
  across lists (including include/exclude overlap), nonstrings, unknown policy
  fields and duplicate JSON object keys at **any** depth. No silent deduplication.
  Title >4,096 UTF-8 bytes or >1,024 scalars is `TITLE_UNUSABLE` when filtered;
  the matcher does not change the upstream parser/body caps.
- Precedence: existing replay state first (§4), then no filter, then unusable or
  missing/empty/whitespace-only title (`TITLE_UNUSABLE` or `TITLE_MISSING`, skip),
  then **any exclusion wins** (`EXCLUDE_MATCH`), then nonempty include list needs
  at least one include (`INCLUDE_MISS` otherwise). A present usable title with
  empty include list passes as `EXCLUDE_ONLY_PASS`; include match is `INCLUDE_MATCH`.
  Missing content never becomes a positive match, even with exclude-only policy.

Private synthetic explanations (not public channel posts or fetched samples):

| Policy include / exclude | Parsed title | Decision and explanation |
| --- | --- | --- |
| `rocket league` / `esports` | `ROCKET  LEAGUE patch` | `INCLUDE_MATCH`; ASCII case and repeated/NBSP spaces normalize |
| `rocket league` / `esports` | `Rocket League esports` | `EXCLUDE_MATCH`; exclusion wins even though include matches |
| `patch` / none | `Weekly news` | `INCLUDE_MISS`; no literal include phrase |
| none / `rumor` | missing or `  ` | `TITLE_MISSING`; cannot evaluate an absent title |
| none / `rumor` | `Patch notes` | `EXCLUDE_ONLY_PASS`; not proof of topic relevance |
| `café` / none | `CAFÉ` or `café` | `INCLUDE_MISS`; non-ASCII case/composition are exact |
| `patch` / none | `<b>patch</b> @everyone` | `INCLUDE_MATCH`; markup is literal text, not trusted/rendered HTML or permission to ping |
| no policy | missing title | `NO_FILTER`; unchanged legacy path, not a title match |

The private explanation may show a phrase's **index** to its authorized author,
not leak it into logs. Every output uses closed mentions and escaped untrusted
text; no example opens a URL or causes a message send.

## 4. Durable decisions: a scheduling cursor cannot prevent replay

Proposed future oracle: `feed_title_v1_decision_matrix` plus
`feed_title_v1_replay_model` using synthetic inputs, fake clock and independent
disposable-DB connections. These names are **planned**, not installed tests.

Keep the existing stable item-key/nonce derivation. No revision, title, policy
hash, poll time or destination may be added to the dedup key. Repeated keys with
changed titles/URLs do not become new items. Evaluate at most the existing 200
parsed candidates per relay/pass, in current candidate order. Source order and
publication timestamps are untrusted; no maximum timestamp, last array offset
or oldest-seen watermark alone may mark unseen keys handled.

A future **single atomic decision namespace**, scoped to guild/relay/item, must
arbitrate new filtered decisions against existing pending/delivered claims:

| Existing state | Required behavior |
| --- | --- |
| Delivered | Terminal; return `ALREADY_DELIVERED`, no rematch, no POST |
| Filtered | Terminal; return `ALREADY_FILTERED` with original reason/revision, no rematch or POST |
| Pending/uncertain | Existing recovery only; never classify filtered or release on policy change |
| Absent | Under relay fence compare enabled state and policy revision, then commit filtered **or** admit delivery, never both |

A rejected matcher result commits a terminal skip with key, fixed reason,
matcher version and policy revision **before** considering that item handled.
No title, URL, keyword or body copy belongs in a skip receipt. Persistence failure
is `CHECKPOINT_FAILED`, not filtered/sent; stop fresh admission for this relay.
Crash before commit -> unhandled, may evaluate again under the then-current
policy. Crash after commit or unknown commit -> read the authoritative decision
first; no send on unknown outcome. Filtered counts increment only for newly
committed skip decisions, not every poll of a rejected key.

Every send admission pins immutable content, canonical URL, nonce, destination,
matcher/policy revision and content digest before any POST. Policy/enable/remove
writes and admissions share a bounded relay-scoped fence, never a lock across
HTTP. Existing legacy pending rows without such a snapshot stay on their current
recovery-only path; never reconstruct their content from current XML. Confirmed
nonce history may settle them; a bounded miss is not proof of no send. Unknown
sends keep their original fences. A filter change cannot cancel, redirect, release
or re-admit pending work, nor rewrite a delivered row. Current access/feature/halt
checks may park an admitted effect, **not** silently broaden its authority.

Filter edits/disable increment an opaque monotonic relay policy revision, with
CAS on the expected revision; no-op edits return the same revision. Only items
without a committed decision see the new revision. Disabling a filter is not
resuming a paused relay and does not erase skips. An item never observed during
a pause is undecided and may be considered on resume; disclose that backlog
limit instead of promising no historical posts at all. Skips do not consume the
20 POST-attempt budget: filter rejections commit before that counter increments.
Pausing preserves all decisions; resuming never rematches terminal ones. Unseen
pause-window candidates still obey the normal 20-attempt/pass cap, not an unlimited
catch-up. At decision-capacity exhaustion park new admissions, including after
filter disable; do not evict guards. Reply `CAPACITY_REACHED`: "Filter off; relay
still parked at its decision limit. History was preserved; nothing was replayed."
Disable is not a capacity reset, and recovery of existing pending rows stays separate.

## 5. Retention, erasure and storage STOP

**No storage recommendation or retention exception is accepted here.** Arbitrary
unordered feeds can repeat old keys forever. A finite key TTL alone would replay
skipped items after expiry, while indefinite retention would be a new privacy
boundary. Timestamp checkpoints, hash-only identity and bounded history misses
do not resolve that tradeoff. B stays unbuildable until the following exact
storage/erasure/recovery contract has independent disposition.

| Proposed data | Bound / deletion and unresolved boundary |
| --- | --- |
| Preview/title examples | In-memory request only, <=10 supplied synthetic titles, no audit/cache/export; discard at reply completion |
| Policy | One active policy per relay, <=25 relays/guild, bounds in §3; obsolete keyword text deleted at replacement, not copied into audit/history |
| New decision records | Hard <=10,000/relay (<=250,000/guild); capacity parks admission, never recycles keys; fixed codes/revisions only |
| Private count/detail view | <=25 rows/page; count aggregates over <=30 daily buckets, then delete buckets; aggregate expiry never deletes replay guards |
| Pending snapshots/guards | No automatic TTL/release; preserve all existing uncertainty; new snapshot retention while unresolved needs explicit CISO disposition |
| Settled filtered keys | No safe age-only TTL established. Need approved finite relay lifetime/terminal retirement or an explicitly justified retention exception **before** recommending schema |

Future creator/member erasure, guild purge and relay removal must share a
reviewed pending-recovery refusal under guild-scoped maintenance, then delete
policy, skips and related snapshots with the relay once safe. Feed removal refuses
deletion while deliveries are pending; an enabled-to-paused transition also
refuses pending deliveries. **Member/guild erasure still lacks the feed-pending
guard**; do not claim it is safe for new filter state. No subject hash, hidden
archive or copied keyword survives as a
substitute. The schema manifest, grants and receipts require explicit future
coverage. Relay/item IDs and hashes can be linkable; do not label them anonymous.
Deletion does not recall Discord posts. Never silently clear an unresolved send
to satisfy erasure or report it erased. If finite retention cannot coexist with
no replay, keep A. No implementation/test card until that choice is accepted.

Backups/restores must preserve the entire committed decision namespace and pinned
pending snapshots together, plus authorized erasure handling. Older backups
cannot prove absence of skips or sends: keep affected relays disabled until an
authorized reconciliation/retirement establishes safety. Old binaries that ignore
filtered decisions/pins must not run against B-enabled relays. Rollback parks B
relays; it does not drop skip state or deploy an old sender blindly. Current
backup snapshot retention is not a new day-based TTL for this feature.

## 6. Authority, quiet UI and agent interface

Proposed controls are guild-only, private, on one saved relay ID. Preserve current
configured-guild, application, feature/activation and human-actor fences. Require
current guild membership and ManageGuild; require requester visibility of the
saved destination/source configuration and bot permissions needed by the relay.
Unknown permissions refuse, not fallback. No destination override, new channel,
role mention, per-member preference, external source fetch or credential input.
Missing/foreign/unauthorized IDs share `NOT_AVAILABLE` without source, title,
counts, revision or destination leakage. Reauthorize every inspect/preview/apply;
agent-supplied actor IDs or components are not authority.

Private preview uses only **caller-supplied synthetic** titles, <=10, same title
bounds as §3; absent/null title is a missing-title fixture. Example IDs are unique
1..32 ASCII alphanumeric/`-`/`_` strings. Return one example per page in input order
with a 1-based page/count footer; no dropped examples. Echo at most 160 UTF-16 units
of escaped display title (explicitly marked excerpt), code and matched phrase index,
not full policy text. Inspect packs whole status rows, <=25/page and <=2,000 UTF-16
units/reply. Fixed IDs/codes/footer are never truncated; if even one fixed row
cannot fit, refuse `INVALID_INPUT`. Matcher input is never truncated. Show relay/revision, limitations, match/skip reason and
"Example only — no fetch, saved filter, checkpoint, queue, audit or send."
An example pass means eligible for normal admission, **not delivered**. Private
inspection distinguishes filtered, delivered (Discord accepted, not read), failed
(known refusal), pending/uncertain and fetch/parse failure. Do not turn a clean
fetch, zero matches, or an unknown checkpoint into a health/success verdict.

Illustrative **future JSON**, not an available endpoint:

```json
{"version":1,"operation":"preview","guild_id":"456","relay_id":"feed-example","expected_revision":"7","policy":{"include":["patch"],"exclude":["rumor"]},"examples":[{"id":"example-1","title":"PATCH notes"}],"dry_run":true}
```

```json
{"version":1,"operation":"preview","relay_id":"feed-example","policy_revision":"7","matcher_version":"title-literal-v1","outcome":"preview","code":"INCLUDE_MATCH","example_id":"example-1","retryable":false,"next_action":"confirm_exact_policy","effects":{"fetches":0,"writes":0,"posts":0}}
```

Every JSON path needs a duplicate-aware decoder **before** ordinary typed/map
materialization (last-key-wins decoding is forbidden), with <=64 KiB UTF-8 envelope
and <=8 nesting levels. Unknown versions/fields, duplicate keys, invalid scalar
types or missing required IDs/mode fail `INVALID_INPUT` / `correct_request`.
Preview and `dry_run=true` never write, even an
idempotency receipt or audit. Future apply requires `operation=apply`,
`dry_run=false`, expected revision, bounded request ID (1..64 ASCII alphanumeric,
`-`/`_`), and explicit authorized staff confirmation of that exact policy/relay.
Fingerprint canonical validated policy plus guild/relay/expected revision: same
idempotency key+same fingerprint returns original result; changed input conflicts.
Different keys cannot evade CAS or replay items. Reauthorize replay reads too;
idempotency metadata is **also a storage STOP**: a policy fingerprint can retain
keyword-derived/linkable data and cannot silently survive replacement/erasure as
an exception to §5. An accepted design must resolve receipt lifetime versus exact
same-key conflict/replay guarantees before build; neither hash-only storage nor
an undocumented short expiry is accepted here. Agents may preview; they cannot
impersonate host intent.

| Stable code | retryable / next_action |
| --- | --- |
| `NO_FILTER`, `INCLUDE_MATCH`, `EXCLUDE_ONLY_PASS` | false / `confirm_exact_policy` in preview; no delivery claim |
| `TITLE_MISSING`, `TITLE_UNUSABLE`, `EXCLUDE_MATCH`, `INCLUDE_MISS` | false / `revise_policy_or_source` in preview; `none` for committed skip |
| `ALREADY_FILTERED`, `ALREADY_DELIVERED` | false / `none` |
| `INVALID_INPUT`, `NOT_AVAILABLE` | false / `correct_request` or `none`, respectively |
| `STALE_REVISION`, `CONFLICT` | false / `preview_current_revision` or `inspect_original_request`, respectively |
| `CHECKPOINT_FAILED`, `UNAVAILABLE` | true / `inspect_then_retry`; never claim filtered or sent |
| `DELIVERY_FAILED`, `DELIVERY_UNCERTAIN` | false / `inspect_delivery` or `preserve_recovery_fence`, respectively |
| `CAPACITY_REACHED`, `RESTORE_UNCERTAIN` | false / `keep_relay_parked` |

All authorized responses carry version, relay ID, matcher/policy revision,
outcome/code, retryable, next_action and explicit effect counts. Denials redact
resource identifiers. Future metrics use only fixed reason/outcome enums: no
keyword, title, URL, host, member, guild or relay metric label. Counts are bounded
saturating integers with overflow disclosed, not arbitrary user labels. Logs never
include matched phrases or raw errors; private status cannot conflate filter skips
with transport/DB failures.

## 7. Future deterministic acceptance matrix

**Planned assertions**, not executed matcher/ledger tests. Immutable acceptance
tests must be supplied independently before a future build. Match fixtures use
exact Unicode scalars and byte lengths; ledger fixtures use controlled clocks,
synthetic guilds, independent DB connections and a recording Discord double.
Every row asserts code, next_action, decision state and exact effect counts.

| Case | Required oracle |
| --- | --- |
| F01 include / exclude | OR within each list; exclude wins; substring/punctuation/phrase semantics |
| F02 ASCII / Unicode | ASCII case-insensitive; non-ASCII exact; NFC/decomposed, emoji and zero-width cases |
| F03 whitespace | Every frozen whitespace scalar, repeated/leading/trailing runs; other controls unusable |
| F04 empty / missing | null, absent, empty, repeated fields/spaces cannot match; fallback Untitled differs from real Untitled; absent policy preserves current behavior |
| F05 misleading markup | No HTML/Markdown reconstruction or double entity decoding; parser partial title disclosed; mentions closed |
| F06 bounds | Exact and one-over list/scalar/byte/total/title limits; normalized duplicates; empty object rejected |
| F07 JSON | Duplicate keys at all depths, unknown version/fields, wrong types and UTF-8 errors refuse with zero effects |
| F08 duplicate / reorder | Changed title/URL for same item key and reordered/undated items never replay terminal decisions |
| F09 revision / disable | Skip at r7 then edit/disable at r8 stays skipped; undecided item sees r8; no-op revision unchanged |
| F10 pause / resume | Preserve terminal/pending state; never-seen items remain undecided; no fabricated checkpoint for a missed poll |
| F11 crash checkpoint | Before commit undecided, so edit before retry uses new revision; after/unknown commit read authoritative state; failed commit never counted filtered |
| F12 pending / crash send | Pinned old content/destination/policy survives edit; unknown send and bounded history miss remain pending; no release/resend |
| F13 concurrency / remove | Cross-worker skip-vs-send and edit/admit/remove serialize; same key wins once; no lock held across HTTP |
| F14 retention / capacity | Counter expiry not guard expiry; disable at capacity reports parked, no new POST; erasure refuses unresolved guards; receipt-lifetime disposition required |
| F15 restore / rollback | Missing/incompatible ledger disables affected sender; complete snapshot preserves skips and uncertainty |
| F16 authority / privacy | Cross-guild and denied same response; revoked membership/access, stale preview, idempotent replay reauthorization |
| F17 preview / counts | No fetch/write/audit/queue/send; bounded private output; only committed new skips counted, no keyword metric labels |

## 8. Offline documentation contract check and decision gate

Run from the repo root (Python standard library; no network, DB or runtime):

```sh
python3 scripts/check_docs_index.py --json
python3 - <<'PY'
from pathlib import Path
import hashlib, json, re, subprocess, sys
root = Path.cwd().resolve()
spec = root / "docs/feed-title-filter-spec.md"
try:
    protected = ["AGENTS.md", "SECURITY.md", "CONSTRAINTS.md",
                 "crates/core/src/feeds.rs", "crates/core/src/feeds_store.rs",
                 "crates/bot/src/feed_jobs.rs",
                 "crates/cutover/migrations/0180_feeds.sql",
                 "crates/cutover/src/member_erasure_plan.json"]
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    tracked = ["docs/feed-title-filter-spec.md", *protected]
    committed = {p: subprocess.check_output(["git", "show", f"{commit}:{p}"])
                 for p in tracked}
    if any((root / p).read_bytes() != committed[p] for p in tracked):
        raise ValueError("DIRTY_CHECKED_FILE")
    text = committed["docs/feed-title-filter-spec.md"].decode("utf-8")
    checked = 0
    for link in re.findall(r"\]\(([^)\s]+)\)", text):
        if "://" in link or link.startswith("#"):
            continue
        path = (spec.parent / link.split("#")[0]).resolve()
        if not path.is_relative_to(root) or not path.is_file():
            raise ValueError("MISSING_PATH")
        checked += 1
    cases = re.findall(r"^\| (F\d{2}) ", text, re.M)
    if cases != [f"F{i:02}" for i in range(1, 18)]:
        raise ValueError("MATRIX_INCOMPLETE")
    sha = lambda p: hashlib.sha256(committed[p]).hexdigest()
    print(json.dumps({"version": 1, "outcome": "documentation_checked",
        "code": "PATHS_AND_MATRIX_ONLY", "retryable": False,
        "next_action": "request_exact_revision_disposition",
        "spec_commit": commit,
        "spec_sha256": sha("docs/feed-title-filter-spec.md"), "relative_links_checked": checked,
        "matrix_rows": len(cases), "runtime_verified": False,
        "protected_sha256": {p: sha(p) for p in protected}}, sort_keys=True))
except (OSError, ValueError, subprocess.CalledProcessError):
    print(json.dumps({"version": 1, "outcome": "failed", "code": "DOC_CONTRACT_FAILED",
        "retryable": False, "next_action": "fix_paths_or_matrix"}))
    sys.exit(1)
PY
```

Run after committing: staged or unstaged changes to the spec/protected files
refuse a receipt. Hashes come from the captured commit's blobs, not mutable files.
This checks paths/inventory and fingerprints only; it does **not** execute the
planned oracles, validate source semantics, prove demand or approve storage.
Before push also run `git diff --check` and `context-pack --base origin/main`.
Keep CI and independent review on the exact PR head, then squash auto-merge.

Before **any implementation or test card**, record CEO/host intent alignment
against the exact spec commit + SHA-256: retain A, or present a real host use case
and explicitly accept B's literal/missing-title/backlog limitations. Separately
obtain CISO disposition for new policy/skip/pinned-content persistence, source and
destination visibility, erasure, finite retention versus replay, grants, restore
and rollback. Current status: **neither acceptance is recorded**; no host consent
is inferred from a design assignment or prior source-preview work. No routine
deployment approval hop is added. Unresolved/rejected boundaries at 2026-11-01
mean retain A, not passive consent. Any spec change invalidates prior exact-revision
acceptance; changed protected hashes require source reinspection. No live sample
fetch, filter write, runtime/schema work or production activation is part of this
slice.
