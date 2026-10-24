//! Explorer context-menu registration (per user, `HKCU\Software\Classes`).
//!
//! The menu is built from [`Settings`]: submenu or flat layout, icons on or
//! off, which actions appear, Shift-only, and the label language. Applying
//! settings always removes every key Zap has ever registered first, so
//! switching layouts never leaves stale entries behind.
//!
//! Safety: the folder-background entry targets the folder being viewed, so
//! it only ever opens the confirmation dialog — never a silent delete.

use crate::settings::{Language, MenuLayout, Settings};
use std::path::{Path, PathBuf};

pub const CLASSES: &str = r"Software\Classes";
const OBJECT_SHELL: &str = r"AllFilesystemObjects\shell";
const BACKGROUND_SHELL: &str = r"Directory\Background\shell";
const EXPLORER_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer";
/// Explorer refuses multi-item invokes above this many items (default 15).
const MAX_CONTEXT_MENU_SELECTIONS: u32 = 10_000;
/// `ECF_SEPARATORBEFORE`: draw a separator above the entry.
const SEPARATOR_BEFORE: u32 = 0x20;

/// Every key Zap (or its predecessor turbo-delete) may have created,
/// relative to `HKCU\Software\Classes`.
const ALL_KNOWN_KEYS: &[&str] = &[
    r"AllFilesystemObjects\shell\zap",
    r"AllFilesystemObjects\shell\zap-1-dialog",
    r"AllFilesystemObjects\shell\zap-2-recycle",
    r"AllFilesystemObjects\shell\zap-3-delete",
    r"Directory\Background\shell\zap",
    r"Directory\shell\zap",
    r"*\shell\zap",
    r"Directory\shell\turbo-delete",
    r"Directory\Background\shell\turbo-delete",
    r"*\shell\turbo-delete",
    r"AllFilesystemObjects\shell\turbo-delete",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegValue {
    Str(String),
    Dword(u32),
}

/// One registry key to create under HKCU, with its values (`""` = default).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeySpec {
    pub path: String,
    pub values: Vec<(String, RegValue)>,
}

impl KeySpec {
    fn new(path: String) -> Self {
        Self {
            path,
            values: Vec::new(),
        }
    }

    fn str(mut self, name: &str, value: impl Into<String>) -> Self {
        self.values
            .push((name.to_owned(), RegValue::Str(value.into())));
        self
    }

    fn dword(mut self, name: &str, value: u32) -> Self {
        self.values.push((name.to_owned(), RegValue::Dword(value)));
        self
    }
}

/// Executables the menu commands point at.
#[derive(Clone, Debug)]
pub struct MenuTargets {
    pub zapg: PathBuf,
    pub zapw: PathBuf,
}

impl MenuTargets {
    /// `zapg.exe` / `zapw.exe` next to the running executable.
    pub fn beside_current_exe() -> std::io::Result<Self> {
        let exe = std::env::current_exe()?;
        let dir = exe.parent().unwrap_or(Path::new("."));
        Ok(Self {
            zapg: dir.join("zapg.exe"),
            zapw: dir.join("zapw.exe"),
        })
    }
}

struct Labels {
    dialog: &'static str,
    recycle: &'static str,
    quick: &'static str,
    background: &'static str,
}

fn labels(language: Language, layout: MenuLayout) -> Labels {
    match (language.resolve(), layout) {
        (Language::Russian, MenuLayout::Submenu) => Labels {
            dialog: "Удалить…",
            recycle: "Переместить в корзину",
            quick: "Удалить безвозвратно",
            background: "Удалить эту папку через Zap…",
        },
        (Language::Russian, MenuLayout::Flat) => Labels {
            dialog: "Удалить через Zap…",
            recycle: "В корзину через Zap",
            quick: "Удалить безвозвратно через Zap",
            background: "Удалить эту папку через Zap…",
        },
        (_, MenuLayout::Submenu) => Labels {
            dialog: "Delete…",
            recycle: "Move to Recycle Bin",
            quick: "Delete permanently",
            background: "Delete this folder with Zap…",
        },
        (_, MenuLayout::Flat) => Labels {
            dialog: "Delete with Zap…",
            recycle: "Recycle with Zap",
            quick: "Delete permanently with Zap",
            background: "Delete this folder with Zap…",
        },
    }
}

fn quoted(path: &Path) -> String {
    format!("\"{}\"", path.display())
}

/// Registry keys for `settings`. Pure: no registry access, so it is unit
/// tested directly.
pub fn plan(settings: &Settings, targets: &MenuTargets) -> Vec<KeySpec> {
    let menu = &settings.menu;
    let mut keys = Vec::new();
    if !menu.enabled {
        return keys;
    }
    let text = labels(settings.language, menu.layout);
    let icon = menu.icons.then(|| format!("{},0", quoted(&targets.zapg)));
    let zapg = quoted(&targets.zapg);
    let zapw = quoted(&targets.zapw);

    // (key suffix, label, command, separator before)
    let mut actions: Vec<(&str, &str, String, bool)> = Vec::new();
    if menu.show_dialog {
        actions.push((
            "1-dialog",
            text.dialog,
            format!("{zapg} --batch \"%1\""),
            false,
        ));
    }
    if menu.show_recycle {
        actions.push((
            "2-recycle",
            text.recycle,
            format!("{zapw} --batch --silent --yes --recycle \"%1\""),
            false,
        ));
    }
    if menu.show_quick_delete {
        // Keep the irreversible, unconfirmed action visually apart.
        let separate = !actions.is_empty();
        actions.push((
            "3-delete",
            text.quick,
            format!("{zapw} --batch --silent --yes \"%1\""),
            separate,
        ));
    }

    let decorate = |mut key: KeySpec, top_level: bool| -> KeySpec {
        if let Some(icon) = &icon {
            key = key.str("Icon", icon.clone());
        }
        // MultiSelectModel=Document: one verb instance per selected item;
        // zapg/zapw coordinate the instances through the batch lock.
        key = key.str("MultiSelectModel", "Document");
        if top_level && menu.shift_only {
            key = key.str("Extended", "");
        }
        key
    };

    if !actions.is_empty() {
        match menu.layout {
            MenuLayout::Submenu => {
                let root = format!(r"{CLASSES}\{OBJECT_SHELL}\zap");
                let parent = KeySpec::new(root.clone())
                    .str("MUIVerb", "Zap")
                    .str("SubCommands", "")
                    .str("Position", "Bottom");
                keys.push(decorate(parent, true));
                for (suffix, label, command, separator) in actions {
                    let item_path = format!(r"{root}\shell\{suffix}");
                    let mut item = KeySpec::new(item_path.clone()).str("MUIVerb", label);
                    if separator {
                        item = item.dword("CommandFlags", SEPARATOR_BEFORE);
                    }
                    keys.push(decorate(item, false));
                    keys.push(KeySpec::new(format!(r"{item_path}\command")).str("", command));
                }
            }
            MenuLayout::Flat => {
                for (suffix, label, command, _) in actions {
                    let item_path = format!(r"{CLASSES}\{OBJECT_SHELL}\zap-{suffix}");
                    let item = KeySpec::new(item_path.clone())
                        .str("MUIVerb", label)
                        .str("Position", "Bottom");
                    keys.push(decorate(item, true));
                    keys.push(KeySpec::new(format!(r"{item_path}\command")).str("", command));
                }
            }
        }
    }

    if menu.show_background {
        let path = format!(r"{CLASSES}\{BACKGROUND_SHELL}\zap");
        let item = KeySpec::new(path.clone())
            .str("MUIVerb", text.background)
            .str("Position", "Bottom");
        let mut item = decorate(item, true);
        // A single folder: no batch coordination needed.
        item.values.retain(|(name, _)| name != "MultiSelectModel");
        keys.push(item);
        keys.push(KeySpec::new(format!(r"{path}\command")).str("", format!("{zapg} \"%V\"")));
    }

    if !keys.is_empty() {
        keys.push(
            KeySpec::new(EXPLORER_KEY.to_owned())
                .dword("MultipleInvokePromptMinimum", MAX_CONTEXT_MENU_SELECTIONS),
        );
    }
    keys
}

/// One line of the file/folder context menu as Explorer will show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewEntry {
    pub label: String,
    /// 0 = top-level entry, 1 = inside the "Zap" submenu.
    pub depth: u8,
    pub icon: bool,
    pub submenu: bool,
    pub separator_before: bool,
}

/// What the file/folder menu will look like, derived from [`plan`] so the
/// preview can never drift from what is registered.
pub fn preview(settings: &Settings) -> Vec<PreviewEntry> {
    let targets = MenuTargets {
        zapg: PathBuf::from("zapg.exe"),
        zapw: PathBuf::from("zapw.exe"),
    };
    let object_root = format!(r"{CLASSES}\{OBJECT_SHELL}\");
    plan(settings, &targets)
        .into_iter()
        .filter(|key| key.path.starts_with(&object_root) && !key.path.ends_with(r"\command"))
        .filter_map(|key| {
            let label = key.values.iter().find_map(|(name, value)| match value {
                RegValue::Str(text) if name == "MUIVerb" => Some(text.clone()),
                _ => None,
            })?;
            let has = |wanted: &str| key.values.iter().any(|(name, _)| name == wanted);
            Some(PreviewEntry {
                label,
                depth: u8::from(key.path.contains(r"\zap\shell\")),
                icon: has("Icon"),
                submenu: has("SubCommands"),
                separator_before: has("CommandFlags"),
            })
        })
        .collect()
}

/// Remove all Zap menu entries, then register the ones `settings` asks for.
#[cfg(windows)]
pub fn apply(settings: &Settings, targets: &MenuTargets) -> std::io::Result<()> {
    unregister()?;
    for key in plan(settings, targets) {
        registry::write_key(&key)?;
    }
    Ok(())
}

/// Remove every Zap menu entry (current and legacy layouts).
#[cfg(windows)]
pub fn unregister() -> std::io::Result<()> {
    for key in ALL_KNOWN_KEYS {
        registry::delete_tree(&format!(r"{CLASSES}\{key}"))?;
    }
    Ok(())
}

/// Whether any Zap entry is currently registered for files/folders.
#[cfg(windows)]
pub fn is_registered() -> bool {
    ALL_KNOWN_KEYS[..5]
        .iter()
        .any(|key| registry::key_exists(&format!(r"{CLASSES}\{key}")))
}

#[cfg(windows)]
mod registry {
    use super::{KeySpec, RegValue};
    use std::ffi::c_void;
    use std::io;

    type Hkey = *mut c_void;
    // Predefined handles are sign-extended 32-bit values.
    const HKEY_CURRENT_USER: Hkey = 0x8000_0001u32 as i32 as isize as Hkey;
    const KEY_READ: u32 = 0x2_0019;
    const KEY_WRITE: u32 = 0x2_0006;
    const REG_SZ: u32 = 1;
    const REG_DWORD: u32 = 4;
    const ERROR_SUCCESS: i32 = 0;
    const ERROR_FILE_NOT_FOUND: i32 = 2;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegCreateKeyExW(
            hkey: Hkey,
            sub_key: *const u16,
            reserved: u32,
            class: *const u16,
            options: u32,
            sam: u32,
            security: *const c_void,
            result: *mut Hkey,
            disposition: *mut u32,
        ) -> i32;
        fn RegOpenKeyExW(
            hkey: Hkey,
            sub_key: *const u16,
            options: u32,
            sam: u32,
            result: *mut Hkey,
        ) -> i32;
        fn RegSetValueExW(
            hkey: Hkey,
            name: *const u16,
            reserved: u32,
            kind: u32,
            data: *const u8,
            len: u32,
        ) -> i32;
        fn RegDeleteTreeW(hkey: Hkey, sub_key: *const u16) -> i32;
        fn RegCloseKey(hkey: Hkey) -> i32;
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn check(status: i32, what: &str, path: &str) -> io::Result<()> {
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            let err = io::Error::from_raw_os_error(status);
            Err(io::Error::new(
                err.kind(),
                format!("{what} HKCU\\{path}: {err}"),
            ))
        }
    }

    pub fn write_key(spec: &KeySpec) -> io::Result<()> {
        let path = wide(&spec.path);
        let mut key: Hkey = std::ptr::null_mut();
        // SAFETY: valid NUL-terminated path and out-pointer.
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                std::ptr::null(),
                0,
                KEY_WRITE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            )
        };
        check(status, "cannot create", &spec.path)?;

        let mut result = Ok(());
        for (name, value) in &spec.values {
            let name_w = wide(name);
            let (kind, bytes): (u32, Vec<u8>) = match value {
                RegValue::Str(text) => (
                    REG_SZ,
                    wide(text).iter().flat_map(|c| c.to_le_bytes()).collect(),
                ),
                RegValue::Dword(n) => (REG_DWORD, n.to_le_bytes().to_vec()),
            };
            // SAFETY: `key` is open; buffers outlive the call.
            let status = unsafe {
                RegSetValueExW(
                    key,
                    name_w.as_ptr(),
                    0,
                    kind,
                    bytes.as_ptr(),
                    bytes.len() as u32,
                )
            };
            if let Err(err) = check(status, "cannot write", &spec.path) {
                result = Err(err);
                break;
            }
        }
        // SAFETY: `key` came from RegCreateKeyExW.
        unsafe {
            RegCloseKey(key);
        }
        result
    }

    pub fn delete_tree(path: &str) -> io::Result<()> {
        let path_w = wide(path);
        // SAFETY: valid NUL-terminated path.
        let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, path_w.as_ptr()) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        check(status, "cannot delete", path)
    }

    pub fn key_exists(path: &str) -> bool {
        let path_w = wide(path);
        let mut key: Hkey = std::ptr::null_mut();
        // SAFETY: valid path and out-pointer; the key is closed below.
        let status =
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path_w.as_ptr(), 0, KEY_READ, &mut key) };
        if status == ERROR_SUCCESS {
            unsafe {
                RegCloseKey(key);
            }
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::MenuSettings;

    fn targets() -> MenuTargets {
        MenuTargets {
            zapg: PathBuf::from(r"C:\Zap\bin\zapg.exe"),
            zapw: PathBuf::from(r"C:\Zap\bin\zapw.exe"),
        }
    }

    fn english(menu: MenuSettings) -> Settings {
        Settings {
            language: Language::English,
            menu,
            ..Settings::default()
        }
    }

    fn find<'a>(keys: &'a [KeySpec], suffix: &str) -> Option<&'a KeySpec> {
        keys.iter().find(|k| k.path.ends_with(suffix))
    }

    fn value<'a>(key: &'a KeySpec, name: &str) -> Option<&'a RegValue> {
        key.values.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }

    #[test]
    fn default_submenu_has_three_actions_with_icons() {
        let keys = plan(&english(MenuSettings::default()), &targets());
        let parent = find(&keys, r"AllFilesystemObjects\shell\zap").unwrap();
        assert_eq!(
            value(parent, "SubCommands"),
            Some(&RegValue::Str(String::new()))
        );
        assert!(value(parent, "Icon").is_some());
        for suffix in ["1-dialog", "2-recycle", "3-delete"] {
            assert!(
                find(&keys, &format!(r"zap\shell\{suffix}")).is_some(),
                "{suffix}"
            );
        }
        let quick = find(&keys, r"zap\shell\3-delete").unwrap();
        assert_eq!(
            value(quick, "CommandFlags"),
            Some(&RegValue::Dword(SEPARATOR_BEFORE))
        );
        assert!(keys.iter().any(|k| k.path == EXPLORER_KEY));
    }

    #[test]
    fn icons_can_be_turned_off_everywhere() {
        let keys = plan(
            &english(MenuSettings {
                icons: false,
                ..MenuSettings::default()
            }),
            &targets(),
        );
        assert!(keys.iter().all(|k| value(k, "Icon").is_none()));
        // Labels stay.
        assert!(keys.iter().any(|k| value(k, "MUIVerb").is_some()));
    }

    #[test]
    fn background_entry_only_opens_the_dialog() {
        let keys = plan(&english(MenuSettings::default()), &targets());
        let command = find(&keys, r"Directory\Background\shell\zap\command").unwrap();
        let RegValue::Str(cmd) = value(command, "").unwrap() else {
            panic!("command must be a string");
        };
        assert!(cmd.contains("zapg.exe"), "{cmd}");
        assert!(
            !cmd.contains("zapw"),
            "background must never delete silently"
        );
        assert!(!cmd.contains("--yes"));
    }

    #[test]
    fn flat_layout_and_shift_only() {
        let keys = plan(
            &english(MenuSettings {
                layout: MenuLayout::Flat,
                shift_only: true,
                show_recycle: false,
                ..MenuSettings::default()
            }),
            &targets(),
        );
        assert!(find(&keys, r"AllFilesystemObjects\shell\zap").is_none());
        let dialog = find(&keys, r"shell\zap-1-dialog").unwrap();
        assert_eq!(
            value(dialog, "MUIVerb"),
            Some(&RegValue::Str("Delete with Zap…".into()))
        );
        assert!(value(dialog, "Extended").is_some());
        assert!(find(&keys, r"shell\zap-2-recycle").is_none());
        // Every created key is covered by unregister().
        for key in &keys {
            if key.path == EXPLORER_KEY || key.path.ends_with(r"\command") {
                continue;
            }
            let relative = key.path.trim_start_matches(&format!(r"{CLASSES}\"));
            assert!(
                ALL_KNOWN_KEYS.iter().any(|k| relative.starts_with(k)),
                "{relative} would leak on unregister"
            );
        }
    }

    #[test]
    fn disabled_menu_registers_nothing() {
        let keys = plan(
            &english(MenuSettings {
                enabled: false,
                ..MenuSettings::default()
            }),
            &targets(),
        );
        assert!(keys.is_empty());
    }

    #[test]
    fn russian_labels() {
        let settings = Settings {
            language: Language::Russian,
            ..Settings::default()
        };
        let keys = plan(&settings, &targets());
        let dialog = find(&keys, r"zap\shell\1-dialog").unwrap();
        assert_eq!(
            value(dialog, "MUIVerb"),
            Some(&RegValue::Str("Удалить…".into()))
        );
    }
}
