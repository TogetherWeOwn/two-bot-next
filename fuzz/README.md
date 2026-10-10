# Untrusted-input fuzz targets

This is a separate `cargo-fuzz` workspace, explicitly excluded from the root
workspace. Normal `cargo build`, workspace tests/clippy, `check` and cargo-deny
keep their existing members, dependency lockfile and stable toolchain. The required
`fuzz-compile` CI job (`check.yml`, gated by `ci-ok`) builds all ten targets
compile-only on pinned nightly `nightly-2026-10-01` + `cargo-fuzz 0.13.2`; no
scheduled or continuous fuzz campaign exists, and CI never executes `cargo fuzz run`.

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
| `vote_kick` | `VoteKickCore` start/vote/refresh | Arbitrary member/target/room/privilege/clock sequences over small synthetic ID pools (forced reuse), including three-way `target_privileged` evidence (`Some(false)`/`Some(true)`/`None`) so fail-closed `AuthorityUnavailable`/`PrivilegedTarget` refusals and mid-vote `TargetProtected` cancels stay reachable. Asserts no panic, replay refused, terminal votes stay terminal, the kick emits once with room scope, and progress arithmetic holds. No Discord client, database or network. |
| `vote_kick_reason` | `voice_rooms::sanitize_vote_reason`, `message_safety::neutralize_mentions` | Arbitrary UTF-8 vote-kick reasons (invalid UTF-8 discarded). VK-04 invariants on every output: at most `VOTE_KICK_PUBLIC_REASON_LIMIT` chars, single line, no `@everyone`/`@here`, no `<@`/`<#`/`<:`/`<a:` pills, no `://`; the neutralizer half is pinned too (no raw mass mention, neutralize-twice stable). Seeds mirror the hostile matrix. No database, Discord client, network or secret. |

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
for target in internal_action automod prefix_trigger mee6_export guild_config_snapshot voice_config rsvp settings_map vote_kick vote_kick_reason; do
  mkdir -p "$FUZZ_OUTPUT_DIR/corpus/$target" "$FUZZ_OUTPUT_DIR/artifacts/$target"
  cargo +nightly fuzz run --target-dir "$FUZZ_OUTPUT_DIR/target" \
    "$target" "$FUZZ_OUTPUT_DIR/corpus/$target" "fuzz/corpus/$target" -- \
    -max_total_time="$FUZZ_SECONDS" -max_len=65536 -rss_limit_mb=2048 -timeout=10 \
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

`fuzz/Cargo.lock` is independent of the root lockfile. Preserve the generated
fuzz lockfile with the verification evidence before reporting a campaign; record
nightly, cargo-fuzz, libfuzzer-sys and OS versions and exact git HEAD. The required
`fuzz-compile` job proves the excluded targets build; it does not execute them. A
bounded smoke result must cite its own non-controller host, command, elapsed time
and artifacts, never CI compile alone.

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
