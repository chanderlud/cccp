#!/bin/bash

set -euxo pipefail

main() {
    cargo fmt -- --check

    if [ "$TARGET" = "mips-unknown-linux-musl" ] || [ "$TARGET" = "mipsel-unknown-linux-musl" ]; then
        # MIPS targets require opt-level 1 due to a known issue in Rust

        RUSTFLAGS='-C opt-level=1' cross build --target $TARGET

        [ -n "${DISABLE_TESTS:-}" ] && return

        RUSTFLAGS='-C opt-level=1' cross clippy --target $TARGET
    else
        cross build --target $TARGET

        [ -n "${DISABLE_TESTS:-}" ] && return

        cross clippy --target $TARGET
    fi
}

# only run if not deploying
[ -z "$TRAVIS_TAG" ] && main
