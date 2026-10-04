# Legacy temp-voice room-name sanitize and automod filter core

`two_bot_core::voice_name_filter` is a pure port of legacy two-bot
[`src/tempVoice/nameFilter.ts`](https://github.com/TogetherWeOwn/two-bot/blob/bffccf3e3a9f56a3da37de67c6f272ac10ecb3b3/src/tempVoice/nameFilter.ts)
(`sanitize`, `filterChannelName`, `renderNameTemplate`) and the create-path
retry in
[`src/tempVoice/service.ts`](https://github.com/TogetherWeOwn/two-bot/blob/bffccf3e3a9f56a3da37de67c6f272ac10ecb3b3/src/tempVoice/service.ts)
(`onGeneratorJoin`). It requires no `db` feature, Discord wire types, clock,
store or external I/O, and does not depend on V1 room lifecycle.

## One matcher, not two lists

A name is fed to the existing automod matcher
(`automod::match_automod`/`normalize_content`) as a synthetic message, so a
word blocked in chat is blocked in a channel name by construction. Only the
content-shaped filters apply — `bad_words`, `invite_link`, `external_link` —
and only those three can reject, with the legacy sentence
`That name is not allowed here (<filter>).` A name cannot mention anybody,
carry an attachment, or repeat itself, so the name-scoped policy neutralises
the other three (`mention_limit` and `repeated_message_count` set to their
maximum, attachment extensions emptied) rather than ignoring them afterwards:
a guild running mention limit 0 would otherwise short-circuit on
`mention_spam` and never reach the invite-link check. Exemptions are not
consulted; legacy inspects the synthetic message with no bypass-role or
exempt-channel check. The generator channel is the filter context.

| Limit | Value |
| --- | --- |
| `MIN_CHANNEL_NAME_CHARS` / `MAX_CHANNEL_NAME_CHARS` | 1–100 Unicode scalars of the sanitized name |

## `sanitize_channel_name`

Legacy `sanitize` in order: NFKC, C0 controls plus DEL become spaces, `@` and
backtick stripped, whitespace runs collapse to one ASCII space, ends trimmed.
Full-width `@` normalises to `@` first, so it strips too. Zero-width
characters survive (they are not whitespace); the automod matcher still sees
through them. Sanitizing is a fixed point, covered by a proptest. Lengths
count Unicode scalars where legacy counts UTF-16 code units, so astral
characters (emoji) count one here and two there — this core is marginally
more permissive for them.

## `render_name_template` and `resolve_create_name`

`render_name_template` substitutes the legacy `{username}`/`{count}`/`{seq}`
placeholders in that order (a username holding `{count}` still expands),
sanitizes, falls back to `voice channel` when empty, and truncates to 100
scalars. This keeps the legacy placeholder syntax; the V5 engine
(`voice_naming`, `@@owner@@` syntax) is a separate path.

`resolve_create_name` filters the rendered template; when it is blocked it
retries with an empty username and mints that name instead (`username_stripped`
reports the downgrade). Only a template that is itself blocked refuses — an
operator misconfiguration to fix, not a member to punish — with the legacy
`That channel name is not allowed here. <cause>` sentence and the stable
`name_blocked` audit reason. Render-level empty and overlong outcomes can never
reach refusal: the render falls back and truncates first.

## Runtime wiring

The bot's room worker (`crates/bot/src/voice_rooms.rs`) is the caller. On each
join, `accept_join` renders the V1 template `{username}'s room` through
`resolve_create_name` under the configured automod policy
(`AutomodPolicy::name_policy_from_map`, applied whether or not automod enforcement
is on). This independently loads the word list and allowed domains with the
same normalization as chat automod; an invalid unrelated count, sanction or
exemption cannot discard name restrictions. Chat automod's strict configuration
validation is unchanged. A blocked display name is retried
without the username; a blocked bare template queues nothing, so no Discord
create call is made, and the worker records the refusal as `name_blocked` in
its failure list. That list feeds the operator error notice and the `/setup`
failure line; it carries only the stable reason and the blocking filter,
never the member's name. `/create` names run through `filter_channel_name`
under the same policy before the REST call.

## Residual parent work

Room caps and claims, audit rows, V5 template expansion (member-requested
renames are not filtered yet), and persistence stay on the voice parent. Unit
and worker fixtures establish behaviour only, not staging readiness.

## Hermetic verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --all-targets -- -D warnings
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test voice_name_filter
python3 scripts/cargo_cache.py run -- test -p two-bot --lib voice_rooms::tests::name_filter_
```

The fixture has table tests for every rule (sanitize steps, length bounds,
all three rejecting filters with their exact sentences, neutralised filters,
render substitution/fallback/truncation, and the create-path retry/refusal),
plus a proptest that sanitize is a fixed point whose output carries no
stripped characters. The `name_filter_` worker fixtures drive the same rules
through `accept_join` and `/create` with an invented blocked term. No test
uses a database, Redis, Discord or a staging identity.
