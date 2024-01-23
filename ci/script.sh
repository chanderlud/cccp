#!/bin/bash

set -euxo pipefail

main() {
    cargo fmt -- --check

    case "$TARGET" in
        *darwin*)
            # macOs targets require rust-src component
            $HOME/.cargo/bin/rustup component add rust-src
            CMD="cross build --target ${TARGET}"
            ;;
        "mipsel-unknown-linux-musl" | "mips-unknown-linux-musl")
            # these targets require opt-level 1 due to a known issue in Rust
            CMD="RUSTFLAGS=\"-C opt-level=1\" cross build --target ${TARGET}"
            ;;
        *)
            CMD="cross build --target ${TARGET}"
            ;;
    esac

    if [ -n "${OPT_LEVEL:-}" ]; then
        PROFILE="opt-level-${OPT_LEVEL}"
    else
        PROFILE="release"
    fi

    if [ "${NO_INSTALLER:-0}" = "1" ]; then
        $CMD --no-default-features --profile $PROFILE
    else
        $CMD --profile $PROFILE
    fi

    if [ "${ENABLE_TESTS:-0}" = "1" ]; then
        cross clippy --target $TARGET
    fi
}

# only run if not deploying
if [ -z $TRAVIS_TAG ]; then
    main
fi
