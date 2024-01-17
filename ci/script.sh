#!/bin/bash

set -euxo pipefail

main() {
    cargo fmt -- --check

    case "$TARGET" in
        *darwin*)
            $HOME/.cargo/bin/rustup component add rust-src

            cross build --target $TARGET
            ;;
        "mips-unknown-linux-musl" | "mipsel-unknown-linux-musl")
            # MIPS targets require opt-level 1 due to a known issue in Rust
            RUSTFLAGS='-C opt-level=1' cross build --target $TARGET
            ;;
        *)
            cross build --target $TARGET

            if [ "${ENABLE_TESTS:-0}" = "1" ]; then
                cross clippy --target $TARGET
            fi
            ;;
    esac
}

# only run if not deploying
[ -z "$TRAVIS_TAG" ] && main
