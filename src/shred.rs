use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Condvar, LazyLock, Mutex};

use rand::RngCore;

const MAX_CONCURRENT_SHREDS: usize = 2;
static SHRED_SLOTS: LazyLock<(Mutex<usize>, Condvar)> =
    LazyLock::new(|| (Mutex::new(0), Condvar::new()));

const MIN_SHRED_PASSES: usize = 1;

/// Overwrite buffer size. 1 MiB keeps syscall count low on multi-GB files
/// while staying cheap to allocate per shredded file.
const SHRED_BUF_BYTES: usize = 1024 * 1024;

struct ShredPermit;

impl ShredPermit {
    fn acquire() -> io::Result<Self> {
        let (lock, signal) = &*SHRED_SLOTS;
        let mut active = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        while *active >= MAX_CONCURRENT_SHREDS {
            if crate::stop::is_stop_requested() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "shred cancelled",
                ));
            }
            let waited = signal
                .wait_timeout(active, std::time::Duration::from_millis(100))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            active = waited.0;
        }
        *active += 1;
        Ok(Self)
    }
}

impl Drop for ShredPermit {
    fn drop(&mut self) {
        let (lock, signal) = &*SHRED_SLOTS;
        let mut active = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *active = active.saturating_sub(1);
        signal.notify_one();
    }
}

pub fn shred_file(path: &Path, passes: usize) -> io::Result<()> {
    let meta = fs::metadata(path)?;

    if meta.len() == 0 {
        if meta.permissions().readonly() {
            let _ = crate::delete::set_writable(path);
        }
        return fs::remove_file(path);
    }

    let mut file = match OpenOptions::new().write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            if meta.permissions().readonly() {
                crate::delete::set_writable(path)?;
                OpenOptions::new().write(true).open(path)?
            } else {
                return Err(e);
            }
        }
        Err(e) => return Err(e),
    };

    let outcome = overwrite(&mut file, passes)?;
    drop(file);
    match outcome {
        Overwrite::HardLinked => crate::delete::remove_file_with_retry(path),
        Overwrite::Done => remove_with_scrubbed_name(path),
    }
}

/// Result of [`overwrite`].
pub(crate) enum Overwrite {
    /// Every byte was overwritten and synced; the caller deletes the file.
    Done,
    /// Left untouched: other hard links share the data. A hard-linked file
    /// shares its data with names outside the deletion target (pnpm stores,
    /// dedup tools). Overwriting it would destroy that data too, and the
    /// content stays reachable through the other links anyway — so the
    /// caller only removes this name.
    HardLinked,
}

/// Overwrite the whole content of an open, writable file in place:
/// `passes - 1` random passes, then zeros, each synced to the device.
pub(crate) fn overwrite(file: &mut fs::File, passes: usize) -> io::Result<Overwrite> {
    let _permit = ShredPermit::acquire()?;
    if link_count(file)? > 1 {
        return Ok(Overwrite::HardLinked);
    }
    let passes = passes.max(MIN_SHRED_PASSES);
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(Overwrite::Done);
    }

    // Cap the buffer at the file length — no point allocating 1 MiB to
    // shred a 4 KiB file.
    let mut buf = vec![0u8; SHRED_BUF_BYTES.min(len.max(1) as usize)];
    let mut rng = rand::thread_rng();

    for pass in 0..passes {
        crate::stop::wait_if_paused();
        if crate::stop::is_stop_requested() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "shred cancelled",
            ));
        }
        file.seek(SeekFrom::Start(0))?;
        let mut written = 0u64;

        while written < len {
            crate::stop::wait_if_paused();
            if crate::stop::is_stop_requested() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "shred cancelled",
                ));
            }
            let chunk = ((len - written) as usize).min(buf.len());
            if pass < passes - 1 {
                rng.fill_bytes(&mut buf[..chunk]);
            } else {
                buf[..chunk].fill(0);
            }
            file.write_all(&buf[..chunk])?;
            written += chunk as u64;
        }
        file.flush()?;
        // `flush` only empties userspace buffers; force each pass onto the
        // physical device, otherwise the OS may coalesce all passes into a
        // single write and the overwrite guarantee is lost.
        file.sync_data()?;
    }
    Ok(Overwrite::Done)
}

/// Random 12-letter name used to hide a shredded file's original name.
pub(crate) fn random_name() -> String {
    let mut rng = rand::thread_rng();
    (0..12)
        .map(|_| char::from(b'a' + (rng.next_u32() % 26) as u8))
        .collect()
}

#[cfg(windows)]
fn link_count(file: &fs::File) -> io::Result<u32> {
    crate::winapi::link_count(file)
}

#[cfg(unix)]
fn link_count(file: &fs::File) -> io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.nlink() as u32)
}

#[cfg(not(any(windows, unix)))]
fn link_count(_file: &fs::File) -> io::Result<u32> {
    Ok(1)
}

/// The overwrite passes destroy the contents, but the original *filename*
/// would still linger in directory metadata (and journals like NTFS $LogFile)
/// after a plain remove. Rename to an anonymous name first so the deleted
/// entry no longer reveals what the file was called. Best-effort: if the
/// rename fails (e.g. a name collision or permissions), fall back to
/// deleting under the original name — content destruction already happened.
fn remove_with_scrubbed_name(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        for _ in 0..3 {
            let scrubbed = parent.join(random_name());
            if scrubbed.exists() {
                continue;
            }
            if fs::rename(path, &scrubbed).is_ok() {
                return fs::remove_file(&scrubbed);
            }
            break;
        }
    }
    fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("zap-shred-test-{}", id));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_shred_file_removes_file() {
        let dir = temp_dir();
        let path = dir.join("secret.txt");
        let original = b"top secret data that must be destroyed completely";
        fs::write(&path, original).unwrap();

        shred_file(&path, 3).unwrap();
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_shred_file_multiple_passes() {
        let dir = temp_dir();
        let path = dir.join("data.bin");
        let data = vec![0xAAu8; 4096];
        fs::write(&path, &data).unwrap();

        shred_file(&path, 7).unwrap();
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_shred_file_nonexistent() {
        let dir = temp_dir();
        let result = shred_file(&dir.join("no-such-file"), 1);
        assert!(result.is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_shred_leaves_directory_empty() {
        // The name-scrubbing rename must not leave the anonymous file behind.
        let dir = temp_dir();
        let path = dir.join("secret-name.txt");
        fs::write(&path, b"payload").unwrap();
        shred_file(&path, 2).unwrap();
        assert!(!path.exists());
        let leftovers: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert!(leftovers.is_empty(), "no files may remain after shred");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_shred_hard_link_keeps_data_of_other_names() {
        // Regression: shredding one name of a hard-linked file zeroed the
        // data of the other name, which lives outside the deletion target.
        let dir = temp_dir();
        let outside = dir.join("outside.txt");
        let link = dir.join("link.txt");
        fs::write(&outside, b"IMPORTANT DATA").unwrap();
        fs::hard_link(&outside, &link).unwrap();

        shred_file(&link, 3).unwrap();
        assert!(!link.exists());
        assert_eq!(fs::read(&outside).unwrap(), b"IMPORTANT DATA");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_shred_empty_file() {
        let dir = temp_dir();
        let path = dir.join("empty.bin");
        std::fs::File::create(&path).unwrap();
        assert!(path.exists());
        shred_file(&path, 1).unwrap();
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
