# Staging help embed smoke

This is the human-free live proof that Discord serves both branded `/help`
embeds with the Icon 1 footer and brand colour intact. It runs after every
staging deploy and can also be dispatched by hand.

Discord creates slash-command interactions only for real users, so this smoke
does not invoke `/help` and does not claim to prove gateway interaction
delivery. It proves the rendered-message half that offline builder tests
cannot: the exact payloads produced by the Rust builders survive a Discord
post and read-back unchanged.

## Flow

1. CI builds `help_embed_smoke_payloads`, a Rust example in the discord crate.
   It calls `help_response` and `help_detail_response`, passes both replies
   through the executor sanitizer, and emits their single embeds plus the
   expected brand values from `brand.rs`.
2. `scripts/staging_help_embed_smoke.py` fences the run to the staging guild
   and to a dedicated test bot (TWO Voice Synthetic (staging)): the token
   must belong to a bot account that is none of the product applications and
   a member of exactly the staging guild. It verifies that the configured
   channel is a staging guild text channel, and posts each embed as an
   ordinary test-bot message. The staging product bot's token is never
   used; it stays out of GitHub by the recorded decision in
   [the slash smoke doc](staging-slash-smoke.md#why-it-is-not-a-github-workflow).
3. For each post, it reads `GET /channels/{channel_id}/messages/{message_id}`
   and asserts on that read-back—never the POST response—that:
   - `embeds[0].color == 16715346` (`0xFF0E52`);
   - `embeds[0].footer.text == "Together We Own"`;
   - `embeds[0].footer.icon_url == BRAND_ICON_URL`.
4. It deletes both messages, pass or fail, and writes evidence containing
   only row results, durations and message IDs. Before each post it records
   the channel's newest message ID as an anchor; if a post's outcome is
   lost (timeout, 5xx or an unreadable answer), it reconciles by deleting
   every newer test-bot message, so a committed-but-unseen post cannot be
   left behind by a later green run.

The payload deliberately uses the maximal command registry and a wide topic
detail, matching the Rust builder tests. Short `/help` and short
`/help <topic>` replies are intentionally plain ephemeral content and cannot
carry an embed footer; this smoke exercises the embed path of both builders,
rather than simulating staging's current registry size.

## Icon delivery and rollback contract

`crates/discord/src/brand.rs` owns `BRAND_ICON_URL`:
`https://imagedelivery.net/cNVnKlHCCMxI9vqDkwBbUQ/two-icon1-dark512-4469ca165f03/public`.
This is the approved Icon 1 dark 512×512 PNG, already imported as image
`two-icon1-dark512-4469ca165f03`, served with the existing `public` variant.
No upload, new variant or credential is needed by the bot.

The real consumers are the footer icons on the embed branches of
`help_response` and `help_detail_response` in `crates/discord/src/interactions.rs`,
via `brand_embed` → `brand_footer`. Neither builder adds an author icon.
Colour, footer text, body and layout are unchanged. Plain-content help replies
still have no footer. Changing this constant does not synchronize the bot avatar.

Keep `assets/brand/two-icon-1-dark-512.png` unchanged (29,035 bytes; SHA-256
`ff0cd5863ed02b1f405bd3f1bf22b192ea7b9e6dc657792cb6df621f8fce438a`).
The rollback URL is
`https://raw.githubusercontent.com/TogetherWeOwn/two-bot-next/main/assets/brand/two-icon-1-dark-512.png`.
To roll delivery back, restore that URL in the constant and its exact-URL Rust
assertion and Python fixture, through the normal review and release gates.
Do not delete or re-import the Images copy as part of a code rollback.

Use the existing payload builder and smoke above for evidence on the changed
SHA: `expected.icon_url` and both read-back footers must contain the Images URL.
An offline fixture pass or a historical run using the GitHub URL does not prove
runtime Images adoption. Record the exact deployed SHA and smoke run; if that
proof is pending at merge, hand the gap to rollout coordination explicitly.
The read-back proves the URL field, not a visual render or image fetch by Discord.

## CI gate and provisioning

`.github/workflows/help-embed-synthetic.yml` runs only from `main`, by manual
dispatch or as the `help-embed-synthetic` job called by `deploy-staging` after
the deploy. Failed assertions fail that staging run and prevent
`certify-promotion` from certifying it.

Before consumption, a separate main-only `staging-window` job in the existing
`staging` environment checks the [finite-use grant and exact readiness](staging-rollout-gate.md#full-window-workflow-and-recovery-semantics).
The smoke job needs that job and keeps only its test-bot credential. A called run
uses the deploy's exact SHA/deployment/expiry under a disjoint nested group and
never acquires or releases the parent's grant. A standalone run owns the canonical
workflow lock, acquires at most 45 minutes, and releases after smoke success or
failure. Start/assert and cleanup receipts are retained for 14 days. Re-run all
jobs, not only failed jobs; the latter changes the attempt and fails binding.
This source wiring is not activation: the linked inventory, rollback and
live-acceptance holds still apply.

No new credential is created or copied for this smoke. Only an operator or
staging guild admin may place the bindings; agents must never receive, copy
or print any token value:

- GitHub environment: the existing `voice-synthetic`; no new environment
- Environment secret: the existing `TWO_VOICE_SYNTHETIC_BOT_TOKEN` (the
  dedicated test bot's token, already placed for the voice synthetic); no
  new secret
- A dedicated staging text channel named `help-embed-smoke` where the test
  bot has a channel overwrite granting View Channel, Send Messages, Embed
  Links and Read Message History; deleting its own messages needs no Manage
  Messages permission. Nobody creates this channel by hand: the staging
  product bot reconciles it at boot (see below), so the channel always
  exists before the smoke runs
- Repository variable: `DISCORD_STAGING_HELP_SMOKE_CHANNEL_ID`, naming that
  channel
- Repository variable: `HELP_EMBED_SYNTHETIC_REQUIRED=true`, set only after
  the channel variable above

## Boot reconcile (staging only)

At boot, after the command-registry sync, the staging bot finds or creates
the `help-embed-smoke` text channel in the TWO Staging guild and ensures a
member overwrite for the staging test bot with exactly View Channel, Send
Messages, Embed Links and Read Message History. The reconcile runs only
when `TWO_DEPLOY_ENV` is exactly `staging` and the configured guild is the
staging guild; every other combination returns quietly, so production can
never create this channel. It uses the token the process already holds,
never touches the database, and never blocks or fails boot: every error is
logged and startup continues.

The test-bot user ids come from the existing staging setting
`TWO_TEMP_VOICE_SYNTHETIC_HUMAN_IDS`; only listed ids whose guild member
is a bot get the overwrite. If the setting is unset, empty, or holds no
staging-guild bot member, the reconcile logs an error naming the setting
and skips the overwrite while still reporting the channel.

On every successful staging reconciliation the reconcile emits one
structured info line with the channel id and the outcome (`created`,
`already_present` or `overwrite_ensured`). A failed or timed-out pass logs
an error instead. A channel id is a public snowflake and safe to log. If
database initialization fails, boot drains the in-flight pass within a
bounded budget before exiting; the next healthy boot retries it.
The operator reads that id from the staging boot log and places it in
`DISCORD_STAGING_HELP_SMOKE_CHANNEL_ID`, then sets
`HELP_EMBED_SYNTHETIC_REQUIRED=true`.

Until provisioning is complete and the required variable is `true`, missing
bindings produce a notice and a green run. The workflow decides this in a
binding-gate step before any toolchain or payload step runs, so an
unconfigured optional smoke cannot fail on a build problem and block
promotion; the gate sees only whether the token secret is set, never its
value. Once it is `true`, a missing binding fails closed. The token is passed to the called workflow by secret
name, held only in the smoke step's environment and then only in the Python
process environment. It is never a command-line argument, never written to
evidence and never included in error output.

To run it by hand after provisioning, dispatch **help-embed-synthetic** from
`main`. For a local offline check, run the Python fixtures; they use a fake
Discord transport and make no network calls:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_staging_help_embed_smoke.py' -v
```

On the persistent controller, verify the Rust payload contract through the
bounded Cargo wrapper:

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --lib help_embed_smoke
python3 scripts/cargo_cache.py run -- run -p two-bot-discord --example help_embed_smoke_payloads
```

## Reading a result

- Exit `0` / `VERDICT PASS`: both read-backs matched and both test messages
  were deleted.
- Exit `1` / `VERDICT FAIL`: identity, channel, post, read-back or brand
  failed. The row names the first fixed-vocabulary fault; the other embed
  still runs.
- Exit `2` / `REFUSED`: a fence stopped the run before any post, for example
  the wrong guild, a missing or malformed token, a missing channel, an
  invalid snowflake or an invalid or already-unbranded payload file.
- Exit `3` / `VERDICT FAIL`: a cleanup delete failed, or a post whose
  outcome was lost could not be reconciled, and a posted message may
  remain in the dedicated test channel. The workflow fails at once on
  this status instead of retrying, so a later clean attempt cannot read
  green over the leftover. A lost post that reconciliation removes (or
  proves absent) stays a retryable exit `1`.

A brand mismatch in the generated payload fails before posting. A mismatch
only on the read-back means Discord did not serve the posted brand fields as
sent. A cleanup failure leaves the identified message in the dedicated test
channel and fails the row so the leftover is visible rather than silent.
