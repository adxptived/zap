//! Translation of the engine's own error texts for the GUI and the
//! windowless worker.
//!
//! Errors travel as plain strings (they also go to the journal and the
//! CLI, which stay English), so translation happens only where a message
//! is shown. Windows already localizes its own error texts ("Access is
//! denied" arrives in the system language); only Zap's phrases need this.

use crate::settings::Language;

/// The noun form for `n`: `[singular, plural]` in English; `[1, 2–4, 5+]`
/// forms in Russian (21 файл, 22 файла, 25 файлов, 11–14 → many).
pub fn plural(
    lang: Language,
    n: u64,
    en: [&'static str; 2],
    ru: [&'static str; 3],
) -> &'static str {
    match lang {
        Language::Russian => {
            let (rem10, rem100) = (n % 10, n % 100);
            if rem10 == 1 && rem100 != 11 {
                ru[0]
            } else if (2..=4).contains(&rem10) && !(12..=14).contains(&rem100) {
                ru[1]
            } else {
                ru[2]
            }
        }
        _ => en[usize::from(n != 1)],
    }
}

/// Summary line of a partially failed directory deletion.
pub fn entries_failed(count: u64) -> String {
    let noun = plural(Language::English, count, ["entry", "entries"], ["", "", ""]);
    format!("{count} {noun} could not be deleted")
}

/// Shown under the summary when the individual failures are listed.
pub const LOCKED_HINT: &str = "Hint: some files are locked or permission-protected. Close apps using these files (Python/Unity/MCP/etc.) or run as Administrator, then retry.";

/// Last line of a capped failure list.
pub fn more_failures(count: usize) -> String {
    format!("... and {count} more")
}

/// Engine phrases (as they appear anywhere in a line) and their Russian
/// text. Paths and OS messages around them are kept as they are.
const PHRASES: &[(&str, &str)] = &[
    (
        LOCKED_HINT,
        "Подсказка: часть файлов занята другой программой или защищена правами. Закройте программы, которые их используют, или запустите от имени администратора и повторите.",
    ),
    ("cancelled by user", "отменено пользователем"),
    ("deletion cancelled", "удаление отменено"),
    ("shred cancelled", "перезапись отменена"),
    ("worker stopped unexpectedly", "удаление неожиданно прервалось"),
    ("refusing to delete filesystem root", "нельзя удалить корень диска"),
    (
        "refusing to delete dangerous path without extra confirmation",
        "системная папка: нужно отдельное подтверждение",
    ),
    ("unsupported filesystem entry", "неподдерживаемый тип объекта"),
    (
        "bulk deletion only accepts files or symlinks",
        "в пакетном режиме удаляются только файлы и ссылки",
    ),
    (
        "no longer a plain directory (replaced by a link?); not traversed",
        "папку подменили ссылкой во время удаления; пропущена",
    ),
    ("failed to remove symlink", "не удалось удалить ссылку"),
    ("failed to configure thread pool", "не удалось настроить потоки"),
    ("Recycle Bin operation failed", "Не удалось переместить в корзину"),
    (
        "cannot be moved to the Recycle Bin (too large, or the drive has none); nothing was deleted",
        "нельзя переместить в корзину (слишком большой объект или на диске нет корзины); ничего не удалено",
    ),
    (
        "was deleted permanently: the Recycle Bin could not take it",
        "удалено безвозвратно: корзина не смогла его принять",
    ),
];

/// Translate an engine message for display. English (and every other
/// language) returns the text unchanged.
pub fn localize(message: &str, lang: Language) -> String {
    if lang != Language::Russian {
        return message.to_owned();
    }
    message
        .lines()
        .map(localize_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn localize_line(line: &str) -> String {
    let body = line.trim_start();
    let indent = &line[..line.len() - body.len()];

    if let Some(rest) = body.strip_prefix("... and ") {
        if let Some(count) = rest.strip_suffix(" more") {
            return format!("{indent}... и ещё {count}");
        }
    }
    if let Some((count, tail)) = body.split_once(' ') {
        if let Ok(n) = count.parse::<u64>() {
            if tail == "entry could not be deleted" || tail == "entries could not be deleted" {
                let noun = plural(
                    Language::Russian,
                    n,
                    ["", ""],
                    ["элемент", "элемента", "элементов"],
                );
                return format!("{indent}Не удалось удалить {n} {noun}");
            }
        }
    }

    let mut out = line.to_owned();
    for (en, ru) in PHRASES {
        if out.contains(en) {
            out = out.replace(en, ru);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_is_unchanged_and_grammatical() {
        assert_eq!(entries_failed(1), "1 entry could not be deleted");
        assert_eq!(entries_failed(3), "3 entries could not be deleted");
        let msg = format!("{}\n  {LOCKED_HINT}", entries_failed(2));
        assert_eq!(localize(&msg, Language::English), msg);
    }

    #[test]
    fn russian_translates_engine_phrases_and_keeps_paths_and_os_text() {
        let msg = format!(
            "{}\n  {LOCKED_HINT}\n  C:\\x\\locked.txt: Процесс не может получить доступ к файлу. (os error 32)\n  {}",
            entries_failed(21),
            more_failures(5)
        );
        let ru = localize(&msg, Language::Russian);
        let lines: Vec<&str> = ru.lines().collect();
        assert_eq!(lines[0], "Не удалось удалить 21 элемент");
        assert!(lines[1].starts_with("  Подсказка:"));
        assert_eq!(
            lines[2],
            "  C:\\x\\locked.txt: Процесс не может получить доступ к файлу. (os error 32)"
        );
        assert_eq!(lines[3], "  ... и ещё 5");
        assert_eq!(
            localize(
                "refusing to delete filesystem root: D:\\",
                Language::Russian
            ),
            "нельзя удалить корень диска: D:\\"
        );
        assert_eq!(
            localize("cancelled by user", Language::Russian),
            "отменено пользователем"
        );
    }

    #[test]
    fn plural_forms() {
        let ru = |n| plural(Language::Russian, n, ["", ""], ["файл", "файла", "файлов"]);
        assert_eq!(
            (ru(1), ru(2), ru(5), ru(11), ru(22)),
            ("файл", "файла", "файлов", "файлов", "файла")
        );
    }
}
