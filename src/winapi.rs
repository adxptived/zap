//! Thin Win32 helpers used by the scan and delete pipelines.
//!
//! All FFI here is gated on `#[cfg(windows)]`. Each function is a small,
//! single-syscall wrapper that replaces a heavier Rust-stdlib path:
//!
//! * [`is_reparse_point`] replaces `std::fs::symlink_metadata` for the
//!   reparse-attribute check. One syscall (`GetFileAttributesW`) instead of
//!   the open-query-close dance behind `symlink_metadata`.
//!
//! * [`force_delete`] replaces `std::fs::remove_file` for the *last-resort*
//!   retry: it issues a `FILE_DISPOSITION_INFO_EX` with POSIX semantics and
//!   `IGNORE_READONLY_ATTRIBUTE` so a file held open by another process or
//!   marked read-only is unlinked immediately (Win10 1709+).
//!
//! No external dependencies — declarations follow the same `extern "system"`
//! pattern that `main.rs` already uses for `AllocConsole`/`FreeConsole`.
#![cfg(windows)]

use std::ffi::c_void;
use std::io;
use std::iter;
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

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// `DELETE` access right from `winnt.h`.
const DELETE_ACCESS: u32 = 0x0001_0000;

const FILE_SHARE_READ: u32 = 0x01;
const FILE_SHARE_WRITE: u32 = 0x02;
const FILE_SHARE_DELETE: u32 = 0x04;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

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

extern "system" {
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

fn to_wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect()
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
/// Only call this as a *last-resort retry* after `std::fs::remove_file` and
/// the `set_writable` retry both failed with `PermissionDenied`. Typical
/// scenarios where the standard path fails on Windows:
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
            FILE_FLAG_OPEN_REPARSE_POINT, // never traverse the link
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
