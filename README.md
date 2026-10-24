# Zap

Fast Windows deletion from Explorer, with a confirmation dialog by default and
a windowless immediate-delete action when you need it.

<p align="center">
  <img src="assets/branding/zap.png" width="140" alt="Zap logo" />
</p>

<p align="center">
  <img src="assets/screenshots/delete-dialog.png" alt="Zap confirmation dialog" />
</p>

Zap removes large directory trees faster than the default Windows delete flow by
combining parallel directory scanning with multi-threaded file removal. The main
workflow is Explorer-first: right-click selected files or folders, choose Zap,
then pick the safe GUI confirmation flow or the silent immediate-delete action.

## Features

- **Explorer-first GUI** — `Zap -> Delete...` opens a compact confirmation dialog.
- **Windowless delete** — `Zap → Quick delete (no confirmation)` deletes the selection without a GUI or terminal window.
- **Multi-select support** — Explorer selections are batched into one coordinated operation.
- **Fast deletion engine** — on Windows each worker lists a folder with `FindFirstFileExW`, deletes its files right away and recurses into subfolders in parallel (`rayon`); folders are removed as soon as they are empty.
- **Safety guards** — filesystem roots are refused, protected paths are blocked, and symlinks/junctions are never traversed.
- **Read-only recovery** — read-only attributes are cleared and deletion is retried when possible.
- **Filter support** — glob pattern filtering, size filtering, and date-based filtering.
- **Secure deletion** — overwrite files with random data before removing (`--shred`), also available as a checkbox in the GUI.
- **Recycle Bin** — send to Recycle Bin instead of permanent delete (`--recycle`). Multiple items are moved with a single shell call — one Explorer "Undo" restores everything.

## Installation

Download `Zap.exe` from the [latest GitHub release](https://github.com/adxptived/zap/releases/latest),
or install with PowerShell:

```powershell
irm https://raw.githubusercontent.com/adxptived/zap/master/dist/install.ps1 | iex
```

The installer adds the Zap submenu to Windows Explorer and installs:

- `zapg.exe` for the default GUI confirmation action.
- `zapw.exe` for the windowless **Quick delete** / **Move to Recycle Bin** actions.
- `zap.exe` for optional terminal use.

To uninstall, use the Start Menu uninstaller. To remove only the Explorer menu,
turn it off in Settings or run:

```powershell
zapg.exe --unregister-menu
```

## Explorer Usage

Right-click any file or folder and open the **Zap** submenu:

| Menu item | Behavior |
| --- | --- |
| **Delete with confirmation…** | Opens the confirmation dialog. The default and safest flow. |
| **Move to Recycle Bin** | Recycles immediately, no window. Restorable. |
| **Quick delete (no confirmation)** | Deletes permanently and immediately, with no window. A message appears only if something fails. |

Right-clicking the folder background offers **Zap: Delete current folder…**,
which always opens the confirmation dialog — it never deletes silently.

## Settings

Open **Zap Settings** from the Start Menu, the ⚙ button in the dialog, or
`zapg.exe --settings`. Saving re-registers the Explorer menu immediately.

- **Language** — system default, English or Russian (dialog and menu labels).
- **Context menu** — on/off, submenu or items directly in the menu, icons on/off,
  which actions to show, the folder-background entry, Shift-only. A live
  preview shows exactly what will be registered.
- **Dialog** — default action (Delete permanently / Recycle Bin / Secure delete), auto-close after
  success, failure notifications for windowless deletions, operation journal,
  worker-thread limit.

Settings are stored in `%APPDATA%\zap\settings.ini` (plain `key = value`).

The dialog also supports **Preview only**, drag & drop to add items, removing
items before the run, Pause/Stop, and a copyable failure report.
## Optional Terminal Usage

The terminal interface is secondary. Use it for scripts, automation, or manual
debugging.

```shell
zap [FLAGS] [OPTIONS] <path>...
zap --help
zap --version
```

| Flag | Description |
| --- | --- |
| `--yes`, `--force` | Required for destructive terminal deletion. |
| `--dry-run` | Preview without removing files. |
| `--threads N`, `-j N` | Override the Rayon worker thread count. |
| `--silent` | Suppress progress output. Used by `zapw.exe`. |
| `--batch` | Internal Explorer multi-select coordination flag. |
| `--shred` | Overwrite files with random data before deleting (3 passes). Note: file names/MFT records are not wiped, and on SSDs wear-leveling means overwrites are best-effort. |
| `--only-empty` | Only delete if the directory is empty. |
| `--recycle` | Send to Recycle Bin instead of permanent delete (also a checkbox in the GUI dialog and an Explorer context-menu entry). Batched: one shell call / one Undo for the whole selection. |
| `--no-size-preview` | Show the confirmation prompt immediately without calculating preview sizes. Useful for huge trees where sizing would feel like a hang. |
| `--include GLOB` | Only delete paths matching glob pattern (repeatable). |
| `--exclude GLOB` | Skip paths matching glob pattern (repeatable). |
| `--min-size SIZE` | Only delete files larger than SIZE (bytes, or `10k`, `5mb`, `1.5g`, `2tb`). |
| `--newer-than WHEN` | Only delete files newer than a date/time (RFC 3339) or age (e.g. `12h`, `30d`). |
| `--older-than WHEN` | Only delete files older than a date/time (RFC 3339) or age (e.g. `12h`, `30d`). |
| `--` | Treat every following argument as a path (for names starting with `-`). |

Examples:

```shell
zap --dry-run "C:\temp\build-output"
zap --yes "C:\temp\build-output"
zap --yes --threads 4 "C:\temp\huge-repo"
zap --no-size-preview "C:\temp\huge-repo"
zap --yes --shred "C:\temp\secret-files"
zap --yes --exclude "*.log" --min-size 10mb "C:\temp\project"
zap --yes -- "-weird-folder-name"
```

## Performance Notes

For large Explorer or CLI selections made of many top-level files, Zap uses a bulk
file-root delete path. This avoids creating one worker/progress item per file and
keeps selections with thousands of files responsive while preserving the same
file, symlink, cancellation, and error-reporting behavior. Directory roots still
use the streaming scan/delete pipeline.

## Safety Model

Zap is destructive, so the deletion engine keeps several guardrails in place:

- Drive roots such as `C:\` and `D:\` are always refused.
- Known system and profile roots are protected.
- Symlinks and junctions are removed as links only; targets are not traversed.
- Read-only files and directories are retried after clearing the read-only flag.
- The GUI keeps the dialog open when deletion fails so the error remains visible.
- `--recycle` is also subject to protection guards (cannot recycle system paths).

Protected examples include:

- `C:\Windows`
- `C:\Program Files`
- `C:\ProgramData`
- `C:\Users`
- user profile roots and well-known folders such as `Desktop`, `Documents`, `Downloads`, `AppData`
- `C:\System Volume Information`
- `C:\Recovery`

## Build From Source

Prerequisites:

- Rust toolchain with the MSVC target.
- Python with PyInstaller on `PATH`.
- Inno Setup 6.

```shell
git clone https://github.com/adxptived/zap.git
cd zap
cargo build --profile release-optimized
```

Build the full Windows release package:

```bat
rebuild.bat
```

The installer is written to:

```text
dist\output\Zap.exe
```

## Development

```shell
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Useful build scripts:

| Script | Purpose |
| --- | --- |
| `rebuild.bat` | Non-interactive full release pipeline. |
| `rebuild-ui.bat` | Interactive build menu for local development. |
| `rebuild.ps1` | Shared PowerShell build pipeline. |

Generated paths:

- `target/` — Cargo output.
- `build/` — PyInstaller work files.
- `bin/` — staged release binaries.
- `dist/output/` — final installer output.

## Project Layout

```text
src/
  main.rs                  Console CLI and batch coordination
  cli.rs                   Argument parser
  delete.rs                Core deletion engine
  scan.rs                  Directory walking and entry classification
  protect.rs               Protected path checks
  filter.rs                Glob pattern and metadata filtering
  recycle.rs               Recycle Bin integration
  shred.rs                 Secure deletion (overwrite with random data)
  size.rs                  Directory size calculation
  stop.rs                  Global graceful-stop signal
  path_utils.rs            Path formatting and dedup helpers
  batch.rs                 Multi-process Explorer batch coordination
  winapi.rs                Windows API helpers
  bin/
    zapg/                  Explorer GUI confirmation dialog
      main.rs              Entry point, args, batch coordinator election
      app.rs               Dialog state and background workers
      ui.rs                Theme and rendering
    zapw.rs                Windowless immediate-delete binary

assets/
  branding/                Logo and installer icon
  screenshots/             README and release screenshots
  manifests/               UAC manifests for Explorer-launched binaries

dist/
  zap.iss                  Inno Setup installer script
  install.ps1              Remote install script
```

## Release Artifact

- Installer: `dist\output\Zap.exe`
- SHA256: `01DE05E6F357BA1CF97AF0355E1D63556D3945F811E941EA0BF866DEACE91743`

## License

Apache-2.0. See [LICENSE.txt](LICENSE.txt).
