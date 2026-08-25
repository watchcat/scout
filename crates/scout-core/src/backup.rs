//! When a backup is taken, what it is called, and how many are kept.
//!
//! The copying itself belongs to `store`, which owns the connection. This is
//! the policy around it.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Keep two weeks. About 220 MB at this database's size against 84 GB free,
/// and long enough to notice damage that is not immediately obvious — a bad
/// migration often only surfaces when someone runs `/stat`.
pub const KEEP: usize = 14;

/// How stale the newest backup may be before another is taken.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

const PREFIX: &str = "scout-";
const SUFFIX: &str = ".duckdb";

/// Why a backup was taken. Part of the filename, because the question at
/// restore time is never which timestamp alone — it is which one, and what was
/// about to happen to the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Nightly,
    /// Before a schema change, which is the one that cannot be undone.
    Migration { to: i64 },
    Manual,
}

impl Reason {
    fn slug(&self) -> String {
        match self {
            Reason::Nightly => "nightly".to_string(),
            Reason::Migration { to } => format!("migration-v{to}"),
            Reason::Manual => "manual".to_string(),
        }
    }
}

pub(crate) fn file_name(at: &chrono::DateTime<chrono::Utc>, reason: Reason) -> String {
    format!("{PREFIX}{}-{}{SUFFIX}", at.format("%Y-%m-%dT%H%M%SZ"), reason.slug())
}

/// The name a backup taken right now would have.
pub fn file_name_now(reason: Reason) -> String {
    file_name(&chrono::Utc::now(), reason)
}

fn is_ours(name: &str) -> bool {
    name.starts_with(PREFIX) && name.ends_with(SUFFIX)
}

/// Ours, oldest first. The names carry ISO timestamps, so lexical order is
/// chronological order and nothing has to be parsed.
fn existing(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut found: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_str().is_some_and(is_ours))
            .map(|e| e.path())
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    found.sort();
    Ok(found)
}

/// Deletes all but the newest `keep`.
///
/// Only ever considers files this module named. The live database sits in the
/// directory above, but a mistake here must not be able to reach anything that
/// is not a backup — which is why the filter is a prefix and suffix rather
/// than a glob for `*.duckdb`.
pub fn prune(dir: &Path, keep: usize) -> anyhow::Result<()> {
    let all = existing(dir)?;
    for old in all.iter().rev().skip(keep) {
        if let Err(e) = std::fs::remove_file(old) {
            tracing::warn!(path = %old.display(), error = %e, "could not remove an old backup");
        }
    }
    Ok(())
}

/// Whether the newest backup is old enough that another is wanted.
///
/// Reads modification time rather than parsing the name, so there is no clock
/// to inject and no state to keep across a restart.
pub fn is_due(dir: &Path) -> anyhow::Result<bool> {
    let Some(newest) = existing(dir)?.pop() else {
        return Ok(true);
    };
    let modified = std::fs::metadata(&newest)?.modified()?;
    Ok(SystemTime::now()
        .duration_since(modified)
        .map(|age| age >= MAX_AGE)
        // A file dated in the future means a clock moved backwards. Taking
        // another backup is the harmless answer; refusing to would leave the
        // database unprotected until the clock caught up.
        .unwrap_or(true))
}

/// Where backups live: beside the database, on the same volume.
///
/// That covers corruption, a bad migration and a mistaken delete. It does not
/// cover losing the disk — which is what shipping them off the box is for, and
/// is deliberately not here.
pub fn dir_for(db_path: &Path) -> PathBuf {
    db_path.parent().unwrap_or(Path::new(".")).join("backups")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn a_backup_says_when_it_was_taken_and_why() {
        let at = chrono::Utc.with_ymd_and_hms(2026, 8, 25, 2, 15, 0).unwrap();
        assert_eq!(file_name(&at, Reason::Nightly), "scout-2026-08-25T021500Z-nightly.duckdb");
        assert_eq!(
            file_name(&at, Reason::Migration { to: 6 }),
            "scout-2026-08-25T021500Z-migration-v6.duckdb"
        );
    }

    #[test]
    fn pruning_keeps_the_newest_and_never_touches_anything_else() {
        let dir = tempfile::tempdir().unwrap();
        for d in 1..=5 {
            std::fs::write(dir.path().join(format!("scout-2026-08-0{d}T000000Z-nightly.duckdb")), b"x").unwrap();
        }
        // Not ours. A mistake in the filter must not be able to eat these.
        std::fs::write(dir.path().join("scout.duckdb"), b"live").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"keep me").unwrap();

        prune(dir.path(), 2).unwrap();

        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "notes.txt".to_string(),
                "scout-2026-08-04T000000Z-nightly.duckdb".to_string(),
                "scout-2026-08-05T000000Z-nightly.duckdb".to_string(),
                "scout.duckdb".to_string(),
            ]
        );
    }

    #[test]
    fn a_backup_is_due_when_there_is_none_and_not_when_one_was_just_taken() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_due(dir.path()).unwrap(), "nothing yet means one is due");
        assert!(is_due(&dir.path().join("nonexistent")).unwrap(), "no directory at all means one is due");

        std::fs::write(dir.path().join("scout-2026-08-05T000000Z-nightly.duckdb"), b"x").unwrap();
        assert!(!is_due(dir.path()).unwrap(), "one taken just now is not due again");
    }
}
