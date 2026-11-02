//! Endpoint-Slots: statt eines Claude-Accounts ein eigener Anthropic-kompatibler
//! Server, z. B. ein eigenes Modell auf Runpod (llama.cpp mit `/v1/messages`).
//!
//! Umschalten heißt hier: `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN` und die
//! Modellnamen im Block `env` von `<Konfigurationsordner>/settings.json`
//! eintragen. `ANTHROPIC_AUTH_TOKEN` hat bei Claude Code Vorrang vor dem
//! claude.ai-Login; der Login selbst (Token und `oauthAccount`) bleibt dabei
//! unangetastet. Beim Wechsel zurück zu einem Account-Slot werden die Werte
//! wieder entfernt bzw. die vorherigen zurückgelegt. Die vorherigen Werte
//! stehen solange in `endpoint-backup.json` im Slot-Ordner.
//!
//! Claude Code liest `settings.json` beim Start. Eine laufende Sitzung nutzt
//! den Endpoint (bzw. wieder den Account) erst nach einem Neustart.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::claude;
use crate::fsx;

/// Die Variablen, die ein Endpoint-Slot setzt. Alle Modell-Aliasse zeigen auf
/// dasselbe Modell, damit auch Subagenten und Hintergrundaufgaben (sonst
/// "haiku") beim Endpoint landen.
pub const MANAGED_KEYS: [&str; 6] = [
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
];

const BACKUP_FILE: &str = "endpoint-backup.json";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    /// Anzeigename, z. B. "Runpod Qwen3-Coder".
    #[serde(default)]
    pub name: String,
    /// z. B. `https://<ENDPOINT_ID>.api.runpod.ai`, ohne `/v1`.
    pub base_url: String,
    /// Wird als `Authorization: Bearer …` gesendet (bei Runpod: der API-Key).
    pub auth_token: String,
    /// Modellname, den der Server erwartet (bei llama.cpp: `--alias`).
    pub model: String,
}

impl Endpoint {
    /// Bereinigt die Eingaben und prüft sie.
    pub fn normalized(self) -> Result<Self> {
        let base_url = self.base_url.trim().trim_end_matches('/').to_string();
        let auth_token = self.auth_token.trim().to_string();
        let model = self.model.trim().to_string();
        let mut name = self.name.trim().to_string();
        if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
            bail!(tr!(
                "Die Adresse muss mit https:// beginnen",
                "The address must start with https://"
            ));
        }
        if base_url.ends_with("/v1") || base_url.ends_with("/v1/messages") {
            bail!(tr!(
                "Die Adresse ohne /v1 angeben, Claude Code hängt /v1/messages selbst an",
                "Leave out /v1, Claude Code appends /v1/messages itself"
            ));
        }
        if base_url.contains(char::is_whitespace) {
            bail!(tr!("Die Adresse enthält Leerzeichen", "The address contains spaces"));
        }
        if auth_token.is_empty() {
            bail!(tr!("Token fehlt", "Token is missing"));
        }
        if model.is_empty() || model.contains(char::is_whitespace) {
            bail!(tr!("Modellname fehlt oder enthält Leerzeichen", "Model name is missing or contains spaces"));
        }
        if name.is_empty() {
            name = host(&base_url).to_string();
        }
        Ok(Self {
            name,
            base_url,
            auth_token,
            model,
        })
    }

    pub fn display_name(&self) -> String {
        if self.name.is_empty() {
            host(&self.base_url).to_string()
        } else {
            self.name.clone()
        }
    }

    fn env(&self) -> [(&'static str, &str); 6] {
        [
            ("ANTHROPIC_BASE_URL", &self.base_url),
            ("ANTHROPIC_AUTH_TOKEN", &self.auth_token),
            ("ANTHROPIC_MODEL", &self.model),
            ("ANTHROPIC_DEFAULT_OPUS_MODEL", &self.model),
            ("ANTHROPIC_DEFAULT_SONNET_MODEL", &self.model),
            ("ANTHROPIC_DEFAULT_HAIKU_MODEL", &self.model),
        ]
    }

    /// Steht genau dieser Endpoint gerade in `settings.json`?
    fn matches(&self, env: &Map<String, Value>) -> bool {
        let get = |key: &str| env.get(key).and_then(Value::as_str);
        get("ANTHROPIC_BASE_URL") == Some(&self.base_url)
            && get("ANTHROPIC_AUTH_TOKEN") == Some(&self.auth_token)
    }
}

/// `https://abc.api.runpod.ai/x` → `abc.api.runpod.ai`
pub fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

pub fn settings_path() -> Result<PathBuf> {
    Ok(claude::config_home()?.join("settings.json"))
}

fn read_settings(path: &Path) -> Result<Map<String, Value>> {
    let Some(text) = fsx::read_opt(path)? else {
        return Ok(Map::new());
    };
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    serde_json::from_str(&text).with_context(|| {
        let shown = path.display();
        tr!("{shown} ist kein gültiges JSON, nicht angefasst", "{shown} is not valid JSON, left untouched")
    })
}

fn write_settings(path: &Path, settings: &Map<String, Value>) -> Result<()> {
    let text = serde_json::to_string_pretty(settings)? + "\n";
    fsx::write_atomic(path, text.as_bytes())
}

fn env_of(settings: &Map<String, Value>) -> Map<String, Value> {
    settings
        .get("env")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Der aktuelle Block `env` aus `settings.json`.
pub fn current_env() -> Result<Map<String, Value>> {
    Ok(env_of(&read_settings(&settings_path()?)?))
}

pub fn is_active(endpoint: &Endpoint) -> Result<bool> {
    Ok(endpoint.matches(&current_env()?))
}

pub fn is_active_in(endpoint: &Endpoint, env: &Map<String, Value>) -> bool {
    endpoint.matches(env)
}

/// Werte vor dem ersten Umschalten auf einen Endpoint.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Backup {
    /// Gab es vorher überhaupt einen Block `env`?
    had_env: bool,
    /// Vorheriger Wert je Variable, `null` = war nicht gesetzt.
    previous: Map<String, Value>,
    saved_at: String,
}

pub fn backup_exists(dir: &Path) -> bool {
    dir.join(BACKUP_FILE).is_file()
}

/// Trägt den Endpoint in `settings.json` ein. Andere Einstellungen und andere
/// Variablen im Block `env` bleiben unverändert.
pub fn activate(dir: &Path, endpoint: &Endpoint) -> Result<()> {
    let path = settings_path()?;
    let mut settings = read_settings(&path)?;
    let had_env = settings.contains_key("env");
    if had_env && !settings["env"].is_object() {
        let shown = path.display();
        bail!(tr!(
            "In {shown} ist \"env\" kein Objekt, nicht angefasst",
            "\"env\" in {shown} is not an object, left untouched"
        ));
    }
    // Nur beim ersten Endpoint sichern. Bei Endpoint → Endpoint steht in
    // settings.json schon ein Endpoint, die Sicherung hält die Werte davor.
    let backup_path = dir.join(BACKUP_FILE);
    if !backup_path.is_file() {
        let env = env_of(&settings);
        let previous = MANAGED_KEYS
            .iter()
            .map(|key| (key.to_string(), env.get(*key).cloned().unwrap_or(Value::Null)))
            .collect();
        let backup = Backup {
            had_env,
            previous,
            saved_at: chrono::Local::now().to_rfc3339(),
        };
        fsx::write_atomic(&backup_path, serde_json::to_string_pretty(&backup)?.as_bytes())?;
    }
    let env = settings
        .entry("env")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("checked above");
    for (key, value) in endpoint.env() {
        env.insert(key.to_string(), Value::String(value.to_string()));
    }
    write_settings(&path, &settings)
}

/// Nimmt den Endpoint wieder aus `settings.json` und legt die vorherigen Werte
/// zurück. Ohne Sicherung werden die Variablen nur entfernt.
pub fn deactivate(dir: &Path) -> Result<()> {
    let path = settings_path()?;
    let mut settings = read_settings(&path)?;
    let backup_path = dir.join(BACKUP_FILE);
    let backup: Option<Backup> = fsx::read_opt(&backup_path)?
        .and_then(|text| serde_json::from_str(&text).ok());
    let had_env = backup.as_ref().is_none_or(|b| b.had_env);
    if let Some(env) = settings.get_mut("env").and_then(Value::as_object_mut) {
        for key in MANAGED_KEYS {
            match backup.as_ref().and_then(|b| b.previous.get(key)) {
                Some(value) if !value.is_null() => {
                    env.insert(key.to_string(), value.clone());
                }
                _ => {
                    env.remove(key);
                }
            }
        }
        if env.is_empty() && !had_env {
            settings.remove("env");
        }
    }
    write_settings(&path, &settings)?;
    if backup_path.exists() {
        let shown = backup_path.display();
        std::fs::remove_file(&backup_path)
            .with_context(|| tr!("{shown} nicht löschbar", "Cannot delete {shown}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_input() {
        let ep = Endpoint {
            name: " ".into(),
            base_url: " https://abc.api.runpod.ai/ ".into(),
            auth_token: " rpa_123 ".into(),
            model: "qwen3-coder-next".into(),
        }
        .normalized()
        .unwrap();
        assert_eq!(ep.base_url, "https://abc.api.runpod.ai");
        assert_eq!(ep.auth_token, "rpa_123");
        assert_eq!(ep.name, "abc.api.runpod.ai");

        let bad = |url: &str| {
            Endpoint {
                base_url: url.into(),
                auth_token: "t".into(),
                model: "m".into(),
                ..Default::default()
            }
            .normalized()
            .is_err()
        };
        assert!(bad("abc.api.runpod.ai"));
        assert!(bad("https://abc.api.runpod.ai/v1"));
        assert!(!bad("http://localhost:8080"));
    }
}
