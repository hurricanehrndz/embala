//! The embedded MSZIP cabinet: one folder holding every payload file, named
//! by its MSI `File` key, in `File.Sequence` order.
//
// Portions ported from deno desktop (cli/tools/desktop.rs),
// Copyright 2018-2026 the Deno authors, MIT license.

use std::collections::HashMap;
use std::io::Write as _;

use crate::Result;
use crate::summary::FIXED_TIMESTAMP_SECS;
use crate::tables::StagedFile;

pub(crate) fn build(files: &[StagedFile]) -> Result<Vec<u8>> {
    // The cab crate stamps each member with the current UTC time by default;
    // pin it to the same fixed instant as the summary info so consecutive
    // builds are byte-identical.
    let fixed = time::OffsetDateTime::from_unix_timestamp(FIXED_TIMESTAMP_SECS as i64)
        .expect("fixed timestamp is valid");
    let fixed = time::PrimitiveDateTime::new(fixed.date(), fixed.time());

    let mut builder = cab::CabinetBuilder::new();
    {
        let folder = builder.add_folder(cab::CompressionType::MsZip);
        for f in files {
            folder.add_file(f.key.clone()).set_datetime(fixed);
        }
    }

    let cursor = std::io::Cursor::new(Vec::<u8>::new());
    let mut writer = builder.build(cursor)?;
    // Files come back in the order they were added; look each one up by its
    // cab name (the MSI File key) to stay robust to ordering.
    let by_key: HashMap<&str, &StagedFile> = files.iter().map(|f| (f.key.as_str(), f)).collect();
    while let Some(mut file_writer) = writer.next_file()? {
        let name = file_writer.file_name().to_string();
        let staged = by_key[name.as_str()];
        let data = std::fs::read(&staged.src)?;
        file_writer.write_all(&data)?;
    }
    let cursor = writer.finish()?;
    Ok(cursor.into_inner())
}
