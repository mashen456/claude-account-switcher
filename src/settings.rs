//! Einstellungen in `settings.json` im Slot-Ordner: Anzahl der Slots, Sprache.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::fsx;
use crate::i18n::Lang;

pub const DEFAULT_SLOTS: usize = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default = "default_slots")]
    pub slot_count: usize,
    /// `None`: Sprache des Systems.
    #[serde(default)]
    pub language: Option<Lang>,
}

fn default_slots() -> usize {
    DEFAULT_SLOTS
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            slot_count: DEFAULT_SLOTS,
            language: None,
        }
    }
}

impl Settings {
    /// Fehlt die Datei oder ist sie kaputt, gelten die Standardwerte.
    pub fn load(dir: &Path) -> Self {
        fsx::read_opt(&dir.join("settings.json"))
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        fsx::write_atomic(&dir.join("settings.json"), text.as_bytes())
    }
}
