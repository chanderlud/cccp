#!/bin/bash

set -euxo pipefail

main() {
    SRC=$(pwd)
    STAGE=$(mktemp -d)

    test -f Cargo.lock || cargo generate-lockfile

    case $TARGET in
        *darwin*)
            # macOs targets require rust-src component
            $HOME/.cargo/bin/rustup component add rust-src
            ;;
    esac

    if [ "${NO_INSTALLER:-0}" = "1" ]; then
        cross build --target $TARGET --release --no-default-features
    else
        cross build --target $TARGET --release
    fi

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