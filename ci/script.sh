# This script takes care of testing your crate

set -ex

main() {
    docker pull ghcr.io/cross-rs/$TARGET:main

    cross build --target $TARGET

    if [ ! -z $DISABLE_TESTS ]; then
        return
    fi

    cross fmt --target $TARGET
    cross clippy --target $TARGET
}

# we don't run the "test phase" when doing deploys
if [ -z $TRAVIS_TAG ]; then
    main
fi