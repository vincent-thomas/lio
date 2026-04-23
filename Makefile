.PHONY: lint lint-full doc book book-serve book-test cbuild test test-debug test-release test-lio-uring test-doc test-lib test-integration test-nix vm-linux vm-windows vm-freebsd vm-all

lint:
	nix run .#lint

lint-full:
	nix run .#lint-full

fmt-fix:
	nix develop -c cargo fmt

doc: test-doc
	RUSTDOCFLAGS="--cfg docsrs" nix develop -c cargo doc --no-deps --all-features

book:
	mdbook build

book-serve:
	mdbook serve

book-test:
	CARGO_TARGET_DIR=target/mdbook-test cargo build -p lio
	mdbook test -L target/mdbook-test/debug/deps

cbuild:
	cargo rustc -p lio --crate-type dylib,staticlib --features unstable_ffi --release

test: test-debug

test-debug:
	nix run .#test

test-release:
	nix run .#test -- --release

test-uring:
	nix run .#test-uring

test-ffi:
	nix run .#test-ffi

# VM-based cross-platform testing
vm-linux:
	./vm/linux/run.sh

vm-windows:
	./vm/windows/run.sh

vm-freebsd:
	./vm/freebsd/run.sh

vm-all: vm-linux vm-freebsd vm-windows
