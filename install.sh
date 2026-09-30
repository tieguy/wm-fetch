#!/usr/bin/env bash
# Install wm-fetch by SYMLINK so this repo stays the single source of
# truth (v1.0 was lost because a copy lived only in one machine's
# ~/.local/bin). Idempotent; refuses to clobber a foreign file.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
target="$HOME/.local/bin/wm-fetch"

mkdir -p "$HOME/.local/bin"

if [ -L "$target" ] && [ "$(readlink "$target")" = "$here/wm-fetch" ]; then
  echo "already installed: $target -> $here/wm-fetch"
  exit 0
fi
if [ -e "$target" ] && [ ! -L "$target" ]; then
  echo "refusing to overwrite non-symlink $target (move it aside first)" >&2
  exit 1
fi
ln -sfn "$here/wm-fetch" "$target"
echo "installed: $target -> $here/wm-fetch"
