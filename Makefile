.PHONY: lint lint-full doc cbuild test test-lio-uring test-doc test-lib test-integration test-nix vm-linux vm-windows vm-freebsd vm-all

lint:
	nix develop .#ci -c ./scripts/lint.sh

lint-full:
	nix develop .#ci -c ./scripts/lint-full.sh

fmt-fix:
	nix develop -c cargo fmt

doc: test-doc
	RUSTDOCFLAGS="--cfg docsrs" nix develop -c cargo doc --no-deps --all-features

cbuild:
	cargo rustc -p lio --crate-type dylib,staticlib --features unstable_ffi --release

test:
	nix develop -c ./scripts/test.sh

test-lio-uring:
	nix develop -c ./scripts/test-lio-uring.sh

test-ffi:
	nix develop -c ./scripts/test-ffi.sh

# VM-based cross-platform testing
vm-linux:
	./vm/linux/run.sh

vm-windows:
	./vm/windows/run.sh

vm-freebsd:
	./vm/freebsd/run.sh

vm-all: vm-linux vm-freebsd vm-windows
