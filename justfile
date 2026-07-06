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

# Cross-compile the setup.exe runtime stub for both Windows arches via zig and
# stage the committed artifacts embala-setup include_bytes!-embeds. Dev-shell
# only (needs zig + cargo-zigbuild) — `embala build` never compiles.
stubs:
    #!/usr/bin/env bash
    set -euo pipefail
    for target in x86_64-pc-windows-gnu aarch64-pc-windows-gnullvm; do
        cargo zigbuild -p embala-setup-runtime --release --target "$target"
        dest="crates/embala-setup/stubs/$target"
        mkdir -p "$dest"
        cp "target/$target/release/setup-stub.exe" "$dest/setup-stub.exe"
    done

# Differential oracle: build the same install with wixl and with embala,
# export every table from both, and diff table-by-table. Inspection tool —
# differences are reported, not fatal. Ids/GUIDs/short-names may differ;
# whole missing table classes are what to look for.
msi-diff:
    #!/usr/bin/env bash
    set -euo pipefail
    workdir=/tmp/msi-diff
    rm -rf "$workdir" && mkdir -p "$workdir/ref" "$workdir/ours"
    wixl -a x64 -o "$workdir/ref.msi" fixtures/wix/hello.wxs
    cargo run -q -p embala -- build --config fixtures/hello/embala.toml --formats msi --out-dir dist
    cp dist/hello-0.1.0-x86_64.msi "$workdir/ours.msi"
    for side in ref ours; do
        for t in $(msiinfo tables "$workdir/$side.msi"); do
            [ "$t" = "_SummaryInformation" ] && continue
            msiinfo export "$workdir/$side.msi" "$t" > "$workdir/$side/$t.idt" 2>/dev/null || true
            # Drop empty tables (wixl emits every table it knows, mostly empty)
            [ "$(wc -l < "$workdir/$side/$t.idt")" -le 3 ] && rm -f "$workdir/$side/$t.idt"
        done
    done
    echo "=== table classes: ref (wixl) vs ours (embala) ==="
    comm <(ls "$workdir/ref") <(ls "$workdir/ours") \
        | sed 's/^\t\t/BOTH   /; s/^\t/OURS   /; s/^/REF    /; s/^REF    \(BOTH\|OURS\)/\1/'
    echo
    for t in $(ls "$workdir/ref"); do
        if [ -f "$workdir/ours/$t" ]; then
            echo "=== diff $t (ref | ours) ==="
            diff "$workdir/ref/$t" "$workdir/ours/$t" || true
            echo
        fi
    done
    echo "exports left in $workdir/{ref,ours} for inspection"
