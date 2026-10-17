//! Zwischenablage: Login-Link kopieren, Code einfügen.

use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result};
use arboard::{Clipboard, Error};

pub fn set(text: &str) -> Result<()> {
    retry(|clipboard| clipboard.set_text(text))
        .with_context(|| tr!("Zwischenablage nicht beschreibbar", "Cannot write to the clipboard"))
}

pub fn get() -> Result<String> {
    retry(|clipboard| clipboard.get_text()).map_err(|err| match err {
        Error::ContentNotAvailable => {
            anyhow::anyhow!(tr!("Die Zwischenablage enthält keinen Text", "The clipboard holds no text"))
        }
        err => anyhow::Error::new(err)
            .context(tr!("Zwischenablage nicht lesbar", "Cannot read the clipboard")),
    })
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
