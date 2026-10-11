# Staging voice synthetic

An automated test bot verifies the temp-voice room lifecycle on the deployed
staging bot, so nobody has to join a voice channel to check a voice change.

## What it checks

`scripts/staging_voice_synthetic.py` connects a dedicated test bot to the
Discord gateway, joins the staging **Create a Lobby** creator (the only
creator) and asserts:

| Check | Pass condition |
|---|---|
| create | a new room appears in the creator's category and the test bot is moved in |
| name | the room is created already carrying a template name, not the `<display>'s room` fallback |
| position | the room is directly below its creator among the category's voice channels |
| owner | the room has a member overwrite for the joiner allowing View, Connect and Manage Channels |
| status | a voice-channel status line is set on the room |
| delete | after the test bot leaves, the room is deleted within the empty grace |
| metrics | optional: `two_bot_voice_names_total{outcome="created_with_template"}` grows (SKIP without a metrics token) |

The script refuses (exit 2, no request sent) unless the guild is TWO Staging
and the creator is the staging Create a Lobby channel.

## When it runs

- After every `deploy-staging` run (job `voice-synthetic`). A failure fails the
  staging deploy, and `deploy-production` only promotes a commit whose latest
  `deploy-staging` run succeeded, so a broken voice lifecycle cannot ship.
- Every 6 hours (`voice-synthetic.yml` schedule) to catch drift. A failed run on
  main is re-run once by the host flake ledger and opens a `[main-red]` card if
  it fails again.
- On pull requests only the offline suite (`scripts/test_staging_voice_synthetic.py`)
  runs, and only when voice inputs change (`scripts/job-inputs.py` `voice`). A
  PR's code is not deployed to staging before merge, so a live run would test
  main, not the PR.

Each live run retries once after 30 s; two failures fail it. Evidence JSON is
uploaded as the `voice-synthetic-<run id>` artifact for 14 days.

## Identity

The test bot is a separate Discord application, **TWO Voice Synthetic
(staging)**, invited only to the TWO Staging guild with View Channels, Connect
and View Audit Log. It is never the bot under test.

Discord marks it as a bot, and bots never count as room occupants. The staging
Worker therefore sets `TWO_TEMP_VOICE_SYNTHETIC_HUMAN_IDS` to its user ID, so
the voice runtime treats it as a human: it can own a room and empty it by
leaving. The setting is environment-only (never dashboard-stored), takes at most
four IDs, refuses malformed values, and is ignored for the live guild by code.
Production never sets it.

Bindings:

| Where | Name | Value |
|---|---|---|
| GitHub environment `voice-synthetic` (main only, no reviewer) | secret `TWO_VOICE_SYNTHETIC_BOT_TOKEN` | the test bot token |
| GitHub repository variable | `VOICE_SYNTHETIC_REQUIRED` | `true` once the first live run is green |
| GitHub repository variable (optional) | `STAGING_EMPTY_GRACE_SECONDS` | staging empty grace, default 60 |
| Staging Worker setting | `TWO_TEMP_VOICE_SYNTHETIC_HUMAN_IDS` | the test bot's user ID |

Until the token exists, runs report "not configured" and pass. With
`VOICE_SYNTHETIC_REQUIRED=true`, a missing token fails the run.

## Run it by hand

Actions, then **voice-synthetic**, then **Run workflow** on `main`. Locally (staging
only):

```sh
TWO_VOICE_SYNTHETIC_BOT_TOKEN=... python3 scripts/staging_voice_synthetic.py --evidence out.json
```
