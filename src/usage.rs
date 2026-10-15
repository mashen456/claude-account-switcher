//! Verbrauch eines Accounts über dieselbe Schnittstelle, die Claude Code für
//! `/usage` nutzt. Pro Account wird der letzte Stand gespeichert, damit ein
//! inaktiver Slot (dessen Access-Token nach einigen Stunden abläuft) trotzdem
//! seinen letzten Stand und den Reset-Zeitpunkt zeigt.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fsx;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const OAUTH_BETA: &str = "oauth-2025-04-20";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    /// Auslastung in Prozent.
    pub pct: f64,
    pub resets_at: Option<String>,
}

/// Wochenlimit für ein einzelnes Modell.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelWindow {
    pub name: String,
    pub window: Window,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    #[serde(default)]
    pub models: Vec<ModelWindow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reading {
    pub usage: Usage,
    pub fetched_at: String,
}

#[derive(Debug)]
pub enum FetchError {
    Unauthorized,
    RateLimited,
    Http(u16),
    Network(String),
    Format(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "Token abgelehnt (401)"),
            FetchError::RateLimited => write!(f, "Zu viele Abfragen (429), später erneut"),
            FetchError::Http(code) => write!(f, "Server antwortet mit {code}"),
            FetchError::Network(msg) => write!(f, "Keine Verbindung: {msg}"),
            FetchError::Format(msg) => write!(f, "Unerwartete Antwort: {msg}"),
        }
    }
}

pub struct Token {
    pub value: String,
    expires_at_ms: Option<i64>,
}

impl Token {
    /// Abgelaufen oder läuft in der nächsten Minute ab.
    pub fn expired(&self) -> bool {
        let now_ms = chrono::Utc::now().timestamp_millis();
        self.expires_at_ms.is_some_and(|at| at <= now_ms + 60_000)
    }
}

pub fn token_of(credentials: &Value) -> Option<Token> {
    let oauth = credentials.get("claudeAiOauth")?;
    let value = oauth.get("accessToken")?.as_str().filter(|s| !s.is_empty())?;
    Some(Token {
        value: value.to_string(),
        expires_at_ms: oauth.get("expiresAt").and_then(Value::as_i64),
    })
}

pub fn fetch(access_token: &str) -> Result<Usage, FetchError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent
        .get(USAGE_URL)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-beta", OAUTH_BETA)
        .header("User-Agent", concat!("claude-slots/", env!("CARGO_PKG_VERSION")))
        .call()
        .map_err(|e| FetchError::Network(e.to_string()))?;
    match response.status().as_u16() {
        200 => {}
        401 => return Err(FetchError::Unauthorized),
        429 => return Err(FetchError::RateLimited),
        code => return Err(FetchError::Http(code)),
    }
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| FetchError::Network(e.to_string()))?;
    let data: Value = serde_json::from_str(&body).map_err(|e| FetchError::Format(e.to_string()))?;
    Ok(parse(&data))
}

fn parse(data: &Value) -> Usage {
    let window = |w: &Value| -> Option<Window> {
        Some(Window {
            pct: w.get("utilization")?.as_f64()?,
            resets_at: w.get("resets_at").and_then(Value::as_str).map(str::to_owned),
        })
    };
    // Wochenlimits einzelner Modelle stehen im Feld `limits`.
    let mut models = Vec::new();
    for limit in data.get("limits").and_then(Value::as_array).into_iter().flatten() {
        let name = limit.pointer("/scope/model/display_name").and_then(Value::as_str);
        let pct = limit.get("percent").and_then(Value::as_f64);
        if let (Some(name), Some(pct)) = (name, pct) {
            models.push(ModelWindow {
                name: name.to_string(),
                window: Window {
                    pct,
                    resets_at: limit.get("resets_at").and_then(Value::as_str).map(str::to_owned),
                },
            });
        }
    }
    Usage {
        five_hour: data.get("five_hour").and_then(window),
        seven_day: data.get("seven_day").and_then(window),
        models,
    }
}

/// Letzter Stand pro Account, in `usage-cache.json`.
pub struct UsageCache {
    path: PathBuf,
    readings: BTreeMap<String, Reading>,
}

impl UsageCache {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("usage-cache.json");
        let readings = fsx::read_opt(&path)
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self { path, readings }
    }

    pub fn get(&self, key: &str) -> Option<&Reading> {
        self.readings.get(key)
    }

    pub fn put(&mut self, key: String, usage: Usage) {
        let reading = Reading {
            usage,
            fetched_at: chrono::Local::now().to_rfc3339(),
        };
        self.readings.insert(key, reading);
        if let Ok(text) = serde_json::to_string_pretty(&self.readings) {
            let _ = fsx::write_atomic(&self.path, text.as_bytes());
        }
    }
}
