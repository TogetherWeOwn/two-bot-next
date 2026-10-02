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
