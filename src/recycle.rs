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
    let wide_path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .chain(std::iter::once(0))
        .collect();

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
        return Err(io::Error::from_raw_os_error(result));
    }
    if file_op.fAnyOperationsAborted != 0 {
        return Err(io::Error::other(
            "Recycle operation was aborted by the user",
        ));
    }

    Ok(())
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
