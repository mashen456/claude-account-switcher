//! Deutsch und Englisch. Jeder sichtbare Text steht als Paar im Code:
//! `tr!("Slot {n} ist leer", "Slot {n} is empty")`.

use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    De,
    En,
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn set(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

pub fn get() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        0 => Lang::De,
        _ => Lang::En,
    }
}

/// Sprache des Systems: Deutsch für "de-…", sonst Englisch.
pub fn system() -> Lang {
    match sys_locale::get_locale() {
        Some(locale) if locale.to_lowercase().starts_with("de") => Lang::De,
        _ => Lang::En,
    }
}

/// Text in der eingestellten Sprache, mit `format!`-Platzhaltern.
#[macro_export]
macro_rules! tr {
    ($de:literal, $en:literal $(,)?) => {
        match $crate::i18n::get() {
            $crate::i18n::Lang::De => format!($de),
            $crate::i18n::Lang::En => format!($en),
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_language() {
        let n = 3;
        set(Lang::En);
        assert_eq!(tr!("Slot {n} ist leer", "Slot {n} is empty"), "Slot 3 is empty");
        set(Lang::De);
        assert_eq!(tr!("Slot {n} ist leer", "Slot {n} is empty"), "Slot 3 ist leer");
    }
}
