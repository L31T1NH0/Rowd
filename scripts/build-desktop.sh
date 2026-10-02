#!/usr/bin/env bash
# Build the current desktop version and keep the terminal command on this build.
set -euo pipefail
cd "$(dirname "$0")/.."
rowd_root="$PWD"
rowd_cargo="${ROWD_CARGO:-$HOME/.cargo/bin/cargo}"
"$rowd_cargo" build --release -p rowd --locked
rowd_bin_dir="${ROWD_BIN_DIR:-$HOME/.local/bin}"
mkdir -p "$rowd_bin_dir"
ln -sfnT -- "$rowd_root/target/release/rowd" "$rowd_bin_dir/rowd"
"$rowd_bin_dir/rowd" --version
