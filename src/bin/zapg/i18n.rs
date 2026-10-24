//! Minimal two-language support for the dialog. Strings live at their call
//! sites as `(english, russian)` pairs, which keeps them readable in context
//! and makes a missing translation impossible.

use zap::settings::Language;

/// Pick the string for `lang` (already resolved, never `Auto`).
#[inline]
pub fn pick(lang: Language, en: &'static str, ru: &'static str) -> &'static str {
    match lang {
        Language::Russian => ru,
        _ => en,
    }
}

/// "item"/"items" in English; "элемент"/"элемента"/"элементов" in Russian.
pub fn items_word(lang: Language, n: u64) -> &'static str {
    match lang {
        Language::Russian => {
            let (rem10, rem100) = (n % 10, n % 100);
            if rem10 == 1 && rem100 != 11 {
                "элемент"
            } else if (2..=4).contains(&rem10) && !(12..=14).contains(&rem100) {
                "элемента"
            } else {
                "элементов"
            }
        }
        _ => {
            if n == 1 {
                "item"
            } else {
                "items"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn russian_plurals() {
        let ru = |n| items_word(Language::Russian, n);
        assert_eq!(ru(1), "элемент");
        assert_eq!(ru(3), "элемента");
        assert_eq!(ru(5), "элементов");
        assert_eq!(ru(11), "элементов");
        assert_eq!(ru(12), "элементов");
        assert_eq!(ru(21), "элемент");
        assert_eq!(ru(104), "элемента");
        assert_eq!(items_word(Language::English, 1), "item");
        assert_eq!(items_word(Language::English, 2), "items");
    }
}
