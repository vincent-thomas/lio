RUST_BACKTRACE=1 cargo test --doc

FEATURES=$(cargo metadata --no-deps --format-version 1   | jq -r '.packages[0].features | keys[]'   | grep -v '^unstable_ffi$'   | tr '\n' ' ')

cargo test -p lio --features "$FEATURES" --release --lib
cargo test -p lio --features "$FEATURES" --release --test '*'
