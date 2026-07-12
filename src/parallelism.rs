//! Shared concurrency policy for filesystem-heavy work.
//!
//! More workers do not imply more throughput for metadata operations. Keeping
//! the default bounded avoids scheduler thrashing and lets the UI remain
//! responsive, while an explicit user value still permits tuning fast SSDs.

/// Hard safety ceiling for an explicit `--threads`/GUI override.
pub const MAX_WORKERS: usize = 64;
/// Conservative ceiling used when the user did not request a value.
pub const DEFAULT_MAX_WORKERS: usize = 8;

#[inline]
pub fn logical_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .max(1)
}

/// Effective worker count for deletion and other I/O-heavy jobs.
#[inline]
pub fn worker_count(requested: Option<usize>) -> usize {
    requested
        .unwrap_or_else(|| logical_cpus().min(DEFAULT_MAX_WORKERS))
        .clamp(1, MAX_WORKERS)
}

/// A scan runs concurrently with deletion, so it receives only a portion of
/// the shared budget. Four workers are enough to overlap directory reads on
/// most disks without letting jwalk create an unbounded second pool.
#[inline]
pub fn scan_worker_count(total_workers: usize) -> usize {
    total_workers.clamp(1, MAX_WORKERS).div_ceil(4).clamp(1, 4)
}

/// Deletion receives the rest of the shared budget. The scanner itself runs
/// on a coordinator thread, so retain at least one Rayon deletion worker.
#[inline]
pub fn delete_worker_count(total_workers: usize) -> usize {
    let total = total_workers.clamp(1, MAX_WORKERS);
    total.saturating_sub(scan_worker_count(total)).max(1)
}

/// Number of in-flight entries retained by the streaming delete pipeline.
#[inline]
pub fn queue_capacity(workers: usize) -> usize {
    workers.clamp(1, MAX_WORKERS).saturating_mul(256).clamp(512, 8192)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_worker_count_is_safely_clamped() {
        assert_eq!(worker_count(Some(0)), 1);
        assert_eq!(worker_count(Some(MAX_WORKERS + 100)), MAX_WORKERS);
    }

    #[test]
    fn default_worker_count_is_bounded() {
        assert!((1..=DEFAULT_MAX_WORKERS).contains(&worker_count(None)));
    }

    #[test]
    fn queue_is_bounded_independently_of_input() {
        assert_eq!(queue_capacity(usize::MAX), 8192);
        assert!(queue_capacity(1) >= 512);
    }
}
