# Production Worker deploy

`.github/workflows/deploy-production.yml` is the only path that deploys the
`production` Wrangler environment. It runs only when someone dispatches it, never
on push, PR or schedule. The B4 cutover executor (TOG-9699) dispatches it from
`main`. Agents never do. The cutover gates in [cutover.md](cutover.md) still
apply.

**Before the first dispatch**, a repository admin must set up three things.

1. Create the `production` GitHub Environment with at least one required
   reviewer, and restrict deployment branches to the single custom branch
   `main`. Turn on prevent self-review.
2. Set the `PRODUCTION_WORKER_URL` variable to the production Worker's
   `https://` URL. It must differ from `STAGING_WORKER_URL`.
3. Optionally add production-scoped `CLOUDFLARE_API_TOKEN` /
   `CLOUDFLARE_ACCOUNT_ID` secrets. Environment secrets override the repository
   secrets that staging uses.

Until the Environment exists with reviewers and a main-only branch policy, the
`sha guard` job refuses every dispatch. A job that names a missing Environment
makes GitHub create it with no protection, so the guard checks before the
deploy job can run. This repository is private. GitHub offers required
reviewers on private repositories only on the Enterprise plan; on Free, Pro
and Team plans, the guard keeps refusing.

**Deploy.** Dispatch with `sha` set to a full 40-character commit that is on
`main`. That commit needs green `check` and `worker check` runs (from GitHub
Actions) and a successful `deploy-staging` run. Staging runs queue in a single
concurrency group, so an intermediate commit may never stage. Pick one that did.
After the reviewer approves, the job:

1. checks out exactly that commit and re-verifies that it is on `origin/main`;
2. runs `wrangler deploy --message <sha>`;
3. writes the SHA and the old and new Worker version IDs to the run summary;
4. gates on `/health` 200 and a truthful `/readyz` (200 ready, 503 parked),
   with the same contract as staging.

**Roll back.** Dispatch again with `rollback` set to the previous version ID
from the failed run's summary. Set `sha` to that version's commit, or any other
green `main` commit, which is recorded as the rollback message. The rollback
passes the same guard and the same Environment approval. It then runs
`wrangler rollback <version-id> --yes` and fails unless that version serves
100% of traffic. The `/readyz` gate runs again after the rollback.
