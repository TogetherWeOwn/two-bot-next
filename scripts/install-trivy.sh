#!/usr/bin/env bash
# Install only the reviewed Linux/amd64 release; never execute downloaded installers.
set -euo pipefail

version=0.69.3
archive="trivy_${version}_Linux-64bit.tar.gz"
# Revalidate against the release checksums AND GitHub asset digest when bumping.
sha256=1816b632dfe529869c740c0913e36bd1629cb7688bd5634f4a858c1d57c88b75
url="https://github.com/aquasecurity/trivy/releases/download/v${version}/${archive}"

if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
  printf 'Trivy installer requires Linux/x86_64\n' >&2
  exit 1
fi
: "${RUNNER_TEMP:?runner scratch directory required}"
: "${GITHUB_PATH:?Actions PATH file required}"
download=$(mktemp -d "$RUNNER_TEMP/trivy-download.XXXXXX")
bin=$(mktemp -d "$RUNNER_TEMP/trivy-bin.XXXXXX")
installed=false
cleanup() {
  rm -rf -- "$download"
  if [[ "$installed" != true ]]; then
    rm -rf -- "$bin"
  fi
}
trap cleanup EXIT

printf 'Downloading Trivy %s from %s\n' "$version" "$url"
# -q disables runner curlrc overrides. Bound the whole retry sequence as well
# as each attempt; diagnostics never print signed redirect URLs or response bodies.
if timeout --kill-after=5s 360s curl -q --fail --silent --show-error --location \
  --proto '=https' --proto-redir '=https' --max-redirs 3 \
  --connect-timeout 10 --max-time 120 --retry 2 --retry-all-errors \
  --retry-delay 2 --retry-max-time 300 --max-filesize 104857600 \
  --write-out 'Trivy archive HTTP %{http_code}; redirects %{num_redirects}\n' \
  --output "$download/$archive" "$url"; then
  printf 'Trivy archive download completed\n'
else
  status=$?
  printf 'Trivy archive download failed (exit %s)\n' "$status" >&2
  exit "$status"
fi
printf '%s  %s\n' "$sha256" "$download/$archive" | sha256sum --check --strict -
# Extract only the binary to a fresh directory, without trusting archive paths,
# permissions, links or other archive members. Integrity is checked first.
tar -xOzf "$download/$archive" -- trivy > "$bin/trivy"
chmod 0755 "$bin/trivy"
"$bin/trivy" --version
printf '%s\n' "$bin" >> "$GITHUB_PATH"
installed=true
