//! Summary information stream. Every value here is a msiexec acceptance
//! gotcha: wrong codepage, Template, or Word Count makes Windows Installer
//! silently reject the package.
//
// Portions ported from deno desktop (cli/tools/desktop.rs),
// Copyright 2018-2026 the Deno authors, MIT license.

use std::io::{Read, Seek, Write};

use crate::{MsiSpec, guids};

/// Fixed creation time (2020-01-01T00:00:00Z) for reproducible output.
pub(crate) const FIXED_TIMESTAMP_SECS: u64 = 1_577_836_800;

pub(crate) fn write<F: Read + Write + Seek>(package: &mut msi::Package<F>, spec: &MsiSpec) {
    let summary = package.summary_info_mut();
    // Summary stream must be an ANSI codepage too, not the msi crate's
    // UTF-8 default (see the database codepage note in lib.rs).
    summary.set_codepage(msi::CodePage::Windows1252);
    summary.set_title("Installation Database");
    summary.set_subject(spec.display_name.clone());
    summary.set_author(spec.publisher.clone());
    summary.set_comments(spec.description.clone());
    // Template = "<arch>;<language>", e.g. "x64;1033".
    summary.set_arch(spec.arch.template());
    summary.set_languages(&[msi::Language::from_code(1033)]);
    summary.set_creating_application("embala");
    // Revision = PackageCode: identifies this exact package build.
    summary.set_uuid(guids::package_uuid(&spec.identifier, &spec.version));
    // Word Count bit 1 (2) = source files are compressed (in cabinets);
    // bit 0 clear = long file names allowed.
    summary.set_word_count(2);
    // Page Count = minimum Windows Installer version (2.00).
    summary.set_page_count(200);
    summary.set_creation_time(
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(FIXED_TIMESTAMP_SECS),
    );
}
