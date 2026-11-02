//! Runpod-Endpoints: Worker-Status und Token-Statistik.
//!
//! Erkennt Endpoint-Slots, deren Adresse `https://<ID>.api.runpod.ai` ist
//! (Load-Balancer-Endpoint mit llama.cpp), und liest zwei Dinge:
//!
//! - Worker-Status über `https://api.runpod.ai/v2/<ID>/health`. Das fragt nur
//!   Runpod und weckt keinen Worker, kostet also nichts.
//! - Token-Zähler über `<Adresse>/metrics` (llama.cpp mit `--metrics`). Diese
//!   Anfrage landet beim Worker selbst. Sie würde einen abgeschalteten Worker
//!   starten und hält einen laufenden wach, deshalb gilt:
//!     * nur abfragen, wenn laut Runpod gerade ein Worker läuft,
//!     * aufhören, sobald sich die Zähler eine Weile nicht mehr ändern, damit der
//!       Worker nach seiner Leerlaufzeit abschalten kann.
//!
//! Die Zähler von llama.cpp beginnen bei jedem Worker-Start bei null. Die
//! Summen werden deshalb hier fortgeschrieben (`runpod-stats.json`). Was ein
//! Worker zwischen der letzten Abfrage und seinem Ende noch verarbeitet hat,
//! fehlt darin; die Summen sind also Mindestwerte.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::endpoint;
use crate::fsx;
use crate::usage::{self, FetchError, USER_AGENT};

/// Worker-Status abfragen (kostenlos).
pub const HEALTH_EVERY: Duration = Duration::from_secs(30);
/// Token-Zähler abfragen, solange gearbeitet wird.
pub const METRICS_EVERY: Duration = Duration::from_secs(60);
/// Ändert sich so lange nichts, keine Zähler mehr abfragen. Zusammen mit der
/// Leerlaufzeit des Workers (z. B. 15 Min.) läuft er dadurch höchstens so viel
/// länger.
pub const QUIET_STOP: Duration = Duration::from_secs(4 * 60);

const STATS_FILE: &str = "runpod-stats.json";

/// `https://abc123.api.runpod.ai` → `abc123`
pub fn endpoint_id(base_url: &str) -> Option<String> {
    let host = endpoint::host(base_url).to_ascii_lowercase();
    let id = host.strip_suffix(".api.runpod.ai")?;
    (!id.is_empty() && !id.contains('.')).then(|| id.to_string())
}

/// Worker-Zahlen aus Runpods `/health`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Workers {
    pub idle: u64,
    pub initializing: u64,
    pub ready: u64,
    pub running: u64,
    pub throttled: u64,
    pub unhealthy: u64,
}

impl Workers {
    /// Läuft (oder startet) gerade ein Worker, also eine bezahlte GPU?
    pub fn up(&self) -> bool {
        self.idle + self.ready + self.running + self.initializing > 0
    }
}

fn get_json(url: &str, token: &str) -> Result<Value, FetchError> {
    let mut response = usage::agent()
        .get(url)
        .header("Authorization", &format!("Bearer {token}"))
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| FetchError::Network(e.to_string()))?;
    match response.status().as_u16() {
        200 => {}
        401 | 403 => return Err(FetchError::Unauthorized),
        429 => return Err(FetchError::RateLimited(None)),
        code => return Err(FetchError::Http(code)),
    }
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| FetchError::Network(e.to_string()))?;
    serde_json::from_str(&body).map_err(|e| FetchError::Format(e.to_string()))
}

pub fn fetch_health(id: &str, token: &str) -> Result<Workers, FetchError> {
    let data = get_json(&format!("https://api.runpod.ai/v2/{id}/health"), token)?;
    let workers = data
        .get("workers")
        .ok_or_else(|| FetchError::Format("workers fehlt".into()))?;
    let n = |key: &str| workers.get(key).and_then(Value::as_u64).unwrap_or(0);
    Ok(Workers {
        idle: n("idle"),
        initializing: n("initializing"),
        ready: n("ready"),
        running: n("running"),
        throttled: n("throttled"),
        unhealthy: n("unhealthy"),
    })
}

/// Zähler von llama.cpp seit dem Start des Workers.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Metrics {
    pub prompt_tokens: f64,
    pub prompt_seconds: f64,
    pub predicted_tokens: f64,
    pub predicted_seconds: f64,
    pub processing: f64,
    pub deferred: f64,
}

impl Metrics {
    /// Einlese-Tempo seit Worker-Start, Tokens pro Sekunde.
    pub fn prompt_rate(&self) -> Option<f64> {
        (self.prompt_seconds > 0.0).then(|| self.prompt_tokens / self.prompt_seconds)
    }

    /// Antwort-Tempo seit Worker-Start, Tokens pro Sekunde.
    pub fn predicted_rate(&self) -> Option<f64> {
        (self.predicted_seconds > 0.0).then(|| self.predicted_tokens / self.predicted_seconds)
    }
}

/// Prometheus-Text von llama.cpp lesen.
pub fn parse_metrics(text: &str) -> Result<Metrics, FetchError> {
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(name), Some(value)) = (parts.next(), parts.next()) else {
            continue;
        };
        if let Ok(value) = value.parse::<f64>() {
            values.insert(name.to_string(), value);
        }
    }
    let get = |name: &str| values.get(&format!("llamacpp:{name}")).copied();
    let prompt_tokens = get("prompt_tokens_total").ok_or_else(|| {
        FetchError::Format(tr!(
            "keine llama.cpp-Zähler (läuft llama-server mit --metrics?)",
            "no llama.cpp counters (is llama-server running with --metrics?)"
        ))
    })?;
    Ok(Metrics {
        prompt_tokens,
        prompt_seconds: get("prompt_seconds_total").unwrap_or(0.0),
        predicted_tokens: get("tokens_predicted_total").unwrap_or(0.0),
        predicted_seconds: get("tokens_predicted_seconds_total").unwrap_or(0.0),
        processing: get("requests_processing").unwrap_or(0.0),
        deferred: get("requests_deferred").unwrap_or(0.0),
    })
}

pub fn fetch_metrics(base_url: &str, token: &str) -> Result<Metrics, FetchError> {
    let mut response = usage::agent()
        .get(&format!("{base_url}/metrics"))
        .header("Authorization", &format!("Bearer {token}"))
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| FetchError::Network(e.to_string()))?;
    match response.status().as_u16() {
        200 => {}
        401 | 403 => return Err(FetchError::Unauthorized),
        code => return Err(FetchError::Http(code)),
    }
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| FetchError::Network(e.to_string()))?;
    parse_metrics(&body)
}

/// Fortgeschriebene Summen je Endpoint.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub prompt_tokens: u64,
    pub predicted_tokens: u64,
    /// Tag (lokal, `YYYY-MM-DD`), zu dem die Tageswerte gehören.
    #[serde(default)]
    pub day: String,
    #[serde(default)]
    pub day_prompt_tokens: u64,
    #[serde(default)]
    pub day_predicted_tokens: u64,
    /// Letzter gesehener Zählerstand des Workers.
    #[serde(default)]
    pub last_prompt: u64,
    #[serde(default)]
    pub last_predicted: u64,
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl Totals {
    /// Neuen Zählerstand übernehmen; liefert, ob sich etwas geändert hat.
    /// Ist ein Zähler kleiner als zuvor, lief inzwischen ein neuer Worker an.
    /// `new_worker`: seit der letzten Abfrage lief nachweislich ein neuer Worker an.
    pub fn record(&mut self, metrics: &Metrics, today: &str, new_worker: bool) -> bool {
        let prompt = metrics.prompt_tokens.max(0.0) as u64;
        let predicted = metrics.predicted_tokens.max(0.0) as u64;
        let restarted =
            new_worker || prompt < self.last_prompt || predicted < self.last_predicted;
        let (dp, dg) = if restarted {
            (prompt, predicted)
        } else {
            (prompt - self.last_prompt, predicted - self.last_predicted)
        };
        if self.day != today {
            self.day = today.to_string();
            self.day_prompt_tokens = 0;
            self.day_predicted_tokens = 0;
        }
        self.prompt_tokens += dp;
        self.predicted_tokens += dg;
        self.day_prompt_tokens += dp;
        self.day_predicted_tokens += dg;
        self.last_prompt = prompt;
        self.last_predicted = predicted;
        self.updated_at = Some(chrono::Local::now().to_rfc3339());
        dp + dg > 0
    }

    pub fn today(&self, today: &str) -> (u64, u64) {
        if self.day == today {
            (self.day_prompt_tokens, self.day_predicted_tokens)
        } else {
            (0, 0)
        }
    }
}

/// `runpod-stats.json` im Slot-Ordner, Schlüssel = Endpoint-ID.
pub struct StatsFile {
    path: PathBuf,
    pub entries: BTreeMap<String, Totals>,
}

impl StatsFile {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join(STATS_FILE);
        let entries = fsx::read_opt(&path)
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self { path, entries }
    }

    /// Zählerstand übernehmen und speichern; liefert, ob neue Tokens dazukamen.
    pub fn record(&mut self, id: &str, metrics: &Metrics, new_worker: bool) -> bool {
        // Erst frisch einlesen: ein zweites Fenster könnte geschrieben haben.
        let fresh = Self::load(self.path.parent().unwrap_or(Path::new(".")));
        self.entries = fresh.entries;
        let today = today();
        let totals = self.entries.entry(id.to_string()).or_default();
        // Lange nichts gehört (z. B. Programm war zu): der Worker von damit ist
        // längst abgeschaltet, die Zähler gehören zu einem neuen.
        let stale = totals
            .updated_at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .is_some_and(|at| chrono::Local::now().signed_duration_since(at) > chrono::Duration::minutes(30));
        let changed = totals.record(metrics, &today, new_worker || stale);
        if let Ok(text) = serde_json::to_string_pretty(&self.entries) {
            let _ = fsx::write_atomic(&self.path, text.as_bytes());
        }
        changed
    }
}

pub fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Wann welche Abfrage fällig ist. Ohne Netz, damit es testbar bleibt.
#[derive(Debug, Default)]
pub struct Schedule {
    last_health: Option<Instant>,
    last_metrics: Option<Instant>,
    /// Seit wann sich die Zähler nicht mehr geändert haben.
    quiet_since: Option<Instant>,
    /// Zähler-Abfragen eingestellt, bis ein neuer Worker startet oder "Aktualisieren".
    pub paused: bool,
    worker_up: bool,
    /// Seit der letzten Zähler-Abfrage ist ein Worker neu angelaufen.
    new_worker: bool,
    last_health_result: bool,
}

impl Schedule {
    pub fn health_due(&self, now: Instant) -> bool {
        self.last_health.is_none_or(|t| now.duration_since(t) >= HEALTH_EVERY)
    }

    pub fn health_started(&mut self, now: Instant) {
        self.last_health = Some(now);
    }

    /// Neuer Worker-Status. Ein neu gestarteter Worker hebt die Pause auf.
    pub fn health_result(&mut self, workers: &Workers) {
        let up = workers.up();
        if up && !self.worker_up {
            self.paused = false;
            self.quiet_since = None;
            // Beim allerersten Status ist unklar, ob der Worker neu ist.
            self.new_worker = self.last_health_result;
        }
        self.last_health_result = true;
        self.worker_up = up;
    }

    /// Sollen jetzt die Zähler abgefragt werden? Nie ohne laufenden Worker.
    pub fn metrics_due(&self, now: Instant, manual: bool) -> bool {
        if !self.worker_up {
            return false;
        }
        if manual {
            return true;
        }
        !self.paused && self.last_metrics.is_none_or(|t| now.duration_since(t) >= METRICS_EVERY)
    }

    pub fn metrics_started(&mut self, now: Instant, manual: bool) {
        self.last_metrics = Some(now);
        if manual {
            self.paused = false;
            self.quiet_since = None;
        }
    }

    /// Für `StatsFile::record`: ist ein neuer Worker angelaufen? Setzt das Merkmal zurück.
    pub fn take_new_worker(&mut self) -> bool {
        std::mem::take(&mut self.new_worker)
    }

    /// Ergebnis einer Zähler-Abfrage: gearbeitet wird, solange neue Tokens
    /// dazukommen oder Anfragen laufen.
    pub fn metrics_result(&mut self, now: Instant, busy: bool) {
        if busy {
            self.quiet_since = None;
            return;
        }
        let since = *self.quiet_since.get_or_insert(now);
        if now.duration_since(since) >= QUIET_STOP {
            self.paused = true;
        }
    }
}

/// 1234 → "1.234" (Deutsch) bzw. "1,234", ab einer Million "2,50 Mio." / "2.50 M"
pub fn format_tokens(n: u64) -> String {
    match crate::i18n::get() {
        crate::i18n::Lang::De => format_with(n, '.', ',', "Mio."),
        crate::i18n::Lang::En => format_with(n, ',', '.', "M"),
    }
}

fn format_with(n: u64, thousands: char, decimal: char, million: &str) -> String {
    if n >= 1_000_000 {
        let m = format!("{:.2}", n as f64 / 1_000_000.0).replace('.', &decimal.to_string());
        return format!("{m} {million}");
    }
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(thousands);
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_runpod_endpoints() {
        assert_eq!(endpoint_id("https://504ljfwnbzfgsq.api.runpod.ai").as_deref(), Some("504ljfwnbzfgsq"));
        assert_eq!(endpoint_id("https://ABC.api.runpod.ai/").as_deref(), Some("abc"));
        assert_eq!(endpoint_id("https://api.runpod.ai"), None);
        assert_eq!(endpoint_id("https://x.y.api.runpod.ai"), None);
        assert_eq!(endpoint_id("http://localhost:8080"), None);
    }

    #[test]
    fn parses_llama_metrics() {
        let text = "\
# HELP llamacpp:prompt_tokens_total Number of prompt tokens processed.
# TYPE llamacpp:prompt_tokens_total counter
llamacpp:prompt_tokens_total 270788
llamacpp:prompt_seconds_total 77.931
llamacpp:tokens_predicted_total 109
llamacpp:tokens_predicted_seconds_total 1.169
llamacpp:requests_processing 2
llamacpp:requests_deferred 0
";
        let m = parse_metrics(text).unwrap();
        assert_eq!(m.prompt_tokens, 270788.0);
        assert_eq!(m.processing, 2.0);
        assert!((m.predicted_rate().unwrap() - 93.24).abs() < 0.1);
        assert!(parse_metrics("foo 1").is_err());
    }

    #[test]
    fn totals_survive_worker_restarts() {
        let metrics = |p: f64, g: f64| Metrics {
            prompt_tokens: p,
            predicted_tokens: g,
            ..Default::default()
        };
        let mut totals = Totals::default();
        assert!(totals.record(&metrics(1000.0, 10.0), "2026-11-02", false));
        assert!(totals.record(&metrics(1500.0, 30.0), "2026-11-02", false));
        assert!(!totals.record(&metrics(1500.0, 30.0), "2026-11-02", false));
        // Neuer Worker: Zähler fangen von vorn an.
        assert!(totals.record(&metrics(200.0, 5.0), "2026-11-02", false));
        assert_eq!((totals.prompt_tokens, totals.predicted_tokens), (1700, 35));
        assert_eq!(totals.today("2026-11-02"), (1700, 35));
        // Neuer Worker, dessen Zähler schon höher sind: zählt ganz.
        assert!(totals.record(&metrics(2000.0, 40.0), "2026-11-02", true));
        assert_eq!(totals.prompt_tokens, 3700);
        // Neuer Tag.
        totals.record(&metrics(2100.0, 40.0), "2026-11-03", false);
        assert_eq!(totals.today("2026-11-03"), (100, 0));
        assert_eq!(totals.today("2026-11-02"), (0, 0));
        assert_eq!(totals.prompt_tokens, 3800);
    }

    #[test]
    fn stops_polling_when_quiet() {
        let t0 = Instant::now();
        let mut s = Schedule::default();
        let up = Workers { running: 1, ..Default::default() };
        // Ohne Worker nie die Zähler abfragen (würde ihn starten).
        assert!(!s.metrics_due(t0, true));
        s.health_result(&up);
        assert!(s.metrics_due(t0, false));
        s.metrics_started(t0, false);
        s.metrics_result(t0, false);
        assert!(!s.metrics_due(t0 + Duration::from_secs(10), false));
        let later = t0 + QUIET_STOP;
        s.metrics_result(later, false);
        assert!(s.paused);
        assert!(!s.metrics_due(later + METRICS_EVERY, false));
        // "Aktualisieren" fragt trotzdem.
        assert!(s.metrics_due(later, true));
        // Worker aus und wieder an: Pause aufgehoben, neuer Worker gemeldet.
        assert!(!s.take_new_worker(), "first status is no restart");
        s.health_result(&Workers::default());
        s.health_result(&up);
        assert!(!s.paused);
        assert!(s.take_new_worker());
        assert!(!s.take_new_worker());
    }

    #[test]
    fn formats_token_counts() {
        assert_eq!(format_with(950, '.', ',', "Mio."), "950");
        assert_eq!(format_with(270_788, '.', ',', "Mio."), "270.788");
        assert_eq!(format_with(2_500_000, '.', ',', "Mio."), "2,50 Mio.");
        assert_eq!(format_with(1_234, ',', '.', "M"), "1,234");
    }
}
