# Staging 0481 pre-change recovery

The staging recovery inventory ran on 2026-11-22 at 03:04:42 UTC through
the main-only `staging-migrate-plan` environment as
`two_bot_migrator_ro`. The workflow read catalogs and both migration
ledgers. Application rows and credential material were outside the
capture scope.

| Evidence | Verified value |
| --- | --- |
| Capture source | `f74dc4eb696a4446eba0df7d948a08bc4f314f4b` |
| Capture job | [44978937903, attempt 1](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/44978937903/job/137074962973) |
| Encrypted inventory | [Artifact 14736996293](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/44978937903/artifacts/14736996293) |
| Artifact archive SHA-256 | `f00e8dc7608ca429c799da57b29ac35c9fc44a3c7d00fea21c9ceb9cb711e58b` |
| Inventory SHA-256 | `3640a5616dd3b7b52db1637b8b025286a229ea30e2d8df9a8cf4250810f17c3e` |
| Host, database and branch fingerprint | `73f8984e07f48f9787c5905d0459588330ddcbd103841fa84107fae7ee7b87dd` |
| Migration 0481 SHA-384 | `bf82ecee86a8d0d2508cf36a2419f745b6a3a95508c4dce136f893c593e208fccc96f18dd12d3bcee48dd5c1a86e4b7b` |

The artifact archive matched GitHub's digest. The decrypted inventory
matched the digest printed by the capture job. A protected copy retains
the full target pins, definitions, ownership, object and default ACLs,
login flags, memberships, and ledger metadata. The target fingerprint
hashes the UTF-8 JSON object with sorted keys and compact separators.

The inventory records 151 relations in scope, 1,037 columns, 1,095
constraints, 219 indexes, six routines, two extensions, 24 roles and 26
memberships across the bot's `public` and `web_v1` schemas and their
associated role inventory. The capture reported no missing or unsupported
objects in scope.

The target and complete ledger version, description, checksum and success
records matched the [prior read-only staging plan](https://github.com/TogetherWeOwn/two-bot-next/actions/runs/44970808060/job/137049953844).
The cutover ledger had 117 successful rows through 0480. The store ledger
had 21 successful rows through 0478. Neither ledger had failed rows.

Before 0481, `public.internal_action_log.intent_id` was required. The
existing outcome check was validated, and the delivery hash, source
channel, request ID and receipt check were absent. The reviewed
[0481 migration](../crates/cutover/migrations/0481_internal_voice_move_audit.sql)
adds nullable receipt fields and uses named `NOT VALID` checks. The
receiver remains disabled. Use the reviewed
[ACL sequence](database-roles.md#operator-boundary-and-deployment-order).

Use this receipt as `docs/staging-0481-recovery.md` in the final
read-only plan, and bind apply to that plan through the
[staging apply claim](staging-migrate-claim.md). The board confirms that
exact binding before dispatch. Capture ran without migration or deployment.
