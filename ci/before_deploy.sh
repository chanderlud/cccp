#!/bin/bash

set -euxo pipefail

main() {
    local src=$(pwd)
    stage=$(mktemp -d)

    test -f Cargo.lock || cargo generate-lockfile

    case $TARGET in
        *darwin*)
            $HOME/.cargo/bin/rustup component add rust-src

            cross rustc --target $TARGET --release -Z build-std=core,std,alloc,proc_macro -- -C lto
            cp target/$TARGET/release/cccp $stage/
            ;;
        *windows*)
            cross rustc --target $TARGET --release -- -C lto
            cp target/$TARGET/release/cccp.exe $stage/
            ;;
        *)
            cross rustc --target $TARGET --release -- -C lto
            cp target/$TARGET/release/cccp $stage/
            ;;
    esac

    cd $stage
    tar czf $src/$CRATE_NAME-$TRAVIS_TAG-$TARGET.tar.gz *
    cd $src

    rm -rf $stage
}

main