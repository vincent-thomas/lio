lint:
	cargo fmt
	cargo clippy --all-features

lint-flags:
	cargo hack check --feature-powerset --lib --tests

doc:
	RUSTDOCFLAGS="--cfg docsrs" cargo +nightly doc --no-deps --all-features

cbuild:
	cargo rustc -p lio --crate-type dylib --features unstable_ffi --release
	echo "lio: built c api at: $(pwd)/target/release/liblio.(dylib|so|dll)"

test:
	cargo nextest r --release -p lio --all-features --stress-count 4
	RUST_BACKTRACE=1 cargo test --doc
	./lio/tests/ffi/test.sh
	./lio/tests/nix-build/test.sh
