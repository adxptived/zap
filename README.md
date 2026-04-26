
<p align="center">
  <img src="assets/zap.png" width="160" alt="Zap" />
</p>

<h1 align="center">Zap</h1>
<h4 align="center">A blazing fast alternative to the default Windows delete.</h4>
<br>

Zap is a blazing fast alternative to the default Windows delete function. It uses parallel directory scanning and multi-threaded file deletion to dramatically speed up removing large directory trees.

Key features:
- **Parallel deletion** — process up to 5 directories simultaneously, each with its own real-time progress bar
- **Multi-select from Explorer** — select any number of files/folders and delete them through the GUI confirmation flow
- **Context menu integration** — right-click any file or folder in Windows Explorer
- **Symlink & junction safety** — never follows links, only removes the link itself
- **Read-only recovery** — automatically clears read-only flags and retries

> Zap integrates with your context menu as well as the command-line!

<br>

# :zap: Installation

### Install from GitHub Releases

Download `Zap.exe` from the [latest release](https://github.com/adxptived/zap/releases/latest), or run:

```powershell
iwr https://raw.githubusercontent.com/adxptived/zap/master/dist/install.ps1 -UseB | iex
```

### Build from Source

Prerequisites:
- [Rust toolchain](https://rustup.rs/) with the MSVC target
- Python with PyInstaller on `PATH` for helper executables
- Inno Setup 6 for the Windows installer

```shell
git clone https://github.com/adxptived/zap.git
cd zap
cargo build --profile release-optimized
```

The optimized binaries are written to `target/release-optimized/zap.exe`, `target/release-optimized/zapw.exe`, and `target/release-optimized/zapg.exe`.

To build the complete Windows release package, run:

```bat
rebuild.bat
```

The installer is written to `dist\output\Zap.exe`.

### Uninstall

Run the uninstaller from the Start Menu, or run:

```powershell
# Remove context menu
%APPDATA%\zap\bin\unregister-context-menu.exe

# Remove from PATH and delete files (if installed via Inno Setup)
# Or simply delete %APPDATA%\zap manually
```

<br>

# 🖥️ Command-Line Usage

CLI (Command-Line Interface) lets you run Zap from a terminal — PowerShell, CMD, or any shell. This is the most flexible way to use the tool.

**Typical workflow:** first run with `--dry-run` to preview what would be deleted, then run with `--yes` to actually delete.

```
zap [--yes|--force] [--dry-run] [--threads N] <path>...
zap --help
zap --version
```

### Options

| Flag | Description |
|------|-------------|
| `--yes`, `--force` | **Required** to perform actual deletion. Without one of these the command refuses to delete. |
| `--dry-run` | Preview what would be deleted without removing anything. Does not require `--yes`. |
| `--threads N`, `-j N` | Override the number of Rayon worker threads (default: number of CPU cores). Must be ≥ 1. |
| `--help`, `-h` | Print usage information. |
| `--version`, `-V` | Print the version. |
| `--batch` | Internal flag used by the context menu launcher for multi-select coordination. |

### Examples

```shell
# Preview deletion of a folder (safe, nothing is removed)
zap --dry-run "C:\temp\build-output"

# Delete a folder (confirmation bypassed with --yes)
zap --yes "C:\temp\build-output"

# Delete multiple paths at once
zap --yes "C:\temp\dir1" "C:\temp\dir2"

# Delete using 4 threads
zap --yes --threads 4 "C:\temp\huge-repo"

# Delete a single file
zap --yes "C:\temp\log.txt"
```

On success the elapsed time in seconds is printed. On failure a summary of failed entries is shown and the process exits with code 1.

<br>

# 🖱️ Context Menu Integration

When installed, Zap adds a **"Zap"** submenu to the Windows Explorer context menu for files, directories, and the background of the current directory.

## Menu Items

- **Delete...** — opens the GUI confirmation dialog with deletion options
- **Zap Delete** — deletes the selection immediately without opening a GUI or terminal window

## Multi-Select Support

Select **any number** of files or folders in Explorer, right-click → Zap → **Delete...**. All selected items are processed through the GUI confirmation dialog by default. Use **Zap Delete** when you want immediate deletion with no visible window.

## Architecture

| File | Purpose |
|------|---------|
| `zap.exe` | Console application — the actual deletion engine |
| `zapg.exe` | Default Explorer GUI confirmation dialog used by the **Delete...** context menu item |
| `zapw.exe` | Windowless immediate-delete binary used by **Zap Delete** |

The context menu registry entries invoke `zapg.exe --batch ...` for the default GUI action and `zapw.exe --batch --silent --yes ...` for windowless immediate deletion. Each selected item starts one launcher instance; the instances coordinate via temp files to elect a single leader process. This prevents the "100 terminals/windows" problem when selecting many files.

> **Administrator rights (UAC)** — `zapw.exe` and `zapg.exe` ship with external UAC manifests (`zapw.exe.manifest` and `zapg.exe.manifest`) that request `requireAdministrator`. This allows deletion of protected system folders (e.g. `C:\Windows\SoftwareDistribution\Download`). `zap.exe` itself does not carry a manifest, so running it from a terminal does not trigger a UAC prompt; elevated privileges are inherited when it is spawned by `zapw.exe` or started from an already-elevated shell.

<br>

# 🛡️ Safety Features

Zap is a destructive tool and includes several guards to prevent accidental data loss:

### Confirmation Required

The CLI **refuses to delete** unless `--yes` / `--force` is passed. Use `--dry-run` to preview first.

### Dangerous Paths

The following system directories require an extra yes/cancel confirmation before
deletion, even when `--yes` is passed:

- `C:\Windows` (via `%SystemRoot%`)
- `C:\Program Files` and `C:\Program Files (x86)`
- `C:\ProgramData`
- `C:\Users` (root)
- `C:\Users\<name>` (user profile root)
- `C:\Users\<name>\Desktop`, `Documents`, `Downloads`, `Pictures`, `Music`, `Videos`
- `C:\Users\<name>\AppData`, `AppData\Local`, `AppData\Local\Temp`, `AppData\Roaming`, `AppData\LocalLow`
- `C:\System Volume Information`
- `C:\Recovery`

Subdirectories **deeper** than these well-known folders are allowed (e.g. `C:\Users\you\Documents\myproject` is fine; `C:\Users\you\Documents` itself is blocked).

### Filesystem Root Refused

Passing a drive root such as `C:\`, `D:\`, or any other filesystem root is
always rejected.

### Symlink & Junction Safety

- **Symlinks and junctions are never followed.** They are classified as `EntryKind::Symlink` during scanning and removed as links only — the target directory is left untouched.
- On Windows, reparse points (including junctions) are detected via the `FILE_ATTRIBUTE_REPARSE_POINT` attribute and treated as symlinks.
- A symlink passed directly as the deletion target is removed as a link, not traversed.

### Permission Recovery

If a file or directory is read-only and deletion fails with `PermissionDenied`, Zap automatically clears the read-only flag and retries. This handles the common case of read-only files inside `node_modules` and similar directories.

<br>

# ⚙️ How It Works

Zap uses a **two-phase deletion strategy** powered by [Rayon](https://github.com/rayon-rs/rayon) for parallelism and [jwalk](https://github.com/jessegros/jwalk) for fast directory walking.

## Single Path

1. **Scan phase** — `jwalk::WalkDir` walks the target directory with `follow_links(false)`. Every entry is classified as `File`, `Dir { depth }`, or `Symlink`. A spinner shows the scan count.

2. **Delete phase** — Entries are split into files/links and directories:
   - **Files and symlinks** are deleted in parallel using `rayon::par_chunks` (chunk size 1024). A progress bar tracks completion.
   - **Directories** are sorted by depth (deepest first) and deleted in parallel batches per depth level, so children are always removed before their parents.
   - The root directory itself is removed last.

## Multiple Paths (up to 5 in parallel)

When multiple paths are passed (e.g. from Explorer multi-select), they are processed in chunks of 5. Each path gets a dedicated `ProgressBar` within a single `MultiProgress` — all bars render simultaneously in one terminal window, showing the folder name, scan count, and deletion progress.

```
node_modules  ⠋ [###############>----------] 5670/12345 (2s)
.next         ⠋ [##############>-----------] 3456/8901  (1s)
dist          ⠋ [#######>------------------] 123/4567 (4s)
```

If any entries fail, deletion stops early for that path and a summary of up to 20 failures is reported.

<br>

# 🔧 Development

### Build Scripts

Two convenience scripts are provided:

| Script | Purpose |
|--------|---------|
| `rebuild.bat` | Linear release build for terminal/CI: optimized Rust build, PyInstaller helpers, staging, Inno Setup installer |
| `rebuild-ui.bat` | Interactive menu with individual build, test, lint, package, installer, and clean steps |
| `rebuild.ps1` | Shared build pipeline used by both batch files |

Generated files are intentionally excluded from Git:
- `target/` for Cargo output
- `build/` for PyInstaller work files
- `bin/` for staged release binaries
- `dist/output/` for the final installer

### Build

```shell
cargo build                    # debug build
cargo build --profile release-optimized  # optimized release (LTO, codegen-units=1)
```

The optimized binaries are written to `target/release-optimized/`:
- `zap.exe` — main CLI binary
- `zapw.exe` — windowless launcher used by the Explorer context menu
- `zapg.exe` — confirmation dialog used by the Explorer context menu

### Run

```shell
cargo run -- --dry-run .\test-folder
cargo run -- --yes .\test-folder
cargo run -- --yes .\dir1 .\dir2 .\dir3  # multiple paths (parallel)
```

### End-to-End Example

```shell
# 1. Create a disposable test tree
mkdir test-delete\sub && echo hi > test-delete\sub\file.txt
attrib +R test-delete\sub\file.txt      # make it read-only

# 2. Preview (nothing is deleted)
cargo run -- --dry-run .\test-delete
# Output: Would delete directory: .\test-delete (1 files, 1 dirs, 0 symlinks)

# 3. Actually delete
cargo run -- --yes .\test-delete
# Output: 0.03   (elapsed seconds)

# 4. Verify it's gone
dir test-delete
# Output: File Not Found
```

### Test & Lint

```shell
cargo test
cargo clippy -- -D warnings   # all warnings are errors
cargo fmt
```

Pre-commit checklist — all three must pass with zero errors.

> **Warning:** Never test against important directories. Use disposable temp folders only.

### Project Structure

```
src/
  main.rs          — Entry point: arg parsing, batch coordination, timing
  cli.rs           — CLI argument parser (`CliOptions`/`CliAction`)
  delete.rs        — Core deletion: parallel file/dir removal, `MultiProgress` bars, retry, dry-run
  scan.rs          — Directory walking & entry classification (`jwalk`)
  protect.rs       — Protected-path checks & path sanitization
  bin/
    zapw.rs        — GUI-subsystem launcher that spawns `zap.exe` with `CREATE_NO_WINDOW`
    zapg.rs        — GUI confirmation dialog for the **Delete...** context-menu item
dist/
  zap.iss          — Inno Setup installer script
  environment.iss  — PATH manipulation helpers
  register-context-menu.py   — Explorer context menu registration
  unregister-context-menu.py — Explorer context menu removal
  install.ps1      — Remote download & install script
bin/              — Generated staging directory for release binaries (ignored by Git)
build/            — Generated PyInstaller work/output directory (ignored by Git)
dist/output/      — Generated installer output, `Zap.exe` (ignored by Git)
assets/           — Icons, repository media, and UAC manifests (`zapw.exe.manifest`, `zapg.exe.manifest`)
```

## Authors

[adxptived](https://www.github.com/adxptived)

## Versioning

We use [SemVer](http://semver.org/) for versioning. For the versions available, see the [tags on this repository](https://github.com/adxptived/zap/tags).

## License

This project is licensed under the Apache 2.0 License — see the [LICENSE](LICENSE.txt) file for details.
