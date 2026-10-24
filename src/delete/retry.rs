use super::*;

pub fn set_writable(path: &Path) -> io::Result<()> {
    let mut perms = std::fs::symlink_metadata(path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(path, perms)
}

#[inline]
fn try_force_delete(path: &Path, original: io::Error) -> io::Result<()> {
    #[cfg(windows)]
    {
        if crate::winapi::force_delete(path).is_ok() {
            return Ok(());
        }
    }
    let _ = path;
    Err(original)
}

/// Shared recovery path for `PermissionDenied` failures: clear the
/// read-only attribute and retry `op`, then fall back to the Win32
/// force-delete. Any other error kind is returned unchanged.
fn retry_after_clearing_readonly(
    path: &Path,
    original: io::Error,
    op: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<()> {
    if original.kind() != io::ErrorKind::PermissionDenied {
        return Err(original);
    }
    let meta = std::fs::symlink_metadata(path).ok();
    if meta.as_ref().is_some_and(|m| m.permissions().readonly()) {
        let mut perms = meta.unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        if std::fs::set_permissions(path, perms).is_ok() && op(path).is_ok() {
            return Ok(());
        }
    }
    // Transient locks (antivirus scanners, indexers) usually clear within
    // milliseconds — retry briefly before the expensive force-delete fallback.
    for delay_ms in [10u64, 50] {
        if crate::stop::is_stop_requested() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "deletion cancelled",
            ));
        }
        std::thread::park_timeout(std::time::Duration::from_millis(delay_ms));
        if crate::stop::is_stop_requested() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "deletion cancelled",
            ));
        }
        if op(path).is_ok() {
            return Ok(());
        }
    }
    try_force_delete(path, original)
}

pub fn remove_file_with_retry(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    if is_injected_failure(path) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "injected remove_file failure",
        ));
    }

    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) => retry_after_clearing_readonly(path, e, |p| std::fs::remove_file(p)),
    }
}

pub fn remove_dir_with_retry(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(e) => retry_after_clearing_readonly(path, e, |p| std::fs::remove_dir(p)),
    }
}

pub fn delete_symlink(path: &Path) -> io::Result<()> {
    fn remove_either(path: &Path) -> io::Result<()> {
        std::fs::remove_file(path).or_else(|file_err| {
            std::fs::remove_dir(path).map_err(|dir_err| {
                io::Error::new(
                    dir_err.kind(),
                    format!(
                        "failed to remove symlink {}: {}; {}",
                        path.display(),
                        file_err,
                        dir_err
                    ),
                )
            })
        })
    }

    match remove_either(path) {
        Ok(()) => Ok(()),
        Err(e) => retry_after_clearing_readonly(path, e, remove_either),
    }
}
