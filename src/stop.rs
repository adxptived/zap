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
    STOP_REQUESTED.load(Ordering::Relaxed)
}
