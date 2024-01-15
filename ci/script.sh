# This script takes care of testing your crate

set -ex

main() {
    cargo fmt -- --check

    case $TARGET in
        # mips targets require opt-level 1 due to rust-lang/rust/issues/108835
        mips-unknown-linux-musl)
            RUSTFLAGS='-C opt-level=1' cross build --target $TARGET

            if [ ! -z $DISABLE_TESTS ]; then
                return
            fi

            RUSTFLAGS='-C opt-level=1' cross clippy --target $TARGET
            ;;
        mipsel-unknown-linux-musl)
            RUSTFLAGS='-C opt-level=1' cross build --target $TARGET

            if [ ! -z $DISABLE_TESTS ]; then
                return
            fi

            RUSTFLAGS='-C opt-level=1' cross clippy --target $TARGET
            ;;
        *)
            cross build --target $TARGET

            if [ ! -z $DISABLE_TESTS ]; then
                return
            fi

            cross clippy --target $TARGET
            ;;
    esac
}

# we don't run the "test phase" when doing deploys
if [ -z $TRAVIS_TAG ]; then
    main
fi