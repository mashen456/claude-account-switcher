//! Verbrauch eines Accounts über dieselbe Schnittstelle, die Claude Code für
//! `/usage` nutzt. Pro Account wird der letzte Stand gespeichert, damit ein
//! inaktiver Slot (dessen Access-Token nach einigen Stunden abläuft) trotzdem
//! seinen letzten Stand und den Reset-Zeitpunkt zeigt.
//!
//! Die Schnittstelle erlaubt Programmen außer Claude Code nur etwa 28–30
//! Abfragen pro Stunde und Account, gezählt über ein gleitendes Stundenfenster.
//! Ist es voll, bleibt der Account bis zu einer Stunde gesperrt (gemessen vom
//! Projekt claude-swap, siehe dessen `poll_policy.py`). Deshalb hier:
//! feste Mindestabstände pro Account, Zeitpunkte in der Cache-Datei (so zählen
//! Neustarts und mehrere offene Fenster mit) und lange Pausen nach einem 429.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fsx;

/// Abstand für den aktiven Account (dessen Verbrauch sich gerade ändert).
pub const ACTIVE_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Abstand für inaktive Accounts.
pub const INACTIVE_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Untergrenze für alles, auch für "Aktualisieren".
pub const MIN_INTERVAL: Duration = Duration::from_secs(3 * 60);
/// Eine Stunde nach einem 429 höchstens so oft.
const AFTER_429_INTERVAL: Duration = Duration::from_secs(10 * 60);
const RECENT_429: Duration = Duration::from_secs(60 * 60);
/// Zuschlag auf Retry-After: Sperren laufen oft etwas länger als angekündigt.
const RETRY_AFTER_MARGIN: Duration = Duration::from_secs(15 * 60);
const MAX_PAUSE: Duration = Duration::from_secs(90 * 60);

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const OAUTH_BETA: &str = "oauth-2025-04-20";
pub const USER_AGENT: &str = concat!("claude-slots/", env!("CARGO_PKG_VERSION"));

/// HTTP-Client mit Zeitlimit, der Fehlerstatus als normale Antwort liefert.
pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into()
}

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

#[derive(Debug)]
pub enum FetchError {
    Unauthorized,
    /// Mit der Wartezeit aus dem Header Retry-After, falls angegeben.
    RateLimited(Option<u64>),
    Http(u16),
    Network(String),
    Format(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchError::Unauthorized => {
                f.write_str(&tr!("Token abgelehnt (401)", "Token rejected (401)"))
            }
            FetchError::RateLimited(_) => {
                f.write_str(&tr!("Zu viele Abfragen (429)", "Too many requests (429)"))
            }
            FetchError::Http(code) => {
                f.write_str(&tr!("Server antwortet mit {code}", "Server answered {code}"))
            }
            FetchError::Network(msg) => {
                f.write_str(&tr!("Keine Verbindung: {msg}", "No connection: {msg}"))
            }
            FetchError::Format(msg) => {
                f.write_str(&tr!("Unerwartete Antwort: {msg}", "Unexpected response: {msg}"))
            }
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
    let mut response = agent()
        .get(USAGE_URL)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("anthropic-beta", OAUTH_BETA)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| FetchError::Network(e.to_string()))?;
    match response.status().as_u16() {
        200 => {}
        401 => return Err(FetchError::Unauthorized),
        429 => {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse().ok());
            return Err(FetchError::RateLimited(retry_after));
        }
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

/// Stand und Abfrage-Zeitpunkte eines Accounts. Ältere Cache-Dateien
/// (nur `usage` + `fetchedAt`) lassen sich weiter lesen.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<String>,
    /// Letzter Versuch, auch fehlgeschlagen oder noch laufend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attempt_at: Option<String>,
    /// Vorher nicht erneut fragen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_429_at: Option<String>,
    #[serde(default)]
    failures: u32,
}

fn parse_time(value: &Option<String>) -> Option<DateTime<Utc>> {
    let text = value.as_deref()?;
    Some(DateTime::parse_from_rfc3339(text).ok()?.with_timezone(&Utc))
}

fn stamp(time: DateTime<Utc>) -> Option<String> {
    Some(time.to_rfc3339())
}

impl Entry {
    /// Bis wann wegen eines Fehlers pausiert wird, falls noch in der Zukunft.
    pub fn paused_until(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        parse_time(&self.retry_at).filter(|at| *at > now)
    }

    pub fn recently_limited(&self, now: DateTime<Utc>) -> bool {
        parse_time(&self.last_429_at).is_some_and(|at| now - at < chrono_dur(RECENT_429))
    }

    /// Ist eine Abfrage fällig, wenn höchstens alle `interval` gefragt werden soll?
    fn due(&self, interval: Duration, now: DateTime<Utc>) -> bool {
        if self.paused_until(now).is_some() {
            return false;
        }
        let mut interval = interval.max(MIN_INTERVAL);
        if self.recently_limited(now) {
            interval = interval.max(AFTER_429_INTERVAL);
        }
        let last = parse_time(&self.fetched_at).max(parse_time(&self.attempt_at));
        last.is_none_or(|at| now - at >= chrono_dur(interval))
    }

    fn succeeded(&mut self, usage: Usage, now: DateTime<Utc>) {
        self.usage = Some(usage);
        self.fetched_at = stamp(now);
        self.retry_at = None;
        self.failures = 0;
    }

    fn failed(&mut self, err: &FetchError, now: DateTime<Utc>) {
        self.failures = self.failures.saturating_add(1);
        let doubling = |base: u64| Duration::from_secs(base << self.failures.saturating_sub(1).min(6));
        let pause = match err {
            FetchError::RateLimited(retry_after) => {
                self.last_429_at = stamp(now);
                match retry_after {
                    Some(seconds) if *seconds > 0 => Duration::from_secs(*seconds) + RETRY_AFTER_MARGIN,
                    // Ohne Angabe: 5, 10, 20, 40 … Minuten.
                    _ => doubling(5 * 60),
                }
            }
            // Beim aktiven Account erneuert Claude Code das Token bald.
            FetchError::Unauthorized => Duration::from_secs(10 * 60),
            // Netzwerk oder Server: 1, 2, 4 … Minuten.
            _ => doubling(60),
        };
        self.retry_at = stamp(now + chrono_dur(pause.min(MAX_PAUSE)));
    }
}

fn chrono_dur(duration: Duration) -> chrono::Duration {
    chrono::Duration::from_std(duration).unwrap_or(chrono::Duration::MAX)
}

/// Stand pro Account, in `usage-cache.json`. Jede Änderung liest die Datei
/// vorher neu ein, damit sich mehrere offene Fenster nicht überschreiben und
/// gemeinsam an die Abstände halten.
pub struct UsageCache {
    path: PathBuf,
    entries: BTreeMap<String, Entry>,
}

impl UsageCache {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("usage-cache.json");
        let entries = read_entries(&path);
        Self { path, entries }
    }

    /// Stand anderer Fenster übernehmen.
    pub fn refresh(&mut self) {
        self.entries = read_entries(&self.path);
    }

    pub fn get(&self, key: &str) -> Option<&Entry> {
        self.entries.get(key)
    }

    /// Darf jetzt abgefragt werden? Falls ja, wird der Versuch sofort
    /// vermerkt, damit kein zweites Fenster gleichzeitig fragt.
    pub fn claim(&mut self, key: &str, interval: Duration) -> bool {
        self.refresh();
        let now = Utc::now();
        if !self.entries.get(key).is_none_or(|entry| entry.due(interval, now)) {
            return false;
        }
        self.update(key, |entry| entry.attempt_at = stamp(now));
        true
    }

    pub fn record(&mut self, key: &str, result: &Result<Usage, FetchError>) {
        let now = Utc::now();
        self.update(key, |entry| match result {
            Ok(usage) => entry.succeeded(usage.clone(), now),
            Err(err) => entry.failed(err, now),
        });
    }

    fn update(&mut self, key: &str, change: impl FnOnce(&mut Entry)) {
        self.entries = read_entries(&self.path);
        change(self.entries.entry(key.to_string()).or_default());
        if let Ok(text) = serde_json::to_string_pretty(&self.entries) {
            let _ = fsx::write_atomic(&self.path, text.as_bytes());
        }
    }
}

fn read_entries(path: &Path) -> BTreeMap<String, Entry> {
    fsx::read_opt(path)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-16T12:00:00Z").unwrap().with_timezone(&Utc)
            + chrono::Duration::minutes(minutes)
    }

    #[test]
    fn keeps_intervals_and_backs_off_after_429() {
        let mut entry = Entry::default();
        assert!(entry.due(ACTIVE_INTERVAL, at(0)));
        entry.succeeded(Usage::default(), at(0));
        assert!(!entry.due(ACTIVE_INTERVAL, at(4)));
        assert!(entry.due(ACTIVE_INTERVAL, at(5)));
        assert!(!entry.due(INACTIVE_INTERVAL, at(14)));
        // "Aktualisieren" fragt trotzdem nicht öfter als alle 3 Minuten.
        assert!(!entry.due(Duration::ZERO, at(2)));
        assert!(entry.due(Duration::ZERO, at(3)));

        // 429 mit Retry-After 3600: eine Stunde plus 15 Minuten Pause.
        entry.failed(&FetchError::RateLimited(Some(3600)), at(10));
        assert!(!entry.due(ACTIVE_INTERVAL, at(84)));
        assert!(entry.due(ACTIVE_INTERVAL, at(85)));

        // Eine Stunde nach einem 429 höchstens alle 10 Minuten.
        let mut entry = Entry::default();
        entry.succeeded(Usage::default(), at(0));
        entry.failed(&FetchError::RateLimited(None), at(6));
        assert!(!entry.due(ACTIVE_INTERVAL, at(10)));
        entry.succeeded(Usage::default(), at(11));
        assert!(!entry.due(ACTIVE_INTERVAL, at(17)));
        assert!(entry.due(ACTIVE_INTERVAL, at(21)));
        entry.succeeded(Usage::default(), at(70));
        assert!(entry.due(ACTIVE_INTERVAL, at(75)));

        // 429 ohne Angabe: 5, 10, 20 Minuten.
        let mut entry = Entry::default();
        entry.failed(&FetchError::RateLimited(None), at(0));
        assert_eq!(entry.paused_until(at(0)), Some(at(5)));
        entry.failed(&FetchError::RateLimited(Some(0)), at(5));
        assert_eq!(entry.paused_until(at(5)), Some(at(15)));
        entry.failed(&FetchError::RateLimited(None), at(15));
        assert_eq!(entry.paused_until(at(15)), Some(at(35)));
        // Höchstens 90 Minuten, auch bei absurdem Retry-After.
        entry.failed(&FetchError::RateLimited(Some(86_400)), at(35));
        assert_eq!(entry.paused_until(at(35)), Some(at(125)));
    }

    /// Zwei Fenster (oder ein Neustart) auf derselben Cache-Datei fragen nicht doppelt.
    #[test]
    fn windows_share_the_schedule() {
        let dir = std::env::temp_dir().join(format!("claude-slots-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut first = UsageCache::load(&dir);
        let mut second = UsageCache::load(&dir);

        assert!(first.claim("acc", ACTIVE_INTERVAL));
        assert!(!second.claim("acc", ACTIVE_INTERVAL));
        assert!(!second.claim("acc", Duration::ZERO));
        assert!(second.claim("other", INACTIVE_INTERVAL));

        first.record("acc", &Err(FetchError::RateLimited(Some(600))));
        second.refresh();
        assert!(second.get("acc").unwrap().paused_until(Utc::now()).is_some());
        assert!(second.get("other").is_some());
        // Ein Neustart sieht dieselbe Pause.
        assert!(!UsageCache::load(&dir).claim("acc", Duration::ZERO));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_old_cache_format() {
        let old = r#"{"k": {"usage": {"fiveHour": {"pct": 12.0, "resetsAt": null}, "sevenDay": null, "models": []},
                       "fetchedAt": "2026-10-16T12:00:00+02:00"}}"#;
        let entries: BTreeMap<String, Entry> = serde_json::from_str(old).unwrap();
        let entry = &entries["k"];
        assert_eq!(entry.usage.as_ref().unwrap().five_hour.as_ref().unwrap().pct, 12.0);
        assert!(!entry.due(INACTIVE_INTERVAL, at(-110)));
        assert!(entry.due(INACTIVE_INTERVAL, at(-105)));
    }
}
