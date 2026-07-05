# Changelog

All notable changes to this project will be documented in this file.

## Unreleased

### Fixed

- **zapw now journals context-menu deletions** — the Explorer context-menu
  worker (the most common deletion surface) previously never wrote to the
  operation journal; it now records per-path outcomes for direct, batch,
  and late-drained deletions, honoring `--no-journal`/`ZAP_NO_JOURNAL`.
- **Cancelled bulk deletions no longer reported as successes** — when Stop
  interrupts a bulk file deletion, the skipped paths are now marked
  "cancelled by user" instead of silently passing as deleted in the GUI
  item list and the operation journal.
- **`zapg --no-journal`** — the GUI dialog now accepts the same
  `--no-journal` flag as `zap` and `zapw`; previously the journal could
  only be disabled for GUI runs via `ZAP_NO_JOURNAL`.
- **Unverifiable reparse points no longer abort the whole run** — when a
  directory's reparse status cannot be checked (e.g. access denied), the
  scan now skips just that subtree (still fail-closed: nothing beneath it
  is deleted) instead of cancelling the entire operation; the failure
  surfaces later as a directory-removal error.
- **Per-run error log name** — failure logs are written to
  `%TEMP%\zap-errors-<pid>.log` with create-new semantics instead of
  truncating a fixed, predictable `zap-errors.log` path.

### Added

- **`zap --journal [N]`** — print the N most recent operation-journal
  entries (default 20) plus the journal location, spanning the rotated
  file when the current journal is short.
- **Shred filename scrubbing** — after the overwrite passes, shredded files
  are renamed to an anonymous name before removal so the original filename
  no longer lingers in directory metadata; best-effort with a safe
  fallback to plain removal.

### Performance

- **O(paths) journal recording** — per-path error lookup during journal
  recording now uses a hash map instead of a linear scan per path,
  removing an O(paths × errors) hotspot on bulk runs with mass failures.
- **Faster shred** — the overwrite buffer grew from 64 KiB to 1 MiB
  (capped at the file size), cutting syscall count ~16x on large files,
  and the RNG handle is reused across chunks instead of re-created per
  64 KiB block.

- **Parallel size scanning** — `dir_size_recursive` now bridges the walk
  onto the rayon pool so per-entry metadata reads run in parallel, resolves
  plain files with a single metadata call (no jwalk spin-up), and the GUI
  sizes all selected roots concurrently. Size badge and byte-weighted ETA
  appear much sooner on large or bulk selections.
- **Faster treemap collection** — `dir_size_tree` no longer walks every
  file's ancestor chain (O(files × depth) allocations); sizes are credited
  to the immediate parent and rolled up in one bottom-up pass.
- **Cheaper treemap rendering** — entries are sorted, capped and summed once
  in the collection thread; render frames no longer sort or sum the raw
  entry list (which could hold tens of thousands of paths).
- **O(1) event routing in zapg** — worker events are applied through a
  path→index map per frame instead of a linear scan per event, and batch /
  drag-and-drop dedup uses a HashSet instead of quadratic scans.

### Added

- **Operation journal** — every real (non-dry-run) run is appended to
  `%LOCALAPPDATA%\zap\journal.log` as tab-separated lines
  (`timestamp  action  ok|error  path`), covering CLI, GUI and worker runs.
  Opt out with `--no-journal` or the `ZAP_NO_JOURNAL=1` env var. Best-effort:
  journaling never fails or slows down a deletion.
- **Pause/Resume in the GUI** — a new Pause button next to Stop blocks all
  deletion loops at their next checkpoint and resumes them on click; Stop
  still works while paused, and paused time is excluded from the elapsed
  timer and the ETA.
- **Byte-weighted progress + ETA** — the zapg progress bar and a new
  `ETA ~…` readout now weight items by their on-disk size (computed by the
  existing background size pass, no extra walks) instead of counting every
  item equally, which is far more accurate on mixed selections.

- **Stop button in the GUI** — the Cancel button becomes a working *Stop*
  during a run: it raises the graceful-stop flag, the in-flight item aborts
  and everything not yet started is marked "cancelled by user".
- **Drag & drop in the GUI** — drop files/folders onto the zapg window to
  add them to the list (with a drop-hint overlay); duplicates are skipped
  and the size badge / system-folder warning refresh automatically.
- **Humane age filters** — `--newer-than` / `--older-than` now accept ages
  like `12h`, `30d`, `90min` in addition to RFC 3339 timestamps.

- **GitHub Actions CI** (`.github/workflows/ci.yml`): fmt + clippy (deny
  warnings) + tests on Linux and Windows + release build with uploaded
  Windows binaries on every push/PR.
- **Shred in the GUI** — new "Shred (overwrite data, unrecoverable)"
  checkbox in zapg; mutually exclusive with Recycle Bin mode, with its own
  button color-independent label and warning hint.
- **Taskbar progress (Windows)** — the zapg taskbar button now mirrors run
  progress via `ITaskbarList3` (green fill while running, red on failures),
  implemented with raw COM FFI, no new dependencies.

### Changed

- **Batched Recycle Bin moves** — recycling several items now issues a
  *single* `SHFileOperationW` call (one shell roundtrip, one Explorer
  "Undo" entry) in zap, zapw and the batch context-menu flow.

### Fixed

- Recycle Bin moves now always pass absolute paths to the shell and reject
  paths beyond the shell's `MAX_PATH` limit with a clear error instead of a
  cryptic shell code.

- **Critical:** zapw (the windowless context-menu worker) dropped the
  `--recycle`, `--shred`, `--only-empty` and filter flags by constructing
  `DeleteOptions` manually — the "Move to Recycle Bin" context-menu entry
  deleted permanently. Now built via `to_delete_options` + regression test.

## [1.3.0] - 2026-06-12

Correctness and refactor release — pipeline fixes, safer flags, GUI cleanup.

### Fixed

- **Scan + delete pipeline restored**: file batches were collected into a
  vector and only deleted after the full scan finished, silently disabling
  the 1.2.0 pipeline overlap. Batches are now processed as they arrive via
  `par_bridge`, so deletion overlaps scanning again.
- **`--only-empty` race**: emptiness was checked with a separate read before
  deleting, which raced with concurrent file creation. Now relies on atomic
  `remove_dir` semantics (`DirectoryNotEmpty` → skip), and non-empty
  directories are reported instead of failing the run.
- **Top-level file roots now respect filters**: passing a file directly with
  `--include`/`--exclude`/`--min-size`/date filters previously deleted it
  unconditionally; filters are now applied.
- **`--shred` durability**: each overwrite pass now calls `sync_data` so
  passes actually reach the disk instead of being coalesced by the OS cache.
- **Ctrl-C responsiveness**: the stop flag is now honored between file
  batches, directory batches, and before the final root removal; cancelled
  runs report "Cancelled" instead of finishing silently.
- **Treemap layout**: row orientation no longer disagrees with the
  remaining-bounds bookkeeping (rectangles could overlap or leave gaps);
  layout is iterative (no stack overflow on huge trees) and capped at the
  150 largest entries.
- **Treemap sizes double-counted**: the GUI summed every directory *and* the
  files inside it; it now uses each root's immediate children, so the total
  matches the real selection size.
- Tests that assert Windows protected paths are now gated to Windows
  instead of failing on other platforms.

### Added

- **Recycle Bin integration in Explorer and GUI**: new "Move to Recycle Bin"
  context-menu entry (windowless, instant, recoverable) and a
  "Move to Recycle Bin" checkbox in the `zapg` confirmation dialog (amber
  *Recycle* button, adjusted hints).
- `--min-size` accepts human-readable sizes: `10k`, `5mb`, `1.5g`, `2tb`
  (binary multiples), in addition to plain bytes.
- `--` end-of-flags separator for deleting paths that start with `-`.
- Unknown short flags are rejected with a hint instead of being treated as
  paths to delete.
- `--shred` and `--recycle` are now mutually exclusive (previously shredded
  files were *not* recycled despite the flag).
- GUI: scrollable per-item status list for multi-select runs, with per-path
  success/failure indicators.
- GUI: per-rectangle colors in the disk-usage treemap.

### Stability

- One unreadable entry (access denied, file vanished mid-scan) no longer
  aborts the whole scan+delete run — it is skipped and any real failure is
  reported via the failure summary.
- `--recycle` is now rejected when combined with `--only-empty` or with
  filters: previously `--only-empty`/filters were silently ignored and the
  *entire* item was moved to the Recycle Bin.
- Interactive confirmation now says "Move to Recycle Bin?" instead of
  "Delete permanently?" when `--recycle` is active.

- GUI no longer hangs forever if the delete worker thread dies unexpectedly —
  remaining items are marked failed and the dialog finishes.
- Transient file locks (antivirus/indexer) are retried briefly (10/50 ms)
  before falling back to force-delete, so fewer spurious failures.

### Internal

- `zapg` god-object split into `main.rs` / `app.rs` / `ui.rs` modules.
- Deduplicated read-only-retry logic in `delete.rs`; `only_empty` no longer
  threads through every pipeline function signature.
- `CliOptions` derives `Clone`; option rebuilding boilerplate removed in
  `main.rs` and `zapw.rs`.
- GUI preview size calculation parallelised; treemap data no longer cloned
  every frame.
- 20+ new unit tests (parser, treemap geometry, only-empty/filter edge
  cases, treemap data collection); suite: 98 tests, clippy-clean.

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
