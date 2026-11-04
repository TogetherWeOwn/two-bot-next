#!/usr/bin/env bash
# Hosted Ubuntu and the pinned rust containers already supply rustup. Refuse
# missing tools instead of executing a network bootstrap or using a moving pin.
set -euo pipefail
command -v rustup
command -v cargo
command -v rustc

[[ ${RUST_TOOLCHAIN:-} =~ ^([0-9]+\.[0-9]+\.[0-9]+|nightly-[0-9]{4}-[0-9]{2}-[0-9]{2})$ ]] || {
  printf '%s\n' 'An exact Rust toolchain is required' >&2
  exit 1
}
# Match rustup's existing runner/container home, including a custom CARGO_HOME.
export CARGO_HOME=${CARGO_HOME:-"$HOME/.cargo"}
[[ $CARGO_HOME != *$'\n'* && $CARGO_HOME != *$'\r'* ]] || {
  printf '%s\n' 'Invalid Cargo home' >&2
  exit 1
}
flags=()
components_input=${RUST_COMPONENTS:-}
read -r -a components <<< "${components_input//,/ }"
for component in "${components[@]}"; do
  flags+=(--component "$component")
done
export RUSTUP_PERMIT_COPY_RENAME=1
rustup toolchain install "$RUST_TOOLCHAIN" "${flags[@]}" --profile minimal --no-self-update --force-non-host
rustup default "$RUST_TOOLCHAIN" --force-non-host
rustc +"$RUST_TOOLCHAIN" --version --verbose

# Preserve the former setup action's environment for subsequent steps. The
# home is single-line runner data; incremental/color defaults are fixed strings.
printf 'CARGO_HOME=%s\n' "$CARGO_HOME" >> "$GITHUB_ENV"
if [[ ! -v CARGO_INCREMENTAL ]]; then
  printf '%s\n' 'CARGO_INCREMENTAL=0' >> "$GITHUB_ENV"
fi
if [[ ! -v CARGO_TERM_COLOR ]]; then
  printf '%s\n' 'CARGO_TERM_COLOR=always' >> "$GITHUB_ENV"
fi
