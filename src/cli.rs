use std::{collections::HashSet, ffi::OsString, io, path::PathBuf};

use owo_colors::{AnsiColors, OwoColorize};

/// Upper bound for `--threads`. Anything above this is almost certainly a typo
/// and would just waste memory creating an oversized Rayon pool.
const MAX_THREADS: usize = 1024;

#[derive(Debug)]
pub struct CliOptions {
    pub dry_run: bool,
    pub threads: Option<usize>,
    pub paths: Vec<PathBuf>,
    pub batch: bool,
    pub silent: bool,
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
    let mut threads = None;
    let mut paths = Vec::new();
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        match arg.to_str() {
            Some("--dry-run") => dry_run = true,
            Some("--batch") => batch = true,
            Some("--silent") => silent = true,
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
            _ => paths.push(PathBuf::from(arg)),
        }
    }

    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "please provide one or more paths",
        ));
    }

    // Deduplicate paths preserving first-seen order. The deletion engine is
    // idempotent on duplicates (second attempt fails with NotFound) but
    // de-duping avoids spurious failure entries in the summary and saves
    // unnecessary scan work.
    let mut seen: HashSet<PathBuf> = HashSet::with_capacity(paths.len());
    paths.retain(|p| seen.insert(p.clone()));

    if !dry_run && !force {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to delete without --yes or --force; use --dry-run to preview",
        ));
    }

    Ok(CliAction::Run(CliOptions {
        dry_run,
        threads,
        paths,
        batch,
        silent,
    }))
}

pub fn print_help() {
    println!("Usage: zap [--yes|--force] [--dry-run] [--silent] [--threads N] <path>...");
    println!("       zap --help");
    println!("       zap --version");
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
    fn test_parse_args_requires_force_for_delete() {
        let result = parse_args([OsString::from("file.txt")]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("without --yes or --force"));
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
                assert!(
                    options.dry_run,
                    "--dry-run should take precedence over --yes"
                );
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
        assert!(
            msg.contains(&format!("<= {MAX_THREADS}")),
            "unexpected error message: {msg}"
        );
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
