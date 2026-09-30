#!/usr/bin/env bash
# Install wm-fetch by SYMLINK to the built release binary so this repo
# stays the single source of truth (v1.0 was lost because a copy lived
# only in one machine's ~/.local/bin). Idempotent; refuses to clobber a
# foreign file.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
target="$HOME/.local/bin/wm-fetch"
bin="$here/target/release/wm-fetch"

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found — install a Rust toolchain (https://rustup.rs) or grab a release binary:" >&2
  echo "  https://github.com/tieguy/wm-fetch/releases" >&2
  exit 1
fi

echo "building (release)…"
cargo build --release --manifest-path "$here/Cargo.toml"

mkdir -p "$HOME/.local/bin"

if [ -L "$target" ] && [ "$(readlink "$target")" = "$bin" ]; then
  echo "already installed: $target -> $bin"
elif [ -e "$target" ] && [ ! -L "$target" ]; then
  echo "refusing to overwrite non-symlink $target (move it aside first)" >&2
  exit 1
else
  ln -sfn "$bin" "$target"
  echo "installed: $target -> $bin"
fi

config="${XDG_CONFIG_HOME:-$HOME/.config}/wm-fetch/config.toml"
if [ ! -f "$config" ]; then
  echo
  echo "next step: configure your contact info (required before first fetch):"
  echo "  $target --init   # writes a template at $config — edit in your email/user page"
fi
