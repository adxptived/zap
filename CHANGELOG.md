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

- Installer: `Zap.exe` (12.5 MB)
- SHA256: `F6BED08622812E1E9A45E2926F8D401BFA0911BD6F1BD612102F474AE895E478`

## [0.1.1] - 2026-04-26

Maintenance release for the Explorer-first Zap workflow.

### Changed

- Bumped the Zap application and installer version to `0.1.1`.
- Updated the README around the GUI-first Explorer workflow, where
  **Delete...** is the default confirmation flow and **Zap Delete** is the
  direct no-window action.
- Switched the documented one-line installer command to the modern PowerShell
  `irm ... | iex` form.
- Added the GUI delete confirmation screenshot to the README.
- Organized release assets into `assets/branding`, `assets/screenshots`, and
  `assets/manifests`.

### Release Artifact

- Installer: `Zap.exe`
- SHA256: `01DE05E6F357BA1CF97AF0355E1D63556D3945F811E941EA0BF866DEACE91743`

## [0.1.0] - 2026-04-26

Initial public release of Zap, a fast Windows deletion tool with Explorer
integration and a packaged installer.

### Added

- Added the `zap.exe` CLI for fast deletion of files and directories using
  parallel scanning (`jwalk`) and multi-threaded deletion (`rayon`).
- Added the default Explorer context menu action **Zap -> Delete...**, which
  opens a GUI confirmation dialog before deleting selected files or folders.
- Added **Zap -> Zap Delete**, a secondary Explorer action for immediate
  deletion with no GUI window and no terminal window.
- Added multi-select Explorer batching so large selections are coordinated
  through a single operation instead of opening one visible window per item.
- Added `zapg.exe`, a GUI-subsystem confirmation dialog for Explorer deletion.
- Added `zapw.exe`, a windowless immediate-delete binary used by **Zap Delete**.
- Added an Inno Setup installer that installs Zap under the user profile and
  registers the Explorer context menu automatically.
- Added helper scripts and release automation for rebuilding binaries,
  packaging helper executables, building the installer, and updating the
  installer SHA256 in `dist/install.ps1`.

### Safety

- Refuses filesystem roots such as `C:\`.
- Blocks known protected Windows paths unless an elevated interactive flow
  explicitly allows them.
- Treats symlinks and junctions as links only, never as directories to traverse.
- Retries read-only files and directories by clearing the read-only attribute.
- Keeps the GUI dialog open when deletion fails so the failure is visible.

### Improved

- The GUI dialog now closes automatically after successful destructive deletion.
- The GUI dialog is fixed-size, cannot be maximized, and opens centered on the
  monitor under the cursor.
- GUI startup is faster by avoiding runtime icon decoding and reducing Explorer
  batch wait time.
- GUI deletion progress now updates while files are being removed.
- Context menu wording is simplified to only two actions: **Delete...** and
  **Zap Delete**.

### Removed

- Removed legacy Turbo Delete binary names and installer artifacts.
- Removed terminal-based Explorer actions from the context menu.

### Release Artifact

- Installer: `Zap.exe`
- SHA256: `B2F2B3AEBA1D9D564669896685436D408D3FED66207974E011EF271B00128D42`
