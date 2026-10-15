//! Zwischenablage. Unter Windows wird ein exportierter Login aus dem Verlauf
//! (Win+V) und der Cloud-Synchronisierung herausgehalten.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result};
use arboard::{Clipboard, Error};

pub fn set_secret(text: &str) -> Result<()> {
    retry(|clipboard| {
        #[cfg(windows)]
        {
            use arboard::SetExtWindows;
            clipboard.set().exclude_from_monitoring().text(text)
        }
        #[cfg(not(windows))]
        {
            clipboard.set_text(text)
        }
    })
    .context("Zwischenablage nicht beschreibbar")
}

pub fn get() -> Result<String> {
    retry(|clipboard| clipboard.get_text()).map_err(|err| match err {
        Error::ContentNotAvailable => anyhow::anyhow!("Die Zwischenablage enthält keinen Text"),
        err => anyhow::Error::new(err).context("Zwischenablage nicht lesbar"),
    })
}

/// Leert die Zwischenablage, wenn sie noch `text` enthält.
pub fn clear_if(text: &str) {
    let _ = retry(|clipboard| {
        if clipboard.get_text().is_ok_and(|current| current == text) {
            clipboard.clear()?;
        }
        Ok(())
    });
}

/// Andere Programme (z. B. der Zwischenablage-Dienst von Windows) halten die
/// Zwischenablage nach jeder Änderung kurz offen. arboard versucht es selbst
/// nur etwa 25 ms lang, deshalb hier länger.
fn retry<T>(mut op: impl FnMut(&mut Clipboard) -> Result<T, Error>) -> Result<T, Error> {
    let mut attempts = 10;
    loop {
        let result = Clipboard::new().and_then(|mut clipboard| op(&mut clipboard));
        match result {
            Err(Error::ClipboardOccupied) if attempts > 1 => {
                attempts -= 1;
                sleep(Duration::from_millis(50));
            }
            result => return result,
        }
    }
}
