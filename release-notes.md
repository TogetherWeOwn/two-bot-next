:robot: I have created a release *beep* *boop*

## Thinking Path

> - two-bot-next is the Together We Own Discord bot, written in Rust, running as one always-on container.
> - Its workspace ships as one synchronized release with one root changelog and tag.
> - Generated metadata must use the main snapshot that is independently reviewed and merged.
> - Regeneration must retain excluded-consumer synchronization, contributor-example CI coverage and every historical release note.
> - This pull request prepares synchronized 0.3.0 metadata; it does not authorize production cutover or a stale-main merge.

## Linked Issues or Issue Description

- Address the [latest freshness finding](https://github.com/TogetherWeOwn/two-bot-next/pull/71#pullrequestreview-5406543783) on this same PR. Regenerate from snapshot `48b070a2`, incorporating #566 staging-apply claims and #564 safety-flag tests, and retain the reviewed source and canonical-metadata repairs. Main moved again after that generation, so a new green head alone cannot satisfy the live freshness gate.

## What Changed

- Regenerate the root release manifest, seven package versions and local dependency pins, Cargo.lock, and CHANGELOG.md through release-please 17.6.0.
- Restore the same seven-file source repair: excluded fuzz path requirements, copyable testsupport dependency example, native generic extra-file updates, lifecycle regression coverage, release documentation, and contributor-only Worker CI selection.
- Preserve those seven repair files byte-for-byte from the prior repaired source. Keep the reconciled incoming changelog verbatim, not a hand-spliced conflict resolution.
- Retain the native overflow link and full freshly generated notes region/footer unchanged by this seven-section metadata repair. Preserve every prior notes line and its bullet multiplicity; add the native fix entry for #566.
- Include #564 in the source snapshot without inventing a user-facing changelog entry for its `test` type. Preserve #550/#533 notes and the prior #350 source/toolchain changes.
- Credit release-please automation and the repository contributors whose changes appear below.

## Verification

- [Regeneration workflow 37208033953](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/37208033953): SUCCESS from snapshot `48b070a24e8db01b0576ea349cb6ef97ba82de10`; reconciled generated head `b8765488e20df1d5f04ea7af9cf27cc24351e8d5`.
- The previous head `8ccbad3f` passed [check/ci-ok 37203426434](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/37203426434) and [supply-chain 37203426228](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/37203426228). Those are historical results, not proof for this regenerated head.
- Before this regeneration, current-main release planning returned `reuse_pr=false`. Main subsequently moved to observed `c60167e4` after #540 while this generation was in flight. Do not merge this snapshot as though it were fresh.
- Current repair pushed as `6b88aa3f`. `NODE_PATH=<pinned-dependencies> node scripts/test-release.cjs`: PASS, 22 bootstrap plus 22 post-release native lifecycles and all migration/overflow/snapshot guards.
- `node scripts/test-release-publication.cjs` with release-please 17.6.0: PASS, publication/retry and misplaced-template negative control.
- `python3 scripts/test-release-retry.py`: 28 tests PASS. `python3 scripts/test-pr-lint.py`: 27 tests PASS.
- `PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_job_inputs.py' -v`: 58 tests PASS; equivalent discovery for `test_container_inputs.py`: 26 tests PASS.
- `python3 scripts/test-docker-deps.py`: seven valid package targets PASS without Cargo compilation. `git diff --check`: PASS. All seven repair files match the prior repaired source; CHANGELOG.md matches the incoming generated head byte-for-byte.
- No old CI or approval is substituted for new exact-head checks and independent review.
- Canonical-body verification checks strict seven-section metadata with the generated-branch exemption removed, byte-identical native region/footer and unchanged visible link, preservation of historical bullet counts, and the actual pinned native overflow publication path with a misplaced-template negative control. The replay mocks all GitHub transport and creates no real release.
- No local Rust compilation, full fuzz campaign, staging E2E, production test, deployment, tag verification or SBOM publication verification is claimed. Exact-head CI and independent approval remain pending, and live freshness must be restored before merge.

## Risks

- Pre-1.0 release publication is not production deployment or cutover approval.
- A merge during the generation/CI/review window invalidates the snapshot, even when its source CI is green and the branch is mergeable. Resolve the merge-admission coordination rather than weakening freshness.
- Native delimiter placement controls published notes; the pinned publication fixture protects that boundary.
- The generic updater changes only annotated version lines; native lifecycles preserve the unpublished fuzz package version, external dependencies and target definitions.
- The freeze stays until tag/Release, attached SBOMs and the tagged label are verified. No author merge or stale-main exception.

## Model Used

- OpenAI `gpt-6.1-sol` authored the earlier source repairs and this regeneration integration, canonical metadata repair and verification. release-please 17.6.0 generated versions and release notes. No routed model output was incorporated. Historical work remains credited to its contributors; context-window size was not supplied.

## Checklist

- [x] I wrote a thinking path that runs from the project to this change
- [x] I named the model used, with its version
- [x] I searched for related PRs and linked them above
- [x] I ran the relevant offline release tests locally and they pass
- [x] I retained regression coverage for excluded consumers, subsequent releases and contributor-only CI selection
- [x] I updated the documentation this change touches
- [x] No secret, token or credential is in the diff, the title, the body or the branch name
- [x] No internal ticket id, instance link or private host is in the title, body, commits or branch name
- [ ] CI is green on the regenerated exact head before independent approval (pending)
---


## [0.3.0](https://github.com/TogetherWeOwn/two-bot-next/compare/v0.2.0...v0.3.0) (2026-10-04)

### Added

* **analytics:** preserve voice reconcile and leave-gap integrity checks ([#332](https://github.com/TogetherWeOwn/two-bot-next/issues/332)) ([9c6adf0](https://github.com/TogetherWeOwn/two-bot-next/commit/9c6adf0232b53fcd2a6791b16ac8e436d24d35ae))
* **attendance:** bound occurrence IDs before recording ([#188](https://github.com/TogetherWeOwn/two-bot-next/issues/188)) ([6b7326a](https://github.com/TogetherWeOwn/two-bot-next/commit/6b7326a212aa14aebfb68746ee707db877ae0afe))
* **audit:** mirror delivery, reconciliation and kill-switch enforcement ([#69](https://github.com/TogetherWeOwn/two-bot-next/issues/69)) ([b4ed160](https://github.com/TogetherWeOwn/two-bot-next/commit/b4ed1603def0ecca3b9bd0498c77fd3d64c21ad9))
* **audit:** record member, voice and message events from gateway ([#390](https://github.com/TogetherWeOwn/two-bot-next/issues/390)) ([0efd9ad](https://github.com/TogetherWeOwn/two-bot-next/commit/0efd9adbf12fbafb65a69e1f879bf72715c99f3f))
* **audit:** run the mirror retry sweep in the bot runtime ([#257](https://github.com/TogetherWeOwn/two-bot-next/issues/257)) ([573434e](https://github.com/TogetherWeOwn/two-bot-next/commit/573434e14aea3768bcdaf8c97187d8f856cddbae))
* **automod:** add durable gateway decisions and capture-only handoff ([#40](https://github.com/TogetherWeOwn/two-bot-next/issues/40)) ([8c6458d](https://github.com/TogetherWeOwn/two-bot-next/commit/8c6458dc5e8655014219e09ec10d147a59703ad0))
* **automod:** add shared activation orchestrator over the REST executor ([#266](https://github.com/TogetherWeOwn/two-bot-next/issues/266)) ([fcb02f7](https://github.com/TogetherWeOwn/two-bot-next/commit/fcb02f78b622cc5257a27b53c74ee4668f339cc6))
* **automod:** wire activation into the gateway loop ([#279](https://github.com/TogetherWeOwn/two-bot-next/issues/279)) ([c1f5531](https://github.com/TogetherWeOwn/two-bot-next/commit/c1f55318d090e7db39fbd1337d9d4166af96eb25))
* **backup:** cover durable tables and restore every sequence ([#135](https://github.com/TogetherWeOwn/two-bot-next/issues/135)) ([1d9fb4b](https://github.com/TogetherWeOwn/two-bot-next/commit/1d9fb4b172132c2d5c95b366ad37bee3b812eb85))
* **bot:** report the gateway start-failure class on /readyz ([#374](https://github.com/TogetherWeOwn/two-bot-next/issues/374)) ([8ed6641](https://github.com/TogetherWeOwn/two-bot-next/commit/8ed66419658322141b048f5ef0e7b8045812f52c))
* **commands:** port custom-command runtime and persistence ([#107](https://github.com/TogetherWeOwn/two-bot-next/issues/107)) ([2187981](https://github.com/TogetherWeOwn/two-bot-next/commit/21879816a001bbf893fde28b7408d5f2a782a797))
* **containment:** add durable event and incident claim store ([#255](https://github.com/TogetherWeOwn/two-bot-next/issues/255)) ([f40b86c](https://github.com/TogetherWeOwn/two-bot-next/commit/f40b86c92f3599d2732d653eae3f4951cb09f170))
* **containment:** enforce per-executor cooldown and pin alert shape ([#174](https://github.com/TogetherWeOwn/two-bot-next/issues/174)) ([e1fdb95](https://github.com/TogetherWeOwn/two-bot-next/commit/e1fdb95ccc685163561f54035758a09809fb695d))
* **containment:** wire audit-log entries to quarantine plans ([#393](https://github.com/TogetherWeOwn/two-bot-next/issues/393)) ([381dd4e](https://github.com/TogetherWeOwn/two-bot-next/commit/381dd4e44815897fc7e8d7c8dbdf5d82c125be97))
* **core:** add pure prefix trigger parser for text commands ([#235](https://github.com/TogetherWeOwn/two-bot-next/issues/235)) ([3b8b7cc](https://github.com/TogetherWeOwn/two-bot-next/commit/3b8b7cc32d8db4898184290d3fea17eb8997404e))
* **core:** add pure V12a template-assistant monthly-cap ledger ([#224](https://github.com/TogetherWeOwn/two-bot-next/issues/224)) ([5809a60](https://github.com/TogetherWeOwn/two-bot-next/commit/5809a60330ff70cb58e9fe12d162715931d9618f))
* **core:** add pure V7c channelinfo resolution core ([#228](https://github.com/TogetherWeOwn/two-bot-next/issues/228)) ([4fd810b](https://github.com/TogetherWeOwn/two-bot-next/commit/4fd810b78733d14ae0e307c44b73fd39d1612947))
* **core:** add pure V7d alias-resolution core ([#230](https://github.com/TogetherWeOwn/two-bot-next/issues/230)) ([bb5ca56](https://github.com/TogetherWeOwn/two-bot-next/commit/bb5ca56fd231b6f06593d221fe2030da508cb6b5))
* **core:** add pure V8a room-permission override builder ([#194](https://github.com/TogetherWeOwn/two-bot-next/issues/194)) ([4b8f19b](https://github.com/TogetherWeOwn/two-bot-next/commit/4b8f19b7b90e2d9c92a9e9f4b5012f2800da8936))
* **core:** add pure voice condition evaluator ([#246](https://github.com/TogetherWeOwn/two-bot-next/issues/246)) ([78beeb5](https://github.com/TogetherWeOwn/two-bot-next/commit/78beeb5ba491b60cd60cf46c4573d93316d1c430))
* **core:** add pure voice private-room join-request core ([#206](https://github.com/TogetherWeOwn/two-bot-next/issues/206)) ([32aa189](https://github.com/TogetherWeOwn/two-bot-next/commit/32aa189582b685ea7e8e98e68b1aedc3bd8d272d))
* **core:** add pure voice room placement and numbering core ([#195](https://github.com/TogetherWeOwn/two-bot-next/issues/195)) ([48683c4](https://github.com/TogetherWeOwn/two-bot-next/commit/48683c45c8fdb5e6686f42e77c2ca1a64aca0e3e))
* **core:** add pure voice styling-mode library ([#200](https://github.com/TogetherWeOwn/two-bot-next/issues/200)) ([e41354a](https://github.com/TogetherWeOwn/two-bot-next/commit/e41354a1a0f6b9519c76a6f176cc061bc82bb019))
* **core:** add scan_joins_for_bursts raid replay with legacy historical-raid scenarios ([#272](https://github.com/TogetherWeOwn/two-bot-next/issues/272)) ([03efce8](https://github.com/TogetherWeOwn/two-bot-next/commit/03efce870d54d9f9ab8858800c55bf5cbe18df9a))
* **core:** add voice config import diff preview ([#201](https://github.com/TogetherWeOwn/two-bot-next/issues/201)) ([3647519](https://github.com/TogetherWeOwn/two-bot-next/commit/3647519e7dcc5dc15b6a0ec6710788d4f97821b8))
* **core:** port Sunday Squad scheduled-event payload and occurrence builders ([#274](https://github.com/TogetherWeOwn/two-bot-next/issues/274)) ([ee0a516](https://github.com/TogetherWeOwn/two-bot-next/commit/ee0a516ddfa02d5e49ef23babed827877273a7dc))
* **core:** pure owner room-controls decisions ([#193](https://github.com/TogetherWeOwn/two-bot-next/issues/193)) ([1e872d1](https://github.com/TogetherWeOwn/two-bot-next/commit/1e872d14681eaf5faa5c8ffcb54dc811ffbb790d))
* **core:** pure V9 companion text-channel rules ([#196](https://github.com/TogetherWeOwn/two-bot-next/issues/196)) ([0be1ff8](https://github.com/TogetherWeOwn/two-bot-next/commit/0be1ff86097a90a0bad836bc0828b42dda212733))
* **core:** pure voice permission health evaluator and notice policy ([#199](https://github.com/TogetherWeOwn/two-bot-next/issues/199)) ([ccfe134](https://github.com/TogetherWeOwn/two-bot-next/commit/ccfe134d0349e80d0f6f9a6f6fff797c5fa0af2c))
* **cutover:** add audit-switch halt/resume/status operator CLI ([#182](https://github.com/TogetherWeOwn/two-bot-next/issues/182)) ([322422e](https://github.com/TogetherWeOwn/two-bot-next/commit/322422ed6032412e43b338b7b4f0d43d3acfa024))
* **cutover:** add command restoration rehearsal harness ([#250](https://github.com/TogetherWeOwn/two-bot-next/issues/250)) ([1e8f950](https://github.com/TogetherWeOwn/two-bot-next/commit/1e8f95008fe03a7479a0f8349dbb714f99c813fa))
* **cutover:** add guarded legacy Postgres copy tooling ([#113](https://github.com/TogetherWeOwn/two-bot-next/issues/113)) ([36da530](https://github.com/TogetherWeOwn/two-bot-next/commit/36da530bcffae14114e1a2138a00289883a01315))
* **cutover:** add on-demand rules-gate stuck report query ([#180](https://github.com/TogetherWeOwn/two-bot-next/issues/180)) ([3a571ab](https://github.com/TogetherWeOwn/two-bot-next/commit/3a571ab171cdbe0afdf2fc99b34d782b6cd9200d))
* **cutover:** add read-only legacy data verification ([#97](https://github.com/TogetherWeOwn/two-bot-next/issues/97)) ([f1e7ab6](https://github.com/TogetherWeOwn/two-bot-next/commit/f1e7ab63286c9d39275cea025c21f912612d5f98))
* **cutover:** add read-only Next-window delta report ([#261](https://github.com/TogetherWeOwn/two-bot-next/issues/261)) ([d055861](https://github.com/TogetherWeOwn/two-bot-next/commit/d055861370ea11ba13c441b4db2f1db54d3bf784))
* **cutover:** add read-only preconditions checker ([#184](https://github.com/TogetherWeOwn/two-bot-next/issues/184)) ([6c7a61f](https://github.com/TogetherWeOwn/two-bot-next/commit/6c7a61f3ec11b732f7e336ab940efadb6695a910))
* **cutover:** add read-only rollback-readiness probe ([#300](https://github.com/TogetherWeOwn/two-bot-next/issues/300)) ([8166037](https://github.com/TogetherWeOwn/two-bot-next/commit/816603768a736e33117caff21f289a24d6fa754c))
* **cutover:** add read-only voice-ghosts ghost-channel count ([#437](https://github.com/TogetherWeOwn/two-bot-next/issues/437)) ([2cca63f](https://github.com/TogetherWeOwn/two-bot-next/commit/2cca63f5dd47ccdae12bb0a48d145a1eeca3c889))
* **cutover:** add read-only voice-reconcile and leave-gap report CLI ([#334](https://github.com/TogetherWeOwn/two-bot-next/issues/334)) ([0860a4c](https://github.com/TogetherWeOwn/two-bot-next/commit/0860a4c7cd28cf45867251e14eb4736be961577a))
* **cutover:** add staging freeze drill harness with timings ([#519](https://github.com/TogetherWeOwn/two-bot-next/issues/519)) ([50ac790](https://github.com/TogetherWeOwn/two-bot-next/commit/50ac790f758e2df63152abc0284e9243d3bf11b2))
* **cutover:** add staging-only SQLx migration runner ([#169](https://github.com/TogetherWeOwn/two-bot-next/issues/169)) ([f6ce78b](https://github.com/TogetherWeOwn/two-bot-next/commit/f6ce78b0cd6b64d1f4cd13752b521cceef2dd121))
* **cutover:** on-demand reengagement list CLI (never scheduled) ([#236](https://github.com/TogetherWeOwn/two-bot-next/issues/236)) ([c57dc8d](https://github.com/TogetherWeOwn/two-bot-next/commit/c57dc8d64689b52563599e023e075b9e6834ca32))
* **cutover:** paginate archived-thread discovery beyond first page ([#189](https://github.com/TogetherWeOwn/two-bot-next/issues/189)) ([71bba75](https://github.com/TogetherWeOwn/two-bot-next/commit/71bba7564fcf5c794486bb954f1442457f3798c3))
* **db:** bind staging apply to reviewed plan manifest hash ([#430](https://github.com/TogetherWeOwn/two-bot-next/issues/430)) ([5d876c0](https://github.com/TogetherWeOwn/two-bot-next/commit/5d876c0b759529bedf6c5a99baa03dc74d27a911))
* **db:** bootstrap role-plan phase with ephemeral membership ([#391](https://github.com/TogetherWeOwn/two-bot-next/issues/391)) ([7a99911](https://github.com/TogetherWeOwn/two-bot-next/commit/7a999118bcb160f208f4c294710ef3a0ad22b4d5))
* **db:** read-only plan group plus plan/apply job split ([#408](https://github.com/TogetherWeOwn/two-bot-next/issues/408)) ([f7863c8](https://github.com/TogetherWeOwn/two-bot-next/commit/f7863c8cf51bdcc2bfb225e25ea38ceb95cbae5e))
* **discord:** add guild command registry diff and publish ([#110](https://github.com/TogetherWeOwn/two-bot-next/issues/110)) ([6124481](https://github.com/TogetherWeOwn/two-bot-next/commit/6124481f2f7806dce9c23c6e96602893587a1313))
* **discord:** enforce durable token-wide send admission ([#117](https://github.com/TogetherWeOwn/two-bot-next/issues/117)) ([b3fbbd0](https://github.com/TogetherWeOwn/two-bot-next/commit/b3fbbd0bede7b862c9c49d13acd98118a45f8dfe))
* **discord:** share a 429 cooldown governor across announcement executors ([#252](https://github.com/TogetherWeOwn/two-bot-next/issues/252)) ([79cb56d](https://github.com/TogetherWeOwn/two-bot-next/commit/79cb56d5f3218f46625cf81088232499096a0fdd))
* **events:** execute internal scheduled-event actions ([#98](https://github.com/TogetherWeOwn/two-bot-next/issues/98)) ([bc66198](https://github.com/TogetherWeOwn/two-bot-next/commit/bc66198cebc1818ec997646209f9c5560ca85cf3))
* **evidence:** name soak and alert packets after their ruleId ([#225](https://github.com/TogetherWeOwn/two-bot-next/issues/225)) ([7917950](https://github.com/TogetherWeOwn/two-bot-next/commit/79179504387c7e98e4561e42185ac4466a07d83b))
* **evidence:** reconcile expected vs committed soak events ([#204](https://github.com/TogetherWeOwn/two-bot-next/issues/204)) ([44b0896](https://github.com/TogetherWeOwn/two-bot-next/commit/44b08960f40250f8f8c28b9bf76dbed85b1eb2dd))
* **feeds:** add pinned-address HTTPS connector for feed fetching ([#115](https://github.com/TogetherWeOwn/two-bot-next/issues/115)) ([048ca7f](https://github.com/TogetherWeOwn/two-bot-next/commit/048ca7f6ca9ef83c225e2da436db74e112a05fbf))
* **feeds:** manage polling and reconcile uncertain deliveries ([#120](https://github.com/TogetherWeOwn/two-bot-next/issues/120)) ([b427ed0](https://github.com/TogetherWeOwn/two-bot-next/commit/b427ed0a25a777da62d2c2c4f54fccaf47473721))
* **feeds:** port domain, SSRF fetch policy and delivery claims ([#47](https://github.com/TogetherWeOwn/two-bot-next/issues/47)) ([d0c9e60](https://github.com/TogetherWeOwn/two-bot-next/commit/d0c9e60f1024b62accddf508e749f211999300d7))
* **feeds:** wire feed commands through the shared command runtime ([#116](https://github.com/TogetherWeOwn/two-bot-next/issues/116)) ([54f9e89](https://github.com/TogetherWeOwn/two-bot-next/commit/54f9e89c4a8c45e90111793b4f8713d466e591fb))
* **gateway:** one-shot force-fresh IDENTIFY for first production boot ([#209](https://github.com/TogetherWeOwn/two-bot-next/issues/209)) ([b334647](https://github.com/TogetherWeOwn/two-bot-next/commit/b33464729da01d5245e8a9e9792a2b98aaac28b7))
* **interactions:** auto-defer handlers and redact error replies ([#90](https://github.com/TogetherWeOwn/two-bot-next/issues/90)) ([ecfcb2b](https://github.com/TogetherWeOwn/two-bot-next/commit/ecfcb2b84395e6d69c7b923f89d4e1e2f48a5b79))
* **internal-actions:** add private receiver configuration ([#114](https://github.com/TogetherWeOwn/two-bot-next/issues/114)) ([83e972f](https://github.com/TogetherWeOwn/two-bot-next/commit/83e972f9050bb504b5d87af4d69fbe2d2fd034c3))
* **internal-actions:** add private staging announcement receiver ([#343](https://github.com/TogetherWeOwn/two-bot-next/issues/343)) ([7e4cc3b](https://github.com/TogetherWeOwn/two-bot-next/commit/7e4cc3b6086886dc0f563ebd9176ab4a852524b7))
* **internal-actions:** execute automations import and export ([#438](https://github.com/TogetherWeOwn/two-bot-next/issues/438)) ([5e5b40d](https://github.com/TogetherWeOwn/two-bot-next/commit/5e5b40d111c1adcd6094aa9ea8b03037f20f7780))
* **internal-actions:** execute member join and role assignment ([#93](https://github.com/TogetherWeOwn/two-bot-next/issues/93)) ([908ff3a](https://github.com/TogetherWeOwn/two-bot-next/commit/908ff3af5598bc6b8a6e64e702c47d24ce073bcb))
* **internal-actions:** execute validated announcements once ([#64](https://github.com/TogetherWeOwn/two-bot-next/issues/64)) ([9d06001](https://github.com/TogetherWeOwn/two-bot-next/commit/9d060016fead3de1f6e41dc2c6d074efad0a091b))
* **internal-actions:** execute versioned guild settings commands ([#81](https://github.com/TogetherWeOwn/two-bot-next/issues/81)) ([4788d80](https://github.com/TogetherWeOwn/two-bot-next/commit/4788d80a947beb553b481d8227619ba5d37209e8))
* **invites:** bound snapshot staleness and pin vanity/unknown attribution ([#164](https://github.com/TogetherWeOwn/two-bot-next/issues/164)) ([7c060d2](https://github.com/TogetherWeOwn/two-bot-next/commit/7c060d2be626c888aa03ef3129144317b20fba2a))
* **jobs:** poll guild_settings and publish live snapshots ([#118](https://github.com/TogetherWeOwn/two-bot-next/issues/118)) ([c0894fe](https://github.com/TogetherWeOwn/two-bot-next/commit/c0894feaccde2274898dd9d4a4f65fc8d7a47624))
* **jobs:** supervise presence probe, weekly scorecard and inactivity sweep ([#119](https://github.com/TogetherWeOwn/two-bot-next/issues/119)) ([33594fc](https://github.com/TogetherWeOwn/two-bot-next/commit/33594fca024e31f65073cbc56e58a80beca9df58))
* **leveling:** integrate ordered gateway awards and shared Discord effects ([#68](https://github.com/TogetherWeOwn/two-bot-next/issues/68)) ([2cd77f4](https://github.com/TogetherWeOwn/two-bot-next/commit/2cd77f42f6236bc82a9895faed44082b83820a0d))
* **lfg:** wire commands and selects through shared runtime ([#111](https://github.com/TogetherWeOwn/two-bot-next/issues/111)) ([055f1b8](https://github.com/TogetherWeOwn/two-bot-next/commit/055f1b85964f5ec3324a361b9fbdf5c588ddf8ed))
* **metrics:** add authenticated /ops/metrics scrape path and alert rules ([#197](https://github.com/TogetherWeOwn/two-bot-next/issues/197)) ([df0e6ac](https://github.com/TogetherWeOwn/two-bot-next/commit/df0e6ac92e188c82abb0d45963b17a4994ce7f26))
* **metrics:** add DB error counter and send-admission series plus paging rules ([#442](https://github.com/TogetherWeOwn/two-bot-next/issues/442)) ([7c524fb](https://github.com/TogetherWeOwn/two-bot-next/commit/7c524fb17bb4512872d9e8ac84acf86ec7c54d52))
* **metrics:** expose bounded internal health metrics ([#82](https://github.com/TogetherWeOwn/two-bot-next/issues/82)) ([c88ee15](https://github.com/TogetherWeOwn/two-bot-next/commit/c88ee15872e32e720ac74fd3692b5d1557207ad2))
* **metrics:** record supervised job outcomes ([#123](https://github.com/TogetherWeOwn/two-bot-next/issues/123)) ([8546447](https://github.com/TogetherWeOwn/two-bot-next/commit/854644763b037e99f1cf5fe14919bc79ac94bc84))
* **moderation:** enforce duration caps and schedule/sticky bounds ([#165](https://github.com/TogetherWeOwn/two-bot-next/issues/165)) ([5ddab2d](https://github.com/TogetherWeOwn/two-bot-next/commit/5ddab2dfec52853a0c87cc6e555baf6575eae77e))
* **moderation:** execute internal channel actions with durable fences ([#106](https://github.com/TogetherWeOwn/two-bot-next/issues/106)) ([fed8311](https://github.com/TogetherWeOwn/two-bot-next/commit/fed83113fb1a0c0ca6f75a47213370497ea60c48))
* **moderation:** execute internal member actions with shared ledger ([#397](https://github.com/TogetherWeOwn/two-bot-next/issues/397)) ([3c9f380](https://github.com/TogetherWeOwn/two-bot-next/commit/3c9f380334cd877133f579bae3746a269063767d))
* **moderation:** port member actions and durable unban ledger ([#30](https://github.com/TogetherWeOwn/two-bot-next/issues/30)) ([08ff236](https://github.com/TogetherWeOwn/two-bot-next/commit/08ff2362767ecdeefe1a8f0a3e1d2ea6169a85de))
* **moderation:** refuse disable while releases are still owed ([#396](https://github.com/TogetherWeOwn/two-bot-next/issues/396)) ([285f0fc](https://github.com/TogetherWeOwn/two-bot-next/commit/285f0fc7cfbd31cf19c2bfd0559e0a7c6935558b))
* **moderation:** wire channel commands through the shared runtime ([#143](https://github.com/TogetherWeOwn/two-bot-next/issues/143)) ([9d81f59](https://github.com/TogetherWeOwn/two-bot-next/commit/9d81f59499bf4386e770c2c3cb4d9b5430580cbf))
* **observability:** enable production Workers Logs per environment ([#239](https://github.com/TogetherWeOwn/two-bot-next/issues/239)) ([d9ead4c](https://github.com/TogetherWeOwn/two-bot-next/commit/d9ead4c17c6900ec632692dbf9955441c68aa882))
* **onboarding:** wire shared gateway and picker runtime ([#65](https://github.com/TogetherWeOwn/two-bot-next/issues/65)) ([5247987](https://github.com/TogetherWeOwn/two-bot-next/commit/5247987ac49a96a2981f301e62f87f614b4cd1fb))
* **preflight:** add read-only pre-deploy checks ([#109](https://github.com/TogetherWeOwn/two-bot-next/issues/109)) ([a25017e](https://github.com/TogetherWeOwn/two-bot-next/commit/a25017e837456efef864311bab6f75f6d9b9ffc3))
* **presence:** skip overlapping probe cycles with a stale-lease guard ([#237](https://github.com/TogetherWeOwn/two-bot-next/issues/237)) ([69d0bc9](https://github.com/TogetherWeOwn/two-bot-next/commit/69d0bc94c0dd807ba653a72c4183913724303917))
* **privacy:** add guild-scoped member erasure ([#131](https://github.com/TogetherWeOwn/two-bot-next/issues/131)) ([72c1675](https://github.com/TogetherWeOwn/two-bot-next/commit/72c16757ab2e4fea9c963de8638ddff65c4b7701))
* **probes:** add read-only cutover acceptance probes ([#468](https://github.com/TogetherWeOwn/two-bot-next/issues/468)) ([cd32d03](https://github.com/TogetherWeOwn/two-bot-next/commit/cd32d03a51130c8c7db12e4b4ea38fb55df1a1dc))
* **raid:** add durable join-risk event claim store ([#362](https://github.com/TogetherWeOwn/two-bot-next/issues/362)) ([6969848](https://github.com/TogetherWeOwn/two-bot-next/commit/696984828fb53b05072e1bb6f27e13409c62ca7f))
* **raid:** add guarded cohort export and audited removal ([#108](https://github.com/TogetherWeOwn/two-bot-next/issues/108)) ([9d86a27](https://github.com/TogetherWeOwn/two-bot-next/commit/9d86a2747d3d3037f577be79f62121e232efde24))
* **raid:** deliver join-risk flags behind anti-nuke fences ([#383](https://github.com/TogetherWeOwn/two-bot-next/issues/383)) ([42973af](https://github.com/TogetherWeOwn/two-bot-next/commit/42973af7fa30bf0ab360f81de26784cd736739ea))
* **raid:** wire join-burst watch to the gateway and shared executor ([#377](https://github.com/TogetherWeOwn/two-bot-next/issues/377)) ([3cc3855](https://github.com/TogetherWeOwn/two-bot-next/commit/3cc385541292d47559a12ce14c9be6b56ff3291b))
* **redirect:** bounded short miss cache for campaign lookups ([#331](https://github.com/TogetherWeOwn/two-bot-next/issues/331)) ([5dd4e60](https://github.com/TogetherWeOwn/two-bot-next/commit/5dd4e60e41a2f3771e1c221c35f2dc9584851699))
* **redirect:** connect the click store through an optional REDIRECT_DB binding ([#238](https://github.com/TogetherWeOwn/two-bot-next/issues/238)) ([30c0d0b](https://github.com/TogetherWeOwn/two-bot-next/commit/30c0d0be66b4429bb4b16a05343b4760344a8a73))
* **rejection-telemetry:** add bounded scalar rejection core ([#218](https://github.com/TogetherWeOwn/two-bot-next/issues/218)) ([4135e23](https://github.com/TogetherWeOwn/two-bot-next/commit/4135e232cec4c0f2ae9febe991284787fd9aa26f))
* **scheduled:** run the 15 s scheduled-message ticker on the job supervisor ([#256](https://github.com/TogetherWeOwn/two-bot-next/issues/256)) ([f014149](https://github.com/TogetherWeOwn/two-bot-next/commit/f014149061b60cb613d21538a24ea7b5c59559f0))
* **schedule:** scheduled-message domain, store and ticker-port migration ([#31](https://github.com/TogetherWeOwn/two-bot-next/issues/31)) ([578c4e9](https://github.com/TogetherWeOwn/two-bot-next/commit/578c4e9549772687de6c574f349023e9d1b92ac2))
* **schedule:** wire schedule commands through shared command runtime ([#262](https://github.com/TogetherWeOwn/two-bot-next/issues/262)) ([814b1e0](https://github.com/TogetherWeOwn/two-bot-next/commit/814b1e0cc149aff61181bbc6c8c081df4fcabbf6))
* **self-roles:** port role plans and fenced claim storage ([#43](https://github.com/TogetherWeOwn/two-bot-next/issues/43)) ([8cc6edf](https://github.com/TogetherWeOwn/two-bot-next/commit/8cc6edf70188ea0f6f0f0752af27a60c6431283b))
* **self-roles:** wire claimed panel changes through the shared runtime ([#137](https://github.com/TogetherWeOwn/two-bot-next/issues/137)) ([c765421](https://github.com/TogetherWeOwn/two-bot-next/commit/c765421e878920de0b9d89488dc02d497c70e345))
* **shutdown:** configurable drain deadline and second-signal exit ([#85](https://github.com/TogetherWeOwn/two-bot-next/issues/85)) ([a183982](https://github.com/TogetherWeOwn/two-bot-next/commit/a1839828f6c0ea8a0ec8d405f5010665f1320736))
* **soak:** add offline gateway-event-log to soak-summary parser ([#469](https://github.com/TogetherWeOwn/two-bot-next/issues/469)) ([505f49b](https://github.com/TogetherWeOwn/two-bot-next/commit/505f49bad119ab70b110cf5d11239d1e45f5fa7a))
* **sticky:** wire commands and gateway re-post through shared runtime ([#66](https://github.com/TogetherWeOwn/two-bot-next/issues/66)) ([100dde2](https://github.com/TogetherWeOwn/two-bot-next/commit/100dde2bf58dd088ea140515cf1aaf9341738ccb))
* **store:** add durable rollback journal and watermarks ([#234](https://github.com/TogetherWeOwn/two-bot-next/issues/234)) ([191a174](https://github.com/TogetherWeOwn/two-bot-next/commit/191a174b670a940953740b9f591ed96746cb2f58))
* **store:** persist funnel runtime with sqlx and database readiness ([#45](https://github.com/TogetherWeOwn/two-bot-next/issues/45)) ([1dc0889](https://github.com/TogetherWeOwn/two-bot-next/commit/1dc0889f06d3a0beb58193b3c8db1acd4d89f713))
* **store:** ship invite_campaigns target migration for REDIRECT_DB click store ([#281](https://github.com/TogetherWeOwn/two-bot-next/issues/281)) ([6d9cf71](https://github.com/TogetherWeOwn/two-bot-next/commit/6d9cf7106e2b703664cb67295584f6a102204982))
* **tickets:** add lifecycle domain and durable transcript store ([#27](https://github.com/TogetherWeOwn/two-bot-next/issues/27)) ([a7ae704](https://github.com/TogetherWeOwn/two-bot-next/commit/a7ae704bbfc787c34d8954a0fe38cc4f772b91aa))
* **tickets:** integrate lifecycle runtime with shared router and executor ([#155](https://github.com/TogetherWeOwn/two-bot-next/issues/155)) ([f456ed2](https://github.com/TogetherWeOwn/two-bot-next/commit/f456ed22259b5c6beb410d473bb9fdc62b06458c))
* **voice:** add /access to set guild room controls ([#368](https://github.com/TogetherWeOwn/two-bot-next/issues/368)) ([f08a5da](https://github.com/TogetherWeOwn/two-bot-next/commit/f08a5da04df13190c42a34a5309c213a4d5d669f))
* **voice:** add /logging to set guild notice level, channel and mention role ([#372](https://github.com/TogetherWeOwn/two-bot-next/issues/372)) ([64848f6](https://github.com/TogetherWeOwn/two-bot-next/commit/64848f6d09d246108202835c6aa41895be32d545))
* **voice:** add durable room lifecycle foundation ([#42](https://github.com/TogetherWeOwn/two-bot-next/issues/42)) ([a0fb795](https://github.com/TogetherWeOwn/two-bot-next/commit/a0fb79576157f35779839344e77d75732b30f6f7))
* **voice:** add pure assistant-output scenario validator ([#295](https://github.com/TogetherWeOwn/two-bot-next/issues/295)) ([7ce5e2f](https://github.com/TogetherWeOwn/two-bot-next/commit/7ce5e2f4ee1e2b1737c852d9173e3ed2d6c17eeb))
* **voice:** add pure game-alias table and nick core ([#212](https://github.com/TogetherWeOwn/two-bot-next/issues/212)) ([c122126](https://github.com/TogetherWeOwn/two-bot-next/commit/c122126d678c98e171bdf92cb20d731a9529c93e))
* **voice:** add pure guild-level room command access gate ([#205](https://github.com/TogetherWeOwn/two-bot-next/issues/205)) ([97d28df](https://github.com/TogetherWeOwn/two-bot-next/commit/97d28dff03ea511376ea4d46e6506fc2a6c1f4b2))
* **voice:** add pure logging-channel resolution core ([#291](https://github.com/TogetherWeOwn/two-bot-next/issues/291)) ([70a4654](https://github.com/TogetherWeOwn/two-bot-next/commit/70a4654373090a386119e018fc094b6acd09e013))
* **voice:** add pure naming template engine ([#32](https://github.com/TogetherWeOwn/two-bot-next/issues/32)) ([91425f8](https://github.com/TogetherWeOwn/two-bot-next/commit/91425f83c666d509ac9ce1f70059023593abd789))
* **voice:** add pure ping/invite utility core ([#287](https://github.com/TogetherWeOwn/two-bot-next/issues/287)) ([f2e7a10](https://github.com/TogetherWeOwn/two-bot-next/commit/f2e7a1039afd0f3c1a9c89f99857dfc21d2031ef))
* **voice:** add pure rename-coalescer core ([#292](https://github.com/TogetherWeOwn/two-bot-next/issues/292)) ([90494bd](https://github.com/TogetherWeOwn/two-bot-next/commit/90494bd865c057fb36740d3df990f250711baccf))
* **voice:** add pure room-create admission core ([#278](https://github.com/TogetherWeOwn/two-bot-next/issues/278)) ([13513b2](https://github.com/TogetherWeOwn/two-bot-next/commit/13513b2d9d9aa0777c834657003dd6fb86037bad))
* **voice:** add pure room-name sanitize and automod filter core ([#280](https://github.com/TogetherWeOwn/two-bot-next/issues/280)) ([dda076d](https://github.com/TogetherWeOwn/two-bot-next/commit/dda076d90214d93fd3cf9d6668b71c83f2d840c6))
* **voice:** add pure voice component custom_id codec ([#277](https://github.com/TogetherWeOwn/two-bot-next/issues/277)) ([c57353f](https://github.com/TogetherWeOwn/two-bot-next/commit/c57353f1d87c5a7cddb070be0404ef498cca16b4))
* **voice:** add template lint and six-scenario preview core ([#247](https://github.com/TogetherWeOwn/two-bot-next/issues/247)) ([6a40560](https://github.com/TogetherWeOwn/two-bot-next/commit/6a4056014e0b098c38b8a40ad8958fd8e2b48a29))
* **voice:** add template-assistant request builder and reply parser ([#244](https://github.com/TogetherWeOwn/two-bot-next/issues/244)) ([9ae4368](https://github.com/TogetherWeOwn/two-bot-next/commit/9ae4368f0704ff5a84bb0cc2f1eb9cc2f1162f3f))
* **voice:** add vote-kick enforcement writes ([#335](https://github.com/TogetherWeOwn/two-bot-next/issues/335)) ([a27ba40](https://github.com/TogetherWeOwn/two-bot-next/commit/a27ba4030b2b1f41245eec548eccd0a4277944d5))
* **voice:** caretaker succession plus /reclaim and /transfer commands ([#348](https://github.com/TogetherWeOwn/two-bot-next/issues/348)) ([75c0215](https://github.com/TogetherWeOwn/two-bot-next/commit/75c0215ee834d0ab539c417f7a1c715cd2679bcc))
* **voice:** companion text channels, /textchannels and admin-role overwrites ([#349](https://github.com/TogetherWeOwn/two-bot-next/issues/349)) ([fbcd051](https://github.com/TogetherWeOwn/two-bot-next/commit/fbcd05148f20f890701d8954366142710fb72a16))
* **voice:** compose V6 conditionals and styling into the naming engine ([#327](https://github.com/TogetherWeOwn/two-bot-next/issues/327)) ([a1dc460](https://github.com/TogetherWeOwn/two-bot-next/commit/a1dc460c8ded662e2cba78112a3b9c14d3a381c8))
* **voice:** emit lifecycle outcome signals for cutover verification ([#414](https://github.com/TogetherWeOwn/two-bot-next/issues/414)) ([8b3a0b3](https://github.com/TogetherWeOwn/two-bot-next/commit/8b3a0b3fce2e3c6f33dd7bf2557632798fe1fd91))
* **voice:** enforce guild creation switch and room-command role gate ([#363](https://github.com/TogetherWeOwn/two-bot-next/issues/363)) ([ea8edb9](https://github.com/TogetherWeOwn/two-bot-next/commit/ea8edb9f7d5a0dac5a33cdfa7819ea7907df359a))
* **voice:** export and import commands with diff preview and confirm ([#384](https://github.com/TogetherWeOwn/two-bot-next/issues/384)) ([bad5b59](https://github.com/TogetherWeOwn/two-bot-next/commit/bad5b5992f58597c1f81a39310c2f5e029c2adb9))
* **voice:** ghost-channel reconcile and cleanup action ([#418](https://github.com/TogetherWeOwn/two-bot-next/issues/418)) ([7047d0e](https://github.com/TogetherWeOwn/two-bot-next/commit/7047d0e22f0476235e4669545d497247a96452f2))
* **voice:** honor group_by_category in the room planner ([#415](https://github.com/TogetherWeOwn/two-bot-next/issues/415)) ([d3212a2](https://github.com/TogetherWeOwn/two-bot-next/commit/d3212a2688a202fd6363a2cc574f3ce160c28339))
* **voice:** list missing bot permissions in /setup, naming the category override ([#373](https://github.com/TogetherWeOwn/two-bot-next/issues/373)) ([b6f4314](https://github.com/TogetherWeOwn/two-bot-next/commit/b6f4314a857aa910c8640a5d8c50fea234909aa3))
* **voice:** persist configuration snapshot and transactional apply ([#364](https://github.com/TogetherWeOwn/two-bot-next/issues/364)) ([b3d66e7](https://github.com/TogetherWeOwn/two-bot-next/commit/b3d66e7c947008ab9cbdfe393cb2003a43c8de4f))
* **voice:** persist guild-level room access controls ([#360](https://github.com/TogetherWeOwn/two-bot-next/issues/360)) ([0b6ef36](https://github.com/TogetherWeOwn/two-bot-next/commit/0b6ef3656edf858604dbd7a33e0f53a2a8e5fef4))
* **voice:** persist text-channel settings and companion records ([#333](https://github.com/TogetherWeOwn/two-bot-next/issues/333)) ([c26d9ca](https://github.com/TogetherWeOwn/two-bot-next/commit/c26d9ca0ae5a7e55df8486fca3f1a4c9916ba987))
* **voice:** record lifecycle REST outcomes in executor metrics ([#412](https://github.com/TogetherWeOwn/two-bot-next/issues/412)) ([60bd266](https://github.com/TogetherWeOwn/two-bot-next/commit/60bd2661129a76e87693553c677491039ff2e632))
* **voice:** run vote-kick state and enforcement in the guild worker ([#369](https://github.com/TogetherWeOwn/two-bot-next/issues/369)) ([5db961c](https://github.com/TogetherWeOwn/two-bot-next/commit/5db961ca6fbc2f55658c6aa5cb281080505ff554))
* **voice:** send voice-room error notices per the guild logging settings ([#378](https://github.com/TogetherWeOwn/two-bot-next/issues/378)) ([f0a8759](https://github.com/TogetherWeOwn/two-bot-next/commit/f0a87594cd21b0e3a29bdc6b6710a6a476fe6b12))
* **voice:** serve /ping and /invite utility commands ([#337](https://github.com/TogetherWeOwn/two-bot-next/issues/337)) ([d69f6db](https://github.com/TogetherWeOwn/two-bot-next/commit/d69f6dba81b7c5e39d60c7264bec2b7d87fc9356))
* **voice:** start vote-kicks from /kick with button ballots ([#392](https://github.com/TogetherWeOwn/two-bot-next/issues/392)) ([e2d02c6](https://github.com/TogetherWeOwn/two-bot-next/commit/e2d02c64571761f60c7add0e2ff1d562e82a788b))
* **voice:** template-assistant config gate and command shape ([#347](https://github.com/TogetherWeOwn/two-bot-next/issues/347)) ([257029c](https://github.com/TogetherWeOwn/two-bot-next/commit/257029cd6f1291975e46d98cced8bd51f8788af4))
* **voice:** template-assistant select-and-retry build pipeline ([#409](https://github.com/TogetherWeOwn/two-bot-next/issues/409)) ([234cc28](https://github.com/TogetherWeOwn/two-bot-next/commit/234cc282a1dcf7e4a8bbb24f2c72a6c7c76dda07))
* **voice:** wire naming template engine into room create/rename ([#229](https://github.com/TogetherWeOwn/two-bot-next/issues/229)) ([6da27af](https://github.com/TogetherWeOwn/two-bot-next/commit/6da27afb34a07adb126252d117160225861f81d4))
* **voice:** wire per-creator settings commands ([#419](https://github.com/TogetherWeOwn/two-bot-next/issues/419)) ([b54f3e0](https://github.com/TogetherWeOwn/two-bot-next/commit/b54f3e013841574cf65d21a818e3597fe6d10f6a))
* **voice:** wire V8 placement, permission inheritance and creator defaults ([#336](https://github.com/TogetherWeOwn/two-bot-next/issues/336)) ([c2c3a90](https://github.com/TogetherWeOwn/two-bot-next/commit/c2c3a909a95d9b776e7453b02279eb4c2139a838))
* **watch:** count gateway disconnects and missed sequence events ([#443](https://github.com/TogetherWeOwn/two-bot-next/issues/443)) ([ca3f4c7](https://github.com/TogetherWeOwn/two-bot-next/commit/ca3f4c77dd664e13a2133980dab797717770454f))
* **worker:** add voice-failure metrics alert rule ([#497](https://github.com/TogetherWeOwn/two-bot-next/issues/497)) ([17518c2](https://github.com/TogetherWeOwn/two-bot-next/commit/17518c270ea8d247d29171c9dd8ec4107b85ac2c))
* **worker:** alert on sustained Container unready status ([#74](https://github.com/TogetherWeOwn/two-bot-next/issues/74)) ([0a06482](https://github.com/TogetherWeOwn/two-bot-next/commit/0a064825d73ca2fabddc77d5726b1c2a65b1d478))
* **worker:** deep-link metrics alert packets to runbook anchors ([#223](https://github.com/TogetherWeOwn/two-bot-next/issues/223)) ([5b0fa4c](https://github.com/TogetherWeOwn/two-bot-next/commit/5b0fa4c608aae69f7e0839f1e460ea595fe04187))
* **worker:** forward reviewed TWO_* flag allowlist into the Container ([#210](https://github.com/TogetherWeOwn/two-bot-next/issues/210)) ([c0818fa](https://github.com/TogetherWeOwn/two-bot-next/commit/c0818fa0fb344e3968b733053cc77a6081fa746f))
* **wrangler:** add staging-only default-dark ingress for the internal-actions receiver ([#370](https://github.com/TogetherWeOwn/two-bot-next/issues/370)) ([0aaca93](https://github.com/TogetherWeOwn/two-bot-next/commit/0aaca93617f00b5b5a7663bf717c6b0ed5253bb3))

- Enforce the live-activation identity and capability fence at boot: derive the application id from the bot token (never config), permit every capability only for the staging guild/application pair, and restrict the live pair to the reviewed `LIVE_CLEARED_CAPABILITIES` allowlist (shipped: `self_roles` only). Refused capabilities validate no feature gates, register no commands, construct no runtime, request no privileged intents, and log one structured refusal line while the process stays up for cleared surfaces. The tickets runtime and its ticket-driven `MESSAGE_CONTENT` request land under the same default-deny rule.
- Register an immediate-first, non-overlapping feed poll job with the existing owned job supervisor, pinned HTTPS connector, shared REST executor and fenced SQLx ledger. Preserve exact string nonces (including decimal-looking values), recover pending deliveries even after items leave the feed, rotate the bounded recovery queue without starving XML-present items, and surface bounded-history misses or uncertain send/completion receipts as recovery-required rather than reposting. Announcements remain off by default; no deployment or activation is included.
- Add self-role reads and singular role operations to the shared REST executor:
  authoritative member/bot/role/channel policy snapshots, fetched reaction-message
  identity, paced single-attempt operations with pre/post ownership checks and
  retained ambiguous or accepted-but-stale exchanges. Add event-first/lane-first
  runtime admission, concurrent cancellation-safe renewal of both leases, and
  one-time fenced intent initialization (migration 0201). Recovery preserves
  intentionally empty snapshots and computes remaining work from freshly fetched
  member state. Add durable paced send journaling, fresh-policy singular execution,
  effect checkpoints, and a monotonic compensation phase (migration 0202) that
  survives restart without retrying rejected intent. Add newly leased stale-worker
  repair to the committed target, live atomic runtime settlement, and durable
  pending-exchange recovery (migration 0203) that refuses false success after an
  interrupted remote send. Add isolated Postgres/mock REST regressions for
  partial/ambiguous failures, compensation recovery, stale in-flight repair,
  selected/empty targets, expiry rollback and unresolved settlement refusal.
  Add injectable shared component/reaction dispatch with source and input-type
  validation, ephemeral defer-before-admission and final-settlement-only success
  replies. Add fenced dry-run audits without role mutations or simulated target
  publication, plus input and orchestration regressions. Add bounded configured-
  source discovery and generation-fenced recovery of expired processing audits
  without gateway redelivery, preserving initialized empty targets and unresolved
  evidence. Gate reaction dispatch through the shared router and add actual
  partial/duplicate add/remove fixtures. Add optional shared-supervisor recovery
  registration with bounded cadence/timeout and discovery I/O, per-name parked
  status, and cancellation/evidence-preservation fixtures. Add separate terminal
  supersession discovery, fresh typed evidence leases, dual-fenced repair
  journaling/completion receipts and inherited-unknown preservation (migration
  0204), with isolated source regressions. Add cancellation-owned terminal restart
  repair through the shared journaled executor to a freshly leased committed
  selected/empty target, atomic paired ownership checks and bounded fair mixed
  discovery. Preserve inherited uncertainty and terminal rejection; dry-run skips
  terminal claims. Add source fixtures for restart convergence, missing/unknown
  targets, pending evidence, cancelled renewals and mixed sweeps. Extend terminal
  fault-injection source coverage for partial repair rejection/rate-limit/received
  ambiguity/timeout, independent evidence/lane transfer during REST, and expiry
  after pacing or journal waits without false sends or uncertainty retirement.
  Route older processing audits inserted after the winning lane's bulk
  supersession into terminal discovery before prepared ownership, using a fresh
  post-lock event fence and stored scope/chronology checks. Preserve all intent,
  pending and compensation evidence without REST or winner publication; add
  early-supersession runtime/store and lock-wait source fixtures. Preserve singular
  role-response status before unused provider-body reads can fail or stall, while
  requiring complete snapshot bodies. Resolve definite new role/direction evidence
  without erasing older unknown sends or mislabeling unrelated acknowledged work;
  add partial-body, ownership-loss and inherited-evidence source regressions.
  Add distinct per-send journal tickets and idempotent response/no-send receipts
  (migration 0205) that survive generation transfer without former-worker audit
  or target authority. Pending tickets gate settlement and cannot be erased by
  aggregate checkpoint clearing. Read ticket evidence only after audit lock waits
  and preserve unresolved role/direction IDs from pending tickets. Add processing/
  terminal provenance, lock-wait and role-matrix source fixtures. Wire tickets
  into normal processing and typed terminal paced runtime steps, persisting raw
  response/no-send receipts before stale aggregate writes; retain timeout and
  cancellation uncertainty without retry. Extend runtime/executor source fixtures
  for status provenance, generation transfer, post-journal no-send and pending
  tickets. Add live-fenced current-owner receipt evidence incorporation to processing
  recovery and typed terminal repair, restoring cumulative attempts and acknowledged
  204 compensation while preserving pending/legacy uncertainty, snapshot effects,
  terminal outcome and committed target. Distinguish received ambiguous effects
  from unknown in-flight sends. Add replay, stale-fence, lock-wait and recovery
  source coverage. Add explicit legacy uncertainty baselines and current-owner
  receipt-specific retirement (migration 0206), preserving overlapping pending
  tickets and legacy work. Require live fences after receipt lock waits; received
  ambiguous effects are not no-effect verdicts or unknown sends. Protect unretired
  completed tickets and legacy floors from aggregate clearing/settlement. Wire
  processing/typed terminal retirement and post-commit claim refresh; atomically
  pin untracked stale-path journals instead of borrowing a ticket's provenance.
  Add classifier, overlap/replay, stale-fence, lock-wait and recovery source
  fixtures plus baseline role-matrix coverage. Replace lane-only stale-maintenance
  journaling with immutable-metadata handoff to the shared fresh typed terminal
  owner and newly leased committed target. Remove the maintenance alternative from
  normal processing steps; every new repair send has a distinct ticket. Preserve
  inherited pending/legacy work and former-worker refusal, without publishing a
  target or claiming success for obsolete input. Add active-owner/mismatched-hint,
  stale-cache, overlap/no-send and selected/empty convergence source fixtures.
  Document bounded unknown-work continuation and add a shared-service restart
  regression for repeated pending sweeps, durable lease backoff, other-row progress,
  one acknowledged repair, genuine sender completion and preserved legacy floors.
  Compose one boot-time self-role service, shared by gateway dispatch and the
  supervised recovery job, only for a nonempty catalogue in the pinned TWO Staging
  guild with a verified bot identity; any other guild, an empty or invalid
  catalogue or a failed identity read parks the surface and the job.
- Pure `scan_joins_for_bursts` raid replay over recorded joins (fresh watch per
  call, instant-ordered, strict RFC 3339) with the legacy historical-raid
  scenarios as integration tests. No runtime or database wiring.
- Wire leveling through the shared command runtime, interaction router and REST executor with ordered, awaited gateway awards. Preserve message eligibility, measured voice duration, session/dry-run reward suppression, ephemeral rank and mention-suppressed public top 10. Ordinary level-ups only grant roles; explicit revokes require a pinned staging fence and whole-set permission/hierarchy preflight. Mock REST and migrated disposable database proofs run in CI, including a shared-runtime single-callback regression.
- Wire LFG commands and role selects through the shared interaction runtime and REST executor, with ephemeral replies, mention-free message refresh, serialized capacity/closure, nonce recovery, failed-post cleanup and shared announcement audit outcomes.
- Add bounded component-bearing posts, member-role deltas and deferred-response edits to the shared Discord executor, with explicit onboarding mention/menu rendering and fail-closed per-guild configuration. Wire pre-update gateway capture and shared-router orchestration with live settings/permissions, guarded welcomes and post-role routing; verify through mock Discord and isolated testdb acceptance. Persist pre-pipeline welcome/goodbye jobs atomically with gateway checkpoints and bound restart attempts; retain token-free interrupted-component receipts instead of replaying uncertain role writes. Commit anchor marker/routing together and finish deferred processing errors with bounded honest replies. Recovery validation remains in progress; no live activation is claimed.
- Automod gateway decision/enrichment seams and capture-only funnel handoff,
  staging/live-approval and dry-run fences, protected-target enforcement plans,
  repeat-history expiration, and replay-safe delivery claims with the
  legacy-compatible once-per-message violation ledger. Shared executor/shard
  activation is not enabled by this slice.
- Scheduled-message domain logic, PostgreSQL store and migration, with validation, prefix-resolved removal, recurring timing and retry outcomes. Discord router/executor wiring follows separately.
- Scheduled-store integration tests run against the isolated PostgreSQL service container in CI.
- Add ticket lifecycle domain logic and guild-scoped Postgres persistence, with atomic transcript capture, 300-second cooldowns, restart-recovery plans and 90-day transcript purge. Shared-router/REST runtime wiring remains pending.
- **Self-role domain and storage:** framework-free button/select/reaction plans,
  configuration and live-role safety validation, hierarchy refusals, and
  legacy-compatible audit/panel tables (migration 0200). Shared event and
  exclusive-panel leases support renewal, expiry recovery, immutable mutation
  intent, fencing, and atomic audit/target settlement. Lease checks use database
  wall time after lock waits; recovery preserves cumulative attempted/compensated
  evidence. Generated events have cross-worker tie-breakers, and controls enforce
  UTF-16 and Discord size limits. The isolated Postgres lease regression test
  runs in CI and gates the required `check` job.
  Runtime router/REST wiring remains deferred; this does not enable Discord
  role mutations.
- Framework-free custom-command validation, template rendering, feature-gate
  decisions, accepted-message text trigger selection, command-list formatting,
  registry merge outcomes and automation audit facts.
- Postgres custom-command and shared audit persistence, migration `0130`,
  transaction-compatible capacity locking, and a credential-free database
  regression test against agent-testdb or a CI service container.
- Shared-router custom-command execution adapter with transactional management,
  deferred mention-safe replies, dynamic slash execution auditing, and serialized
  full-registry publication. Add mock REST + isolated testdb runtime fixtures;
  gateway execution is wired with bounded dispatch/checkpoint ordering.
- Explicit moderation-acceptance contract and prefix execution through the shared
  REST executor, with immutable pre-send audit reservations preventing replay of
  unknown deliveries. Preserve uncertain transport/timeout/5xx/429 outcomes as
  unresolved rather than auditing a definite failure. Add prefix failure/restart
  fixtures, dual-gated Message Content intent, and unchanged Worker forwarding of
  custom-command and automod flags. Add shared bootstrap metadata reads, cold-RESUME
  context and gateway dispatch under the existing heartbeat-safe checkpoint budget.
  Prefixes require explicit automod-disabled configuration until the ordinary
  moderation-result producer is integrated; unknown acceptance stays fail-closed.
- Wire `/feed-add`, `/feed-remove` and `/feed-list` through the same command runtime, router and REST executor as sticky commands. Defer ephemerally before guild-scoped CRUD and audit writes, preserve the invoking channel, and publish the complete gated command registry on Ready. Isolated Postgres and mock REST acceptance cover feed commands and sticky coexistence; announcements remain off by default, with no fetching, polling or relay posts.
- Wire `/sticky` and `/sticky-remove` through the shared interaction router and drive accepted-message re-posts through the shared REST executor. The runtime claims one re-post window atomically per burst, validates and records the replacement id before retiring the previous sticky (best-effort), releases the claim and audits `post_failed` on REST failure, and deletes an orphaned replacement when the claim moved on. Commands defer ephemerally before I/O and edit the original reply; refusals are limited to owned sticky commands. `TWO_AUTOMATIONS` gates both surfaces; ManageGuild is required, and `/sticky` validates a 1–2000 UTF-16 body with a 1–300 s debounce (default 5 s). Covered by isolated Postgres and mock REST acceptance tests.
- Add the audit mirror delivery service over the shared REST executor: record-before-deliver, private guild-fenced destinations, enforced nonce/mention suppression, kill-switch enforcement, crash-safe marker reconciliation and quarantine. Revalidate prepared ownership after shared transport pacing, preserve interrupted dedup adoption evidence, treat malformed history as uncertain, and run service fault-injection regressions in CI. Runtime wiring and activation remain deferred.
- Add the pinned-address HTTPS feed connector (`feeds_connector`). `fetch_feed` validates the source through `feeds_http`, resolves all A/AAAA answers once, pins them into a `PublicRequest`, and dials only those addresses through a per-request hyper/rustls client whose resolver answers only the pinned host. TLS SNI, certificate verification and the Host header keep the URL hostname; every redirect hop re-validates and re-pins under a three-hop, same-host, HTTPS-only budget, one 15 s total deadline, `Owen/1.0 (+https://two.gg)` identity, and compressed-then-decompressed `MAX_FEED_BYTES` bounds before XML parsing. Injected resolver/connector seams keep every test hermetic; the private-fixture constructor exists only under `#[cfg(test)]`.

- Add a durable channel-mutation lane that blocks different request keys after
  ambiguous outcomes, plus atomic claim completion, audit insertion and confirmed
  unlock recovery cleanup (migration 0123).
- Register channel moderation adapters on the shared interaction router and execute
  purge, slowmode, lockdown and recorded-state unlock through the shared REST executor.
  Verify gates, exact overwrite recovery, durable retry exclusion and finalization-only
  retries with mock REST and isolated PostgreSQL tests. Defer ephemerally before SQL/REST
  work, then edit the original response without mentions or effect retries.
  Compose these handlers alongside sticky and feed commands at startup and publish
  the complete gated set on READY/RESUMED. Bound asynchronous work in separate message,
  interaction and registry lanes; cancel it on shard exit without releasing uncertain
  effects. Moderation remains default-off; no live guild activation is included.

### Fixed

* **activation:** enforce token identity and live capability clearance ([#124](https://github.com/TogetherWeOwn/two-bot-next/issues/124)) ([9f0835e](https://github.com/TogetherWeOwn/two-bot-next/commit/9f0835e970a3cb04e8865fbaf8ba19d55e7f6215))
* **audit:** capture sweep logs through a global subscriber ([#284](https://github.com/TogetherWeOwn/two-bot-next/issues/284)) ([1be7367](https://github.com/TogetherWeOwn/two-bot-next/commit/1be7367c035966b04c8e7d759bedddfa7055e720))
* **automod:** match legacy repeat lookback depth ([#313](https://github.com/TogetherWeOwn/two-bot-next/issues/313)) ([1397c72](https://github.com/TogetherWeOwn/two-bot-next/commit/1397c720b4af44c6a717390427470ddcc1b6e2cd))
* **backup:** reject malformed dump record metadata ([#133](https://github.com/TogetherWeOwn/two-bot-next/issues/133)) ([40f7110](https://github.com/TogetherWeOwn/two-bot-next/commit/40f7110a15843c3bbb5da66a64cf1edc2e5435f8))
* **backup:** resume restored sequences past archived high-water marks ([#254](https://github.com/TogetherWeOwn/two-bot-next/issues/254)) ([2fe1e04](https://github.com/TogetherWeOwn/two-bot-next/commit/2fe1e0442457eeb9d19da2a3ef532e1973d29263))
* **backup:** supply sequence floors in the shipped restore drill ([#307](https://github.com/TogetherWeOwn/two-bot-next/issues/307)) ([81bc2fd](https://github.com/TogetherWeOwn/two-bot-next/commit/81bc2fd23bf5dcf69a3a3fc7a369739f69ae711d))
* **build-cache:** release lease when cargo spawn fails ([#202](https://github.com/TogetherWeOwn/two-bot-next/issues/202)) ([f87b71f](https://github.com/TogetherWeOwn/two-bot-next/commit/f87b71f5223cea4800d93081c6e77491eff1f26c))
* **cache:** recover stale leases when the slot lock is free ([#211](https://github.com/TogetherWeOwn/two-bot-next/issues/211)) ([94b4efe](https://github.com/TogetherWeOwn/two-bot-next/commit/94b4efec9e0f87f572f07f8a44fa0bc897857c50))
* **ci:** accept wrangler-action version-probe sessions in staging rollout gate ([#359](https://github.com/TogetherWeOwn/two-bot-next/issues/359)) ([59ae361](https://github.com/TogetherWeOwn/two-bot-next/commit/59ae36101b94b179b29b2319a3ae0a071f27f574))
* **ci:** accept wrangler-action version-probe sessions in the staging rollout gate ([#361](https://github.com/TogetherWeOwn/two-bot-next/issues/361)) ([e6a37e3](https://github.com/TogetherWeOwn/two-bot-next/commit/e6a37e3c951477445da4939407f28d25e6d2194a))
* **ci:** green hosted benchmark revision step and nightly doc lint ([#268](https://github.com/TogetherWeOwn/two-bot-next/issues/268)) ([3128e1a](https://github.com/TogetherWeOwn/two-bot-next/commit/3128e1a1f39d03fb546d17a7be53d719ff1988a4))
* **ci:** harden workflows and enforce source gates ([#96](https://github.com/TogetherWeOwn/two-bot-next/issues/96)) ([b0f4685](https://github.com/TogetherWeOwn/two-bot-next/commit/b0f4685148abd1d4abf66ef62d19a1c307c90a0f))
* **ci:** isolate gitleaks installation per runner ([#152](https://github.com/TogetherWeOwn/two-bot-next/issues/152)) ([a671413](https://github.com/TogetherWeOwn/two-bot-next/commit/a671413fa98b522c26c53c7032cf99426bdc6718))
* **ci:** measure container image size as the sum of uncompressed layers ([#145](https://github.com/TogetherWeOwn/two-bot-next/issues/145)) ([9e9868c](https://github.com/TogetherWeOwn/two-bot-next/commit/9e9868cf80ff4cca7036d03f2c9d2350013c378d))
* **ci:** repair the nightly ignored-test sweep and docs step ([#387](https://github.com/TogetherWeOwn/two-bot-next/issues/387)) ([1bf5191](https://github.com/TogetherWeOwn/two-bot-next/commit/1bf5191941e63a973759475ba972bec13fabb3fc))
* **ci:** report last observed stage when the staging rollout gate times out ([#367](https://github.com/TogetherWeOwn/two-bot-next/issues/367)) ([a863199](https://github.com/TogetherWeOwn/two-bot-next/commit/a863199ced3e00ba6588ad453b766a4c5cf4d330))
* **ci:** restore a runnable green check gate on hosted runners ([#263](https://github.com/TogetherWeOwn/two-bot-next/issues/263)) ([e0a15bb](https://github.com/TogetherWeOwn/two-bot-next/commit/e0a15bbea0d62f485daf2b13f17bd5f5a26ba0db))
* **ci:** scope SBOM handoff artifact to run attempt ([#426](https://github.com/TogetherWeOwn/two-bot-next/issues/426)) ([11d3f3a](https://github.com/TogetherWeOwn/two-bot-next/commit/11d3f3a2ce48a852cb2b4b491e161a2402393d27))
* **ci:** warn on case-insensitive internal ids in public head refs ([#534](https://github.com/TogetherWeOwn/two-bot-next/issues/534)) ([2c78420](https://github.com/TogetherWeOwn/two-bot-next/commit/2c78420cf41af5f38bbb1e062f475f4232872ad0))
* **cli:** make --help print usage instead of running backup or guild-config-snapshot ([#271](https://github.com/TogetherWeOwn/two-bot-next/issues/271)) ([581680c](https://github.com/TogetherWeOwn/two-bot-next/commit/581680cacb4ec7c13f144a534970bfcdf304e020))
* **commands:** build send admission for live Discord targets ([#527](https://github.com/TogetherWeOwn/two-bot-next/issues/527)) ([4c1cfa4](https://github.com/TogetherWeOwn/two-bot-next/commit/4c1cfa4162acd654544e041b245c3163dbf607a7))
* **containment:** include incident ID in staff alert ([#405](https://github.com/TogetherWeOwn/two-bot-next/issues/405)) ([9bdc4ef](https://github.com/TogetherWeOwn/two-bot-next/commit/9bdc4efdbaab87149391b8db1db1e01145569bbb))
* **core:** drop redundant rustdoc link targets in voice_channelinfo ([#386](https://github.com/TogetherWeOwn/two-bot-next/issues/386)) ([3a9517f](https://github.com/TogetherWeOwn/two-bot-next/commit/3a9517fa53f5eb36768cd789648b01fa69513fdc))
* **cutover:** bound member pagination and reject stalled cursors ([#168](https://github.com/TogetherWeOwn/two-bot-next/issues/168)) ([482feab](https://github.com/TogetherWeOwn/two-bot-next/commit/482feab09efb26a3ab6c74bd7d5e2dea37a6e15f))
* **cutover:** classify invite_campaigns in rollback-delta specs ([#324](https://github.com/TogetherWeOwn/two-bot-next/issues/324)) ([7c90a86](https://github.com/TogetherWeOwn/two-bot-next/commit/7c90a86e30a4eb77e917de8eeb073b2b4da08372))
* **cutover:** commit capture invite counters as one transaction ([#186](https://github.com/TogetherWeOwn/two-bot-next/issues/186)) ([a050a94](https://github.com/TogetherWeOwn/two-bot-next/commit/a050a9478c5cd7f4900385b2ad0b381e5e37a2d8))
* **cutover:** enforce read-only plan login ([#550](https://github.com/TogetherWeOwn/two-bot-next/issues/550)) ([3af90f4](https://github.com/TogetherWeOwn/two-bot-next/commit/3af90f47c2cc0ffa32fdbf325c7e1e47776a3be9))
* **cutover:** fence event dedupe to an explicit guild ([#142](https://github.com/TogetherWeOwn/two-bot-next/issues/142)) ([5fa9d3c](https://github.com/TogetherWeOwn/two-bot-next/commit/5fa9d3c049d615df15b58fb3c9867ac627261b96))
* **cutover:** keep MEE6 inventory and dry-run from running migrations ([#157](https://github.com/TogetherWeOwn/two-bot-next/issues/157)) ([f4f7823](https://github.com/TogetherWeOwn/two-bot-next/commit/f4f7823ee82022e1f1a5ace7bf4698874e913026))
* **cutover:** make settings_db test schema names unique per test ([#173](https://github.com/TogetherWeOwn/two-bot-next/issues/173)) ([3844216](https://github.com/TogetherWeOwn/two-bot-next/commit/3844216e5503cbd8363471e7bff743be98ed081e))
* **cutover:** preserve Unicode literals in MEE6 template translation ([#171](https://github.com/TogetherWeOwn/two-bot-next/issues/171)) ([a99b59b](https://github.com/TogetherWeOwn/two-bot-next/commit/a99b59b6eccd9f3c0a8a663810164598e20eac86))
* **cutover:** preserve UTF-8 boundaries in leave-attribution parsing ([#176](https://github.com/TogetherWeOwn/two-bot-next/issues/176)) ([b86b6c6](https://github.com/TogetherWeOwn/two-bot-next/commit/b86b6c64bc41b9777de13114cb68915c74ec929b))
* **cutover:** publish staging apply claims before approval ([#566](https://github.com/TogetherWeOwn/two-bot-next/issues/566)) ([bdb71ac](https://github.com/TogetherWeOwn/two-bot-next/commit/bdb71ac9a1b5eaeb764fd7eba656f97a0cdfb25c))
* **cutover:** reject everyone roles in MEE6 reward preflight ([#153](https://github.com/TogetherWeOwn/two-bot-next/issues/153)) ([4cbcde0](https://github.com/TogetherWeOwn/two-bot-next/commit/4cbcde05220fbac6d90f162ee9a4f06004447daf))
* **cutover:** reject valued and ambiguous safety opt-ins ([#140](https://github.com/TogetherWeOwn/two-bot-next/issues/140)) ([c0206db](https://github.com/TogetherWeOwn/two-bot-next/commit/c0206db38a70b38ef9ab5535bc998fb52e5948f6))
* **cutover:** repair multi-width verify decode and add end-to-end rehearsal ([#216](https://github.com/TogetherWeOwn/two-bot-next/issues/216)) ([a73d327](https://github.com/TogetherWeOwn/two-bot-next/commit/a73d327922715a5c1ce04ffee846e3d1c2de98c2))
* **cutover:** report interrupted history scans as incomplete ([#175](https://github.com/TogetherWeOwn/two-bot-next/issues/175)) ([b82e76d](https://github.com/TogetherWeOwn/two-bot-next/commit/b82e76d929592ad9a13ea5eecb5d5af6b6215e33))
* **cutover:** run the staging plan as the read-only role ([#530](https://github.com/TogetherWeOwn/two-bot-next/issues/530)) ([3d1e2dd](https://github.com/TogetherWeOwn/two-bot-next/commit/3d1e2ddd59dd0b1787eee6141e502f596ffa7b1e))
* **cutover:** validate MEE6 XP ceiling before level evaluation ([#172](https://github.com/TogetherWeOwn/two-bot-next/issues/172)) ([8d87fc5](https://github.com/TogetherWeOwn/two-bot-next/commit/8d87fc55d77e9f184d4e9ddd409882a831bd5684))
* **cutover:** validate reward levels and role uniqueness before DB access ([#158](https://github.com/TogetherWeOwn/two-bot-next/issues/158)) ([f2da7da](https://github.com/TogetherWeOwn/two-bot-next/commit/f2da7da10eed1f8afa1c9e776e57f9bc0c410812))
* **db-roles:** complete matrix and enumerate migrations for coverage ([#389](https://github.com/TogetherWeOwn/two-bot-next/issues/389)) ([6a74563](https://github.com/TogetherWeOwn/two-bot-next/commit/6a7456345cb512cd29090b4693740adee7b99e14))
* **db:** fence remaining Postgres connect paths with TLS policy ([#330](https://github.com/TogetherWeOwn/two-bot-next/issues/330)) ([ed105cf](https://github.com/TogetherWeOwn/two-bot-next/commit/ed105cfdaf8c317734e3e4dea21649b73c3c3980))
* **db:** prove staging apply hash came from the named plan run ([#528](https://github.com/TogetherWeOwn/two-bot-next/issues/528)) ([6d66260](https://github.com/TogetherWeOwn/two-bot-next/commit/6d662604aedbcee8389c62813d5e9fa791f64482))
* **db:** provision least-privilege roles and verify grants ([#102](https://github.com/TogetherWeOwn/two-bot-next/issues/102)) ([f7d86b3](https://github.com/TogetherWeOwn/two-bot-next/commit/f7d86b34bc2a6f73902ad10ba6c5febd081ed3f8))
* **db:** require authenticated TLS for remote Postgres URLs ([#245](https://github.com/TogetherWeOwn/two-bot-next/issues/245)) ([a6bc262](https://github.com/TogetherWeOwn/two-bot-next/commit/a6bc262c27f4eb2b596b06540247b6a323b922b8))
* **db:** runbook env names and sixth read-only drift case ([#410](https://github.com/TogetherWeOwn/two-bot-next/issues/410)) ([35ec6bc](https://github.com/TogetherWeOwn/two-bot-next/commit/35ec6bcddf5ac74dc66176fca2aa58b678e17e21))
* **deploy:** retry staging ownership takeover across version propagation ([#521](https://github.com/TogetherWeOwn/two-bot-next/issues/521)) ([e64a3d6](https://github.com/TogetherWeOwn/two-bot-next/commit/e64a3d67db4658b6fe02249bd2ca972a6d357c1b))
* **deploy:** tolerate a trailing application image listing in staging verify ([#529](https://github.com/TogetherWeOwn/two-bot-next/issues/529)) ([695a644](https://github.com/TogetherWeOwn/two-bot-next/commit/695a6441c1590c9ade94908e9f5bc85f19506ba3))
* **deploy:** tolerate completed-rollout active-counter lag in staging verify ([#525](https://github.com/TogetherWeOwn/two-bot-next/issues/525)) ([0bbbf84](https://github.com/TogetherWeOwn/two-bot-next/commit/0bbbf8401e84000803f03333e3418e062b0b0a01))
* **discord:** bound outbound text and suppress injected mentions ([#80](https://github.com/TogetherWeOwn/two-bot-next/issues/80)) ([8af9b23](https://github.com/TogetherWeOwn/two-bot-next/commit/8af9b23be40935b97cf29512be3aa45a8b368858))
* **discord:** map stalled body timeout to wire Timeout ([#306](https://github.com/TogetherWeOwn/two-bot-next/issues/306)) ([7b2da29](https://github.com/TogetherWeOwn/two-bot-next/commit/7b2da294107296f85a4558528eb21af7c39580f0))
* **docs:** clear the rustdoc errors that fail the nightly docs job ([#533](https://github.com/TogetherWeOwn/two-bot-next/issues/533)) ([94ae748](https://github.com/TogetherWeOwn/two-bot-next/commit/94ae7483da50139d79d50a249b5d382f0efed17a))
* **http:** redact webhook tokens from request trace spans ([#450](https://github.com/TogetherWeOwn/two-bot-next/issues/450)) ([8d9f71f](https://github.com/TogetherWeOwn/two-bot-next/commit/8d9f71f73f176ba329752566ff59dd789ced8fb4))
* **interactions:** actionable denied-path copy with next steps ([#425](https://github.com/TogetherWeOwn/two-bot-next/issues/425)) ([96e700d](https://github.com/TogetherWeOwn/two-bot-next/commit/96e700da5377dee674ecd890014a6103b965eec2))
* **internal-actions:** fail closed on clock rollback ([#215](https://github.com/TogetherWeOwn/two-bot-next/issues/215)) ([9192328](https://github.com/TogetherWeOwn/two-bot-next/commit/9192328aac8ced619ad02e15e0952dfa1a5b55c9))
* **internal-actions:** refuse duplicate key ids, reused secrets and duplicate JSON keys ([#251](https://github.com/TogetherWeOwn/two-bot-next/issues/251)) ([27e752e](https://github.com/TogetherWeOwn/two-bot-next/commit/27e752e4ceaf2a3bd5a1b861af428425569b4872))
* **internal-actions:** threat-model Worker and pin auth guards ([#100](https://github.com/TogetherWeOwn/two-bot-next/issues/100)) ([b3fbf07](https://github.com/TogetherWeOwn/two-bot-next/commit/b3fbf0701b44ab84d925fcec9a1532947490f45d))
* **jobs:** persist bounded Monday scorecard retries ([#126](https://github.com/TogetherWeOwn/two-bot-next/issues/126)) ([ab78714](https://github.com/TogetherWeOwn/two-bot-next/commit/ab78714b10fc389aae57946ac2355ac632e3d4eb))
* **leveling:** show members without XP rows as unranked ([#121](https://github.com/TogetherWeOwn/two-bot-next/issues/121)) ([fa6bb02](https://github.com/TogetherWeOwn/two-bot-next/commit/fa6bb02699c48bbc010f268a7ee335ce3f2cd54f))
* **lfg:** refuse reserved leave-action role key in role specs ([#260](https://github.com/TogetherWeOwn/two-bot-next/issues/260)) ([bf845f1](https://github.com/TogetherWeOwn/two-bot-next/commit/bf845f103f1632394060e9007b7b74b71ef0a701))
* **message-safety:** pin mention refusals and webhook redaction ([#185](https://github.com/TogetherWeOwn/two-bot-next/issues/185)) ([50e6d71](https://github.com/TogetherWeOwn/two-bot-next/commit/50e6d71143202e04b35d658b6fe0b1200062e468))
* **metrics:** build SharedState in the scrape-contract test ([#192](https://github.com/TogetherWeOwn/two-bot-next/issues/192)) ([6418cac](https://github.com/TogetherWeOwn/two-bot-next/commit/6418cac233a1650eeae2184a2077135db658eac0))
* **moderation:** keep signed audit reasons within the outbound limit ([#170](https://github.com/TogetherWeOwn/two-bot-next/issues/170)) ([8189cc4](https://github.com/TogetherWeOwn/two-bot-next/commit/8189cc410c69af0f2d46c8cfd3fed2833b65cc11))
* **onboarding:** retain valid picks and withhold hidden fallbacks ([#128](https://github.com/TogetherWeOwn/two-bot-next/issues/128)) ([66ab305](https://github.com/TogetherWeOwn/two-bot-next/commit/66ab305bc0e30f1473dcfb0355cbde625929dca4))
* **permissions:** centralize runtime command authorization ([#88](https://github.com/TogetherWeOwn/two-bot-next/issues/88)) ([bbddaca](https://github.com/TogetherWeOwn/two-bot-next/commit/bbddacaebcee39194430fe3e62d9a0c55531f424))
* **preflight:** validate ticket staff-role and category ([#181](https://github.com/TogetherWeOwn/two-bot-next/issues/181)) ([1c6481b](https://github.com/TogetherWeOwn/two-bot-next/commit/1c6481b6ff04e8dc8532239fc813e4b214d274e1))
* **presence:** cap bot-floor scans and persist truncation ([#125](https://github.com/TogetherWeOwn/two-bot-next/issues/125)) ([49635c3](https://github.com/TogetherWeOwn/two-bot-next/commit/49635c3b5c140fe113e63c31771e9de70660001b))
* **privacy:** erase automod delivery-claim author IDs behind a replay guard ([#267](https://github.com/TogetherWeOwn/two-bot-next/issues/267)) ([b6c6b68](https://github.com/TogetherWeOwn/two-bot-next/commit/b6c6b68a7b11a67dc765f3114f587bdedb416fe3))
* **quota:** canonical caller identity, unknown budget, terminal 429 hold ([#290](https://github.com/TogetherWeOwn/two-bot-next/issues/290)) ([36816fb](https://github.com/TogetherWeOwn/two-bot-next/commit/36816fba9f254c70cc459dda5a09a530dd6cfed3))
* **redirect:** bound redirectErrorClass to a fixed vocabulary ([#346](https://github.com/TogetherWeOwn/two-bot-next/issues/346)) ([80772ee](https://github.com/TogetherWeOwn/two-bot-next/commit/80772eea3acbf773d6fb8a199a032845f56e0d87))
* **redirect:** cache missing slugs with a bounded short TTL ([#122](https://github.com/TogetherWeOwn/two-bot-next/issues/122)) ([4a72d16](https://github.com/TogetherWeOwn/two-bot-next/commit/4a72d16cc6643fcd5e19d92b8ecb51e4d7e3dddd))
* **redirect:** expire idle token buckets with bounded sweep and cardinality cap ([#177](https://github.com/TogetherWeOwn/two-bot-next/issues/177)) ([49fbbe1](https://github.com/TogetherWeOwn/two-bot-next/commit/49fbbe140e3b17f8eea607ce0f8095b885de954a))
* **redirect:** forward real hold retry-after on redirect 429 ([#296](https://github.com/TogetherWeOwn/two-bot-next/issues/296)) ([9dc2dd6](https://github.com/TogetherWeOwn/two-bot-next/commit/9dc2dd628a107471f59f2119d8f95e7809185fca))
* **redirect:** validate configuration and reserve healthz inputs ([#130](https://github.com/TogetherWeOwn/two-bot-next/issues/130)) ([abf5b07](https://github.com/TogetherWeOwn/two-bot-next/commit/abf5b07a44fa37eb7b634d17817c10b2782e2163))
* **release:** accept Security section in bootstrap migration ([#329](https://github.com/TogetherWeOwn/two-bot-next/issues/329)) ([b6a34d6](https://github.com/TogetherWeOwn/two-bot-next/commit/b6a34d6a8d763366f57e1ca8ca3a113ff45402c7))
* **release:** drop internal tracker footer from release-please config ([#531](https://github.com/TogetherWeOwn/two-bot-next/issues/531)) ([5ca08aa](https://github.com/TogetherWeOwn/two-bot-next/commit/5ca08aabe3af558fe4038a9c33f0d1ebc8f14357))
* **release:** emit valid JSON when no release PR remains ([#70](https://github.com/TogetherWeOwn/two-bot-next/issues/70)) ([44338b2](https://github.com/TogetherWeOwn/two-bot-next/commit/44338b28a7feac0093cbd72dc9cecc45ab33a15d))
* **release:** reconcile post-release Unreleased notes ([#112](https://github.com/TogetherWeOwn/two-bot-next/issues/112)) ([9a4040f](https://github.com/TogetherWeOwn/two-bot-next/commit/9a4040f2fa747d3d6509b573e3bd348e7155f9b3))
* **release:** support pagination on older gh CLIs ([#146](https://github.com/TogetherWeOwn/two-bot-next/issues/146)) ([4fce302](https://github.com/TogetherWeOwn/two-bot-next/commit/4fce30241d18d370c0e5789a4ccf4b8415d98ac7))
* **secrets:** redact stored credentials and diagnostic output ([#105](https://github.com/TogetherWeOwn/two-bot-next/issues/105)) ([23cb87f](https://github.com/TogetherWeOwn/two-bot-next/commit/23cb87f2ff572d527188104e9bde4cf5faf3849d))
* **snapshots:** fence stopped collectors before publication ([#132](https://github.com/TogetherWeOwn/two-bot-next/issues/132)) ([63fe6bd](https://github.com/TogetherWeOwn/two-bot-next/commit/63fe6bd82535e13bbb3fd45882b45521aff1add0))
* **staging-migrate:** pin Neon staging identity instead of staging database name ([#379](https://github.com/TogetherWeOwn/two-bot-next/issues/379)) ([144c479](https://github.com/TogetherWeOwn/two-bot-next/commit/144c4797cccb42d221b0eaf87ed2e9d6703858d9))
* **staging-migrate:** set-based reconcile, plan-bound apply, direct-endpoint guard ([#388](https://github.com/TogetherWeOwn/two-bot-next/issues/388)) ([897f260](https://github.com/TogetherWeOwn/two-bot-next/commit/897f260d4853afab616b2cebae2deda300285bca))
* **startup:** accept Neon options and verify staging rollout health ([#156](https://github.com/TogetherWeOwn/two-bot-next/issues/156)) ([ecd4373](https://github.com/TogetherWeOwn/two-bot-next/commit/ecd437395c580078cff77682a28466c2c9a28706))
* **supply-chain:** scan pinned images and publish release SBOMs ([#84](https://github.com/TogetherWeOwn/two-bot-next/issues/84)) ([06a2263](https://github.com/TogetherWeOwn/two-bot-next/commit/06a22637b7fe6c9458c39cbf88f066a16f5722c1))
* **testsupport:** give fixture admin DDL its own statement timeout ([#159](https://github.com/TogetherWeOwn/two-bot-next/issues/159)) ([260fbf4](https://github.com/TogetherWeOwn/two-bot-next/commit/260fbf4ed92674991327a9264447ae3252648c8c))
* **voice:** fold NFKC and case in the unique-name collision check ([#270](https://github.com/TogetherWeOwn/two-bot-next/issues/270)) ([281adfa](https://github.com/TogetherWeOwn/two-bot-next/commit/281adfa57b0989d1be70b140e7fb27f08c861b3d))
* **worker:** persist fail-closed container ownership ([#144](https://github.com/TogetherWeOwn/two-bot-next/issues/144)) ([985784c](https://github.com/TogetherWeOwn/two-bot-next/commit/985784cd758dba7f6fe38a93fe400f278b3836b3))
* **worker:** reap idle buckets before shedding a new key at the TokenBuckets cap ([#275](https://github.com/TogetherWeOwn/two-bot-next/issues/275)) ([67fa059](https://github.com/TogetherWeOwn/two-bot-next/commit/67fa05938918b340076d62deebff29295272ad09))
* **worker:** restrict public health probes to bounded GET/HEAD ([#259](https://github.com/TogetherWeOwn/two-bot-next/issues/259)) ([f6ed362](https://github.com/TogetherWeOwn/two-bot-next/commit/f6ed362a8a4c0e525b9797e8778c00baa77122a1))

- Support the exact authenticated-user REST read used by feed history reconciliation, and preserve the uncompressed runtime-image budget when Docker's containerd store also accounts for compressed blobs.
- Retain unsent onboarding welcome/goodbye payloads when successful REST responses contain inconsistent role snapshots or unusable channel evidence, while preserving proven-denial skips. Resolve only submitted session destinations so an unavailable unselected room cannot block valid routing. Add mock restart and selected/unselected regressions; current-head execution and independent review remain pending.
- Synchronize the interrupted onboarding callback restart fixture on its first durable claim, not early HTTP arrival. Keep token-free receipts, exact attempt counts, replay fencing and fresh-reselection assertions unchanged.
- Reconcile onboarding with the shared sticky/feed command runtime and outbound message safety. Component posts accept only typed no-mention/one-member welcome policies, neutralize mass mentions and validate effective payloads; original-response edits retain shared scalar-safe truncation and clearing semantics. Preserve independent gateway/feature pools, bounded ingress and worker admission. Add focused mock-wire regressions; preceding ingress repair passed required CI, while the complete reconciliation and independent review remain pending.
- Reserve a distinct gateway-only database pool within the existing five-connection gateway budget and admit at most two onboarding workers independently of the 32-row durable queue. Keep shard ingress polling through ordered SQL with a bounded packet buffer and cancellation-owned, receive-relative component acknowledgement; gate selection effects on committed jobs and confirmed ACK, fence stale readiness and finish post-defer settings/configuration failures honestly. Preserve feature transaction/member-lock semantics across REST; exact-head validation and independent review remain pending.
- Preserve unavailable onboarding permission evidence for bounded durable recovery instead of consuming unsent welcome/goodbye jobs; distinguish visible forum routing from plain-message posting and keep existing game pickers usable without landing channels, including anchor-only configuration. Add shaped forum, hot-removal and mock-shard transient-read restart regressions; validation and the independent review gate remain pending.
- Include the onboarding recovery table and sequence in the reviewed runtime privilege matrix so DML-only gateway startup can recover pending jobs; preserve web-reader denial and verify queue grants through isolated role regressions.
- Fold the self-role runtime CI command so its Rust module selector is literal
  text, not an invalid YAML mapping; retain the isolated mocked acceptance opt-in.
- Accept and strip Neon's `channel_binding` URL option before SQLx without changing TLS mode; retain fatal startup exits with fixed, credential-safe diagnostics and sanitized Worker HTTP 500 responses. Gate staging deployments on the intended new container rollout, immutable image/build identity and serving Worker version; parked readiness 503 no longer passes deployment acceptance.
- Keep parallel settings DB fixture schemas distinct when wall-clock readings
  repeat, without sharing schemas or serializing the CAS regressions.
- Grant runtime-only CRUD on the automod relations and cover migrations 0220–0223
  in least-privilege role tests. Keep edit retry identity stable across member
  role changes, and inspect updates without replacing or evicting CREATE repeat
  history needed by queued deliveries.
- Give owned disposable-database teardown a separate finite 30-second statement
  timeout for checkpoint waits, retaining five-second fixture query deadlines and
  verified cleanup after failures or caller cancellation.
- Bound automod repeat inspection to each revision's time window without letting
  unstamped updates prune delayed creates; retain immutable CREATE facts during
  role enrichment and clarify enforce-only preserved-match recovery. Run the
  preserved-replay database regression alongside durable dedupe in CI.
- Redact automod delivery-claim capabilities from derived debug output, including
  acquired and preserved results; expose tokens only at SQL fencing binds.
- Grant the least-privilege runtime role scheduled-message CRUD and claim access, with web-reader denial coverage.
- Isolate scheduled-store fixtures in per-test schema-only pools with awaited teardown on success or panic, so concurrent suites cannot replace each other's claims. Use literal legacy CHECK probes compatible with SQLx 0.9.
- Upgrade legacy scheduled-message queues to BIGINT intervals and add missing claim/nonce columns without losing definitions or run facts (additive migration 0141).
- Validate scheduled bodies in UTF-16 units and refuse whitespace/invisible-only effective messages before saving.
- Scheduled claims lease one occurrence per call, preventing distinct messages from sharing a batch nonce while preserving the nonce across retries and restarts.
- Scheduled-message ID prefixes treat `%`, `_` and backslashes literally, matching the domain resolver.
- Configured scheduled-store tests fail on connection, migration or cleanup errors instead of silently skipping; test URLs are restricted to test databases.
- Keep mock announcement response-classification tests on the production executor
  deadline; reserve the 100 ms fixture deadline for deliberate header/body timeout
  coverage. Preserve malformed-response, no-retry and uncertain-outcome assertions
  without changing production timing or weakening checks.
- Execute the standalone backup round trip in CI with the current retry-ticket
  schema while preserving frozen legacy-v3 input coverage. Release only a newly
  claimed warning key when connection acquisition fails before INSERT; uncertain
  execution and lost acknowledgments remain fenced. Recurring scratch restore
  drills allocate fresh migrated test-only targets and retain prior databases,
  private archive copies and stage receipts instead of overwriting moderation
  history. Scratch-only pinned legacy DDL prepares seven archive tables not yet
  shipped by S6; receipts retain dropped-column diagnostics and sync the retained
  directory entry before allocation. Archive hashing uses a bounded read buffer
  instead of allocating the entire compressed file. Restore explicitly locks/checks the newer
  channel FK child and truncates it only when empty, without CASCADE. Destination
  history protection and moderation default-off remain enforced. CI uses separate
  guarded databases for backup round-trip and actual-CLI publication faults;
  runbook command-drift checks follow the drill's real confirmation parser.
  Nightly routes every guarded channel fixture, including shared timestamp
  coverage, to the mandatory dedicated suite; offline regressions pin routing
  and cutover reference autolinks without weakening strict rustdoc warnings.
- Resolve historical member-ban acceptance only with exact-attempt acceptance
  and ordering evidence, atomically audited without taking newer ownership or
  clearing dispatched DELETE uncertainty. Moderation activation remains deferred.
- Fence member-ban PUT confirmation and rejection to the attempt generation
  returned by staging, including safely rejected request-ID retries. Legacy
  identity-only reconciliation fails closed; accepted-PUT audits retain the
  exact attempt generation. Moderation activation remains deferred.
- Refuse restores over destination moderation history and quarantine every
  executable imported expiry, even accepted snapshots, while preserving remote
  dispatch evidence. Fresh-target restore requires reconciliation before
  moderation activation; transient pre-write failures have scoped retry tests.
- Answer published but unwired commands ephemerally instead of timing out, preserving the existing router refusals and complete registry. Synchronize the registry on resumed process startup as well as Ready, without blocking gateway polling.
- Capture the REST pacing timestamp after the lane wait completes, preserving adjacent-request spacing across three or more reads and kicks.
- Grant the least-privilege runtime role CRUD on the self-role audit and panel-claim relations (migration 0200), cover 0200 in the role-matrix tests, and prove runtime claim access with continued web-reader denial.
- Record late result/compensation evidence for superseded self-role events under their still-current token/generation without reopening settlement or panel publication, with regression coverage.
- Reject the guild @everyone role as a self-role mutation target during catalogue validation and unconditionally at dispatch.
- Hold self-role claim fencing tokens in `Secret` so derived `Debug` redacts them; the raw value is exposed only at the SQL fencing comparisons.
- Align channel/member shared audit and idempotency timestamps with an additive,
  row-preserving migration and explicit SQL timestamp casts (migration 0124).
- Grant the existing runtime group narrow member-moderation ledger and generation-sequence access, with explicit role-matrix and restricted-store coverage. Definite unban refusals yield a durable queue ticket so later due members progress without retrying the same operation in a tick or reclaiming unknown outcomes.

### Security

- Require authenticated TLS for `two_bot_cutover::connect` (threat-model F6). `TWO_DATABASE_TLS` defaults to `required`, which refuses local hosts and missing, `disable`, `allow` or `prefer` sslmode, and always connects as `verify-full`. `local-only` (tests and CI only) allows loopback, CI service and socket hosts and refuses remote ones. Refusals are fixed strings that never echo the URL. See `docs/database-tls.md`.
- Fence the remaining Postgres connect paths with the same TLS policy (threat-model F6): the gateway store pool, both `two-bot backup` URL parses, and `channel_moderation_store::connect`. Each refuses a `sslmode=disable` remote URL with the same fixed string and connects `Required` URLs as `verify-full`.

---
This PR was generated with [Release Please](https://github.com/googleapis/release-please). See [documentation](https://github.com/googleapis/release-please#release-please).
