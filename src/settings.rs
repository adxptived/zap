//! User settings shared by `zapg`, `zapw` and the context-menu registration.
//!
//! Stored as a small `key = value` file at `%APPDATA%\zap\settings.ini` so it
//! stays human-editable without pulling in a serialization dependency.
//! Unknown keys are ignored and missing keys fall back to defaults, so files
//! written by older or newer versions keep working.

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    /// Follow the Windows display language.
    #[default]
    Auto,
    English,
    Russian,
}

impl Language {
    pub const ALL: [Language; 3] = [Language::Auto, Language::English, Language::Russian];

    fn key(self) -> &'static str {
        match self {
            Language::Auto => "auto",
            Language::English => "en",
            Language::Russian => "ru",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.key() == value)
    }

    /// Resolve `Auto` to a concrete language.
    pub fn resolve(self) -> Language {
        match self {
            Language::Auto => system_language(),
            other => other,
        }
    }
}

#[cfg(windows)]
fn system_language() -> Language {
    extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    const LANG_RUSSIAN: u16 = 0x19;
    // SAFETY: no arguments, returns a LANGID.
    let lang_id = unsafe { GetUserDefaultUILanguage() };
    if lang_id & 0x3ff == LANG_RUSSIAN {
        Language::Russian
    } else {
        Language::English
    }
}

#[cfg(not(windows))]
fn system_language() -> Language {
    Language::English
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MenuLayout {
    /// One "Zap ▸" entry with the actions in a submenu.
    #[default]
    Submenu,
    /// Actions directly in the context menu.
    Flat,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DefaultMode {
    #[default]
    Delete,
    Recycle,
    Shred,
}

impl DefaultMode {
    fn key(self) -> &'static str {
        match self {
            DefaultMode::Delete => "delete",
            DefaultMode::Recycle => "recycle",
            DefaultMode::Shred => "shred",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [
            DefaultMode::Delete,
            DefaultMode::Recycle,
            DefaultMode::Shred,
        ]
        .into_iter()
        .find(|m| m.key() == value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuSettings {
    pub enabled: bool,
    pub layout: MenuLayout,
    pub icons: bool,
    /// "Delete…" — opens the confirmation dialog.
    pub show_dialog: bool,
    /// "Move to Recycle Bin" — silent, recoverable.
    pub show_recycle: bool,
    /// "Delete permanently" — silent, no confirmation.
    pub show_quick_delete: bool,
    /// Entry on the folder background ("Delete this folder…", dialog only).
    pub show_background: bool,
    /// Only show the entries on Shift + right-click.
    pub shift_only: bool,
}

impl Default for MenuSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            layout: MenuLayout::Submenu,
            icons: true,
            show_dialog: true,
            show_recycle: true,
            show_quick_delete: true,
            show_background: true,
            shift_only: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub language: Language,
    pub menu: MenuSettings,
    /// Mode preselected when the dialog opens.
    pub default_mode: DefaultMode,
    /// Close the dialog automatically after a clean run.
    pub auto_close: bool,
    /// Record runs in the operation journal.
    pub journal: bool,
    /// Show a message when a silent context-menu deletion fails.
    pub notify_failures: bool,
    /// Worker threads; `None` = automatic.
    pub threads: Option<usize>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            language: Language::Auto,
            menu: MenuSettings::default(),
            default_mode: DefaultMode::Delete,
            auto_close: true,
            journal: true,
            notify_failures: true,
            threads: None,
        }
    }
}

pub fn settings_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("zap").join("settings.ini")
}

impl Settings {
    /// Load settings, falling back to defaults when the file is missing or
    /// unreadable. Never fails: a broken settings file must not block a
    /// deletion started from Explorer.
    pub fn load() -> Self {
        std::fs::read_to_string(settings_path())
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    pub fn save(&self) -> io::Result<()> {
        let path = settings_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Write-then-rename so a crash never leaves a truncated file.
        let tmp = path.with_extension("ini.tmp");
        std::fs::write(&tmp, self.to_text())?;
        std::fs::rename(&tmp, &path)
    }

    pub fn parse(text: &str) -> Self {
        let mut s = Settings::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            let flag = parse_bool(value);
            match key {
                "language" => s.language = Language::parse(value).unwrap_or(s.language),
                "default_mode" => {
                    s.default_mode = DefaultMode::parse(value).unwrap_or(s.default_mode)
                }
                "auto_close" => s.auto_close = flag.unwrap_or(s.auto_close),
                "journal" => s.journal = flag.unwrap_or(s.journal),
                "notify_failures" => s.notify_failures = flag.unwrap_or(s.notify_failures),
                "threads" => {
                    s.threads = value
                        .parse::<usize>()
                        .ok()
                        .filter(|n| (1..=crate::parallelism::MAX_WORKERS).contains(n))
                }
                "menu.enabled" => s.menu.enabled = flag.unwrap_or(s.menu.enabled),
                "menu.layout" => {
                    s.menu.layout = match value {
                        "flat" => MenuLayout::Flat,
                        "submenu" => MenuLayout::Submenu,
                        _ => s.menu.layout,
                    }
                }
                "menu.icons" => s.menu.icons = flag.unwrap_or(s.menu.icons),
                "menu.dialog" => s.menu.show_dialog = flag.unwrap_or(s.menu.show_dialog),
                "menu.recycle" => s.menu.show_recycle = flag.unwrap_or(s.menu.show_recycle),
                "menu.quick_delete" => {
                    s.menu.show_quick_delete = flag.unwrap_or(s.menu.show_quick_delete)
                }
                "menu.background" => {
                    s.menu.show_background = flag.unwrap_or(s.menu.show_background)
                }
                "menu.shift_only" => s.menu.shift_only = flag.unwrap_or(s.menu.shift_only),
                _ => {}
            }
        }
        s
    }

    pub fn to_text(&self) -> String {
        let mut out = String::from("# Zap settings. Edit in the Zap settings window or by hand.\n");
        let mut kv = |k: &str, v: &str| {
            let _ = writeln!(out, "{k} = {v}");
        };
        kv("language", self.language.key());
        kv("default_mode", self.default_mode.key());
        kv("auto_close", bool_str(self.auto_close));
        kv("journal", bool_str(self.journal));
        kv("notify_failures", bool_str(self.notify_failures));
        kv(
            "threads",
            &self
                .threads
                .map_or_else(|| "auto".to_owned(), |n| n.to_string()),
        );
        kv("menu.enabled", bool_str(self.menu.enabled));
        kv(
            "menu.layout",
            match self.menu.layout {
                MenuLayout::Submenu => "submenu",
                MenuLayout::Flat => "flat",
            },
        );
        kv("menu.icons", bool_str(self.menu.icons));
        kv("menu.dialog", bool_str(self.menu.show_dialog));
        kv("menu.recycle", bool_str(self.menu.show_recycle));
        kv("menu.quick_delete", bool_str(self.menu.show_quick_delete));
        kv("menu.background", bool_str(self.menu.show_background));
        kv("menu.shift_only", bool_str(self.menu.shift_only));
        out
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn bool_str(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_text() {
        let settings = Settings {
            language: Language::Russian,
            menu: MenuSettings {
                enabled: true,
                layout: MenuLayout::Flat,
                icons: false,
                show_dialog: true,
                show_recycle: false,
                show_quick_delete: true,
                show_background: false,
                shift_only: true,
            },
            default_mode: DefaultMode::Recycle,
            auto_close: false,
            journal: false,
            notify_failures: false,
            threads: Some(6),
        };
        assert_eq!(Settings::parse(&settings.to_text()), settings);
    }

    #[test]
    fn tolerates_garbage_and_unknown_keys() {
        let parsed = Settings::parse(
            "# comment\nmenu.icons = off\nthreads = 999\nfuture.option = 1\nnot a line\nlanguage = xx\n",
        );
        let expected = Settings {
            menu: MenuSettings {
                icons: false,
                ..MenuSettings::default()
            },
            ..Settings::default()
        };
        assert_eq!(parsed, expected);
    }
}
