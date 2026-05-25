use crate::delete::DeleteOptions;
use crate::filter::FilterConfig;
use glob::Pattern;
use std::{collections::HashSet, ffi::OsString, io, path::PathBuf};

use owo_colors::{AnsiColors, OwoColorize};

const MAX_THREADS: usize = 1024;

#[derive(Debug)]
pub struct CliOptions {
    pub dry_run: bool,
    pub threads: Option<usize>,
    pub paths: Vec<PathBuf>,
    pub batch: bool,
    pub silent: bool,
    pub filter: FilterConfig,
    pub shred: bool,
    pub only_empty: bool,
    pub recycle: bool,
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
        let opts = if self.shred { opts.shred() } else { opts };
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
}

pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> io::Result<CliAction> {
    let mut dry_run = false;
    let mut force = false;
    let mut batch = false;
    let mut silent = false;
    let mut shred = false;
    let mut only_empty = false;
    let mut recycle = false;
    let mut threads = None;
    let mut paths = Vec::new();
    let mut filter_includes: Vec<Pattern> = Vec::new();
    let mut filter_excludes: Vec<Pattern> = Vec::new();
    let mut min_size: Option<u64> = None;
    let mut newer_than: Option<std::time::SystemTime> = None;
    let mut older_than: Option<std::time::SystemTime> = None;
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.to_str() {
            Some("--dry-run") => dry_run = true,
            Some("--batch") => batch = true,
            Some("--silent") => silent = true,
            Some("--shred") => shred = true,
            Some("--only-empty") => only_empty = true,
            Some("--recycle") => recycle = true,
            Some("--force") | Some("--yes") => force = true,
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
                min_size = Some(s.parse::<u64>().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("--min-size must be a positive integer, got '{s}'"),
                    )
                })?);
            }
            Some("--newer-than") => {
                let value = iter.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--newer-than requires an RFC 3339 date/time",
                    )
                })?;
                let s = value.to_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--newer-than must be valid UTF-8",
                    )
                })?;
                let dt = humantime::parse_rfc3339(s).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid date/time: {e}"),
                    )
                })?;
                newer_than = Some(dt);
            }
            Some("--older-than") => {
                let value = iter.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--older-than requires an RFC 3339 date/time",
                    )
                })?;
                let s = value.to_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "--older-than must be valid UTF-8",
                    )
                })?;
                let dt = humantime::parse_rfc3339(s).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("invalid date/time: {e}"),
                    )
                })?;
                older_than = Some(dt);
            }
            _ => {
                if arg.to_str().is_some_and(|s| s.starts_with("--")) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown flag: {}", arg.to_string_lossy()),
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

    let mut seen: HashSet<PathBuf> = HashSet::with_capacity(paths.len());
    paths.retain(|p| seen.insert(p.clone()));

    // If no --yes/--force and no --dry-run: auto-enable dry-run + prompt.
    // The caller (main.rs) will handle the interactive confirm flow.
    if !dry_run && !force {
        dry_run = true;
    }

    Ok(CliAction::Run(CliOptions {
        dry_run,
        threads,
        paths,
        batch,
        silent,
        filter: FilterConfig {
            includes: filter_includes,
            excludes: filter_excludes,
            min_size,
            newer_than,
            older_than,
        },
        shred,
        only_empty,
        recycle,
    }))
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
    println!();
    println!("Options:");
    println!(
        "  --threads N, -j N   Use N threads for deletion (1–{})",
        MAX_THREADS
    );
    println!("  --include GLOB      Only delete paths matching glob pattern (repeatable)");
    println!("  --exclude GLOB      Skip paths matching glob pattern (repeatable)");
    println!("  --min-size BYTES    Only delete files larger than BYTES");
    println!("  --newer-than RFC3339 Only delete files newer than date/time");
    println!("  --older-than RFC3339 Only delete files older than date/time");
    println!();
    println!("Examples:");
    println!("  zap --dry-run ./node_modules");
    println!("  zap --yes --threads 8 ./build ./dist");
    println!("  zap --yes --exclude '*.log' --min-size 1048576 ./project");
    println!("  zap --yes --shred ./secret-files");
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
    fn test_parse_args_auto_enables_dry_run() {
        let result = parse_args([OsString::from("file.txt")]).unwrap();
        match result {
            CliAction::Run(options) => assert!(options.dry_run),
            _ => panic!("expected run action"),
        }
    }

    #[test]
    fn test_parse_args_accepts_force() {
        let result = parse_args([OsString::from("--yes"), OsString::from("file.txt")]).unwrap();
        match result {
            CliAction::Run(options) => {
                assert!(!options.dry_run);
                assert_eq!(options.threads, None);
                assert_eq!(options.paths, vec![PathBuf::from("file.txt")]);
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
