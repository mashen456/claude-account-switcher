//! Datei-Helfer: optional lesen und atomar schreiben.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result};

/// Liest eine Datei, `None` wenn es sie nicht gibt. Eine UTF-8-BOM (von
/// Editoren wie Notepad) wird entfernt, sonst lehnt serde_json die Datei ab.
pub fn read_opt(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(match text.strip_prefix('\u{feff}') {
            Some(rest) => rest.to_string(),
            None => text,
        })),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("{} ist nicht lesbar", path.display())),
    }
}

/// Schreibt erst eine Temp-Datei im selben Ordner und ersetzt dann das Ziel.
/// So liest Claude Code nie eine halb geschriebene Datei.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("Pfad ohne Ordner")?;
    fs::create_dir_all(dir).with_context(|| format!("{} nicht anlegbar", dir.display()))?;
    let name = path.file_name().context("Pfad ohne Dateinamen")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    {
        let mut file = fs::File::create(&tmp)
            .with_context(|| format!("{} nicht anlegbar", tmp.display()))?;
        file.write_all(data)?;
        file.sync_all()?;
    }
    restrict_permissions(&tmp);

    // Unter Windows scheitert das Ersetzen, solange ein anderer Prozess
    // (z. B. Claude Code) die Datei gerade offen hat. Kurz erneut versuchen.
    let mut last_err = None;
    for _ in 0..20 {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                last_err = Some(e);
                sleep(Duration::from_millis(50));
            }
            Err(e) => {
                last_err = Some(e);
                break;
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    let err = last_err.expect("mindestens ein Versuch");
    Err(err).with_context(|| format!("{} konnte nicht ersetzt werden", path.display()))
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}
