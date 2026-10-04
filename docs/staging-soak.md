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

## Waiver ledger (B4 parity sign-off)

Cutover preconditions ([cutover.md](cutover.md)) require every non-DROP
parity behavior to have merged wiring plus acceptance evidence, **or an
explicitly approved waiver**. Writing a waiver down is not approval: a
`waived` checklist entry is a *proposed* waiver of staging execution only.
Parity coverage stays, and the row remains unresolved (`NEEDS WORK`) until
B4 records acceptance — reason, accepting actor and the owning slice's
isolated fixture evidence — on [TOG-9699](/TOG/issues/TOG-9699).

The ledger is machine-checked, not prose:

- Authoritative: `docs/soak-checklist.json` — one entry per non-DROP parity
  row in `docs/parity.md` §§1–8 (pure `DROP` rows are not ported, so they
  carry no soak obligation; mixed mapped/`DROP` rows stay covered), per §12
  addition under the same rule, and per non-`dropped` §13 ledger row
  (history-rewrite replays are `dropped`). §12/§13 entries name their
  `owner` cards, which must match the cards in the parity disposition.
- Rendered: `docs/soak-checklist.md` (regenerate with
  `python3 scripts/check_soak_checklist.py --render` after editing JSON).
- Gate (offline, runs in CI before Cargo):
  `python3 scripts/check_soak_checklist.py` plus
  `python3 -m unittest discover -s scripts -p 'test_soak_checklist.py' -v`.
  Any unmapped row, any missing or stale owner card, any `waived` entry
  without **both** `reason` and `approver`, or any Markdown drift fails the
  gate.

Every `waived` entry carries `reason` and `approver`. The first waivers,
filed under [TOG-12140](/TOG/issues/TOG-12140), cover the 25 rows staging
cannot exercise — data-plane/operator paths (backup timers, DB stores,
internal actions, Postgres guards) and the DROP-adjacent rows
(`community_facts` with its dropped rota extensions, operator-script
runtime drops) — all with `approver: pending — CEO/DoE acceptance on
[TOG-9699](/TOG/issues/TOG-9699) (proposed, not approved)`.

Accepting a waiver means replacing `pending` with the accepting actor, the
decision reference and the attached fixture receipt. Moderation,
automation, scheduled unbans and internal actions cannot be silently
waived: their entries still require that explicit acceptance plus the
owning slice's fixture evidence — until then they stay `NEEDS WORK`.
