#!/bin/sh
# Build release binaries of nanus and install them into a directory on PATH.
#
# Both binaries are installed, side by side: `nanus tui` runs `nanus-tui` from beside the
# core binary and never looks it up on PATH, so installing one without the other leaves the
# interface unreachable.
#
# Usage: scripts/install-local.sh [destination]
#   destination defaults to $NANUS_BIN_DIR, then ~/.local/bin.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-${NANUS_BIN_DIR:-$HOME/.local/bin}}

# `--workspace` is required: `default-members` is the kernel alone, so a bare release build
# produces neither program.
cargo build --release --workspace --manifest-path "$repo/Cargo.toml"

target=${CARGO_TARGET_DIR:-$repo/target}
mkdir -p "$dest"
for bin in nanus nanus-tui; do
    # Copy beside the destination and rename over it, rather than writing into the old file:
    # on macOS, overwriting a signed binary in place can leave the kernel holding a stale code
    # signature and kill the next launch, and a rename also leaves a running copy untouched.
    tmp="$dest/.$bin.tmp.$$"
    cp "$target/release/$bin" "$tmp"
    chmod 0755 "$tmp"
    mv -f "$tmp" "$dest/$bin"
    echo "installed $dest/$bin"
done

case ":$PATH:" in
    *":$dest:"*) ;;
    *) echo "note: $dest is not on PATH; add it to invoke nanus from anywhere" >&2 ;;
esac
