# Changelog

All notable changes to this project will be documented in this file.

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
