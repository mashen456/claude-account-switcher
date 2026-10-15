//! Wo das aktive Token von Claude Code liegt, je nach Betriebssystem.
//!
//! Windows und Linux: die Datei `.credentials.json`.
//! macOS: der Schlüsselbund, die Datei nur als Rückfall.

mod file;
#[cfg(target_os = "macos")]
mod keychain;

use anyhow::Result;

pub use file::FileStore;

pub trait CredentialStore {
    /// Das Token-Objekt als Text, `None` wenn keins da ist.
    fn read(&self) -> Result<Option<String>>;
    fn write(&self, credentials: &str) -> Result<()>;
}

#[cfg(not(target_os = "macos"))]
pub fn active() -> Result<Box<dyn CredentialStore>> {
    Ok(Box::new(FileStore::new(crate::claude::credentials_path()?)))
}

#[cfg(target_os = "macos")]
pub fn active() -> Result<Box<dyn CredentialStore>> {
    Ok(Box::new(keychain::KeychainStore::new()?))
}
