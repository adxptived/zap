use glob::Pattern;
use std::path::Path;
use std::time::SystemTime;

fn glob_opts() -> glob::MatchOptions {
    glob::MatchOptions {
        case_sensitive: false,
        require_literal_separator: false,
        require_literal_leading_dot: false,
    }
}

#[derive(Debug, Clone, Default)]
pub struct FilterConfig {
    pub includes: Vec<Pattern>,
    pub excludes: Vec<Pattern>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
    pub newer_than: Option<SystemTime>,
    pub older_than: Option<SystemTime>,
}

impl FilterConfig {
    pub fn is_empty(&self) -> bool {
        self.includes.is_empty()
            && self.excludes.is_empty()
            && self.min_size.is_none()
            && self.max_size.is_none()
            && self.newer_than.is_none()
            && self.older_than.is_none()
    }

    pub fn needs_metadata(&self) -> bool {
        self.min_size.is_some()
            || self.max_size.is_some()
            || self.newer_than.is_some()
            || self.older_than.is_some()
    }

    pub fn matches_relative(
        &self,
        full_path: &Path,
        base_dir: &Path,
        is_file: bool,
        file_size: Option<u64>,
        modified_time: Option<SystemTime>,
    ) -> bool {
        if is_file {
            if let Some(min) = self.min_size {
                if file_size.unwrap_or(0) < min {
                    return false;
                }
            }
            if let Some(max) = self.max_size {
                // Unknown size fails closed for --max-size: a file we could
                // not stat may well be huge, so do not delete it.
                match file_size {
                    Some(size) if size <= max => {}
                    _ => return false,
                }
            }
        }

        if let Some(modified) = modified_time {
            if let Some(cutoff) = self.newer_than {
                if modified < cutoff {
                    return false;
                }
            }
            if let Some(cutoff) = self.older_than {
                if modified > cutoff {
                    return false;
                }
            }
        }

        if self.includes.is_empty() && self.excludes.is_empty() {
            return true;
        }

        let relative = full_path.strip_prefix(base_dir).unwrap_or(full_path);
        let rel_str = relative.to_string_lossy();
        let opts = glob_opts();

        if !self.includes.is_empty()
            && !self.includes.iter().any(|p| p.matches_with(&rel_str, opts))
        {
            return false;
        }

        if self.excludes.iter().any(|p| p.matches_with(&rel_str, opts)) {
            return false;
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glob::Pattern;
    use std::fs::{self};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn create_test_dir() -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("zap-filter-test-{id}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn file_info(path: &Path) -> (u64, Option<SystemTime>) {
        let meta = fs::metadata(path).unwrap();
        (meta.len(), meta.modified().ok())
    }

    #[test]
    fn test_include_only_matches_glob() {
        let dir = create_test_dir();
        let pat = Pattern::new("*.txt").unwrap();
        let cfg = FilterConfig {
            includes: vec![pat],
            ..Default::default()
        };
        let f = dir.join("hello.txt");
        fs::write(&f, b"data").unwrap();
        let (sz, mt) = file_info(&f);
        assert!(cfg.matches_relative(&f, &dir, true, Some(sz), mt));

        let f2 = dir.join("hello.rs");
        fs::write(&f2, b"data").unwrap();
        let (sz2, mt2) = file_info(&f2);
        assert!(!cfg.matches_relative(&f2, &dir, true, Some(sz2), mt2));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_exclude_filters_out_glob() {
        let dir = create_test_dir();
        let pat = Pattern::new("*.tmp").unwrap();
        let cfg = FilterConfig {
            excludes: vec![pat],
            ..Default::default()
        };
        let f = dir.join("junk.tmp");
        fs::write(&f, b"data").unwrap();
        let (sz, mt) = file_info(&f);
        assert!(!cfg.matches_relative(&f, &dir, true, Some(sz), mt));

        let f2 = dir.join("keep.txt");
        fs::write(&f2, b"data").unwrap();
        let (sz2, mt2) = file_info(&f2);
        assert!(cfg.matches_relative(&f2, &dir, true, Some(sz2), mt2));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_include_and_exclude_exclude_wins() {
        let dir = create_test_dir();
        let inc = Pattern::new("*.log").unwrap();
        let exc = Pattern::new("errors.log").unwrap();
        let cfg = FilterConfig {
            includes: vec![inc],
            excludes: vec![exc],
            ..Default::default()
        };
        let f = dir.join("errors.log");
        fs::write(&f, b"data").unwrap();
        let (sz, mt) = file_info(&f);
        assert!(!cfg.matches_relative(&f, &dir, true, Some(sz), mt));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_min_size_filters() {
        let dir = create_test_dir();
        let cfg = FilterConfig {
            min_size: Some(10),
            ..Default::default()
        };
        let small = dir.join("small.txt");
        fs::write(&small, b"short").unwrap();
        let (sz_s, mt_s) = file_info(&small);
        let big = dir.join("big.txt");
        fs::write(&big, b"long enough data").unwrap();
        let (sz_b, mt_b) = file_info(&big);
        assert!(!cfg.matches_relative(&small, &dir, true, Some(sz_s), mt_s));
        assert!(cfg.matches_relative(&big, &dir, true, Some(sz_b), mt_b));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dir_always_passes_size_filter() {
        let dir = create_test_dir();
        let sub = dir.join("subdir");
        fs::create_dir(&sub).unwrap();
        let cfg = FilterConfig {
            min_size: Some(1000),
            ..Default::default()
        };
        assert!(cfg.matches_relative(&sub, &dir, false, None, None));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_newer_than_filter() {
        let dir = create_test_dir();
        fs::write(dir.join("old.txt"), b"data").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let cutoff = SystemTime::now();
        std::thread::sleep(Duration::from_millis(50));
        fs::write(dir.join("recent.txt"), b"data").unwrap();
        let cfg = FilterConfig {
            newer_than: Some(cutoff),
            ..Default::default()
        };
        let (_, mt_old) = file_info(&dir.join("old.txt"));
        let (_, mt_recent) = file_info(&dir.join("recent.txt"));
        assert!(!cfg.matches_relative(&dir.join("old.txt"), &dir, true, None, mt_old));
        assert!(cfg.matches_relative(&dir.join("recent.txt"), &dir, true, None, mt_recent));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_older_than_filter() {
        let dir = create_test_dir();
        fs::write(dir.join("ancient.txt"), b"data").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        let cutoff = SystemTime::now();
        std::thread::sleep(Duration::from_millis(50));
        fs::write(dir.join("fresh.txt"), b"data").unwrap();
        let cfg = FilterConfig {
            older_than: Some(cutoff),
            ..Default::default()
        };
        let (_, mt_ancient) = file_info(&dir.join("ancient.txt"));
        let (_, mt_fresh) = file_info(&dir.join("fresh.txt"));
        assert!(cfg.matches_relative(&dir.join("ancient.txt"), &dir, true, None, mt_ancient));
        assert!(!cfg.matches_relative(&dir.join("fresh.txt"), &dir, true, None, mt_fresh));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_is_empty_returns_true_for_no_filters() {
        let cfg = FilterConfig::default();
        assert!(cfg.is_empty());
    }

    #[test]
    fn test_needs_metadata_only_for_size_or_time_filters() {
        assert!(!FilterConfig::default().needs_metadata());
        assert!(!FilterConfig {
            includes: vec![Pattern::new("*.txt").unwrap()],
            ..Default::default()
        }
        .needs_metadata());
        assert!(FilterConfig {
            min_size: Some(1),
            ..Default::default()
        }
        .needs_metadata());
        assert!(FilterConfig {
            newer_than: Some(SystemTime::now()),
            ..Default::default()
        }
        .needs_metadata());
    }

    #[test]
    fn test_include_matches_dir_by_name() {
        let dir = create_test_dir();
        let sub = dir.join("target");
        fs::create_dir(&sub).unwrap();

        let pat_sub = Pattern::new("target").unwrap();
        let pat_other = Pattern::new("other").unwrap();

        let cfg_sub = FilterConfig {
            includes: vec![pat_sub],
            ..Default::default()
        };
        let cfg_other = FilterConfig {
            includes: vec![pat_other],
            ..Default::default()
        };

        assert!(cfg_sub.matches_relative(&sub, &dir, false, None, None));
        assert!(!cfg_other.matches_relative(&sub, &dir, false, None, None));
        let _ = fs::remove_dir_all(&dir);
    }
}
