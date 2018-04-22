# This script takes care of testing your crate

set -ex

main() {
    if [ ! -z $DISABLE_TESTS ]; then
        return
    fi

    cargo test --all
}

# we don't run the "test phase" when doing deploys
if [ -z $TRAVIS_TAG ]; then
    main
fi
