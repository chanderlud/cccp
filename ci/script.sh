# This script takes care of testing your crate

set -ex

main() {
    # set CROSS_CONTAINER_OPTS="--platform linux/amd64"
    rustup component add rustfmt
    cross fmt --target $TARGET
    cross build --target $TARGET

    if [ ! -z $DISABLE_TESTS ]; then
        return
    fi

    cross clippy --target $TARGET
}

# we don't run the "test phase" when doing deploys
if [ -z $TRAVIS_TAG ]; then
    main
fi