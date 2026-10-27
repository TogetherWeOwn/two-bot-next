# Optional welcome image — Proposed

Status: **Proposed, design only**, 2026-10-27. Review **2026-11-05**.
**Recommend no build now** (option A: the text welcome plus `/welcome-preview`).
This page authorizes no code, setting, asset, dependency, migration, live
rendering or activation. Merge accepts documentation, not host intent or a
security boundary. Every number below is an engineering proposal, not a measured
or current limit.

## 1. Decision and evidence

Hypothesis: a new member who sees one branded image in the welcome channel feels
the server is alive sooner. MEE6, Dyno and Welcomer
advertise welcome cards (assignment evidence, not independently verified here);
vendor capability is not evidence of TWO demand.

| Option | Benefit / cost | Recommendation |
| --- | --- | --- |
| A: text welcome (mention, copy, picker) with private `/welcome-preview` | No new asset, dependency, PII surface or failure path; hosts already see the exact text | **Choose now** |
| B: same single message plus one default-off static branded template with the member's name | Warmer first impression; needs an asset, a renderer, an attachment send and a name-in-image privacy disposition | Conditional design only |
| C: avatar, animation, per-member art, hotlinked or third-party art, an external image service, DM or goodbye images | Wider PII, decode, licensing or spend surface | Excluded |

On 2026-11-05 retain B only if (1) a no-spend render path demonstrably fits the
container (§3), (2) an authorized community host states an axis on which the image
beats text (for example "new members look welcomed"), and (3) the join path keeps
its text fallback. **Defer B** if rendering needs a new service, or the fallback
cannot be kept. This kills the hypothesis, not the onboarding project. Do not
create build or acceptance-test cards automatically. No live demand experiment
is authorized; success is an evidenced need and an accepted safe contract, not
image count.

## 2. Existing seams, not claimed guarantees

Source inspected at `7046b8eda953a20d129291accd23f6b24a5a0ad0`; not deployment
verification. Start with the [operator guide](onboarding-operator-guide.md),
[domain port](onboarding-port.md) and [privacy](privacy.md).

- The welcome is a durable queued job (`OnboardingJob::Welcome`,
  `crates/bot/src/onboarding.rs:54-62`), not work inside the gateway handler. It
  re-reads the member (`:697`), takes the prompt guard (`:721`) and sends one
  message (`:730`). The guard serializes sends but is not exactly-once across a
  crash after Discord accepts (operator guide, "Prompt decisions").
- The only send seam is `post_channel_message`
  (`crates/discord/src/executor.rs:3266-3306`): content, components, mentions. It
  has **no attachment parameter**, and this page does not claim the channel-create
  transport already carries a multipart upload. Existing attachment use is
  interaction replies (`crates/bot/src/voice_rooms.rs:11926-11929`). Verifying and
  adding an upload through the same paced single-attempt lane is a build prerequisite.
- The re-read member body already carries `user` (`fresh_recipient`, `:121`) but
  the job keeps only IDs and roles. B would read the display name from that same
  response, never from the queued payload, so no new API call or stored text.
- Anchor mode is a deliberately silent one-message welcome with no extras
  (operator guide). B excludes anchor mode. Dry run suppresses legacy/anchor
  welcomes, so B never renders there; session mode posts in dry run and B follows it.
- `Cargo.lock` contains `flate2` and `crc32fast` only: no PNG encoder, image,
  font or rasterizer crate. Any in-Rust render adds crates that must pass
  `deny.toml`'s license allow-list and supply-chain review, plus a bundled font.
- The runtime image carries only the release binary and CA data (`Dockerfile`
  header), so no system fonts or image tools, and targets the 256 MiB `lite` tier
  ([ADR 0001](adr/0001-discord-framework.md)). No render measurement exists.
- `welcome()` reads no per-member
  [opt-out](privacy.md#member-opt-out-per-user-per-guild) state; B adds that check.

## 3. Rendering contract (no new spend, no external service)

Decision: **in-process render from one bundled static template**, never a
network service, never fetched art. If the build spike cannot meet these bounds
inside the container without a new service, B is deferred (§1).

| Item | Proposal |
| --- | --- |
| Template | One repo-authored raster, 960×320 px, asset ID `welcome-card-v1`, decoded once into memory; image text slot reserved for one name line. |
| Name text | One line from one bundled font, asset ID `welcome-font-v1` (OFL or equivalent, license recorded). Only scalars the font covers are drawn; no fallback fonts, shaping or emoji. |
| Output | PNG, <=128 KiB target, **256 KiB hard cap**; larger is `RENDER_FAILED`. Fixed filename `welcome.png`. |
| Budget | Render runs off the async threads (blocking pool), **250 ms CPU**, **500 ms wall**, one permit (non-blocking try-acquire), working memory <=8 MiB, measured on staging before enabling. |
| Cache | Template decoded once. The member image is never cached or written to disk or the database; it exists only in the send request. |
| Rate | At most 6 images per guild per rolling minute; excess joins get the text welcome (`RATE_LIMITED`). |
| Order | Render after the fresh-member read and before the prompt guard, so the guard is never held across a render. |

**Fallback is the only failure mode.** Every render or asset problem yields the
ordinary text welcome in the same single message, with no extra delay beyond the
500 ms budget and no failed job. Disabled, unfenced, busy, over budget, unreadable
name, missing asset: all send exactly today's message. An attachment the transport
or Discord **refuses with a known no-message result** may be followed by the text
send once in the same attempt. An **uncertain** outcome (timeout, partial) must
not trigger a second message; retries of that job send text only. A build carries
that bit in the job attempt, not a new table, or revises this spec. A successful
send is one message: text, picker and image together; never a follow-up post.

## 4. Configuration, fencing and provenance

- One new admin-only setting, ID `onboarding.welcome_image`, values `off`
  (default, absent means off) and `static_name`. It follows the existing stored,
  live-refreshed settings path ([configuration](configuration.md),
  [admin registry](admin-registry.md)); no member command, public route or
  dashboard editor. Unknown values read as `off`.
- Fence: the configured guild only (`OnboardingConfig.guild_id`), legacy and
  session modes only. Another guild, anchor mode or dry-run legacy is `NOT_CONFIGURED`
  with the existing behavior. Changing the setting takes effect for the next
  join; it never re-renders or edits an earlier message.
- Provenance: the template and font are authored or licensed for this use,
  recorded in one in-repo asset manifest (asset ID, author, source, license,
  SHA-256), reviewed with the build PR. No hotlinked, scraped or third-party art
  without a separate license decision. A missing, unlisted or hash-mismatched
  asset is `ASSET_UNAVAILABLE`, and the render is skipped. This page ships no
  binary asset.
- Alt text is the fixed string "Welcome to Together We Own"; it never contains
  the member's name. The text message keeps today's mention policy
  (exactly the new member; no roles or everyone).

## 5. Name, avatar and privacy disposition

- The image draws one name line from the live member body: guild nickname, else
  global display name, else username. It is a **member-supplied, untrusted**
  string. Draw only if it is 1..24 Unicode scalars, trimmed, NFC, free of
  controls, bidi overrides, zero-width or private-use scalars, and every scalar
  is covered by `welcome-font-v1`. Anything else is `NAME_NOT_RENDERABLE`: send
  the text welcome (the mention already names the member). Never truncate,
  transliterate or draw replacement glyphs. Mentions and Markdown are inert in a
  bitmap; the name is never placed in alt text, filename, logs or metrics.
- **Avatar is excluded.** Reading it means fetching and decoding an
  attacker-controlled image, a new network and parser surface, and re-publishing
  a member image. Adding it requires its own proposal, CISO disposition and an
  explicit privacy decision; it is never a default or a hidden option of B.
- Opt-out: a member with a stored per-guild opt-out gets the text welcome
  (`PRIVACY_OPTED_OUT`). A failed lookup is `UNAVAILABLE`, which also sends text.
- B stores **nothing new**: no image, name, hash, cache or receipt. The existing
  prompt marker is unchanged. The sent image is Discord-hosted message content,
  like the text mention already is; bot-side erasure does not delete Discord
  messages or attachments ([privacy](privacy.md#retention-and-explicit-exceptions)).
  Disclose that a name rendered into an image persists with the message until
  Discord or a moderator removes it, and cannot be redacted by editing text.
  Staff may delete the message; the bot does not re-edit or backfill old welcomes.
- **CISO disposition: not accepted or waived by this proposal.** Before a build,
  obtain exact-revision disposition for name-in-image persistence, the new
  crates and font, the upload path and the opt-out check. No new permission,
  secret, external provider or spend is proposed.

## 6. Agent handoff: illustrative JSON, not an available API

One repo source: this document. No executable flag or mutation route is added.
Envelopes <=4 KiB; reasons are the finite set below, each with fixed outcome and
next action. Unknown reasons are a contract failure, never a new success.

```json
{"version":1,"asset_id":"welcome-card-v1","config_id":"onboarding.welcome_image","outcome":"text_fallback","reason":"NAME_NOT_RENDERABLE","retryable":false,"next_action":"none","effects":{"messages":1,"images":0,"stored_rows":0}}
```

| reason | outcome / retryable / next_action |
| --- | --- |
| `IMAGE_SENT` | image_sent / false / none |
| `NOT_CONFIGURED`, `PRIVACY_OPTED_OUT` | text_only / false / none |
| `NAME_NOT_RENDERABLE`, `RENDER_BUSY`, `RATE_LIMITED` | text_fallback / false / none |
| `RENDER_FAILED`, `RENDER_TIMEOUT`, `ASSET_UNAVAILABLE` | text_fallback / false / host_review_assets |
| `ATTACHMENT_REFUSED` | text_fallback / false / host_review_permissions |
| `UNAVAILABLE` | text_fallback / true / none |
| `SEND_UNCERTAIN` | uncertain / false / inspect_welcome_channel |

## 7. Finite future acceptance matrix

Design oracles only, **not executed behavior tests**. Future independent tests
use synthetic identities, fixed clocks, recording Discord doubles and disposable
databases, never staging or production. Every row asserts: exactly one message,
text body unchanged, and stable reason and next_action.

| ID | Scenario | Required oracle |
| --- | --- | --- |
| W01 | Setting `off`, absent or unknown value | Today's text message; no render, no permit taken |
| W02 | `static_name`, legacy and session, normal name | One message: text, picker and `welcome.png`; `IMAGE_SENT` |
| W03 | Other guild, anchor mode, dry-run legacy | `NOT_CONFIGURED`; behavior unchanged |
| W04 | Name with mentions, Markdown, bidi, controls, zero-width, emoji | `NAME_NOT_RENDERABLE`; text welcome; name not in any log or label |
| W05 | Name of 0, 24 and 25 scalars; uncovered script | 24 renders; 0, 25 and uncovered fall back; no truncation |
| W06 | Template, font or manifest missing, unlisted or hash mismatch | `ASSET_UNAVAILABLE`; text welcome |
| W07 | Render exceeds 500 ms or 256 KiB, or panics | Text welcome; job succeeds; no second message |
| W08 | Join burst: permit busy, 7th image in a minute | `RENDER_BUSY` or `RATE_LIMITED`; every join still welcomed once |
| W09 | Attachment refused with a known no-message result | One text send in the same attempt; `ATTACHMENT_REFUSED` |
| W10 | Timeout or partial result after the image send | `SEND_UNCERTAIN`; no second message; retry sends text only |
| W11 | Opted-out member, failed opt-out lookup | Text welcome; no name read into any render |
| W12 | Rejoin after goodbye; duplicate gateway events | One greeting per join; prompt guard unchanged |
| W13 | Erasure, guild removal, restore | No new rows exist; nothing to purge; disclosed Discord-side persistence |
| W14 | Setting flipped mid-burst | Next join obeys the new value; no edit of sent messages |

## 8. Offline documentation check and exact-spec gate

Run from the repo root **after committing**; Python standard library, no network,
Discord or database. It checks the committed document's link inventory, finite
matrix and reason table, not behavior, demand, rendering or security acceptance.

```sh
python3 scripts/check_docs_index.py --json
python3 - <<'PY'
from pathlib import Path
import hashlib, json, re, subprocess, sys
p = Path('docs/welcome-image-spec.md')
try:
    commit = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    blob = subprocess.check_output(['git', 'show', f'{commit}:{p.as_posix()}'])
    if blob != p.read_bytes():
        raise ValueError('DIRTY_SPEC')
    text = blob.decode('utf-8')
    cases = re.findall(r'^\| (W\d{2}) \|', text, re.M)
    if cases != [f'W{i:02}' for i in range(1, 15)]:
        raise ValueError('MATRIX_INCOMPLETE')
    table = text.split('| reason | outcome', 1)[1].split('\n\n', 1)[0]
    reasons = re.findall(r'`([A-Z_]+)`', table.split('\n', 2)[2])
    if len(reasons) != 12 or len(set(reasons)) != 12:
        raise ValueError('REASONS_INCOMPLETE')
    sample = json.loads(re.search(r'```json\n(.+?)\n```', text, re.S).group(1))
    if sample['reason'] not in reasons:
        raise ValueError('SAMPLE_REASON_UNKNOWN')
    root = Path.cwd().resolve()
    links = re.findall(r'\]\(([^)\s]+)\)', text)
    local = [x for x in links if '://' not in x and not x.startswith('#')]
    for link in local:
        target = (p.parent / link.split('#')[0]).resolve()
        if not target.is_relative_to(root) or not target.is_file():
            raise ValueError('MISSING_PATH')
    print(json.dumps({'version': 1, 'outcome': 'documentation_checked',
        'reason': 'LINKS_MATRIX_REASONS_ONLY', 'next_action': 'exact_spec_disposition',
        'spec_commit': commit, 'spec_sha256': hashlib.sha256(blob).hexdigest(),
        'matrix_rows': len(cases), 'reasons': len(reasons),
        'relative_links_checked': len(local), 'runtime_verified': False}, sort_keys=True))
except (OSError, ValueError, KeyError, IndexError, AttributeError, subprocess.CalledProcessError):
    print(json.dumps({'version': 1, 'outcome': 'failed',
        'reason': 'DOC_CONTRACT_FAILED', 'next_action': 'fix_paths_matrix_or_reasons'}))
    sys.exit(1)
PY
```

Before **any build or test card**, route the exact spec commit and SHA-256 to the
CEO for host-intent alignment: retain A, or name the authorized unmet host use
case and accept B's scope, name-in-image persistence and text-fallback behavior.
Separately obtain the CISO disposition in §5; none is recorded here. A revision
invalidates prior exact-spec acceptance. Missing evidence, a render path that does
not fit the container, or rejected boundaries on 2026-11-05 means **defer B**,
not passive approval. These are design boundaries, not a routine deployment
approval hop. No implementation or test tasks, assets, dependencies or live
activation follow this PR.
