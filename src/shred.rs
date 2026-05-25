use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::Path;

use rand::RngCore;

const MIN_SHRED_PASSES: usize = 1;

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

    let mut buf = vec![0u8; 65536];

    for pass in 0..passes {
        file.seek(SeekFrom::Start(0))?;
        let mut written = 0u64;

        while written < len {
            let chunk = ((len - written) as usize).min(buf.len());
            if pass < passes - 1 {
                rand::thread_rng().fill_bytes(&mut buf[..chunk]);
            } else {
                buf[..chunk].fill(0);
            }
            file.write_all(&buf[..chunk])?;
            written += chunk as u64;
        }
        file.flush()?;
    }
    drop(file);

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
