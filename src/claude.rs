//! Wo Claude Code seine Daten ablegt, und wer gerade eingeloggt ist.
//!
//! Die Pfade folgen Claude Codes eigener Auflösung:
//! - Konfigurationsordner: `CLAUDE_CONFIG_DIR`, sonst `~/.claude`
//! - Token-Datei: `<Konfigurationsordner>/.credentials.json`
//! - globale Konfiguration: `<Konfigurationsordner>/.config.json`, wenn es sie
//!   (nicht leer) gibt, sonst `(CLAUDE_CONFIG_DIR oder Benutzerordner)/.claude.json`.
//!   Darin steht im Block `oauthAccount`, welcher Account eingeloggt ist.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::fsx;

fn home() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!(tr!("Benutzerordner nicht gefunden", "Home folder not found")))
}

fn config_dir_env() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

pub fn config_home() -> Result<PathBuf> {
    match config_dir_env() {
        Some(dir) => Ok(dir),
        None => Ok(home()?.join(".claude")),
    }
}

pub fn credentials_path() -> Result<PathBuf> {
    Ok(config_home()?.join(".credentials.json"))
}

pub fn global_config_path() -> Result<PathBuf> {
    let legacy = config_home()?.join(".config.json");
    let legacy_used = std::fs::metadata(&legacy)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false);
    if legacy_used {
        return Ok(legacy);
    }
    let base = match config_dir_env() {
        Some(dir) => dir,
        None => home()?,
    };
    Ok(base.join(".claude.json"))
}

/// Der Block `oauthAccount` aus der globalen Konfiguration.
pub fn oauth_account() -> Result<Option<Value>> {
    let path = global_config_path()?;
    let Some(text) = fsx::read_opt(&path)? else {
        return Ok(None);
    };
    let shown = path.display();
    let config: Value = serde_json::from_str(&text)
        .with_context(|| tr!("{shown} ist kein gültiges JSON", "{shown} is not valid JSON"))?;
    Ok(config.get("oauthAccount").filter(|v| v.is_object()).cloned())
}

/// Ersetzt nur den Block `oauthAccount`; Projekte, MCP-Server und
/// Einstellungen in derselben Datei bleiben unverändert.
pub fn set_oauth_account(oauth_account: &Value) -> Result<()> {
    let path = global_config_path()?;
    let mut config = match fsx::read_opt(&path)? {
        Some(text) => serde_json::from_str::<Map<String, Value>>(&text).with_context(|| {
            let shown = path.display();
            tr!("{shown} ist kein gültiges JSON, nicht angefasst", "{shown} is not valid JSON, left untouched")
        })?,
        None => Map::new(),
    };
    config.insert("oauthAccount".into(), oauth_account.clone());
    let text = serde_json::to_string_pretty(&Value::Object(config))?;
    fsx::write_atomic(&path, text.as_bytes())
}

/// Woran ein Account erkannt wird.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub account_uuid: String,
    pub org_uuid: String,
    pub email: String,
}

impl Identity {
    pub fn from_oauth_account(oauth_account: &Value) -> Option<Self> {
        let field = |key: &str| {
            oauth_account
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let id = Identity {
            account_uuid: field("accountUuid"),
            org_uuid: field("organizationUuid"),
            email: field("emailAddress"),
        };
        (!id.account_uuid.is_empty() || !id.email.is_empty()).then_some(id)
    }

    /// Gleiche Person (accountUuid, ersatzweise E-Mail) in derselben Organisation.
    pub fn same_account(&self, other: &Identity) -> bool {
        let person = if !self.account_uuid.is_empty() && !other.account_uuid.is_empty() {
            self.account_uuid == other.account_uuid
        } else {
            !self.email.is_empty() && self.email == other.email
        };
        person && self.org_uuid == other.org_uuid
    }

    /// Schlüssel für den Verbrauchs-Cache.
    pub fn key(&self) -> String {
        let person = if self.account_uuid.is_empty() {
            &self.email
        } else {
            &self.account_uuid
        };
        format!("{person}:{}", self.org_uuid)
    }
}

/// Abo aus dem Token-Objekt, z. B. "Max 20x" oder "Pro".
pub fn plan_label(credentials: &Value) -> Option<String> {
    let oauth = credentials.get("claudeAiOauth")?;
    let plan = oauth.get("subscriptionType").and_then(Value::as_str)?;
    let mut label = capitalize(plan);
    let tier = oauth
        .get("rateLimitTier")
        .and_then(Value::as_str)
        .unwrap_or_default();
    for multiplier in ["20x", "5x"] {
        if tier.ends_with(multiplier) {
            label = format!("{label} {multiplier}");
            break;
        }
    }
    Some(label)
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}
