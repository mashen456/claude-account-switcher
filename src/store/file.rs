use std::path::PathBuf;

use anyhow::Result;

use super::CredentialStore;
use crate::fsx;

/// Das Token als Klartext-JSON-Datei. Claude Code lädt es neu, sobald sich die
/// Änderungszeit der Datei ändert, deshalb ist kein Neustart nötig.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn exists(&self) -> bool {
        self.path.is_file()
    }
}

impl CredentialStore for FileStore {
    fn read(&self) -> Result<Option<String>> {
        fsx::read_opt(&self.path)
    }

    fn write(&self, credentials: &str) -> Result<()> {
        fsx::write_atomic(&self.path, credentials.as_bytes())
    }
}
