# Automod repeat-window and expiry acceptance

`crates/core/tests/automod_repeat_expiry.rs` holds the acceptance tests for the
repeated-message filter in [parity §8](parity.md) (automod sanctions). The tests
call only the existing public core API: `AutomodConfig::from_map`,
`RepeatTracker::{observe, expire}`, `match_automod`, `normalize_content` and
`sanction_for`. They need no database, Discord request, network, process
environment mutation or wall clock. All IDs and message text are synthetic.

Legacy source: [`TogetherWeOwn/two-bot` at
`a8d9f53f6958d036a1f6fc34afcca70baa8e45a4`](https://github.com/TogetherWeOwn/two-bot/blob/a8d9f53f6958d036a1f6fc34afcca70baa8e45a4/src/automod/matcher.ts),
specifically `MemoryRepeatTracker.observe` and the `matchAutomod` filter order.
The [golden corpus](automod-corpus.md) already covers the single-case repeat
fixtures. This suite adds the multi-step window, expiry and ordering properties
that a one-row fixture cannot express.

## Acceptance criteria

| Criterion | Test |
| --- | --- |
| N identical messages from one author inside `repeated_message_window_seconds` trip the repeat sanction | `configured_count_inside_window_trips_repeat_sanction` |
| `expire()` drops rows older than the window, so an idle guild retains no stale history | `expire_sweeps_every_idle_author_without_another_message`, `expire_drops_only_rows_older_than_the_window` |
| Repeats from different authors never combine | `repeats_from_different_authors_never_combine` |
| Empty normalized content never counts | `empty_normalized_content_never_counts` |
| Messages that pass the bad-word check still count toward the repeat total (legacy `matchAutomod` order) | `messages_past_the_bad_word_check_count_even_when_a_later_filter_matches`, `bad_word_hits_return_before_the_repeat_observation` |

What each test pins:

- **Configured window.** The window uses `TWO_AUTOMOD_REPEAT_COUNT=4` and
  `TWO_AUTOMOD_REPEAT_WINDOW_SECONDS=10`. The fourth copy lands exactly at the
  window edge: the cutoff is inclusive, so it trips `RepeatedMessage`. The first
  sanction for that strike is `Delete`. When the fourth copy arrives 1 ms later,
  the first copy has left the window, so it does not trip; the fifth copy does.
- **Expiry.** `RepeatTracker` exposes no row accessor, so expiry is proven
  through behaviour:
  - Two authors each leave two copies, and the guild then goes idle.
  - A backdated probe for each author would complete the run of three, so it
    trips if any stale row survived the sweep.
  - With no sweep, both probes trip. This is the control, and it proves the
    probe is sensitive.
  - After `expire(last + window + 1 ms)`, neither probe trips.
  - `expire` is guild-wide: neither author sent another message before the sweep.
  - A second test shows the boundary. A row exactly `window` old survives, and
    one 1 ms older is dropped. Rows newer than the cutoff stay and keep counting.

  This matches the legacy per-key timer, which deletes a key `windowMs` after its
  last observation.
- **Author isolation.** Three authors interleave copies of the same text, and so
  does the same author ID in another guild. No count pools across these keys.
  Each author trips only on their own third copy. The tracker key is
  `guild:author`.
- **Empty content.** Blank, ASCII whitespace and Unicode whitespace inputs
  (U+00A0, U+3000, U+2003, U+2028, U+2029) normalize to `""`, are never
  recorded and never trip. They also do not break a real streak: blank messages
  between two copies leave the pair intact, and the second copy trips at count 2.
- **Filter order.** Bad words return before the repeat observation, so a
  bad-word hit is neither counted nor treated as a break in the streak.
  - `X, bad, bad, bad, X, X` trips on the last `X`.
  - Two bad-word hits followed by three copies under a policy without bad words
    trip on the third copy, not the first. Bad-word hits were never recorded.
  - Messages that a later filter matches (invite link, mention spam, attachment
    type) are observed first. Copies 1 and 2 report the later filter, and copy 3
    reports `RepeatedMessage`, as in legacy
    `["invite_link","invite_link","repeated_message"]`.

## Known divergences (not pinned)

The suite pins only behaviour where legacy and Rust agree. Two differences are
tracked in [TOG-12560](/TOG/issues/TOG-12560) and are deliberately left out of
these tests:

1. **Lookback depth.** With the default count of 3, legacy keeps up to `count`
   earlier rows and trips on the fourth message of both `X, Y, X, X` and
   `X, X, Y, X`. Rust `matches_window` inspects only `count - 1` earlier rows and
   returns `None` for both.
2. **Whitespace set.** JS `\s` includes U+FEFF and excludes U+0085, while Rust
   `split_whitespace` does the opposite:
   - In legacy, a BOM-only message normalizes to empty and never counts, while a
     NEL-only message repeats.
   - In Rust, the BOM-only message repeats and the NEL-only message never counts.

The legacy results were reproduced with node against the pinned `matcher.ts`.
The Rust side is executed by CI: `cargo test --workspace --test '*'` runs this
file. TOG-12560 decides whether to fix each difference or record it in
§8.

## Run

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test automod_repeat_expiry
```

CI runs the same file through `cargo test --workspace --test '*'`.
