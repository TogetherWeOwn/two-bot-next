# Raid response: operator evidence and removal

Port of the legacy [RAID-RESPONSE.md](https://github.com/TogetherWeOwn/two-bot/blob/main/docs/RAID-RESPONSE.md), [raid-list.ts](https://github.com/TogetherWeOwn/two-bot/blob/main/scripts/raid-list.ts), [raidRemoval.ts](https://github.com/TogetherWeOwn/two-bot/blob/main/src/moderation/raidRemoval.ts) and [decision tests](https://github.com/TogetherWeOwn/two-bot/blob/main/test/unit.raidremoval.test.ts). Historical raid counts are not current authorization and are not embedded in these tools.

## Look before acting

Discord verification and membership screening govern participation, not membership. An old farm account can join and never accept the rules. AutoMod's content rules do not prevent a join raid. Conversely, a genuine invite surge can resemble a raid: a risk flag is evidence to review, not proof that someone should be removed.

The core [raid-watch and join-risk contracts](raid-port.md) remain alert/flag-only. This tool is separate, manually invoked, and does not enable containment runtime or scheduled removal. Review attribution and activity, authorize a specific cohort, and use Discord's reversible invite pause if necessary. Server safety-setting changes are a separate operator decision.

## Two tools, two decisions

`raid-list` exports flagged join evidence for an explicit guild and half-open UTC window `[--from, --to)`. JSON and CSV carry string snowflakes and provenance; no usernames or message contents are fetched. Protect these files as member metadata, do not commit them, and prune them under your operational retention policy. Exporting a list never kicks, bans or messages anyone.

`raid-remove` reads a reviewed file; it never discovers additional targets. The default is a **network-free dry run**: no Discord client, token or membership request is constructed. A dry run reports intent, not a verified promise that every target is removable. Execution re-reads membership and protection immediately before each kick, and may keep members that changed roles after the report.

Execution requires `--execute --expect N`, where N is the exact unique input count, and a moderation `--reason`. Neither an environment variable nor a risk score can arm removal. The live-guild fence must also be deliberately opened with `--allow-live-guild`; that flag is a technical guard, not moderation authorization or cutover approval.

It **kicks, never bans**. A kicked account can rejoin through the rules gate. There is no automatic escalation from kick to ban.

## Audit and resume

Keep one audit file per approved cohort. Every reached target has an outcome, including dry-run intent, protection refusals, missing members and transport failures. The file is appended and fsynced before the next target is considered. A write failure ends the run; it must never silently proceed without an audit.

Execute outcomes `kicked` and `already_gone` are terminal for that guild/member in that audit file. A later dry-run line cannot re-arm an earlier kick. Resume with the same input and audit file: settled targets are skipped without Discord requests; failures remain retryable. If an approved target genuinely rejoins, review it as a new cohort with a new audit file. A crash between kick and audit converges through a fresh membership 404 on resume.

The shared executor preserves legacy kick pacing (350 ms), rate-limit handling, and four retries. Three consecutive member failures abort the cohort rather than repeating a systemic error down the whole list. Authentication/permission refusal stops immediately: do not try a different token or hunt for a credential. Fix access through its authorized provisioner before resuming.

Do not run simultaneous removals using the same audit file. The tool holds an exclusive process lock for the audit lifetime and refuses concurrent use; inspect a stale lock after a crash before deliberately clearing it.

## Verification boundary

Tests use pure fixtures, loopback mock REST, and disposable agent-testdb/CI service containers behind the existing test-database guard. No test, probe or verification belongs against staging or production databases or guilds. Shipping the binaries is not evidence of a live removal, deployment, or containment activation.

SQL parameters are bound rather than interpolated, following the [SQLx 0.9 prepared-query contract](https://docs.rs/sqlx/0.9.0/sqlx/fn.query.html#dynamic-input-parameter-binding-with-querybind).
