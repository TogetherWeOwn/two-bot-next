#!/usr/bin/env bash
# Offline self-test for gitleaks-scan.sh: check history coverage and verify
# that PR, merge-queue and dispatch scans enforce the trusted base policy.
#
# Builds throwaway git repos with refs/remotes/origin/main standing in for the
# fetched base branch and runs the wrapper with the real binary. It derives a
# synthetic token at run time so no credential-shaped literal is committed.
# Nothing here touches the network, a real credential or the workspace under test.
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

# expect_finding NAME FINGERPRINT GOT_RC: exit 1 alone also matches scan errors.
expect_finding() {
  cases=$((cases + 1))
  if [[ "$3" == 1 ]] && ! grep -Fq "${token}" "${work}/last.log" &&
      sed -n 's/^Fingerprint:[[:blank:]]*//p' "${work}/last.log" | grep -Fx "$2" >/dev/null; then
    printf 'ok   %s (redacted finding)\n' "$1"
  else
    failures=$((failures + 1))
    printf 'FAIL %s: expected redacted finding %s and exit 1, got exit %s\n' "$1" "$2" "$3"
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

# -- history coverage --------------------------------------------------------
new_repo no-diff
write_stock_config
base_commit
printf 'fixture.txt -diff\n' > "${repo}/.gitattributes"
plant_token
pr_commit
expect_finding "pr: token in a -diff file is detected" \
  "$(git -C "${repo}" rev-parse HEAD):fixture.txt:github-pat:1" "$(scan_rc pull_request)"

new_repo merge-resolution
write_stock_config
printf 'base\n' > "${repo}/fixture.txt"
base_commit

git -C "${repo}" switch -q -c side
printf 'side\n' > "${repo}/fixture.txt"
pr_commit

git -C "${repo}" switch -q main
printf 'main\n' > "${repo}/fixture.txt"
base_commit

if git -C "${repo}" merge --no-edit side || [[ -z "$(git -C "${repo}" ls-files -u -- fixture.txt)" ]]; then
  printf 'FAIL merge-resolution setup: expected a conflict in fixture.txt\n' >&2
  exit 1
fi
plant_token
pr_commit
expect_finding "pr: token introduced only in a merge resolution is detected" \
  "$(git -C "${repo}" rev-parse HEAD):fixture.txt:github-pat:1" "$(scan_rc pull_request)"

new_repo merge-inherited-ignore
write_stock_config
base_commit
git -C "${repo}" switch -q -c side
plant_token
pr_commit
fp="$(fingerprint_of "${work}/stock.toml")"
: "${fp:?expected the original fixture fingerprint}"
git -C "${repo}" switch -q main
printf '%s\n' "${fp}" > "${repo}/.gitleaksignore"
base_commit
git -C "${repo}" merge -q --no-ff --no-edit side
expect "pr: exact fingerprint is honoured for a finding inherited unchanged" 0 "$(scan_rc pull_request)"
printf 'other = "%s"\n' "${token}" >> "${repo}/fixture.txt"
git -C "${repo}" add fixture.txt
git -C "${repo}" commit -q --amend --no-edit
expect_finding "pr: exact fingerprint does not cover a new merge finding" \
  "$(git -C "${repo}" rev-parse HEAD):fixture.txt:github-pat:2" "$(scan_rc pull_request)"

new_repo octopus-merge
write_stock_config
base_commit
for side in first second; do
  git -C "${repo}" switch -q -c "${side}" main
  printf 'side\n' > "${repo}/${side}.txt"
  pr_commit
done
git -C "${repo}" switch -q main
printf 'more docs\n' >> "${repo}/README.md"
base_commit
git -C "${repo}" merge -q --no-ff --no-commit first second
plant_token
pr_commit
git -C "${repo}" rev-parse --verify HEAD^3 >/dev/null
expect_finding "pr: token introduced by a three-parent merge is detected" \
  "$(git -C "${repo}" rev-parse HEAD):fixture.txt:github-pat:1" "$(scan_rc pull_request)"

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

# -- merge_group runs honour the base policy, never the queue commit's own ---
# BASE_REF arrives as a full ref (refs/heads/main) on merge_group, so every
# case below passes it that way to prove the wrapper strips the prefix.
new_repo mg-clean
write_stock_config
base_commit
printf 'more docs\n' >> "${repo}/README.md"
pr_commit
expect "merge_group: clean change passes" 0 "$(scan_rc merge_group refs/heads/main)"

new_repo mg-plain
write_stock_config
base_commit
plant_token
pr_commit
expect "merge_group: planted token is detected" 1 "$(scan_rc merge_group refs/heads/main)"

new_repo mg-config
write_stock_config
base_commit
plant_token
write_allowlist_config
pr_commit
expect "merge_group: allowlist added in .gitleaks.toml does not silence the scan" 1 \
  "$(scan_rc merge_group refs/heads/main)"

new_repo mg-ignore
write_stock_config
base_commit
plant_token
pr_commit
fp="$(fingerprint_of "${work}/stock.toml")"
if [[ -z "${fp}" ]]; then
  failures=$((failures + 1))
  echo "FAIL merge_group: could not derive the fingerprint of the planted token"
else
  printf '%s\n' "${fp}" > "${repo}/.gitleaksignore"
  pr_commit
  expect "merge_group: fingerprint added in .gitleaksignore does not silence the scan" 1 \
    "$(scan_rc merge_group refs/heads/main)"
fi

new_repo mg-base-allowlist
write_allowlist_config
base_commit
plant_token
pr_commit
expect "merge_group: allowlist already on the base branch is honoured" 0 \
  "$(scan_rc merge_group refs/heads/main)"

new_repo mg-no-base-ref
base_commit
printf 'more docs\n' >> "${repo}/README.md"
pr_commit
expect "merge_group: unfetched base branch fails the scan" 2 \
  "$(scan_rc merge_group refs/heads/missing-branch)"

# -- PR-validation dispatch uses the merged main policy -----------------------
new_repo dispatch-config
write_stock_config
base_commit
plant_token
write_allowlist_config
pr_commit
expect "dispatch: PR-added config exclusion is ignored" 1 "$(scan_rc workflow_dispatch)"

new_repo dispatch-ignore
write_stock_config
base_commit
plant_token
pr_commit
fp="$(fingerprint_of "${work}/stock.toml")"
if [[ -z "${fp}" ]]; then
  failures=$((failures + 1))
  echo "FAIL dispatch: could not derive the fingerprint of the planted token"
else
  printf '%s\n' "${fp}" > "${repo}/.gitleaksignore"
  pr_commit
  expect "dispatch: PR-added ignore exclusion is ignored" 1 "$(scan_rc workflow_dispatch)"
fi

new_repo dispatch-clean
write_stock_config
base_commit
printf 'more docs\n' >> "${repo}/README.md"
pr_commit
expect "dispatch: clean change passes" 0 "$(scan_rc workflow_dispatch refs/heads/main)"

new_repo dispatch-base-config
write_allowlist_config
base_commit
plant_token
pr_commit
expect "dispatch: merged config exclusion is honoured" 0 "$(scan_rc workflow_dispatch)"

new_repo dispatch-base-ignore
write_stock_config
plant_token
git -C "${repo}" add -A
git -C "${repo}" commit -q -m "synthetic fixture on base"
fp="$(fingerprint_of "${work}/stock.toml")"
if [[ -z "${fp}" ]]; then
  failures=$((failures + 1))
  echo "FAIL dispatch: could not derive the base fingerprint"
else
  printf '%s\n' "${fp}" > "${repo}/.gitleaksignore"
  base_commit
  printf 'more docs\n' >> "${repo}/README.md"
  pr_commit
  expect "dispatch: merged ignore exclusion is honoured" 0 "$(scan_rc workflow_dispatch)"
fi

new_repo dispatch-no-base
write_stock_config
base_commit
printf 'more docs\n' >> "${repo}/README.md"
pr_commit
git -C "${repo}" update-ref -d refs/remotes/origin/main
expect "dispatch: missing trusted main fails closed" 2 "$(scan_rc workflow_dispatch)"

new_repo dispatch-wrong-base
write_stock_config
base_commit
printf 'more docs\n' >> "${repo}/README.md"
pr_commit
git -C "${repo}" update-ref refs/remotes/origin/feature HEAD
expect "dispatch: fetched non-main policy is refused" 2 "$(scan_rc workflow_dispatch feature)"
expect "dispatch: revision syntax in base ref is refused" 2 "$(scan_rc workflow_dispatch 'main~1')"

printf '\n%d cases, %d failed\n' "${cases}" "${failures}"
[[ "${failures}" -eq 0 ]]
