#!/bin/bash

set -euxo pipefail

main() {
    SRC=$(pwd)
    STAGE=$(mktemp -d)

    test -f Cargo.lock || cargo generate-lockfile

    case $TARGET in
        *darwin*)
            $HOME/.cargo/bin/rustup component add rust-src
            ;;
    esac

    case $TARGET in
        # these targets fail on opt level 3
        "aarch64-pc-windows-msvc" | "mips-unknown-linux-musl" | "mips64-unknown-linux-gnuabi64")
            cross build --target $TARGET --profile opt-level-2
            ;;
        *)
            cross build --target $TARGET --release
            ;;
    esac

    if [[ -f "target/${TARGET}/release/${CRATE_NAME}.exe" ]]; then
        mv "target/${TARGET}/release/${CRATE_NAME}.exe" "${STAGE}/"
    else
        mv "target/${TARGET}/release/${CRATE_NAME}" "${STAGE}/"
    fi

    cd $STAGE
    tar czf "${SRC}/${CRATE_NAME}-${TRAVIS_TAG}-${TARGET}.tar.gz" *
    cd $SRC

    rm -rf $STAGE
}

main