//! Append-only operation journal: records what was deleted and when, so
//! users can audit past runs. One line per path, tab-separated:
//!
//! ```text
//! 2026-07-05T12:00:00Z<TAB>delete<TAB>ok<TAB>C:\projects\node_modules
//! 2026-07-05T12:00:01Z<TAB>recycle<TAB>error: access is denied<TAB>C:\locked.txt
//! ```
//!
//! The journal lives in the per-user data directory
//! (`%LOCALAPPDATA%\zap\journal.log` on Windows) and rotates once to
//! `journal.1.log` when it exceeds [`MAX_JOURNAL_BYTES`]. Journaling is
//! best-effort: failures never abort or slow down a deletion run.

use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Rotate the journal when it grows beyond 5 MiB. One rotation level is
/// kept (`journal.1.log`), bounding total disk use at ~10 MiB.
const MAX_JOURNAL_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalAction {
    Delete,
    Recycle,
    Shred,
}

impl JournalAction {
    pub fn as_str(self) -> &'static str {
        match self {
            JournalAction::Delete => "delete",
            JournalAction::Recycle => "recycle",
            JournalAction::Shred => "shred",
        }
    }
}

/// Per-path outcome of a run: `None` means success, `Some(msg)` a failure.
pub type PathOutcome = (PathBuf, Option<String>);

/// Directory for per-user zap data. Prefers the platform data dir and only
/// falls back to the temp dir when no user profile is available.
fn data_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(base).join("zap");
    }
    if let Some(base) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(base).join("zap");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local").join("share").join("zap");
    }
    std::env::temp_dir().join("zap")
}

/// Where the journal is written. Public so the CLI can print it.
pub fn journal_path() -> PathBuf {
    data_dir().join("journal.log")
}

/// Journaling can be disabled globally with the `ZAP_NO_JOURNAL` env var
/// (any non-empty value) in addition to the per-run `--no-journal` flag.
pub fn is_disabled_by_env() -> bool {
    std::env::var_os("ZAP_NO_JOURNAL").is_some_and(|v| !v.is_empty())
}

/// Record the outcome of a run. Best-effort: errors are returned for tests
/// but callers are expected to ignore them (`let _ = ...`).
pub fn record(action: JournalAction, outcomes: &[PathOutcome]) -> io::Result<()> {
    record_to(&journal_path(), action, outcomes)
}

/// Testable core: append `outcomes` to the journal at `path`, rotating first
/// if the file is over the size cap.
pub fn record_to(path: &Path, action: JournalAction, outcomes: &[PathOutcome]) -> io::Result<()> {
    if outcomes.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    rotate_if_needed(path)?;

    let timestamp = humantime::format_rfc3339_seconds(SystemTime::now()).to_string();
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut w = BufWriter::new(file);
    for (path, error) in outcomes {
        let outcome = match error {
            None => "ok".to_owned(),
            Some(msg) => format!("error: {}", sanitize_field(msg)),
        };
        writeln!(
            w,
            "{timestamp}\t{}\t{}\t{}",
            action.as_str(),
            outcome,
            sanitize_field(&path.display().to_string()),
        )?;
    }
    w.flush()
}

/// Tabs and newlines are field/record separators — replace them so a hostile
/// or unusual path/error message cannot forge extra journal records.
fn sanitize_field(s: &str) -> String {
    if s.contains(['\t', '\n', '\r']) {
        s.replace(['\t', '\n', '\r'], " ")
    } else {
        s.to_owned()
    }
}

/// Delete the journal and its rotated predecessor (`--journal-clear`).
/// Missing files are not an error — clearing an absent journal is a no-op.
pub fn clear() -> io::Result<()> {
    clear_at(&journal_path())
}

/// Testable core of [`clear`].
pub fn clear_at(path: &Path) -> io::Result<()> {
    let mut result = Ok(());
    for target in [path.to_path_buf(), path.with_extension("1.log")] {
        match fs::remove_file(&target) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            // Keep trying the other file, report the first real failure.
            Err(err) => {
                if result.is_ok() {
                    result = Err(err);
                }
            }
        }
    }
    result
}

/// Read the most recent `limit` journal lines (newest last). Falls back to
/// the rotated file when the current journal has fewer lines than requested,
/// so recent history survives a rotation. Missing files yield an empty list.
pub fn read_recent(limit: usize) -> io::Result<Vec<String>> {
    read_recent_from(&journal_path(), limit)
}

/// Testable core of [`read_recent`].
pub fn read_recent_from(path: &Path, limit: usize) -> io::Result<Vec<String>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let read_lines = |p: &Path| -> io::Result<Vec<String>> {
        match fs::read_to_string(p) {
            Ok(content) => Ok(content.lines().map(str::to_owned).collect()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(err) => Err(err),
        }
    };
    let mut lines = read_lines(path)?;
    if lines.len() < limit {
        let rotated = path.with_extension("1.log");
        let mut older = read_lines(&rotated)?;
        let need = limit - lines.len();
        if older.len() > need {
            older.drain(..older.len() - need);
        }
        older.append(&mut lines);
        lines = older;
    }
    if lines.len() > limit {
        lines.drain(..lines.len() - limit);
    }
    Ok(lines)
}

fn rotate_if_needed(path: &Path) -> io::Result<()> {
    let size = match fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if size <= MAX_JOURNAL_BYTES {
        return Ok(());
    }
    let rotated = path.with_extension("1.log");
    // Replace the previous rotation — bounded disk usage beats history depth.
    let _ = fs::remove_file(&rotated);
    fs::rename(path, &rotated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_journal_file() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("zap-journal-test-{pid}-{id}"));
        let _ = fs::remove_dir_all(&dir);
        dir.join("journal.log")
    }

    fn cleanup(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn record_appends_ok_and_error_lines() {
        let journal = test_journal_file();
        let outcomes = vec![
            (PathBuf::from("C:\\good"), None),
            (
                PathBuf::from("C:\\bad"),
                Some("access is denied".to_owned()),
            ),
        ];
        record_to(&journal, JournalAction::Delete, &outcomes).unwrap();
        record_to(&journal, JournalAction::Recycle, &outcomes[..1]).unwrap();

        let content = fs::read_to_string(&journal).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("\tdelete\tok\tC:\\good"));
        assert!(lines[1].contains("\tdelete\terror: access is denied\tC:\\bad"));
        assert!(lines[2].contains("\trecycle\tok\tC:\\good"));
        // Timestamp field parses as RFC 3339.
        let ts = lines[0].split('\t').next().unwrap();
        assert!(humantime::parse_rfc3339(ts).is_ok(), "bad timestamp {ts}");
        cleanup(&journal);
    }

    #[test]
    fn record_sanitizes_tabs_and_newlines() {
        let journal = test_journal_file();
        let outcomes = vec![(
            PathBuf::from("C:\\evil\tname"),
            Some("multi\nline\terror".to_owned()),
        )];
        record_to(&journal, JournalAction::Shred, &outcomes).unwrap();
        let content = fs::read_to_string(&journal).unwrap();
        assert_eq!(content.lines().count(), 1);
        let fields: Vec<&str> = content.lines().next().unwrap().split('\t').collect();
        assert_eq!(fields.len(), 4, "forged separators must be neutralized");
        cleanup(&journal);
    }

    #[test]
    fn empty_outcomes_do_not_create_a_file() {
        let journal = test_journal_file();
        record_to(&journal, JournalAction::Delete, &[]).unwrap();
        assert!(!journal.exists());
        cleanup(&journal);
    }

    #[test]
    fn read_recent_returns_newest_lines_and_spans_rotation() {
        let journal = test_journal_file();
        fs::create_dir_all(journal.parent().unwrap()).unwrap();
        fs::write(journal.with_extension("1.log"), "old1\nold2\nold3\n").unwrap();
        fs::write(&journal, "new1\nnew2\n").unwrap();

        // Within the current file only.
        assert_eq!(read_recent_from(&journal, 2).unwrap(), vec!["new1", "new2"]);
        // Spills into the rotated file, oldest first.
        assert_eq!(
            read_recent_from(&journal, 4).unwrap(),
            vec!["old2", "old3", "new1", "new2"]
        );
        // Asking for more than exists returns everything.
        assert_eq!(read_recent_from(&journal, 99).unwrap().len(), 5);
        // Missing journal is not an error.
        let missing = journal.parent().unwrap().join("nope.log");
        assert!(read_recent_from(&missing, 5).unwrap().is_empty());
        cleanup(&journal);
    }

    #[test]
    fn test_clear_at_removes_journal_and_rotated_file() {
        let journal = test_journal_file();
        fs::create_dir_all(journal.parent().unwrap()).unwrap();
        let rotated = journal.with_extension("1.log");
        fs::write(&journal, "entry\n").unwrap();
        fs::write(&rotated, "old entry\n").unwrap();

        clear_at(&journal).unwrap();
        assert!(!journal.exists());
        assert!(!rotated.exists());

        // Clearing an already-absent journal is a no-op, not an error.
        clear_at(&journal).unwrap();

        cleanup(&journal);
    }

    #[test]
    fn journal_rotates_when_over_cap() {
        let journal = test_journal_file();
        fs::create_dir_all(journal.parent().unwrap()).unwrap();
        // Pre-fill beyond the cap so the next record triggers rotation.
        fs::write(&journal, vec![b'x'; (MAX_JOURNAL_BYTES + 1) as usize]).unwrap();

        let outcomes = vec![(PathBuf::from("C:\\after-rotate"), None)];
        record_to(&journal, JournalAction::Delete, &outcomes).unwrap();

        let rotated = journal.with_extension("1.log");
        assert!(rotated.exists(), "old journal must be rotated aside");
        assert!(fs::metadata(&rotated).unwrap().len() > MAX_JOURNAL_BYTES);
        let fresh = fs::read_to_string(&journal).unwrap();
        assert_eq!(fresh.lines().count(), 1);
        assert!(fresh.contains("C:\\after-rotate"));
        cleanup(&journal);
    }
}
