#!/bin/bash

set -euxo pipefail

main() {
    local SRC=$(pwd)
    STAGE=$(mktemp -d)

    test -f Cargo.lock || cargo generate-lockfile

    case $TARGET in
        *darwin*)
            $HOME/.cargo/bin/rustup component add rust-src
            ;;
    esac

    cross build --target $TARGET --release

    if [[ -f "target/${TARGET}/release/cccp.exe" ]]; then
        mv "target/${TARGET}/release/cccp.exe" "${STAGE}/"
    else
        mv "target/${TARGET}/release/cccp" "${STAGE}/"
    fi

    cd $STAGE
    tar czf $src/$CRATE_NAME-$TRAVIS_TAG-$TARGET.tar.gz *
    cd $src

    rm -rf $STAGE
}

main