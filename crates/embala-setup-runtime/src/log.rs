//! Install log: records + reversal plan (spec R12) — pure logic, no Win32.
//!
//! Every *mutating* `embala.*` call appends one [`Record`] to an in-memory log
//! that is flushed to `<install_dir>\install.log`. A default log-driven
//! uninstaller reverses the install by replaying the log **LIFO** ([R14]): the
//! last thing created is the first thing removed, so nested `mkdir`s unwind
//! correctly (child dir removed before its now-empty parent).
//!
//! The on-disk format is **JSON lines** — one `serde_json` object per record,
//! `\n`-separated. JSON escaping makes it round-trip *exactly*, including paths
//! with spaces, tabs, or unicode (a tab-separated format could not). This module
//! is deliberately free of any Win32 call so it unit-tests on the host; the
//! thin Win32 execution layer ([`crate::api`]) interprets the [`Reversal`]s.
//!
//! [R14]: uninstaller reversal semantics
//! [`crate::api`]: the Win32 execution layer

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Registry hive an operation targets. Also used by the `registry.*`/`arp.*` API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Hive {
    Hkcu,
    Hklm,
}

/// `env.set` scope. Selects the registry location + broadcast target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnvScope {
    User,
    Machine,
}

/// One logged mutating operation. The `op` tag names the record kind on disk.
///
/// Crash-consistency ordering (documented once, applied by every API call): the
/// record is appended **before** the Win32 mutation is attempted and the log is
/// best-effort flushed, so an interrupted install can always be reversed for
/// work that *may* have happened — reversal of a not-actually-created resource
/// is a harmless no-op (delete-if-exists / rmdir-if-empty). The alternative
/// (log after success) would orphan a resource created just before a crash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum Record {
    /// A file was written at `path` (absolute). Reverse: delete it.
    File { path: PathBuf },
    /// A directory was created at `path`. Reverse: remove it if empty.
    Mkdir { path: PathBuf },
    /// A `.lnk` shortcut was created at `path`. Reverse: delete it.
    Shortcut { path: PathBuf },
    /// A registry value was set. `key_created` records whether *this* call
    /// created the key (so reversal can delete the whole key, not just the
    /// value). Kept simple: only a key created by this install is removed.
    Registry {
        hive: Hive,
        key: String,
        /// `None` = the key's default value.
        name: Option<String>,
        key_created: bool,
    },
    /// An ARP `Uninstall\<identifier>` entry was written. Reverse: delete the key.
    Arp { hive: Hive, identifier: String },
    /// An environment variable was set. Reverse: delete it + re-broadcast.
    Env { name: String, scope: EnvScope },
}

/// A single undo action, derived from a [`Record`]. The Win32 layer executes
/// these; keeping the type pure lets the LIFO ordering be unit-tested on the
/// host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reversal {
    DeleteFile(PathBuf),
    RemoveDirIfEmpty(PathBuf),
    DeleteShortcut(PathBuf),
    DeleteRegistryValue {
        hive: Hive,
        key: String,
        name: Option<String>,
        /// Delete the whole key after the value (the install created it).
        delete_key: bool,
    },
    DeleteArp {
        hive: Hive,
        identifier: String,
    },
    DeleteEnv {
        name: String,
        scope: EnvScope,
    },
}

/// Encode records as JSON lines (trailing `\n` after each).
pub fn encode(records: &[Record]) -> String {
    let mut out = String::new();
    for r in records {
        // serde_json on an enum never fails for these owned fields.
        out.push_str(&serde_json::to_string(r).expect("record serializes"));
        out.push('\n');
    }
    out
}

/// Decode JSON lines back into records. Blank lines are skipped; a malformed
/// line is surfaced (a corrupt log must fail loud, not silently under-reverse).
pub fn decode(text: &str) -> Result<Vec<Record>, serde_json::Error> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect()
}

/// Map an install log to its undo actions in **LIFO** order (spec R14).
pub fn reversal_plan(records: &[Record]) -> Vec<Reversal> {
    records.iter().rev().map(reverse_one).collect()
}

fn reverse_one(record: &Record) -> Reversal {
    match record {
        Record::File { path } => Reversal::DeleteFile(path.clone()),
        Record::Mkdir { path } => Reversal::RemoveDirIfEmpty(path.clone()),
        Record::Shortcut { path } => Reversal::DeleteShortcut(path.clone()),
        Record::Registry {
            hive,
            key,
            name,
            key_created,
        } => Reversal::DeleteRegistryValue {
            hive: *hive,
            key: key.clone(),
            name: name.clone(),
            delete_key: *key_created,
        },
        Record::Arp { hive, identifier } => Reversal::DeleteArp {
            hive: *hive,
            identifier: identifier.clone(),
        },
        Record::Env { name, scope } => Reversal::DeleteEnv {
            name: name.clone(),
            scope: *scope,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Record> {
        vec![
            Record::Mkdir {
                path: PathBuf::from("/opt/My App"),
            },
            Record::File {
                path: PathBuf::from("/opt/My App/héllo wörld.exe"),
            },
            Record::Shortcut {
                path: PathBuf::from("/menu/My App.lnk"),
            },
            Record::Registry {
                hive: Hive::Hkcu,
                key: r"Software\My App".to_string(),
                name: Some("Path".to_string()),
                key_created: true,
            },
            Record::Arp {
                hive: Hive::Hkcu,
                identifier: "com.example.app".to_string(),
            },
            Record::Env {
                name: "MYAPP_HOME".to_string(),
                scope: EnvScope::User,
            },
        ]
    }

    #[test]
    fn records_round_trip_including_spaces_and_unicode() {
        // Why: the uninstaller reads this log back to know what to delete; a path
        // with a space or non-ASCII char that did not survive encode→decode
        // would leave that exact file/registry value orphaned on uninstall.
        let records = sample();
        let decoded = decode(&encode(&records)).expect("valid log");
        assert_eq!(decoded, records);
    }

    #[test]
    fn each_line_is_one_json_record() {
        let text = encode(&sample());
        assert_eq!(text.lines().count(), sample().len());
        // Blank lines and trailing newline tolerated on the way back in.
        assert_eq!(decode(&format!("\n{text}\n")).unwrap(), sample());
    }

    #[test]
    fn reversal_is_lifo_so_nested_dirs_unwind() {
        // Why: the parent dir is created before the child; only child-before-parent
        // (LIFO) removal leaves each dir empty at the moment it is removed. A FIFO
        // reversal would try to rmdir a non-empty parent and leak the tree.
        let records = vec![
            Record::Mkdir {
                path: PathBuf::from("/opt/app"),
            },
            Record::Mkdir {
                path: PathBuf::from("/opt/app/data"),
            },
            Record::File {
                path: PathBuf::from("/opt/app/data/f.txt"),
            },
        ];
        let plan = reversal_plan(&records);
        assert_eq!(
            plan,
            vec![
                Reversal::DeleteFile(PathBuf::from("/opt/app/data/f.txt")),
                Reversal::RemoveDirIfEmpty(PathBuf::from("/opt/app/data")),
                Reversal::RemoveDirIfEmpty(PathBuf::from("/opt/app")),
            ]
        );
    }

    #[test]
    fn registry_key_created_flag_drives_key_deletion() {
        // Why: deleting a value we set is always safe; deleting the *key* is only
        // safe when this install created it (else we'd remove a pre-existing key
        // with unrelated values). The flag must flow into the reversal.
        let plan = reversal_plan(&[Record::Registry {
            hive: Hive::Hklm,
            key: r"Software\X".to_string(),
            name: None,
            key_created: false,
        }]);
        assert_eq!(
            plan,
            vec![Reversal::DeleteRegistryValue {
                hive: Hive::Hklm,
                key: r"Software\X".to_string(),
                name: None,
                delete_key: false,
            }]
        );
    }
}
