#!/usr/bin/env bash
# Full-history gitleaks scan with a base-branch policy on pull requests and
# merge-queue commits.
#
# On `pull_request` the checkout is the PR merge ref, and on `merge_group` it
# is the unmerged queue commit, so the root `.gitleaks.toml`, the root
# `.gitleaksignore` and any inline allow comment are whatever the PR author
# wrote. Left alone, a PR could silence the required `gitleaks` check
# for its own leak by adding an allowlist. This wrapper makes a PR or queue
# run honour only the policy that has already merged:
#   * `--config` points at the base branch's `.gitleaks.toml` (or the stock
#     rules when the base branch has none);
#   * `--gitleaks-ignore-path` points at the base branch's `.gitleaksignore`
#     (or an empty file). gitleaks also reads `<repo>/.gitleaksignore` on top
#     of that flag, so the working-tree copy is replaced with the base copy
#     too. `gitleaks git` scans history, never the working tree, so this does
#     not hide anything from the scan;
#   * `--ignore-gitleaks-allow` drops inline `gitleaks:allow` suppressions.
# A PR that legitimately needs a new allowlist merges that change first, as
# its own small reviewed PR. Push-to-main and dispatch runs scan with the
# repo's own policy, which is already merged.
#
# Not covered: the workflow and this script come from the PR as well. Closing
# that needs a platform control (protected workflow paths / required review).
#
# Inputs (environment):
#   GITLEAKS_BIN  path to the verified gitleaks binary (required)
#   EVENT_NAME    github.event_name; the base-branch policy applies to
#                 `pull_request` and `merge_group`
#   BASE_REF      github.base_ref, or on `merge_group`
#                 github.event.merge_group.base_ref (a full ref such as
#                 `refs/heads/main`; the prefix is stripped here). Required
#                 when EVENT_NAME is `pull_request` or `merge_group`.
#   RUNNER_TEMP   scratch directory (falls back to TMPDIR, then /tmp)
# Run from the repository root. In a PR or queue run it overwrites the
# working-tree `.gitleaksignore`; run it only in a disposable checkout.
set -euo pipefail

: "${GITLEAKS_BIN:?GITLEAKS_BIN must point at the verified gitleaks binary}"

# --log-opts=HEAD: scan only history reachable from HEAD, not every fetched branch.
args=(git . --redact --verbose --exit-code 1 --log-opts=HEAD --ignore-gitleaks-allow)

if [[ "${EVENT_NAME:-}" == "pull_request" || "${EVENT_NAME:-}" == "merge_group" ]]; then
  : "${BASE_REF:?BASE_REF is required for pull_request and merge_group runs}"
  # merge_group.base_ref arrives as a full ref (refs/heads/main); strip it.
  base_ref="${BASE_REF#refs/heads/}"
  base="refs/remotes/origin/${base_ref}"
  if ! git rev-parse --verify --quiet "${base}^{commit}" >/dev/null; then
    echo "::error::base branch ${base_ref} is not fetched; the ${EVENT_NAME} scan cannot load its policy" >&2
    exit 2
  fi

  scratch="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/gitleaks-base-policy.XXXXXX")"
  config="${scratch}/gitleaks.toml"
  ignore="${scratch}/.gitleaksignore"

  if git cat-file -e "${base}:.gitleaks.toml" 2>/dev/null; then
    git show "${base}:.gitleaks.toml" > "${config}"
  else
    printf '[extend]\nuseDefault = true\n' > "${config}"
  fi
  if git cat-file -e "${base}:.gitleaksignore" 2>/dev/null; then
    git show "${base}:.gitleaksignore" > "${ignore}"
  else
    : > "${ignore}"
  fi

  if [[ -s "${ignore}" ]]; then
    rm -f .gitleaksignore
    cp "${ignore}" .gitleaksignore
  else
    rm -f .gitleaksignore
  fi

  echo "${EVENT_NAME} scan policy: ${base} .gitleaks.toml ($(sha256sum "${config}" | cut -c1-12))," \
    ".gitleaksignore ($(grep -c . "${ignore}" || true) non-empty lines)"
  args+=(--config "${config}" --gitleaks-ignore-path "${ignore}")
fi

"${GITLEAKS_BIN}" "${args[@]}"
