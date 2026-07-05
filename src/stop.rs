//! Global graceful-stop and pause signals.
//! Stop is set by Ctrl+C handlers or the GUI Stop button; pause by the GUI
//! Pause button. Both are checked by deletion loops: stop bails out early
//! without panicking rayon scopes, pause blocks workers until resumed.

use std::sync::atomic::{AtomicBool, Ordering};

static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
static PAUSE_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Sleep granularity while paused: short enough that Resume/Stop feel
/// instant, long enough to keep paused workers effectively idle.
const PAUSE_POLL_MS: u64 = 50;

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

/// Pause all deletion loops at their next checkpoint (GUI Pause button).
pub fn request_pause() {
    PAUSE_REQUESTED.store(true, Ordering::SeqCst);
}

/// Resume previously paused deletion loops (GUI Resume button).
pub fn request_resume() {
    PAUSE_REQUESTED.store(false, Ordering::SeqCst);
}

#[inline]
pub fn is_paused() -> bool {
    PAUSE_REQUESTED.load(Ordering::Acquire)
}

/// Checkpoint for worker loops: returns immediately when not paused
/// (a single atomic load), otherwise blocks until resumed. Stop always
/// wins over pause so a paused run can still be cancelled.
#[inline]
pub fn wait_if_paused() {
    while is_paused() && !is_stop_requested() {
        std::thread::sleep(std::time::Duration::from_millis(PAUSE_POLL_MS));
    }
}

/// Clear stop and pause flags before starting a new run (GUI runs are
/// in-process, so a previous cancellation/pause must not poison the next run).
pub fn reset() {
    STOP_REQUESTED.store(false, Ordering::SeqCst);
    PAUSE_REQUESTED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// These tests mutate process-global atomics, so they must not run
    /// concurrently with each other — the default parallel test runner
    /// would let one test's `reset()` break another's paused state.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn wait_if_paused_returns_immediately_when_not_paused() {
        let _guard = SERIAL.lock().unwrap();
        reset();
        let start = std::time::Instant::now();
        wait_if_paused();
        assert!(start.elapsed().as_millis() < 40);
    }

    #[test]
    fn stop_wins_over_pause() {
        let _guard = SERIAL.lock().unwrap();
        reset();
        request_pause();
        request_stop();
        let start = std::time::Instant::now();
        wait_if_paused();
        assert!(
            start.elapsed().as_millis() < 200,
            "stop must break the pause wait"
        );
        reset();
    }

    #[test]
    fn resume_unblocks_paused_wait() {
        let _guard = SERIAL.lock().unwrap();
        reset();
        request_pause();
        let waiter = std::thread::spawn(wait_if_paused);
        std::thread::sleep(std::time::Duration::from_millis(120));
        assert!(
            !waiter.is_finished(),
            "worker must stay blocked while paused"
        );
        request_resume();
        waiter.join().unwrap();
        reset();
    }
}
