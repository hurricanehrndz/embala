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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

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

/// `install.log.partial` beside `log_path`: where a run flushes its records
/// after each one. `install.log` itself is written only once the run succeeds
/// ([`crate::rollback::conclude`]), so a failed run leaves an existing
/// install's log, which its uninstaller replays, byte for byte as it was.
pub fn partial_path(log_path: &Path) -> PathBuf {
    log_path.with_extension("log.partial")
}

/// Best-effort flush of the records so far to the partial log.
pub fn flush_partial(log_path: &Path, records: &[Record]) -> std::io::Result<()> {
    std::fs::write(partial_path(log_path), encode(records))
}

/// The records a successful run commits to `install.log`. `previous` is the
/// install's existing log text (`None` when there is none): its records are
/// carried forward ahead of this run's ([`merge`]), so the uninstaller removes
/// what any version installed. A fresh install commits `current` unchanged.
/// A previous log that does not decode is dropped with a warning (returned),
/// not an error: the install itself succeeded.
pub fn committed_records(
    previous: Option<&str>,
    current: Vec<Record>,
) -> (Vec<Record>, Option<String>) {
    match previous.map(decode) {
        None => (current, None),
        Some(Ok(previous)) => (merge(&previous, &current), None),
        Some(Err(e)) => (
            current,
            Some(format!(
                "previous install.log unreadable ({e}); it lists only this install now"
            )),
        ),
    }
}

/// The previous log's records followed by this run's, one record per thing
/// owned. The dedupe key is case-insensitive (Windows paths and registry names
/// are): a path for `File`, `Mkdir` and `Shortcut` (each kind separately);
/// hive + key + value name for `Registry`; hive + identifier for `Arp`; name +
/// scope for `Env`.
///
/// Position decides LIFO reversal order. A `Mkdir` stays at its earliest
/// position, so it is reversed after every file put under it by any version.
/// Every other record moves to its latest position: removing a file or value
/// early never hurts. Ownership is sticky: if any occurrence of a registry
/// value says this product created the key, the merged record does too.
/// Merging a run's own records again changes nothing, so the log stays bounded
/// across repeated upgrades and reinstalls.
pub fn merge(previous: &[Record], current: &[Record]) -> Vec<Record> {
    let all: Vec<&Record> = previous.iter().chain(current).collect();
    let keys: Vec<String> = all.iter().map(|r| dedupe_key(r)).collect();
    let created: BTreeSet<&str> = all
        .iter()
        .zip(&keys)
        .filter(|(r, _)| {
            matches!(
                r,
                Record::Registry {
                    key_created: true,
                    ..
                }
            )
        })
        .map(|(_, k)| k.as_str())
        .collect();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (i, (record, key)) in all.iter().zip(&keys).enumerate() {
        let keep = match record {
            Record::Mkdir { .. } => seen.insert(key.as_str()),
            _ => !keys[i + 1..].contains(key),
        };
        if !keep {
            continue;
        }
        let mut record = (*record).clone();
        if let Record::Registry { key_created, .. } = &mut record {
            *key_created = created.contains(key.as_str());
        }
        out.push(record);
    }
    out
}

fn dedupe_key(record: &Record) -> String {
    let fold = |s: &str| s.to_lowercase();
    let path = |p: &Path| fold(&p.to_string_lossy());
    match record {
        Record::File { path: p } => format!("file\0{}", path(p)),
        Record::Mkdir { path: p } => format!("mkdir\0{}", path(p)),
        Record::Shortcut { path: p } => format!("shortcut\0{}", path(p)),
        Record::Registry {
            hive, key, name, ..
        } => format!(
            "registry\0{hive:?}\0{}\0{}",
            fold(key),
            name.as_deref().map(fold).unwrap_or_default()
        ),
        Record::Arp { hive, identifier } => format!("arp\0{hive:?}\0{}", fold(identifier)),
        Record::Env { name, scope } => format!("env\0{scope:?}\0{}", fold(name)),
    }
}

/// Map an install log to its undo actions in **LIFO** order (spec R14).
pub fn reversal_plan(records: &[Record]) -> Vec<Reversal> {
    records.iter().rev().map(reverse_one).collect()
}

/// The undo action for one record. Also used by rollback for a record that
/// replaced nothing ([`crate::rollback`]).
pub fn reverse_one(record: &Record) -> Reversal {
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

    fn file(p: &str) -> Record {
        Record::File {
            path: PathBuf::from(p),
        }
    }
    fn mkdir(p: &str) -> Record {
        Record::Mkdir {
            path: PathBuf::from(p),
        }
    }
    fn reg(name: &str, key_created: bool) -> Record {
        Record::Registry {
            hive: Hive::Hklm,
            key: r"Software\App".to_string(),
            name: Some(name.to_string()),
            key_created,
        }
    }

    /// v1: created the data subfolder, shipped old.dll, created the key.
    fn v1() -> Vec<Record> {
        vec![
            mkdir("/app/data"),
            file("/app/data/a.txt"),
            file("/app/app.exe"),
            file("/app/old.dll"),
            reg("Version", true),
        ]
    }
    /// v2 over v1: the folder and key exist, so no Mkdir and key_created=false;
    /// adds b.txt under the folder; no longer ships old.dll.
    fn v2() -> Vec<Record> {
        vec![
            file("/app/data/a.txt"),
            file("/app/data/b.txt"),
            file("/app/app.exe"),
            reg("Version", false),
        ]
    }

    #[test]
    fn an_upgrade_carries_what_only_the_previous_version_owned() {
        // Why: v2's log alone would leave v1's folder, old.dll and key behind
        // at uninstall. The folder must be reversed after every file under it,
        // v2's b.txt included, and the key still deleted as v1 created it.
        let merged = merge(&v1(), &v2());
        assert_eq!(
            merged,
            vec![
                mkdir("/app/data"),
                file("/app/old.dll"),
                file("/app/data/a.txt"),
                file("/app/data/b.txt"),
                file("/app/app.exe"),
                reg("Version", true),
            ]
        );
        assert_eq!(
            reversal_plan(&merged).last(),
            Some(&Reversal::RemoveDirIfEmpty(PathBuf::from("/app/data")))
        );
    }

    #[test]
    fn dedupe_ignores_case_and_keeps_key_ownership_from_any_version() {
        let v1 = vec![reg("Version", true), file(r"C:\App\App.exe")];
        let v2 = vec![
            Record::Registry {
                hive: Hive::Hklm,
                key: r"SOFTWARE\APP".to_string(),
                name: Some("version".to_string()),
                key_created: false,
            },
            file(r"c:\app\app.exe"),
        ];
        let merged = merge(&v1, &v2);
        assert_eq!(merged.len(), 2);
        assert!(matches!(
            merged[0],
            Record::Registry {
                key_created: true,
                ..
            }
        ));
        // Ownership sticks whichever version created the key.
        assert!(matches!(
            merge(&v2, &v1)[0],
            Record::Registry {
                key_created: true,
                ..
            }
        ));
    }

    #[test]
    fn repeated_upgrades_and_reinstalls_converge() {
        // Why: without dedupe every reinstall would append its records and the
        // log would grow without bound.
        let once = merge(&v1(), &v2());
        let twice = merge(&once, &v2());
        assert_eq!(twice, once);
        assert_eq!(merge(&twice, &v2()), once);
        assert_eq!(merge(&v1(), &v1()), merge(&[], &v1()));
    }

    #[test]
    fn a_fresh_install_commits_its_records_unchanged() {
        // Even its own duplicates: byte for byte what earlier versions wrote.
        let current = vec![file("/app/a"), file("/app/a")];
        assert_eq!(committed_records(None, current.clone()), (current, None));
    }

    #[test]
    fn a_corrupt_previous_log_is_dropped_with_a_warning() {
        let (records, warning) = committed_records(Some("{not json\n"), v2());
        assert_eq!(records, v2());
        assert!(warning.unwrap().contains("unreadable"));
    }

    #[test]
    fn a_0_12_0_previous_log_is_carried() {
        let previous = concat!(
            r#"{"op":"mkdir","path":"/app/data"}"#,
            "\n",
            r#"{"op":"file","path":"/app/old.dll"}"#,
            "\n",
            r#"{"op":"registry","hive":"hklm","key":"Software\\App","name":"Version","key_created":true}"#,
            "\n",
        );
        let (records, warning) = committed_records(Some(previous), v2());
        assert_eq!(warning, None);
        assert_eq!(records[0], mkdir("/app/data"));
        assert!(records.contains(&file("/app/old.dll")));
        assert!(records.contains(&reg("Version", true)));
    }

    #[test]
    fn a_log_written_by_0_12_0_still_decodes() {
        // Why: the uninstaller on disk may be newer than the log beside it (an
        // install made by 0.12.0, upgraded later). If the format drifts, the
        // replay is skipped as unreadable and the whole install is left behind.
        let text = concat!(
            r#"{"op":"mkdir","path":"C:\\Program Files\\App"}"#,
            "\n",
            r#"{"op":"file","path":"C:\\Program Files\\App\\app.exe"}"#,
            "\n",
            r#"{"op":"shortcut","path":"C:\\ProgramData\\App.lnk"}"#,
            "\n",
            r#"{"op":"registry","hive":"hklm","key":"Software\\App","name":null,"key_created":true}"#,
            "\n",
            r#"{"op":"arp","hive":"hklm","identifier":"com.example.app"}"#,
            "\n",
            r#"{"op":"env","name":"APP_HOME","scope":"machine"}"#,
            "\n",
        );
        let records = decode(text).expect("0.12.0 log decodes");
        assert_eq!(
            records,
            vec![
                Record::Mkdir {
                    path: PathBuf::from(r"C:\Program Files\App"),
                },
                Record::File {
                    path: PathBuf::from(r"C:\Program Files\App\app.exe"),
                },
                Record::Shortcut {
                    path: PathBuf::from(r"C:\ProgramData\App.lnk"),
                },
                Record::Registry {
                    hive: Hive::Hklm,
                    key: r"Software\App".to_string(),
                    name: None,
                    key_created: true,
                },
                Record::Arp {
                    hive: Hive::Hklm,
                    identifier: "com.example.app".to_string(),
                },
                Record::Env {
                    name: "APP_HOME".to_string(),
                    scope: EnvScope::Machine,
                },
            ]
        );
        // This version writes the same bytes, so an older uninstaller can read
        // a log written by this one.
        assert_eq!(encode(&records), text);
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
