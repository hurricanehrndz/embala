//! Spike B — BOM correctness against mkbom.
//!
//! Walks a file tree and writes a BOM for it via apple-bom's `BomBuilder`.
//! Validate on macOS against Apple tooling for the same tree:
//!
//! ```sh
//! mkbom /tmp/pkgroot /tmp/ref.bom
//! lsbom -p fmugsc /tmp/ref.bom > a; lsbom -p fmugsc spike.bom > b; diff a b
//! ```
//!
//! `mkbom` records the tree's actual on-disk ownership, while `pkgbuild`
//! normalizes to root:wheel (0:0) — pass `UID`/`GID` to match whichever
//! oracle you are comparing against.
//!
//! Usage: `cargo run -p embala-pkg --example bom_spike <TREE> [OUTPUT] [UID] [GID]`

//!
//! Unix-only: `write_bom` reads mode bits off the filesystem, which is what
//! makes it comparable to `mkbom` in the first place.

#[cfg(not(unix))]
fn main() {
    eprintln!("bom_spike is a mkbom/lsbom oracle — Unix hosts only");
}

#[cfg(unix)]
use embala_pkg::write_bom;

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let tree = args
        .next()
        .ok_or("usage: bom_spike <TREE> [OUTPUT] [UID] [GID]")?;
    let output = args.next().unwrap_or_else(|| "spike.bom".to_string());
    let uid = args.next().map(|v| v.parse()).transpose()?.unwrap_or(0);
    let gid = args.next().map(|v| v.parse()).transpose()?.unwrap_or(0);

    let bom = write_bom(std::path::Path::new(&tree), uid, gid)?;
    std::fs::write(&output, bom)?;

    println!("wrote {output} (owner {uid}:{gid})");
    Ok(())
}
