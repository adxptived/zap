#![cfg(windows)]

#[link(name = "shell32")]
extern "system" {}

use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

#[repr(C)]
#[allow(non_snake_case)]
struct SHFileOpStructW {
    hwnd: *mut std::ffi::c_void,
    wFunc: u32,
    pFrom: *const u16,
    pTo: *const u16,
    fFlags: u16,
    fAnyOperationsAborted: i32,
    hNameMappings: *mut std::ffi::c_void,
    lpszProgressTitle: *const u16,
}

const FO_DELETE: u32 = 0x0003;
const FOF_ALLOWUNDO: u16 = 0x0040;
const FOF_NOCONFIRMATION: u16 = 0x0010;
const FOF_NOERRORUI: u16 = 0x0400;
const FOF_SILENT: u16 = 0x0004;

extern "system" {
    fn SHFileOperationW(lpFileOp: *mut SHFileOpStructW) -> i32;
}

/// Move a file or directory to the Windows Recycle Bin.
pub fn recycle_path(path: &Path) -> io::Result<()> {
    recycle_many(std::slice::from_ref(&path))
}

/// Move several paths to the Recycle Bin with a *single* `SHFileOperationW`
/// call. `pFrom` is a double-null-terminated list of null-terminated paths,
/// so Explorer treats the whole batch as one operation: one shell roundtrip
/// and a single "Undo" entry instead of one per item.
pub fn recycle_paths<P: AsRef<Path>>(paths: &[P]) -> io::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let refs: Vec<&Path> = paths.iter().map(|p| p.as_ref()).collect();
    recycle_many(&refs)
}

/// `SHFileOperationW` rejects paths at or beyond the classic `MAX_PATH`
/// limit and does not understand `\\?\` verbatim prefixes.
const SHELL_MAX_PATH: usize = 260;

fn recycle_many(paths: &[&Path]) -> io::Result<()> {
    let mut wide_path: Vec<u16> = Vec::new();
    for path in paths {
        // The shell resolves relative paths against an unpredictable working
        // directory — always hand it absolute paths.
        let abs = std::path::absolute(path)?;
        let wide: Vec<u16> = abs.as_os_str().encode_wide().collect();
        if wide.len() + 1 >= SHELL_MAX_PATH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidFilename,
                format!(
                    "path is too long for the Recycle Bin shell API (>= {SHELL_MAX_PATH} chars): {} — delete permanently instead",
                    abs.display()
                ),
            ));
        }
        wide_path.extend(wide);
        wide_path.push(0);
    }
    wide_path.push(0); // double-null terminator for the whole list

    let mut file_op = SHFileOpStructW {
        hwnd: std::ptr::null_mut(),
        wFunc: FO_DELETE,
        pFrom: wide_path.as_ptr(),
        pTo: std::ptr::null(),
        fFlags: FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };

    let result = unsafe { SHFileOperationW(&mut file_op) };
    if result != 0 {
        return Err(shell_op_error(result));
    }
    if file_op.fAnyOperationsAborted != 0 {
        return Err(io::Error::other(
            "Recycle operation was aborted by the user",
        ));
    }

    Ok(())
}

/// `SHFileOperationW` returns its own `DE_*` codes (0x71–0x88), not Win32
/// error codes — mapping them through `io::Error::from_raw_os_error` yields
/// misleading messages. Translate the common ones explicitly.
fn shell_op_error(code: i32) -> io::Error {
    let (kind, msg) = match code {
        0x74 => (io::ErrorKind::InvalidInput, "the source is a root directory"),
        0x76 => (io::ErrorKind::PermissionDenied, "security settings denied access to the source"),
        0x78 => (io::ErrorKind::PermissionDenied, "access denied to the source file or folder"),
        0x7C => (io::ErrorKind::InvalidInput, "the path is invalid"),
        0x7E | 0x80 => (io::ErrorKind::AlreadyExists, "an item with this name already exists"),
        0x81 => (io::ErrorKind::InvalidInput, "the file is too large for the Recycle Bin"),
        0x82 => (io::ErrorKind::PermissionDenied, "the source is a read-only disc"),
        0x02 => (io::ErrorKind::NotFound, "the file was not found"),
        0x03 => (io::ErrorKind::NotFound, "the path was not found"),
        0x05 => (io::ErrorKind::PermissionDenied, "access is denied"),
        _ => {
            return io::Error::other(format!(
                "Recycle Bin operation failed (shell error code {code:#x})"
            ))
        }
    };
    io::Error::new(kind, format!("Recycle Bin operation failed: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("zap-recycle-test-{id}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_recycle_file_moves_to_bin() {
        let dir = temp_dir();
        let path = dir.join("recycle_me.txt");
        fs::write(&path, b"test data").unwrap();
        assert!(path.exists());

        recycle_path(&path).unwrap();
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_recycle_dir_moves_to_bin() {
        let dir = temp_dir();
        let sub = dir.join("folder_to_recycle");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("inside.txt"), b"data").unwrap();

        recycle_path(&sub).unwrap();
        assert!(!sub.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_recycle_nonexistent_returns_error() {
        let dir = temp_dir();
        let result = recycle_path(&dir.join("ghost.txt"));
        assert!(result.is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
