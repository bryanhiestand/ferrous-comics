.PHONY: build release check fmt fmt-check lint test clean run dump install

## Default target
build:
	cargo build

## Optimized release build
release:
	cargo build --release

## Run all checks (format + lint + test)
check: fmt-check lint test

## Format source code in place
fmt:
	cargo fmt

## Check formatting without writing
fmt-check:
	cargo fmt --check

## Lint with warnings as errors (mirrors CI)
lint:
	cargo clippy -- -D warnings

## Run tests
test:
	cargo test

## Remove build artifacts
clean:
	cargo clean

## Run the binary (debug build)
run:
	cargo run

## Dump all comic history records as newline-delimited JSON
dump:
	cargo run -- dump

## Install release binary to ~/.cargo/bin
install:
	cargo install --path .
