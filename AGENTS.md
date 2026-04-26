# Repository Guidelines

## Project Overview

Zap is a Rust CLI application for fast Windows directory deletion.
It uses parallel directory scanning (`jwalk`) and multi-threaded file deletion
(`rayon`) to dramatically outperform the default Windows delete.

## Project Structure & Module Organization

Source code is organized under `src/`:

- `main.rs` — Entry point of the `zap` binary: arg parsing dispatch, timing, error summary, batch coordination (`run_batch`).
- `cli.rs` — CLI argument parser (`parse_args`), help/version output, `CliOptions` / `CliAction` types.
- `delete.rs` — Core deletion logic: parallel file/dir removal via Rayon, retry on `PermissionDenied` (files, dirs, and symlinks), dry-run, symlink handling.
- `scan.rs` — Directory walking with `jwalk::WalkDir` and entry classification (`EntryKind`: `File`, `Dir { depth }`, `Symlink`).
- `protect.rs` — Protected-path checks (`is_protected_path`), path sanitization (`sanitize_path`), filesystem-root detection.
- `bin/zapw.rs` — Entry point of the `zapw` (windowless) binary. Spawns the sibling `zap.exe` with `CREATE_NO_WINDOW` so the Explorer context menu does not flash a console.
- `bin/zapg.rs` — Entry point of the `zapg` (GUI dialog) binary. Provides an egui confirmation dialog for the **Delete...** context menu item.

The three binaries are declared explicitly in `Cargo.toml` (`[[bin]] name = "zap"` / `"zapw"` / `"zapg"`), so `cargo build` produces `zap.exe`, `zapw.exe`, and `zapg.exe` directly with no rename step.

Cargo metadata and dependency versions are in `Cargo.toml` and `Cargo.lock`.
Key dependencies: `jwalk` (parallel walk), `rayon` (parallel delete), `indicatif` (progress bars), `owo-colors` (colored output), `eframe` (GUI dialog), `rfd` (file dialogs).

Packaging and installer artifacts live in `dist/`, including Inno Setup files,
PowerShell install scripts, and context-menu registration helpers. Prebuilt
executables are stored in `bin/`. Visual assets such as icons and demo media are
stored in `assets/`.

## Build Commands

| Command | What it does | Output location |
|---------|-------------|-----------------|
| `cargo build` | Debug build for local iteration | `target/debug/zap.exe`, `target/debug/zapw.exe`, `target/debug/zapg.exe` |
| `cargo build --release` | Standard release build | `target/release/zap.exe`, `target/release/zapw.exe`, `target/release/zapg.exe` |
| `cargo build --profile release-optimized` | Optimized release (LTO, codegen-units=1) | `target/release-optimized/zap.exe`, `target/release-optimized/zapw.exe`, `target/release-optimized/zapg.exe` |

## Build Scripts

Two batch files are provided for building:

- **`rebuild.bat`** — Linear build script for **terminal/CI/automation**. Runs the full release pipeline without interaction. Use this when you want a one-pass build or are running in a non-interactive environment.

- **`rebuild-ui.bat`** — Interactive menu-driven build script. Presents a menu with individual build steps (debug, release, tests, lint, full rebuild, clean). Use this for **manual interactive development** when you want to run specific steps without typing commands.

**Recommendation:** Use `rebuild.bat` for terminal/automated builds. Use `rebuild-ui.bat` only when you need an interactive menu.

The `release-optimized` profile is defined in `Cargo.toml` and is the profile
used for distributed binaries. It enables `opt-level = 3`, `lto = "fat"`, and
`codegen-units = 1` for maximum performance.

## Run Commands

```shell
# Preview what would be deleted (safe, no files removed)
cargo run -- --dry-run .\some-folder

# Actually delete a folder (requires --yes to confirm)
cargo run -- --yes .\some-folder

# Delete with custom thread count
cargo run -- --yes --threads 4 .\some-folder

# Delete multiple paths
cargo run -- --yes .\dir1 .\dir2 .\file.txt

# Print help or version
cargo run -- --help
cargo run -- --version
```

**Important:** Always use disposable test directories. Never run `--yes` against
important user data.

## Test Commands

| Command | What it does |
|---------|-------------|
| `cargo test` | Run the full test suite (all modules) |
| `cargo test -- --nocapture` | Run tests with stdout visible (for debugging) |
| `cargo test test_delete_` | Run only tests matching `test_delete_` prefix |
| `cargo test --test-threads=1` | Run tests single-threaded (reduces temp-dir collisions) |

Test coverage by module:
- `cli.rs` — argument parsing, flag validation (`--yes`, `--dry-run`, `--threads`), error messages, duplicate paths.
- `delete.rs` — file/dir deletion, readonly retry, symlink handling, dry-run, error reporting, junction safety (`#[cfg(windows)]` only), idempotency.
- `scan.rs` — entry classification (`File`/`Dir`/`Symlink`), symlink/junction detection, root depth=0 inclusion.
- `protect.rs` — protected-path matching (Windows dirs, user profile dirs, well-known subdirs), filesystem-root detection, path sanitization, symlink-as-non-root.

Tests use atomic counters (`AtomicU64`) for unique temp directories under
`%TEMP%`. Some tests are gated with `#[cfg(windows)]` (junction, symlink_dir)
and won't run on non-Windows platforms.

## Lint & Format Commands

| Command | What it does |
|---------|-------------|
| `cargo clippy -- -D warnings` | Run Clippy linter; **all warnings are errors**. Must pass before committing. |
| `cargo fmt` | Auto-format all Rust code with `rustfmt`. |
| `cargo fmt -- --check` | Check formatting without modifying files (CI-friendly). |
| `cargo clippy --all-targets -- -D warnings` | Lint including tests and benchmarks. |

**Pre-commit checklist** — run these three commands before every commit:
```shell
cargo fmt
cargo clippy -- -D warnings
cargo test
```
All three must succeed with zero errors.

## Git Workflow

- Commit locally when explicitly asked to commit.
- Do **not** push to GitHub, create tags, or publish GitHub releases unless the
  user explicitly asks for push/release/publish/upload.
- If the user asks to "commit" without mentioning GitHub, stop after the local
  commit and report the commit hash.

## CLI Flags

The CLI requires `--yes` or `--force` to perform actual deletion; without it
the command refuses. `--dry-run` previews without deleting and does not require
`--yes`. `--threads N` / `-j N` overrides the Rayon thread pool size (must be ≥ 1).
`--help` / `-h` and `--version` / `-V` print usage and version respectively.

## Coding Style & Naming Conventions

Use Rust 2021 idioms and `rustfmt` defaults. Prefer 4-space indentation, clear
function names in `snake_case`, and type names in `PascalCase`. Keep CLI-facing
messages concise and actionable. Avoid broad refactors when changing deletion
logic; small changes are easier to validate for a destructive tool.

## Safety & Security

The tool is destructive by default. Several guards are in place:

- **Confirmation required** — `--yes` / `--force` must be passed to delete.
- **Protected paths** — `is_protected_path` blocks system directories (Windows,
  Program Files, Users root, well-known profile subdirs, System Volume Information,
  Recovery). Non-Windows platforms return `false` (no protection).
- **Filesystem root refused** — paths with no parent (e.g. `C:\`) are rejected.
- **Symlink/junction safety** — `follow_links(false)` in jwalk; symlinks and
  Windows reparse points are classified as `EntryKind::Symlink` and removed as
  links only, never traversed.
- **Permission recovery** — `remove_file_with_retry` / `remove_dir_with_retry`
  clear the readonly flag and retry on `PermissionDenied`.

When modifying deletion logic, preserve these guards. Avoid following untrusted
paths, preserve explicit user intent for deletion targets, and validate installer
script changes carefully before release.

## Platform Notes

- **Windows** — primary target. Junction and symlink tests require Windows.
  `is_protected_path` is only effective on Windows; on other platforms it
  returns `false`.
- **Symlinks on Windows** — creating symlinks may require Developer Mode or
  elevated privileges. Junctions (`mklink /J`) do not require elevation.
- The `.cargo/config.toml` may contain target-specific linker settings.

