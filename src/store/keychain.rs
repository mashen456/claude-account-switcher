//! macOS: Claude Code speichert das Token im Schlüsselbund (Dienst
//! "Claude Code-credentials", Konto = Benutzername). Die Datei
//! `~/.claude/.credentials.json` gibt es dort nur als Rückfall.
//!
//! UNGETESTET: Dieser Teil ist noch auf keinem Mac gelaufen.

use std::process::Command;

use anyhow::{Context, Result, bail};

use super::{CredentialStore, FileStore};
use crate::claude;

const SERVICE: &str = "Claude Code-credentials";
const SECURITY: &str = "/usr/bin/security";
/// Exit-Code von `security`, wenn der Eintrag nicht existiert.
const NOT_FOUND: i32 = 44;

pub struct KeychainStore {
    account: String,
    file: FileStore,
}

impl KeychainStore {
    pub fn new() -> Result<Self> {
        if std::env::var_os("CLAUDE_CONFIG_DIR").is_some_and(|v| !v.is_empty()) {
            // Claude Code hängt dann einen Hash des Pfads an den Dienstnamen.
            bail!(tr!(
                "CLAUDE_CONFIG_DIR wird auf dem Mac noch nicht unterstützt",
                "CLAUDE_CONFIG_DIR is not supported on macOS yet"
            ));
        }
        let account = std::env::var("USER")
            .ok()
            .filter(|user| !user.is_empty())
            .with_context(|| tr!("Umgebungsvariable USER fehlt", "Environment variable USER is missing"))?;
        Ok(Self {
            account,
            file: FileStore::new(claude::credentials_path()?),
        })
    }
}

impl CredentialStore for KeychainStore {
    fn read(&self) -> Result<Option<String>> {
        let out = Command::new(SECURITY)
            .args(["find-generic-password", "-s", SERVICE, "-a", &self.account, "-w"])
            .output()
            .with_context(|| tr!("security konnte nicht gestartet werden", "Cannot run security"))?;
        if out.status.success() {
            let text = String::from_utf8(out.stdout)
                .with_context(|| tr!("Schlüsselbund-Eintrag ist kein UTF-8", "Keychain item is not UTF-8"))?;
            return Ok(Some(text.trim_end_matches('\n').to_string()));
        }
        if out.status.code() == Some(NOT_FOUND) {
            return self.file.read();
        }
        let detail = String::from_utf8_lossy(&out.stderr);
        let detail = detail.trim();
        bail!(tr!("Schlüsselbund nicht lesbar: {detail}", "Cannot read the keychain: {detail}"))
    }

    fn write(&self, credentials: &str) -> Result<()> {
        let hex: String = credentials.bytes().map(|b| format!("{b:02x}")).collect();
        let out = Command::new(SECURITY)
            .args(["add-generic-password", "-U", "-a", &self.account, "-s", SERVICE, "-X", &hex])
            .output()
            .with_context(|| tr!("security konnte nicht gestartet werden", "Cannot run security"))?;
        if !out.status.success() {
            let detail = String::from_utf8_lossy(&out.stderr);
            let detail = detail.trim();
            bail!(tr!("Schlüsselbund nicht beschreibbar: {detail}", "Cannot write the keychain: {detail}"));
        }
        // Liegt zusätzlich die Datei da, auch sie neu schreiben: ihre neue
        // Änderungszeit bringt ein laufendes Claude Code zum Neuladen.
        if self.file.exists() {
            self.file.write(credentials)?;
        }
        Ok(())
    }
}
