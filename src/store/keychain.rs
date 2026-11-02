//! macOS: Claude Code speichert das Token im Schlüsselbund (Dienst
//! "Claude Code-credentials", Konto = Benutzername). Die Datei
//! `~/.claude/.credentials.json` gibt es dort nur als Rückfall, wenn der
//! Schlüsselbund gesperrt ist (z. B. per SSH).
//!
//! Mit `CLAUDE_CONFIG_DIR` hängt Claude Code an den Dienstnamen einen Bindestrich
//! und die ersten 8 Hex-Zeichen des SHA-256 des Ordnerpfads an. Das ist aus
//! anderen Werkzeugen übernommen, die denselben Eintrag lesen, und noch nicht
//! auf einem Mac geprüft.

use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::{CredentialStore, FileStore};
use crate::claude;

const SERVICE: &str = "Claude Code-credentials";
const SECURITY: &str = "/usr/bin/security";
/// Exit-Code von `security`, wenn der Eintrag nicht existiert.
const NOT_FOUND: i32 = 44;

pub struct KeychainStore {
    service: String,
    account: String,
    file: FileStore,
}

impl KeychainStore {
    pub fn new() -> Result<Self> {
        let service = match std::env::var("CLAUDE_CONFIG_DIR").ok().filter(|v| !v.is_empty()) {
            Some(dir) => service_for_config_dir(&dir),
            None => SERVICE.to_string(),
        };
        Ok(Self {
            service,
            account: user_name()?,
            file: FileStore::new(claude::credentials_path()?),
        })
    }
}

/// Dienstname für einen eigenen Konfigurationsordner.
fn service_for_config_dir(dir: &str) -> String {
    let digest = Sha256::digest(dir.as_bytes());
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{SERVICE}-{hex}")
}

/// Der Login-Name, wie ihn Claude Code verwendet (`os.userInfo().username`).
/// Aus dem Finder gestartet fehlt `USER` manchmal, dann fragt `id -un`.
fn user_name() -> Result<String> {
    if let Some(user) = std::env::var("USER").ok().filter(|u| !u.is_empty()) {
        return Ok(user);
    }
    let out = Command::new("/usr/bin/id")
        .arg("-un")
        .output()
        .with_context(|| tr!("Benutzername nicht ermittelbar", "Cannot determine the user name"))?;
    let user = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || user.is_empty() {
        bail!(tr!("Benutzername nicht ermittelbar", "Cannot determine the user name"));
    }
    Ok(user)
}

impl CredentialStore for KeychainStore {
    fn read(&self) -> Result<Option<String>> {
        let out = Command::new(SECURITY)
            .args(["find-generic-password", "-s", &self.service, "-a", &self.account, "-w"])
            .output()
            .with_context(|| tr!("security konnte nicht gestartet werden", "Cannot run security"))?;
        if out.status.success() {
            let text = String::from_utf8(out.stdout)
                .with_context(|| tr!("Schlüsselbund-Eintrag ist kein UTF-8", "Keychain item is not UTF-8"))?;
            return Ok(Some(decode_password(text.trim_end_matches('\n'))));
        }
        if out.status.code() == Some(NOT_FOUND) {
            return self.file.read();
        }
        let detail = String::from_utf8_lossy(&out.stderr);
        let detail = detail.trim();
        bail!(tr!("Schlüsselbund nicht lesbar: {detail}", "Cannot read the keychain: {detail}"))
    }

    fn write(&self, credentials: &str) -> Result<()> {
        // Als Hex wie Claude Code selbst: so kommen Sonderzeichen unbeschadet an.
        let hex: String = credentials.bytes().map(|b| format!("{b:02x}")).collect();
        let out = Command::new(SECURITY)
            .args(["add-generic-password", "-U", "-a", &self.account, "-s", &self.service, "-X", &hex])
            .output()
            .with_context(|| tr!("security konnte nicht gestartet werden", "Cannot run security"))?;
        if !out.status.success() {
            let detail = String::from_utf8_lossy(&out.stderr);
            let detail = detail.trim();
            bail!(tr!("Schlüsselbund nicht beschreibbar: {detail}", "Cannot write the keychain: {detail}"));
        }
        // Liegt zusätzlich die Datei da, auch sie neu schreiben: ihre neue
        // Änderungszeit bringt ein laufendes Claude Code zum Neuladen, und ein
        // Rückfall auf die Datei fände sonst einen alten Account.
        if self.file.exists() {
            self.file.write(credentials)?;
        }
        Ok(())
    }
}

/// `security -w` gibt ein Passwort, das nicht druckbar ist, als Hex aus (ohne
/// Präfix). JSON beginnt immer mit `{`; ein reiner Hex-Text wird dekodiert.
fn decode_password(text: &str) -> String {
    let looks_hex = !text.is_empty()
        && text.len() % 2 == 0
        && !text.starts_with('{')
        && text.bytes().all(|b| b.is_ascii_hexdigit());
    if !looks_hex {
        return text.to_string();
    }
    let bytes: Option<Vec<u8>> = (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect();
    match bytes.and_then(|bytes| String::from_utf8(bytes).ok()) {
        Some(decoded) if decoded.trim_start().starts_with('{') => decoded,
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_name_with_config_dir() {
        let name = service_for_config_dir("/Users/test/.claude-work");
        assert!(name.starts_with("Claude Code-credentials-"));
        assert_eq!(name.len(), SERVICE.len() + 9);
    }

    #[test]
    fn decodes_hex_passwords() {
        assert_eq!(decode_password("{\"a\":1}"), "{\"a\":1}");
        let hex: String = "{\"ä\":1}".bytes().map(|b| format!("{b:02x}")).collect();
        assert_eq!(decode_password(&hex), "{\"ä\":1}");
        assert_eq!(decode_password("abcd"), "abcd");
    }
}
