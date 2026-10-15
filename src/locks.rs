//! Die Sperren, die Claude Code selbst benutzt (npm-Paket proper-lockfile).
//!
//! Eine Sperre ist ein Ordner: wer ihn per mkdir anlegen kann, hält sie.
//! Claude Code nimmt beim Erneuern des Tokens `<Konfigurationsordner>/.oauth_refresh.lock`
//! und `~/.claude.lock`, beim Schreiben der globalen Konfiguration
//! `~/.claude.json.lock`. Halten wir dieselben Sperren, während wir die Dateien
//! tauschen, kann ein laufendes Claude Code nicht mittendrin ein erneuertes
//! Token des alten Accounts zurückschreiben.

use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::claude;

/// Token-Sperren gelten bei Claude Code erst nach 60 s als verwaist,
/// die Konfigurationssperre nach 10 s. Lebende Halter erneuern sie alle 5 s.
const CREDENTIALS_STALE: Duration = Duration::from_secs(60);
const CONFIG_STALE: Duration = Duration::from_secs(10);
const TIMEOUT: Duration = Duration::from_secs(5);

struct DirLock {
    path: PathBuf,
}

impl DirLock {
    fn acquire(path: PathBuf, stale_after: Duration) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let start = Instant::now();
        loop {
            match fs::create_dir(&path) {
                Ok(()) => return Ok(DirLock { path }),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("{} nicht anlegbar", path.display()));
                }
            }
            let stale = fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > stale_after);
            if stale {
                let _ = fs::remove_dir(&path);
                continue;
            }
            if start.elapsed() > TIMEOUT {
                bail!(
                    "{} ist belegt, Claude Code erneuert vermutlich gerade das Token. \
                     Gleich nochmal versuchen.",
                    path.display()
                );
            }
            sleep(Duration::from_millis(250));
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

/// Hält alle drei Sperren, solange der Wert lebt.
pub struct ClaudeLocks {
    _held: Vec<DirLock>,
}

/// Nimmt die Sperren in derselben Reihenfolge wie Claude Code.
pub fn claude_locks() -> Result<ClaudeLocks> {
    let config_home = claude::config_home()?;
    let global_config = claude::global_config_path()?;
    let held = vec![
        DirLock::acquire(config_home.join(".oauth_refresh.lock"), CREDENTIALS_STALE)?,
        DirLock::acquire(with_suffix(&config_home, ".lock"), CREDENTIALS_STALE)?,
        DirLock::acquire(with_suffix(&global_config, ".lock"), CONFIG_STALE)?,
    ];
    Ok(ClaudeLocks { _held: held })
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}
