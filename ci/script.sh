#!/bin/bash

set -euxo pipefail

main() {
    if command -v protoc >/dev/null 2>&1; then
        echo "protoc is installed"
    else
        echo "protoc is not installed"
        apt-get -y update
        apt-get install -y wget unzip
        wget https://github.com/protocolbuffers/protobuf/releases/download/v25.2/protoc-25.2-linux-x86_64.zip
        unzip protoc-25.2-linux-x86_64.zip
        mv bin/protoc /usr/local/bin/
    fi

    cargo fmt -- --check

    case "$TARGET" in
      *darwin*)
        $HOME/.cargo/bin/rustup component add rust-src

        cross build --target $TARGET -Z build-std=core,std,alloc,proc_macro
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
