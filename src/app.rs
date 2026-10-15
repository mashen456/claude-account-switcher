//! Die Oberfläche: eine Karte pro Slot mit Verbrauch und Knöpfen.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use chrono::{Datelike, Weekday};
use eframe::egui::{self, Color32, RichText};

use crate::claude::{self, Identity};
use crate::clipboard;
use crate::login::PendingLogin;
use crate::slots::{self, Live, SLOT_COUNT, Slot, SlotStore};
use crate::usage::{self, FetchError, Usage, UsageCache, Window};

/// Wie oft die Dateien neu gelesen werden (lokal, billig).
const RELOAD_EVERY: Duration = Duration::from_secs(3);
/// Wie oft der Verbrauch abgefragt wird.
const USAGE_EVERY: Duration = Duration::from_secs(5 * 60);
/// Mindestabstand für "Aktualisieren", damit die Schnittstelle nicht mit 429 antwortet.
const MIN_MANUAL_REFRESH: Duration = Duration::from_secs(20);

const GREEN: Color32 = Color32::from_rgb(90, 180, 100);
const AMBER: Color32 = Color32::from_rgb(220, 160, 50);
const RED: Color32 = Color32::from_rgb(210, 70, 60);
const GRAY: Color32 = Color32::from_rgb(150, 150, 150);

/// Ergebnisse aus Hintergrund-Threads (Netzwerk).
enum Msg {
    Usage {
        key: String,
        result: Result<Usage, FetchError>,
    },
    Login {
        slot: usize,
        result: anyhow::Result<Slot>,
    },
}

/// Das Fenster "Neu einloggen".
struct LoginDialog {
    slot: usize,
    login: PendingLogin,
    code: String,
    busy: bool,
    copied: bool,
    error: Option<String>,
}

#[derive(Default)]
struct UsageState {
    loading: bool,
    note: Option<String>,
}

struct Status {
    text: String,
    error: bool,
}

impl Status {
    fn ok(text: String) -> Self {
        Self { text, error: false }
    }

    fn error(err: anyhow::Error) -> Self {
        Self {
            text: format!("{err:#}"),
            error: true,
        }
    }
}

enum Action {
    Switch(usize),
    Capture(usize),
    Login(usize),
    /// Die Rückfrage zum Überschreiben wurde bestätigt.
    Confirm,
    Refresh,
}

/// Wartet auf Bestätigung, weil ein Slot mit einem anderen Account überschrieben würde.
enum Pending {
    Capture(usize),
    Login { slot: usize, data: Slot },
}

pub struct App {
    store: Result<SlotStore, String>,
    slots: Vec<Option<Slot>>,
    live: Option<Live>,
    live_error: Option<String>,
    cache: Option<UsageCache>,
    usage: HashMap<String, UsageState>,
    status: Option<Status>,
    pending: Option<Pending>,
    login: Option<LoginDialog>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    last_reload: Option<Instant>,
    last_usage: Option<Instant>,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let store = SlotStore::open().map_err(|e| format!("{e:#}"));
        let cache = store.as_ref().ok().map(|s| UsageCache::load(s.dir()));
        let (tx, rx) = channel();
        Self {
            store,
            slots: vec![None; SLOT_COUNT],
            live: None,
            live_error: None,
            cache,
            usage: HashMap::new(),
            status: None,
            pending: None,
            login: None,
            tx,
            rx,
            last_reload: None,
            last_usage: None,
        }
    }

    fn store(&self) -> anyhow::Result<&SlotStore> {
        self.store.as_ref().map_err(|e| anyhow!("{e}"))
    }

    fn reload(&mut self) {
        self.last_reload = Some(Instant::now());
        let Ok(store) = &self.store else { return };
        for n in 1..=SLOT_COUNT {
            match store.load(n) {
                Ok(slot) => self.slots[n - 1] = slot,
                Err(err) => {
                    self.slots[n - 1] = None;
                    self.status = Some(Status::error(err));
                }
            }
        }
        match slots::read_live() {
            Ok(live) => {
                self.live = live;
                self.live_error = None;
            }
            Err(err) => {
                self.live = None;
                self.live_error = Some(format!("{err:#}"));
            }
        }
    }

    fn active_slot(&self) -> Option<usize> {
        let live = self.live.as_ref()?;
        (1..=SLOT_COUNT).find(|&n| {
            self.slots[n - 1]
                .as_ref()
                .and_then(Slot::identity)
                .is_some_and(|id| id.same_account(&live.identity))
        })
    }

    fn poll_usage(&mut self, ctx: &egui::Context) {
        self.last_usage = Some(Instant::now());
        // Für Tests und Screenshots mit erfundenen Tokens.
        if std::env::var_os("CLAUDE_SLOTS_NO_FETCH").is_some_and(|v| !v.is_empty()) {
            return;
        }
        let active = self.active_slot();
        for n in 1..=SLOT_COUNT {
            let Some(slot) = &self.slots[n - 1] else { continue };
            let Some(id) = slot.identity() else { continue };
            let is_active = active == Some(n);
            // Für den aktiven Account das Token, das Claude Code gerade benutzt.
            // Claude Code erneuert es selbst, es ist also immer frisch.
            let credentials = if is_active {
                self.live.as_ref().map(|live| &live.credentials)
            } else {
                Some(&slot.credentials)
            };
            let token = credentials.and_then(usage::token_of);
            let state = self.usage.entry(id.key()).or_default();
            if state.loading {
                continue;
            }
            let Some(token) = token else {
                state.note = Some("Kein Access-Token vorhanden".into());
                continue;
            };
            if token.expired() {
                state.note = Some(if is_active {
                    "Token abgelaufen, Claude Code erneuert es beim nächsten Prompt".into()
                } else {
                    "Token abgelaufen, angezeigt wird der letzte bekannte Stand".into()
                });
                continue;
            }
            state.loading = true;
            let (tx, ctx, key) = (self.tx.clone(), ctx.clone(), id.key());
            std::thread::spawn(move || {
                let result = usage::fetch(&token.value);
                let _ = tx.send(Msg::Usage { key, result });
                ctx.request_repaint();
            });
        }
    }

    fn receive(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Usage { key, result } => {
                    let state = self.usage.entry(key.clone()).or_default();
                    state.loading = false;
                    match result {
                        Ok(usage) => {
                            state.note = None;
                            if let Some(cache) = &mut self.cache {
                                cache.put(key, usage);
                            }
                        }
                        Err(err) => state.note = Some(err.to_string()),
                    }
                }
                Msg::Login { slot, result } => self.login_finished(slot, result),
            }
        }
    }

    fn login_finished(&mut self, slot: usize, result: anyhow::Result<Slot>) {
        // Fenster inzwischen geschlossen oder ein neuer Versuch: Ergebnis verwerfen.
        let Some(dialog) = self.login.as_mut().filter(|d| d.slot == slot && d.busy) else {
            return;
        };
        dialog.busy = false;
        match result {
            Err(err) => dialog.error = Some(format!("{err:#}")),
            Ok(data) => {
                self.login = None;
                if self.overwrites_other(slot, data.identity().as_ref()) {
                    self.pending = Some(Pending::Login { slot, data });
                } else {
                    self.status = Some(self.store_login(slot, data));
                    self.reload();
                    self.last_usage = None;
                }
            }
        }
    }

    /// Würde Slot `n` mit einem anderen Account überschrieben? Dann erst nachfragen.
    fn overwrites_other(&self, n: usize, new: Option<&Identity>) -> bool {
        let old = self.slots[n - 1].as_ref().and_then(Slot::identity);
        match (old, new) {
            (Some(old), Some(new)) => !old.same_account(new),
            _ => false,
        }
    }

    fn capture(&self, n: usize) -> Status {
        match self.store().and_then(|store| slots::capture(store, n)) {
            Ok(id) => Status::ok(format!("{} in Slot {n} gespeichert.", id.email)),
            Err(err) => Status::error(err),
        }
    }

    fn store_login(&self, n: usize, data: Slot) -> Status {
        match self.store().and_then(|store| slots::store_login(store, n, data)) {
            Ok((id, false)) => Status::ok(format!("{} in Slot {n} eingeloggt.", id.email)),
            Ok((id, true)) => Status::ok(format!(
                "{} in Slot {n} eingeloggt. Claude Code nutzt den neuen Login ebenfalls.",
                id.email
            )),
            Err(err) => Status::error(err),
        }
    }

    fn run(&mut self, action: Action) {
        let mut fetch_usage = true;
        match action {
            Action::Refresh => {
                fetch_usage = self
                    .last_usage
                    .is_none_or(|t| t.elapsed() >= MIN_MANUAL_REFRESH);
            }
            Action::Switch(n) => {
                let result = self.store().and_then(|store| slots::switch_to(store, n));
                self.status = Some(match result {
                    Ok(id) => Status::ok(format!(
                        "Gewechselt zu Slot {n} ({}). Claude Code übernimmt den Account \
                         beim nächsten Prompt.",
                        id.email
                    )),
                    Err(err) => Status::error(err),
                });
            }
            Action::Capture(n) => {
                if self.overwrites_other(n, self.live.as_ref().map(|live| &live.identity)) {
                    self.pending = Some(Pending::Capture(n));
                    return;
                }
                self.status = Some(self.capture(n));
            }
            Action::Login(n) => {
                match PendingLogin::start() {
                    Ok(login) => {
                        self.login = Some(LoginDialog {
                            slot: n,
                            login,
                            code: String::new(),
                            busy: false,
                            copied: false,
                            error: None,
                        });
                    }
                    Err(err) => self.status = Some(Status::error(err)),
                }
                return;
            }
            Action::Confirm => match self.pending.take() {
                Some(Pending::Capture(n)) => self.status = Some(self.capture(n)),
                Some(Pending::Login { slot, data }) => {
                    self.status = Some(self.store_login(slot, data));
                }
                None => {}
            },
        }
        self.reload();
        if fetch_usage {
            self.last_usage = None;
        }
    }

    fn body(&self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        if let Err(err) = &self.store {
            ui.colored_label(RED, err.as_str());
            return;
        }
        let active = self.active_slot();
        if let Some(err) = &self.live_error {
            ui.colored_label(RED, err.as_str());
        } else if let Some(live) = &self.live {
            if active.is_none() {
                ui.colored_label(
                    AMBER,
                    format!(
                        "Claude Code ist mit {} eingeloggt, der Account liegt in keinem Slot. \
                         Bei einem Slot \"Login übernehmen\" klicken.",
                        live.identity.email
                    ),
                );
            }
        } else {
            ui.colored_label(AMBER, "Claude Code ist nicht eingeloggt.");
        }
        ui.add_space(4.0);
        for n in 1..=SLOT_COUNT {
            self.slot_card(ui, n, active == Some(n), action);
            ui.add_space(8.0);
        }
    }

    fn slot_card(&self, ui: &mut egui::Ui, n: usize, active: bool, action: &mut Option<Action>) {
        let slot = self.slots[n - 1].as_ref();
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.heading(format!("Slot {n}"));
                    if active {
                        ui.label(
                            RichText::new(" AKTIV ")
                                .strong()
                                .color(Color32::BLACK)
                                .background_color(GREEN),
                        );
                    }
                });
                match slot.and_then(|s| s.identity().map(|id| (s, id))) {
                    None => {
                        ui.label(RichText::new("Leer").weak());
                    }
                    Some((slot, id)) => {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&id.email).strong());
                            if let Some(plan) = claude::plan_label(&slot.credentials) {
                                ui.label(RichText::new(plan).weak());
                            }
                        });
                        ui.add_space(4.0);
                        self.usage_rows(ui, n, &id);
                    }
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let can_switch = slot.is_some() && !active;
                    if ui
                        .add_enabled(can_switch, egui::Button::new("Hierher wechseln"))
                        .clicked()
                    {
                        *action = Some(Action::Switch(n));
                    }
                    if ui
                        .button("Login übernehmen")
                        .on_hover_text(
                            "Speichert den Account, mit dem Claude Code gerade eingeloggt ist, \
                             in diesem Slot.",
                        )
                        .clicked()
                    {
                        *action = Some(Action::Capture(n));
                    }
                    if ui
                        .button("Neu einloggen")
                        .on_hover_text(
                            "Mit einem Account einloggen, ohne Claude Code abzumelden. Jeder \
                             Login ist eigenständig, auch auf mehreren PCs gleichzeitig.",
                        )
                        .clicked()
                    {
                        *action = Some(Action::Login(n));
                    }
                });
            });
    }

    fn usage_rows(&self, ui: &mut egui::Ui, n: usize, id: &Identity) {
        let key = id.key();
        let reading = self.cache.as_ref().and_then(|cache| cache.get(&key));
        let state = self.usage.get(&key);
        match reading {
            None => {
                let text = if state.is_some_and(|s| s.loading) {
                    "Verbrauch wird geladen …"
                } else {
                    "Noch kein Verbrauch bekannt"
                };
                ui.label(RichText::new(text).weak());
            }
            Some(reading) => {
                egui::Grid::new(("usage", n))
                    .num_columns(3)
                    .spacing([10.0, 4.0])
                    .show(ui, |ui| {
                        if let Some(window) = &reading.usage.five_hour {
                            window_row(ui, "5 Std.", window);
                        }
                        if let Some(window) = &reading.usage.seven_day {
                            window_row(ui, "7 Tage", window);
                        }
                        for model in &reading.usage.models {
                            window_row(ui, &model.name, &model.window);
                        }
                    });
                if let Some(age) = age_text(&reading.fetched_at) {
                    ui.label(RichText::new(age).small().weak());
                }
            }
        }
        if let Some(note) = state.and_then(|s| s.note.as_deref()) {
            ui.label(RichText::new(note).small().color(AMBER));
        }
    }

    fn footer(&self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        ui.add_space(6.0);
        if let Some(status) = &self.status {
            let color = if status.error { RED } else { GREEN };
            ui.colored_label(color, status.text.as_str());
        }
        ui.horizontal(|ui| {
            if ui.button("Aktualisieren").clicked() {
                *action = Some(Action::Refresh);
            }
            if let Ok(store) = &self.store {
                let dir = store.dir();
                if ui
                    .button("Ordner öffnen")
                    .on_hover_text(dir.display().to_string())
                    .clicked()
                {
                    open_folder(dir);
                }
            }
        });
        ui.label(
            RichText::new(
                "Account einrichten: bei einem Slot \"Neu einloggen\", oder den aktuellen \
                 Login von Claude Code mit \"Login übernehmen\" speichern.",
            )
            .small()
            .weak(),
        );
        ui.add_space(6.0);
    }

    fn login_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.login else { return };
        let (mut submit, mut cancel) = (false, false);
        let response = egui::Modal::new(egui::Id::unique("login")).show(ctx, |ui| {
            ui.set_max_width(400.0);
            ui.heading(format!("Slot {}: einloggen", dialog.slot));
            ui.add_space(6.0);
            ui.label("1. Link öffnen und mit dem Account anmelden, der in diesen Slot soll.");
            ui.label(
                RichText::new(
                    "Ist der Browser schon mit einem anderen Claude-Account angemeldet: \
                     Link kopieren und in einem privaten Fenster öffnen.",
                )
                .small()
                .weak(),
            );
            ui.horizontal(|ui| {
                if ui.button("Im Browser öffnen").clicked() {
                    open_url(&dialog.login.url);
                }
                let label = if dialog.copied { "Kopiert" } else { "Link kopieren" };
                if ui.button(label).clicked() {
                    match clipboard::set(&dialog.login.url) {
                        Ok(()) => dialog.copied = true,
                        Err(err) => dialog.error = Some(format!("{err:#}")),
                    }
                }
            });
            ui.add_space(8.0);
            ui.label("2. Nach der Anmeldung zeigt die Seite einen Code. Hier einfügen:");
            ui.horizontal(|ui| {
                let edit = ui.add_enabled(
                    !dialog.busy,
                    egui::TextEdit::singleline(&mut dialog.code)
                        .hint_text("Code")
                        .desired_width(280.0),
                );
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                if ui.add_enabled(!dialog.busy, egui::Button::new("Einfügen")).clicked() {
                    match clipboard::get() {
                        Ok(text) => dialog.code = text.trim().to_string(),
                        Err(err) => dialog.error = Some(format!("{err:#}")),
                    }
                }
            });
            if let Some(err) = &dialog.error {
                ui.colored_label(RED, err.as_str());
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let ready = !dialog.busy && !dialog.code.trim().is_empty();
                if ui.add_enabled(ready, egui::Button::new("Anmelden")).clicked() {
                    submit = true;
                }
                if ui.add_enabled(!dialog.busy, egui::Button::new("Abbrechen")).clicked() {
                    cancel = true;
                }
                if dialog.busy {
                    ui.spinner();
                    ui.label("Anmeldung läuft …");
                }
            });
        });
        if submit && !dialog.busy && !dialog.code.trim().is_empty() {
            dialog.busy = true;
            dialog.error = None;
            let (tx, ctx) = (self.tx.clone(), ctx.clone());
            let (slot, login, code) = (dialog.slot, dialog.login.clone(), dialog.code.clone());
            std::thread::spawn(move || {
                let result = login.finish(&code);
                let _ = tx.send(Msg::Login { slot, result });
                ctx.request_repaint();
            });
        } else if !dialog.busy && (cancel || response.should_close()) {
            self.login = None;
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context, action: &mut Option<Action>) {
        let (n, new) = match &self.pending {
            None => return,
            Some(Pending::Capture(n)) => (
                *n,
                self.live
                    .as_ref()
                    .map(|live| live.identity.email.clone())
                    .unwrap_or_default(),
            ),
            Some(Pending::Login { slot, data }) => (
                *slot,
                format!(
                    "{} (neu eingeloggt)",
                    data.identity().map(|id| id.email).unwrap_or_default()
                ),
            ),
        };
        let old = self.slots[n - 1]
            .as_ref()
            .and_then(Slot::identity)
            .map(|id| id.email)
            .unwrap_or_default();
        let (mut confirmed, mut cancelled) = (false, false);
        let response = egui::Modal::new(egui::Id::unique("confirm_overwrite")).show(ctx, |ui| {
            ui.heading(format!("Slot {n} überschreiben?"));
            ui.label(format!("In Slot {n} liegt {old}. Mit {new} überschreiben?"));
            ui.label(RichText::new(format!("Der gespeicherte Login von {old} geht dabei verloren.")).weak());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                confirmed = ui.button("Überschreiben").clicked();
                cancelled = ui.button("Abbrechen").clicked();
            });
        });
        if confirmed {
            *action = Some(Action::Confirm);
        } else if cancelled || response.should_close() {
            self.pending = None;
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive();
        if self.last_reload.is_none_or(|t| t.elapsed() >= RELOAD_EVERY) {
            self.reload();
        }
        if self.last_usage.is_none_or(|t| t.elapsed() >= USAGE_EVERY) {
            self.poll_usage(ctx);
        }
        ctx.request_repaint_after(Duration::from_secs(2));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut action = None;
        egui::Panel::bottom("footer").show(ui, |ui| self.footer(ui, &mut action));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.body(ui, &mut action));
        });
        let ctx = ui.ctx().clone();
        self.login_dialog(&ctx);
        self.confirm_dialog(&ctx, &mut action);
        if let Some(action) = action {
            self.run(action);
        }
    }
}

fn window_row(ui: &mut egui::Ui, label: &str, window: &Window) {
    ui.label(label);
    let fraction = (window.pct / 100.0).clamp(0.0, 1.0) as f32;
    let reset_passed = window
        .resets_at
        .as_deref()
        .and_then(|iso| chrono::DateTime::parse_from_rfc3339(iso).ok())
        .is_some_and(|at| at <= chrono::Utc::now());
    // Nach dem Reset ist der alte Wert überholt: grau statt rot.
    let color = if reset_passed {
        GRAY
    } else if window.pct >= 90.0 {
        RED
    } else if window.pct >= 70.0 {
        AMBER
    } else {
        GREEN
    };
    ui.add(
        egui::ProgressBar::new(fraction)
            .desired_width(150.0)
            .fill(color)
            .text(format!("{:.0} %", window.pct)),
    );
    let reset = window.resets_at.as_deref().and_then(format_reset).unwrap_or_default();
    ui.label(RichText::new(reset).weak());
    ui.end_row();
}

fn open_folder(dir: &std::path::Path) {
    let program = if cfg!(target_os = "macos") { "open" } else { "explorer" };
    let _ = std::process::Command::new(program).arg(dir).spawn();
}

/// Öffnet eine Adresse im Standardbrowser. Unter Windows über url.dll statt
/// `cmd /c start`, weil cmd die `&` in der Adresse als Befehlstrenner liest.
fn open_url(url: &str) {
    let mut command = if cfg!(windows) {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else {
        std::process::Command::new("xdg-open")
    };
    let _ = command.arg(url).spawn();
}

/// "Reset in 2 Std. 5 Min. (14:30)", bei anderem Tag mit Wochentag.
fn format_reset(iso: &str) -> Option<String> {
    let at = chrono::DateTime::parse_from_rfc3339(iso)
        .ok()?
        .with_timezone(&chrono::Local);
    let now = chrono::Local::now();
    let secs = (at - now).num_seconds();
    if secs <= 0 {
        return Some("inzwischen zurückgesetzt".into());
    }
    let (days, hours, mins) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    let rest = if days > 0 {
        format!("{days} T. {hours} Std.")
    } else if hours > 0 {
        format!("{hours} Std. {mins} Min.")
    } else {
        format!("{} Min.", mins.max(1))
    };
    let clock = at.format("%H:%M");
    let when = if at.date_naive() == now.date_naive() {
        clock.to_string()
    } else {
        format!("{} {clock}", weekday(at.weekday()))
    };
    Some(format!("Reset in {rest} ({when})"))
}

fn weekday(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Mo",
        Weekday::Tue => "Di",
        Weekday::Wed => "Mi",
        Weekday::Thu => "Do",
        Weekday::Fri => "Fr",
        Weekday::Sat => "Sa",
        Weekday::Sun => "So",
    }
}

/// "Stand vor 7 Min.", erst ab zwei Minuten Alter.
fn age_text(fetched_at: &str) -> Option<String> {
    let at = chrono::DateTime::parse_from_rfc3339(fetched_at).ok()?;
    let mins = chrono::Utc::now()
        .signed_duration_since(at.with_timezone(&chrono::Utc))
        .num_minutes();
    if mins < 2 {
        return None;
    }
    Some(if mins < 60 {
        format!("Stand vor {mins} Min.")
    } else if mins < 48 * 60 {
        format!("Stand vor {} Std.", mins / 60)
    } else {
        format!("Stand vor {} Tagen", mins / 1440)
    })
}
