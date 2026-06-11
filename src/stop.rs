//! Global graceful-stop signal set by Ctrl+C handlers.
//! Checked by deletion loops to bail out early without panicking rayon scopes.

use std::sync::atomic::{AtomicBool, Ordering};

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn install_handler() {
    let _ = ctrlc::set_handler(|| {
        STOP_REQUESTED.store(true, Ordering::SeqCst);
    });
}

#[inline]
pub fn is_stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::Acquire)
}

/// Request a graceful stop programmatically (used by the GUI Stop button).
pub fn request_stop() {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
}

/// Clear the stop flag before starting a new run (GUI runs are in-process,
/// so a previous cancellation must not poison the next run).
pub fn reset() {
    STOP_REQUESTED.store(false, Ordering::SeqCst);
}
