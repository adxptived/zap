//! "YOLO" escalation: the aggressive last-resort strategies used when a
//! delete keeps failing with `ACCESS_DENIED` even after the POSIX
//! force-delete in [`crate::winapi::force_delete`].
//!
//! Two escalations live here, tried in order by [`escalate_delete`]:
//!
//! 1. **Take ownership + grant `DELETE`.** When a file's DACL denies the
//!    current user (common for files left behind by another account, or
//!    `TrustedInstaller`-owned leftovers), we enable the take-ownership /
//!    restore privileges, set ourselves as the owner, then rewrite the DACL
//!    to grant full control. This only works when the process runs
//!    elevated — otherwise `SetNamedSecurityInfoW` returns `ACCESS_DENIED`
//!    and we fall through.
//!
//! 2. **Schedule deletion on reboot.** A file genuinely held open by the OS
//!    (pending-rename targets, in-use drivers) cannot be unlinked live.
//!    `MoveFileExW(..., MOVEFILE_DELAY_UNTIL_REBOOT)` registers it for
//!    removal during early boot before most handles are held.
//!
//! IMPORTANT: this module never bypasses [`crate::protect`]. Callers still
//! refuse protected system paths before any of this runs — YOLO breaks
//! locks and permissions on the *user's own* files, not the OS install.
//!
//! All FFI is `#[cfg(windows)]`. `advapi32` is linked explicitly (unlike
//! `kernel32`, it is not linked by default).
#![cfg(windows)]

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::Once;

// ---------- Win32 type / constant declarations ----------------------------

#[allow(clippy::upper_case_acronyms, non_camel_case_types)]
type HANDLE = *mut c_void;
#[allow(clippy::upper_case_acronyms, non_camel_case_types)]
type BOOL = i32;
#[allow(clippy::upper_case_acronyms, non_camel_case_types)]
type PSID = *mut c_void;
#[allow(clippy::upper_case_acronyms, non_camel_case_types)]
type PACL = *mut c_void;

/// `SE_FILE_OBJECT` — the named object is a file/directory path.
const SE_FILE_OBJECT: i32 = 1;
const OWNER_SECURITY_INFORMATION: u32 = 0x0000_0001;
const DACL_SECURITY_INFORMATION: u32 = 0x0000_0004;

const TOKEN_ADJUST_PRIVILEGES: u32 = 0x0020;
const TOKEN_QUERY: u32 = 0x0008;
const SE_PRIVILEGE_ENABLED: u32 = 0x0000_0002;

/// `TokenUser` value of the `TOKEN_INFORMATION_CLASS` enum.
const TOKEN_USER_CLASS: i32 = 1;

/// `GENERIC_ALL` — full control in the generic rights mapping.
const GENERIC_ALL: u32 = 0x1000_0000;
/// `SET_ACCESS` — the ACE replaces any existing rights for the trustee.
const SET_ACCESS: u32 = 2;
const NO_INHERITANCE: u32 = 0;
/// `TRUSTEE_IS_SID` — `ptstrName` is actually a `PSID`.
const TRUSTEE_IS_SID: u32 = 0;
const TRUSTEE_IS_UNKNOWN: u32 = 0;
const NO_MULTIPLE_TRUSTEE: u32 = 0;

const MOVEFILE_DELAY_UNTIL_REBOOT: u32 = 0x0000_0004;

const ERROR_SUCCESS: u32 = 0;

#[repr(C)]
#[allow(non_snake_case)]
struct Luid {
    LowPart: u32,
    HighPart: i32,
}

#[repr(C)]
#[allow(non_snake_case)]
struct LuidAndAttributes {
    Luid: Luid,
    Attributes: u32,
}

#[repr(C)]
#[allow(non_snake_case)]
struct TokenPrivileges {
    PrivilegeCount: u32,
    Privileges: [LuidAndAttributes; 1],
}

#[repr(C)]
#[allow(non_snake_case)]
struct SidAndAttributes {
    Sid: PSID,
    Attributes: u32,
}

#[repr(C)]
#[allow(non_snake_case)]
struct TokenUser {
    User: SidAndAttributes,
}

#[repr(C)]
#[allow(non_snake_case)]
struct TrusteeW {
    pMultipleTrustee: *mut c_void,
    MultipleTrusteeOperation: u32,
    TrusteeForm: u32,
    TrusteeType: u32,
    ptstrName: *mut u16,
}

#[repr(C)]
#[allow(non_snake_case)]
struct ExplicitAccessW {
    grfAccessPermissions: u32,
    grfAccessMode: u32,
    grfInheritance: u32,
    Trustee: TrusteeW,
}

extern "system" {
    fn GetCurrentProcess() -> HANDLE;
    fn CloseHandle(hObject: HANDLE) -> BOOL;
    fn LocalFree(hMem: *mut c_void) -> *mut c_void;
    fn MoveFileExW(lpExistingFileName: *const u16, lpNewFileName: *const u16, dwFlags: u32)
        -> BOOL;
}

#[link(name = "advapi32")]
extern "system" {
    fn OpenProcessToken(
        ProcessHandle: HANDLE,
        DesiredAccess: u32,
        TokenHandle: *mut HANDLE,
    ) -> BOOL;
    fn LookupPrivilegeValueW(
        lpSystemName: *const u16,
        lpName: *const u16,
        lpLuid: *mut Luid,
    ) -> BOOL;
    fn AdjustTokenPrivileges(
        TokenHandle: HANDLE,
        DisableAllPrivileges: BOOL,
        NewState: *const TokenPrivileges,
        BufferLength: u32,
        PreviousState: *mut c_void,
        ReturnLength: *mut u32,
    ) -> BOOL;
    fn GetTokenInformation(
        TokenHandle: HANDLE,
        TokenInformationClass: i32,
        TokenInformation: *mut c_void,
        TokenInformationLength: u32,
        ReturnLength: *mut u32,
    ) -> BOOL;
    fn SetNamedSecurityInfoW(
        pObjectName: *mut u16,
        ObjectType: i32,
        SecurityInfo: u32,
        psidOwner: PSID,
        psidGroup: PSID,
        pDacl: PACL,
        pSacl: PACL,
    ) -> u32;
    fn SetEntriesInAclW(
        cCountOfExplicitEntries: u32,
        pListOfExplicitEntries: *mut ExplicitAccessW,
        OldAcl: PACL,
        NewAcl: *mut PACL,
    ) -> u32;
}

// ---------- Helpers --------------------------------------------------------

/// Encode `path` as a NUL-terminated UTF-16 buffer. Unlike
/// [`crate::winapi`], the security APIs (`SetNamedSecurityInfoW`) do not
/// reliably accept the verbatim `\\?\` prefix, so we pass the plain path.
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Enable the privileges that let an elevated process seize objects it does
/// not own. Idempotent (guarded by a `Once`) and best-effort: on a
/// non-elevated token the adjustments silently no-op and later security
/// calls fail with `ACCESS_DENIED`, which the caller handles.
fn enable_privileges() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: pseudo-handle, never closed; token handle closed below.
        let process = unsafe { GetCurrentProcess() };
        let mut token: HANDLE = std::ptr::null_mut();
        let opened = unsafe {
            OpenProcessToken(
                process,
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            )
        };
        if opened == 0 || token.is_null() {
            return;
        }
        for name in [
            "SeTakeOwnershipPrivilege",
            "SeRestorePrivilege",
            "SeBackupPrivilege",
            "SeSecurityPrivilege",
        ] {
            enable_one(token, name);
        }
        // SAFETY: `token` came from OpenProcessToken above.
        unsafe { CloseHandle(token) };
    });
}

fn enable_one(token: HANDLE, name: &str) {
    let wide_name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut luid = Luid {
        LowPart: 0,
        HighPart: 0,
    };
    // SAFETY: `wide_name` is NUL-terminated; `luid` is a valid out-param.
    if unsafe { LookupPrivilegeValueW(std::ptr::null(), wide_name.as_ptr(), &mut luid) } == 0 {
        return;
    }
    let tp = TokenPrivileges {
        PrivilegeCount: 1,
        Privileges: [LuidAndAttributes {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: `tp` outlives the call; other pointers are null (allowed).
    unsafe {
        AdjustTokenPrivileges(
            token,
            0,
            &tp,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
}

/// Fetch the current process user's SID into a heap buffer. The returned
/// `Vec` owns the SID bytes; the `PSID` handed to Win32 calls points into
/// it, so the buffer must outlive those calls.
fn current_user_sid() -> io::Result<Vec<u8>> {
    // SAFETY: pseudo-handle for the current process.
    let process = unsafe { GetCurrentProcess() };
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }

    // Two-call idiom: first learn the required size, then fetch.
    let mut needed: u32 = 0;
    // SAFETY: passing a null buffer with len 0 asks for the size in `needed`.
    unsafe {
        GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 {
        let err = io::Error::last_os_error();
        unsafe { CloseHandle(token) };
        return Err(err);
    }

    let mut buf = vec![0u8; needed as usize];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TOKEN_USER_CLASS,
            buf.as_mut_ptr() as *mut c_void,
            needed,
            &mut needed,
        )
    };
    let err = (ok == 0).then(io::Error::last_os_error);
    // SAFETY: token from OpenProcessToken above.
    unsafe { CloseHandle(token) };
    match err {
        Some(e) => Err(e),
        None => Ok(buf),
    }
}

/// Take ownership of `path` and grant the current user full control.
///
/// Requires an elevated token; returns the `ACCESS_DENIED` (or other) error
/// verbatim when the process lacks the necessary privileges so the caller
/// can decide whether to fall through to reboot scheduling.
fn take_ownership_and_grant(path: &Path) -> io::Result<()> {
    enable_privileges();
    let sid_buf = current_user_sid()?;
    // SAFETY: `sid_buf` holds a valid TOKEN_USER; `.User.Sid` points inside
    // the same allocation and stays valid for as long as `sid_buf` lives.
    let sid: PSID = unsafe { (*(sid_buf.as_ptr() as *const TokenUser)).User.Sid };

    let mut name = wide(path);

    // Step 1: become the owner. With SeTakeOwnershipPrivilege enabled this
    // succeeds even when the current DACL grants us nothing.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            name.as_mut_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            sid,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }

    // Step 2: as owner we implicitly hold WRITE_DAC, so rewrite the DACL to
    // grant ourselves GENERIC_ALL (which includes DELETE).
    let mut ea = ExplicitAccessW {
        grfAccessPermissions: GENERIC_ALL,
        grfAccessMode: SET_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: TrusteeW {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid as *mut u16,
        },
    };
    let mut new_acl: PACL = std::ptr::null_mut();
    // SAFETY: single valid EXPLICIT_ACCESS entry; `new_acl` is an out-param
    // that receives a LocalAlloc'd ACL we free below.
    let rc = unsafe { SetEntriesInAclW(1, &mut ea, std::ptr::null_mut(), &mut new_acl) };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }

    let rc = unsafe {
        SetNamedSecurityInfoW(
            name.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_acl,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: `new_acl` was allocated by SetEntriesInAclW; free it exactly
    // once regardless of the set outcome.
    if !new_acl.is_null() {
        unsafe { LocalFree(new_acl as *mut c_void) };
    }
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

/// Register `path` for deletion during the next boot. Best-effort last
/// resort for files the OS holds open right now.
fn schedule_delete_on_reboot(path: &Path) -> io::Result<()> {
    let name = wide(path);
    // SAFETY: `name` is NUL-terminated; a null target means "delete".
    let ok = unsafe {
        MoveFileExW(
            name.as_ptr(),
            std::ptr::null(),
            MOVEFILE_DELAY_UNTIL_REBOOT,
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Outcome of a YOLO escalation attempt on a single path.
pub enum YoloOutcome {
    /// The path is gone from disk now.
    Deleted,
    /// The path could not be removed live but is registered for deletion on
    /// the next reboot.
    ScheduledForReboot,
    /// Every strategy failed; the wrapped error is the most useful one.
    Failed(io::Error),
}

/// Aggressive delete escalation: take ownership + grant DELETE, retry the
/// POSIX force-delete, and finally schedule removal on reboot.
///
/// Callers must have already exhausted the normal retries and confirmed the
/// path is not protected.
pub fn escalate_delete(path: &Path) -> YoloOutcome {
    // Grabbing ownership/rights is what unblocks the common ACCESS_DENIED
    // case. Ignore its error here: even if it fails (non-elevated), the
    // force-delete below may still work, and reboot scheduling is the final
    // safety net.
    let ownership = take_ownership_and_grant(path);

    if crate::winapi::force_delete(path).is_ok() {
        return YoloOutcome::Deleted;
    }
    // A plain remove can now succeed if ownership/DACL were fixed but the
    // file predates POSIX-delete support.
    if std::fs::remove_file(path)
        .or_else(|_| std::fs::remove_dir(path))
        .is_ok()
    {
        return YoloOutcome::Deleted;
    }

    match schedule_delete_on_reboot(path) {
        Ok(()) => YoloOutcome::ScheduledForReboot,
        Err(reboot_err) => YoloOutcome::Failed(ownership.err().unwrap_or(reboot_err)),
    }
}
