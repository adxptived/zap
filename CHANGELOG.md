# Changelog

All notable changes to this project will be documented in this file.

## [1.2.0] - 2026-05-25

Performance release — parallel scan, pipeline delete, and adaptive parallelism.

### Performance

- **Pipeline scan + delete**: file deletion now starts immediately as `jwalk`
  discovers entries instead of waiting for the full scan to finish. On large
  directories (100 k+ entries) this eliminates the entire scan-then-delete
  dead-time, effectively halving elapsed time.
- **Parallel directory walk**: `jwalk` now uses
  `Parallelism::RayonNewPool(N)` (up to 8 I/O threads) instead of the
  default single-threaded walk. Directories with deep or wide trees scan
  noticeably faster.
- **Adaptive chunk size**: Rayon `par_chunks` batch size is now computed
  dynamically (`total / (threads * 4)`, clamped to `[64, 65536]`) so small
  deletes get fine-grained parallelism and large ones amortise per-chunk
  overhead, instead of a fixed 16 384.
- **Reduced progress-bar overhead**: scan tick interval raised from 32 to 512
  entries, cutting the number of atomic progress-bar updates by 16×.
- **Larger initial Vec capacity**: `files_and_links` pre-allocates 8 192
  slots (was 1 024), reducing reallocations on moderately sized trees.
- `panic = "abort"` added to the `release-optimized` profile — removes
  unwinding tables, shrinks binaries slightly.

### Dependencies

- Added `crossbeam-channel 0.5` for the bounded scan → delete pipeline
  channel.

### Internal

- New `scan_into_channel` API in `scan.rs` — streaming scan that sends
  `File` / `Symlink` entries directly into a caller-supplied channel while
  accumulating `Dir` entries for depth-first removal.
- `delete_directory_pool_inner` split into `delete_directory_pipeline`
  (no-filter fast path) and `delete_directory_filtered` (filter-aware path).
- Shared helpers `process_file_batch`, `delete_dir_batches`, and `finalize`
  extracted to eliminate duplication between the two paths.

### Release Artifact

- Installer: `Zap.exe` (v1.2.0)
- SHA256: `12464F2E2BAF63C4681D1A5BD4377B4A6C1E0AB6B1619BF6222ED9557D2C4A47`
