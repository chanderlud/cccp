#!/bin/bash

set -ex

main() {
    # install binstall
    curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash

    # install cross w/ binstall
    cargo binstall cross --git https://github.com/cross-rs/cross --no-confirm

    # these targets require building a custom image
    case "$TARGET" in
        *darwin*)
            IMAGE_NAME="$TARGET-cross:local"
            if ! docker image inspect "$IMAGE_NAME" > /dev/null 2>&1; then
                git clone https://github.com/cross-rs/cross
                cd cross
                git submodule update --init --remote

                case "$TARGET" in
                    i686-apple-darwin)
                        # sdk v10.13 for i686
                        MACOS_SDK_URL='MACOS_SDK_URL=https://github.com/phracker/MacOSX-SDKs/releases/download/11.3/MacOSX10.13.sdk.tar.xz'
                        ;;
                    *)
                        # sdk v11.3 for x86_64 & aarch64
                        MACOS_SDK_URL='MACOS_SDK_URL=https://github.com/phracker/MacOSX-SDKs/releases/download/11.3/MacOSX11.3.sdk.tar.xz'
                        ;;
                esac

                cargo build-docker-image $TARGET-cross --tag local --build-arg $MACOS_SDK_URL
            else
                echo "Docker image $IMAGE_NAME already exists."
            fi
            ;;
        "aarch64-pc-windows-msvc" | "x86_64-pc-windows-msvc" | "i686-pc-windows-msvc")
            IMAGE_NAME="$TARGET-cross:local"
            if ! docker image inspect "$IMAGE_NAME" > /dev/null 2>&1; then
                git clone https://github.com/cross-rs/cross
                cd cross
                git submodule update --init --remote
                cargo build-docker-image $TARGET-cross --tag local
            else
                echo "Docker image $IMAGE_NAME already exists."
            fi
            ;;
    esac
}

main
