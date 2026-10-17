//! Kommandozeile, z. B. für Skripte oder ein Tastenkürzel.
//!
//! Der Release-Build hat kein Konsolenfenster: Ausgaben erscheinen dort nur,
//! wenn sie umgeleitet werden (`| more`). Der Exit-Code stimmt immer.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::i18n::{self, Lang};
use crate::login::PendingLogin;
use crate::slots::{self, SlotStore};
use crate::usage::{self, UsageCache};

fn help() -> String {
    tr!(
        "\
claude-slots                  Oberfläche starten
claude-slots --status         Slots und aktuellen Login anzeigen
claude-slots --capture N      aktuellen Login in Slot N speichern
claude-slots --switch N       zu Slot N wechseln
claude-slots --login N        mit einem Account einloggen und in Slot N speichern
claude-slots --add-slot       leeren Slot anhängen
claude-slots --remove-slot N  Slot N entfernen
claude-slots --lang de|en     Sprache einstellen
claude-slots --usage          Verbrauch des aktuellen Logins abfragen",
        "\
claude-slots                  start the window
claude-slots --status         show slots and the current login
claude-slots --capture N      store the current login in slot N
claude-slots --switch N       switch to slot N
claude-slots --login N        log in to an account and store it in slot N
claude-slots --add-slot       append an empty slot
claude-slots --remove-slot N  remove slot N
claude-slots --lang de|en     set the language
claude-slots --usage          query usage of the current login"
    )
}

pub fn run(args: &[String]) -> i32 {
    match dispatch(args) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("{} {err:#}", tr!("Fehler:", "Error:"));
            1
        }
    }
}

fn dispatch(args: &[String]) -> Result<()> {
    let store = SlotStore::open()?;
    match args {
        [cmd] if cmd == "--status" => status(&store),
        [cmd, n] if cmd == "--capture" => {
            let n = slot_number(&store, n)?;
            let email = slots::capture(&store, n)?.email;
            println!("{}", tr!("{email} in Slot {n} gespeichert", "{email} stored in slot {n}"));
            Ok(())
        }
        [cmd, n] if cmd == "--switch" => {
            let n = slot_number(&store, n)?;
            let email = slots::switch_to(&store, n)?.email;
            println!("{}", tr!("Gewechselt zu Slot {n} ({email})", "Switched to slot {n} ({email})"));
            Ok(())
        }
        [cmd, n] if cmd == "--login" => login(&store, slot_number(&store, n)?),
        [cmd] if cmd == "--add-slot" => {
            let n = store.add_slot()?;
            println!("{}", tr!("Slot {n} angelegt", "Slot {n} added"));
            Ok(())
        }
        [cmd, n] if cmd == "--remove-slot" => {
            let n = slot_number(&store, n)?;
            store.remove_slot(n)?;
            println!("{}", tr!("Slot {n} entfernt", "Slot {n} removed"));
            Ok(())
        }
        [cmd, lang] if cmd == "--lang" => {
            let lang = match lang.as_str() {
                "de" => Lang::De,
                "en" => Lang::En,
                _ => bail!(tr!("Sprache: de oder en", "Language: de or en")),
            };
            store.set_language(lang)?;
            i18n::set(lang);
            println!("{}", tr!("Sprache: Deutsch", "Language: English"));
            Ok(())
        }
        [cmd] if cmd == "--usage" => print_usage(&store),
        _ => {
            println!("{}", help());
            Ok(())
        }
    }
}

fn slot_number(store: &SlotStore, text: &str) -> Result<usize> {
    let n: usize = text
        .parse()
        .with_context(|| tr!("Slot-Nummer erwartet", "Slot number expected"))?;
    let count = store.count();
    if !(1..=count).contains(&n) {
        bail!(tr!("Slot muss zwischen 1 und {count} liegen", "Slot must be between 1 and {count}"));
    }
    Ok(n)
}

fn status(store: &SlotStore) -> Result<()> {
    let live = slots::read_live()?;
    match &live {
        Some(live) => println!("Claude Code: {}", live.identity.email),
        None => println!("Claude Code: {}", tr!("nicht eingeloggt", "not logged in")),
    }
    for n in 1..=store.count() {
        let id = store.load(n)?.and_then(|slot| slot.identity());
        let active = matches!((&id, &live), (Some(id), Some(live)) if id.same_account(&live.identity));
        match id {
            Some(id) if active => println!("Slot {n}: {}  [{}]", id.email, tr!("aktiv", "active")),
            Some(id) => println!("Slot {n}: {}", id.email),
            None => println!("Slot {n}: {}", tr!("leer", "empty")),
        }
    }
    let dir = store.dir().display();
    println!("{}", tr!("Slot-Ordner: {dir}", "Slot folder: {dir}"));
    Ok(())
}

fn login(store: &SlotStore, n: usize) -> Result<()> {
    let login = PendingLogin::start()?;
    let url = &login.url;
    println!(
        "{}",
        tr!(
            "Diesen Link öffnen und mit dem Account für Slot {n} anmelden:\n\n{url}\n",
            "Open this link and sign in with the account for slot {n}:\n\n{url}\n"
        )
    );
    eprint!("{}", tr!("Code von der Seite einfügen: ", "Paste the code from the page: "));
    let mut code = String::new();
    std::io::stdin().read_line(&mut code)?;
    let slot = login.finish(&code)?;
    let old = store.load(n)?.and_then(|slot| slot.identity());
    if let (Some(old), Some(new)) = (&old, &slot.identity()) {
        if !old.same_account(new) {
            let email = &old.email;
            bail!(tr!(
                "In Slot {n} liegt {email}; überschreiben nur über die Oberfläche",
                "Slot {n} holds {email}; overwrite it in the window"
            ));
        }
    }
    let (id, live) = slots::store_login(store, n, slot)?;
    let email = id.email;
    println!("{}", tr!("{email} in Slot {n} eingeloggt", "{email} logged in to slot {n}"));
    if live {
        println!("{}", tr!("Claude Code nutzt den neuen Login ebenfalls", "Claude Code uses the new login too"));
    }
    Ok(())
}

fn print_usage(store: &SlotStore) -> Result<()> {
    let live = slots::read_live()?
        .with_context(|| tr!("Claude Code ist nicht eingeloggt", "Claude Code is not logged in"))?;
    let token = usage::token_of(&live.credentials)
        .with_context(|| tr!("Kein Access-Token", "No access token"))?;
    if token.expired() {
        bail!(tr!(
            "Access-Token abgelaufen, Claude Code erneuert es beim nächsten Prompt",
            "Access token expired, Claude Code renews it on the next prompt"
        ));
    }
    // Dieselben Abstände wie das Fenster, gemeinsam über die Cache-Datei.
    let key = live.identity.key();
    let mut cache = UsageCache::load(store.dir());
    if cache.claim(&key, Duration::ZERO) {
        let result = usage::fetch(&token.value);
        cache.record(&key, &result);
        result.map_err(|e| anyhow!("{e}"))?;
    } else {
        eprintln!(
            "{}",
            tr!(
                "Letzte Abfrage jünger als 3 Minuten oder pausiert, zeige gespeicherten Stand",
                "Last query less than 3 minutes ago or paused, showing stored values"
            )
        );
    }
    let entry = cache.get(&key);
    let usage = entry
        .and_then(|e| e.usage.clone())
        .with_context(|| tr!("Noch kein Verbrauch bekannt", "No usage known yet"))?;
    if let Some(at) = entry.and_then(|e| e.fetched_at.clone()) {
        println!("{}", tr!("Stand: {at}", "As of: {at}"));
    }
    let rows = [
        (tr!("5 Std.", "5 h"), &usage.five_hour),
        (tr!("7 Tage", "7 days"), &usage.seven_day),
    ];
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
