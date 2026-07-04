default:
    @just --list

build:
    cargo build

check:
    cargo check --workspace

test:
    cargo test --workspace

fmt:
    treefmt

lint:
    cargo clippy --workspace -- -D warnings
