#!/bin/bash

set -euxo pipefail

main() {
    cargo fmt -- --check

    case "$TARGET" in
      *darwin*)
        $HOME/.cargo/bin/rustup component add rust-src clippy

        cross build --target $TARGET -Z build-std
        ;;
      "mips-unknown-linux-musl" | "mipsel-unknown-linux-musl")
        # MIPS targets require opt-level 1 due to a known issue in Rust
        RUSTFLAGS='-C opt-level=1' cross build --target $TARGET
        ;;
      *)
        cross build --target $TARGET

        [ -n "${DISABLE_TESTS:-}" ] && return

        cross clippy --target $TARGET
        ;;
    esac
}

# only run if not deploying
[ -z "$TRAVIS_TAG" ] && main
