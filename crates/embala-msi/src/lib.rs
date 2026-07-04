//! WiX-less MSI builder.
//!
//! Writes the Windows Installer database (CFB container + relational tables +
//! embedded MSZIP cabinet) directly via the `msi` and `cab` crates, so MSIs
//! build on any OS with no WiX, Wine, or .NET.
//!
//! Scope for v1 (see research paper §8, risk 6): install files to Program
//! Files, Start-menu shortcut, Add/Remove Programs entry, clean uninstall,
//! MajorUpgrade. Nothing else.
