//! Spike A — xar assembly from apple-xar primitives.
//!
//! Builds an archive holding two small files with different content and
//! modes. Validate on macOS:
//!
//! ```sh
//! xar -tf spike.xar          # lists both files
//! xar -xf spike.xar -C out   # extracted bytes + modes match
//! ```
//!
//! Usage: `cargo run -p embala-pkg --example xar_spike [OUTPUT]`

use embala_pkg::{XarEntry, write_xar};

const HELLO: &[u8] = b"#!/bin/sh\necho \"hello from embala xar spike\"\n";
const README: &[u8] = b"Hello fixture README for embala pkg spike.\n";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "spike.xar".to_string());

    let entries = [
        XarEntry {
            name: "embala-hello",
            data: HELLO,
            mode: 0o755,
        },
        XarEntry {
            name: "README",
            data: README,
            mode: 0o644,
        },
    ];

    let file = std::fs::File::create(&output)?;
    write_xar(&entries, std::io::BufWriter::new(file))?;

    println!("wrote {output}");
    Ok(())
}
