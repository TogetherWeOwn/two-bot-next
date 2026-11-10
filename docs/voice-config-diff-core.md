# V11a import diff preview core

`two_bot_core::voice_config_diff` is an original, pure implementation derived
only from [the voice-room specification](voice-rooms.md#v11-configuration-export-and-import).
It requires no `db` feature, Discord wire types, clock or external I/O.

## Domain contract

- `skip_unknown_channels(incoming, inventory)` removes every incoming entry
  that references a channel ID absent from the inventory: a creator whose room
  or permission-source channel is unknown, a template on an unknown channel,
  and logging to an unknown channel. It returns the remaining candidate and the
  sorted, unique unknown IDs. Only absence counts as unknown; cross-guild and
  wrong-kind channels stay for the codec to reject.
- `diff_configuration(current, incoming, inventory)` skips those entries
  first, then compares `current` with the remainder. It reports added, removed
  and changed entries per section (creators, templates, aliases, lists,
  logging), a settings change, and `skipped_unknown_channels`. Import replaces
  the configuration, so a skipped entry whose key exists in `current` shows as
  a removal.
- Changed entries carry the names of the differing fields in declaration
  order. Creators and templates are keyed by channel ID, aliases by `game`,
  lists by `name`. Logging and settings are singletons.
- Entry order in the input lists is insignificant. Every section is sorted by
  key, and the preview renders sections in a fixed order (creators, templates,
  aliases, lists, logging, settings, skipped channels).
- `apply_diff(current, diff)` returns the remaining incoming candidate with
  each section sorted by key. `version` and `guild_id` come from `current`.
- `render_preview(diff, max_lines)` builds a compact ephemeral-message body:
  a summary line, at most `max_lines` entry lines, then a `+N more` trailer
  for the lines left out. The body never exceeds `PREVIEW_CHAR_LIMIT` (2000),
  measured in UTF-16 code units, which is never less than the character
  count. An empty diff renders `No changes`.
- Snowflake-shaped keys are shown bare. Free-text keys are quoted, escaped and
  cut at 80 characters, and each line at 200, so uploaded text cannot break
  the layout. The text is not Markdown-escaped: send it with mentions
  disabled.

## Complete private review (`voice-config-review.v1`)

Admin `/import` keeps the compact summary and Confirm/Cancel buttons. Every
confirmable preview also attaches `voice-config-diff-guild-<guild-id>-v1.json`
in that same ephemeral response, with mentions disabled. The filename uses
only the trusted numeric guild ID, never the uploaded filename or text. Read
this attachment before confirming: the summary still clips long values and
shows `+N more` when necessary. Its attachment hint is reserved inside the
same 2000 UTF-16-unit message budget (`render_review_preview`).

`serialize_full_review(diff, guild_id, hash)` emits UTF-8 JSON with these fields:

- `schema`: exactly `voice-config-review.v1` (review format, not import codec).
- `guild_id`, `hash`: the same guild and current/candidate content hash as the
  pending preview and its buttons; `change_count` matches the summary.
- `proposed`: `creators`, `templates`, `aliases`, `lists`, `logging` and
  `settings`. Each keyed section has `added`, `removed` and `changed` arrays.
  Added and removed entries contain their complete values (the absent side is
  absent, not an empty string). Changed entries have `key`, complete `before`
  and `after` objects, and `fields` listing differing field names. A changed
  setting uses that same object shape; unchanged settings are `null`.
  Section + key + field identifies the changed configuration path. Creator
  and template keys are channel IDs; aliases use `game`, lists use `name`,
  and logging/settings are singletons. Nothing is clipped, including keys,
  list choices, old values, astral Unicode or escaped control characters.
- `skipped_unknown_channels`: sorted unique channel IDs excluded from the
  input by the admitted inventory, separate from applicable changes. As with
  the existing replace-style import, skipping an existing entry can also
  propose its removal; the `removed` section shows its complete old value.

The **inclusive serialized-byte cap is 32768 bytes (32 KiB)**, independent of
the 256 KiB upload cap. JSON escaping, metadata, skipped IDs and complete old
values all count. Serialization borrows the typed diff and streams into one
fixed-capacity buffer; it stops before an overflowing write. An incomplete
buffer is discarded, never attached or truncated. No raw uploaded values or
serialization error details are logged or audited.

If the full review exceeds the cap, the response states the byte limit and
asks the admin to reduce the proposed changes in the file and try again. Keep
unchanged entries: imports replace configuration, so splitting a file by
omitting them proposes removals. A serialization failure also refuses with a
retry correction. Both paths send no attachment or buttons and insert no
pending candidate. Re-preview refusal consumes the stale pending entry;
unrelated existing previews retain their usual binding and TTL. Empty and
skipped-only diffs keep the existing text-only, no-Confirm notice.

Summary, review and pending candidate/hash are built from one admitted
current/input/inventory snapshot without another read. A stale Confirm or
compare-and-swap conflict that requires a fresh preview uses the same review
builder with the fresh snapshot; refusal cannot fall back to the old Confirm.
The original pruning, uploader/guild/admin gates, download bounds/deadline,
TTL, cancel/replay and compare-and-swap writes are unchanged. Preview never
writes configuration or updates the live actor. This extension adds no endpoint,
import format, destination or permission.

Protected interaction regressions run without external IO:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_config_full_diff_acceptance
python3 scripts/cargo_cache.py run -- test -p two-bot --lib full_diff_acceptance -- --test-threads=1
python3 scripts/cargo_cache.py run -- test -p two-bot --lib voice_rooms::tests::import_ -- --test-threads=1
python3 scripts/cargo_cache.py run -- test -p two-bot --lib rollback -- --test-threads=1
```

They cover complete values and >20 changes, mixed/skipped-only input,
exact/one-over serialized cap, long/astral values, unsafe mentions,
absent/foreign guild/uploader, no-change, download failure, stale/current
Confirm, cancel/replay and unchanged no-write/CAS controls. These are synthetic
handler/store/response checks, not proof of Discord delivery or deployment.

## Residual parent integration (not parity evidence)

The parent V11 slice still owns Manage Server checks, attachment handling,
mapping the candidate to V1/V7 settings, template compilation, IANA timezone
and command-name verification, re-reading current inventory/settings, the
confirmation step, revalidation of `skip_unknown_channels(..).0` with
`voice_config::validate_configuration` before confirming and again before
writing, and the single transaction. This core never writes and never
validates beyond unknown-channel detection. No live import is performed or
verified by this component's tests.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_config_diff
```

The fixture is the codec's synthetic example plus spare inventory channels.
Golden tests pin the full preview, unknown-channel skipping, ordering, the
`No changes` body, truncation and escaping. Property tests over a structured
generator pin `diff(a, a)` emptiness, apply-equals-remaining-incoming with
revalidation, the exact skipped set, order invariance and the preview limit.
No network, Discord, database, credentials or sleep is needed.
