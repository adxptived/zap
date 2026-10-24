//! Thin Win32 helpers used by the scan and delete pipelines.
//!
//! All FFI here is gated on `#[cfg(windows)]`. Each function is a small,
//! single-syscall wrapper that replaces a heavier Rust-stdlib path:
//!
//! * [`is_reparse_point`] replaces `std::fs::symlink_metadata` for the
//!   reparse-attribute check. One syscall (`GetFileAttributesW`) instead of
//!   the open-query-close dance behind `symlink_metadata`.
//!
//! * [`force_delete`] issues a `FILE_DISPOSITION_INFO_EX` with POSIX
//!   semantics and `IGNORE_READONLY_ATTRIBUTE` so a file held open by another
//!   process or marked read-only is unlinked immediately (Win10 1709+). The
//!   fast tree delete uses it as its primary path; the std-based helpers use
//!   it as a last-resort retry.
//!
//! * [`for_each_dir_entry`] enumerates a directory with `FindFirstFileExW`,
//!   returning attributes and sizes without per-entry metadata syscalls.
//!
//! No external dependencies — declarations follow the same `extern "system"`
//! pattern that `main.rs` already uses for `AllocConsole`/`FreeConsole`.
#![cfg(windows)]

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

// ---------- Win32 type / constant declarations ----------------------------

// `HANDLE` is the canonical Win32 type name; suppress the `upper_case_acronyms`
// lint so the FFI signatures read like the official documentation.
#[allow(clippy::upper_case_acronyms, non_camel_case_types)]
type HANDLE = *mut c_void;

const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

/// `GetFileAttributesW` returns this when the call fails.
const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;

pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// `DELETE` access right from `winnt.h`.
const DELETE_ACCESS: u32 = 0x0001_0000;

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;

const FILE_SHARE_READ: u32 = 0x01;
const FILE_SHARE_WRITE: u32 = 0x02;
const FILE_SHARE_DELETE: u32 = 0x04;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
/// Required for `CreateFileW` to open a directory handle.
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

/// FINDEX_INFO_LEVELS::FindExInfoBasic — skips the 8.3 short-name lookup.
const FIND_EX_INFO_BASIC: i32 = 1;
/// FINDEX_SEARCH_OPS::FindExSearchNameMatch.
const FIND_EX_SEARCH_NAME_MATCH: i32 = 0;
/// Larger kernel buffer per directory query: fewer syscalls on big folders.
const FIND_FIRST_EX_LARGE_FETCH: u32 = 0x02;

const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_NO_MORE_FILES: i32 = 18;

/// Paths at least this long get the `\\?\` prefix so Win32 calls are not
/// limited by `MAX_PATH` (std does the same internally).
const LONG_PATH_THRESHOLD: usize = 240;

/// FILE_INFO_BY_HANDLE_CLASS::FileDispositionInfoEx (= 21).
const FILE_DISPOSITION_INFO_EX_CLASS: i32 = 21;

const FILE_DISPOSITION_FLAG_DELETE: u32 = 0x01;
const FILE_DISPOSITION_FLAG_POSIX_SEMANTICS: u32 = 0x02;
const FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE: u32 = 0x10;

#[repr(C)]
#[allow(non_snake_case)]
struct FileDispositionInfoEx {
    Flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FileTime {
    low: u32,
    high: u32,
}

#[repr(C)]
#[allow(non_snake_case)]
struct Win32FindDataW {
    dwFileAttributes: u32,
    ftCreationTime: FileTime,
    ftLastAccessTime: FileTime,
    ftLastWriteTime: FileTime,
    nFileSizeHigh: u32,
    nFileSizeLow: u32,
    dwReserved0: u32,
    dwReserved1: u32,
    cFileName: [u16; 260],
    cAlternateFileName: [u16; 14],
}

extern "system" {
    fn FindFirstFileExW(
        lpFileName: *const u16,
        fInfoLevelId: i32,
        lpFindFileData: *mut c_void,
        fSearchOp: i32,
        lpSearchFilter: *const c_void,
        dwAdditionalFlags: u32,
    ) -> HANDLE;
    fn FindNextFileW(hFindFile: HANDLE, lpFindFileData: *mut c_void) -> i32;
    fn FindClose(hFindFile: HANDLE) -> i32;
    fn GetFileAttributesW(lpFileName: *const u16) -> u32;
    fn CreateFileW(
        lpFileName: *const u16,
        dwDesiredAccess: u32,
        dwShareMode: u32,
        lpSecurityAttributes: *const c_void,
        dwCreationDisposition: u32,
        dwFlagsAndAttributes: u32,
        hTemplateFile: HANDLE,
    ) -> HANDLE;
    fn SetFileInformationByHandle(
        hFile: HANDLE,
        FileInformationClass: i32,
        lpFileInformation: *const c_void,
        dwBufferSize: u32,
    ) -> i32;
    fn CloseHandle(hObject: HANDLE) -> i32;
}

// ---------- Helpers --------------------------------------------------------

/// UTF-16 path without the trailing NUL, prefixed with `\\?\` (or
/// `\\?\UNC\`) when it is long enough to hit the `MAX_PATH` limit.
/// Relative paths are left alone: the verbatim prefix requires absolute ones.
fn wide_path(path: &Path) -> Vec<u16> {
    const BACKSLASH: u16 = b'\\' as u16;
    let raw: Vec<u16> = path.as_os_str().encode_wide().collect();
    if raw.len() < LONG_PATH_THRESHOLD || raw.starts_with(&[BACKSLASH, BACKSLASH, b'?' as u16]) {
        return raw;
    }
    let prefix: &str = if raw.starts_with(&[BACKSLASH, BACKSLASH]) {
        r"\\?\UNC"
    } else if raw.len() > 2 && raw[1] == b':' as u16 && raw[2] == BACKSLASH {
        r"\\?\"
    } else {
        return raw;
    };
    let skip = if prefix.ends_with("UNC") { 1 } else { 0 };
    prefix
        .encode_utf16()
        .chain(raw[skip..].iter().copied())
        .collect()
}

fn to_wide(path: &Path) -> Vec<u16> {
    let mut wide = wide_path(path);
    wide.push(0);
    wide
}

/// One child returned by [`for_each_dir_entry`].
pub struct RawDirEntry<'a> {
    pub name: &'a [u16],
    pub attributes: u32,
    pub size: u64,
}

impl RawDirEntry<'_> {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// Symlinks, junctions and other reparse points: never traversed.
    #[inline]
    pub fn is_reparse_point(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
}

/// Enumerate the direct children of `dir` (excluding `.` and `..`).
///
/// Uses `FindFirstFileExW` with `FindExInfoBasic` + `FIND_FIRST_EX_LARGE_FETCH`:
/// attributes and sizes come straight from the directory query, so callers
/// need no per-entry metadata syscall.
pub fn for_each_dir_entry(dir: &Path, mut f: impl FnMut(RawDirEntry<'_>)) -> io::Result<()> {
    let mut pattern = wide_path(dir);
    if pattern.last() != Some(&(b'\\' as u16)) {
        pattern.push(b'\\' as u16);
    }
    pattern.extend([b'*' as u16, 0]);

    // SAFETY: an all-zero WIN32_FIND_DATAW is a valid bit pattern.
    let mut data: Win32FindDataW = unsafe { std::mem::zeroed() };
    // SAFETY: `pattern` is NUL-terminated; `data` is a valid out-buffer.
    let handle = unsafe {
        FindFirstFileExW(
            pattern.as_ptr(),
            FIND_EX_INFO_BASIC,
            &mut data as *mut _ as *mut c_void,
            FIND_EX_SEARCH_NAME_MATCH,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let err = io::Error::last_os_error();
        // An empty drive root reports "not found" rather than an empty set.
        return if err.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) {
            Ok(())
        } else {
            Err(err)
        };
    }

    let result = loop {
        let len = data
            .cFileName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(data.cFileName.len());
        let name = &data.cFileName[..len];
        let is_dot = name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16];
        if !is_dot {
            f(RawDirEntry {
                name,
                attributes: data.dwFileAttributes,
                size: (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow),
            });
        }
        // SAFETY: `handle` is a live find handle and `data` a valid buffer.
        if unsafe { FindNextFileW(handle, &mut data as *mut _ as *mut c_void) } == 0 {
            let err = io::Error::last_os_error();
            break if err.raw_os_error() == Some(ERROR_NO_MORE_FILES) {
                Ok(())
            } else {
                Err(err)
            };
        }
    };

    // SAFETY: `handle` came from FindFirstFileExW and is closed exactly once.
    unsafe {
        FindClose(handle);
    }
    result
}

/// Join a directory and a raw UTF-16 child name from [`for_each_dir_entry`].
pub fn join_wide(dir: &Path, name: &[u16]) -> std::path::PathBuf {
    use std::os::windows::ffi::OsStringExt;
    dir.join(std::ffi::OsString::from_wide(name))
}

/// `true` when the volume rejected POSIX-semantics deletion (FAT32/exFAT,
/// network shares, pre-1709 Windows). Callers stop trying it for the run.
pub fn is_posix_delete_unsupported(err: &io::Error) -> bool {
    const ERROR_INVALID_FUNCTION: i32 = 1;
    const ERROR_NOT_SUPPORTED: i32 = 50;
    const ERROR_INVALID_PARAMETER: i32 = 87;
    matches!(
        err.raw_os_error(),
        Some(ERROR_INVALID_FUNCTION | ERROR_NOT_SUPPORTED | ERROR_INVALID_PARAMETER)
    )
}

/// Returns `true` if `path` carries the reparse-point attribute.
///
/// On Windows, junctions (`mklink /J`) are reparse-points but `jwalk` reports
/// them as regular directories (`jwalk::FileType::is_symlink()` is `false`).
/// Without this check the scanner would walk *into* the junction target —
/// exactly the safety hazard `follow_links(false)` is supposed to prevent.
///
/// Cost: one `GetFileAttributesW` syscall (no handle, no buffer). Faster
/// than `std::fs::symlink_metadata`, which opens a handle, queries
/// `BasicInformation`, and closes the handle.
pub fn is_reparse_point(path: &Path) -> io::Result<bool> {
    let wide = to_wide(path);
    // SAFETY: `wide` is a NUL-terminated UTF-16 buffer alive for the call.
    let attrs = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attrs == INVALID_FILE_ATTRIBUTES {
        return Err(io::Error::last_os_error());
    }
    Ok((attrs & FILE_ATTRIBUTE_REPARSE_POINT) != 0)
}

/// Force-delete a file (or empty directory / symlink / junction) using POSIX
/// semantics, bypassing readonly attributes and tolerating other open
/// handles.
///
/// Returns `Err` if the runtime does not support `FileDispositionInfoEx`
/// (pre-Win10 1709) or if the file does not exist. Callers should treat
/// any error as "POSIX delete unavailable; try other strategies".
///
/// Directories must already be empty (`ERROR_DIR_NOT_EMPTY` otherwise).
/// Typical scenarios where the standard `remove_file` path fails but this
/// succeeds:
///
/// * the file is held open by another process (Defender, IDE, indexer) that
///   did not grant `FILE_SHARE_DELETE`;
/// * the readonly attribute is set on a file inside a non-writable directory
///   so `set_writable` itself fails;
/// * the file is locked by an in-progress backup or transparent compression.
///
/// `FILE_DISPOSITION_FLAG_POSIX_SEMANTICS` removes the directory entry
/// immediately. Other open handles keep working on a "ghost" file until
/// they close.
pub fn force_delete(path: &Path) -> io::Result<()> {
    let wide = to_wide(path);

    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            DELETE_ACCESS,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            // OPEN_REPARSE_POINT: act on the link itself, never its target.
            // BACKUP_SEMANTICS: needed to open (empty) directories at all.
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    let info = FileDispositionInfoEx {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };

    // SAFETY: `info` is a valid FILE_DISPOSITION_INFO_EX whose lifetime
    // exceeds the syscall; `handle` is live for the duration of the call.
    let ok = unsafe {
        SetFileInformationByHandle(
            handle,
            FILE_DISPOSITION_INFO_EX_CLASS,
            &info as *const _ as *const c_void,
            std::mem::size_of::<FileDispositionInfoEx>() as u32,
        )
    };
    let err = if ok == 0 {
        Some(io::Error::last_os_error())
    } else {
        None
    };

    // SAFETY: `handle` was returned by CreateFileW above; closing is always
    // safe regardless of the SetFileInformationByHandle outcome.
    unsafe {
        CloseHandle(handle);
    }

    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
