use super::*;

const MAX_REPORTED_FAILURES: usize = 20;

#[derive(Debug)]
pub(super) struct DeletionFailure {
    pub(super) path: std::path::PathBuf,
    pub(super) error: String,
}

pub(super) fn lock_failures(
    failures: &Mutex<Vec<DeletionFailure>>,
) -> std::sync::MutexGuard<'_, Vec<DeletionFailure>> {
    failures
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(super) fn push_failure(failures: &Mutex<Vec<DeletionFailure>>, path: &Path, err: &io::Error) {
    let mut failures = lock_failures(failures);
    if failures.len() < MAX_REPORTED_FAILURES {
        failures.push(DeletionFailure {
            path: path.to_path_buf(),
            error: err.to_string(),
        });
    }
}

pub(super) fn failure_summary(total_errors: u64, failures: &[DeletionFailure]) -> String {
    let mut message = crate::messages::entries_failed(total_errors);
    if !failures.is_empty() {
        message.push_str("\n  ");
        message.push_str(crate::messages::LOCKED_HINT);
    }
    for failure in failures {
        message.push_str(&format!(
            "\n  {}: {}",
            failure.path.display(),
            failure.error
        ));
    }
    if total_errors as usize > failures.len() {
        message.push_str("\n  ");
        message.push_str(&crate::messages::more_failures(
            total_errors as usize - failures.len(),
        ));
    }
    message
}
