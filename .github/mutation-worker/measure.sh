#!/usr/bin/env bash
set -euo pipefail
test "${GITHUB_ACTIONS:-}" = true

if ! npm audit --prefix wrangler --audit-level=high --include=dev; then
  echo "::warning::Worker mutation measurement blocked by the dependency audit"
  exit 0
fi
npm ci --prefix wrangler
sandbox=$(mktemp -d "$RUNNER_TEMP/mutation-worker.XXXXXX")
git archive HEAD | tar -x -C "$sandbox"
ln -s "$GITHUB_WORKSPACE/wrangler/node_modules" "$sandbox/wrangler/node_modules"
node .github/mutation-worker/run.mjs "$sandbox" "$RUNNER_TEMP/worker-mutation-plan.json" \
  "$RUNNER_TEMP/worker-mutation.json" warn
