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

    if [ -n "${OPT_LEVEL}" ]; then
        PROFILE="opt-level-${OPT_LEVEL}"
    else
        PROFILE="release"
    fi

    if [ "${NO_INSTALLER:-0}" = "1" ]; then
        cross build --target $TARGET --no-default-features --profile $PROFILE
    else
        cross build --target $TARGET --release --profile $PROFILE
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