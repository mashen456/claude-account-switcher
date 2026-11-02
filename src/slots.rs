//! Die Slots und das eigentliche Umschalten.
//!
//! Ein Slot ist eine Kopie von dem, was Claude Code nach `/login` ablegt: das
//! Token-Objekt aus `.credentials.json` und der Block `oauthAccount` aus
//! `~/.claude.json`. Slots sind von 1 an durchnummeriert (`slot1.json`, …),
//! ihre Anzahl steht in `settings.json`.
//!
//! Wichtigste Regel: Ein Refresh-Token gilt nur einmal. Claude Code erneuert
//! das Token des aktiven Accounts laufend, die Kopie im Slot veraltet dabei.
//! Deshalb wird vor jedem Wechsel der aktuelle Stand in den Slot des
//! bisherigen Accounts zurückgeschrieben. Ein inaktiver Slot wird nie selbst
//! erneuert, also kann dieses Programm kein Token verbrauchen.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::claude::{self, Identity};
use crate::endpoint::{self, Endpoint};
use crate::fsx;
use crate::i18n::Lang;
use crate::locks;
use crate::settings::Settings;
use crate::store::{self, CredentialStore};

/// Obergrenze, damit das Fenster überschaubar bleibt.
pub const MAX_SLOTS: usize = 10;

/// Maschinenweite Felder in `.credentials.json` (MCP- und Plugin-Logins).
/// Sie gehören keinem Account, deshalb bleibt beim Wechsel der aktuelle Stand.
const SHARED_KEYS: [&str; 5] = [
    "mcpOAuth",
    "mcpOAuthClientConfig",
    "mcpXaaIdp",
    "mcpXaaIdpConfig",
    "pluginSecrets",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Slot {
    /// Komplettes Token-Objekt wie in `.credentials.json`. Bei Endpoint-Slots leer.
    #[serde(default)]
    pub credentials: Value,
    /// Block `oauthAccount` aus `~/.claude.json`. Bei Endpoint-Slots leer.
    #[serde(default)]
    pub oauth_account: Value,
    /// Gesetzt bei einem Endpoint-Slot (eigener Server statt Claude-Account).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<Endpoint>,
    pub saved_at: String,
}

impl Slot {
    pub fn account(credentials: Value, oauth_account: Value) -> Self {
        Self {
            credentials,
            oauth_account,
            endpoint: None,
            saved_at: now(),
        }
    }

    pub fn with_endpoint(endpoint: Endpoint) -> Self {
        Self {
            credentials: Value::Null,
            oauth_account: Value::Null,
            endpoint: Some(endpoint),
            saved_at: now(),
        }
    }

    /// Account eines Account-Slots, `None` bei leeren und Endpoint-Slots.
    pub fn identity(&self) -> Option<Identity> {
        if self.endpoint.is_some() {
            return None;
        }
        Identity::from_oauth_account(&self.oauth_account)
    }

    /// E-Mail des Accounts bzw. Name des Endpoints.
    pub fn label(&self) -> String {
        match &self.endpoint {
            Some(endpoint) => endpoint.display_name(),
            None => self.identity().map(|id| id.email).unwrap_or_default(),
        }
    }
}

/// Wohin gewechselt wurde.
pub struct Switched {
    /// E-Mail des Accounts bzw. Name des Endpoints.
    pub label: String,
    /// Ein laufendes Claude Code muss neu gestartet werden (settings.json geändert).
    pub restart_needed: bool,
}

/// Der Login, mit dem Claude Code gerade arbeitet.
pub struct Live {
    pub credentials: Value,
    pub oauth_account: Value,
    pub identity: Identity,
}

pub struct SlotStore {
    dir: PathBuf,
}

impl SlotStore {
    /// Windows: `%LOCALAPPDATA%\claude-slots`, macOS: `~/Library/Application Support/claude-slots`.
    /// `CLAUDE_SLOTS_DIR` setzt einen anderen Ordner (zum Testen).
    pub fn open() -> Result<Self> {
        let dir = match std::env::var_os("CLAUDE_SLOTS_DIR").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => dirs::data_local_dir()
                .with_context(|| tr!("Ordner für Programmdaten nicht gefunden", "App data folder not found"))?
                .join("claude-slots"),
        };
        let shown = dir.display();
        fs::create_dir_all(&dir)
            .with_context(|| tr!("{shown} nicht anlegbar", "Cannot create {shown}"))?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn settings(&self) -> Settings {
        Settings::load(&self.dir)
    }

    pub fn set_language(&self, lang: Lang) -> Result<()> {
        let mut settings = self.settings();
        settings.language = Some(lang);
        settings.save(&self.dir)
    }

    /// Anzahl laut Einstellungen, mindestens aber bis zur höchsten Slot-Datei.
    pub fn count(&self) -> usize {
        let highest = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                name.to_str()?
                    .strip_prefix("slot")?
                    .strip_suffix(".json")?
                    .parse::<usize>()
                    .ok()
            })
            .max()
            .unwrap_or(0);
        self.settings().slot_count.max(highest).clamp(1, MAX_SLOTS)
    }

    /// Hängt einen leeren Slot an und liefert seine Nummer.
    pub fn add_slot(&self) -> Result<usize> {
        let count = self.count();
        if count >= MAX_SLOTS {
            bail!(tr!("Mehr als {MAX_SLOTS} Slots gehen nicht", "No more than {MAX_SLOTS} slots"));
        }
        let mut settings = self.settings();
        settings.slot_count = count + 1;
        settings.save(&self.dir)?;
        Ok(count + 1)
    }

    /// Entfernt Slot `n` samt gespeichertem Login, die folgenden Slots rücken nach.
    pub fn remove_slot(&self, n: usize) -> Result<()> {
        let count = self.count();
        if count <= 1 {
            bail!(tr!("Der letzte Slot bleibt", "The last slot cannot be removed"));
        }
        if !(1..=count).contains(&n) {
            bail!(tr!("Slot {n} gibt es nicht", "There is no slot {n}"));
        }
        let slot = self.load(n)?;
        // Ein aktiver Endpoint ließe sich danach nicht mehr sauber abschalten.
        if let Some(endpoint) = slot.as_ref().and_then(|slot| slot.endpoint.as_ref()) {
            if endpoint::is_active(endpoint)? {
                bail!(tr!(
                    "Slot {n} ist gerade aktiv. Erst zu einem anderen Slot wechseln.",
                    "Slot {n} is currently active. Switch to another slot first."
                ));
            }
        }
        // Der aktive Login läge danach in keinem Slot, und Wechseln wäre gesperrt.
        if let (Some(slot), Some(live)) = (slot, read_live()?) {
            if slot.identity().is_some_and(|id| id.same_account(&live.identity)) {
                bail!(tr!(
                    "Slot {n} ist gerade aktiv. Erst zu einem anderen Slot wechseln.",
                    "Slot {n} is currently active. Switch to another slot first."
                ));
            }
        }
        let path = self.slot_path(n);
        if path.exists() {
            let shown = path.display();
            fs::remove_file(&path)
                .with_context(|| tr!("{shown} nicht löschbar", "Cannot delete {shown}"))?;
        }
        // Von unten nach oben verschieben: das Ziel ist immer schon frei.
        for k in n + 1..=count {
            let src = self.slot_path(k);
            if src.exists() {
                let shown = src.display();
                fs::rename(&src, self.slot_path(k - 1))
                    .with_context(|| tr!("{shown} nicht verschiebbar", "Cannot move {shown}"))?;
            }
        }
        let mut settings = self.settings();
        settings.slot_count = count - 1;
        settings.save(&self.dir)
    }

    fn slot_path(&self, n: usize) -> PathBuf {
        self.dir.join(format!("slot{n}.json"))
    }

    pub fn load(&self, n: usize) -> Result<Option<Slot>> {
        let path = self.slot_path(n);
        let Some(text) = fsx::read_opt(&path)? else {
            return Ok(None);
        };
        let shown = path.display();
        serde_json::from_str(&text)
            .map(Some)
            .with_context(|| tr!("{shown} ist beschädigt", "{shown} is corrupt"))
    }

    fn save(&self, n: usize, slot: &Slot) -> Result<()> {
        let text = serde_json::to_string_pretty(slot)?;
        fsx::write_atomic(&self.slot_path(n), text.as_bytes())
    }

    fn find(&self, id: &Identity) -> Result<Option<usize>> {
        for n in 1..=self.count() {
            if let Some(slot) = self.load(n)? {
                if slot.identity().is_some_and(|other| other.same_account(id)) {
                    return Ok(Some(n));
                }
            }
        }
        Ok(None)
    }

    /// Sicherheitskopie des Live-Logins, bevor er überschrieben wird.
    fn write_safety_copy(&self, live: &Live) -> Result<()> {
        let copy = Slot::account(live.credentials.clone(), live.oauth_account.clone());
        let text = serde_json::to_string_pretty(&copy)?;
        fsx::write_atomic(&self.dir.join("last-live.json"), text.as_bytes())
    }

    /// Der Endpoint-Slot, der gerade in `settings.json` eingetragen ist.
    pub fn active_endpoint(&self) -> Result<Option<usize>> {
        let env = endpoint::current_env()?;
        for n in 1..=self.count() {
            if let Some(endpoint) = self.load(n)?.and_then(|slot| slot.endpoint) {
                if endpoint::is_active_in(&endpoint, &env) {
                    return Ok(Some(n));
                }
            }
        }
        Ok(None)
    }

    /// Ist (noch) ein Endpoint eingetragen, den dieses Programm gesetzt hat?
    fn endpoint_engaged(&self) -> Result<bool> {
        Ok(endpoint::backup_exists(&self.dir) || self.active_endpoint()?.is_some())
    }
}

/// Legt einen Endpoint in Slot `n` ab (neu oder geändert). Ein Slot mit
/// Claude-Account wird nicht überschrieben. War der bisherige Endpoint dieses
/// Slots aktiv, werden die neuen Werte gleich eingetragen; liefert dann `true`.
pub fn save_endpoint(slots: &SlotStore, n: usize, endpoint: Endpoint) -> Result<bool> {
    let endpoint = endpoint.normalized()?;
    let old = slots.load(n)?;
    if let Some(id) = old.as_ref().and_then(Slot::identity) {
        let email = id.email;
        bail!(tr!(
            "In Slot {n} liegt der Account {email}. Für einen Endpoint einen leeren Slot nehmen.",
            "Slot {n} holds the account {email}. Use an empty slot for an endpoint."
        ));
    }
    let was_active = match old.as_ref().and_then(|slot| slot.endpoint.as_ref()) {
        Some(old) => endpoint::is_active(old)?,
        None => false,
    };
    slots.save(n, &Slot::with_endpoint(endpoint.clone()))?;
    if was_active {
        endpoint::activate(slots.dir(), &endpoint)?;
    }
    Ok(was_active)
}

pub fn read_live() -> Result<Option<Live>> {
    read_live_from(&*store::active()?)
}

fn read_live_from(creds: &dyn CredentialStore) -> Result<Option<Live>> {
    let Some(text) = creds.read()? else {
        return Ok(None);
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    let credentials: Value = serde_json::from_str(&text).with_context(|| {
        tr!("Claude Codes Token-Datei ist kein gültiges JSON", "Claude Code's token file is not valid JSON")
    })?;
    let oauth_account = claude::oauth_account()?.with_context(|| {
        tr!(
            "Claude Code hat ein Token, aber in ~/.claude.json steht kein Account",
            "Claude Code has a token, but ~/.claude.json names no account"
        )
    })?;
    let identity = Identity::from_oauth_account(&oauth_account).with_context(|| {
        tr!(
            "Der Account in ~/.claude.json hat weder E-Mail noch ID",
            "The account in ~/.claude.json has neither email nor ID"
        )
    })?;
    Ok(Some(Live {
        credentials,
        oauth_account,
        identity,
    }))
}

/// Claude Code leert Access- und Refresh-Token, wenn eine Erneuerung
/// abgelehnt wurde. So ein Objekt darf nie einen Slot überschreiben.
pub fn has_tokens(credentials: &Value) -> bool {
    let oauth = &credentials["claudeAiOauth"];
    ["accessToken", "refreshToken"]
        .iter()
        .any(|key| oauth[key].as_str().is_some_and(|s| !s.is_empty()))
}

fn has_refresh_token(credentials: &Value) -> bool {
    credentials["claudeAiOauth"]["refreshToken"]
        .as_str()
        .is_some_and(|s| !s.is_empty())
}

/// Darf der Live-Stand den gespeicherten ersetzen? Nie mit leeren Tokens und
/// nie ohne Refresh-Token über einen Stand mit Refresh-Token.
fn may_replace(stored: &Value, live: &Value) -> bool {
    has_tokens(live) && (has_refresh_token(live) || !has_refresh_token(stored))
}

fn already_in_slot(email: &str, other: usize) -> anyhow::Error {
    anyhow!(tr!("{email} liegt schon in Slot {other}", "{email} is already in slot {other}"))
}

/// Speichert den aktuellen Login von Claude Code in Slot `n`.
pub fn capture(slots: &SlotStore, n: usize) -> Result<Identity> {
    let creds = store::active()?;
    let _locks = locks::claude_locks()?;
    let live = read_live_from(&*creds)?
        .ok_or_else(|| anyhow!(tr!("Claude Code ist nicht eingeloggt", "Claude Code is not logged in")))?;
    if !has_tokens(&live.credentials) {
        bail!(tr!(
            "Der aktuelle Login hat keine Tokens mehr, in Claude Code neu einloggen (/login)",
            "The current login has no tokens left, log in again in Claude Code (/login)"
        ));
    }
    if let Some(other) = slots.find(&live.identity)? {
        if other != n {
            return Err(already_in_slot(&live.identity.email, other));
        }
    }
    refuse_active_endpoint(slots, n)?;
    slots.save(n, &Slot::account(live.credentials, live.oauth_account))?;
    Ok(live.identity)
}

/// Ein Endpoint, den Claude Code gerade nutzt, wird nicht überschrieben:
/// danach ließe er sich nicht mehr sauber abschalten.
fn refuse_active_endpoint(slots: &SlotStore, n: usize) -> Result<()> {
    if let Some(endpoint) = slots.load(n)?.and_then(|slot| slot.endpoint) {
        if endpoint::is_active(&endpoint)? {
            bail!(tr!(
                "Slot {n} ist der aktive Endpoint. Erst zu einem anderen Slot wechseln.",
                "Slot {n} is the active endpoint. Switch to another slot first."
            ));
        }
    }
    Ok(())
}

/// Legt einen frisch eingeloggten Account (siehe `login.rs`) in Slot `n` ab.
///
/// Ist es der Account, mit dem Claude Code gerade arbeitet, bekommt auch
/// Claude Code den neuen Login, sonst schriebe der nächste Wechsel den alten
/// Stand über den neuen. Liefert zusätzlich, ob Claude Code umgestellt wurde.
pub fn store_login(slots: &SlotStore, n: usize, mut slot: Slot) -> Result<(Identity, bool)> {
    let id = slot.identity().ok_or_else(|| {
        anyhow!(tr!("Die Anmeldung enthält keinen Account", "The sign-in returned no account"))
    })?;
    if let Some(other) = slots.find(&id)? {
        if other != n {
            return Err(already_in_slot(&id.email, other));
        }
    }
    refuse_active_endpoint(slots, n)?;
    // Felder, die nur Claude Code nachlädt (z. B. Rollen), vom bisherigen Stand behalten.
    if let Some(old) = slots.load(n)? {
        if old.identity().is_some_and(|old_id| old_id.same_account(&id)) {
            slot.oauth_account = merged(&old.oauth_account, &slot.oauth_account);
        }
    }

    let creds = store::active()?;
    let _locks = locks::claude_locks()?;
    let live = read_live_from(&*creds)?;
    slots.save(n, &slot)?;
    let Some(live) = live.filter(|live| live.identity.same_account(&id)) else {
        return Ok((id, false));
    };
    slots.write_safety_copy(&live)?;
    write_live(
        &*creds,
        &with_shared_keys(&slot.credentials, Some(&live)),
        &merged(&live.oauth_account, &slot.oauth_account),
    )?;
    Ok((id, true))
}

fn merged(base: &Value, overlay: &Value) -> Value {
    let mut out = base.as_object().cloned().unwrap_or_default();
    for (key, value) in overlay.as_object().into_iter().flatten() {
        out.insert(key.clone(), value.clone());
    }
    Value::Object(out)
}

/// Account-gebundene Felder aus `target`, maschinenweite (MCP-Logins) vom aktuellen Stand.
fn with_shared_keys(target: &Value, live: Option<&Live>) -> Value {
    let mut credentials = target.clone();
    if let (Some(object), Some(live)) = (credentials.as_object_mut(), live) {
        for key in SHARED_KEYS {
            match live.credentials.get(key) {
                Some(value) => object.insert(key.to_string(), value.clone()),
                None => object.remove(key),
            };
        }
    }
    credentials
}

/// Schreibt Token und Account für Claude Code. Scheitert die Konfiguration,
/// wird das alte Token zurückgelegt.
fn write_live(creds: &dyn CredentialStore, credentials: &Value, oauth_account: &Value) -> Result<()> {
    let previous = creds.read()?;
    creds.write(&serde_json::to_string(credentials)?)?;
    if let Err(err) = claude::set_oauth_account(oauth_account) {
        if let Some(previous) = previous {
            let _ = creds.write(&previous);
        }
        return Err(err.context(tr!(
            "Abgebrochen, alter Login wiederhergestellt",
            "Aborted, previous login restored"
        )));
    }
    Ok(())
}

/// Macht Slot `target` zum Login bzw. Endpoint von Claude Code.
pub fn switch_to(slots: &SlotStore, target: usize) -> Result<Switched> {
    let target_slot = slots
        .load(target)?
        .ok_or_else(|| anyhow!(tr!("Slot {target} ist leer", "Slot {target} is empty")))?;

    // Endpoint: nur settings.json, der Claude-Login bleibt, wie er ist.
    if let Some(target_endpoint) = &target_slot.endpoint {
        if endpoint::is_active(target_endpoint)? {
            bail!(tr!("Slot {target} ist bereits aktiv", "Slot {target} is already active"));
        }
        endpoint::activate(slots.dir(), target_endpoint)?;
        return Ok(Switched {
            label: target_endpoint.display_name(),
            restart_needed: true,
        });
    }

    let target_id = target_slot.identity().ok_or_else(|| {
        anyhow!(tr!("Slot {target} enthält keinen Account", "Slot {target} holds no account"))
    })?;
    if !has_tokens(&target_slot.credentials) {
        bail!(tr!(
            "Slot {target} hat keine Tokens, bitte neu einloggen",
            "Slot {target} has no tokens, please log in again"
        ));
    }

    let endpoint_engaged = slots.endpoint_engaged()?;
    let creds = store::active()?;
    let _locks = locks::claude_locks()?;
    let live = read_live_from(&*creds)?;
    let switched = Switched {
        label: target_id.email.clone(),
        restart_needed: endpoint_engaged,
    };

    // 1. Bisherigen Account sichern.
    if let Some(live) = &live {
        if live.identity.same_account(&target_id) {
            // Nur der Endpoint lag noch darüber: den abschalten genügt.
            if endpoint_engaged {
                endpoint::deactivate(slots.dir())?;
                return Ok(switched);
            }
            bail!(tr!("Slot {target} ist bereits aktiv", "Slot {target} is already active"));
        }
        let Some(current) = slots.find(&live.identity)? else {
            let email = &live.identity.email;
            bail!(tr!(
                "Claude Code ist mit {email} eingeloggt, und der Account liegt in keinem Slot. \
                 Erst \"Login übernehmen\", sonst geht dieser Login verloren.",
                "Claude Code is logged in as {email}, and that account is in no slot. \
                 Use \"Take over login\" first, or this login would be lost."
            ));
        };
        slots.write_safety_copy(live)?;
        let stored = slots.load(current)?.map(|slot| slot.credentials).unwrap_or_default();
        if may_replace(&stored, &live.credentials) {
            slots.save(
                current,
                &Slot::account(live.credentials.clone(), live.oauth_account.clone()),
            )?;
        }
    }

    // 2. Ziel-Slot für Claude Code schreiben.
    write_live(
        &*creds,
        &with_shared_keys(&target_slot.credentials, live.as_ref()),
        &target_slot.oauth_account,
    )?;

    // 3. Einen Endpoint, der noch darüber liegt, abschalten.
    if endpoint_engaged {
        endpoint::deactivate(slots.dir())?;
    }
    Ok(switched)
}

fn now() -> String {
    chrono::Local::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn login_slot(who: &str, refresh: &str) -> Slot {
        Slot::account(
            json!({ "claudeAiOauth": { "accessToken": format!("at-{refresh}"),
                                       "refreshToken": refresh } }),
            json!({ "accountUuid": format!("uuid-{who}"),
                    "emailAddress": format!("{who}@example.com"),
                    "organizationUuid": format!("org-{who}") }),
        )
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    fn email(slots: &SlotStore, n: usize) -> Option<String> {
        slots.load(n).unwrap().and_then(|slot| slot.identity()).map(|id| id.email)
    }

    /// Einziger Test, der Umgebungsvariablen setzt (Tests laufen parallel).
    #[test]
    fn store_login_and_slot_management() {
        let root = std::env::temp_dir().join(format!("claude-slots-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let config = root.join("claude");
        fs::create_dir_all(&config).unwrap();
        // SAFETY: kein anderer Test liest oder setzt Umgebungsvariablen.
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", &config);
            std::env::set_var("CLAUDE_SLOTS_DIR", root.join("slots"));
        }
        let live_creds = json!({ "claudeAiOauth": { "accessToken": "at-A-old", "refreshToken": "A-old" },
                                 "mcpOAuth": { "server": "live" } });
        fs::write(config.join(".credentials.json"), live_creds.to_string()).unwrap();
        let live_config = json!({ "projects": { "x": {} },
                                  "oauthAccount": { "accountUuid": "uuid-a", "emailAddress": "a@example.com",
                                                    "organizationUuid": "org-a", "organizationRole": "admin" } });
        fs::write(config.join(".claude.json"), live_config.to_string()).unwrap();
        let slots = SlotStore::open().unwrap();
        assert_eq!(slots.count(), 2);

        // Anderer Account in leeren Slot: Claude Code bleibt unberührt.
        let (id, live) = store_login(&slots, 2, login_slot("b", "B1")).unwrap();
        assert_eq!((id.email.as_str(), live), ("b@example.com", false));
        assert_eq!(read(&config.join(".credentials.json")), live_creds);

        // Neuer Login für den aktiven Account: Claude Code bekommt ihn auch.
        let (_, live) = store_login(&slots, 1, login_slot("a", "A-new")).unwrap();
        assert!(live);
        let creds = read(&config.join(".credentials.json"));
        assert_eq!(creds["claudeAiOauth"]["refreshToken"], "A-new");
        assert_eq!(creds["mcpOAuth"]["server"], "live");
        let cfg = read(&config.join(".claude.json"));
        assert_eq!(cfg["oauthAccount"]["organizationRole"], "admin");
        assert!(cfg["projects"]["x"].is_object());
        assert_eq!(slots.load(1).unwrap().unwrap().credentials["claudeAiOauth"]["refreshToken"], "A-new");
        assert!(root.join("slots").join("last-live.json").exists());

        // Derselbe Account nicht in zwei Slots.
        assert!(store_login(&slots, 2, login_slot("a", "A-x")).is_err());

        // Slots hinzufügen: der neue ist leer und zählt bei der Suche mit.
        assert_eq!(slots.add_slot().unwrap(), 3);
        assert_eq!(slots.count(), 3);
        assert_eq!(email(&slots, 3), None);
        store_login(&slots, 3, login_slot("c", "C1")).unwrap();
        assert!(store_login(&slots, 3, login_slot("b", "B2")).is_err());

        // Entfernen: die folgenden Slots rücken nach.
        slots.remove_slot(2).unwrap();
        assert_eq!(slots.count(), 2);
        assert_eq!(email(&slots, 1).as_deref(), Some("a@example.com"));
        assert_eq!(email(&slots, 2).as_deref(), Some("c@example.com"));
        assert!(!root.join("slots").join("slot3.json").exists());

        // Den aktiven Slot nicht, den letzten auch nicht.
        assert!(slots.remove_slot(1).is_err());
        slots.remove_slot(2).unwrap();
        assert_eq!(slots.count(), 1);
        assert!(slots.remove_slot(1).is_err());

        // Grenze nach oben.
        for _ in 1..MAX_SLOTS {
            slots.add_slot().unwrap();
        }
        assert_eq!(slots.count(), MAX_SLOTS);
        assert!(slots.add_slot().is_err());

        // Endpoint-Slots: eigene Einstellungen in settings.json bleiben erhalten.
        let settings_path = config.join("settings.json");
        fs::write(
            &settings_path,
            json!({ "theme": "dark", "env": { "FOO": "1", "ANTHROPIC_MODEL": "user-model" } }).to_string(),
        )
        .unwrap();
        let runpod = Endpoint {
            name: "Runpod".into(),
            base_url: "https://abc.api.runpod.ai/".into(),
            auth_token: "rpa_secret".into(),
            model: "qwen3-coder-next".into(),
        };
        assert!(!save_endpoint(&slots, 2, runpod.clone()).unwrap());
        assert!(save_endpoint(&slots, 1, runpod.clone()).is_err(), "account slot is kept");
        assert_eq!(slots.load(2).unwrap().unwrap().label(), "Runpod");
        assert_eq!(slots.active_endpoint().unwrap(), None);

        let switched = switch_to(&slots, 2).unwrap();
        assert!(switched.restart_needed);
        assert_eq!(switched.label, "Runpod");
        let settings = read(&settings_path);
        assert_eq!(settings["theme"], "dark");
        assert_eq!(settings["env"]["FOO"], "1");
        assert_eq!(settings["env"]["ANTHROPIC_BASE_URL"], "https://abc.api.runpod.ai");
        assert_eq!(settings["env"]["ANTHROPIC_AUTH_TOKEN"], "rpa_secret");
        assert_eq!(settings["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "qwen3-coder-next");
        assert_eq!(slots.active_endpoint().unwrap(), Some(2));
        // Der Claude-Login bleibt unberührt.
        assert_eq!(read(&config.join(".credentials.json"))["claudeAiOauth"]["refreshToken"], "A-new");
        assert!(switch_to(&slots, 2).is_err());
        assert!(slots.remove_slot(2).is_err());
        assert!(capture(&slots, 2).is_err());

        // Aktiven Endpoint bearbeiten: die neuen Werte landen sofort in settings.json.
        let changed = Endpoint { model: "other-model".into(), ..runpod.clone() };
        assert!(save_endpoint(&slots, 2, changed).unwrap());
        assert_eq!(read(&settings_path)["env"]["ANTHROPIC_MODEL"], "other-model");

        // Zurück zum selben Account: nur der Endpoint verschwindet, alte Werte zurück.
        let switched = switch_to(&slots, 1).unwrap();
        assert!(switched.restart_needed);
        assert_eq!(switched.label, "a@example.com");
        let settings = read(&settings_path);
        assert_eq!(settings["env"]["ANTHROPIC_MODEL"], "user-model");
        assert_eq!(settings["env"]["FOO"], "1");
        assert!(settings["env"].get("ANTHROPIC_BASE_URL").is_none());
        assert!(settings["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
        assert_eq!(slots.active_endpoint().unwrap(), None);
        assert!(switch_to(&slots, 1).is_err(), "already active");

        // Endpoint → anderer Account: Token wechselt, Endpoint wird abgeschaltet.
        fs::write(&settings_path, "{}").unwrap();
        store_login(&slots, 3, login_slot("c", "C2")).unwrap();
        switch_to(&slots, 2).unwrap();
        let switched = switch_to(&slots, 3).unwrap();
        assert_eq!(switched.label, "c@example.com");
        assert_eq!(read(&config.join(".credentials.json"))["claudeAiOauth"]["refreshToken"], "C2");
        assert_eq!(read(&settings_path), json!({}));
        assert!(!endpoint::backup_exists(slots.dir()));

        // Leerer Slot nach einem Endpoint wieder als Account nutzbar.
        slots.remove_slot(2).unwrap();
        assert_eq!(email(&slots, 2).as_deref(), Some("c@example.com"));

        let _ = fs::remove_dir_all(&root);
    }
}
