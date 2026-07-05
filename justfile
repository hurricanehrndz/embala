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

# Cross-compile the fixture Windows test exes (0.2.0 feeds the upgrade test)
fixtures:
    mkdir -p fixtures/hello/dist
    x86_64-w64-mingw32-cc -DVERSION='"0.1.0"' -o fixtures/hello/dist/hello.exe fixtures/hello/hello.c
    x86_64-w64-mingw32-cc -DVERSION='"0.2.0"' -o fixtures/hello/dist/hello-0.2.0.exe fixtures/hello/hello.c
