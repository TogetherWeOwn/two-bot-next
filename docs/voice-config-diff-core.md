# V11a import diff preview core

`two_bot_core::voice_config_diff` is an original, pure implementation derived
only from [the voice-room specification](voice-rooms.md#v11-configuration-export-and-import).
It requires no `db` feature, Discord wire types, clock or external I/O.

## Domain contract

- `diff_configuration(current, incoming, inventory)` compares two
  `voice_config::VoiceConfiguration` values and reports added, removed and
  changed entries per section (creators, templates, aliases, lists, logging,
  settings), plus `skipped_unknown_channels`.
- Incoming entries that touch a channel ID absent from the inventory are
  skipped: they produce no add/remove/change and leave any current entry
  under the same key untouched. A skipped permission-source channel is
  reported alongside its owner key. The skipped IDs are sorted and unique.
- Changed creator/template entries carry the field names that differ;
  alias/list changes are keyed (`game`/`name`); logging and settings are
  singletons with their own field lists.
- Entry order in the input lists is insignificant. All change lists are
  sorted by key, and the preview renders sections in a fixed order
  (creators, templates, aliases, lists, logging, settings, skipped).
- `apply_diff(current, diff)` yields the filtered incoming in deterministic
  sorted order. `version` and `guild_id` are carried from `current`.
- `render_preview(diff, max_lines)` builds a compact ephemeral-message body:
  at most `max_lines` entry lines, then a `+N more` trailer. The result never
  exceeds `PREVIEW_CHAR_LIMIT` (2000). An empty diff renders `No changes`.

## Residual parent integration (not parity evidence)

The parent V11 slice still owns Manage Server checks, attachment handling,
mapping the candidate to V1/V7 settings, template compilation, IANA timezone
and command-name verification, re-reading current inventory/settings, the
confirmation step, revalidation with
`voice_config::validate_configuration` before writing, and the single
transaction. This core never writes, never validates beyond unknown-channel
detection, and silently treats cross-guild or wrong-kind channels as known
(the codec rejects those on revalidation). No live import is performed or
verified by this component's tests.

## Hermetic verification

```sh
cargo fmt --all -- --check
cargo clippy -p two-bot-core --all-targets --locked -- -D warnings
cargo test -p two-bot-core --test voice_config_diff --locked
```

The fixture mirrors the codec's synthetic example. Golden tests pin the full
preview, the skipped-channel list, deterministic ordering, the `No changes`
body and preview truncation. Property tests pin `diff(a, a)` emptiness,
apply-equals-filtered-incoming with revalidation, and the preview limit. No
network, Discord, database, credentials or sleep is needed.
