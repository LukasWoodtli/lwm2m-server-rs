#!/usr/bin/bash

set -ue

rust_versions=("stable" "nightly" "1.81")


for ver in "${rust_versions[@]}"
do
	figlet "Rust: $ver"
	rustup override set $ver

	cargo --version
	rustc --version

	cargo fmt --all

	cargo clean
	cargo build

	cargo test --verbose

	cargo clippy --all-targets --all-features -- -D warnings


	echo "===================="

done

