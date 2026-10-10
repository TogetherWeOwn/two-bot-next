# Changelog

## [0.4.0](https://github.com/TogetherWeOwn/two-bot-next/compare/v0.3.0...v0.4.0) (2026-10-10)


### Added

* **commands:** add /help discovery command ([#536](https://github.com/TogetherWeOwn/two-bot-next/issues/536)) ([9ed4929](https://github.com/TogetherWeOwn/two-bot-next/commit/9ed49294dbb710f4f36bd74548cd906076cb9ab4))
* **commands:** advertise max_length matching runtime caps ([#674](https://github.com/TogetherWeOwn/two-bot-next/issues/674)) ([0ee56a8](https://github.com/TogetherWeOwn/two-bot-next/commit/0ee56a8ce953ec60156e78eb7511f0608b90ada0))
* **commands:** publish the voice command set behind TWO_VOICE ([#559](https://github.com/TogetherWeOwn/two-bot-next/issues/559)) ([32501d3](https://github.com/TogetherWeOwn/two-bot-next/commit/32501d378d203913b8fd7422b07450e9dcdd6f72))
* **community:** capture message_created facts from the gateway pipeline ([#688](https://github.com/TogetherWeOwn/two-bot-next/issues/688)) ([c13a326](https://github.com/TogetherWeOwn/two-bot-next/commit/c13a326f1505b52a44eb4d657cbe3ddf9cee48e9))
* **deploy:** automated production approval gate ([#588](https://github.com/TogetherWeOwn/two-bot-next/issues/588)) ([2c9ab5a](https://github.com/TogetherWeOwn/two-bot-next/commit/2c9ab5a0fcfe40b0f44ffd2216c4a23c00e4f52e))
* **events:** route signed event.read through internal-action receiver ([#583](https://github.com/TogetherWeOwn/two-bot-next/issues/583)) ([0138f78](https://github.com/TogetherWeOwn/two-bot-next/commit/0138f787d4fa8fadf427d13f39c62c65cd0df290))
* **evidence:** add a reconcile runner for the B2 soak packet ([#630](https://github.com/TogetherWeOwn/two-bot-next/issues/630)) ([0c6ea7c](https://github.com/TogetherWeOwn/two-bot-next/commit/0c6ea7c1a9283ba9bf0ca89684803678a07673c5))
* **fuzz:** rsvp decision-core target for classification inputs ([#695](https://github.com/TogetherWeOwn/two-bot-next/issues/695)) ([9e2d346](https://github.com/TogetherWeOwn/two-bot-next/commit/9e2d346f943c7a5e92f114cf238a7cace0691b4c))
* **gateway:** count and quiet dispatch-lane saturation drops ([#699](https://github.com/TogetherWeOwn/two-bot-next/issues/699)) ([588bd02](https://github.com/TogetherWeOwn/two-bot-next/commit/588bd0289843f57b093b2eff7505e7fc4d49d75f))
* **internal-actions:** execute membership website actions through signed receiver ([#686](https://github.com/TogetherWeOwn/two-bot-next/issues/686)) ([119ac98](https://github.com/TogetherWeOwn/two-bot-next/commit/119ac98d48c171757d9616b9a0c2d1a5bbc4932b))
* **internal-actions:** executors for the settings get/set website actions ([#679](https://github.com/TogetherWeOwn/two-bot-next/issues/679)) ([e9085d5](https://github.com/TogetherWeOwn/two-bot-next/commit/e9085d53977d12ae0c105fbc9b6daee5f156e6c7))
* **internal-actions:** wire moderation ban, tempban, kick and warn website actions ([#678](https://github.com/TogetherWeOwn/two-bot-next/issues/678)) ([e2276c3](https://github.com/TogetherWeOwn/two-bot-next/commit/e2276c30b2167746d197bf0c1f3eaba8dc2e31ca))
* **internal-actions:** wire moderation-timeout website action ([#663](https://github.com/TogetherWeOwn/two-bot-next/issues/663)) ([cc8e8b1](https://github.com/TogetherWeOwn/two-bot-next/commit/cc8e8b129ae2cb8df9c2e842a069bd18bd5c06c1))
* **logging:** emit structured JSON lifecycle logs ([#86](https://github.com/TogetherWeOwn/two-bot-next/issues/86)) ([8985090](https://github.com/TogetherWeOwn/two-bot-next/commit/89850905d8629616ddc210764bd63c018ad4885a))
* **moderation:** add confirmed operator channel lane release ([dfc1433](https://github.com/TogetherWeOwn/two-bot-next/commit/dfc1433fab43e41b51f9710e0fb3a1d01943d573))
* **moderation:** wire member commands, unban sweep and reconcile CLI ([#398](https://github.com/TogetherWeOwn/two-bot-next/issues/398)) ([414e3a5](https://github.com/TogetherWeOwn/two-bot-next/commit/414e3a59c0f0dcd93799645dfcf732bee0edf68e))
* **rsvp:** wire commands through shared interaction runtime ([#67](https://github.com/TogetherWeOwn/two-bot-next/issues/67)) ([271703c](https://github.com/TogetherWeOwn/two-bot-next/commit/271703cc081bde4972db68199c7de46d663b7cc5))
* **soak:** measure Gate 5 outage-start-to-recovery offline ([#669](https://github.com/TogetherWeOwn/two-bot-next/issues/669)) ([11a4696](https://github.com/TogetherWeOwn/two-bot-next/commit/11a4696e3e501580cdbac5bed8141b7a4dd6e6b2))
* **staging:** add read-only plan audit for ledger owner and memberships ([#632](https://github.com/TogetherWeOwn/two-bot-next/issues/632)) ([a4f8641](https://github.com/TogetherWeOwn/two-bot-next/commit/a4f864138d6eeda782b10f07a51ee9a7ebd2e100))
* **staging:** add reviewed-image container backout/restore drill ([#605](https://github.com/TogetherWeOwn/two-bot-next/issues/605)) ([6a5c358](https://github.com/TogetherWeOwn/two-bot-next/commit/6a5c358587a26674888b14af0c2a4c6f286d1807))
* **staging:** add voice room-lifecycle smoke harness ([#616](https://github.com/TogetherWeOwn/two-bot-next/issues/616)) ([48c932f](https://github.com/TogetherWeOwn/two-bot-next/commit/48c932f6932de8f1b76a684bf3efbd7d03e7ed29))
* **voice:** add /name panel with custom-name modal and restore button ([#580](https://github.com/TogetherWeOwn/two-bot-next/issues/580)) ([2e94c64](https://github.com/TogetherWeOwn/two-bot-next/commit/2e94c64b382d15597ae8607e0726dbb002e4ba7d))
* **voice:** add /private and /public with persisted privacy and the Join channel ([#578](https://github.com/TogetherWeOwn/two-bot-next/issues/578)) ([5cddc90](https://github.com/TogetherWeOwn/two-bot-next/commit/5cddc905585327e7150d10b3972cba4c2e256d87))
* **voice:** answer Join channel requests with Approve, Deny and Block buttons ([#599](https://github.com/TogetherWeOwn/two-bot-next/issues/599)) ([99cf832](https://github.com/TogetherWeOwn/two-bot-next/commit/99cf832395202a485f7647f8e8d04de58b1bf659))
* **voice:** audit vote-kick start, refusal, result and enforcement ([#594](https://github.com/TogetherWeOwn/two-bot-next/issues/594)) ([dec2e85](https://github.com/TogetherWeOwn/two-bot-next/commit/dec2e85a456cfee9388cd1615c84822e49488460))
* **voice:** persist approved Connect grants through worker restarts ([#606](https://github.com/TogetherWeOwn/two-bot-next/issues/606)) ([b4f1eab](https://github.com/TogetherWeOwn/two-bot-next/commit/b4f1eabe7f01097b91d434b937ca27fa38433636))
* **voice:** persist create admission and gate worker creates on it ([#552](https://github.com/TogetherWeOwn/two-bot-next/issues/552)) ([43425be](https://github.com/TogetherWeOwn/two-bot-next/commit/43425be078c661459fa05bf9d1a935b533616983))
* **voice:** wire /limit and /unlimit through the room worker ([#576](https://github.com/TogetherWeOwn/two-bot-next/issues/576)) ([4330700](https://github.com/TogetherWeOwn/two-bot-next/commit/43307000e49dfa234da35a64c8ac6ffdccc6d245))
* **wrangler:** add offline process-history V1 contract and proofs ([#647](https://github.com/TogetherWeOwn/two-bot-next/issues/647)) ([4656f05](https://github.com/TogetherWeOwn/two-bot-next/commit/4656f05b2e43156b4c4618ba55c6edafe48bb2d6))
* **wrangler:** forward non-secret Discord ID variables ([#668](https://github.com/TogetherWeOwn/two-bot-next/issues/668)) ([fcb422c](https://github.com/TogetherWeOwn/two-bot-next/commit/fcb422c1a7479efbd4624c501577b357f66b53ac))


### Fixed

* **admission:** backfill 0419 lease stamp for legacy-held send lanes ([#613](https://github.com/TogetherWeOwn/two-bot-next/issues/613)) ([af6fba0](https://github.com/TogetherWeOwn/two-bot-next/commit/af6fba043546936267d763415cc892a31767850d))
* **admission:** degrade to pre-lease lane when the lease column is missing ([#612](https://github.com/TogetherWeOwn/two-bot-next/issues/612)) ([d12476d](https://github.com/TogetherWeOwn/two-bot-next/commit/d12476da592a04780461bbaee593a377cd9ff25b))
* **admission:** self-heal stuck send lane; attach boot sync cause ([#609](https://github.com/TogetherWeOwn/two-bot-next/issues/609)) ([e54bee8](https://github.com/TogetherWeOwn/two-bot-next/commit/e54bee8a6b5c00257726192319c8c1a5e020b9b3))
* **attendance:** trusted occurrence resolution and host check-in authority ([#697](https://github.com/TogetherWeOwn/two-bot-next/issues/697)) ([32e8621](https://github.com/TogetherWeOwn/two-bot-next/commit/32e8621760fc572cb86dda15795a9f57c823de94))
* **automation:** bound text triggers and LFG signup by actor ([#640](https://github.com/TogetherWeOwn/two-bot-next/issues/640)) ([cd1bda7](https://github.com/TogetherWeOwn/two-bot-next/commit/cd1bda7e95e45d251a40fd7beaf8166a6d65291b))
* **automation:** cap guild schedules, feeds and open LFG posts ([#639](https://github.com/TogetherWeOwn/two-bot-next/issues/639)) ([7548131](https://github.com/TogetherWeOwn/two-bot-next/commit/75481312ad8eb38a7bae60e8f5e985f985bbd762))
* **backup:** classify store-chain journal, watermark and ledger tables ([#586](https://github.com/TogetherWeOwn/two-bot-next/issues/586)) ([ee5faef](https://github.com/TogetherWeOwn/two-bot-next/commit/ee5faef39c8fc08e0fd0488a9282d9253365aeae))
* **backup:** refuse malformed guild config restore inputs ([#139](https://github.com/TogetherWeOwn/two-bot-next/issues/139)) ([3921ec4](https://github.com/TogetherWeOwn/two-bot-next/commit/3921ec41995885595a21b265937461e9ce2a16af))
* **bot:** fence send-admission pools with database TLS policy ([09fcb3c](https://github.com/TogetherWeOwn/two-bot-next/commit/09fcb3c97f13c70a24d8c5d6a00891dc462fb2fa))
* **bot:** keep moderation commands answerable when the interaction lane is full ([#557](https://github.com/TogetherWeOwn/two-bot-next/issues/557)) ([2e8ba3d](https://github.com/TogetherWeOwn/two-bot-next/commit/2e8ba3dfc4a1528579f586c6069e1ec870d2ab67))
* **ci:** list both sides of a rename in the container-inputs selector ([#555](https://github.com/TogetherWeOwn/two-bot-next/issues/555)) ([d5c62cb](https://github.com/TogetherWeOwn/two-bot-next/commit/d5c62cbcf27d9496145b4d2cd8648c19327473b7))
* **commands:** pass automod verdict into prefix trigger handler ([#682](https://github.com/TogetherWeOwn/two-bot-next/issues/682)) ([420098e](https://github.com/TogetherWeOwn/two-bot-next/commit/420098ec8f01b15507b9bcfd7bce3cf6b249bb0f))
* **community:** mark scorecard coverage only for captured streams ([#658](https://github.com/TogetherWeOwn/two-bot-next/issues/658)) ([948296c](https://github.com/TogetherWeOwn/two-bot-next/commit/948296cd93bc7a98644a3e25a5c38927a2e3c57c))
* **core:** gate unsafe with workspace deny lint ([#664](https://github.com/TogetherWeOwn/two-bot-next/issues/664)) ([be3e3e5](https://github.com/TogetherWeOwn/two-bot-next/commit/be3e3e57b50453db6061406038210ac8c85966ae))
* **cutover:** name the cutover probe's agent and record the live voice rehearsal ([#596](https://github.com/TogetherWeOwn/two-bot-next/issues/596)) ([d111423](https://github.com/TogetherWeOwn/two-bot-next/commit/d111423bd7501e332ce6d1583f352f0028650ca1))
* **cutover:** port three backfill parser fixes and re-verify nine ledger rows ([#577](https://github.com/TogetherWeOwn/two-bot-next/issues/577)) ([a90bea1](https://github.com/TogetherWeOwn/two-bot-next/commit/a90bea128c071a508a10287ed6838b1afdc133b1))
* **db:** keep reservation history append-only and preserve migrator membership ([#634](https://github.com/TogetherWeOwn/two-bot-next/issues/634)) ([ccc2dc2](https://github.com/TogetherWeOwn/two-bot-next/commit/ccc2dc27f94619dcbc840736c4cb17f50e1f8864))
* **deploy:** fail fast on out-of-band Worker version churn ([#608](https://github.com/TogetherWeOwn/two-bot-next/issues/608)) ([2312b94](https://github.com/TogetherWeOwn/two-bot-next/commit/2312b94423c270221c5b2c55bc903354a57065f5))
* **deploy:** stamp production build identity and gate on the matching readyz revision ([#660](https://github.com/TogetherWeOwn/two-bot-next/issues/660)) ([866f69b](https://github.com/TogetherWeOwn/two-bot-next/commit/866f69b2516744f219f9e4e2115662ff044138ef))
* **deps:** pin sharp to 0.35.5 in the wrangler tooling lockfile ([#635](https://github.com/TogetherWeOwn/two-bot-next/issues/635)) ([89db4d1](https://github.com/TogetherWeOwn/two-bot-next/commit/89db4d13c24f5f28d8469a01acb8b3e61ba64cd9))
* **feeds:** bound relay polls and resume fairly after deadlines ([#641](https://github.com/TogetherWeOwn/two-bot-next/issues/641)) ([d84c92f](https://github.com/TogetherWeOwn/two-bot-next/commit/d84c92f56f0254e4ea7121a3b44ab9f43e3db935))
* **feeds:** escape item titles and bound feed sources ([#644](https://github.com/TogetherWeOwn/two-bot-next/issues/644)) ([49ecc47](https://github.com/TogetherWeOwn/two-bot-next/commit/49ecc47194a52a9fb0129c2f18d113283e2f93cb))
* **feeds:** skip item URLs that contain masked-link syntax ([#648](https://github.com/TogetherWeOwn/two-bot-next/issues/648)) ([1b4a3c3](https://github.com/TogetherWeOwn/two-bot-next/commit/1b4a3c3fcee43d45288f0ebb36ddd69ca0e67811))
* **gateway:** give self-role reactions their own bounded lane ([#676](https://github.com/TogetherWeOwn/two-bot-next/issues/676)) ([4f26593](https://github.com/TogetherWeOwn/two-bot-next/commit/4f26593aa5eb14a439c62586b6d16e67fa3bc650))
* **gateway:** pin dispatch-worker failures and guard ACK permits ([#659](https://github.com/TogetherWeOwn/two-bot-next/issues/659)) ([b2eddb3](https://github.com/TogetherWeOwn/two-bot-next/commit/b2eddb3a299edd8deb6b9a30c4c5cf36e9bd04c1))
* **gateway:** wait out a held send lane while the gateway boots ([#628](https://github.com/TogetherWeOwn/two-bot-next/issues/628)) ([c322a70](https://github.com/TogetherWeOwn/two-bot-next/commit/c322a709014fc62ad1b6f6e4dde362d5ff24d967))
* **internal-actions:** bind receiver to wildcard with container marker ([#619](https://github.com/TogetherWeOwn/two-bot-next/issues/619)) ([94bd3c3](https://github.com/TogetherWeOwn/two-bot-next/commit/94bd3c38c9e2d48eeb9f30cd5adff756d1a3fcf4))
* **jobs:** bind the ticker and feed poller to the live-identity fence ([#590](https://github.com/TogetherWeOwn/two-bot-next/issues/590)) ([a244e90](https://github.com/TogetherWeOwn/two-bot-next/commit/a244e90a1c809d35bf522dd57e8ee9d9c19fd440))
* **jobs:** gate the scheduled-message ticker on automations and resolve ten ledger rows ([#575](https://github.com/TogetherWeOwn/two-bot-next/issues/575)) ([204f473](https://github.com/TogetherWeOwn/two-bot-next/commit/204f4735f8e08e0de45fb2e5a62f51548d060476))
* **logging:** report the original target for bridged log records ([#625](https://github.com/TogetherWeOwn/two-bot-next/issues/625)) ([18bde75](https://github.com/TogetherWeOwn/two-bot-next/commit/18bde75ab382dd5bed0a8b9336286ed6ca26b980))
* **moderation:** deny thread and reaction sends on lockdown; purge old messages singly ([#574](https://github.com/TogetherWeOwn/two-bot-next/issues/574)) ([1663ca3](https://github.com/TogetherWeOwn/two-bot-next/commit/1663ca3db976e8bab5ae1fcc5aa099af4aa96530))
* **moderation:** free the channel lane when a purge history read fails ([#556](https://github.com/TogetherWeOwn/two-bot-next/issues/556)) ([fd582b7](https://github.com/TogetherWeOwn/two-bot-next/commit/fd582b7ee99e79034fc62f40e5a0cda198a80b18))
* **moderation:** tag stranded running unban claims and pin the report truncation boundary ([#545](https://github.com/TogetherWeOwn/two-bot-next/issues/545)) ([33e98bc](https://github.com/TogetherWeOwn/two-bot-next/commit/33e98bc66c47601cadb2406c0ae2cfc5dfed20c6))
* **onboarding:** close two picker drifts and mark eight parity rows ported ([#579](https://github.com/TogetherWeOwn/two-bot-next/issues/579)) ([9075df7](https://github.com/TogetherWeOwn/two-bot-next/commit/9075df712b679af81ba0209876fcca2720369465))
* **parity:** pin scan malformed accounting, automod validator, welcome copy, inactivity window ([#589](https://github.com/TogetherWeOwn/two-bot-next/issues/589)) ([26bb496](https://github.com/TogetherWeOwn/two-bot-next/commit/26bb496611a0a480c2c5899b2009240bc1cbf9c6))
* **probes:** send explicit User-Agent from workers.dev probes ([#611](https://github.com/TogetherWeOwn/two-bot-next/issues/611)) ([e565efe](https://github.com/TogetherWeOwn/two-bot-next/commit/e565efea9279940abf76ee025d2bb1a8e298aef7))
* **readyz:** accept the bot's real component set in both gates ([#657](https://github.com/TogetherWeOwn/two-bot-next/issues/657)) ([86a6668](https://github.com/TogetherWeOwn/two-bot-next/commit/86a6668a7f52a9f9d8e704b1e584145bd6e7e49e))
* **release:** emit seven-section metadata in release PRs ([#587](https://github.com/TogetherWeOwn/two-bot-next/issues/587)) ([09b0cd4](https://github.com/TogetherWeOwn/two-bot-next/commit/09b0cd4bd66d6e0d9e797ac71b51b85ef81f7af9))
* **self-roles:** run strict catalogue parser in preflight ([#582](https://github.com/TogetherWeOwn/two-bot-next/issues/582)) ([a978b78](https://github.com/TogetherWeOwn/two-bot-next/commit/a978b78557fcb0802b3e3cfac5dbf78ccf7d7529))
* **staging:** accept either completed counter shape on the final rollout re-read ([#593](https://github.com/TogetherWeOwn/two-bot-next/issues/593)) ([5a42c35](https://github.com/TogetherWeOwn/two-bot-next/commit/5a42c3575a6b82f6ac05ebfc0af35b9438fc3ce6))
* **staging:** accept replaced rollout as backout proof with image match ([#622](https://github.com/TogetherWeOwn/two-bot-next/issues/622)) ([0be210e](https://github.com/TogetherWeOwn/two-bot-next/commit/0be210e08b35f44c1eccd56a11b78892fda24927))
* **staging:** accept the durable-object in-use instance counter shape in the rollout gate ([#592](https://github.com/TogetherWeOwn/two-bot-next/issues/592)) ([03e9960](https://github.com/TogetherWeOwn/two-bot-next/commit/03e9960308dca4680c54d908097b5862352ffde7))
* **staging:** pin answering deployment across takeover retries ([#656](https://github.com/TogetherWeOwn/two-bot-next/issues/656)) ([8bc33c9](https://github.com/TogetherWeOwn/two-bot-next/commit/8bc33c978a0516cb2920d5cf25c0d7fcaaffa8f3))
* **staging:** print where the application listing points when the rollout gate reports image drift ([#600](https://github.com/TogetherWeOwn/two-bot-next/issues/600)) ([3368d2b](https://github.com/TogetherWeOwn/two-bot-next/commit/3368d2be66174b8c56a83ec331e3472901c55446))
* **staging:** record readiness diagnostics on unconverged polls ([#650](https://github.com/TogetherWeOwn/two-bot-next/issues/650)) ([bce86a7](https://github.com/TogetherWeOwn/two-bot-next/commit/bce86a79185051032e4c919cf2e721879aa7bb35))
* **staging:** report the ownership takeover refusal reason and widen the wait ([ab89e81](https://github.com/TogetherWeOwn/two-bot-next/commit/ab89e81a37645472a8a254a7ffb142d88904bd9e))
* **staging:** report zero-instance rollout timeout state ([#645](https://github.com/TogetherWeOwn/two-bot-next/issues/645)) ([7bf551e](https://github.com/TogetherWeOwn/two-bot-next/commit/7bf551eb01c7fcc97d30f9e1d61e6cb790477dc6))
* **staging:** send an explicit User-Agent from the rollout gate ([#584](https://github.com/TogetherWeOwn/two-bot-next/issues/584)) ([ac5ff90](https://github.com/TogetherWeOwn/two-bot-next/commit/ac5ff9086a2675c98a89e0822d973ba0fa379e42))
* **staging:** split backout rollout proof into three refusal codes ([#618](https://github.com/TogetherWeOwn/two-bot-next/issues/618)) ([1dee3c6](https://github.com/TogetherWeOwn/two-bot-next/commit/1dee3c6d44408a28a36e36b6b9d0f372fd4aa9ed))
* **staging:** stop rollback mutations after GET auth refusals ([#602](https://github.com/TogetherWeOwn/two-bot-next/issues/602)) ([946769f](https://github.com/TogetherWeOwn/two-bot-next/commit/946769f99daac62beaaff814d6685a555c5050f4))
* **staging:** tolerate 24 stale application-listing polls in the rollout gate ([#636](https://github.com/TogetherWeOwn/two-bot-next/issues/636)) ([a961e3a](https://github.com/TogetherWeOwn/two-bot-next/commit/a961e3ad1694cf0afd4a1f15daa9e84693559861))
* **voice:** bind import preview text and compare-and-swap apply ([#585](https://github.com/TogetherWeOwn/two-bot-next/issues/585)) ([794811e](https://github.com/TogetherWeOwn/two-bot-next/commit/794811e67d00f5ced97a3638d23e823e630d4f52))
* **voice:** bound position first-number and validate inherit source ([#621](https://github.com/TogetherWeOwn/two-bot-next/issues/621)) ([8336f31](https://github.com/TogetherWeOwn/two-bot-next/commit/8336f314618b1d8abaf496c47adc6b493f83dc26))
* **voice:** bound reconcile timestamp parsing and pin duration parity ([#569](https://github.com/TogetherWeOwn/two-bot-next/issues/569)) ([82d1c14](https://github.com/TogetherWeOwn/two-bot-next/commit/82d1c1428efd7fce50f404af9236382e771a18e7))
* **voice:** cap dead-letter list and prune idle voice chains ([#677](https://github.com/TogetherWeOwn/two-bot-next/issues/677)) ([63827c5](https://github.com/TogetherWeOwn/two-bot-next/commit/63827c528e4cde6d14f7d954fe22e294f77c4a93))
* **voice:** exclude Manage Roles from all room create overwrites ([#568](https://github.com/TogetherWeOwn/two-bot-next/issues/568)) ([85e7c31](https://github.com/TogetherWeOwn/two-bot-next/commit/85e7c31adcacc2b941026471a292e5e5bddf1e35))
* **voice:** hide /setup detail from members, harden /import, reserve voice names ([#572](https://github.com/TogetherWeOwn/two-bot-next/issues/572)) ([1c5a2f6](https://github.com/TogetherWeOwn/two-bot-next/commit/1c5a2f634bd8d3cd6157f441a19e79481bbca99d))
* **voice:** honor communication timeouts in guild authority ([#631](https://github.com/TogetherWeOwn/two-bot-next/issues/631)) ([6671aa8](https://github.com/TogetherWeOwn/two-bot-next/commit/6671aa8df1067aef30c239840fc5b25fc7561239))
* **voice:** isolate per-room delete backoff ([#570](https://github.com/TogetherWeOwn/two-bot-next/issues/570)) ([ddb3258](https://github.com/TogetherWeOwn/two-bot-next/commit/ddb32581be3acb8ca33d9106d05d9609b93b97c3))
* **voice:** name the missing permission and filter room names on the create path ([#558](https://github.com/TogetherWeOwn/two-bot-next/issues/558)) ([d68f697](https://github.com/TogetherWeOwn/two-bot-next/commit/d68f69733ea30043745ae55feb9382d3e1fc9546))
* **voice:** protect infrastructure and honor ordinary empty grace ([#571](https://github.com/TogetherWeOwn/two-bot-next/issues/571)) ([9dc5455](https://github.com/TogetherWeOwn/two-bot-next/commit/9dc5455b9c83d819523689264b4e2af802ce38bb))
* **voice:** refuse exports that import would reject for size ([#646](https://github.com/TogetherWeOwn/two-bot-next/issues/646)) ([63c180d](https://github.com/TogetherWeOwn/two-bot-next/commit/63c180dbaccdde52a6879f0f5a4e2f84dbaa196a))
* **voice:** render vote-kick reasons as bounded mention-safe plain text ([#692](https://github.com/TogetherWeOwn/two-bot-next/issues/692)) ([472d525](https://github.com/TogetherWeOwn/two-bot-next/commit/472d5254208a1f536503017d2e921bdba4890dc4))
* **voice:** resolve interactions by registered command identity ([#642](https://github.com/TogetherWeOwn/two-bot-next/issues/642)) ([20f79b2](https://github.com/TogetherWeOwn/two-bot-next/commit/20f79b2ebf17b45f597e9ad8afbaf72d8536a2d1))
* **voice:** route /kick through moderation before the room vote ([#591](https://github.com/TogetherWeOwn/two-bot-next/issues/591)) ([91a4d01](https://github.com/TogetherWeOwn/two-bot-next/commit/91a4d012a33a9d71f0768c31bb0c07b2e9989702))
* **voice:** separate guild admin authority from room owner grants ([#561](https://github.com/TogetherWeOwn/two-bot-next/issues/561)) ([c8f35f1](https://github.com/TogetherWeOwn/two-bot-next/commit/c8f35f1067b6c153dec147a5ce249ce62c6c68a9))
* **voice:** show new template text in import preview ([#581](https://github.com/TogetherWeOwn/two-bot-next/issues/581)) ([07f456b](https://github.com/TogetherWeOwn/two-bot-next/commit/07f456bc0bf70f51626ef4b184df008132eb2604))
* **voice:** survive a poisoned room lock without restarting the gateway ([#662](https://github.com/TogetherWeOwn/two-bot-next/issues/662)) ([b52f151](https://github.com/TogetherWeOwn/two-bot-next/commit/b52f151ebf4e4b57316df348922d59cc37663be0))
* **voice:** tolerate reconcile prune in shared-room kick ballot test ([#610](https://github.com/TogetherWeOwn/two-bot-next/issues/610)) ([775bc9d](https://github.com/TogetherWeOwn/two-bot-next/commit/775bc9d8a63ecd41d712e942cde6ccb5e62e327a))
* **voice:** validate logging roles and channel destinations ([#563](https://github.com/TogetherWeOwn/two-bot-next/issues/563)) ([e266f6a](https://github.com/TogetherWeOwn/two-bot-next/commit/e266f6abaf10a5bf3e8cc56e9d0f46fb65e4ebd1))
* **worker:** restrict alert forwarding to opt-in Discord webhooks ([#601](https://github.com/TogetherWeOwn/two-bot-next/issues/601)) ([f2c5fc5](https://github.com/TogetherWeOwn/two-bot-next/commit/f2c5fc5f0c280e3e40a2844115ef9bfb2e45cc41))
* **wrangler:** bound the ops metrics fetch and harden its bearer gate ([#654](https://github.com/TogetherWeOwn/two-bot-next/issues/654)) ([08762f2](https://github.com/TogetherWeOwn/two-bot-next/commit/08762f2f4e3609254d4889b46a323c4bf424b59d))

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
* **cutover:** report blind windows and unknown-start counts in voice reconcile ([#548](https://github.com/TogetherWeOwn/two-bot-next/issues/548)) ([8b332f6](https://github.com/TogetherWeOwn/two-bot-next/commit/8b332f6cb5c5c62937f3216063840175e6ae1d51))
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
- Wire RSVP, namespaced RSVP totals and ManageEvents-gated host attendance through the shared interaction router, sqlx stores and REST executor, with ephemeral deferred replies and test-container/mock-Discord acceptance. Publish the full shared command registry before gateway startup, including persisted-session RESUMED boots. Defer queued commands at receipt, complete accepted RSVP commands serially before their checkpoints, and drain accepted replies within the shared shutdown bounds. Preserve the persistent funnel dispatcher’s fatal backlog/I/O limits, receipt timestamps, invite snapshots and disconnect-generation readiness fence.
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

- Keep moderation commands answerable while the shared interaction lane is busy. Cap each member at three admitted interactions, reserve an eight-worker lane for permission-gated slash commands from members who hold the permission, and answer anything not admitted with one ephemeral "busy, try again" callback instead of dropping it silently. Bound the LFG queue to the running request plus eight waiters so sign-up selects cannot fill the lane.
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
* **moderation:** preserve live overwrite edits when unlocking ([#562](https://github.com/TogetherWeOwn/two-bot-next/issues/562)) ([c2214ab](https://github.com/TogetherWeOwn/two-bot-next/commit/c2214ab581d636c8c4e93f1cdfdb8b598b8eb1f8))
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
- Make `/lockdown` deny `SendMessagesInThreads`, `CreatePublicThreads`, `CreatePrivateThreads` and `AddReactions` on `@everyone` along with `SendMessages`, so members cannot keep posting through threads or reactions while the reply says `locked_down`. `/unlock` restores each of those bits only while it still holds the locked state, so it also unlocks a channel locked earlier by the narrower rule and keeps a bit a moderator changed since. A role overwrite that allows sending still wins.
- Make `/purge` work on quiet channels: it bulk-deletes the messages under 14 days old and deletes older ones one by one instead of failing the whole call with a Discord 400. It also skips pinned messages and the bot's own posts (ticket, sticky and LFG panels), skips a message deleted after the listing, and reports the count actually deleted when a later delete is refused.
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
- Preserve one RSVP acknowledgement owner when composing the ordered RSVP path with the shared sticky/feed command runtime; the latter must not send fallback replies for RSVP or redundantly publish a registry already synchronized at boot.
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

## [0.2.0](https://github.com/TogetherWeOwn/two-bot-next/releases/tag/v0.2.0) (2026-09-30)

### Added

* **audit:** add durable fenced delivery storage ([#58](https://github.com/TogetherWeOwn/two-bot-next/issues/58)) ([4f8c460](https://github.com/TogetherWeOwn/two-bot-next/commit/4f8c4606fbdc9256a88888c740168057eecbad3c))
* **audit:** port operational audit classifiers and moderation MAC ([#29](https://github.com/TogetherWeOwn/two-bot-next/issues/29)) ([58b5184](https://github.com/TogetherWeOwn/two-bot-next/commit/58b51843fae9fed08052d3d5a7dee0e4b28155b8))
* **backup:** port dump/restore, S3 upload, sealed guild-config snapshot ([#11](https://github.com/TogetherWeOwn/two-bot-next/issues/11)) ([4cb4733](https://github.com/TogetherWeOwn/two-bot-next/commit/4cb47339b58d8e9c320d147ebd9e1a8bb7f47c31))
* **bot:** B2 container hardening, staging deploy, soak runbook ([#4](https://github.com/TogetherWeOwn/two-bot-next/issues/4)) ([63fbdb1](https://github.com/TogetherWeOwn/two-bot-next/commit/63fbdb1130eee22d3db5f376bd49cd1cf8b3cc61))
* **commands:** automod matcher plus sanctions and env gates ([#14](https://github.com/TogetherWeOwn/two-bot-next/issues/14)) ([0129b58](https://github.com/TogetherWeOwn/two-bot-next/commit/0129b58962a4b739141d58adbc9db503311882a0))
* **commands:** feature command shapes plus env gates ([#10](https://github.com/TogetherWeOwn/two-bot-next/issues/10)) ([84809c1](https://github.com/TogetherWeOwn/two-bot-next/commit/84809c1ee77b933f5565b5b63495b94dfae2254a))
* **commands:** moderation shapes plus policy and env gates ([#12](https://github.com/TogetherWeOwn/two-bot-next/issues/12)) ([16d1f11](https://github.com/TogetherWeOwn/two-bot-next/commit/16d1f113830f975bc8b16e79ca5d9a41e5695103))
* **commands:** registry merge plus leveling domain port ([#9](https://github.com/TogetherWeOwn/two-bot-next/issues/9)) ([d0d4a36](https://github.com/TogetherWeOwn/two-bot-next/commit/d0d4a36e2ecc26c6ebbd736e12bae5d2a31ff669))
* **community:** presence probe, weekly scorecard, inactivity flagging ([#46](https://github.com/TogetherWeOwn/two-bot-next/issues/46)) ([2dc5e7f](https://github.com/TogetherWeOwn/two-bot-next/commit/2dc5e7f9bb97910773818b3013ca8008dc41dca7))
* **containment:** port pure anti-nuke policy and quarantine planner ([#44](https://github.com/TogetherWeOwn/two-bot-next/issues/44)) ([d8f46e4](https://github.com/TogetherWeOwn/two-bot-next/commit/d8f46e4afdbb9b90368a5160cd7d5d6b1087bc30))
* **cutover:** port MEE6 XP, rewards, history backfill operator tools ([#13](https://github.com/TogetherWeOwn/two-bot-next/issues/13)) ([fe20bdf](https://github.com/TogetherWeOwn/two-bot-next/commit/fe20bdf1d7cfcd9bf738d1d588e42581cb5ff08e))
* **discord:** REST action executor with legacy pacing plus mock REST double ([#63](https://github.com/TogetherWeOwn/two-bot-next/issues/63)) ([e12aee0](https://github.com/TogetherWeOwn/two-bot-next/commit/e12aee051a37bab72f389bc5ddad2ffe2f1ea853))
* **gateway:** event pipeline plus funnel port ([#15](https://github.com/TogetherWeOwn/two-bot-next/issues/15)) ([173146e](https://github.com/TogetherWeOwn/two-bot-next/commit/173146ee9d85b470bcdd41f60a1a2f5f3c6a8b6d))
* **gateway:** persist checkpoints and resume across restarts ([#26](https://github.com/TogetherWeOwn/two-bot-next/issues/26)) ([6c47742](https://github.com/TogetherWeOwn/two-bot-next/commit/6c477427348b4515df2c757129cddbeea2e895ca))
* **internal-actions:** add durable replay and execution store ([#61](https://github.com/TogetherWeOwn/two-bot-next/issues/61)) ([751499e](https://github.com/TogetherWeOwn/two-bot-next/commit/751499e988a3c1b0b4c83c32ad2376bf5d0a14de))
* **internal-actions:** port signing, replay guard, buckets, allowlist validators ([#17](https://github.com/TogetherWeOwn/two-bot-next/issues/17)) ([dda04e5](https://github.com/TogetherWeOwn/two-bot-next/commit/dda04e5f1914248e15b1785c5ae57dc5e960b371))
* **jobs:** port website-contract counter, rank and scheduled-events ticks ([#33](https://github.com/TogetherWeOwn/two-bot-next/issues/33)) ([64ae194](https://github.com/TogetherWeOwn/two-bot-next/commit/64ae194088ecc5acb2a51f08553277fbd6b19b57))
* **leveling:** port transactional XP runtime and legacy replies ([#54](https://github.com/TogetherWeOwn/two-bot-next/issues/54)) ([0da949c](https://github.com/TogetherWeOwn/two-bot-next/commit/0da949c722cc9f9d185d834a7cbe9ce99097a598))
* **lfg:** port signup domain and transactional store ([#39](https://github.com/TogetherWeOwn/two-bot-next/issues/39)) ([6129e69](https://github.com/TogetherWeOwn/two-bot-next/commit/6129e696ad9bfdc04797da0683dfa60042063cd0))
* **moderation:** add channel moderation domain and durable store ([#22](https://github.com/TogetherWeOwn/two-bot-next/issues/22)) ([d32e342](https://github.com/TogetherWeOwn/two-bot-next/commit/d32e342ef81de6b733adf30b7c710cadf24e8812))
* **onboarding:** port picker, welcome and goodbye domain with funnel store ([#41](https://github.com/TogetherWeOwn/two-bot-next/issues/41)) ([db47381](https://github.com/TogetherWeOwn/two-bot-next/commit/db47381c90ddfd710ca3be69da7f4734a3291bbb))
* **prototype:** measured twilight gateway plus slash command ([#16](https://github.com/TogetherWeOwn/two-bot-next/issues/16)) ([09e5724](https://github.com/TogetherWeOwn/two-bot-next/commit/09e5724116eaf8a0c25026ef5b96f7c7c4b92d30))
* **raid:** port join burst and join-risk decisions ([#55](https://github.com/TogetherWeOwn/two-bot-next/issues/55)) ([e5e72f0](https://github.com/TogetherWeOwn/two-bot-next/commit/e5e72f057a85408d1eadc0524cf4c393ed72a9cf))
* **redirect:** port go.two.gg redirect server to Worker route ([#7](https://github.com/TogetherWeOwn/two-bot-next/issues/7)) ([5348fa1](https://github.com/TogetherWeOwn/two-bot-next/commit/5348fa1d6079bcb2d7277f869b130619cb8955b6))
* **router:** interaction router plus full command registry publish ([#57](https://github.com/TogetherWeOwn/two-bot-next/issues/57)) ([3c4fe98](https://github.com/TogetherWeOwn/two-bot-next/commit/3c4fe9805d3762a67d61d3cc2114dc2da835afb4))
* **rsvp:** port RSVP transitions, totals, and host check-in ([#24](https://github.com/TogetherWeOwn/two-bot-next/issues/24)) ([eb88087](https://github.com/TogetherWeOwn/two-bot-next/commit/eb880874e1a1fc92166a3d6945cc577bbf3e6ccb))
* **scaffold:** Cargo workspace and Container deploy skeleton ([#3](https://github.com/TogetherWeOwn/two-bot-next/issues/3)) ([2fe0042](https://github.com/TogetherWeOwn/two-bot-next/commit/2fe0042aea0f6297f58291cd4a85f4e816f6c278))
* **settings:** guild_settings hot reload with 15s version poll and audit ([#25](https://github.com/TogetherWeOwn/two-bot-next/issues/25)) ([430e438](https://github.com/TogetherWeOwn/two-bot-next/commit/430e438aad3202f6cdede177e4cc47f5075effb9))
* **sticky:** debounced re-post domain, store and migration ([#28](https://github.com/TogetherWeOwn/two-bot-next/issues/28)) ([c89e053](https://github.com/TogetherWeOwn/two-bot-next/commit/c89e05376aee62bfb8d784bba85f51071e1e0092))
* **voice:** add offline readiness and safe diagnostic contract ([#48](https://github.com/TogetherWeOwn/two-bot-next/issues/48)) ([43c7f61](https://github.com/TogetherWeOwn/two-bot-next/commit/43c7f6121774efcba4ffdea302c9dfc7e07933bf))
* **voice:** add pure ownership transition core ([#49](https://github.com/TogetherWeOwn/two-bot-next/issues/49)) ([a926ffb](https://github.com/TogetherWeOwn/two-bot-next/commit/a926ffb5ef7a9f6af68e999f47dea09beaba627a))
* **voice:** add pure vote-kick decision core ([#50](https://github.com/TogetherWeOwn/two-bot-next/issues/50)) ([e6694ee](https://github.com/TogetherWeOwn/two-bot-next/commit/e6694ee1c18012d686cd8cb4b377159302a3538b))
* **voice:** add standalone versioned configuration codec ([#51](https://github.com/TogetherWeOwn/two-bot-next/issues/51)) ([dcbe432](https://github.com/TogetherWeOwn/two-bot-next/commit/dcbe432e447df9965411d7835d28c9aa5aa8d62f))

- Add durable operational audit rows, fenced delivery claims, accepted-message recovery, quarantine and a persistent delivery halt to the sqlx core store. Reserve audit migrations 0340–0349 and run isolated Postgres regressions in CI; Discord service/runtime activation remains a follow-up. Every owner mutation takes the audit row lock before evaluating token, generation and the current lease, and preflight-only failures park rows out of queue discovery for a 60 s backoff without counting a POST attempt. Deferral and failure releases stamp a fairness yield fixing queue position at release time, so expired backoffs rotate past repeatedly failing rows while aged retries keep position ahead of later arrivals instead of being starved by continued fresh rows, and a lock-free eligibility precheck keeps ineligible claims from waiting on a row lock while holding the shared halt guard.
- Port the website-to-bot internal-actions auth core (HMAC-SHA256 rotation-aware signing, skew + nonce replay guard, post-verify token buckets, 19-verb allowlist with env flags, settings catalog guard, bind guard, `authorize` pipeline) as framework-free domain logic. HTTP route, durable stores, and Discord execution remain follow-up slices.
- Framework-free LFG role parsing, future start-time validation, signup capacity decisions, select-menu data, permission checks, and message rendering.
- PostgreSQL LFG persistence and migration `0170`, preserving legacy table and column names; guild-fenced post writes and serialized capacity/close transactions.
- LFG regression tests against an isolated PostgreSQL CI service. Runtime command/component registration and Discord side effects remain dependent on the S4 interaction-router and REST-executor slices.
- Port leveling XP awards and per-source cooldowns to a transactional sqlx store over the imported tables, with legacy rank/leaderboard replies, idempotent reward-role plans and isolated Postgres parity tests. Router, REST and async gateway wiring remain follow-up integration work.
- Presence probe, weekly community scorecard, and inactivity flagging as framework-free domain logic with feature-gated Postgres stores and migrations 0310–0311, verified against a golden scorecard from the legacy build: hourly presence series with 24 h bot-floor re-list and the reopen trigger, Monday 06:15 UTC closed-week runs with fail-closed coverage, and an hourly read-only quiet-member sweep that never messages.
- Port join-burst detection, join-risk scoring and mention-suppressed staff alert proposals to the Rust domain core, with occurrence/processing clock boundaries and mock acceptance. No gateway, durable store, alert delivery or anti-nuke activation is added.
- Port game/session picker decisions, legacy/session/anchor welcome modes, rules-gate prompt eligibility and mention-free session goodbyes to the Rust domain core. Preserve legacy funnel rows with a sqlx prompt guard, migration 0190 and isolated agent-testdb/mock delivery tests. Runtime router/REST wiring remains a follow-up.
- Port sticky-message domain logic, debounce claims and PostgreSQL persistence, with a legacy timestamp upgrade and UTF-16-compatible body limits. Discord command and REST wiring remains in the S4 integration slices.
- Feed command and polling plans, bounded RSS/YouTube/Twitch XML parsing, public-IP/redirect fetch policy, and guild-scoped, fenced delivery storage with crash-reconciliation outcomes. Runtime transport wiring remains default-off pending the shared router and REST executor.
- RSVP domain logic and database store for going, interested and declined responses, namespaced attendance totals, and ManageEvents-gated host check-in facts, with legacy-compatible tables and replies.
- Port website-contract counter, rank and scheduled-events domain logic and transactional storage, with legacy-shaped read views. Runtime job wiring remains deferred.
- Port the guild-settings catalogue, 15-second poll contract, cache, sqlx store, and migration to Rust. Runtime ticker and interaction integration remain follow-up work.
- Run isolated settings persistence and concurrency regressions against a disposable Postgres CI service without credentials.
- Persist gateway session, resume URL and processed sequence across Container restarts. Commit funnel rows and checkpoints atomically, restore message milestones, discard stale sessions, and fall back to IDENTIFY when Discord invalidates a session.
- Channel moderation domain and SQL store for purge bounds, slowmode bounds,
  exact lockdown overwrite recovery, refusal of unlock without recorded state,
  generation-fenced idempotency claims and audit rows. Router/REST execution wiring follows when
  the shared S4 seams are merged.
- Fence lockdown recovery cleanup to the generation that was restored, so a delayed
  unlock (or a retried cleanup whose earlier result was lost) reports stale instead of
  deleting a later lockdown cycle's seed. Repeated lockdowns preserve the original
  generation alongside the original seed (migration 0122).
- Member moderation domain for ban, tempban, kick, timeout and warn, with
  idempotent claims, durable scheduled-unban recovery and audit/warning ledgers.
- Feature-gated Postgres moderation store and isolated test-container CI coverage.

### Fixed

* **build:** refresh stale Cargo.lock so --locked Docker build passes ([#19](https://github.com/TogetherWeOwn/two-bot-next/issues/19)) ([1aa9ce8](https://github.com/TogetherWeOwn/two-bot-next/commit/1aa9ce819d802fe8b9f387eb4cae329d8af5f9d2))
* **config:** isolate environment parser tests ([#52](https://github.com/TogetherWeOwn/two-bot-next/issues/52)) ([32ef2b6](https://github.com/TogetherWeOwn/two-bot-next/commit/32ef2b62226e9778ec879bc0d019b88bcfe1b4f7))
* **deploy:** wire explicit Container/DO bindings for staging and production ([#8](https://github.com/TogetherWeOwn/two-bot-next/issues/8)) ([66884ff](https://github.com/TogetherWeOwn/two-bot-next/commit/66884ff523bef839df94abb020e340c1fef7b751))
* **gateway:** park until checkpoint prerequisites are configured ([#59](https://github.com/TogetherWeOwn/two-bot-next/issues/59)) ([bd8d415](https://github.com/TogetherWeOwn/two-bot-next/commit/bd8d4155139c655e0edfec20520ce4ef85d850d2))
* **release:** preserve bootstrap Notes tail in first release ([#62](https://github.com/TogetherWeOwn/two-bot-next/issues/62)) ([9d1cc1d](https://github.com/TogetherWeOwn/two-bot-next/commit/9d1cc1d58f238bfb49698a5e51cfe0f6bac100bc))
* **worker:** forward environment on automatic container starts ([#21](https://github.com/TogetherWeOwn/two-bot-next/issues/21)) ([5436321](https://github.com/TogetherWeOwn/two-bot-next/commit/543632175675a642974ded84e18983da4c41d5e6))

- Match legacy ECMAScript whitespace trimming for LFG roles, slot numbers, and titles, including BOM and NEL edge cases.
- Detect settings changes with a commit-ordered transactional revision instead of a sequence maximum, including deletes and late commits with lower row versions.
- Serialize settings reads, writes, and audits, including concurrent inserts into absent keys; load cache rows and revision from one consistent database snapshot.
- Refuse environment-only and unknown settings through the cache getter as well as environment snapshots.
- Enforce append-only settings audit data for updates, deletes, and truncation.
- Render integral JSON settings as integer environment strings so decimal/exponent thresholds survive database round-trips into config readers, without rounding integer IDs through floating point.
- Serialize concurrent first RSVP responses before reading the previous status, including when no response row exists yet.
- Fail configured RSVP database-test setup errors instead of silently skipping, and isolate each test invocation in its own schema.
- Preserve isolated bot schemas when applying the website contract, without rebinding the public read views.
- Refuse non-test targets before resetting the website-contract acceptance database.
- Reject malformed scheduled-event timestamps without panicking or replacing the last good mirror.
- Recover gateway sessions rejected with close codes 4007/4009, preserve the committed READY URL after endpoint fallback, and exit for Container restart when the essential gateway task stops instead of serving a healthy zombie.
- Bound total checkpoint SQL waits to a heartbeat-safe deadline and report readiness unavailable while persistence is pending; fail closed and restore from committed state after a slow-database restart.
- Scope unban recovery to its guild, fence older expiries with durable ban
  generations, and recover only explicitly accepted bans rather than guessed
  staging. Permanent bans supersede older tempbans; failed refusal cleanup keeps
  a reconciliation fence instead of scheduling an unsafe unban.
- Claim sweep jobs individually so cancellation cannot strand an undispatched
  batch, and bound the final generated expiry reason with UTF-16-compatible,
  Unicode-safe truncation.
- Refuse new bans while a dispatched unban remains uncertain; confirmation cannot
  erase running claims, which require exact-token authoritative reconciliation.
- Upgrade legacy moderation TEXT timestamps in place without reactivating
  quarantined imports, audit accepted effects before completion writes, and
  release idempotency keys after definite pre-dispatch transaction rollback.
- Audit accepted ban PUTs before confirmation/activation and scheduled DELETEs
  while their dispatch claims remain held; preserve the required expiry after
  an authoritatively voided DELETE, and retry never-dispatched fence refusals
  without releasing the older uncertain operation.
- Preserve ban ownership, generations and expiry eligibility in consistent
  backups; replace stale destination ownership and reset generation sequences
  on restore. Old v3 restores quarantine unknown expiries without inventing
  acceptance or discarding running DELETE fences.
- Redact unban claim tokens from diagnostics, lock member migrations to their
  unchanged checksums, and isolate moderation database CI on self-hosted runners.

### Notes

- Command/component wiring and the Discord REST reads stay on the S4 interaction router and REST executor slices; the outcome enums are the integration surface until they land. Scorecard and probe collection are not enabled by this change.
- Runtime registration, the shared REST executor's 5-second abort and the
  30-second scheduler remain gated on the S4 integration slices; moderation
  is not enabled by this change.
