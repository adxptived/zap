use std::{
    io,
    path::{Path, PathBuf},
};

#[cfg(test)]
pub fn is_filesystem_root(path: &Path) -> io::Result<bool> {
    let metadata = std::fs::symlink_metadata(path)?;

    if metadata.file_type().is_symlink() {
        return Ok(false);
    }

    let canonical_path = std::fs::canonicalize(path)?;

    Ok(canonical_path.parent().is_none())
}

pub fn sanitize_path(path: &Path) -> io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    let canonical = strip_verbatim(canonical);
    Ok(canonical)
}

/// On Windows, canonicalize returns a verbatim path (`\\?\C:\...` or
/// `\\?\UNC\server\share\...`). Strip the prefix so comparisons in
/// `is_protected_path` work and `parent()` reports volume roots correctly.
#[cfg(windows)]
fn strip_verbatim(canonical: PathBuf) -> PathBuf {
    let s = canonical.to_string_lossy();
    if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{unc}"));
    }
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(stripped);
    }
    canonical
}

/// Metadata and canonical identity captured at the destructive API boundary.
/// Symlinks are intentionally not canonicalized: deleting a link must never
/// turn into deleting its target.
pub struct ValidatedDeleteTarget {
    pub metadata: std::fs::Metadata,
    pub canonical: Option<PathBuf>,
}

/// Validate a target immediately before a destructive operation.
///
/// All public deletion paths use this function so callers cannot accidentally
/// bypass filesystem-root and protected-path checks. Callers must still use
/// the original path for the actual unlink, preserving symlink semantics.
pub fn validate_delete_target(
    path: &Path,
    allow_dangerous: bool,
) -> io::Result<ValidatedDeleteTarget> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(ValidatedDeleteTarget {
            metadata,
            canonical: None,
        });
    }

    let canonical = sanitize_path(path)?;
    if canonical.parent().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to delete filesystem root: {}", path.display()),
        ));
    }
    if is_protected_path(&canonical) && !allow_dangerous {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refusing to delete dangerous path without extra confirmation: {}",
                path.display()
            ),
        ));
    }

    Ok(ValidatedDeleteTarget {
        metadata,
        canonical: Some(canonical),
    })
}

/// Resolve whether a target needs the explicit dangerous-path confirmation.
/// Missing/unreadable targets are not classified here; destructive APIs still
/// return their concrete validation error when the operation starts.
pub fn is_dangerous_target(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if metadata.file_type().is_symlink() {
        return false;
    }
    sanitize_path(path).is_ok_and(|canonical| is_protected_path(&canonical))
}

#[cfg(windows)]
struct ProtectedConfig {
    /// Prefixes (lower-cased once at init) that are fully protected: any
    /// path equal to or beneath one of these is refused.
    prefixes: Vec<String>,
    /// Index inside `prefixes` of the `<SystemDrive>\Users` entry. The Users
    /// branch has special-case logic that allows deep user content while
    /// still protecting well-known profile subdirs.
    users_index: usize,
    /// Folders protected themselves (their contents stay deletable): the
    /// current profile and its well-known folders wherever they really
    /// live — a profile outside `<SystemDrive>\Users`, Documents redirected
    /// to another drive, OneDrive. Lower-cased.
    exact: Vec<String>,
}

#[cfg(windows)]
const PROTECTED_USER_SUBDIRS: &[&str] = &[
    "appdata",
    r"appdata\local",
    r"appdata\local\temp",
    r"appdata\roaming",
    r"appdata\locallow",
    "desktop",
    "documents",
    "downloads",
    "pictures",
    "music",
    "videos",
    "onedrive",
    "saved games",
    "favorites",
    "links",
    "contacts",
    "searches",
    ".ssh",
];

/// `{profile}`, `{profile}\<each PROTECTED_USER_SUBDIRS>` and the extra
/// folders, lower-cased, for [`ProtectedConfig::exact`].
#[cfg(windows)]
fn exact_entries(profile: Option<&str>, extra: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut exact: Vec<String> = Vec::new();
    if let Some(profile) = profile.filter(|p| !p.is_empty()) {
        let profile = profile.trim_end_matches('\\');
        exact.push(profile.to_string());
        for sub in PROTECTED_USER_SUBDIRS {
            exact.push(format!(r"{profile}\{sub}"));
        }
    }
    exact.extend(extra.into_iter().filter(|p| !p.is_empty()));
    for entry in &mut exact {
        *entry = entry.to_lowercase();
    }
    exact.sort();
    exact.dedup();
    exact
}

#[cfg(windows)]
mod known_folders {
    use std::ffi::c_void;

    #[repr(C)]
    pub struct Guid(u32, u16, u16, [u8; 8]);

    pub const DESKTOP: Guid = Guid(
        0xB4BFCC3A,
        0xDB2C,
        0x424C,
        [0xB0, 0x29, 0x7F, 0xE9, 0x9A, 0x87, 0xC6, 0x41],
    );
    pub const DOCUMENTS: Guid = Guid(
        0xFDD39AD0,
        0x238F,
        0x46AF,
        [0xAD, 0xB4, 0x6C, 0x85, 0x48, 0x03, 0x69, 0xC7],
    );
    pub const DOWNLOADS: Guid = Guid(
        0x374DE290,
        0x123F,
        0x4565,
        [0x91, 0x64, 0x39, 0xC4, 0x92, 0x5E, 0x46, 0x7B],
    );
    pub const PICTURES: Guid = Guid(
        0x33E28130,
        0x4E1E,
        0x4676,
        [0x83, 0x5A, 0x98, 0x39, 0x5C, 0x3B, 0xC3, 0xBB],
    );
    pub const MUSIC: Guid = Guid(
        0x4BD8D571,
        0x6D19,
        0x48D3,
        [0xBE, 0x97, 0x42, 0x22, 0x20, 0x08, 0x0E, 0x43],
    );
    pub const VIDEOS: Guid = Guid(
        0x18989B1D,
        0x99B5,
        0x455B,
        [0x84, 0x1C, 0xAB, 0x7C, 0x74, 0xE4, 0xDD, 0xFC],
    );
    pub const ROAMING_APP_DATA: Guid = Guid(
        0x3EB685DB,
        0x65F9,
        0x4CF6,
        [0xA0, 0x3A, 0xE3, 0xEF, 0x65, 0x72, 0x9F, 0x3D],
    );
    pub const LOCAL_APP_DATA: Guid = Guid(
        0xF1B32785,
        0x6FBA,
        0x4FCF,
        [0x9D, 0x55, 0x7B, 0x8E, 0x7F, 0x15, 0x70, 0x91],
    );
    pub const ONEDRIVE: Guid = Guid(
        0xA52BBA46,
        0xE9E1,
        0x435F,
        [0xB3, 0xD9, 0x28, 0xDA, 0xA6, 0x48, 0xC0, 0xF6],
    );
    pub const SAVED_GAMES: Guid = Guid(
        0x4C5C32FF,
        0xBB9D,
        0x43B0,
        [0xB5, 0xB4, 0x2D, 0x72, 0xE5, 0x4E, 0xAA, 0xA4],
    );
    pub const FAVORITES: Guid = Guid(
        0x1777F761,
        0x68AD,
        0x4D8A,
        [0x87, 0xBD, 0x30, 0xB7, 0x59, 0xFA, 0x33, 0xDD],
    );

    pub const ALL: [&Guid; 11] = [
        &DESKTOP,
        &DOCUMENTS,
        &DOWNLOADS,
        &PICTURES,
        &MUSIC,
        &VIDEOS,
        &ROAMING_APP_DATA,
        &LOCAL_APP_DATA,
        &ONEDRIVE,
        &SAVED_GAMES,
        &FAVORITES,
    ];

    #[link(name = "shell32")]
    extern "system" {
        fn SHGetKnownFolderPath(
            id: *const Guid,
            flags: u32,
            token: *mut c_void,
            path: *mut *mut u16,
        ) -> i32;
    }
    #[link(name = "ole32")]
    extern "system" {
        fn CoTaskMemFree(pv: *mut c_void);
    }

    /// Current location of a known folder, or `None` if it has none.
    pub fn path(id: &Guid) -> Option<String> {
        let mut raw: *mut u16 = std::ptr::null_mut();
        // SAFETY: valid GUID and out-pointer; no access token (current user).
        let hr = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut raw) };
        let result = (hr >= 0 && !raw.is_null()).then(|| {
            // SAFETY: on success `raw` is a NUL-terminated UTF-16 string.
            unsafe {
                let len = (0..).take_while(|&i| *raw.add(i) != 0).count();
                String::from_utf16_lossy(std::slice::from_raw_parts(raw, len))
            }
        });
        // SAFETY: the buffer must be freed whatever the result; null is fine.
        unsafe { CoTaskMemFree(raw as *mut c_void) };
        result
    }
}

#[cfg(windows)]
fn protected_config() -> &'static ProtectedConfig {
    use std::sync::OnceLock;
    static CACHE: OnceLock<ProtectedConfig> = OnceLock::new();
    CACHE.get_or_init(|| {
        use std::env;

        let system_root = env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        let program_files =
            env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".to_string());
        let program_files_x86 =
            env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".to_string());
        let program_data =
            env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".to_string());
        let system_drive = env::var("SystemDrive").unwrap_or_else(|_| r"C:".to_string());

        let users_dir = format!(r"{}\Users", system_drive);
        let svi_dir = format!(r"{}\System Volume Information", system_drive);
        let recovery_dir = format!(r"{}\Recovery", system_drive);

        // Users sits at index 4 — keep this in sync with the array below.
        let raw = [
            system_root,
            program_files,
            program_files_x86,
            program_data,
            users_dir,
            svi_dir,
            recovery_dir,
        ];
        let users_index = 4;
        let prefixes: Vec<String> = raw.iter().map(|s| s.to_lowercase()).collect();

        let profile = env::var("USERPROFILE").ok();
        let extra = known_folders::ALL
            .iter()
            .filter_map(|id| known_folders::path(id))
            .chain(
                ["OneDrive", "OneDriveCommercial", "OneDriveConsumer"]
                    .iter()
                    .filter_map(|var| env::var(var).ok()),
            );
        ProtectedConfig {
            prefixes,
            users_index,
            exact: exact_entries(profile.as_deref(), extra),
        }
    })
}

/// ASCII-only case-insensitive byte comparison. Sufficient for Windows path
/// prefixes (drive letters, system folder names, well-known profile subdirs
/// are all ASCII). Avoids the per-call `String` allocations that
/// `str::to_lowercase()` requires for Unicode-correct folding.
#[cfg(windows)]
#[inline]
fn ascii_ci_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.eq_ignore_ascii_case(b)
}

#[cfg(windows)]
#[inline]
fn ascii_ci_starts_with(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

/// Compare two backslash-separated paths component-wise, ASCII
/// case-insensitively, ignoring empty components (doubled separators).
/// Zero allocations — replaces the collect-and-join approach.
#[cfg(windows)]
fn components_ci_eq(a: &[u8], b: &[u8]) -> bool {
    let mut ai = a.split(|&c| c == b'\\').filter(|s| !s.is_empty());
    let mut bi = b.split(|&c| c == b'\\').filter(|s| !s.is_empty());
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if x.eq_ignore_ascii_case(y) => {}
            _ => return false,
        }
    }
}

#[cfg(windows)]
pub fn is_protected_path(path: &Path) -> bool {
    is_protected_with(protected_config(), path)
}

#[cfg(windows)]
fn is_protected_with(cfg: &ProtectedConfig, path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    let path_bytes = path_str.as_bytes();

    if cfg
        .exact
        .iter()
        .any(|folder| components_ci_eq(path_bytes, folder.as_bytes()))
    {
        return true;
    }

    for (i, prefix) in cfg.prefixes.iter().enumerate() {
        let pb = prefix.as_bytes();
        let exact = ascii_ci_eq(path_bytes, pb);
        let with_sep = path_bytes.len() > pb.len()
            && ascii_ci_starts_with(path_bytes, pb)
            && path_bytes[pb.len()] == b'\\';

        if !exact && !with_sep {
            continue;
        }

        if i == cfg.users_index {
            // Special case: <SystemDrive>\Users
            //   Users itself or Users\<name>           -> protected (depth ≤ 1)
            //   Users\<name>\<well-known-subdir>       -> protected
            //   Users\<name>\<well-known-subdir>\…     -> allowed
            //   Users\<name>\<other>                   -> allowed (deep content)
            if exact {
                return true;
            }
            let rest = &path_bytes[pb.len() + 1..];
            // Skip the first component (the user name); `user_rel` is the
            // remainder relative to the user's profile folder.
            let mut comps = rest.split(|&b| b == b'\\').filter(|s| !s.is_empty());
            if comps.next().is_none() {
                return true; // "Users\" with only empty components
            }
            let Some(first_rel) = comps.next() else {
                return true; // Users\<name> (depth 1)
            };
            // Rebuild the relative slice without allocating: it starts at
            // `first_rel` and runs to the end of `rest`.
            let start = first_rel.as_ptr() as usize - rest.as_ptr() as usize;
            let user_rel = &rest[start..];
            if PROTECTED_USER_SUBDIRS
                .iter()
                .any(|dir| components_ci_eq(user_rel, dir.as_bytes()))
            {
                return true;
            }
            continue;
        }

        return true;
    }

    false
}

#[cfg(not(windows))]
pub fn is_protected_path(path: &Path) -> bool {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    is_protected_unix(path, home.as_deref())
}

/// Unix system trees: protected together with everything inside them.
const UNIX_PROTECTED_TREES: &[&str] = &[
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/proc",
    "/run",
    "/sbin",
    "/sys",
    "/usr",
    "/System",
    "/private/etc",
];

/// Unix folders protected themselves; their contents stay deletable.
const UNIX_PROTECTED_FOLDERS: &[&str] = &[
    "/home",
    "/root",
    "/opt",
    "/srv",
    "/mnt",
    "/media",
    "/tmp",
    "/var",
    "/var/lib",
    "/var/log",
    "/var/tmp",
    "/Users",
    "/Library",
    "/Applications",
    "/Volumes",
    "/private",
    "/private/tmp",
    "/private/var",
];

/// Folders of the user's home protected themselves.
const UNIX_HOME_FOLDERS: &[&str] = &[
    "Desktop",
    "Documents",
    "Downloads",
    "Pictures",
    "Music",
    "Videos",
    "Movies",
    "Public",
    "Templates",
    ".ssh",
    ".gnupg",
    ".config",
    ".local",
    ".local/share",
    "Library",
];

/// Pure core of the Unix check (`path` is canonical). Compiled on every
/// platform so its tests run everywhere; only non-Windows builds call it.
#[cfg_attr(windows, allow(dead_code))]
fn is_protected_unix(path: &Path, home: Option<&Path>) -> bool {
    if UNIX_PROTECTED_TREES
        .iter()
        .any(|tree| path.starts_with(tree))
    {
        return true;
    }
    if UNIX_PROTECTED_FOLDERS
        .iter()
        .any(|dir| path == Path::new(dir))
    {
        return true;
    }
    home.filter(|home| home.parent().is_some())
        .is_some_and(|home| {
            path == home || UNIX_HOME_FOLDERS.iter().any(|dir| path == home.join(dir))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn create_test_dir() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = std::env::temp_dir().join(format!("zap-protect-test-{}", id));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();
        temp
    }

    #[test]
    #[cfg(windows)] // exercises Windows drive paths; is_protected_path is a no-op elsewhere
    fn test_is_filesystem_root_detects_root() {
        let root = Path::new("C:\\");
        assert!(is_filesystem_root(root).unwrap());
    }

    #[test]
    fn test_is_filesystem_root_non_root() {
        let temp = create_test_dir();
        assert!(!is_filesystem_root(&temp).unwrap());
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_is_filesystem_root_symlink_not_root() {
        let temp = create_test_dir();
        let target = temp.join("target");
        fs::create_dir(&target).unwrap();

        let link = temp.join("link");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&target, &link).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(!is_filesystem_root(&link).unwrap());
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    #[cfg(windows)]
    fn test_strip_verbatim_prefix() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\dir")),
            PathBuf::from(r"C:\dir")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\UNC\server\share\dir")),
            PathBuf::from(r"\\server\share\dir")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"C:\plain")),
            PathBuf::from(r"C:\plain")
        );
    }

    #[test]
    #[cfg(windows)]
    fn test_network_share_root_is_filesystem_root() {
        // `\\server\share\` (and a mapped drive letter, which canonicalizes
        // to it) is the root of a remote volume and must be refused.
        assert!(strip_verbatim(PathBuf::from(r"\\?\UNC\server\share\"))
            .parent()
            .is_none());
        let share = Path::new(r"\\localhost\C$\");
        if share.exists() {
            let err = validate_delete_target(share, true)
                .err()
                .expect("must refuse");
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn test_sanitize_path_resolves_canonical() {
        let temp = create_test_dir();
        let canonical = sanitize_path(&temp).unwrap();
        assert!(canonical.is_absolute());
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    #[cfg(windows)] // exercises Windows drive paths; is_protected_path is a no-op elsewhere
    fn test_is_protected_path_windows_dirs() {
        assert!(is_protected_path(Path::new(r"C:\Windows")));
        assert!(is_protected_path(Path::new(r"C:\Program Files")));
        assert!(is_protected_path(Path::new(r"C:\ProgramData")));
    }

    #[test]
    #[cfg(windows)] // exercises Windows drive paths; is_protected_path is a no-op elsewhere
    fn test_is_protected_path_blocks_user_profile_dirs() {
        assert!(is_protected_path(Path::new(r"C:\Users\test\AppData")));
        assert!(is_protected_path(Path::new(r"C:\Users\test\AppData\Local")));
        assert!(is_protected_path(Path::new(
            r"C:\Users\test\AppData\Local\Temp"
        )));
        assert!(is_protected_path(Path::new(
            r"C:\Users\test\AppData\Roaming"
        )));
        assert!(is_protected_path(Path::new(
            r"C:\Users\test\AppData\LocalLow"
        )));
        assert!(is_protected_path(Path::new(r"C:\Users\test\Desktop")));
        assert!(is_protected_path(Path::new(r"C:\Users\test\Documents")));
        assert!(is_protected_path(Path::new(r"C:\Users\test\Downloads")));
        assert!(is_protected_path(Path::new(r"C:\Users\test\Pictures")));
        assert!(is_protected_path(Path::new(r"C:\Users\test\Music")));
        assert!(is_protected_path(Path::new(r"C:\Users\test\Videos")));
    }

    #[test]
    fn test_is_protected_path_allows_inside_temp() {
        assert!(!is_protected_path(Path::new(
            r"C:\Users\test\AppData\Local\Temp\somefolder"
        )));
        assert!(!is_protected_path(Path::new(
            r"C:\Users\test\AppData\Local\Temp\a\b\c"
        )));
    }

    #[test]
    fn test_is_protected_path_allows_deep_user_content() {
        assert!(!is_protected_path(Path::new(
            r"C:\Users\test\Documents\myproject"
        )));
        assert!(!is_protected_path(Path::new(
            r"C:\Users\test\Desktop\folder"
        )));
        assert!(!is_protected_path(Path::new(
            r"C:\Users\test\AppData\Roaming\SomeApp"
        )));
        assert!(!is_protected_path(Path::new(
            r"C:\Users\test\somecustomdir"
        )));
    }

    #[test]
    #[cfg(windows)]
    fn test_is_protected_path_blocks_more_profile_folders() {
        for sub in ["OneDrive", "Saved Games", "Favorites", "Links", ".ssh"] {
            let path = format!(r"C:\Users\test\{sub}");
            assert!(is_protected_path(Path::new(&path)), "{path}");
        }
        assert!(!is_protected_path(Path::new(
            r"C:\Users\test\OneDrive\old-project"
        )));
    }

    #[test]
    #[cfg(windows)]
    fn test_relocated_profile_and_redirected_folders_are_protected() {
        // A profile outside <SystemDrive>\Users and a Documents folder
        // redirected to another drive must be protected themselves, while
        // their contents stay deletable.
        let cfg = ProtectedConfig {
            prefixes: vec![r"c:\windows".into(), r"c:\users".into()],
            users_index: 1,
            exact: exact_entries(Some(r"D:\Profiles\Bob"), [r"E:\Docs".to_string()]),
        };
        assert!(is_protected_with(&cfg, Path::new(r"D:\Profiles\Bob")));
        assert!(is_protected_with(
            &cfg,
            Path::new(r"d:\profiles\bob\Desktop")
        ));
        assert!(is_protected_with(
            &cfg,
            Path::new(r"D:\Profiles\Bob\AppData\Local")
        ));
        assert!(is_protected_with(&cfg, Path::new(r"E:\Docs")));
        assert!(!is_protected_with(&cfg, Path::new(r"E:\Docs\project")));
        assert!(!is_protected_with(&cfg, Path::new(r"D:\Profiles\Bob\code")));
        assert!(!is_protected_with(&cfg, Path::new(r"D:\Profiles")));
    }

    #[test]
    #[cfg(windows)]
    fn test_known_folders_of_current_user_are_protected() {
        let documents = known_folders::path(&known_folders::DOCUMENTS).expect("Documents");
        assert!(is_protected_path(Path::new(&documents)), "{documents}");
    }

    #[test]
    fn test_unix_protection_rules() {
        let home = Some(Path::new("/home/bob"));
        let protected = |p: &str| is_protected_unix(Path::new(p), home);
        // System trees, including their contents.
        assert!(protected("/usr"));
        assert!(protected("/usr/lib/libc.so.6"));
        assert!(protected("/etc/passwd"));
        assert!(protected("/boot"));
        // Folders protected themselves, contents deletable.
        assert!(protected("/home"));
        assert!(protected("/var"));
        assert!(protected("/tmp"));
        assert!(!protected("/tmp/build-cache"));
        assert!(!protected("/var/cache/apt/archives"));
        // The user's home and its well-known folders.
        assert!(protected("/home/bob"));
        assert!(protected("/home/bob/Documents"));
        assert!(protected("/home/bob/.ssh"));
        assert!(protected("/home/bob/.local/share"));
        assert!(!protected("/home/bob/Documents/old-project"));
        assert!(!protected("/home/bob/code/node_modules"));
        // Component-wise, not textual: /usrdata is not /usr.
        assert!(!protected("/usrdata"));
        // A HOME of "/" protects nothing extra.
        assert!(!is_protected_unix(Path::new("/data"), Some(Path::new("/"))));
    }

    #[test]
    #[cfg(windows)] // exercises Windows drive paths; is_protected_path is a no-op elsewhere
    fn test_is_protected_path_blocks_users_root() {
        assert!(is_protected_path(Path::new(r"C:\Users")));
    }

    #[test]
    fn test_is_protected_path_allows_non_system_dir() {
        let temp = create_test_dir();
        assert!(!is_protected_path(&temp));
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    #[cfg(windows)] // exercises Windows drive paths; is_protected_path is a no-op elsewhere
    fn test_delete_path_refuses_protected_path() {
        let canonical = PathBuf::from(r"C:\Windows");
        assert!(is_protected_path(&canonical));
    }
}
