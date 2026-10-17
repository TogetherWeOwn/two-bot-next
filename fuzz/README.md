# Untrusted-input fuzz targets

This is a separate `cargo-fuzz` workspace, explicitly excluded from the root
workspace. Normal `cargo build`, workspace tests/clippy, `check` and cargo-deny
keep their existing members, dependency lockfile and stable toolchain. The required
`fuzz-compile` CI job (`check.yml`, gated by `ci-ok`) builds all nineteen targets
on pinned nightly `nightly-2026-10-01` + `cargo-fuzz 0.13.2` and then replays
the `eventsub_delivery` seed corpus once (`-runs=0`, no fuzzing). The other
eighteen targets are compile-only in required CI; bounded manual smokes exist for
`vote_kick` and `helix_token`. No scheduled or continuous fuzz campaign exists.

## Coverage

| Target | Production seam | Checks / limits |
| --- | --- | --- |
| `internal_action` | `internal_actions::authorize`, `KeyRing::verify` and payload validators | Arbitrary raw bytes are signed with public synthetic test material so MAC failure cannot starve JSON parsing. Also malformed headers, signed timestamps, unknown key and altered-body rejection. Fresh nonce/rate-limit state each call; no executor. Channel-moderation bodies (`moderation.purge`/`slowmode`/`lockdown`/`unlock`): snowflake actor/channel ids (canonical nonzero u64), trimmed audit reason, purge `count` 1–100 and slowmode `seconds` 0–6h, with an independent refusal-vs-accept oracle plus the fixed happy/bad-key matrix (seeds `seed-channel-*`). |
| `automod` | `AutomodPolicy::is_exempt`, `match_automod` | NFKC/zero-width text, bad words, URL/domain parsing, attachments, mentions and three distinct repeat observations. No moderation effects. |
| `prefix_trigger` | `leveling::valid_text_trigger`, `valid_command_name` | Independent byte-grammar oracle for `![a-z0-9_-]{1,32}`. **The current bot has no prefix-message parser** (`router.rs` documents this); this target covers the existing validators, not a nonexistent dispatcher. |
| `mee6_export` | `mee6_xp::parse_mee6_export`, `mee6_rewards::parse_mee6_role_rewards`, `parse_roles_snapshot` | JSON export variants, row types, numeric boundaries, role references. No import planner or database. |
| `guild_config_snapshot` | Restore CLI's JSON object/version gate, `verify_snapshot_integrity`, `canonical_snapshot`, `snapshot_counts`, `plan_restore` | Seal round-trip invariant and pure restore-plan decoding against an in-memory current guild. No restore apply/Discord call. This is not a full schema/authenticity check. |
| `voice_config` | `voice_config::import_configuration`, `export_configuration` | Strict decoding, same-/cross-guild references against a **fixed trusted inventory**, lossless/deterministic round trips. Templates are not compiled or run. |
| `rsvp` | `rsvp::partition_rsvps`, `checkin_classification`, `checkin_idempotency_key` (+ `checkin_source_event_id`, `checkin_source`, `checkin_metadata_json`) | Arbitrary NUL/newline-separated field sequences (capped at 64 fields, 16 records, 256 chars per field) with forced user-id reuse over a 4-id synthetic pool and legacy-order status cycling. Partition total/count/order invariants, classification exact-value and determinism pins, idempotency exact-format/determinism/byte-bounded checks with a metadata JSON round-trip. No database, Discord client, network or secret. |
| `settings_map` | Guild/assistant settings-map parsing including `AssistantConfig::from_map`, `FeatureGates`, `ModerationGates`, `VoiceGates`, `AutomodConfig`, `OnboardingGates`, `DisableGates`, `SelfRoleGates`, `InternalFlags`, `InternalActionConfig` and the settings catalogue | Arbitrary `KEY=VALUE` line maps with hostile strings, huge values, wrong types and unknown keys (max 32 entries, `-max_len=65536`). Invalid input fails closed (`None`/`Err`/disabled, never half-applied); unknown keys must not change outcomes. `Secret<T>` spot-checks assert constant `[REDACTED]` rendering on fuzz-derived values. No environment, network, database or secret use. |
| `vote_kick` | `VoteKickCore` start/vote/refresh | Arbitrary member/target/room/privilege/clock sequences over small synthetic ID pools (forced reuse), including three-way `target_privileged` evidence (`Some(false)`/`Some(true)`/`None`) so fail-closed `AuthorityUnavailable`/`PrivilegedTarget` refusals and mid-vote `TargetProtected` cancels stay reachable. Asserts no panic, replay refused, terminal votes stay terminal and the kick emits once with room scope (both per start generation: an ID evicted past the cooldown horizon may start again), and progress arithmetic holds. No Discord client, database or network. |
| `vote_kick_reason` | `voice_rooms::sanitize_vote_reason`, `message_safety::neutralize_mentions` | Arbitrary UTF-8 vote-kick reasons (invalid UTF-8 discarded). VK-04 invariants on every output: at most `VOTE_KICK_PUBLIC_REASON_LIMIT` chars, single line, no `@everyone`/`@here`, no `<@`/`<#`/`<:`/`<a:` pills, no `://`; the neutralizer half is pinned too (no raw mass mention, neutralize-twice stable). Seeds mirror the hostile matrix. No database, Discord client, network or secret. |
| `slash_input_bounds` | `lfg::parse_role_spec`, `custom_commands::AuditRecord` constructors, `scheduled::resolve_scheduled_id` | Arbitrary UTF-8 slash input (invalid UTF-8 discarded) driven through the #694 bounds: overlong `/lfg` role keys (including >2000 chars), multi-thousand-char `/command` names, empty plus overlong schedule id prefixes. `TooLong` if and only if the spec exceeds `MAX_ROLE_SPEC_CHARS` (refusal before parsing); `BadKey` echoes at most 128 chars; every audit `target_key` is exactly the 32-char prefix; empty/overlong prefixes resolve `Missing` before resolution and all other outcomes agree with an independent exact-or-prefix-match oracle over a fixed synthetic inventory. Seeds mirror the hostile matrix (mentions, links, markdown, overlong). No database, Discord client, network or secret. |
| `cutover_parse` | `parse_voice_message`, `parse_member_log_message`, `parse_leave_attribution` (+ `member_log_kind_for_channel` hint) | Arbitrary NUL/newline-separated title/description/footer/content/channel fields (capped at 8 fields, 256 chars per field) with hostile mentions, overlong digit runs, placeholder footers and multibyte usernames. The channel hint comes from the fuzzed channel name plus a cycled explicit hint so the no-context path stays reachable. Bad rows refuse (`None`, never half records); returned member/channel ids are snowflake-shaped, leave attributions carry a non-empty content-slice username with a closed `joined_via` set, and every parse is deterministic. Seeds mirror the hostile matrix (Logger/Wick voice, member join, vanity leave, adversarial). No database, Discord client, network or secret. |
| `lfg_select` | `lfg::parse_lfg_select`, `lfg_custom_id`, `parse_role_spec`, `spec_roles`, `lfg_select_options` | Arbitrary NUL-separated post id / select value / raw `custom_id` / role-spec fields (capped at 4 fields, 256 chars per id field, spec up to `MAX_ROLE_SPEC_CHARS` + 64 chars so the boundary is reachable). Select parse: foreign and forged (case, padding, zero-width) prefixes refuse (`None`); a doubled prefix strips once; after the one exact `two:lfg:` prefix the post id and role key are carried byte-for-byte (no trim, fold or unescape; the parser has no length bound, so the select path inherits Discord's 100-char component caps), and `Leave` only on the exact `__leave__` value. Accepted specs hold the store invariants (1–20 unique valid keys, never `__leave__`, trimmed 1–80 unit labels, slots 1–99), re-parse from their canonical rendering, and mint select options whose values sign up for exactly that role key (leave option last). A separately built row around a fuzzed label keeps every option label within the 100-unit cap. `TooLong` iff over `MAX_ROLE_SPEC_CHARS` (checked first); `BadCount` for more than 20 entries within that bound; `BadKey` echoes at most 128 chars. Seeds mirror the hostile matrix (forged/doubled prefixes, mentions, reserved and duplicate keys, Kelvin-sign key, radix slots, 20-role boundary, 21 roles, overlong ids and spec). No database, Discord client, network or secret. |
| `internal_action_validators` | `internal_actions::parse_body_object`, `require_timestamp`/`require_reason`/`require_snowflake`/`require_settings_key`, `validate_event_key`/`validate_idempotency_key`/`validate_moderation_numbers`, `within_skew`, `valid_nonce_format`, `build_role_keys`/`build_channel_keys`/`parse_keys`, `assert_private_bind`/`is_private_address`, `assert_allowed`, `new_request_id` | The request-validator seams the `internal_action` target does not reach. Arbitrary bytes go through the strict body parser (size ceiling, single object, deterministic), and every accepted object runs the field validators; each NUL-separated field runs the string grammars. Independent oracles pin the nonce, idempotency-key, event-key, snowflake and settings-key grammars, the skew window and the per-verb numeric bounds; accepted timestamps are `toISOString()`-shaped and stable on re-validation; accepted key maps hold trimmed keys and canonical snowflakes; signing-key ids are unique; a successful private bind is never a wildcard and hostnames are never private; unknown actions are never allowed; request ids are 26 Crockford characters. Known quirk, deliberately not asserted: a doubly bracketed host such as `[[::1]]` binds as `[::1]`. Seeds reuse the `internal_action` bodies and the bot nonce fixture strings plus hostile fields, duplicate keys, bind hosts and key maps. No database, Discord client, network or secret. |
| `eventsub_delivery` | `twitch_eventsub::verify_delivery`, `twitch_eventsub::classify` | Structured delivery inputs (signature, timestamp skew, message type, body) signed with public synthetic test material so MAC failure cannot starve the JSON parser. Fixed matrix pins challenge / notification / revocation happy paths plus tampered-signature (`BadSignature`), stale-timestamp (`StaleTimestamp`), unknown-type (`Ignored`) and oversize (`BodyTooLarge`) refusals; fuzzed inputs assert tampered inputs never verify (signature-bypass mutant fails), stale inputs never verify (freshness-widening mutant fails), and verified templates classify to their exact variant (misrouting mutant fails). Raw bodies get a signature-independent `classify` pass for parser coverage. Message ids stay in the header alphabet, types are capped at 64 chars, raw bodies at 8 KiB. Seeds cover challenge, notification, revocation, tampered signature, stale timestamp and unknown message type; keyword-guided inputs suppress byte-derived hostile flags so each seed reaches its named shape (pinned in the fixed matrix), and CI replays the seeds once. No database, Discord client, network or secret. |
| `helix_token` | `twitch_helix::parse_token_response` | Arbitrary bounded bytes decode as JSON before the production token parser; successful parses must retain a nonnegative expiry and redacted token formatting. Seeds cover a valid response, near-expiry token, missing fields, wrong types and a >64 KiB body. The target caps each input at 128 KiB; it uses a fixed synthetic clock and no network, database, credentials or secrets. The separate `helix-token-fuzz-smoke` workflow runs a bounded 60-second seed-backed campaign and uploads the receipt. |
| `fb_page_events` | `fb_page_events::webhook::{verify_challenge, verify_signature, parse_envelope}` | Raw bytes are the POST body verbatim while the lossy text split on NUL (`mode\0token\0challenge`) feeds the challenge verifier, so every input exercises all three seams with no keyword routing. The HMAC is recomputed with public synthetic test material; correct signatures verify exactly within the 1 MiB transport cap, tampered/re-prefixed ones never do. The challenge verdict must equal an independent accept-oracle, the envelope parse must be deterministic with every accepted value inside its bound, and every refusal must be a bounded `DriverError::Failed` that leaks no secret. Seeds cover a valid challenge, a bad-token challenge, a valid envelope, a wrong-object envelope, a malformed envelope and a >1 MiB body. No network, database, Discord client, clock or secret. |
| `voice_room_name` | `voice_room_name::{decide_custom_name, decide_template_name, is_literal_name}` | Arbitrary member/game text and templates through the V3 `/name` decision parsers with member-controlled render facts (bounded owner/game names). Every decided name is never empty, fits `MAX_CHANNEL_NAME_CHARS`, and carries no C0 controls, DEL, `@` or backticks; refusal replies stay short. Seeds cover literal, token, format-only, overlong, conditional-blocked and unicode-fold names. No database, Discord client, network or secret. |
| `helix_subscription_listing` | `twitch_helix::validate_subscription_listing_page`, `twitch_eventsub::parse_subscriptions` | NUL-separated arbitrary bytes decode as one JSON listing page each before the production envelope validator; an independent oracle recomputes the expected verdict so widening/narrowing mutants fail. Malformed envelopes must fail with the bounded `BadResponse` static literal (never response bytes); valid pages must parse deterministically into at most their `data` length. Multi-page accumulation mirrors the list path (cursor chaining, repeated-cursor refusal, terminal pages, malformed-later-page rejects the whole listing). Seeds cover valid empty/single/cursor pages, missing/null/scalar/object `data`, array top level, bad/empty cursor types and two multi-page shapes. The target caps each input at 128 KiB, pages per iteration at 8 (the 100-page production limit stays pinned by the offline acceptance test), entries parsed per page at 1024 and total accumulation at 4096; it uses no network, database, credentials or secrets. |

All seeds use synthetic IDs/text; no token, environment secret, database,
Discord client or network is needed at runtime. MEE6's crate currently has SQLx
and Twilight build dependencies, but these harnesses never construct their
clients. Invalid UTF-8 is discarded for parsers taking `&str`; automod/header
strings use lossy conversion, while JSON byte codecs receive the original bytes.
The JSON decoder's default recursion limit remains enabled.

## Local five-minute runs (non-controller development machine)

Use a supported Linux development machine with nightly Rust (at least the
repository's MSRV), `rust-src`, a C/C++ compiler and `cargo-fuzz`:

```sh
rustup toolchain install nightly --component rust-src
cargo install cargo-fuzz --locked
```

**Do not run these compiling commands on the persistent Paperclip controller.**
Controller commands must use `scripts/cargo_cache.py`; its current command allowlist
does not support `cargo fuzz`. A missing/refused pool is not permission to run
Cargo directly, use an external target, install a replacement cache or remove a
crash sentinel. Controller validation needs an authorized bounded fuzz execution
path first; see [the build-cache runbook](../docs/build-cache.md). The commands
below are for a non-controller machine, not a workaround for that gate.

Run from the repository root. Set `FUZZ_OUTPUT_DIR` to an absolute, writable
**non-source** directory outside the checkout on that development machine. Keep
build output, generated corpus, artifacts and logs there; committed seeds remain
read-only inputs. In Bash:

```bash
set -euo pipefail
: "${FUZZ_OUTPUT_DIR:?set an absolute non-source output directory}"
FUZZ_SECONDS=${FUZZ_SECONDS:-300}
mkdir -p "$FUZZ_OUTPUT_DIR/target" "$FUZZ_OUTPUT_DIR/logs"
for target in internal_action automod prefix_trigger mee6_export guild_config_snapshot voice_config rsvp settings_map vote_kick vote_kick_reason slash_input_bounds cutover_parse lfg_select internal_action_validators eventsub_delivery helix_token helix_subscription_listing fb_page_events voice_room_name; do
  mkdir -p "$FUZZ_OUTPUT_DIR/corpus/$target" "$FUZZ_OUTPUT_DIR/artifacts/$target"
  max_len=65536
  if [[ "$target" == helix_token || "$target" == helix_subscription_listing ]]; then max_len=131072; fi
  if [[ "$target" == fb_page_events ]]; then max_len=2097152; fi
  cargo +nightly fuzz run --target-dir "$FUZZ_OUTPUT_DIR/target" \
    "$target" "$FUZZ_OUTPUT_DIR/corpus/$target" "fuzz/corpus/$target" -- \
    -max_total_time="$FUZZ_SECONDS" -max_len="$max_len" -rss_limit_mb=2048 -timeout=10 \
    -artifact_prefix="$FUZZ_OUTPUT_DIR/artifacts/$target/" \
    2>&1 | tee "$FUZZ_OUTPUT_DIR/logs/$target.log"
done
```

Use `FUZZ_SECONDS=60` for an opt-in smoke; the acceptance run is **300 seconds
per target**, not 300 seconds across the suite. The input/RSS/time bounds are
campaign limits, not proven production limits. For the internal-action 2 MiB
body-size boundary, also run an extended campaign with `-max_len=2097153` when
resources permit. Build time does not count toward libFuzzer's fuzzing time.

Formatting is separate because the root workspace intentionally excludes fuzz:

```sh
cargo fmt --all -- --check
cargo fmt --manifest-path fuzz/Cargo.toml --all -- --check
```

`fuzz/Cargo.lock` is committed and seeded from the root lockfile: every shared
crate pins the same version as the root build, plus exactly three fuzz-only
crates (`arbitrary`, `jobserver`, `libfuzzer-sys`). The required `fuzz-compile`
job runs `cargo fetch --locked` before building, so the gate fails instead of
silently resolving fresh crates.io versions. Dependabot tracks `/fuzz` for
bump PRs; when the root lockfile moves, re-seed with `cp Cargo.lock
fuzz/Cargo.lock` plus `cargo generate-lockfile --manifest-path
fuzz/Cargo.toml` and pin any drifted shared crate back with `cargo update -p
<crate> --precise <root version>`. Preserve the generated fuzz lockfile with
the verification evidence before reporting a campaign; record nightly,
cargo-fuzz, libfuzzer-sys and OS versions and exact git HEAD. The required
`fuzz-compile` job proves the excluded targets build and replays the
`eventsub_delivery` seeds once (`-runs=0`); it runs no fuzz campaign for any
target. A bounded smoke result must cite its own non-controller host, command,
elapsed time and artifacts, never CI compile (or seed replay) alone.

## Crashes and evidence

Reproduce an artifact with the same toolchain and target, for example:

```sh
cargo +nightly fuzz run --target-dir "$FUZZ_OUTPUT_DIR/target" \
  voice_config "$FUZZ_OUTPUT_DIR/artifacts/voice_config/crash-<hash>"
```

Minimize with `cargo +nightly fuzz tmin` on the same non-controller machine.
Any discovered crash needs a fix plus a regression **unit test in the production
crate**; a corpus file alone is not a regression test. Retain crash/time-out/OOM
artifacts and logs as evidence; do not clean them as build cache. Keep only small,
reviewed `seed-*` cases committed, never the generated corpus or compiled output.

Report in the PR, for **each** target: HEAD, toolchain, command, elapsed fuzz time,
exit status, final run/coverage counts, and artifact count. A build failure, absent
toolchain or admission refusal means **not run**, not a clean five-minute result.
Normal CI (`check`, `pr-lint`, `gitleaks`) must also be green on the exact head,
followed by independent review and squash merge. No continuous fuzzing service is
part of this change.
