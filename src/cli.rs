//! Kommandozeile, z. B. für Skripte oder ein Tastenkürzel.
//!
//! Der Release-Build hat kein Konsolenfenster: Ausgaben erscheinen dort nur,
//! wenn sie umgeleitet werden (`| more`). Der Exit-Code stimmt immer.

use anyhow::{Context, Result, bail};

use crate::login::PendingLogin;
use crate::slots::{self, SLOT_COUNT, SlotStore};
use crate::usage;

const HELP: &str = "\
claude-slots              Oberfläche starten
claude-slots --status     Slots und aktuellen Login anzeigen
claude-slots --capture N  aktuellen Login in Slot N speichern
claude-slots --switch N   zu Slot N wechseln
claude-slots --login N    mit einem Account einloggen und in Slot N speichern
claude-slots --usage      Verbrauch des aktuellen Logins abfragen";

pub fn run(args: &[String]) -> i32 {
    match dispatch(args) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("Fehler: {err:#}");
            1
        }
    }
}

fn dispatch(args: &[String]) -> Result<()> {
    let store = SlotStore::open()?;
    match args {
        [cmd] if cmd == "--status" => status(&store),
        [cmd, n] if cmd == "--capture" => {
            let id = slots::capture(&store, slot_number(n)?)?;
            println!("{} in Slot {n} gespeichert", id.email);
            Ok(())
        }
        [cmd, n] if cmd == "--switch" => {
            let id = slots::switch_to(&store, slot_number(n)?)?;
            println!("Gewechselt zu Slot {n} ({})", id.email);
            Ok(())
        }
        [cmd, n] if cmd == "--login" => login(&store, slot_number(n)?),
        [cmd] if cmd == "--usage" => print_usage(),
        _ => {
            println!("{HELP}");
            Ok(())
        }
    }
}

fn slot_number(text: &str) -> Result<usize> {
    let n: usize = text.parse().context("Slot-Nummer erwartet")?;
    if !(1..=SLOT_COUNT).contains(&n) {
        bail!("Slot muss zwischen 1 und {SLOT_COUNT} liegen");
    }
    Ok(n)
}

fn status(store: &SlotStore) -> Result<()> {
    let live = slots::read_live()?;
    match &live {
        Some(live) => println!("Claude Code: {}", live.identity.email),
        None => println!("Claude Code: nicht eingeloggt"),
    }
    for n in 1..=SLOT_COUNT {
        let id = store.load(n)?.and_then(|slot| slot.identity());
        let active = matches!((&id, &live), (Some(id), Some(live)) if id.same_account(&live.identity));
        match id {
            Some(id) => println!("Slot {n}: {}{}", id.email, if active { "  [aktiv]" } else { "" }),
            None => println!("Slot {n}: leer"),
        }
    }
    println!("Slot-Ordner: {}", store.dir().display());
    Ok(())
}

fn login(store: &SlotStore, n: usize) -> Result<()> {
    let login = PendingLogin::start()?;
    println!("Diesen Link öffnen und mit dem Account für Slot {n} anmelden:\n\n{}\n", login.url);
    eprint!("Code von der Seite einfügen: ");
    let mut code = String::new();
    std::io::stdin().read_line(&mut code)?;
    let slot = login.finish(&code)?;
    let old = store.load(n)?.and_then(|slot| slot.identity());
    if let (Some(old), Some(new)) = (&old, &slot.identity()) {
        if !old.same_account(new) {
            bail!("In Slot {n} liegt {}; überschreiben nur über die Oberfläche", old.email);
        }
    }
    let (id, live) = slots::store_login(store, n, slot)?;
    println!("{} in Slot {n} eingeloggt", id.email);
    if live {
        println!("Claude Code nutzt den neuen Login ebenfalls");
    }
    Ok(())
}

fn print_usage() -> Result<()> {
    let live = slots::read_live()?.context("Claude Code ist nicht eingeloggt")?;
    let token = usage::token_of(&live.credentials).context("Kein Access-Token")?;
    if token.expired() {
        bail!("Access-Token abgelaufen, Claude Code erneuert es beim nächsten Prompt");
    }
    let usage = usage::fetch(&token.value).map_err(|e| anyhow::anyhow!("{e}"))?;
    let rows = [("5 Std.", &usage.five_hour), ("7 Tage", &usage.seven_day)];
    for (label, window) in rows {
        if let Some(window) = window {
            println!("{label}: {:.0} % (Reset {})", window.pct, window.resets_at.as_deref().unwrap_or("?"));
        }
    }
    for model in &usage.models {
        println!(
            "{}: {:.0} % (Reset {})",
            model.name,
            model.window.pct,
            model.window.resets_at.as_deref().unwrap_or("?")
        );
    }
    Ok(())
}
