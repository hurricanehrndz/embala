//! Rollback journal for one install run — pure std, no Win32 (host-testable).
//!
//! The [`Journal`] holds every step of the run in call order: each logged
//! [`Record`], with what it replaced (a [`Prior`]) when it overwrote something,
//! and each `embala.exec{ undo = ... }` command. Only the records are persisted
//! to `install.log` ([`Journal::records`]): priors and undo commands are
//! rollback-only state, so the uninstaller never restores old files or runs an
//! undo, and the log format is unchanged.
//!
//! Rollback walks the steps LIFO: a record with a prior is restored (the earlier
//! file contents or registry values), one without is reversed as the
//! uninstaller would (delete-if-exists), and an undo command runs. So a failed
//! upgrade leaves what was there before the run.
//!
//! Crash-ordering: a file is backed up *before* its step is pushed and the step
//! is pushed before the overwrite, so a rollback after a half-finished copy still
//! restores. An OS kill skips rollback: backups stay in the backup dir under
//! `%TEMP%` and nothing is restored.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::log::{self, Record, Reversal};

/// What a mutation replaced. Kept in memory only, never written to `install.log`.
#[derive(Debug, Clone)]
pub enum Prior {
    /// A file existed at the record's path; `backup` holds its earlier contents.
    File { backup: PathBuf },
    /// Registry values that existed before, by name (`""` = the default value).
    /// For an ARP record, every value of the key; else the one value set.
    #[cfg(windows)]
    Values(Vec<(String, winsafe::RegistryValue)>),
}

/// A command `embala.exec{ undo = ... }` asked to run if this run rolls back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoCmd {
    pub cmd: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone)]
enum Step {
    Record {
        record: Record,
        prior: Option<Prior>,
    },
    Undo(UndoCmd),
}

/// One rollback action, in the order [`Journal::rollback_plan`] yields them.
#[derive(Debug, Clone)]
pub enum Action {
    /// Nothing was there before: undo as the uninstaller would.
    Reverse(Reversal),
    /// Something was there before: put it back.
    Restore { record: Record, prior: Prior },
    /// Run an `undo` command.
    Run(UndoCmd),
}

pub struct Journal {
    steps: Vec<Step>,
    /// Holds file backups; created on the first backup, removed by [`Journal::discard`].
    backup_dir: PathBuf,
    backups: usize,
}

impl Journal {
    pub fn new(backup_dir: PathBuf) -> Journal {
        Journal {
            steps: Vec::new(),
            backup_dir,
            backups: 0,
        }
    }

    pub fn push(&mut self, record: Record, prior: Option<Prior>) {
        self.steps.push(Step::Record { record, prior });
    }

    pub fn push_undo(&mut self, undo: UndoCmd) {
        self.steps.push(Step::Undo(undo));
    }

    /// The records, in call order: what `install.log` persists and the
    /// uninstaller reverses. No priors, no undo commands.
    pub fn records(&self) -> Vec<Record> {
        self.steps
            .iter()
            .filter_map(|s| match s {
                Step::Record { record, .. } => Some(record.clone()),
                Step::Undo(_) => None,
            })
            .collect()
    }

    /// Copy the file at `path`, if there is one, into the backup dir before it
    /// is overwritten. `Ok(None)` when nothing is there. A failed backup is an
    /// error: the caller must not overwrite what it could not save.
    pub fn back_up_file(&mut self, path: &Path) -> io::Result<Option<Prior>> {
        if !path.is_file() {
            return Ok(None);
        }
        std::fs::create_dir_all(&self.backup_dir)?;
        let backup = self.backup_dir.join(self.backups.to_string());
        self.backups += 1;
        std::fs::copy(path, &backup)?;
        Ok(Some(Prior::File { backup }))
    }

    /// The rollback actions, LIFO.
    pub fn rollback_plan(&self) -> Vec<Action> {
        self.steps
            .iter()
            .rev()
            .map(|s| match s {
                Step::Record {
                    record,
                    prior: Some(prior),
                } => Action::Restore {
                    record: record.clone(),
                    prior: prior.clone(),
                },
                Step::Record {
                    record,
                    prior: None,
                } => Action::Reverse(log::reverse_one(record)),
                Step::Undo(undo) => Action::Run(undo.clone()),
            })
            .collect()
    }

    /// Undo the run, LIFO and best-effort: a failed action is ignored so it
    /// cannot strand the rest. File restores and undo commands run here;
    /// `system` performs the rest (reversals, registry restores). Discards the
    /// backups afterwards.
    pub fn roll_back(&self, mut system: impl FnMut(&Action) -> io::Result<()>) {
        for action in self.rollback_plan() {
            let _ = match &action {
                Action::Restore {
                    record: Record::File { path } | Record::Shortcut { path },
                    prior: Prior::File { backup },
                } => std::fs::copy(backup, path).map(drop),
                Action::Run(undo) => run_undo(undo),
                other => system(other),
            };
        }
        self.discard();
    }

    /// Remove the backups (after a successful run, or once restored).
    pub fn discard(&self) {
        let _ = std::fs::remove_dir_all(&self.backup_dir);
    }
}

/// Run an undo command to completion. Its exit code is not checked: like any
/// rollback action, a failing undo must not stop the rest.
fn run_undo(undo: &UndoCmd) -> io::Result<()> {
    let mut command = Command::new(&undo.cmd);
    command.args(&undo.args);
    if let Some(cwd) = &undo.cwd {
        command.current_dir(cwd);
    }
    command.output().map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The system side of a rollback on the host: file deletes only.
    fn host_system(action: &Action) -> io::Result<()> {
        match action {
            Action::Reverse(Reversal::DeleteFile(p)) => std::fs::remove_file(p),
            _ => Ok(()),
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "embala-rollback-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Back up then overwrite, as `fs.copy` does.
    fn write_over(j: &mut Journal, path: &Path, contents: &str) {
        let prior = j.back_up_file(path).unwrap();
        j.push(
            Record::File {
                path: path.to_path_buf(),
            },
            prior,
        );
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn rollback_restores_a_file_that_existed_and_deletes_a_new_one() {
        // Why: a failed upgrade used to delete the files it overwrote, leaving
        // the previous install broken. Only files this run created may go.
        let dir = tmp("restore");
        let old = dir.join("app.exe");
        let new = dir.join("new.dll");
        std::fs::write(&old, "v1").unwrap();
        let mut j = Journal::new(dir.join("backup"));

        write_over(&mut j, &old, "v2");
        write_over(&mut j, &new, "v2");
        j.roll_back(host_system);

        assert_eq!(std::fs::read_to_string(&old).unwrap(), "v1");
        assert!(!new.exists());
        assert!(!dir.join("backup").exists(), "backups discarded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_overwritten_twice_unwinds_to_the_original() {
        let dir = tmp("twice");
        let f = dir.join("a.txt");
        std::fs::write(&f, "v1").unwrap();
        let mut j = Journal::new(dir.join("backup"));
        write_over(&mut j, &f, "v2");
        write_over(&mut j, &f, "v3");
        j.roll_back(host_system);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_runs_after_the_later_file_restores() {
        // Why: the undo of `exec{-serviceremove}` re-installs the service from
        // the exe the run then overwrote, so it must run after that exe is
        // restored (LIFO), and see the old one.
        let dir = tmp("undo");
        let exe = dir.join("app.exe");
        let seen = dir.join("seen");
        std::fs::write(&exe, "old").unwrap();
        let mut j = Journal::new(dir.join("backup"));

        j.push_undo(UndoCmd {
            cmd: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                format!("cat '{}' > '{}'", exe.display(), seen.display()),
            ],
            cwd: None,
        });
        write_over(&mut j, &exe, "new");
        j.roll_back(host_system);

        assert_eq!(std::fs::read_to_string(&seen).unwrap(), "old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_undo_does_not_stop_the_rest() {
        let dir = tmp("failing-undo");
        let f = dir.join("a.txt");
        std::fs::write(&f, "v1").unwrap();
        let mut j = Journal::new(dir.join("backup"));
        write_over(&mut j, &f, "v2");
        j.push_undo(UndoCmd {
            cmd: dir.join("no-such-command").display().to_string(),
            args: vec![],
            cwd: None,
        });
        j.roll_back(host_system);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_and_priors_stay_out_of_the_uninstallers_log() {
        // Why: the uninstaller replays install.log. An undo is rollback-only, and
        // after a successful upgrade the uninstaller must delete the new files
        // once, as for a fresh install, not restore the old ones.
        let dir = tmp("persisted");
        let f = dir.join("app.exe");
        std::fs::write(&f, "v1").unwrap();
        let mut j = Journal::new(dir.join("backup"));
        j.push_undo(UndoCmd {
            cmd: "app.exe".to_string(),
            args: vec!["-serviceinstall".to_string()],
            cwd: None,
        });
        write_over(&mut j, &f, "v2");

        let text = log::encode(&j.records());
        assert!(!text.contains("serviceinstall"));
        assert_eq!(
            log::reversal_plan(&log::decode(&text).unwrap()),
            vec![Reversal::DeleteFile(f.clone())]
        );
        j.discard();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
