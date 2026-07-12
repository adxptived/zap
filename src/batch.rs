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
const MAX_ENCODED_PATH_BYTES: usize = 64 * 1024;

/// Lock file name shared across all binaries so they coordinate with
/// each other instead of having separate lock scopes.
pub const LOCK_FILE_NAME: &str = "zap-batch.lock";
pub const PATHS_DIR_NAME: &str = "zap-batch-paths.d";

/// Returns the batch coordination root. Respects `ZAP_BATCH_ROOT` for
/// test isolation; defaults to `%TEMP%` when the env var is unset.
pub fn batch_root() -> PathBuf {
    std::env::var("ZAP_BATCH_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
}

/// Returns the batch paths directory within the batch root.
pub fn batch_paths_dir() -> PathBuf {
    batch_root().join(PATHS_DIR_NAME)
}

/// Returns the batch lock file path within the batch root.
pub fn batch_lock_file() -> PathBuf {
    batch_root().join(LOCK_FILE_NAME)
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
                    let encoded = hex_encode(p.as_os_str().as_encoded_bytes());
                    writeln!(writer, "{encoded}")?;
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
    let mut paths = Vec::new();
    'files: for file in files {
        let Ok(file) = File::open(file) else {
            continue;
        };
        for line in BufReader::new(file).lines() {
            if paths.len() >= MAX_BATCH_PATHS {
                break 'files;
            }
            let Ok(line) = line else {
                break;
            };
            if line.is_empty() || line.len() > MAX_ENCODED_PATH_BYTES {
                continue;
            }
            let Some(bytes) = hex_decode(&line) else {
                continue;
            };
            // SAFETY: encoded bytes came from an OS string and the exact
            // roundtrip below rejects malformed platform encodings.
            let os_str = unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(bytes.clone()) };
            if os_str.as_encoded_bytes() == bytes {
                paths.push(PathBuf::from(os_str));
            }
        }
    }
    paths
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
        assert!(batch_paths_dir().starts_with(&custom));
        assert!(batch_lock_file().starts_with(&custom));
        std::env::remove_var("ZAP_BATCH_ROOT");
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
