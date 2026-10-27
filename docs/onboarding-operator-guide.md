# Onboarding operator guide

How to run, check and triage the member-onboarding flow. This is the
operations companion to the port record ([onboarding-port.md](onboarding-port.md)),
which freezes the legacy parity source and the runtime integration contract.
This guide changes no behavior.

## Gate modes

`OnboardingMode` (`crates/core/src/onboarding.rs:49`) selects the welcome flow.
`TWO_ONBOARDING_MODE` is deployment-only (`env_only` in
[configuration.md](configuration.md)); stored rows never override it
(`crates/bot/src/onboarding.rs:130`).

| Mode | Value | Behavior |
|---|---|---|
| Legacy (default) | unset, empty, or `legacy` | Game-role picker plus hub routing. Level reward-role writes allowed. |
| Session | `session` | Roleless two-pick routing plus goodbye. Suppresses **all** role writes structurally: no game-picker handler is routed there, and leveling reward writes are off (`level_role_writes_allowed`, `game_picker_allowed` in `crates/core/src/onboarding.rs:78`). |
| Anchor | `anchor` | One-message Sunday Squad welcome. The picker stays live but silent: the welcome attaches nothing, not even a footer. |

`OnboardingMode::VALID` is exactly `legacy`, `session`, `anchor`
(`crates/core/src/onboarding.rs:62`).

### `OnboardingGates::from_map` env surface

Read `crates/core/src/onboarding.rs:119` as the authority. `from_env`
(`:114`) reads the same two keys from the process environment.

- `TWO_ONBOARDING_MODE`: unset, empty, or `legacy` means legacy;
  `session` or `anchor` select their mode. Any other value fails closed:
  `TWO_ONBOARDING_MODE must be exactly "legacy", "session" or "anchor"
  when set, got {value:?}`.
- `TWO_ONBOARDING_DRY_RUN`: unset or `0` means live; `1` means dry run.
  Anything else — including `true`, `yes`, `on`, `TRUE`, `""`, `01`,
  `" 1"` — fails closed: `TWO_ONBOARDING_DRY_RUN must be exactly "0" or
  "1" when set`. The error names the variable and never echoes the value.

Dry run is per-mode, not a universal mute (`crates/core/src/onboarding.rs:97`):
it suppresses game role writes, legacy/anchor welcomes and session
goodbyes. Roleless session welcomes and session-picker acknowledgements and
routing records still run. A bad deployment value is a fatal startup
failure (`onboarding_gates_invalid` on `/readyz`; see [Failure signals](#failure-signals-and-triage)).

### Per-mode channel config

`OnboardingConfig::from_map` (`crates/discord/src/onboarding_config.rs:62`)
adds the deployment routing fields. At the raw config API a missing
`DISCORD_GUILD_ID` returns `Ok(None)` — disabled, never falling back to a
production guild or channel — but that path is unreachable in the running
bot: `OnboardingRuntime::new` always fills `DISCORD_GUILD_ID` from the
gateway `GUILD_ID` (`crates/bot/src/onboarding.rs:201`), the Worker never
forwards `DISCORD_GUILD_ID` (absent from `FORWARDED_DISCORD_IDS`), and a
`None` config would fail closed as `RuntimeError::Config` (`:238`).
Unsetting `DISCORD_GUILD_ID` does not disable onboarding in the bot.

- Anchor mode requires `DISCORD_ANCHOR_WELCOME_CHANNEL_ID`, else
  `Missing("DISCORD_ANCHOR_WELCOME_CHANNEL_ID")`.
- Session mode requires `DISCORD_SESSION_LOOKING_TO_PLAY_CHANNEL_ID` and
  `DISCORD_SESSION_LOBBY_VOICE_CHANNEL_ID`, else `Missing` for the absent
  key. The two-option catalog is built per guild from those ids
  (`build_session_picks`); the `STAGING_*` constants in
  `crates/core/src/onboarding.rs:44` pin the staging proof only.
- `DISCORD_LANDING_CHANNEL_IDS` and `DISCORD_GOODBYE_CHANNEL_IDS` are
  comma-separated nonzero snowflakes (`channel_ids` there): surrounding
  whitespace and empty entries are skipped; `0`, negative, non-numeric or
  overflowing entries fail closed with `InvalidId`. A conflicting legacy
  `DISCORD_GUILD_ID` alias fails closed (`crates/bot/src/onboarding.rs:193`).

Stored hot overrides merge per event from a consistent snapshot
(`crates/bot/src/onboarding.rs:215`): landing/goodbye/anchor/session
destinations refresh live; mode and guild identity never come from stored
rows. A stored `TWO_ONBOARDING_DRY_RUN` rendering merges only when it is
exactly `0`/`1`; a legacy row (`true`, `on`, …) keeps the deployment value
and logs `stored TWO_ONBOARDING_DRY_RUN must be exactly "0" or "1" when
set; keeping deployment value` (key only, never the value). Deleting a
stored row restores the deployment default. The Container receives these
keys through the Worker allowlists: `TWO_ONBOARDING_MODE` and
`TWO_ONBOARDING_DRY_RUN` are in `FORWARDED_FLAGS`, the landing, goodbye,
anchor-welcome and session channel ids in `FORWARDED_DISCORD_IDS`
(`wrangler/src/container-env.ts`). Adding a Worker var alone does not
configure the container (see [runbook.md](runbook.md#logs-and-keepalive)).

## Private welcome preview

Run `/welcome-preview` in the configured server with **Manage Server**. It is
always discoverable, even when welcome delivery is off, and takes no options.
The response is private and starts with **Preview — not a delivery**, the mode,
and fixture identity. It renders the current welcome body and picker through
the same renderer as a newcomer join, with the current stored channel overrides
and deployment mode. It does not edit configuration or send a test welcome.

The fixture is the fixed synthetic, roleless newcomer `765432109876543210`.
Its display name is never resolved or rendered; its mention is inert because the
response disables all mentions. No real member (including the invoker) is fetched.
There are no role grants, channel posts, prompt/funnel records, invite reads,
voice moves or audit-store writes. Preview menus are visual samples only; their
submissions must not run the live picker. Use the ordinary newcomer menu for
real selections.

A dry-run legacy/anchor preview explains the skip; session mode still renders,
matching its live dry-run behavior. A missing landing slot shows an explicit
marker instead of a blank response. Invalid or unavailable settings return a
private retry message without echoing stored values. Fix configuration through
the existing admin path, then try again. An unconfirmed initial acknowledgement
is never retried.

This is a **configuration preview**, not a permission probe: it does not fetch
channels, check current bot posting permissions or prove that a configured
channel still exists. Use [preflight](preflight.md) for those checks. No live
Discord or staging/production database test is needed to preview rendering.

## Prompt decisions

`decide_prompt` (`crates/core/src/onboarding.rs:157`) runs in legacy check
order: bots first, then the rules gate, then the guard that lasts one stay (a processed goodbye clears it, so a rejoin is welcomed again).

| Inputs | Decision |
|---|---|
| `is_bot` | `Skip(Bot)` — before anything else. |
| `pending` (still gated) | `Skip(StillPending)`. |
| `already_prompted` | `Skip(AlreadyPrompted)`. |
| none of the above | `Prompt`. |

`welcome_trigger` (`:183`) decides which gateway events may prompt: an
ungated join (`Joined { pending: false }`) may prompt; a gated join
(`Joined { pending: true }`) prompts nobody and waits for the
`pending: true -> false` member update (`GateCleared`), which may prompt.
The router calls `decide_prompt` next, so a stale `pending` still cannot
slip through.

`adjudicate_welcome` (`:1100`) then decides the post:

- Legacy or session mode with no postable landing channel: `Skip` with
  reason `no_landing_channel`.
- Dry run in legacy or anchor mode: `Skip` with reason `dry_run`. Session
  welcomes still post in dry run because they never grant roles. These
  reasons are `WelcomeEffect` values asserted in core tests, not log lines:
  the bot runtime returns `Ok(())` silently on these paths (see
  [Failure signals](#failure-signals-and-triage)).
- Legacy posts the legacy copy with the games picker; session posts the
  session copy with the session picker; anchor always posts the anchor copy
  to the anchor channel with **no** picker. Mentions name exactly the new
  member; roles and everyone are never allowed.

The executor acquires the store prompt guard (`begin_prompt`) before
sending and records only after Discord accepts; a failed send drops the
guard without a marker, so the next event retries. `has_onboarding_prompt`
is a read fast path, not a send claim. The guard serializes
join/gate-clear sends but is not exactly-once across a crash between
Discord acceptance and the Postgres commit (inherited from legacy).

## Game-picker selection

`plan_game_selection` (`crates/core/src/onboarding.rs:429`) turns "the
member ticked these boxes" into "grant these roles, link these channels".
It is pure: no Discord, no database. First occurrence wins for known and
unknown keys alike; unknown keys are reported in `unknown_keys` and never
routed.

`resolve_destination` (`:395`) resolves one pick against live visibility:

- Primary visible: route the primary, not degraded.
- Primary dark but fallback visible: route the fallback and count it in
  `degraded_count`. Non-zero degraded means members are being sent to the
  hub because the real room is still dark — it surfaces in the weekly
  numbers.
- Neither visible: keep the role but route nowhere. These picks appear in
  `unavailable_keys` and, on the funnel row, under the `unavailable` key
  (present only when at least one pick has no route). A hub-by-design pick
  (no primary) routed to the hub is **not** degraded.

`channel_ids` lists visible channels in first-submission order, deduped.
`game_picker_reply` renders the post-grant reply: no reachable room says
`Game roles saved.` plus one `No channel is available to you right now.`
line per pick; otherwise `Done. Here is where to go:` plus one
`{emoji} **{label}** → {link}` line per destination.

`adjudicate_game_select` (`:602`) adjudicates one submission:

- Session mode returns `None`: the router must not route the game select
  there at all.
- Dry run returns empty role deltas with `Dry run: no roles were changed.`
  and records nothing.
- A raw empty grant is the clear branch: held game roles are removed, the
  reply is `Cleared your game roles. Open the menu again whenever you
  like.`, and no funnel rows are written. A nonempty grant with no
  recognized keys is not a clear: every role is preserved, nothing is
  recorded, and the ephemeral reply invites a retry from a fresh menu.
- Otherwise roles are matched to the menu exactly (unticked held game roles
  are removed), the reply is ephemeral (only the clicker sees it),
  `record_selected` is true and `record_routed` is true only when
  destinations exist. The reply and plan are provisional until the
  executor re-resolves visibility after successful role writes — a
  just-added role can itself reveal the channel to link — so after role
  writes it reloads `MemberAccess`, recomputes visible channels, and calls
  `plan_game_selection` before replying or recording. On role failure the executor replies `I could not set those
  roles - my own permissions are wrong. Staff have been notified in the
  logs.` and records nothing.

Static select ids: `two:onboarding:games` and `two:onboarding:session`
(`GAME_SELECT_ID`, `SESSION_SELECT_ID`).

Session picks (`plan_session`, `:737`) are roleless by construction — no
role field exists. Valid picks route in catalog order; unknown keys are
skipped with a stale-key retry note; empty or wholly unroutable
submissions link nothing. Re-selecting the same option yields a
byte-identical acknowledgement (the idempotency the acceptance asks for).
A routed plan records `channel_routed` with source `session-picker`; there
is deliberately no `game_roles_selected` row and no dry-run branch (the
picker always answers).

Before routing selected session destinations, the runtime reads fresh member,
role and channel evidence. The voice lobby's **Join voice now** recommendation
requires effective **View Channel + Connect** on a voice or stage channel (types 2 and 13), using
@everyone, combined-role and member-overwrite precedence, including the existing
owner/administrator semantics. A visible lobby with Connect denied is unavailable:
it contributes no actionable voice link or successful voice-route fact. If the
text pick is also selected and accessible, it still routes, with `join-voice`
listed under `unavailable`. Only selected destinations are read, once per channel;
deduplication, catalog order and stale-key handling are unchanged. This is a
recommendation filter, not a permission grant, capacity forecast or guarantee of
a future join. Text/forum game-hub visibility remains View-only; welcome posting
still uses its existing View/Send check and does not require Connect.

Sunday Squad (`SUNDAY_SQUAD`, `:875`): Fall Guys, one hour, every Sunday
20:00 America/New_York (wall-clock, so the US-DST weeks are 169/167 hours,
not 168). Far welcomes name the next occurrence with a relative
`<t:...:R>` timestamp; near (under two hours) and live welcomes say it is
happening right now. Goodbyes are session-only
(`adjudicate_goodbye`, `:1206`): other modes return `None`; dry run and
missing-channel skips post nothing; posts render the leaver's name as plain
text under `MentionPolicy::None` (empty parse even for mention-like
usernames) — the person who left is never pinged.

## Failure signals and triage

Check `/health` and `/readyz` first, then Worker **and** container logs
([runbook.md](runbook.md#is-it-alive)). Never dump env values, tokens, or
member data into evidence.

| Signal | Meaning | First action |
|---|---|---|
| `/readyz` `gateway_failure` class `onboarding_gates_invalid` (`crates/bot/src/gateway_failure.rs:85`) | Deployment `TWO_ONBOARDING_MODE` / `TWO_ONBOARDING_DRY_RUN` value rejected at boot. | Fix the deployment value to the exact surface above; redeploy through the approved path. Do not retry with another credential. |
| `/readyz` `gateway_failure` class `onboarding_init_failed` (step `onboarding_init`, `crates/bot/src/main.rs:526`) | Onboarding init failed: missing/invalid per-mode channel, failed `/users/@me` identity probe, or conflicting `DISCORD_GUILD_ID` alias. The `from_map` detail is erased at `crates/bot/src/onboarding.rs:202` (every error maps to `RuntimeError::Config`), so no `Missing`/`InvalidId` text ever reaches the operator. | Supply the mode's required channels as nonzero snowflakes; confirm the guild alias matches the gateway `GUILD_ID`; check the identity probe. |
| `two-bot preflight` exit 2 `invalid TWO_ONBOARDING_MODE or TWO_ONBOARDING_DRY_RUN` (`crates/bot/src/preflight.rs:209`) | Same rejection, caught before any Discord REST. | Same fix; no Discord checks ran. |
| `two-bot preflight` FAIL `level reward coverage` | Onboarding permits level-role writes (legacy/anchor, not dry run) but `--level-role-ids` was not supplied. | Supply `--level-role-ids` from the guild's `level_role_rewards` export, including an explicit empty CSV when none. Not required in session mode or dry run. See [preflight.md](preflight.md#database-backed-level-rewards). |
| `two-bot preflight` channel/role FAILs | Denied, missing, wrong-guild, wrong-type channel; missing/managed/high role. Administrator does not bypass hierarchy. | Fix overwrites, hierarchy (bot's highest role must sit above every game/level role) or config; see [preflight.md](preflight.md#checks). |
| `stored TWO_ONBOARDING_DRY_RUN must be exactly "0" or "1" when set; keeping deployment value` (error) | Legacy stored dry-run row refused; deployment value kept, job keeps running. | Clean the stored row to `0`/`1` or delete it (restores deployment default). Never log or paste the stored value. |
| `onboarding picker configuration unavailable after defer` (warn) | Settings/config failed after the 2.5 s defer; the member got an honest `couldn't load the menu configuration` edit. Terminal: no role or success writes. | Check settings-store health and deployment config; ask the member to re-open the menu. Do not replay roles. |
| `onboarding picker failed; attempting deferred error reply` (warn, with handler) | Selection processing failed after defer. Role failures reply with the permissions message; other failures ask for a fresh menu submission. | For role failures check Manage Roles plus role positions; otherwise check member/channel reads and Discord status. A delivered error reply is terminal; only a failed error edit stays retryable. |
| `gateway onboarding job invalid` (warn) | Worker-built job failed to serialize. | Record the revision; do not restart to clear it. Escalate with the log line if it repeats. |
| Silent skip: bot / still-pending / already-prompted; gated join with no later gate-clear | `decide_prompt` / `welcome_trigger` working as designed. | No action. Gated members prompt when `pending` flips true → false. |
| Silent skip: welcome/goodbye simply absent, nothing logged | No postable destination (none configured or bot lacks View/Send/Embed), or dry run (legacy/anchor welcome, any goodbye). `welcome()`/`goodbye` return `Ok(())` with no log line (`crates/bot/src/onboarding.rs:490,506,425`); the `no_landing_channel` / `no_goodbye_channel` / `dry_run` reason strings live only in core `adjudicate_*` effects, never in logs — do not grep for them. | Check the mode's channel config and the bot's effective channel access via preflight. Dry run needs no action; session welcomes still post. |
| `command runtime parked; onboarding runtime disabled` (warn, `crates/bot/src/main.rs:535`) | The command runtime failed to build, so onboarding is off. Causes include invalid feature gates, invalid moderation gates, invalid send admission, REST executor build failure, or ticket runtime init failure (`crates/bot/src/command_runtime.rs:425,437,471,478,496`). | Read the `... command runtime disabled` warn logged just before — it carries the reason — and fix that cause; onboarding resumes with the runtime. |
| Picker reply `No channel is available to you right now.` with saved roles | All picks unavailable (hub also dark). | Check channel visibility/overwrites; roles are correctly kept. |
| Rising `degraded` in `channel_routed` rows | Primaries dark, members routed to the hub fallback. | Open or re-expose the real rooms; the count is the signal. |

Delivery notes: at most 32 unfinished durable rows per shard owner with at
most two onboarding workers; worker errors stop the essential runner and
restart reclaims interrupted workers (at most three attempts per job).
Exhaustion fails closed until an authorized correction resets that job —
never substitute credentials or restart-loop to hide a
permissions/configuration failure. Interaction callbacks are never replayed
after a process restart (`interrupted` receipt); the member re-submits from
a fresh menu. `completed` means worker handling ended, not that a message
was sent.

## Operator commands

Every flag below was run for this guide; nothing here is invented. Full
coverage and exit-code tables live in [preflight.md](preflight.md).

```sh
two-bot --help
two-bot preflight --help
two-bot preflight [--json] [--level-role-ids CSV]
```

`preflight` issues GET requests only through the governed transport, never
sends messages or grants roles, and live requests require the runtime
`TWO_DATABASE_URL` admission authority. Exit 0 is PASS/WARN only, 1 is
FAIL, 2 is bad invocation or config. `--level-role-ids` takes the guild's
exported `level_role_rewards` role ids; pass an explicit empty CSV to
assert none. `DISCORD_PREFLIGHT_API_BASE` is a loopback-only test seam —
never set it in a real deployment.

`two-bot` has no `serve` or `restart` subcommand: bare `two-bot` runs the
gateway/HTTP service ([runbook.md](runbook.md#start-here)). Backup
`--help` prints the shared operator usage (no per-subcommand help text).

## Verification

```sh
cargo fmt --all -- --check
python3 scripts/cargo_cache.py run -- test -p two-bot-core --locked --lib onboarding
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --locked --lib onboarding_config
python3 scripts/cargo_cache.py run -- test -p two-bot --locked --test preflight -- legacy_requires_an_explicit_reward_export_but_session_does_not help_and_invalid_arguments_need_no_credentials_or_network
python3 scripts/cargo_cache.py run -- test -p two-bot-core --locked --test reference_docs
two-bot --help
two-bot preflight --help
```

Results on this head: fmt clean; core onboarding 43 passed; discord
`onboarding_config` 3 passed; preflight onboarding-relevant 2 passed;
`reference_docs` 7 passed (so [configuration.md](configuration.md) is
current); both `--help` outputs print the preflight usage quoted above
(`two-bot preflight [--json] [--level-role-ids CSV]`, exit 0, no
credentials needed).

## Private goodbye preview

Run `/goodbye-preview` in the configured server with **Manage Server**. It is
always discoverable, even when goodbye posts are off, and takes no options. The
response is private and starts with **Preview — not a delivery**, the mode and
the fixture identity. It then names the first configured goodbye channel and
shows the exact text the live departure path would post, built by the same
adjudication and name sanitizer.

The member is synthetic: a fixed identity with a deliberately hostile display
name and a seven-day stay, so every preview also shows that markdown and
mentions in a real name are neutralised. No real member (including the
invoker) is fetched. There is no goodbye post, no prompt-guard release, no
funnel or audit write, and the response disables all mentions.

Goodbye posts only run in session mode with dry run off and a configured
channel. When any of those is missing, or settings cannot be loaded, the
preview says so and names the next action instead of showing a render: switch
to session mode, turn off dry run, set a goodbye channel, or try again. An
unconfirmed initial acknowledgement is never retried.

This is a **configuration preview**, not a permission probe: the live path also
skips channels the bot cannot post in, and the preview does not check channel
existence or posting permissions. Use [preflight](preflight.md) for those
checks. When a stored `DISCORD_GOODBYE_CHANNEL_IDS` override exists, set the
preflight environment value to the same comma-separated, nonzero channel IDs,
without empty entries. Preflight checks supplied environment values, not stored
overrides. The configured channel shown is the first one; the live path picks the
first it can post in.
