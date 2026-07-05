use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use rand::RngCore;

const MIN_SHRED_PASSES: usize = 1;

/// Overwrite buffer size. 1 MiB keeps syscall count low on multi-GB files
/// while staying cheap to allocate per shredded file.
const SHRED_BUF_BYTES: usize = 1024 * 1024;

pub fn shred_file(path: &Path, passes: usize) -> io::Result<()> {
    let passes = passes.max(MIN_SHRED_PASSES);
    let meta = fs::metadata(path)?;
    let len = meta.len();

    if len == 0 {
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

    // Cap the buffer at the file length — no point allocating 1 MiB to
    // shred a 4 KiB file.
    let mut buf = vec![0u8; SHRED_BUF_BYTES.min(len.max(1) as usize)];
    let mut rng = rand::thread_rng();

    for pass in 0..passes {
        file.seek(SeekFrom::Start(0))?;
        let mut written = 0u64;

        while written < len {
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
    drop(file);

    remove_with_scrubbed_name(path)
}

/// The overwrite passes destroy the contents, but the original *filename*
/// would still linger in directory metadata (and journals like NTFS $LogFile)
/// after a plain remove. Rename to an anonymous name first so the deleted
/// entry no longer reveals what the file was called. Best-effort: if the
/// rename fails (e.g. a name collision or permissions), fall back to
/// deleting under the original name — content destruction already happened.
fn remove_with_scrubbed_name(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        let mut rng = rand::thread_rng();
        for _ in 0..3 {
            let name: String = (0..12)
                .map(|_| char::from(b'a' + (rng.next_u32() % 26) as u8))
                .collect();
            let scrubbed = parent.join(name);
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
