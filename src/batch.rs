//! Shared batch-coordination infrastructure used by all three binaries
//! (zap, zapw, zapg). Collects paths from concurrently-launched Explorer
//! processes into a single deletion run.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const BATCH_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub const BATCH_QUIET_POLLS: usize = 10;
pub const BATCH_MAX_POLLS: usize = 120;

/// Lock file name shared across all binaries so they coordinate with
/// each other instead of having separate lock scopes.
pub const LOCK_FILE_NAME: &str = "zap-batch.lock";
pub const PATHS_DIR_NAME: &str = "zap-batch-paths.d";

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

pub fn batch_state(paths_dir: &Path) -> usize {
    let mut file_count = 0;

    if let Ok(entries) = fs::read_dir(paths_dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_file() {
                    file_count += 1;
                }
            }
        }
    }

    file_count
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
            Ok(mut file) => {
                for p in paths {
                    let encoded = hex_encode(p.as_os_str().as_encoded_bytes());
                    writeln!(file, "{encoded}")?;
                }
                file.flush()?;
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
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| entry.path())
        })
        .collect();
    // Ignore .tmp suffix files — writers use rename(tmp→txt) to make
    // the file appear atomically after flush.
    files.retain(|p| !p.to_string_lossy().ends_with(".tmp"));

    files
        .into_iter()
        .flat_map(|file| {
            let content = match fs::read_to_string(&file) {
                Ok(c) => c,
                Err(_) => {
                    // File may have been deleted between read_dir and now;
                    // silently skip it — the writer has already moved on.
                    return vec![];
                }
            };
            content
                .lines()
                .filter(|line| !line.is_empty())
                .filter_map(|line| {
                    let bytes = hex_decode(line)?;
                    // SAFETY: hex_decode produces arbitrary bytes from hex.
                    // OsString::from_encoded_bytes_unchecked is safe here
                    // because the original path bytes were valid OS strings,
                    // and hex roundtrip preserves exact byte content.
                    unsafe {
                        Some(PathBuf::from(
                            std::ffi::OsString::from_encoded_bytes_unchecked(bytes),
                        ))
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect()
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

/// Refresh the timestamp in the lock file. Uses append so a crash
/// mid-write leaves a valid file (reader ignores truncated last line).
pub fn touch_lock(lock: &mut File) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let _ = writeln!(lock, "tick:{nanos}");
    let _ = lock.flush();
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
