# Zap v1.1.0 Changelog

## Bug Fixes
- **O(n/chunk_size) performance degradation** — removed MAX_PARALLEL_DELETES chunking, all paths dispatch in single rayon::scope
- **Follower process linger** — followers now exit immediately via process::exit(0)
- **zapg lock held entire GUI lifetime** — lock released in finalize_batch_session, process-alive stale check added
- **Triple code duplication** — batch infrastructure extracted to src/batch.rs with unified lock files
- **Busy repaint during recalculation** — explicit total_size_calculating flag
- **Unicode path corruption** — hex-encode batch paths via as_encoded_bytes
- **Double symlink_metadata** — single call reused in remove_dir_with_retry permission check
- **Batch leader silent mode** — console now always allocated for dangerous-path errors
- **Dead dependency** — rfd removed from Cargo.toml

## Performance Improvements
- **is_readonly guard** — skips set_writable (3 syscalls) for files locked by other processes
- **PROGRESS_CHUNK_SIZE 1024→16384** — fewer rayon tasks, less Mutex contention
- **scan progress bar** — only allocates local_bar when no external_bar provided
- **batch state count-only** — removed useless file-size syscalls

## New Features
- **Graceful Ctrl+C** — stop.rs module, ctrlc handler, checked in deletion loops
- **--dry-run default** — auto-enables when --yes/--force absent, shows sizes + interactive confirm
- **Self-deletion guard** — refuses to delete the running executable
- **Colored timing** — green <5s, yellow <30s, red >30s
- **Error log** — failed paths written to %TEMP%\zap-errors.log
- **Size display** — dir_size_recursive in dry-run preview and GUI
- **has_dangerous_paths warning** — extra confirmation checkbox in zapg GUI
- **--batch hidden from help** — no longer listed in public usage

## Code Quality
- **to_delete_options() factory** — single option-builder on CliOptions
- **path_utils.rs** — shared progress_name/compact_path
- **size.rs** — shared dir_size_recursive/format_size
- **Atomic write_batch_paths** — .tmp rename to avoid partial reads
- **Atomic touch_lock** — append semantics, no truncation window
- **batch_state count-only** — streamlined wait_for_batch_quiet
- **Preallocated errors Vec** — Vec::with_capacity in run_delete_parallel
- **inline scan_entries** — removed dead one-liner wrapper
- **print_help updated** — explains default dry-run behavior
- **read_batch_paths** — explicit error handling instead of unwrap_or_default

## Audit
- Full codebase audit: 7 bugs, 3 security notes, 3 dead code, 3 perf, 4 style, 4 edge cases
- All critical findings resolved
- 54 tests pass, clippy clean
