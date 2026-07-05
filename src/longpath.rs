//! Verbatim (`\\?\`) path conversion for Win32 long-path support.
//!
//! The Rust stdlib transparently adds the verbatim prefix for its own
//! filesystem calls, but our direct Win32 helpers in `winapi.rs` pass the
//! path to `CreateFileW`/`GetFileAttributesW` as-is. Without the prefix
//! those calls fail on paths longer than `MAX_PATH` (260) — exactly the
//! deep `node_modules` trees zap is built for.
//!
//! The conversion itself is pure string manipulation, kept in its own
//! platform-independent module so the logic is unit-tested on every host.

/// Convert an absolute Windows path string to verbatim (`\\?\`) form.
///
/// Returns `None` when the input should be passed through unchanged:
/// already-verbatim (`\\?\`) or device (`\\.\`) paths, and relative paths
/// (the `\\?\` form disables CWD-relative resolution, so prefixing a
/// relative path would break it).
///
/// * `C:\dir\file` → `\\?\C:\dir\file`
/// * `\\server\share\x` → `\\?\UNC\server\share\x`
/// * forward slashes are normalised to backslashes (Win32 verbatim paths
///   do not accept `/` as a separator).
pub fn to_verbatim(path: &str) -> Option<String> {
    // Already verbatim (`\\?\`) or a device path (`\\.\`): pass through.
    if path.starts_with("\\\\?\\") || path.starts_with("\\\\.\\") {
        return None;
    }

    // UNC share: `\\server\share\...` → `\\?\UNC\server\share\...`
    if let Some(rest) = path.strip_prefix("\\\\") {
        let mut out = String::with_capacity(path.len() + 6);
        out.push_str("\\\\?\\UNC\\");
        push_normalised(&mut out, rest);
        return Some(out);
    }

    // Drive-absolute: `C:\...` or `C:/...`
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        let mut out = String::with_capacity(path.len() + 4);
        out.push_str("\\\\?\\");
        push_normalised(&mut out, path);
        return Some(out);
    }

    // Relative or unrecognised form: leave untouched.
    None
}

/// Append `s` to `out`, converting `/` separators to `\`.
fn push_normalised(out: &mut String, s: &str) {
    for ch in s.chars() {
        out.push(if ch == '/' { '\\' } else { ch });
    }
}

#[cfg(test)]
mod tests {
    use super::to_verbatim;

    #[test]
    fn drive_absolute_gets_prefix() {
        assert_eq!(
            to_verbatim("C:\\Users\\me\\project\\node_modules").as_deref(),
            Some("\\\\?\\C:\\Users\\me\\project\\node_modules")
        );
    }

    #[test]
    fn forward_slashes_are_normalised() {
        assert_eq!(
            to_verbatim("C:/Users/me/file.txt").as_deref(),
            Some("\\\\?\\C:\\Users\\me\\file.txt")
        );
    }

    #[test]
    fn unc_path_gets_unc_prefix() {
        assert_eq!(
            to_verbatim("\\\\server\\share\\dir\\file").as_deref(),
            Some("\\\\?\\UNC\\server\\share\\dir\\file")
        );
    }

    #[test]
    fn already_verbatim_passes_through() {
        assert_eq!(to_verbatim("\\\\?\\C:\\anything"), None);
        assert_eq!(to_verbatim("\\\\?\\UNC\\srv\\share"), None);
    }

    #[test]
    fn device_path_passes_through() {
        assert_eq!(to_verbatim("\\\\.\\PhysicalDrive0"), None);
    }

    #[test]
    fn relative_paths_pass_through() {
        assert_eq!(to_verbatim("dir\\file.txt"), None);
        assert_eq!(to_verbatim("./dir/file"), None);
        assert_eq!(to_verbatim(""), None);
        // Drive-relative (no separator after the colon) is not absolute.
        assert_eq!(to_verbatim("C:file.txt"), None);
    }

    #[test]
    fn long_path_roundtrip() {
        let deep = format!("C:\\{}\\leaf.txt", "a\\".repeat(200));
        let verbatim = to_verbatim(&deep).expect("absolute path must convert");
        assert!(verbatim.starts_with("\\\\?\\C:\\"));
        assert!(verbatim.len() > 260);
    }
}
