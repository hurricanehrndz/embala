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

# Cross-compile the fixture test exes: Windows via mingw (0.2.0 feeds the
# upgrade test), macOS via zig (rcodesign needs a real Mach-O in the .app).
fixtures:
    mkdir -p fixtures/hello/dist
    x86_64-w64-mingw32-cc -DVERSION='"0.1.0"' -o fixtures/hello/dist/hello.exe fixtures/hello/hello.c
    x86_64-w64-mingw32-cc -DVERSION='"0.2.0"' -o fixtures/hello/dist/hello-0.2.0.exe fixtures/hello/hello.c
    zig cc -target aarch64-macos -DVERSION='"0.1.0"' -o fixtures/hello/dist/hello-mac fixtures/hello/hello.c

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

# Mint a 10-year self-signed code-signing cert for local osslsigncode tests.
# Outputs fixtures/certs/test-win.pfx (+ .pem public half). PFX password is the
# fixed literal `embala-test`. Generated, never committed (.gitignore).
test-cert-win:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p fixtures/certs
    # CA:FALSE is required: OpenSSL 3.x `req -x509` defaults to CA:TRUE and
    # Windows Authenticode rejects a CA cert as leaf signer.
    openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
        -keyout fixtures/certs/test-win.key -out fixtures/certs/test-win.pem \
        -subj "/CN=Embala Test" \
        -addext "basicConstraints=critical,CA:FALSE" \
        -addext "keyUsage=digitalSignature" \
        -addext "extendedKeyUsage=codeSigning"
    openssl pkcs12 -export -passout pass:embala-test \
        -inkey fixtures/certs/test-win.key -in fixtures/certs/test-win.pem \
        -out fixtures/certs/test-win.pfx
    rm -f fixtures/certs/test-win.key

# Mint a self-signed Apple code-signing cert for local rcodesign tests.
# rcodesign emits PEM (.crt/.key), so openssl bundles them into
# fixtures/certs/test-mac.p12 (rcodesign has no direct p12 output).
# Password is the fixed literal `embala-test` (rcodesign rejects empty ones);
# `-legacy` forces PBE-SHA1-3DES — rcodesign's PKCS12 reader can't decrypt
# OpenSSL 3.x's default PBES2/AES encryption.
test-cert-mac:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p fixtures/certs
    rcodesign generate-self-signed-certificate \
        --person-name "Embala Test" --validity-days 3650 \
        --pem-filename fixtures/certs/test-mac
    openssl pkcs12 -export -legacy -passout pass:embala-test \
        -inkey fixtures/certs/test-mac.key -in fixtures/certs/test-mac.crt \
        -out fixtures/certs/test-mac.p12
    rm -f fixtures/certs/test-mac.key fixtures/certs/test-mac.crt

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
