#!/usr/bin/env bash
# Native Syft inventory and Grype matching, without executing the image.
set -euo pipefail

image_id=$(<"$1/image-id.txt")
[[ "$image_id" =~ ^sha256:[0-9a-f]{64}$ ]]
output=$(realpath "$1")
scratch=$(mktemp -d "${RUNNER_TEMP:?}/auditable-scan.XXXXXX")
trap 'rm -rf "$scratch"' EXIT

# Separate empty extraction directories; no downloaded installer is executed.
mkdir "$scratch/syft" "$scratch/grype" "$scratch/cache"
curl --fail --location --silent --show-error --max-time 120 \
  https://github.com/anchore/syft/releases/download/v1.56.0/syft_1.56.0_linux_amd64.tar.gz \
  -o "$scratch/syft/tool.tar.gz"
printf '%s  %s\n' 1c2493cfcf52d3367d6cb25780d9190c111c2219c34c820574c43e8bae15c528 "$scratch/syft/tool.tar.gz" | sha256sum --check
tar -xzf "$scratch/syft/tool.tar.gz" -C "$scratch/syft" syft
curl --fail --location --silent --show-error --max-time 120 \
  https://github.com/anchore/grype/releases/download/v0.121.1/grype_0.121.1_linux_amd64.tar.gz \
  -o "$scratch/grype/tool.tar.gz"
printf '%s  %s\n' bc3ea3f9e6fd56ac781c08c6c5e614271b79f438200757f6932085c19b0a4657 "$scratch/grype/tool.tar.gz" | sha256sum --check
tar -xzf "$scratch/grype/tool.tar.gz" -C "$scratch/grype" grype

export SYFT_CHECK_FOR_APP_UPDATE=false GRYPE_CHECK_FOR_APP_UPDATE=false
export XDG_CACHE_HOME="$scratch/cache" GRYPE_DB_CACHE_DIR="$scratch/cache/grype"
timeout 180 "$scratch/syft/syft" scan "docker:$image_id" --config /dev/null \
  --select-catalogers cargo-auditable-binary-cataloger \
  -o "syft-json=$output/runtime-crates.syft.json"
# Keep every match; the existing lockfile and image gates remain authoritative.
# No --fail-on here: this report proves inventory/matching, not a new exception.
timeout 300 "$scratch/grype/grype" "sbom:$output/runtime-crates.syft.json" \
  --config /dev/null -o json > "$output/runtime-crates.grype.json"
