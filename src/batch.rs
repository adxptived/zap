//! Shared batch-coordination infrastructure used by all three binaries
//! (zap, zapw, zapg). Collects paths from concurrently-launched Explorer
//! processes into a single deletion run.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const BATCH_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub const BATCH_QUIET_POLLS: usize = 10;
pub const BATCH_MAX_POLLS: usize = 120;
const MAX_BATCH_FILES: usize = 16_384;
const MAX_BATCH_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_BATCH_PATHS: usize = 100_000;
/// Longest Windows path (32 767 UTF-16 units) at four hex digits per unit.
const MAX_ENCODED_PATH_BYTES: usize = 4 * 32_768;

/// Returns the batch coordination root. Respects `ZAP_BATCH_ROOT` for
/// test isolation; defaults to `%TEMP%` when the env var is unset.
pub fn batch_root() -> PathBuf {
    std::env::var("ZAP_BATCH_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
}

/// Stable identifier of one batch scope: the binary plus every option that
/// changes what happens to the collected paths.
///
/// Only launches with the same scope may merge. A shared scope let a path
/// sent to "Move to Recycle Bin" (or to the confirmation dialog) be picked
/// up by a running "Delete permanently" leader and erased silently.
pub fn batch_scope(binary: &str, config: &str) -> String {
    // FNV-1a: deterministic across processes and builds, unlike `Hash`.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in binary
        .as_bytes()
        .iter()
        .chain(b"\0")
        .chain(config.as_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{binary}-{hash:016x}")
}

/// Returns the batch paths directory for `scope` within the batch root.
pub fn batch_paths_dir(scope: &str) -> PathBuf {
    batch_root().join(format!("zap-batch-paths-{scope}.d"))
}

/// Returns the batch lock file for `scope` within the batch root.
pub fn batch_lock_file(scope: &str) -> PathBuf {
    batch_root().join(format!("zap-batch-{scope}.lock"))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    out
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut bytes = Vec::with_capacity(s.len() / 2);
    let chars: Vec<u8> = s.bytes().collect();
    for chunk in chars.chunks_exact(2) {
        let hi = hex_byte(chunk[0])?;
        let lo = hex_byte(chunk[1])?;
        bytes.push((hi << 4) | lo);
    }
    Some(bytes)
}

fn hex_byte(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// One batch line per path, in the platform's native, lossless unit:
/// UTF-16 code units on Windows (four hex digits each), raw bytes on Unix.
/// Decoding goes through safe constructors only — a corrupted or hostile
/// batch file can yield a wrong path, never undefined behavior.
#[cfg(windows)]
fn encode_path(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;
    let bytes: Vec<u8> = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_be_bytes)
        .collect();
    hex_encode(&bytes)
}

#[cfg(windows)]
fn decode_path(line: &str) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let bytes = hex_decode(line)?;
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    Some(PathBuf::from(std::ffi::OsString::from_wide(&units)))
}

#[cfg(unix)]
fn encode_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    hex_encode(path.as_os_str().as_bytes())
}

#[cfg(unix)]
fn decode_path(line: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(hex_decode(
        line,
    )?)))
}

#[cfg(not(any(windows, unix)))]
fn encode_path(path: &Path) -> String {
    hex_encode(path.to_string_lossy().as_bytes())
}

#[cfg(not(any(windows, unix)))]
fn decode_path(line: &str) -> Option<PathBuf> {
    String::from_utf8(hex_decode(line)?).ok().map(PathBuf::from)
}

#[inline]
fn is_committed_batch_file(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "txt")
}

pub fn batch_state(paths_dir: &Path) -> usize {
    fs::read_dir(paths_dir)
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .filter(|entry| {
            is_committed_batch_file(&entry.path())
                && entry.metadata().is_ok_and(|metadata| metadata.is_file())
        })
        .take(MAX_BATCH_FILES)
        .count()
}

pub fn wait_for_batch_quiet(paths_dir: &Path, required_quiet_polls: usize, max_polls: usize) {
    let mut last_state = batch_state(paths_dir);
    let mut quiet_polls = 0;

    for _ in 0..max_polls {
        thread::sleep(BATCH_POLL_INTERVAL);
        let current_state = batch_state(paths_dir);
        if current_state == last_state {
            quiet_polls += 1;
            if quiet_polls >= required_quiet_polls {
                break;
            }
        } else {
            quiet_polls = 0;
            last_state = current_state;
        }
    }
}

pub fn write_batch_paths(paths_dir: &Path, paths: &[PathBuf]) -> io::Result<()> {
    fs::create_dir_all(paths_dir)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();

    for attempt in 0..100_u32 {
        let path = paths_dir.join(format!("{timestamp}-{pid}-{attempt}.txt.tmp"));
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(file) => {
                let mut writer = BufWriter::with_capacity(64 * 1024, file);
                for p in paths {
                    writeln!(writer, "{}", encode_path(p))?;
                }
                writer.flush()?;
                drop(writer);

                let final_path = path.with_extension("txt");
                if let Err(err) = fs::rename(&path, &final_path) {
                    let _ = fs::remove_file(&path);
                    return Err(err);
                }
                return Ok(());
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique batch path file",
    ))
}

/// Read and *claim* every committed batch file: each file is renamed to a
/// private name before it is read, so a path is handed to exactly one
/// reader even when an old leader and a new one look at the directory at
/// the same time. Repeated calls therefore return only new arrivals.
pub fn read_batch_paths(paths_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(paths_dir)
        .into_iter()
        .flat_map(|entries| entries.flatten())
        .filter_map(|entry| {
            let path = entry.path();
            if !is_committed_batch_file(&path) {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            (metadata.is_file() && metadata.len() <= MAX_BATCH_FILE_BYTES).then_some(path)
        })
        .take(MAX_BATCH_FILES)
        .collect();

    files.sort_unstable();
    let pid = std::process::id();
    let mut paths = Vec::new();
    for committed in files {
        if paths.len() >= MAX_BATCH_PATHS {
            break; // unclaimed files stay for the next read
        }
        let mut claimed = committed.clone().into_os_string();
        claimed.push(format!(".{pid}.claim"));
        let claimed = PathBuf::from(claimed);
        if fs::rename(&committed, &claimed).is_err() {
            continue; // another reader claimed it first
        }
        read_claimed_file(&claimed, &mut paths);
        let _ = fs::remove_file(&claimed);
    }
    paths
}

fn read_claimed_file(claimed: &Path, paths: &mut Vec<PathBuf>) {
    let Ok(file) = File::open(claimed) else {
        return;
    };
    for line in BufReader::new(file).lines() {
        if paths.len() >= MAX_BATCH_PATHS {
            return;
        }
        let Ok(line) = line else {
            break;
        };
        if line.is_empty() || line.len() > MAX_ENCODED_PATH_BYTES {
            continue;
        }
        if let Some(path) = decode_path(&line) {
            paths.push(path);
        }
    }
}

/// End a leader's session without losing late arrivals.
///
/// The lock goes first, so every launch from now on becomes a new leader
/// instead of handing its paths to us and exiting. Whatever was committed
/// before that is claimed and returned for processing. The directory is
/// removed only if empty: files of a new leader must survive.
pub fn release_and_collect(paths_dir: &Path, lock_file: &Path, lock: Option<File>) -> Vec<PathBuf> {
    drop(lock);
    let _ = fs::remove_file(lock_file);
    let rest = read_batch_paths(paths_dir);
    let _ = fs::remove_dir(paths_dir);
    rest
}

pub fn cleanup_stale_batch(paths_dir: &Path, lock_file: &Path) {
    let should_clean = match fs::read_to_string(lock_file) {
        Ok(content) => {
            let lines: Vec<&str> = content.lines().collect();
            // First line is PID, any subsequent line is a tick.
            let is_alive = lines
                .first()
                .and_then(|s| s.parse::<u32>().ok())
                .is_some_and(is_pid_alive);
            let is_stale_mtime = fs::metadata(lock_file)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|mtime| {
                    mtime
                        .elapsed()
                        .ok()
                        .map(|elapsed| elapsed > Duration::from_secs(30))
                })
                .unwrap_or(false);
            !is_alive && is_stale_mtime
        }
        Err(_) => false,
    };

    if should_clean {
        let _ = fs::remove_dir_all(paths_dir);
        let _ = fs::remove_file(lock_file);
    }
}

pub fn try_acquire_lock(lock_file: &Path) -> io::Result<File> {
    let mut lock = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(lock_file)?;
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    writeln!(lock, "{pid}")?;
    writeln!(lock, "{nanos}")?;
    lock.flush()?;
    Ok(lock)
}

/// Refresh the timestamp without growing the heartbeat file indefinitely.
/// The process still owns the open handle, so rewriting PID + timestamp is
/// sufficient and keeps stale-lock inspection bounded.
pub fn touch_lock(lock: &mut File) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    if lock.set_len(0).is_err() || lock.seek(SeekFrom::Start(0)).is_err() {
        return;
    }
    let _ = writeln!(lock, "{}", std::process::id());
    let _ = writeln!(lock, "{nanos}");
    let _ = lock.flush();
}

/// Reopen `lock_file`, truncate, and write the current PID + timestamp.
/// This allows a background worker to adopt ownership of a lock that was
/// created by a short-lived launcher process, keeping mtime fresh so that
/// `cleanup_stale_batch` does not treat an active batch as abandoned.
pub fn refresh_lock_owner(lock_file: &Path) -> io::Result<File> {
    let mut lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(lock_file)?;
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    writeln!(lock, "{pid}")?;
    writeln!(lock, "{nanos}")?;
    lock.flush()?;
    Ok(lock)
}

#[cfg(windows)]
fn is_pid_alive(pid: u32) -> bool {
    use std::ffi::c_void;
    extern "system" {
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> *mut c_void;
        fn CloseHandle(hObject: *mut c_void) -> i32;
        fn GetExitCodeProcess(hProcess: *mut c_void, lpExitCode: *mut u32) -> i32;
    }

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        ok != 0 && exit_code == STILL_ACTIVE
    }
}

#[cfg(not(windows))]
fn is_pid_alive(_pid: u32) -> bool {
    // Non-Windows: we don't have batch-mode Explorer integration, so
    // this code path is unreachable. Default to false (stale).
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("zap-batch-test-{id}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_read_batch_paths_ignores_non_txt_entries_without_metadata_dependency() {
        let dir = temp_dir();
        let wanted = PathBuf::from(r"C:\Users\example\wanted.txt");
        write_batch_paths(&dir, std::slice::from_ref(&wanted)).unwrap();
        fs::write(dir.join("partial.txt.tmp"), "43415c746d70\n").unwrap();
        fs::write(dir.join("notes.log"), "43415c6c6f67\n").unwrap();
        fs::create_dir(dir.join("nested.txt")).unwrap();

        let paths = read_batch_paths(&dir);
        assert_eq!(paths, vec![wanted]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(windows)]
    fn test_batch_paths_roundtrip_unpaired_surrogates_and_reject_garbage() {
        use std::os::windows::ffi::OsStringExt;
        let dir = temp_dir();
        // Valid on NTFS but not valid UTF-16: must survive unchanged.
        let odd = PathBuf::from(std::ffi::OsString::from_wide(&[
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0xD800,
            b'x' as u16,
        ]));
        write_batch_paths(&dir, std::slice::from_ref(&odd)).unwrap();
        // Hand-written garbage: odd digit counts, non-hex, half a unit.
        fs::write(dir.join("zz-garbage.txt"), "abc\nzzzz\n00410\nf0\n").unwrap();
        assert_eq!(read_batch_paths(&dir), vec![odd]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_batch_paths_claims_each_file_once() {
        let dir = temp_dir();
        let first = PathBuf::from(r"C:\first");
        write_batch_paths(&dir, std::slice::from_ref(&first)).unwrap();
        assert_eq!(read_batch_paths(&dir), vec![first]);
        // A second reader (or a later poll) must not get the same path.
        assert!(read_batch_paths(&dir).is_empty());

        let late = PathBuf::from(r"C:\late");
        write_batch_paths(&dir, std::slice::from_ref(&late)).unwrap();
        assert_eq!(read_batch_paths(&dir), vec![late]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_release_and_collect_returns_late_paths_and_frees_scope() {
        // Regression: a launch that lost the lock race just before the
        // leader wiped the directory was silently dropped.
        let dir = temp_dir();
        let paths_dir = dir.join("paths");
        let lock_file = dir.join("lock");
        let lock = try_acquire_lock(&lock_file).unwrap();
        let late = PathBuf::from(r"C:\late-arrival");
        write_batch_paths(&paths_dir, std::slice::from_ref(&late)).unwrap();
        assert!(
            try_acquire_lock(&lock_file).is_err(),
            "follower loses the race"
        );

        assert_eq!(
            release_and_collect(&paths_dir, &lock_file, Some(lock)),
            vec![late]
        );
        assert!(!paths_dir.exists());
        // The next launch becomes a leader instead of exiting.
        assert!(try_acquire_lock(&lock_file).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_batch_state_counts_committed_txt_without_metadata() {
        let dir = temp_dir();
        fs::write(dir.join("first.txt"), "00\n").unwrap();
        fs::write(dir.join("second.txt"), "00\n").unwrap();
        fs::write(dir.join("partial.txt.tmp"), "00\n").unwrap();
        fs::write(dir.join("notes.log"), "00\n").unwrap();
        fs::create_dir(dir.join("nested.txt")).unwrap();

        assert_eq!(batch_state(&dir), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_batch_paths_rejects_oversized_files() {
        let dir = temp_dir();
        let oversized = File::create(dir.join("oversized.txt")).unwrap();
        oversized.set_len(MAX_BATCH_FILE_BYTES + 1).unwrap();
        drop(oversized);

        assert!(read_batch_paths(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_touch_lock_keeps_heartbeat_compact() {
        let dir = temp_dir();
        let lock_path = dir.join("lock");
        let mut lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        for _ in 0..1_000 {
            touch_lock(&mut lock);
        }
        drop(lock);

        let content = fs::read_to_string(&lock_path).unwrap();
        assert_eq!(content.lines().count(), 2);
        assert!(fs::metadata(&lock_path).unwrap().len() < 128);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_batch_paths_dir_respects_env_override() {
        let custom = std::env::temp_dir().join(format!("zap-batch-custom-{}", std::process::id()));
        std::env::set_var("ZAP_BATCH_ROOT", &custom);
        // batch_root and derived functions must pick up env at call time
        assert!(batch_paths_dir("s").starts_with(&custom));
        assert!(batch_lock_file("s").starts_with(&custom));
        std::env::remove_var("ZAP_BATCH_ROOT");
    }

    #[test]
    fn test_batch_scope_separates_configurations() {
        let recycle = batch_scope("zapw", "recycle");
        assert_eq!(recycle, batch_scope("zapw", "recycle"));
        assert_ne!(recycle, batch_scope("zapw", "delete"));
        assert_ne!(recycle, batch_scope("zapg", "recycle"));
        assert_ne!(batch_lock_file(&recycle), batch_lock_file("zapw-other"));
        assert_ne!(batch_paths_dir(&recycle), batch_paths_dir("zapw-other"));
    }

    #[test]
    fn test_refresh_lock_owner_rewrites_lock_with_current_pid() {
        let dir = temp_dir();
        let lock_file = dir.join("lock");
        // Simulate an old lock from another PID
        fs::write(&lock_file, "99999\n1000\n").unwrap();
        let lock = refresh_lock_owner(&lock_file).unwrap();
        drop(lock);

        let content = fs::read_to_string(&lock_file).unwrap();
        let first_line = content.lines().next().unwrap();
        let pid: u32 = first_line.parse().unwrap();
        // The refresh must have written our PID, not the old one
        assert_eq!(pid, std::process::id());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_batch_paths_handles_partial_write_rename() {
        let dir = temp_dir();
        let path1 = PathBuf::from(r"C:\Users\example\file1.txt");
        let path2 = PathBuf::from(r"C:\Users\example\file2.txt");

        write_batch_paths(&dir, &[path1.clone(), path2.clone()]).unwrap();
        let paths = read_batch_paths(&dir);
        assert_eq!(paths, vec![path1, path2]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_refresh_lock_owner_truncates_old_tick_lines() {
        let dir = temp_dir();
        let lock_file = dir.join("lock");
        fs::write(&lock_file, "11111\n100\n").unwrap();

        // Open and add tick lines
        let mut lock = OpenOptions::new().append(true).open(&lock_file).unwrap();
        writeln!(lock, "tick:200").unwrap();
        writeln!(lock, "tick:300").unwrap();
        lock.flush().unwrap();
        drop(lock);

        // Now refresh -- should truncate to just PID + nanos
        let lock = refresh_lock_owner(&lock_file).unwrap();
        drop(lock);

        let content = fs::read_to_string(&lock_file).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "should have exactly PID + nanos lines");
        assert!(
            !lines[0].starts_with("tick:"),
            "no tick lines after refresh"
        );
        let pid: u32 = lines[0].parse().unwrap();
        assert_eq!(pid, std::process::id());

        let _ = fs::remove_dir_all(&dir);
    }
}
