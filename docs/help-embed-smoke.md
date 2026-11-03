# Staging help-embed smoke

After every staging deploy, a dedicated test bot posts the rendered `/help`
embeds into a staging test channel, reads both messages back through the
Discord REST API, and asserts the brand members actually see — so nobody has
to screenshot `/help` to prove it.

## What it checks

`scripts/staging_help_embed_smoke.py` posts two payloads built by
`crates/discord/src/bin/help_embed_smoke.rs` from the real
`help_response` / `help_detail_response` builders (via `brand_embed()`),
then asserts on the read-back messages:

| Check | Pass condition |
|---|---|
| posted | both messages exist and carry ids |
| color | every embed reads back `color == 0xFF0E52` |
| footer_text | every embed footer reads `Together We Own` |
| footer_icon_url | every footer icon URL is exactly the Icon 1 asset |
| footer_icon_fetch | the Icon 1 asset URL answers HTTP 200 as `image/png` |
| provenance_recorded | the evidence records which case took the builder embed branch |
| cleanup | both test messages are deleted afterwards |

A PASS means Discord served and returned the brand intact; a FAIL names the
assertion that did not hold. The message ids and every assertion land in the
evidence JSON and in `$GITHUB_STEP_SUMMARY`.

## Fences

The script refuses (exit 2, nothing sent) unless the channel resolves to the
TWO Staging guild — the live guild or anything else refuses before any post.
It never invokes slash commands: Discord only creates interactions for real
users, so a bot token cannot. The footer icon fetch carries no bot token
(the asset URL is public). Every response read is capped at 64 KiB and
refuses duplicate JSON keys; refusals use fixed vocabulary and never echo
tokens, bodies or URLs.

## Rollout

The `help-embed-smoke` job in `deploy-staging.yml` runs after every deploy
with up to three attempts (each attempt posts and deletes its own messages,
so retries never collide). While the test bot is not provisioned the job
SKIPs with a notice; flip it to required once provisioning lands:

1. Create a dedicated staging test bot (never the staging product bot, never
   prod) and invite it to TWO Staging with View Channel, Send Messages and
   Read Message History in one dedicated test channel (Manage Messages only
   if the channel forbids the bot from deleting its own messages).
2. Save its token as the `TWO_HELP_EMBED_SMOKE_BOT_TOKEN` secret in the
   `help-embed-smoke` environment.
3. Set the `HELP_EMBED_SMOKE_CHANNEL_ID` variable to the test channel id.
4. Set the `HELP_EMBED_SMOKE_REQUIRED` variable to `true`.

Until step 4 the smoke SKIPs green with a notice, so landing the code never
reds staging. A red run blocks promotion the same way any red staging job
does: the production guard only promotes green runs.

## Reading the evidence

Each run uploads `help-embed-smoke-<run-id>-<attempt>.json` with the channel
id, both message ids, the renderer provenance (`help_builder_embedded`,
`help_detail_builder_embedded`, `help_topic`) and the per-check verdicts.
A content-only registry posts the same text wrapped with the exact
`brand_embed` function the embed branch uses, and the provenance flags say
so — a content-only run never reads as an embed-branch proof.
