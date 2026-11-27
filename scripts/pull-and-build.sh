#!/bin/sh
set -eu

profile=${1-release}
if [ "$#" -gt 1 ] || { [ "$profile" != release ] && [ "$profile" != debug ]; }; then
    printf 'Usage: sh %s [release|debug]\n' "$0" >&2
    exit 2
fi

cd "$(dirname "$0")/.."

if command -v cargo >/dev/null 2>&1; then
    cargo=cargo
else
    cargo=${CARGO_HOME:-"$HOME/.cargo"}/bin/cargo
    if [ ! -x "$cargo" ]; then
        printf '%s\n' 'Cargo was not found. Install Rust with rustup or add Cargo to PATH.' >&2
        exit 127
    fi
fi

git pull --ff-only
exec "$cargo" dist "$profile"
