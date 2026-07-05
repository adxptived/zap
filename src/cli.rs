use crate::delete::DeleteOptions;
use crate::filter::FilterConfig;
use glob::Pattern;
use std::{collections::HashSet, ffi::OsString, io, path::PathBuf};

use owo_colors::{AnsiColors, OwoColorize};

const MAX_THREADS: usize = 1024;

#[derive(Debug, Clone)]
pub struct CliOptions {
    pub dry_run: bool,
    /// Set when neither `--yes`/`--force` nor `--dry-run` was given: the
    /// caller must show a preview and ask for interactive confirmation
    /// before deleting. Kept separate from `dry_run` so an explicit
    /// `--dry-run` (preview only, never prompts) is distinguishable from
    /// "preview, then ask" (previously both collapsed into `dry_run`,
    /// which made the confirmation prompt unreachable).
    pub needs_confirm: bool,
    pub threads: Option<usize>,
    pub paths: Vec<PathBuf>,
    pub batch: bool,
    pub silent: bool,
    pub filter: FilterConfig,
    pub shred: bool,
    pub only_empty: bool,
    pub recycle: bool,
    pub no_size_preview: bool,
    /// Skip writing the operation journal for this run (`--no-journal`).
    pub no_journal: bool,
    /// Number of overwrite passes for `--shred` (`--shred-passes N`,
    /// default [`DEFAULT_SHRED_PASSES`]).
    pub shred_passes: usize,
    /// Emit a machine-readable JSON summary instead of human output
    /// (`--json`).
    pub json: bool,
}

impl CliOptions {
    pub fn to_delete_options(
        &self,
        use_current_pool: bool,
        allow_dangerous: bool,
    ) -> DeleteOptions {
        let opts = if use_current_pool {
            DeleteOptions::default()
        } else {
            DeleteOptions::default().with_threads(self.threads)
        };
        let opts = if self.silent { opts.silent() } else { opts };
        let opts = opts.with_filter(self.filter.clone());
        let opts = if self.shred {
            opts.shred().with_shred_passes(self.shred_passes)
        } else {
            opts
        };
        let opts = if self.only_empty {
            opts.only_empty()
        } else {
            opts
        };
        let opts = if self.recycle { opts.recycle() } else { opts };
        if allow_dangerous {
            opts.allow_dangerous()
        } else {
            opts
        }
    }
}

#[derive(Debug)]
pub enum CliAction {
    Run(CliOptions),
    PrintHelp,
    PrintVersion,
    /// Print the most recent journal entries (`--journal [N]`, default 20).
    ShowJournal(usize),
    /// Delete the operation journal (`--journal-clear`).
    ClearJournal,
    /// Report the N largest files under the given paths without deleting
    /// anything (`--top N <path>...`). `json` selects machine output.
    TopFiles {
        count: usize,
        paths: Vec<PathBuf>,
        json: bool,
    },
}

/// Default number of journal entries shown by `--journal`.
pub const DEFAULT_JOURNAL_ENTRIES: usize = 20;

/// Default number of overwrite passes for `--shred`.
pub const DEFAULT_SHRED_PASSES: usize = 3;

/// Upper bound for `--shred-passes` (Gutmann's 35-pass scheme is the most
/// paranoid published standard; anything beyond it only wastes time).
pub const MAX_SHRED_PASSES: usize = 35;

/// Parse a byte count that optionally carries a binary unit suffix
/// (`k`/`kb`, `m`/`mb`, `g`/`gb`, `t`/`tb`, case-insensitive). Plain
/// integers are interpreted as bytes; fractional values are allowed
/// with a suffix (e.g. `1.5gb`).
fn parse_byte_size(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let lower = s.to_ascii_lowercase();
    let (number, multiplier): (&str, u64) =
        if let Some(n) = lower.strip_suffix("kb").or_else(|| lower.strip_suffix('k')) {
            (n, 1024)
        } else if let Some(n) = lower.strip_suffix("mb").or_else(|| lower.strip_suffix('m')) {
            (n, 1024 * 1024)
        } else if let Some(n) = lower.strip_suffix("gb").or_else(|| lower.strip_suffix('g')) {
            (n, 1024 * 1024 * 1024)
        } else if let Some(n) = lower.strip_suffix("tb").or_else(|| lower.strip_suffix('t')) {
            (n, 1024_u64.pow(4))
        } else {
            return lower.parse::<u64>().ok();
        };
    let number = number.trim();
    if multiplier == 1 {
        return number.parse::<u64>().ok();
    }
    let value = number.parse::<f64>().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let bytes = value * multiplier as f64;
    if bytes > u64::MAX as f64 {
        return None;
    }
    Some(bytes as u64)
}

pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> io::Result<CliAction> {
    let mut dry_run = false;
    let mut force = false;
    let mut batch = false;
    let mut silent = false;
    let mut shred = false;
    let mut only_empty = false;
    let mut recycle = false;
    let mut no_size_preview = false;
    let mut no_journal = false;
    let mut threads = None;
    let mut paths = Vec::new();
    let mut filter_includes: Vec<Pattern> = Vec::new();
    let mut filter_excludes: Vec<Pattern> = Vec::new();
    let mut min_size: Option<u64> = None;
    let mut max_size: Option<u64> = None;
    let mut newer_than: Option<std::time::SystemTime> = None;
    let mut older_than: Option<std::time::SystemTime> = None;
    let mut shred_passes: Option<usize> = None;
    let mut json = false;
    let mut top: Option<usize> = None;
    let mut iter = args.into_iter();
    let mut flags_done = false;

    while let Some(arg) = iter.next() {
        if flags_done {
            paths.push(PathBuf::from(arg));
            continue;
        }
        match arg.to_str() {
            Some("--") => flags_done = true,
            Some("--dry-run") => dry_run = true,
            Some("--batch") => batch = true,
            Some("--silent") => silent = true,
            Some("--shred") => shred = true,
            Some("--only-empty") => only_empty = true,
            Some("--recycle") => recycle = true,
            Some("--no-size-preview") => no_size_preview = true,
            Some("--no-journal") => no_journal = true,
            Some("--json") => json = true,
            Some("--force") | Some("--yes") => force = true,
            Some("--journal-clear") => return Ok(CliAction::ClearJournal),
            Some("--shred-passes") => {
                let value = iter.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--shred-passes requires a pass count",
                    )
                })?;
                let parsed = value.to_str().and_then(|s| s.parse::<usize>().ok());
                let count = parsed
                    .filter(|v| (1..=MAX_SHRED_PASSES).contains(v))
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("--shred-passes must be between 1 and {MAX_SHRED_PASSES}"),
                        )
                    })?;
                shred_passes = Some(count);
                // Asking for overwrite passes only makes sense when
                // shredding — imply --shred for convenience.
                shred = true;
            }
            Some("--top") => {
                let value = iter.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--top requires a file count")
                })?;
                let parsed = value.to_str().and_then(|s| s.parse::<usize>().ok());
                let count = parsed.filter(|v| *v > 0).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--top must be a positive integer",
                    )
                })?;
                top = Some(count);
            }
            Some("--threads") | Some("-j") => {
                let value = iter.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--threads requires a value")
                })?;
                let parsed = value.to_str().and_then(|s| s.parse::<usize>().ok());
                let count = parsed.filter(|v| *v > 0).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--threads must be a positive integer",
                    )
                })?;
                if count > MAX_THREADS {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("--threads must be <= {MAX_THREADS}"),
                    ));
                }
                threads = Some(count);
            }
            Some("--help") | Some("-h") => return Ok(CliAction::PrintHelp),
            Some("--version") | Some("-V") => return Ok(CliAction::PrintVersion),
            Some("--journal") => {
                // Optional count: `--journal 50`. A following path or flag
                // means "use the default count".
                let mut count = DEFAULT_JOURNAL_ENTRIES;
                if let Some(next) = iter.next() {
                    match next.to_str().and_then(|s| s.parse::<usize>().ok()) {
                        Some(n) if n > 0 => count = n,
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "--journal takes an optional positive entry count",
                            ))
                        }
                    }
                }
                return Ok(CliAction::ShowJournal(count));
            }
            Some("--exclude") => {
                let value = iter.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--exclude requires a glob pattern",
                    )
                })?;
                let pat_str = value.to_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--exclude pattern must be valid UTF-8",
                    )
                })?;
                let pattern = Pattern::new(pat_str).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid glob pattern: {e}"),
                    )
                })?;
                filter_excludes.push(pattern);
            }
            Some("--include") => {
                let value = iter.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--include requires a glob pattern",
                    )
                })?;
                let pat_str = value.to_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--include pattern must be valid UTF-8",
                    )
                })?;
                let pattern = Pattern::new(pat_str).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid glob pattern: {e}"),
                    )
                })?;
                filter_includes.push(pattern);
            }
            Some("--min-size") => {
                let value = iter.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--min-size requires a byte count",
                    )
                })?;
                let s = value.to_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--min-size must be valid UTF-8",
                    )
                })?;
                min_size = Some(parse_byte_size(s).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("--min-size must be a byte count like 1048576, 512KB, or 1.5GB, got '{s}'"),
                    )
                })?);
            }
            Some("--max-size") => {
                let value = iter.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--max-size requires a byte count",
                    )
                })?;
                let s = value.to_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--max-size must be valid UTF-8",
                    )
                })?;
                max_size = Some(parse_byte_size(s).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("--max-size must be a byte count like 1048576, 512KB, or 1.5GB, got '{s}'"),
                    )
                })?);
            }
            Some("--newer-than") => {
                let value = iter.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--newer-than requires a date/time or age (e.g. 30d)",
                    )
                })?;
                let s = value.to_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--newer-than must be valid UTF-8",
                    )
                })?;
                let dt = parse_time_spec(s)?;
                newer_than = Some(dt);
            }
            Some("--older-than") => {
                let value = iter.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--older-than requires a date/time or age (e.g. 30d)",
                    )
                })?;
                let s = value.to_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--older-than must be valid UTF-8",
                    )
                })?;
                let dt = parse_time_spec(s)?;
                older_than = Some(dt);
            }
            _ => {
                // Reject anything that looks like a flag (long or short).
                // Paths that genuinely start with '-' can be passed after `--`.
                if arg
                    .to_str()
                    .is_some_and(|s| s.len() > 1 && s.starts_with('-'))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "unknown flag: {} (use `--` before paths that start with '-')",
                            arg.to_string_lossy()
                        ),
                    ));
                }
                paths.push(PathBuf::from(arg))
            }
        }
    }

    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "please provide one or more paths",
        ));
    }

    if let (Some(min), Some(max)) = (min_size, max_size) {
        if max < min {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("--max-size ({max}) must not be smaller than --min-size ({min})"),
            ));
        }
    }

    if let Some(count) = top {
        // Report-only mode: deletion flags are irrelevant and most likely a
        // mistake — reject the destructive ones explicitly.
        if shred || recycle || only_empty {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--top is a report-only mode and cannot be combined with --shred/--recycle/--only-empty",
            ));
        }
        let mut seen: HashSet<PathBuf> = HashSet::with_capacity(paths.len());
        paths.retain(|p| seen.insert(p.clone()));
        return Ok(CliAction::TopFiles { count, paths, json });
    }

    if shred && recycle {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--shred and --recycle are mutually exclusive (shredded files cannot be restored from the Recycle Bin)",
        ));
    }

    if recycle && only_empty {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--recycle and --only-empty are mutually exclusive (an empty directory has no content to recover)",
        ));
    }

    if recycle
        && (!filter_includes.is_empty()
            || !filter_excludes.is_empty()
            || min_size.is_some()
            || max_size.is_some()
            || newer_than.is_some()
            || older_than.is_some())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--recycle cannot be combined with filters (--include/--exclude/--min-size/--max-size/--newer-than/--older-than): the Recycle Bin move always takes the whole item",
        ));
    }

    let mut seen: HashSet<PathBuf> = HashSet::with_capacity(paths.len());
    paths.retain(|p| seen.insert(p.clone()));

    // --json is for scripts: suppress human progress output so stdout
    // carries exactly one parseable JSON object.
    if json {
        silent = true;
    }

    // If no --yes/--force and no --dry-run: the caller (main.rs) must show
    // a preview and ask for interactive confirmation before deleting.
    let needs_confirm = !dry_run && !force;

    Ok(CliAction::Run(CliOptions {
        dry_run,
        needs_confirm,
        threads,
        paths,
        batch,
        silent,
        filter: FilterConfig {
            includes: filter_includes,
            excludes: filter_excludes,
            min_size,
            max_size,
            newer_than,
            older_than,
        },
        shred,
        only_empty,
        recycle,
        no_size_preview,
        no_journal,
        shred_passes: shred_passes.unwrap_or(DEFAULT_SHRED_PASSES),
        json,
    }))
}

/// Parse a point in time given either as RFC 3339 ("2026-01-01T00:00:00Z")
/// or as a humane "age" duration ("30d", "12h", "90min") meaning that long
/// before now. Used by --newer-than / --older-than.
fn parse_time_spec(s: &str) -> io::Result<std::time::SystemTime> {
    if let Ok(dt) = humantime::parse_rfc3339(s) {
        return Ok(dt);
    }
    match humantime::parse_duration(s) {
        Ok(age) => std::time::SystemTime::now()
            .checked_sub(age)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "duration too large")),
        Err(e) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid date/time or duration: {e} (expected RFC 3339 like 2026-01-01T00:00:00Z or an age like 30d, 12h)"),
        )),
    }
}

pub fn print_help() {
    println!("Usage: zap [FLAGS] [OPTIONS] <path>...");
    println!("       zap --help");
    println!("       zap --version");
    println!();
    println!("Without --yes, zap shows a preview and asks for confirmation.");
    println!();
    println!("Flags:");
    println!("  --yes, --force      Confirm deletion (required to actually delete)");
    println!("  --dry-run           Preview what would be deleted without removing");
    println!("  --silent            No output (for background use)");
    println!("  --shred             Overwrite files with random data before deleting");
    println!("  --only-empty        Only delete if directory is empty");
    println!("  --recycle           Send to Recycle Bin instead of permanent delete");
    println!("  --no-size-preview   Skip directory size calculation in the confirmation preview");
    println!("  --no-journal        Do not record this run in the operation journal");
    println!("  --journal [N]       Show the N most recent journal entries (default 20) and exit");
    println!("  --journal-clear     Delete the operation journal and exit");
    println!("  --json              Print a machine-readable JSON summary (implies --silent)");
    println!();
    println!("Options:");
    println!(
        "  --threads N, -j N   Use N threads for deletion (1–{})",
        MAX_THREADS
    );
    println!("  --include GLOB      Only delete paths matching glob pattern (repeatable)");
    println!("  --exclude GLOB      Skip paths matching glob pattern (repeatable)");
    println!(
        "  --min-size SIZE     Only delete files of at least SIZE (bytes, or 512KB/10MB/1.5GB)"
    );
    println!(
        "  --max-size SIZE     Only delete files of at most SIZE (bytes, or 512KB/10MB/1.5GB)"
    );
    println!("  --newer-than WHEN    Only delete files newer than date/time (RFC 3339) or age (e.g. 12h, 30d)");
    println!("  --older-than WHEN    Only delete files older than date/time (RFC 3339) or age (e.g. 12h, 30d)");
    println!(
        "  --shred-passes N    Overwrite passes for --shred (1–{}, default {}; implies --shred)",
        MAX_SHRED_PASSES, DEFAULT_SHRED_PASSES
    );
    println!(
        "  --top N             Report the N largest files under the paths and exit (no deletion)"
    );
    println!("  --                  Treat all remaining arguments as paths");
    println!();
    println!("Examples:");
    println!("  zap --dry-run ./node_modules");
    println!("  zap --yes --threads 8 ./build ./dist");
    println!("  zap --yes --exclude '*.log' --min-size 10MB ./project");
    println!("  zap --yes --shred ./secret-files");
    println!("  zap --yes --shred-passes 7 ./secret-files");
    println!("  zap --top 20 C:\\Users\\me\\Downloads");
    println!("  zap --only-empty ./cache");
}

pub fn print_version() {
    println!("zap {}", env!("CARGO_PKG_VERSION"));
}

pub fn print_error(err: &io::Error) {
    eprintln!(
        "{} {}",
        " ERROR ".on_color(AnsiColors::BrightRed).black(),
        err
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_time_spec_accepts_rfc3339() {
        let dt = parse_time_spec("2026-01-01T00:00:00Z").unwrap();
        assert!(dt < std::time::SystemTime::now());
    }

    #[test]
    fn test_parse_time_spec_accepts_age_duration() {
        let dt = parse_time_spec("30d").unwrap();
        let expected =
            std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 3600);
        let diff = expected.duration_since(dt).unwrap_or_else(|e| e.duration());
        assert!(diff < std::time::Duration::from_secs(5), "diff {diff:?}");
    }

    #[test]
    fn test_parse_time_spec_rejects_garbage() {
        assert!(parse_time_spec("yesterday-ish").is_err());
    }

    #[test]
    fn test_parse_args_journal_default_and_explicit_count() {
        match parse_args([OsString::from("--journal")]).unwrap() {
            CliAction::ShowJournal(n) => assert_eq!(n, DEFAULT_JOURNAL_ENTRIES),
            other => panic!("expected ShowJournal, got {other:?}"),
        }
        match parse_args([OsString::from("--journal"), OsString::from("50")]).unwrap() {
            CliAction::ShowJournal(n) => assert_eq!(n, 50),
            other => panic!("expected ShowJournal, got {other:?}"),
        }
        assert!(parse_args([OsString::from("--journal"), OsString::from("0")]).is_err());
        assert!(parse_args([OsString::from("--journal"), OsString::from("abc")]).is_err());
    }

    #[test]
    fn test_parse_args_journal_clear() {
        match parse_args([OsString::from("--journal-clear")]).unwrap() {
            CliAction::ClearJournal => {}
            other => panic!("expected ClearJournal, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_args_top_files() {
        match parse_args([
            OsString::from("--top"),
            OsString::from("10"),
            OsString::from("dir"),
        ])
        .unwrap()
        {
            CliAction::TopFiles { count, paths, json } => {
                assert_eq!(count, 10);
                assert_eq!(paths, vec![PathBuf::from("dir")]);
                assert!(!json);
            }
            other => panic!("expected TopFiles, got {other:?}"),
        }
        // --json flows into the report mode.
        match parse_args([
            OsString::from("--top"),
            OsString::from("5"),
            OsString::from("--json"),
            OsString::from("dir"),
        ])
        .unwrap()
        {
            CliAction::TopFiles { json, .. } => assert!(json),
            other => panic!("expected TopFiles, got {other:?}"),
        }
        assert!(parse_args([OsString::from("--top"), OsString::from("0")]).is_err());
        assert!(parse_args([OsString::from("--top"), OsString::from("abc")]).is_err());
        // Report mode must reject destructive companions.
        assert!(parse_args([
            OsString::from("--top"),
            OsString::from("5"),
            OsString::from("--shred"),
            OsString::from("dir"),
        ])
        .is_err());
    }

    #[test]
    fn test_parse_args_shred_passes() {
        match parse_args([
            OsString::from("--shred-passes"),
            OsString::from("7"),
            OsString::from("--yes"),
            OsString::from("file.txt"),
        ])
        .unwrap()
        {
            CliAction::Run(options) => {
                assert_eq!(options.shred_passes, 7);
                assert!(options.shred, "--shred-passes must imply --shred");
            }
            other => panic!("expected Run, got {other:?}"),
        }
        // Default without the flag.
        match parse_args([OsString::from("--yes"), OsString::from("file.txt")]).unwrap() {
            CliAction::Run(options) => {
                assert_eq!(options.shred_passes, DEFAULT_SHRED_PASSES);
                assert!(!options.shred);
            }
            other => panic!("expected Run, got {other:?}"),
        }
        assert!(parse_args([OsString::from("--shred-passes"), OsString::from("0")]).is_err());
        assert!(parse_args([OsString::from("--shred-passes"), OsString::from("36")]).is_err());
    }

    #[test]
    fn test_parse_args_max_size() {
        match parse_args([
            OsString::from("--max-size"),
            OsString::from("10MB"),
            OsString::from("--yes"),
            OsString::from("dir"),
        ])
        .unwrap()
        {
            CliAction::Run(options) => {
                assert_eq!(options.filter.max_size, Some(10 * 1024 * 1024));
            }
            other => panic!("expected Run, got {other:?}"),
        }
        // max below min is a contradiction.
        assert!(parse_args([
            OsString::from("--min-size"),
            OsString::from("10MB"),
            OsString::from("--max-size"),
            OsString::from("1MB"),
            OsString::from("--yes"),
            OsString::from("dir"),
        ])
        .is_err());
        // --recycle refuses filters, including the new one.
        assert!(parse_args([
            OsString::from("--recycle"),
            OsString::from("--max-size"),
            OsString::from("1MB"),
            OsString::from("--yes"),
            OsString::from("dir"),
        ])
        .is_err());
    }

    #[test]
    fn test_parse_args_json_implies_silent() {
        match parse_args([
            OsString::from("--json"),
            OsString::from("--yes"),
            OsString::from("file.txt"),
        ])
        .unwrap()
        {
            CliAction::Run(options) => {
                assert!(options.json);
                assert!(options.silent, "--json must imply --silent");
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_args_auto_enables_dry_run() {
        let result = parse_args([OsString::from("file.txt")]).unwrap();
        match result {
            CliAction::Run(options) => {
                assert!(!options.dry_run);
                assert!(options.needs_confirm);
            }
            _ => panic!("expected run action"),
        }
    }

    #[test]
    fn test_parse_args_accepts_no_size_preview() {
        let result = parse_args([
            OsString::from("--no-size-preview"),
            OsString::from("file.txt"),
        ])
        .unwrap();
        match result {
            CliAction::Run(options) => {
                assert!(options.needs_confirm);
                assert!(options.no_size_preview);
            }
            _ => panic!("expected run action"),
        }
    }

    #[test]
    fn test_parse_args_accepts_force() {
        let result = parse_args([OsString::from("--yes"), OsString::from("file.txt")]).unwrap();
        match result {
            CliAction::Run(options) => {
                assert!(!options.dry_run);
                assert!(!options.needs_confirm);
                assert_eq!(options.threads, None);
                assert_eq!(options.paths, vec![PathBuf::from("file.txt")]);
                assert!(!options.no_size_preview);
            }
            _ => panic!("expected run action"),
        }
    }

    #[test]
    fn test_parse_args_accepts_dry_run_without_force() {
        let result = parse_args([OsString::from("--dry-run"), OsString::from("file.txt")]).unwrap();
        match result {
            CliAction::Run(options) => {
                assert!(options.dry_run);
                assert!(!options.needs_confirm);
                assert_eq!(options.threads, None);
                assert_eq!(options.paths, vec![PathBuf::from("file.txt")]);
            }
            _ => panic!("expected run action"),
        }
    }

    #[test]
    fn test_parse_args_dry_run_with_yes_acts_as_dry_run() {
        let result = parse_args([
            OsString::from("--dry-run"),
            OsString::from("--yes"),
            OsString::from("file.txt"),
        ])
        .unwrap();
        match result {
            CliAction::Run(options) => {
                assert!(options.dry_run);
                assert!(!options.needs_confirm);
                assert_eq!(options.threads, None);
            }
            _ => panic!("expected run action"),
        }
    }

    #[test]
    fn test_parse_args_multiple_paths() {
        let result = parse_args([
            OsString::from("--yes"),
            OsString::from("path1"),
            OsString::from("path2"),
            OsString::from("path3"),
        ])
        .unwrap();
        match result {
            CliAction::Run(options) => {
                assert!(!options.dry_run);
                assert_eq!(options.threads, None);
                assert_eq!(
                    options.paths,
                    vec![
                        PathBuf::from("path1"),
                        PathBuf::from("path2"),
                        PathBuf::from("path3"),
                    ]
                );
            }
            _ => panic!("expected run action"),
        }
    }

    #[test]
    fn test_parse_args_deduplicates_paths_preserving_order() {
        let result = parse_args([
            OsString::from("--yes"),
            OsString::from("a"),
            OsString::from("b"),
            OsString::from("a"),
            OsString::from("c"),
            OsString::from("b"),
        ])
        .unwrap();
        match result {
            CliAction::Run(options) => {
                assert_eq!(
                    options.paths,
                    vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c"),]
                );
            }
            _ => panic!("expected run action"),
        }
    }

    #[test]
    fn test_parse_args_accepts_threads() {
        let result = parse_args([
            OsString::from("--yes"),
            OsString::from("--threads"),
            OsString::from("2"),
            OsString::from("file.txt"),
        ])
        .unwrap();
        match result {
            CliAction::Run(options) => {
                assert_eq!(options.threads, Some(2));
                assert_eq!(options.paths, vec![PathBuf::from("file.txt")]);
            }
            _ => panic!("expected run action"),
        }
    }

    #[test]
    fn test_parse_args_rejects_zero_threads() {
        let result = parse_args([
            OsString::from("--yes"),
            OsString::from("--threads"),
            OsString::from("0"),
            OsString::from("file.txt"),
        ]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("positive integer"));
    }

    #[test]
    fn test_parse_args_rejects_excessive_threads() {
        let result = parse_args([
            OsString::from("--yes"),
            OsString::from("--threads"),
            OsString::from("100000"),
            OsString::from("file.txt"),
        ]);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains(&format!("<= {MAX_THREADS}")));
    }

    #[test]
    fn test_parse_args_double_dash_treats_rest_as_paths() {
        let result = parse_args([
            OsString::from("--yes"),
            OsString::from("--"),
            OsString::from("--weird-name"),
            OsString::from("-x"),
        ])
        .unwrap();
        match result {
            CliAction::Run(options) => {
                assert_eq!(
                    options.paths,
                    vec![PathBuf::from("--weird-name"), PathBuf::from("-x")]
                );
            }
            _ => panic!("expected run action"),
        }
    }

    #[test]
    fn test_parse_args_rejects_unknown_short_flag() {
        let result = parse_args([OsString::from("-x"), OsString::from("file.txt")]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown flag"));
    }

    #[test]
    fn test_parse_args_rejects_recycle_with_only_empty() {
        let args = vec![
            OsString::from("--recycle"),
            OsString::from("--only-empty"),
            OsString::from("some-path"),
        ];
        assert!(parse_args(args.into_iter()).is_err());
    }

    #[test]
    fn test_parse_args_rejects_recycle_with_filters() {
        let args = vec![
            OsString::from("--recycle"),
            OsString::from("--min-size"),
            OsString::from("1mb"),
            OsString::from("some-path"),
        ];
        assert!(parse_args(args.into_iter()).is_err());
    }

    #[test]
    fn test_parse_args_rejects_shred_with_recycle() {
        let result = parse_args([
            OsString::from("--yes"),
            OsString::from("--shred"),
            OsString::from("--recycle"),
            OsString::from("file.txt"),
        ]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("mutually exclusive"));
    }

    #[test]
    fn test_parse_byte_size_plain_bytes() {
        assert_eq!(parse_byte_size("0"), Some(0));
        assert_eq!(parse_byte_size("1048576"), Some(1048576));
    }

    #[test]
    fn test_parse_byte_size_with_suffixes() {
        assert_eq!(parse_byte_size("1k"), Some(1024));
        assert_eq!(parse_byte_size("512KB"), Some(512 * 1024));
        assert_eq!(parse_byte_size("10MB"), Some(10 * 1024 * 1024));
        assert_eq!(parse_byte_size("1g"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_byte_size("1TB"), Some(1024_u64.pow(4)));
        assert_eq!(
            parse_byte_size("1.5GB"),
            Some((1.5 * 1024.0 * 1024.0 * 1024.0) as u64)
        );
    }

    #[test]
    fn test_parse_byte_size_rejects_garbage() {
        assert_eq!(parse_byte_size(""), None);
        assert_eq!(parse_byte_size("abc"), None);
        assert_eq!(parse_byte_size("-5"), None);
        assert_eq!(parse_byte_size("-1kb"), None);
        assert_eq!(parse_byte_size("kb"), None);
        assert_eq!(parse_byte_size("1.5"), None); // fractional bytes need a unit
    }

    #[test]
    fn test_parse_args_min_size_accepts_human_units() {
        let result = parse_args([
            OsString::from("--yes"),
            OsString::from("--min-size"),
            OsString::from("10MB"),
            OsString::from("file.txt"),
        ])
        .unwrap();
        match result {
            CliAction::Run(options) => {
                assert_eq!(options.filter.min_size, Some(10 * 1024 * 1024));
            }
            _ => panic!("expected run action"),
        }
    }

    #[test]
    fn test_parse_args_accepts_threads_at_cap() {
        let result = parse_args([
            OsString::from("--yes"),
            OsString::from("--threads"),
            OsString::from(MAX_THREADS.to_string()),
            OsString::from("file.txt"),
        ])
        .unwrap();
        match result {
            CliAction::Run(options) => assert_eq!(options.threads, Some(MAX_THREADS)),
            _ => panic!("expected run action"),
        }
    }
}
