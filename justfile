# Run all tests on both stable and nightly toolchains
[parallel]
test: test-stable test-nightly

# Run all tests on stable toolchain (including trybuild UI tests)
test-stable:
    cargo +stable test --all --all-features

# Run tests on nightly toolchain (excluding trybuild UI tests)
# Trybuild tests are skipped because .stderr files are generated with stable
# and error span formatting differs between Rust versions
test-nightly:
    cargo +nightly test --all --all-features -- --skip ui

# Update trybuild .stderr files (must be run with stable)
update-trybuild:
    TRYBUILD=overwrite cargo +stable test -p sourcery-macros --test trybuild

lint:
    cargo +nightly clippy --all --all-features --all-targets

coverage:
    cargo +nightly llvm-cov --all-features --workspace --doctests -- --skip ui
