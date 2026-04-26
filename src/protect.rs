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
    // On Windows, canonicalize returns a UNC path like \\?\C:\...
    // Strip the \\?\ prefix so that path comparisons in is_protected_path work.
    #[cfg(windows)]
    {
        let s = canonical.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return Ok(PathBuf::from(stripped));
        }
    }
    Ok(canonical)
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
];

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
        ProtectedConfig {
            prefixes,
            users_index,
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

#[cfg(windows)]
pub fn is_protected_path(path: &Path) -> bool {
    let cfg = protected_config();
    let path_str = path.to_string_lossy();
    let path_bytes = path_str.as_bytes();

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
            let components: Vec<&[u8]> = rest
                .split(|&b| b == b'\\')
                .filter(|s| !s.is_empty())
                .collect();
            if components.len() <= 1 {
                return true;
            }
            // Reconstruct the relative path under the user folder for the
            // whitelist comparison. This is the only allocation per call,
            // and only when we are inside the Users tree.
            let user_rel: String = components[1..]
                .iter()
                .map(|c| std::str::from_utf8(c).unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\\");
            for dir in PROTECTED_USER_SUBDIRS {
                if user_rel.eq_ignore_ascii_case(dir) {
                    return true;
                }
            }
            continue;
        }

        return true;
    }

    false
}

#[cfg(not(windows))]
pub fn is_protected_path(_path: &Path) -> bool {
    false
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
    fn test_sanitize_path_resolves_canonical() {
        let temp = create_test_dir();
        let canonical = sanitize_path(&temp).unwrap();
        assert!(canonical.is_absolute());
        let _ = fs::remove_dir_all(&temp);
    }

    #[test]
    fn test_is_protected_path_windows_dirs() {
        assert!(is_protected_path(Path::new(r"C:\Windows")));
        assert!(is_protected_path(Path::new(r"C:\Program Files")));
        assert!(is_protected_path(Path::new(r"C:\ProgramData")));
    }

    #[test]
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
    fn test_delete_path_refuses_protected_path() {
        let canonical = PathBuf::from(r"C:\Windows");
        assert!(is_protected_path(&canonical));
    }
}
