# Automod legacy golden corpus

The offline corpus is `crates/core/tests/fixtures/automod_corpus.json`; its runner
is `crates/core/tests/automod_corpus.rs`. It exercises the existing public core
functions. It does **not** implement a mock automod service and then assert the
mock's own decisions. No database, Discord request, wall clock, process environment
mutation, or production/staging data is required.

## Pinned inventory and coverage boundary

Legacy source: [`TogetherWeOwn/two-bot` at
`a8d9f53f6958d036a1f6fc34afcca70baa8e45a4`](https://github.com/TogetherWeOwn/two-bot/tree/a8d9f53f6958d036a1f6fc34afcca70baa8e45a4).
This snapshot includes the matcher suite added by legacy PR #277; the older
rewrite snapshot does not. Counts are source-derived, **not a claim that the
legacy database tests ran here**.

| Legacy file | Tests | Assertion call sites | Expanded checks | Executable core expectations | Explicitly deferred checks |
| --- | ---: | ---: | ---: | ---: | ---: |
| `test/unit.automodmatcher.test.ts` | 11 | 35 | 86 | 85 | 1 |
| `test/unit.automod.test.ts` | 15 | 70 | 107 | 35 | 72 |
| `test/unit.automodwiring.test.ts` | 5 | 18 | 19 | 0 | 19 |
| **Total** | **31** | **123** | **212** | **120** | **92** |

An expanded check is one table row or fixed-loop iteration of an assertion.
For example, matcher line 70 executes 14 times. References use
`test/unit.automodmatcher.test.ts:70#1` through `:70#14`; one-shot sites have no
suffix. The fixture includes the complete line inventory and multiplicities.
The coverage test requires all 212 references, unique case IDs, valid references,
matching row/assertion counts, and a separate expected outcome for every deferred
assertion. Removing a table row and its mapping fails coverage.

There are **176 fixture cases: 153 executable core cases and 23 explicitly
deferred cases**. The latter include 92 expanded legacy checks plus one scrubbed
native-rule export record with no unit-test assertion. Supplemental cases have
empty source-reference lists and are not added to the legacy assertion count.
Counts distinguish mapped expectations from executed/verified parity.

## Core scenarios

- Harmless blocked phrases: whole words, case, NFKC/fullwidth, inter-letter spaces,
  zero-width separators, punctuation, literal regex characters, empty configured
  words, empty content, and false-positive substring/underscore boundaries.
  The three direct-policy empty-word assertions use literal `raw_bad_words`
  arrays, passed to the real matcher without CSV/config normalization. Inventory
  integrity requires their empty and whitespace-only entries. Config-side
  removal of those entries is tested separately by `supplement-config-empty-words`.
- No blanket confusable folding is claimed: the supplemental Cyrillic-lookalike
  control does not match. Legacy uses NFKC, not Unicode confusable skeletons.
- Explicit mentions: none, below/at/above threshold, repeated mentions of one
  user, and the staging default threshold of five. A reply represented with no
  explicit mentions remains clean; gateway extraction itself is deferred.
- Invite schemes, Discord host variants, case and zero-width splitting; plain
  invite wording and non-invite Discord paths are controls.
- External links: exact/www/subdomain allowlisting, scheme-less/bare hosts,
  punctuation/angles, zero-width splitting, email/source/filename guards. Extra
  WHATWG-host cases cover userinfo, host suffix attacks, queries, IDNA and invalid
  hosts. Replacement hosts retain relevant `.gg`, `.net` and `.com` suffixes;
  using only `.example` would remove legacy bare-domain candidates. A denied bare
  domain inside an allowed URL's query still triggers the separate bare-domain
  scan; a plain-text query is clean. This is preserved legacy behavior, not a
  WHATWG-host parsing divergence. Both outcomes were checked against the pinned
  legacy matcher itself.
- Attachments: uppercase, multi-dot, safe PNG, extensionless/trailing-dot and empty
  lists, plus every one of the eleven default blocked extensions.
- First-hit filter precedence and explicit repeat sequences: same-ID edits,
  distinct IDs, author/guild isolation, blank content, window expiration,
  inclusive cutoff, and NFKC/case/whitespace identity. Legacy lookback depth
  ([TOG-12582](/TOG/issues/TOG-12582)): with count 3 the verdict counts the new
  row plus up to 3 earlier rows, so `supplement-repeat-lookback-filler-first`
  (`X,Y,X,X`) and `supplement-repeat-lookback-filler-middle` (`X,X,Y,X`) trip
  on the 4th while the two-filler control stays clean.
- Config enable/dry-run/enforce gates, parsed policy, invalid sanctions, and
  supplementary selection of `1:delete,2:warn,3:timeout:600` at counts 0/1/2/3/99.
  `supplement-first-timeout-sanction` proves core selection of `1:timeout:600`
  on violation 1. The matching deferred transport-failure record preserves the
  enforcing mode, matched message, ordinary target and bot hierarchy from legacy;
  its service-side timeout failure and retained claim remain unexecuted.
- Pure target-protection and exemption projections. These **do not prove** that
  the adapter invokes protection before deletion or suppresses side effects.

## Known differences and unexecuted runtime expectations

No known case is silently converted into a PASS, and no `#[ignore]` hides it.
The runner prints executed and deferred totals with `--nocapture`. Deferred
records preserve setup, expected result and the issue that owns the missing
runtime verification. They are not waivers for automod activation.

Deferred setup retains branch prerequisites, not just the final assertions:
the bad-guild activation case keeps the bot approved; gateway release failure
follows a successful claim and definite `discord_rejected`/403 deletion refusal
in enforcing mode against an ordinary target. The hierarchy refusal uses equal
role positions, the ordinary-target control keeps a configured protected role,
the resolver refusal retains its typed error and timeout-at-one ladder, and the
partial-edit case has no cached old message. These are source-fidelity records,
not executed service/gateway checks.

| Case family | Preserved expectation / limitation | Tracking |
| --- | --- | --- |
| Raw leading-dot extension policy | Legacy matcher accepts raw `['.exe']`; next matcher expects normalized `['exe']`. `AutomodConfig::from_map` normalizes this correctly, but raw public policy does not. The legacy direct-policy assertion is deferred, not substituted with the config-normalized check. | [TOG-10089](/TOG/issues/TOG-10089) |
| Service ladder and audit | Delete each matched message; warn at 2; timeout 600s at 3; signed reason; warning/audit counts; no content in metadata; replay makes no second mutation. Core sanction selection is separately executable, not a service proof. | [TOG-10089](/TOG/issues/TOG-10089) |
| Dry-run ledger separation | No enforceable violations; later enforcing the same message starts at delete; dry-run skips target resolution and mutations. | [TOG-10089](/TOG/issues/TOG-10089) |
| Retry/failure semantics | Definite delete failure releases claim; delayed replay does not increment; matched in-flight failure stays blocked; uncertain post-mutation/timeout failure retains claim; resolver refusal has no mutation/audit and is retryable. | [TOG-10089](/TOG/issues/TOG-10089) |
| Protection before delete | Owner/staff take zero mutations at first delete rung; refused audit is metadata-only; replay stays silent. Hierarchy-only timeout refusal still permits deletion. | [TOG-10089](/TOG/issues/TOG-10089) |
| Gateway/activation | Blocked creates suppress funnel/downstream work; claim/release failures remain blocked; cached and fetched-partial edits use edit timestamps without a second funnel event; other guild is not inspected; boot rejects unapproved activation identities. | [TOG-10261](/TOG/issues/TOG-10261) |
| Native Discord rule | Scrubbed native rule has threshold 20, raid protection, enabled block action and empty exemptions. It is **not** the custom matcher default of 5 or the unit policy limit of 3; native raid detection is not executable in this core suite. | [TOG-10261](/TOG/issues/TOG-10261) |

The base next revision does not yet expose an automod service/store or gateway
wiring to these core integration tests. Completing the linked runtime work must
promote the deferred expectations to tests of the **real** adapter/store. This
PR changes only fixtures, tests and documentation; runtime wiring is out of scope.

## Scrubbed staging-artifact mapping

All 12 rows of legacy `audit/staging-automod-corpus.md` are accounted for:

| Manual row | Core fixture / preserved runtime expectation |
| --- | --- |
| 1: ordinary text | `legacy-M-clean-*` |
| 2: blocked phrase / obfuscation / boundary | `legacy-M-bad-*`, `legacy-A-filter-*` |
| 3: repeat threshold / window / ladder | `legacy-M-repeat-edit`, `legacy-M-window`, `supplement-sanction-*`, `known-service-ladder` |
| 4: five mentions / repeated user / implicit reply | `staging-default-mentions-*`, `staging-implicit-reply-not-explicit`, `legacy-A-filter-4-duplicates`; extraction remains gateway scope |
| 5: invite / invisible host / plain Discord | `legacy-M-invite-*`, `staging-plain-discord` |
| 6: external and allowed URL controls | `legacy-M-link-*` |
| 7: blocked / safe attachment | `legacy-M-attachment-*` |
| 8: bypass role / ordinary member | `supplement-exemption-role`, `staging-ordinary-not-exempt`, `known-exempt-and-dry-run-effects` |
| 9: exempt / general channel | `supplement-exemption-channel`, `staging-ordinary-not-exempt`, `known-exempt-and-dry-run-effects` |
| 10: edit into trigger / allowed edit | `staging-edit-classification`, `known-gateway-edits`; real event dispatch remains deferred |
| 11: replay | `known-service-ladder`, `known-delayed-store-retry`, `known-gateway-protected-refusal` |
| 12: protected target | `supplement-protection-*`, `known-protection-before-delete-*`, `known-refusal-*` |

**Row 12 of the old manual corpus is stale:** it permits deleting a protected
staff target's message. The corrected legacy tests at service lines 436–502 and
wiring lines 151–215 require **zero mutations before or after refusal**. This
corpus preserves that corrected behavior, not the unsafe old manual wording.
Hierarchy-only refusal remains a different case.

The native export `audit/staging-automod-rules-2026-09-09.json` is represented in
`known-native-discord-rule` with synthetic guild/creator/rule IDs, a neutral name,
and unchanged behavioral fields. Real guild/application/member/role/channel/rule
IDs, tokens, identities, message content and live policy names were not copied.
Bad phrases, ordinary messages, IDs and hosts are deliberately synthetic.

## Verification and release boundary

Controller command (only when bounded admission is deployed):

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test automod_corpus -- --nocapture
python3 scripts/cargo_cache.py run -- clippy -p two-bot-core --test automod_corpus -- -D warnings
cargo fmt --all -- --check
```

Hosted `check` already runs `--workspace --test '*'` and all-target clippy, so this
new integration target is included without adding workflows/dependencies.
Required exact-head gates remain `check` (fmt/clippy/tests/cargo-deny), `pr-lint`
and `gitleaks`. The PR must be independently reviewed and squash-merged.

Initial controller evidence: bounded wrapper refused with exit 75,
`not a real directory: /paperclip/.cache/two-bot-next-bounded`; Cargo was also not
installed on PATH. No direct compilation, alternate cache, test database, Discord
request or staging/production probe was attempted. A verified standalone rustfmt
component in run-owned scratch supplies formatting only; compilation/testing is
left to authorized hosted CI. Local JSON inventory and scrubbing checks are
recorded separately from CI; they are not Rust test evidence.

**Full automod QA: NEEDS WORK until the 92 deferred legacy checks and the staging
soak are verified on the real runtime.** Green corpus CI proves only the executable
core expectations and inventory integrity; it does not authorize live activation.
