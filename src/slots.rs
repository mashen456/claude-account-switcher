//! Die Slots und das eigentliche Umschalten.
//!
//! Ein Slot ist eine Kopie von dem, was Claude Code nach `/login` ablegt: das
//! Token-Objekt aus `.credentials.json` und der Block `oauthAccount` aus
//! `~/.claude.json`.
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
use crate::fsx;
use crate::locks;
use crate::store::{self, CredentialStore};

pub const SLOT_COUNT: usize = 2;

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
    /// Komplettes Token-Objekt wie in `.credentials.json`.
    pub credentials: Value,
    /// Block `oauthAccount` aus `~/.claude.json`.
    pub oauth_account: Value,
    pub saved_at: String,
}

impl Slot {
    pub fn identity(&self) -> Option<Identity> {
        Identity::from_oauth_account(&self.oauth_account)
    }
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
                .context("Ordner für Programmdaten nicht gefunden")?
                .join("claude-slots"),
        };
        fs::create_dir_all(&dir).with_context(|| format!("{} nicht anlegbar", dir.display()))?;
        Ok(Self { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn slot_path(&self, n: usize) -> PathBuf {
        self.dir.join(format!("slot{n}.json"))
    }

    pub fn load(&self, n: usize) -> Result<Option<Slot>> {
        let path = self.slot_path(n);
        let Some(text) = fsx::read_opt(&path)? else {
            return Ok(None);
        };
        serde_json::from_str(&text)
            .map(Some)
            .with_context(|| format!("{} ist beschädigt", path.display()))
    }

    fn save(&self, n: usize, slot: &Slot) -> Result<()> {
        let text = serde_json::to_string_pretty(slot)?;
        fsx::write_atomic(&self.slot_path(n), text.as_bytes())
    }

    fn find(&self, id: &Identity) -> Result<Option<usize>> {
        for n in 1..=SLOT_COUNT {
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
        let copy = Slot {
            credentials: live.credentials.clone(),
            oauth_account: live.oauth_account.clone(),
            saved_at: now(),
        };
        let text = serde_json::to_string_pretty(&copy)?;
        fsx::write_atomic(&self.dir.join("last-live.json"), text.as_bytes())
    }
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
    let credentials: Value =
        serde_json::from_str(&text).context("Claude Codes Token-Datei ist kein gültiges JSON")?;
    let oauth_account = claude::oauth_account()?
        .context("Claude Code hat ein Token, aber in ~/.claude.json steht kein Account")?;
    let identity = Identity::from_oauth_account(&oauth_account)
        .context("Der Account in ~/.claude.json hat weder E-Mail noch ID")?;
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

/// Speichert den aktuellen Login von Claude Code in Slot `n`.
pub fn capture(slots: &SlotStore, n: usize) -> Result<Identity> {
    let creds = store::active()?;
    let _locks = locks::claude_locks()?;
    let live = read_live_from(&*creds)?.ok_or_else(|| anyhow!("Claude Code ist nicht eingeloggt"))?;
    if !has_tokens(&live.credentials) {
        bail!("Der aktuelle Login hat keine Tokens mehr, in Claude Code neu einloggen (/login)");
    }
    if let Some(other) = slots.find(&live.identity)? {
        if other != n {
            bail!("{} liegt schon in Slot {other}", live.identity.email);
        }
    }
    slots.save(
        n,
        &Slot {
            credentials: live.credentials,
            oauth_account: live.oauth_account,
            saved_at: now(),
        },
    )?;
    Ok(live.identity)
}

/// Legt einen frisch eingeloggten Account (siehe `login.rs`) in Slot `n` ab.
///
/// Ist es der Account, mit dem Claude Code gerade arbeitet, bekommt auch
/// Claude Code den neuen Login, sonst schriebe der nächste Wechsel den alten
/// Stand über den neuen. Liefert zusätzlich, ob Claude Code umgestellt wurde.
pub fn store_login(slots: &SlotStore, n: usize, mut slot: Slot) -> Result<(Identity, bool)> {
    let id = slot
        .identity()
        .ok_or_else(|| anyhow!("Die Anmeldung enthält keinen Account"))?;
    if let Some(other) = slots.find(&id)? {
        if other != n {
            bail!("{} liegt schon in Slot {other}", id.email);
        }
    }
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
        return Err(err.context("Abgebrochen, alter Login wiederhergestellt"));
    }
    Ok(())
}

/// Macht Slot `target` zum Login von Claude Code.
pub fn switch_to(slots: &SlotStore, target: usize) -> Result<Identity> {
    let target_slot = slots
        .load(target)?
        .ok_or_else(|| anyhow!("Slot {target} ist leer"))?;
    let target_id = target_slot
        .identity()
        .ok_or_else(|| anyhow!("Slot {target} enthält keinen Account"))?;
    if !has_tokens(&target_slot.credentials) {
        bail!("Slot {target} hat keine Tokens, Login neu übernehmen");
    }

    let creds = store::active()?;
    let _locks = locks::claude_locks()?;
    let live = read_live_from(&*creds)?;

    // 1. Bisherigen Account sichern.
    if let Some(live) = &live {
        if live.identity.same_account(&target_id) {
            bail!("Slot {target} ist bereits aktiv");
        }
        let Some(current) = slots.find(&live.identity)? else {
            bail!(
                "Claude Code ist mit {} eingeloggt, und der Account liegt in keinem Slot. \
                 Erst \"Login übernehmen\", sonst geht dieser Login verloren.",
                live.identity.email
            );
        };
        slots.write_safety_copy(live)?;
        let stored = slots.load(current)?.map(|slot| slot.credentials).unwrap_or_default();
        if may_replace(&stored, &live.credentials) {
            slots.save(
                current,
                &Slot {
                    credentials: live.credentials.clone(),
                    oauth_account: live.oauth_account.clone(),
                    saved_at: now(),
                },
            )?;
        }
    }

    // 2. Ziel-Slot für Claude Code schreiben.
    write_live(
        &*creds,
        &with_shared_keys(&target_slot.credentials, live.as_ref()),
        &target_slot.oauth_account,
    )?;
    Ok(target_id)
}

fn now() -> String {
    chrono::Local::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn login_slot(who: &str, refresh: &str) -> Slot {
        Slot {
            credentials: json!({ "claudeAiOauth": { "accessToken": format!("at-{refresh}"),
                                                    "refreshToken": refresh } }),
            oauth_account: json!({ "accountUuid": format!("uuid-{who}"),
                                   "emailAddress": format!("{who}@example.com"),
                                   "organizationUuid": format!("org-{who}") }),
            saved_at: now(),
        }
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Einziger Test, der Umgebungsvariablen setzt (Tests laufen parallel).
    #[test]
    fn store_login_updates_claude_code_only_for_its_account() {
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

        let _ = fs::remove_dir_all(&root);
    }
}
