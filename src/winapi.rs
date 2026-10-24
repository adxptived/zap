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
//!   semantics and `IGNORE_READONLY_ATTRIBUTE` so a read-only file, or one
//!   another process opened with `FILE_SHARE_DELETE`, is unlinked
//!   immediately (Win10 1709+). The
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
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
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
    fn GetFileInformationByHandle(hFile: HANDLE, lpFileInformation: *mut c_void) -> i32;
    fn GetFileInformationByHandleEx(
        hFile: HANDLE,
        FileInformationClass: i32,
        lpFileInformation: *mut c_void,
        dwBufferSize: u32,
    ) -> i32;
}

#[repr(C)]
#[allow(non_snake_case)]
struct ByHandleFileInformation {
    dwFileAttributes: u32,
    ftCreationTime: FileTime,
    ftLastAccessTime: FileTime,
    ftLastWriteTime: FileTime,
    dwVolumeSerialNumber: u32,
    nFileSizeHigh: u32,
    nFileSizeLow: u32,
    nNumberOfLinks: u32,
    nFileIndexHigh: u32,
    nFileIndexLow: u32,
}

/// `GetFileInformationByHandle` on a raw handle.
fn handle_info(handle: HANDLE) -> io::Result<ByHandleFileInformation> {
    // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid bit pattern.
    let mut info: ByHandleFileInformation = unsafe { std::mem::zeroed() };
    // SAFETY: `handle` is live for the call and `info` is a valid out-buffer.
    if unsafe { GetFileInformationByHandle(handle, &mut info as *mut _ as *mut c_void) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

/// Number of hard links to an open file. Overwriting a file with more than
/// one link would destroy the data seen through the other names too.
pub fn link_count(file: &std::fs::File) -> io::Result<u32> {
    Ok(handle_info(file.as_raw_handle() as HANDLE)?.nNumberOfLinks)
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
        if !is_dot_entry(name) {
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

#[inline]
fn is_dot_entry(name: &[u16]) -> bool {
    name == [b'.' as u16] || name == [b'.' as u16, b'.' as u16]
}

// ---------- Handle-based traversal -----------------------------------------

const FILE_LIST_DIRECTORY: u32 = 0x0001;
const FILE_READ_ATTRIBUTES: u32 = 0x0080;
const SYNCHRONIZE: u32 = 0x0010_0000;
/// FILE_INFO_BY_HANDLE_CLASS::FileFullDirectoryInfo.
const FILE_FULL_DIRECTORY_INFO_CLASS: i32 = 14;
/// Byte offsets inside FILE_FULL_DIR_INFO.
const FULL_DIR_INFO_END_OF_FILE: usize = 40;
const FULL_DIR_INFO_ATTRIBUTES: usize = 56;
const FULL_DIR_INFO_NAME_LENGTH: usize = 60;
const FULL_DIR_INFO_NAME: usize = 68;

const OBJ_CASE_INSENSITIVE: u32 = 0x40;
const FILE_OPEN: u32 = 1;
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
/// Access rights for [`DirHandle::open_entry`].
pub const ACCESS_DELETE: u32 = 0x0001_0000;
pub const ACCESS_WRITE_DATA: u32 = 0x0002;
pub const ACCESS_WRITE_ATTRIBUTES: u32 = 0x0100;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
const FILE_OPEN_FOR_BACKUP_INTENT: u32 = 0x0000_4000;
const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: HANDLE,
    object_name: *const UnicodeString,
    attributes: u32,
    security_descriptor: *const c_void,
    security_quality_of_service: *const c_void,
}

#[repr(C)]
struct IoStatusBlock {
    status: usize,
    information: usize,
}

#[link(name = "ntdll", kind = "raw-dylib")]
extern "system" {
    fn NtCreateFile(
        file_handle: *mut HANDLE,
        desired_access: u32,
        object_attributes: *const ObjectAttributes,
        io_status_block: *mut IoStatusBlock,
        allocation_size: *const i64,
        file_attributes: u32,
        share_access: u32,
        create_disposition: u32,
        create_options: u32,
        ea_buffer: *const c_void,
        ea_length: u32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}

/// `NtCreateFile` of `name` relative to the directory handle `root`, with
/// `FILE_OPEN_REPARSE_POINT` always set: a link is opened as itself.
fn nt_open_relative(root: HANDLE, name: &[u16], access: u32, options: u32) -> io::Result<HANDLE> {
    let Ok(bytes) = u16::try_from(name.len() * 2) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "name too long"));
    };
    let object_name = UnicodeString {
        length: bytes,
        maximum_length: bytes,
        buffer: name.as_ptr(),
    };
    let attributes = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root_directory: root,
        object_name: &object_name,
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: std::ptr::null(),
        security_quality_of_service: std::ptr::null(),
    };
    let mut handle: HANDLE = std::ptr::null_mut();
    let mut io_status = IoStatusBlock {
        status: 0,
        information: 0,
    };
    // SAFETY: all pointers reference locals that outlive the call.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut io_status,
            std::ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            options
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_OPEN_FOR_BACKUP_INTENT
                | FILE_OPEN_REPARSE_POINT,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        // SAFETY: pure conversion of a status code.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    Ok(handle)
}

// ---------- Operations on an open entry ------------------------------------

/// FILE_INFO_BY_HANDLE_CLASS values.
const FILE_BASIC_INFO_CLASS: i32 = 0;
const FILE_RENAME_INFO_CLASS: i32 = 3;
const FILE_DISPOSITION_INFO_CLASS: i32 = 4;
const FILE_ATTRIBUTE_READONLY: u32 = 0x01;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

#[repr(C)]
struct FileBasicInfo {
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    file_attributes: u32,
}

fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle() as HANDLE
}

fn set_info<T>(handle: &OwnedHandle, class: i32, info: &T, size: usize) -> io::Result<()> {
    // SAFETY: `info` points to at least `size` readable bytes laid out as
    // the structure `class` expects; `handle` is live for the call.
    let ok = unsafe {
        SetFileInformationByHandle(
            raw(handle),
            class,
            info as *const T as *const c_void,
            size as u32,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Delete with POSIX semantics, ignoring the read-only attribute: the name
/// disappears at once even if other handles stay open (Win10 1709+, NTFS).
/// See [`is_posix_delete_unsupported`] for the fallback signal.
pub fn delete_posix(handle: &OwnedHandle) -> io::Result<()> {
    let info = FileDispositionInfoEx {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    set_info(
        handle,
        FILE_DISPOSITION_INFO_EX_CLASS,
        &info,
        std::mem::size_of_val(&info),
    )
}

/// Classic delete-on-close, for volumes without POSIX delete. Fails with
/// "access denied" on read-only entries; see [`clear_readonly`].
pub fn delete_legacy(handle: &OwnedHandle) -> io::Result<()> {
    let delete_file: u8 = 1; // FILE_DISPOSITION_INFO { BOOLEAN DeleteFile }
    set_info(handle, FILE_DISPOSITION_INFO_CLASS, &delete_file, 1)
}

/// Clear the read-only attribute (the handle needs `FILE_WRITE_ATTRIBUTES`).
/// Returns `false` when it was not set, so there was nothing to fix.
pub fn clear_readonly(handle: &OwnedHandle) -> io::Result<bool> {
    let attributes = handle_info(raw(handle))?.dwFileAttributes;
    if attributes & FILE_ATTRIBUTE_READONLY == 0 {
        return Ok(false);
    }
    let cleared = attributes & !FILE_ATTRIBUTE_READONLY;
    let info = FileBasicInfo {
        // Zero timestamps mean "leave unchanged".
        creation_time: 0,
        last_access_time: 0,
        last_write_time: 0,
        change_time: 0,
        file_attributes: if cleared == 0 {
            FILE_ATTRIBUTE_NORMAL
        } else {
            cleared
        },
    };
    set_info(
        handle,
        FILE_BASIC_INFO_CLASS,
        &info,
        std::mem::size_of_val(&info),
    )?;
    Ok(true)
}

/// Rename an open entry within its own directory (`new_name` is a bare
/// name). Needs `DELETE` access on the handle.
pub fn rename_in_place(handle: &OwnedHandle, new_name: &[u16]) -> io::Result<()> {
    // FILE_RENAME_INFO: { BOOLEAN ReplaceIfExists; HANDLE RootDirectory;
    // DWORD FileNameLength; WCHAR FileName[] } — name at byte offset 20.
    const NAME_LENGTH_OFFSET: usize = 16;
    const NAME_OFFSET: usize = 20;
    let name_bytes = new_name.len() * 2;
    let total = (NAME_OFFSET + name_bytes + 2).max(24);
    let mut buf = vec![0u64; total.div_ceil(8)];
    // SAFETY: `buf` holds `total` zeroed bytes, 8-byte aligned; the writes
    // below stay within it. ReplaceIfExists = 0, RootDirectory = null.
    unsafe {
        let base = buf.as_mut_ptr() as *mut u8;
        std::ptr::write(base.add(NAME_LENGTH_OFFSET) as *mut u32, name_bytes as u32);
        std::ptr::copy_nonoverlapping(
            new_name.as_ptr(),
            base.add(NAME_OFFSET) as *mut u16,
            new_name.len(),
        );
    }
    set_info(handle, FILE_RENAME_INFO_CLASS, &buf[0], buf.len() * 8)
}

/// Hard-link count of an open entry.
pub fn handle_link_count(handle: &OwnedHandle) -> io::Result<u32> {
    Ok(handle_info(raw(handle))?.nNumberOfLinks)
}

/// An open directory verified not to be a reparse point. Children are
/// opened *relative to this handle*, never by path, so replacing any path
/// component with a junction after the check cannot redirect a traversal
/// into another tree (which matters most for elevated runs).
pub struct DirHandle(HANDLE);

// SAFETY: a Win32 file handle may be used and closed from any thread.
unsafe impl Send for DirHandle {}
// SAFETY: every method only issues independent, thread-safe syscalls.
unsafe impl Sync for DirHandle {}

impl Drop for DirHandle {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by `self` and closed exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

impl DirHandle {
    /// Open the directory at `path` (the traversal root).
    pub fn open(path: &Path) -> io::Result<Self> {
        let wide = to_wide(path);
        // SAFETY: `wide` is NUL-terminated and outlives the call.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Self::verified(handle)
    }

    /// Open the child directory `name` of this directory.
    pub fn open_child(&self, name: &[u16]) -> io::Result<Self> {
        let handle = nt_open_relative(
            self.0,
            name,
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_DIRECTORY_FILE,
        )?;
        Self::verified(handle)
    }

    /// Open the entry `name` of this directory itself — a link is opened
    /// as the link, never followed. `is_dir` must match what the listing
    /// reported: an entry that changed type in between fails to open
    /// instead of being acted on.
    pub fn open_entry(&self, name: &[u16], access: u32, is_dir: bool) -> io::Result<OwnedHandle> {
        let kind = if is_dir {
            FILE_DIRECTORY_FILE
        } else {
            FILE_NON_DIRECTORY_FILE
        };
        let handle = nt_open_relative(
            self.0,
            name,
            access | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            kind,
        )?;
        // SAFETY: `handle` was just opened and is owned by nobody else.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle as _) })
    }

    fn verified(handle: HANDLE) -> io::Result<Self> {
        let dir = Self(handle); // closes the handle on every error path
        let attributes = handle_info(dir.0)?.dwFileAttributes;
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || attributes & FILE_ATTRIBUTE_DIRECTORY == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no longer a plain directory (replaced by a link?); not traversed",
            ));
        }
        Ok(dir)
    }

    /// Enumerate the direct children (excluding `.` and `..`) through this
    /// handle. Attributes and sizes come with the listing, as with
    /// [`for_each_dir_entry`].
    pub fn for_each_entry(&self, mut f: impl FnMut(RawDirEntry<'_>)) -> io::Result<()> {
        // 64 KiB, 8-byte aligned as FILE_FULL_DIR_INFO records require.
        let mut buf = vec![0u64; 8 * 1024];
        let buf_bytes = buf.len() * 8;
        loop {
            // SAFETY: `buf` is a writable buffer of `buf_bytes` bytes.
            let ok = unsafe {
                GetFileInformationByHandleEx(
                    self.0,
                    FILE_FULL_DIRECTORY_INFO_CLASS,
                    buf.as_mut_ptr() as *mut c_void,
                    buf_bytes as u32,
                )
            };
            if ok == 0 {
                let err = io::Error::last_os_error();
                return match err.raw_os_error() {
                    Some(ERROR_NO_MORE_FILES | ERROR_FILE_NOT_FOUND) => Ok(()),
                    _ => Err(err),
                };
            }
            let base = buf.as_ptr() as *const u8;
            let mut offset = 0usize;
            loop {
                if offset + FULL_DIR_INFO_NAME > buf_bytes {
                    break;
                }
                // SAFETY: the kernel filled `buf` with a chain of
                // FILE_FULL_DIR_INFO records; every read is bounds-checked
                // against the buffer and records are 8-byte aligned, so the
                // name (at offset 68) is suitably aligned for u16.
                let (next, entry) = unsafe {
                    let record = base.add(offset);
                    let read_u32 =
                        |at: usize| std::ptr::read_unaligned(record.add(at) as *const u32);
                    let next = read_u32(0) as usize;
                    let attributes = read_u32(FULL_DIR_INFO_ATTRIBUTES);
                    let name_len = read_u32(FULL_DIR_INFO_NAME_LENGTH) as usize / 2;
                    let size = std::ptr::read_unaligned(
                        record.add(FULL_DIR_INFO_END_OF_FILE) as *const i64
                    ) as u64;
                    if offset + FULL_DIR_INFO_NAME + name_len * 2 > buf_bytes {
                        break;
                    }
                    let name = std::slice::from_raw_parts(
                        record.add(FULL_DIR_INFO_NAME) as *const u16,
                        name_len,
                    );
                    (
                        next,
                        RawDirEntry {
                            name,
                            attributes,
                            size,
                        },
                    )
                };
                if !is_dot_entry(entry.name) {
                    f(entry);
                }
                if next == 0 {
                    break;
                }
                offset += next;
            }
        }
    }
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
/// * the file is still open in another process that *did* grant
///   `FILE_SHARE_DELETE` (common for indexers and scanners): POSIX semantics
///   unlink the name immediately instead of leaving a delete-pending entry
///   that keeps the parent directory non-empty;
/// * the readonly attribute is set on a file inside a non-writable directory
///   so `set_writable` itself fails.
///
/// It does **not** help when another process holds the file open without
/// `FILE_SHARE_DELETE`: opening for `DELETE` then fails with a sharing
/// violation, exactly like `remove_file`.
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
    // SAFETY: `handle` was just opened and is owned by nobody else; the
    // OwnedHandle closes it whatever the outcome.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle as _) };
    delete_posix(&handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("zap-winapi-test-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn junction(link: &Path, target: &Path) {
        let status = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(status.status.success());
    }

    fn wide_name(name: &str) -> Vec<u16> {
        std::ffi::OsStr::new(name).encode_wide().collect()
    }

    #[test]
    fn dir_handle_lists_children_like_find_first_file() {
        let dir = temp_dir();
        fs::write(dir.join("a.txt"), b"12345").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub").join("inner.txt"), b"x").unwrap();

        let collect = |handle: &DirHandle| {
            let mut entries = Vec::new();
            handle
                .for_each_entry(|e| {
                    entries.push((String::from_utf16_lossy(e.name), e.is_dir(), e.size))
                })
                .unwrap();
            entries.sort();
            entries
        };
        let root = DirHandle::open(&dir).unwrap();
        let entries = collect(&root);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], ("a.txt".to_owned(), false, 5));
        assert_eq!(entries[1].0, "sub");
        assert!(entries[1].1);

        let sub = root.open_child(&wide_name("sub")).unwrap();
        assert_eq!(collect(&sub), vec![("inner.txt".to_owned(), false, 1)]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_through_parent_handle_ignores_a_junction_swapped_in_later() {
        // The residual race of path-based deletion: the directory is
        // listed, then replaced by a junction to a victim tree holding a
        // file of the same name. Deleting relative to the handle must hit
        // the original directory, never the victim.
        let dir = temp_dir();
        let listed = dir.join("listed");
        let victim = dir.join("victim");
        fs::create_dir(&listed).unwrap();
        fs::create_dir(&victim).unwrap();
        fs::write(listed.join("same.txt"), b"tree").unwrap();
        fs::write(victim.join("same.txt"), b"PRECIOUS").unwrap();

        let handle = DirHandle::open(&listed).unwrap();
        let moved = dir.join("moved-away");
        fs::rename(&listed, &moved).unwrap();
        junction(&listed, &victim);

        let entry = handle
            .open_entry(&wide_name("same.txt"), ACCESS_DELETE, false)
            .unwrap();
        delete_posix(&entry)
            .or_else(|_| delete_legacy(&entry))
            .unwrap();
        drop(entry);

        assert_eq!(fs::read(victim.join("same.txt")).unwrap(), b"PRECIOUS");
        assert!(!moved.join("same.txt").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_entry_refuses_an_entry_that_changed_type() {
        let dir = temp_dir();
        fs::create_dir(dir.join("was-a-file")).unwrap();
        fs::write(dir.join("was-a-dir"), b"x").unwrap();
        let handle = DirHandle::open(&dir).unwrap();
        assert!(handle
            .open_entry(&wide_name("was-a-file"), ACCESS_DELETE, false)
            .is_err());
        assert!(handle
            .open_entry(&wide_name("was-a-dir"), ACCESS_DELETE, true)
            .is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn readonly_entry_is_deleted_after_clearing_the_attribute() {
        let dir = temp_dir();
        let file = dir.join("ro.txt");
        fs::write(&file, b"x").unwrap();
        let mut perms = fs::metadata(&file).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&file, perms).unwrap();

        let handle = DirHandle::open(&dir).unwrap();
        let entry = handle
            .open_entry(
                &wide_name("ro.txt"),
                ACCESS_DELETE | ACCESS_WRITE_ATTRIBUTES,
                false,
            )
            .unwrap();
        // The legacy path used on FAT/exFAT/network volumes.
        assert!(delete_legacy(&entry).is_err());
        assert!(clear_readonly(&entry).unwrap());
        delete_legacy(&entry).unwrap();
        drop(entry);
        assert!(!file.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dir_handle_refuses_to_traverse_junctions() {
        let dir = temp_dir();
        let target = dir.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep.txt"), b"x").unwrap();
        let tree = dir.join("tree");
        fs::create_dir(&tree).unwrap();
        junction(&tree.join("link"), &target);
        junction(&dir.join("root-link"), &target);

        let err = DirHandle::open(&dir.join("root-link"))
            .err()
            .expect("root junction");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let parent = DirHandle::open(&tree).unwrap();
        let err = parent
            .open_child(&wide_name("link"))
            .err()
            .expect("child junction");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        let err = parent
            .open_child(&wide_name("missing"))
            .err()
            .expect("missing");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        let _ = fs::remove_dir_all(&dir);
    }
}
