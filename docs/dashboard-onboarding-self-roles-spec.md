# Dashboard: onboarding and self-roles admin page

Status: **proposed, design only**. Written 2026-10-24; review by 2026-10-31.
No runtime, migration, dashboard edit, live write or experiment is added.
Merge publishes a proposal, not permission to build or activate it. This file is
the single repository contract; review requests bind its commit and SHA-256.

## 1. Decision and evidence

Consumer: a guild admin setting up member onboarding and self-role pickers.
Value: what they save on the dashboard is what the bot does, with no slash
command archaeology and no operator ticket. No customer validation is claimed.

Inspected repository head: `89b26ae050ffba9fb409622ffd54d31cdc79e7fc`. The web
repository, a live guild, permissions and deployments were **not** inspected.

| Option | Cost and value | Disposition |
| --- | --- | --- |
| A. Build the page over every key now | Fastest; but one key stores a value no runtime reads, and five keys store unvalidated IDs that can fail onboarding jobs | Reject: the live-apply map is not clean |
| B. Narrow wiring first, then page the evidenced keys (**recommended**) | Four small bot cards (section 7) before or beside the page; the page writes only keys with live-apply evidence | Recommend for intent review |
| C. No build; operators edit settings | No new write surface; admins keep asking operators | Safe default if B is not accepted |

Result of the per-key check (section 3): **7 of 15** keys are writable with live
evidence (1 needs nothing, 5 need only write-time validation, 1 lands already),
**1** is stored but read by nothing, **2** are environment-only and **5** have no
reader in this bot. Review rule from the card: proceed only if the map is
evidenced per key. It is, so B is recommended; the self-role dry-run toggle stays
read-only until its wiring card lands.

## 2. Verified baseline

- **One store.** `guild_settings(guild_id,key,value,version,cas_version,
  updated_at,updated_by)` with append-only `guild_settings_audit` and a
  transactional poll revision (`crates/cutover/migrations/0330_guild_settings.sql`-`crates/cutover/migrations/0334_guild_settings_cas.sql`). The
  writer is `SettingsStore::set_if_version` (`crates/cutover/src/settings.rs`):
  CAS compare `:203-209`, self-role drain check `:214-241`, audit insert
  `:277-288`, all in one transaction.
- **One verb pair.** `settings.get` / `settings.set`
  (`crates/core/src/internal_settings.rs:35-75`; receiver
  `crates/bot/src/internal_action_http.rs:2614-2727`). `get` returns
  `{key,value,source}` plus the CAS `version` (`:2782`); `set` takes optional
  `expected_version`, `updated_by` (a Discord ID) and an idempotency key.
  Key classification fails closed (`crates/core/src/settings.rs:87-270`).
- **The receiver pins one guild.** Both handlers use the configured guild
  constant (`internal_action_http.rs:2626`, `:2662`) and ignore any body guild.
- **Onboarding state is not configuration.** `onboarding_store.rs` holds prompt
  guards and funnel facts (`begin_prompt`, `record_*`) over the legacy `events`
  table. Nothing on the page reads or writes it.
- **No slash command writes these keys.** Searching `crates/discord/src`,
  `commands.rs` and `router` for the settings store finds no writer. Other
  writers are operator-side imports (`legacy_copy`, `rollback_import`).
- **Self-roles are fenced.** The service exists only when the activation fence
  clears `self_roles` (`crates/bot/src/main.rs:495-499`), the boot catalogue is
  non-empty and the guild is in a one-guild staging allowlist
  (`self_role_handlers.rs:44-46,130,189`).
- **No read path for "last changed by/at".** `SettingsStore::get` selects
  value and CAS only (`settings.rs:132-146`); the web reader role has no view of
  `guild_settings` or its audit (`docs/database-roles.md`).

## 3. Live-apply matrix, per key and store

Source: `SETTING_CLASSES` (`crates/core/src/settings.rs:87-270`) and `HOT_WIRED`
(`:277-312`). "Live" means a stored row changes behavior with no restart.

| Key | Class / hot-wired | Apply path and evidence | Disposition |
| --- | --- | --- | --- |
| `TWO_SELF_ROLE_PANELS` | hot / yes | Resolved from the live snapshot on every dispatch and sweep, boot fallback on absent or unparsable row (`self_role_handlers.rs:59-83`); landed by the merged live-keys change (`docs/ticket-feed-self-role-live-keys-acceptance.md`). Edits and deletes refuse while durable work drains. Excluded from the /ops overlay (`settings.rs:366-369`) | **Live now** |
| `TWO_SELF_ROLE_DRY_RUN` | hot / no | `dry_run` is set once at service construction (`self_role_handlers.rs:117,139`) from `SelfRoleGates::from_env`, which reads the process environment only (`self_roles.rs:776`). A stored row is read by nothing, not even after restart. `docs/configuration.md:297` says "stored unwired" | **Wiring card W2**; read-only until then |
| `TWO_ONBOARDING_DRY_RUN` | hot / no | `OnboardingRuntime::config` reloads a fresh DB snapshot at the start of each job and interaction, 1.5 s timeout (`bot/src/onboarding.rs:276-300,456`); only exact `0`/`1` merges (`:164`). Pinned by `onboarding_runtime_hotpath_config_materializes_only_its_guild` (`onboarding_tests.rs:267`) | **Live now** |
| `DISCORD_LANDING_CHANNEL_IDS`, `DISCORD_GOODBYE_CHANNEL_IDS` | hot / yes | Same per-event reload (`onboarding.rs:21-30,186-205`). No write-time validation, and a malformed stored row makes `config()` fail every job and picker (`:299`, `:427-440`) | Live; **W1** before the page writes them |
| `DISCORD_ANCHOR_WELCOME_CHANNEL_ID` | hot / no | Same reload; consumed only in anchor mode (`discord/src/onboarding_config.rs:67-72`) | Live; **W1** first |
| `DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID`, `DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID` | hot / no | Same reload; both required, and consumed only, in session mode (`onboarding_config.rs:73-84`) | Live; **W1** first |
| `TWO_ONBOARDING_MODE` | env_only | Deployment selects legacy/session/anchor; stored rows never override it (`onboarding.rs:186-195`). `settings.get` refuses env-only keys, so the page cannot show the value | Read-only with reason |
| `DISCORD_GUILD_ID` | env_only | Gateway identity; excluded from the stored merge | Read-only with reason |
| `TWO_ONBOARDING_ROTA_MEASUREMENT`, `_NOTICE`, `_PSEUDONYM_KEY`, `_PRIMARY_ACTOR_ID`, `_READER_IDS` | env_only | Rota runtime dropped; no reader in this repository; refusal kept (`settings.rs:168-177`) | "Not available"; the secret is never rendered |

Findings the matrix exposes, all evidenced above:

1. **`TWO_SELF_ROLE_DRY_RUN` is inert.** `settings.set` accepts it (it is Hot and
   passes the 0/1 check, `settings.rs:1030-1033,1083`), so a naive page would
   show "Saved" for a value that no code reads. A dry-run toggle that silently
   does nothing is the worst case for a safety control.
2. **Channel IDs are unvalidated at write time.** A bad value is stored, then
   breaks every welcome job and picker until corrected. Mode-dependent
   requirements (session mode needs both session channels) are invisible to the
   page because the mode is environment-only.
3. **Four onboarding keys report "restart required" while applying live.** The
   poll reports non-`HOT_WIRED` hot keys as cold (`settings.rs:648-690`), logging
   `settings_restart_required`, yet onboarding reads them per event. Operators
   reading logs would be misled; this is wiring card W3.
4. **/ops overrides never reach onboarding.** The per-event reload builds a fresh
   `SettingsCache::load` with an empty override layer (`settings.rs:556-560`), so
   an override of the landing or goodbye key does not change onboarding. The
   page therefore needs no override banner for these keys.
5. **Unmeasured.** Real apply latency, a parked runtime, and whether the roles or
   channels in a saved value exist in the guild. The server checks shape only.

## 4. Page contract

Built over the moderation page pattern (same shell, same signed client, same
guards); this document does not redefine that shell or the voice page.

- **Scope.** One page, two sections: Onboarding and Self-roles. Nothing else.
  Tickets, announcements/feeds, retention, the /ops override panel and every
  other key are out.
- **Row states** (stable names in the manifest, section 6):
  `writable` (editable, live-apply evidenced), `read_only_unwired` (stored but
  read by nothing; no edit control), `read_only_env` (deployment-managed; shows
  the reason, never a value), `not_available` (no runtime reader; says so).
  Nonexistent configuration is shown as "not available", never invented, and
  never as an empty or default value.
- **Writes** go through the existing signed internal-action client with a fresh
  idempotency key per save and the `expected_version` read for that row. Save
  and "unset" are different buttons; unset sends `null` and hands the key back to
  the deployment value. The admin's Discord ID is `updated_by`.
- **Attribution.** Every committed change records `updated_by` in the row and one
  audit row. Showing the *previous* editor needs wiring card W4; until then the
  page shows only "saved by you just now" and "history is kept in the audit
  trail", never a guessed name.
- **Preview.** Before sending, the page shows a pure, local preview: the
  validator verdict, old and new rendering, and the apply path with its bound. It
  makes no network call and signs nothing. It is unrelated to the
  `*_DRY_RUN` keys.
- **Mode-dependent keys.** Anchor and session channel rows say "used only when
  onboarding runs in anchor/session mode; the mode is set by deployment and not
  shown here". The page never infers or shows the mode.
- **Self-role catalogue.** The editor produces the exact JSON the strict parser
  accepts; the server's refusal is the authority. The page does not duplicate the
  parser and cannot check that roles or channels exist (finding 5).

## 5. Write-through, isolation and failure UX

### Apply latency and honest copy

| Path | Bound (design, unmeasured live) | Copy after a successful save |
| --- | --- | --- |
| Onboarding keys | The next job or picker interaction after commit; the reload times out at 1.5 s | "Saved. Applies to the next member who joins or opens the menu." |
| Self-role catalogue | One 15 s poll (`POLL_SECONDS`, `settings.rs:51`) after commit; a failed poll keeps the last good snapshot | "Saved. Applies within about a minute." |

The page never says "applied" or "live": it cannot observe the runtime. A staging
apply-check before any ship is mandatory (section 8).

### Conflict and version behavior

Guild rows have **no epoch**; the /ops epoch applies only to the global override
layer, which does not reach these keys (finding 4, plus the panels exclusion).
Conflict control is the CAS token: opaque, compared only for equality, `0` means
"expect absent". A stale token returns non-retryable 409
`settings_version_conflict` before any value, audit or revision change. Two
admins, or an admin and an operator writing the same row, resolve as: first
commit wins, the second gets the conflict and a re-read; there is no merge.
Tokens survive restarts; migration `0334` invalidated older tokens, so clients
re-read after upgrade. Replaying one idempotency key returns the first result
with no second write or audit row; a replay whose side-read fails returns 409
`needs_reconciliation`, never a fabricated success.

### Audit facts

One `guild_settings_audit` row per committed change: guild, key, old value, new
value, `updated_by`, time. A refused write, a conflict, a replay and a preview
add none. Values are channel IDs, gate bits and the panel catalogue, never
secrets. Audit retention is the moderation page's open item; this page adds no
second one.

### Guild isolation

The receiver executes against its one configured guild and ignores the body
(section 2). Therefore the page must, **before signing anything**, require that
the session's guild equals the guild this bot deployment serves, and otherwise
show "This server is not managed by this bot deployment" (`FOREIGN_GUILD`). The
web-side guard (admin session plus live Manage Server check) is inherited from
the shell and **unverified here**. Self-roles are additionally served only for a
one-guild staging allowlist, so the self-role section is staging-scoped until a
separate activation decision exists.

### Failure UX

| State | Detect | Copy |
| --- | --- | --- |
| Stale read | 409 `settings_version_conflict` | "Someone else changed this setting. Showing the current value; your edit is kept below. Nothing was saved." |
| Failed write, definite | 4xx with a reason | Field-level reason; never echo the submitted value |
| Failed write, ambiguous | timeout, 5xx, 409 `needs_reconciliation` | "We could not confirm whether this saved. Reload before trying again." Retry only with the **same** idempotency key, or re-read first |
| Unavailable runtime | signer timeout or 5xx before any response | "The bot did not answer. Nothing was changed as far as we know." No optimistic update |
| Self-role work pending | `settings_self_role_recovery_pending` | "Role changes from members are still being finished. Try again in a few minutes." |
| Read-only row | state in the manifest | Reason from section 6; no control |
| Wrong server | `FOREIGN_GUILD` | Section 5 isolation copy; no request sent |

## 6. Agent contract

One repo source (this file), one JSON manifest below, stable key names, a
`reason` and `next_action` for every result, and a local preview. No new
executable CLI or API is registered; adapters discover this file through the
[documentation map](README.md). Results carry `schema`, `guild_id`, `key`,
`reason`, `retryable`, `next_action`, and never a submitted value.

```json
{
  "schema": "dashboard.onboarding_selfroles.v1",
  "keys": [
    {"key": "TWO_ONBOARDING_DRY_RUN", "group": "onboarding", "class": "hot", "hot_wired": false, "state": "writable", "gate": "none", "apply": "per_event_db_read"},
    {"key": "DISCORD_LANDING_CHANNEL_IDS", "group": "onboarding", "class": "hot", "hot_wired": true, "state": "writable", "gate": "W1", "apply": "per_event_db_read"},
    {"key": "DISCORD_GOODBYE_CHANNEL_IDS", "group": "onboarding", "class": "hot", "hot_wired": true, "state": "writable", "gate": "W1", "apply": "per_event_db_read"},
    {"key": "DISCORD_ANCHOR_WELCOME_CHANNEL_ID", "group": "onboarding", "class": "hot", "hot_wired": false, "state": "writable", "gate": "W1", "apply": "per_event_db_read"},
    {"key": "DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID", "group": "onboarding", "class": "hot", "hot_wired": false, "state": "writable", "gate": "W1", "apply": "per_event_db_read"},
    {"key": "DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID", "group": "onboarding", "class": "hot", "hot_wired": false, "state": "writable", "gate": "W1", "apply": "per_event_db_read"},
    {"key": "TWO_ONBOARDING_MODE", "group": "onboarding", "class": "env_only", "hot_wired": false, "state": "read_only_env", "gate": "none", "apply": "boot_env"},
    {"key": "DISCORD_GUILD_ID", "group": "onboarding", "class": "env_only", "hot_wired": false, "state": "read_only_env", "gate": "none", "apply": "boot_env"},
    {"key": "TWO_ONBOARDING_ROTA_MEASUREMENT", "group": "onboarding", "class": "env_only", "hot_wired": false, "state": "not_available", "gate": "none", "apply": "none"},
    {"key": "TWO_ONBOARDING_ROTA_NOTICE", "group": "onboarding", "class": "env_only", "hot_wired": false, "state": "not_available", "gate": "none", "apply": "none"},
    {"key": "TWO_ONBOARDING_ROTA_PSEUDONYM_KEY", "group": "onboarding", "class": "env_only", "hot_wired": false, "state": "not_available", "gate": "none", "apply": "none"},
    {"key": "TWO_ONBOARDING_ROTA_PRIMARY_ACTOR_ID", "group": "onboarding", "class": "env_only", "hot_wired": false, "state": "not_available", "gate": "none", "apply": "none"},
    {"key": "TWO_ONBOARDING_ROTA_READER_IDS", "group": "onboarding", "class": "env_only", "hot_wired": false, "state": "not_available", "gate": "none", "apply": "none"},
    {"key": "TWO_SELF_ROLE_PANELS", "group": "self_roles", "class": "hot", "hot_wired": true, "state": "writable", "gate": "none", "apply": "live_poll_snapshot"},
    {"key": "TWO_SELF_ROLE_DRY_RUN", "group": "self_roles", "class": "hot", "hot_wired": false, "state": "read_only_unwired", "gate": "W2", "apply": "none"}
  ],
  "reasons": {
    "SAVED": {"retryable": false, "next_action": "none"},
    "UNSET": {"retryable": false, "next_action": "none"},
    "PREVIEW": {"retryable": false, "next_action": "submit_or_edit"},
    "INVALID_VALUE": {"retryable": false, "next_action": "fix_value"},
    "REFUSED_ENV_ONLY": {"retryable": false, "next_action": "ask_operator"},
    "REFUSED_UNKNOWN": {"retryable": false, "next_action": "none"},
    "NOT_AVAILABLE": {"retryable": false, "next_action": "none"},
    "NOT_WIRED": {"retryable": false, "next_action": "wait_for_wiring"},
    "VERSION_CONFLICT": {"retryable": false, "next_action": "reread_then_resubmit"},
    "RECOVERY_PENDING": {"retryable": true, "next_action": "retry_after_drain"},
    "RUNTIME_UNAVAILABLE": {"retryable": true, "next_action": "retry_same_idempotency_key"},
    "COMMIT_UNKNOWN": {"retryable": false, "next_action": "reread_before_retry"},
    "FOREIGN_GUILD": {"retryable": false, "next_action": "none"}
  }
}
```

Preview shape (pure, deterministic, offline):
`{schema, guild_id, key, reason, retryable, next_action, state, would: save|unset|refuse, apply, bound}`.

## 7. Wiring follow-ups (named, not built)

| Card | One-PR scope | Needed for |
| --- | --- | --- |
| W1 | Refuse malformed onboarding channel IDs at write time, using the runtime's own `channel_ids` parser, with typed `settings_value_invalid` | Page writes the five channel keys |
| W2 | Apply `TWO_SELF_ROLE_DRY_RUN` live: store-first per-operation read, boot fallback, last-good on a bad row, plus pins | Making the dry-run toggle writable |
| W3 | Report the four non-wired onboarding keys as live, not restart-required, without widening the /ops overlay | Honest operator logs; not a page blocker |
| W4 | Return `updated_by` and `updated_at` from `settings.get` (value-free metadata); shared with the moderation page | Showing the previous editor |

Build order if accepted: W1 and W2 first (parallel), then one web page card.
A page card may ship earlier for `TWO_ONBOARDING_DRY_RUN` and
`TWO_SELF_ROLE_PANELS` plus the read-only rows, leaving channel rows disabled.

## 8. Offline fixture oracle and runnable check

Each row resets the fixture: guild `g1` (and foreign `g2`), a disposable test
database, an injected clock, no network. "Read-back" is the runtime reader named
in section 3, not only `settings.get`. All rows require zero Discord calls.

| Id | Case | Expected |
| --- | --- | --- |
| F01-F07 | Write-then-read-live per writable key: set a valid value, `get`, advance the injected clock through one 15 s poll for the panel catalogue, then read through the runtime reader; then unset | `SAVED`; runtime sees the value; unset returns deployment value; exactly 2 audit rows |
| F08 | `TWO_ONBOARDING_DRY_RUN` set to `"true"` | `INVALID_VALUE`; 0 rows, 0 audit |
| F09 | `TWO_SELF_ROLE_DRY_RUN` via the page | `NOT_WIRED`, nothing signed; a directly stored row leaves `dry_run` unchanged (flips when W2 lands) |
| F10 | EnvOnly refusal: mode, guild ID, each rota key, set and get | `REFUSED_ENV_ONLY`; 0 rows, 0 audit |
| F11 | Unknown key | `REFUSED_UNKNOWN`; 0 rows |
| F12 | Malformed channel list (`"0"`, text, overflow) | Today stored and fails `config()`; after W1 `INVALID_VALUE` |
| F13 | Malformed panel catalogue | `INVALID_VALUE`; 0 rows |
| F14 | Panel edit while a member's role change is processing | `RECOVERY_PENDING`; 0 rows; succeeds after drain |
| F15 | Admins A and B read token `t`; A saves, B saves with `t` | A `SAVED`; B `VERSION_CONFLICT`; B's value absent; 1 audit row |
| F16 | Two creators both expect token `0` | One `SAVED`, one `VERSION_CONFLICT` |
| F17 | Dashboard write vs another signed caller with a different `updated_by` (no slash writer exists today) | Last commit wins; each audited under its own actor; stale token conflicts |
| F18 | Replay one idempotency key | One row, one audit row, same result |
| F19 | Replay whose side-read fails | `COMMIT_UNKNOWN`; never `SAVED` |
| F20 | Restart: tokens persist; onboarding reads the DB; panels before the first poll use the boot catalogue, after it the stored one | Matches |
| F21 | Row deleted, then restart | Deployment value returns |
| F22 | `g2` row present, `g1` runtime reads | `g1` unaffected; non-pinned session guild gets `FOREIGN_GUILD`, 0 requests |
| F23 | /ops override of the landing key | Onboarding value unchanged; panels key excluded |
| F24 | Signer or runtime timeout before a response | `RUNTIME_UNAVAILABLE`; UI shows the old value |
| F25 | Preview of any key | Deterministic output; 0 network calls, 0 signatures |

Runnable offline document check (Python stdlib, from the repo root):

```sh
python3 -m doctest -v docs/dashboard-onboarding-self-roles-spec.md
python3 scripts/check_docs_index.py --json
```

The doctest reads the manifest from this file and checks it against
`crates/core/src/settings.rs`: every key's class and hot-wired flag, state,
gate and apply consistency, and reason/next_action presence. It checks no
fixture row and does **not** run the bot, a database, a dashboard, or any apply
path. Future protected acceptance tests must implement
the oracle against real callers in a disposable database; zero tests is not
proof.

```pycon
>>> import json, pathlib, re
>>> fence = "`" * 3
>>> doc = pathlib.Path("docs/dashboard-onboarding-self-roles-spec.md").read_text()
>>> manifest = json.loads(re.search(fence + r"json\n(.*?)\n" + fence, doc, re.S).group(1))
>>> src = pathlib.Path("crates/core/src/settings.rs").read_text()
>>> table = src.split("pub const SETTING_CLASSES", 1)[1].split("\n];", 1)[0]
>>> classes = dict(re.findall(r'\(\s*"(\w+)",\s*SettingClass::(\w+),?\s*\)', table))
>>> wired = set(re.findall(r'"(\w+)"', src.split("pub const HOT_WIRED", 1)[1].split("\n];", 1)[0]))
>>> rows = manifest["keys"]
>>> assert len({r["key"] for r in rows}) == len(rows) == 15
>>> bad = [r["key"] for r in rows if classes.get(r["key"]) != {"hot": "Hot", "env_only": "EnvOnly"}[r["class"]]]
>>> assert not bad, bad
>>> bad = [r["key"] for r in rows if (r["key"] in wired) != r["hot_wired"]]
>>> assert not bad, bad
>>> assert all(r["class"] == "hot" for r in rows if r["state"] in ("writable", "read_only_unwired"))
>>> assert all(r["class"] == "env_only" for r in rows if r["state"] in ("read_only_env", "not_available"))
>>> assert all(r["gate"] == "W2" for r in rows if r["state"] == "read_only_unwired")
>>> assert all(r["apply"] == "none" for r in rows if r["state"] in ("read_only_unwired", "not_available"))
>>> assert all(v["next_action"] and isinstance(v["retryable"], bool) for v in manifest["reasons"].values())
>>> states = {s: sum(r["state"] == s for r in rows) for s in ("writable", "read_only_unwired", "read_only_env", "not_available")}
>>> print(json.dumps(states, sort_keys=True))
{"not_available": 5, "read_only_env": 2, "read_only_unwired": 1, "writable": 7}
>>> print(json.dumps({"schema": manifest["schema"], "keys": len(rows), "status": "pass"}, sort_keys=True))
{"keys": 15, "schema": "dashboard.onboarding_selfroles.v1", "status": "pass"}

```

## 9. Acceptance boundary, metric and undo

Record CEO spec acceptance **and** host intent alignment against the exact
revision before any build card, protected test handoff or live pilot. Route the
decision brief through the President/COO to the CEO and the existing host intent
path; a queue receipt is not alignment, and nothing is sent to the owner or an
invented executor.

The decision accepts or revises: option B and its four wiring cards; the row
states and copy above; first ship against staging only; and the self-role
section staying staging-scoped. The CISO must give independent disposition on
two points this page widens: a dashboard write path for the self-role catalogue
(it can change which roles members receive), and the unvalidated-channel
hazard until W1. This document grants no credential, permission or activation.

By 2026-10-31 the metric is **all 25 oracle rows have agreed or rejected
outcomes and every manifest row has a state decision**. Proceed only if the
live-apply map stays evidenced per key; otherwise take the wiring-first
alternative (build W1-W4 and defer the page). With no decision the default is no
build (option C); there is no timeout approval. No live experiment was run here.

Unverified: web-repository guards and signer, real apply latency, behavior when
the onboarding runtime is parked, role/channel existence in the guild, and the
production guild (the receiver and self-roles are staging-pinned today).

ACT: publish this proposal and its offline check. WHY: bound the dashboard
write surface for onboarding and self-roles before anyone builds it. UNDO:
revert the docs commit; there is no operational change to undo.
