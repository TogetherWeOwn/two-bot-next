# Staging-guild soak (B2 acceptance)

7-day soak of the Rust bot on Cloudflare Containers against **TWO Staging**
with the **staging bot identity**. Prod bot stays on Coolify throughout; this
soak never touches the prod guild or prod tokens.

## Preconditions (not this card)

- S2 ([TOG-9807](/TOG/issues/TOG-9807)) connects the shard; S3
  ([TOG-9808](/TOG/issues/TOG-9808)) wires the event pipeline. The soak needs
  a live gateway session — a parked supervisor soaks nothing.
- B1 ([TOG-9694](/TOG/issues/TOG-9694)) confirms the `lite` placement
  (RSS < ~200 MiB) that `wrangler.toml` already declares.

## Provisioning (operator, once)

Secrets are GitHub Actions secrets on the repo and Worker secrets per
environment. Names only — values never go in cards, logs, or PRs:

- `CLOUDFLARE_API_TOKEN` / `CLOUDFLARE_ACCOUNT_ID` — GitHub Actions secrets,
  used by `deploy-staging.yml`.
- `DISCORD_TOKEN` — Worker secret (`wrangler secret put --env staging`),
  the staging bot token.
- `GUILD_ID` — Worker secret (`wrangler secret put --env staging`),
  the TWO Staging guild (`1545644954272137297`).
- `DATABASE_URL` — Worker secret (`wrangler secret put --env staging`),
  the direct, unpooled URL for the bot's dedicated `two_bot` database and role
  on the Neon **staging** branch (`br-summer-sunset`, project
  `rough-dream-43123587`), separate from the web's `two` / `two_app`.
  Required for gateway checkpoint persistence; never use a production URL.
- `STAGING_WORKER_URL` — GitHub Actions **variable** (not a secret), the
  staging Worker URL, used by the `/readyz` gate. If it is missing the gate
  fails closed with instructions instead of deploying blind.

## Starting the soak

1. Merge the B2 PR (CI green: `check`, `pr-lint`, `gitleaks`).
2. `deploy-staging.yml` runs on `main`, deploys `--env staging`, then polls
   `/readyz` until 200 (gateway identified). Record the first-200 time: that
   is the redeploy-gap measurement.
3. Record the soak start time (first `/readyz` 200) as a comment on
   [TOG-9695](/TOG/issues/TOG-9695).

## During the soak (7 days)

- `/readyz` returns 200 only when every component reports ready, else 503
  with the per-component breakdown (`process`, `gateway`). Poll it daily; any
  503 names the failed component.
- Once a day, compare the bot's observed events against the staging guild
  audit log / visible join-voice-message activity. Any gap (audit shows an
  event the bot did not process) fails the soak: file a card, fix, redeploy,
  restart the 7-day clock.
- Container restarts (deploy, rollout, host move) show up as a fresh IDENTIFY
  until S5 adds session persistence; the acceptance budget covers it.

## Acceptance

- 7 consecutive days with **zero missed gateway events** (join / voice /
  message audit), recorded on [TOG-9695](/TOG/issues/TOG-9695).
- Redeploy RESUME/IDENTIFY gap measured (deploy finish to first `/readyz`
  200) and within budget: **under 60 s** (one guild, fast IDENTIFY).
- `lite` placement confirmed by B1's RSS measurement.
