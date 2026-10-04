#!/usr/bin/env bash
# Offline self-test for gitleaks-scan.sh: a pull request must not be able to
# silence the secret scan that gates it.
#
# Builds throwaway git repos (a "base" commit on main plus a PR commit on top,
# with refs/remotes/origin/main standing in for the fetched base branch) and
# runs the real wrapper with the real gitleaks binary. The planted token is
# synthetic: it is derived at run time from a hash, so neither this file nor
# the repo history contains a string that looks like a credential. Nothing here
# touches the network, a real credential or the workspace under test.
#
# Usage: GITLEAKS_BIN=/path/to/gitleaks .github/scripts/test-gitleaks-scan.sh
set -euo pipefail

: "${GITLEAKS_BIN:?GITLEAKS_BIN must point at the verified gitleaks binary}"
GITLEAKS_BIN="$(readlink -f "${GITLEAKS_BIN}")"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
scan="${here}/gitleaks-scan.sh"

work="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/gitleaks-selftest.XXXXXX")"
trap 'rm -rf "${work}"' EXIT

# A GitHub-PAT-shaped literal that matches the stock `github-pat` rule.
token="ghp_$(printf 'two-bot-next gitleaks self-test fixture' | sha256sum | cut -c1-36)"

failures=0
cases=0

# new_repo NAME: repo with one base commit, remote-tracking main pointing at it.
new_repo() {
  repo="${work}/$1"
  mkdir -p "${repo}"
  git -C "${repo}" init -q -b main
  git -C "${repo}" config user.email selftest@example.invalid
  git -C "${repo}" config user.name selftest
  git -C "${repo}" config commit.gpgsign false
  printf '# base\n' > "${repo}/README.md"
}

# base_commit: commit everything staged in the working tree as the base branch.
base_commit() {
  git -C "${repo}" add -A
  git -C "${repo}" commit -q -m base
  git -C "${repo}" update-ref refs/remotes/origin/main HEAD
}

# pr_commit: commit the working-tree changes as the PR head.
pr_commit() {
  git -C "${repo}" add -A
  git -C "${repo}" commit -q -m pr
}

plant_token() {
  printf 'value = "%s"\n' "${token}" > "${repo}/fixture.txt"
}

write_stock_config() {
  cat > "${repo}/.gitleaks.toml" <<'TOML'
[extend]
useDefault = true
TOML
}

write_allowlist_config() {
  cat > "${repo}/.gitleaks.toml" <<'TOML'
[extend]
useDefault = true

[[allowlists]]
description = "allowlist for the planted fixture"
paths = ['''^fixture\.txt$''']
TOML
}

# Stock rules, for fingerprinting findings independent of any repo policy.
printf '[extend]\nuseDefault = true\n' > "${work}/stock.toml"

# scan EVENT [BASE_REF] [WRAPPER]: run the wrapper in ${repo}; prints its exit code.
scan_rc() {
  local rc=0
  (cd "${repo}" && GITLEAKS_BIN="${GITLEAKS_BIN}" EVENT_NAME="$1" BASE_REF="${2:-main}" \
    "${3:-${scan}}" >"${work}/last.log" 2>&1) || rc=$?
  printf '%s' "${rc}"
}

# expect NAME WANT_RC GOT_RC
expect() {
  cases=$((cases + 1))
  if [[ "$2" == "$3" ]]; then
    printf 'ok   %s (exit %s)\n' "$1" "$3"
  else
    failures=$((failures + 1))
    printf 'FAIL %s: want exit %s, got %s\n' "$1" "$2" "$3"
    sed 's/^/     | /' "${work}/last.log" | tail -n 25
  fi
}

# fingerprint_of CONFIG: gitleaks fingerprint of the planted finding in ${repo}.
fingerprint_of() {
  (cd "${repo}" && "${GITLEAKS_BIN}" git . --config "$1" --log-opts=HEAD --no-banner \
    --exit-code 0 --report-format json --report-path - 2>/dev/null) \
    | grep -o '"Fingerprint": "[^"]*"' | head -n 1 | cut -d'"' -f4
}

# -- controls: the scan detects the token and passes a clean PR ---------------
new_repo clean
write_stock_config
base_commit
printf 'more docs\n' >> "${repo}/README.md"
pr_commit
expect "pr: clean change passes" 0 "$(scan_rc pull_request)"

new_repo plain
write_stock_config
base_commit
plant_token
pr_commit
expect "pr: planted token is detected" 1 "$(scan_rc pull_request)"

# -- the bypasses a PR could add itself ---------------------------------------
new_repo pr-config
write_stock_config
base_commit
plant_token
write_allowlist_config
pr_commit
expect "pr: allowlist added in .gitleaks.toml does not silence the scan" 1 "$(scan_rc pull_request)"

new_repo pr-ignore
write_stock_config
base_commit
plant_token
pr_commit
fp="$(fingerprint_of "${work}/stock.toml")"
if [[ -z "${fp}" ]]; then
  failures=$((failures + 1))
  echo "FAIL pr: could not derive the fingerprint of the planted token"
else
  printf '%s\n' "${fp}" > "${repo}/.gitleaksignore"
  pr_commit
  expect "pr: fingerprint added in .gitleaksignore does not silence the scan" 1 "$(scan_rc pull_request)"
fi

new_repo pr-ignore-edit
write_stock_config
printf '# base ignore file with only a comment\n' > "${repo}/.gitleaksignore"
base_commit
plant_token
pr_commit
fp="$(fingerprint_of "${work}/stock.toml")"
printf '%s\n' "${fp}" >> "${repo}/.gitleaksignore"
pr_commit
expect "pr: fingerprint appended to an existing .gitleaksignore does not silence the scan" 1 "$(scan_rc pull_request)"

# A repo-local wrapper must survive a PR-controlled ignore symlink to itself.
new_repo pr-ignore-symlink
write_stock_config
printf '# base ignore file with only a comment\n' > "${repo}/.gitleaksignore"
mkdir -p "${repo}/.github/scripts"
cp "${scan}" "${repo}/.github/scripts/gitleaks-scan.sh"
base_commit
rm -f "${repo}/.gitleaksignore"
ln -s .github/scripts/gitleaks-scan.sh "${repo}/.gitleaksignore"
plant_token
pr_commit
expect "pr: .gitleaksignore symlink cannot overwrite the running wrapper" 1 \
  "$(scan_rc pull_request main "${repo}/.github/scripts/gitleaks-scan.sh")"

new_repo pr-inline
write_stock_config
base_commit
printf 'value = "%s" # gitleaks:allow\n' "${token}" > "${repo}/fixture.txt"
pr_commit
expect "pr: inline gitleaks:allow does not silence the scan" 1 "$(scan_rc pull_request)"

# -- policy that already merged is still honoured -----------------------------
new_repo base-allowlist
write_allowlist_config
base_commit
plant_token
pr_commit
expect "pr: allowlist already on the base branch is honoured" 0 "$(scan_rc pull_request)"

new_repo base-ignore
write_stock_config
plant_token
git -C "${repo}" add -A
git -C "${repo}" commit -q -m "token on the base branch"
fp="$(fingerprint_of "${work}/stock.toml")"
printf '%s\n' "${fp}" > "${repo}/.gitleaksignore"
base_commit
printf 'more docs\n' >> "${repo}/README.md"
pr_commit
expect "pr: .gitleaksignore already on the base branch is honoured" 0 "$(scan_rc pull_request)"

# -- base branch without any policy files -------------------------------------
new_repo no-base-config
base_commit
printf 'more docs\n' >> "${repo}/README.md"
pr_commit
expect "pr: base without policy files, clean change passes" 0 "$(scan_rc pull_request)"

new_repo no-base-config-leak
base_commit
plant_token
write_allowlist_config
pr_commit
expect "pr: base without policy files, PR-added allowlist is ignored" 1 "$(scan_rc pull_request)"

# -- fail closed when the base branch is missing ------------------------------
new_repo no-base-ref
base_commit
printf 'more docs\n' >> "${repo}/README.md"
pr_commit
expect "pr: unfetched base branch fails the scan" 2 "$(scan_rc pull_request missing-branch)"

# -- non-PR runs keep the repo's own (already merged) policy ------------------
new_repo push-allowlist
write_allowlist_config
plant_token
base_commit
expect "push: merged allowlist is honoured" 0 "$(scan_rc push)"

new_repo push-leak
write_stock_config
plant_token
base_commit
expect "push: planted token is detected" 1 "$(scan_rc push)"

printf '\n%d cases, %d failed\n' "${cases}" "${failures}"
[[ "${failures}" -eq 0 ]]
