set -ex

main() {
    # Builds for iOS are done on OSX, but require the specific target to be
    # installed.
    # case $TARGET in
    #     aarch64-apple-ios)
    #         rustup target install aarch64-apple-ios
    #         ;;
    #     armv7-apple-ios)
    #         rustup target install armv7-apple-ios
    #         ;;
    #     armv7s-apple-ios)
    #         rustup target install armv7s-apple-ios
    #         ;;
    #     i386-apple-ios)
    #         rustup target install i386-apple-ios
    #         ;;
    #     x86_64-apple-ios)
    #         rustup target install x86_64-apple-ios
    #         ;;
    # esac

    # Install binstall
    curl -L --proto '=https' --tlsv1.2 -sSf https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash

    # Install cross w/ binstall
    cargo binstall cross --git https://github.com/cross-rs/cross --no-confirm
}

main