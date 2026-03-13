RUST_BACKTRACE=1 cargo test --doc

cargo test -p lio --all-features --lib --release
cargo test -p lio --all-features --test '*' --release
