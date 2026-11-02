//! Die Oberfläche: eine Karte pro Slot mit Verbrauch und Knöpfen.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::anyhow;
use chrono::{Datelike, Weekday};
use eframe::egui::{self, Color32, RichText};

use crate::claude::{self, Identity};
use crate::clipboard;
use crate::endpoint::Endpoint;
use crate::i18n::{self, Lang};
use crate::login::PendingLogin;
use crate::runpod::{self, Metrics, Schedule, StatsFile, Workers};
use crate::slots::{self, Live, MAX_SLOTS, Slot, SlotStore};
use crate::usage::{self, FetchError, Usage, UsageCache, Window};

/// Wie oft die Dateien neu gelesen werden (lokal, billig).
const RELOAD_EVERY: Duration = Duration::from_secs(3);
/// Wie oft nachgesehen wird, ob für einen Account eine Abfrage fällig ist.
/// Die eigentlichen Abstände stehen in `usage.rs`.
const CHECK_EVERY: Duration = Duration::from_secs(30);

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
    RunpodHealth {
        id: String,
        result: Result<Workers, FetchError>,
    },
    RunpodMetrics {
        id: String,
        result: Result<Metrics, FetchError>,
    },
}

/// Status eines Runpod-Endpoints (siehe `runpod.rs`).
#[derive(Default)]
struct RunpodState {
    schedule: Schedule,
    workers: Option<Workers>,
    metrics: Option<Metrics>,
    health_error: Option<String>,
    metrics_error: Option<String>,
    health_loading: bool,
    metrics_loading: bool,
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

/// Das Fenster "Endpoint einrichten" bzw. "bearbeiten".
struct EndpointDialog {
    slot: usize,
    endpoint: Endpoint,
    show_token: bool,
    error: Option<String>,
}

/// Hinweis unter dem Verbrauch. Als Wert gespeichert, damit er nach einem
/// Sprachwechsel in der neuen Sprache erscheint.
enum Note {
    NoToken,
    ExpiredActive,
    ExpiredInactive,
    Failed(FetchError),
}

impl Note {
    fn text(&self) -> String {
        match self {
            Note::NoToken => tr!("Kein Access-Token vorhanden", "No access token"),
            Note::ExpiredActive => tr!(
                "Token abgelaufen, Claude Code erneuert es beim nächsten Prompt",
                "Token expired, Claude Code renews it on the next prompt"
            ),
            Note::ExpiredInactive => tr!(
                "Token abgelaufen, angezeigt wird der letzte bekannte Stand",
                "Token expired, showing the last known values"
            ),
            Note::Failed(err) => err.to_string(),
        }
    }
}

#[derive(Default)]
struct UsageState {
    loading: bool,
    note: Option<Note>,
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
    EditEndpoint(usize),
    AddSlot,
    RemoveSlot(usize),
    SetLanguage(Lang),
    /// Die Rückfrage wurde bestätigt.
    Confirm,
    Refresh,
}

/// Wartet auf Bestätigung: Überschreiben eines anderen Accounts oder Entfernen.
enum Pending {
    Capture(usize),
    Login { slot: usize, data: Box<Slot> },
    Remove(usize),
}

pub struct App {
    store: Result<SlotStore, String>,
    slots: Vec<Option<Slot>>,
    live: Option<Live>,
    live_error: Option<String>,
    /// Endpoint-Slot, der gerade in Claude Codes settings.json steht.
    active_endpoint: Option<usize>,
    cache: Option<UsageCache>,
    usage: HashMap<String, UsageState>,
    status: Option<Status>,
    pending: Option<Pending>,
    login: Option<LoginDialog>,
    endpoint_dialog: Option<EndpointDialog>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    last_reload: Option<Instant>,
    last_check: Option<Instant>,
    /// Beim nächsten Durchlauf nachsehen; `true` = über "Aktualisieren".
    check_requested: Option<bool>,
    runpod: HashMap<String, RunpodState>,
    runpod_stats: Option<StatsFile>,
    /// "Aktualisieren" gedrückt: Runpod-Zähler einmal sofort abfragen.
    runpod_manual: bool,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let store = SlotStore::open().map_err(|e| format!("{e:#}"));
        let cache = store.as_ref().ok().map(|s| UsageCache::load(s.dir()));
        let runpod_stats = store.as_ref().ok().map(|s| StatsFile::load(s.dir()));
        let (tx, rx) = channel();
        Self {
            store,
            slots: Vec::new(),
            live: None,
            live_error: None,
            active_endpoint: None,
            cache,
            usage: HashMap::new(),
            status: None,
            pending: None,
            login: None,
            endpoint_dialog: None,
            tx,
            rx,
            last_reload: None,
            last_check: None,
            check_requested: None,
            runpod: HashMap::new(),
            runpod_stats,
            runpod_manual: false,
        }
    }

    fn store(&self) -> anyhow::Result<&SlotStore> {
        self.store.as_ref().map_err(|e| anyhow!("{e}"))
    }

    fn slot(&self, n: usize) -> Option<&Slot> {
        self.slots.get(n.wrapping_sub(1)).and_then(Option::as_ref)
    }

    fn reload(&mut self) {
        self.last_reload = Some(Instant::now());
        let Ok(store) = &self.store else { return };
        let count = store.count();
        self.slots.resize(count, None);
        for n in 1..=count {
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
        match store.active_endpoint() {
            Ok(n) => self.active_endpoint = n,
            Err(err) => {
                self.active_endpoint = None;
                self.status = Some(Status::error(err));
            }
        }
    }

    /// Der Slot, den Claude Code gerade benutzt: ein aktiver Endpoint geht vor.
    fn active_slot(&self) -> Option<usize> {
        self.active_endpoint.or_else(|| self.account_slot())
    }

    /// Der Slot mit dem Account, mit dem Claude Code eingeloggt ist (auch wenn
    /// gerade ein Endpoint darüber liegt).
    fn account_slot(&self) -> Option<usize> {
        let live = self.live.as_ref()?;
        (1..=self.slots.len()).find(|&n| {
            self.slot(n)
                .and_then(Slot::identity)
                .is_some_and(|id| id.same_account(&live.identity))
        })
    }

    /// Fragt die Accounts ab, für die laut `usage.rs` eine Abfrage fällig ist.
    fn poll_usage(&mut self, ctx: &egui::Context, manual: bool) {
        self.last_check = Some(Instant::now());
        // Für Tests und Screenshots mit erfundenen Tokens.
        if std::env::var_os("CLAUDE_SLOTS_NO_FETCH").is_some_and(|v| !v.is_empty()) {
            return;
        }
        let active = self.account_slot();
        let Some(cache) = &mut self.cache else { return };
        for n in 1..=self.slots.len() {
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
                state.note = Some(Note::NoToken);
                continue;
            };
            if token.expired() {
                state.note = Some(if is_active { Note::ExpiredActive } else { Note::ExpiredInactive });
                continue;
            }
            let interval = if manual {
                Duration::ZERO
            } else if is_active {
                usage::ACTIVE_INTERVAL
            } else {
                usage::INACTIVE_INTERVAL
            };
            if !cache.claim(&id.key(), interval) {
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

    /// Runpod-Endpoints: Worker-Status immer (kostenlos), Token-Zähler nur bei
    /// laufendem Worker und solange gearbeitet wird.
    fn poll_runpod(&mut self, ctx: &egui::Context) {
        let manual = std::mem::take(&mut self.runpod_manual);
        if std::env::var_os("CLAUDE_SLOTS_NO_FETCH").is_some_and(|v| !v.is_empty()) {
            return;
        }
        let now = Instant::now();
        let endpoints: Vec<_> = self
            .slots
            .iter()
            .flatten()
            .filter_map(|slot| slot.endpoint.clone())
            .filter_map(|ep| runpod::endpoint_id(&ep.base_url).map(|id| (id, ep)))
            .collect();
        for (id, ep) in endpoints {
            let state = self.runpod.entry(id.clone()).or_default();
            if !state.health_loading && (manual || state.schedule.health_due(now)) {
                state.health_loading = true;
                state.schedule.health_started(now);
                let (tx, ctx, id, token) = (self.tx.clone(), ctx.clone(), id.clone(), ep.auth_token.clone());
                std::thread::spawn(move || {
                    let result = runpod::fetch_health(&id, &token);
                    let _ = tx.send(Msg::RunpodHealth { id, result });
                    ctx.request_repaint();
                });
            }
            if !state.metrics_loading && state.schedule.metrics_due(now, manual) {
                state.metrics_loading = true;
                state.schedule.metrics_started(now, manual);
                let (tx, ctx) = (self.tx.clone(), ctx.clone());
                let (url, token) = (ep.base_url.clone(), ep.auth_token.clone());
                std::thread::spawn(move || {
                    let result = runpod::fetch_metrics(&url, &token);
                    let _ = tx.send(Msg::RunpodMetrics { id, result });
                    ctx.request_repaint();
                });
            }
        }
    }

    /// Worker-Status und Tokens unter einem Runpod-Endpoint.
    fn runpod_rows(&self, ui: &mut egui::Ui, id: &str) {
        let state = self.runpod.get(id);
        let workers = state.and_then(|s| s.workers);
        let metrics = state.and_then(|s| s.metrics);
        let (worker_text, color) = match workers {
            None => (tr!("Worker: Status unbekannt", "Worker: status unknown"), GRAY),
            Some(w) if w.initializing > 0 => (tr!("Worker startet …", "Worker starting …"), AMBER),
            Some(w) if w.up() => {
                let busy = metrics.map_or(0.0, |m| m.processing);
                if busy > 0.0 {
                    let n = busy as u64;
                    (tr!("Worker läuft · {n} Anfrage(n) in Arbeit", "Worker running · {n} request(s) in progress"), GREEN)
                } else {
                    (tr!("Worker läuft (kostet, bis er abschaltet)", "Worker running (billed until it shuts down)"), GREEN)
                }
            }
            Some(w) if w.throttled > 0 => (tr!("Worker wartet auf eine freie GPU", "Worker waiting for a free GPU"), AMBER),
            Some(_) => (tr!("Worker aus (startet beim nächsten Aufruf)", "Worker off (starts on the next request)"), GRAY),
        };
        ui.label(RichText::new(worker_text).small().color(color));

        let totals = self
            .runpod_stats
            .as_ref()
            .and_then(|stats| stats.entries.get(id));
        if let Some(totals) = totals {
            let (day_in, day_out) = totals.today(&runpod::today());
            let (day_in, day_out) = (runpod::format_tokens(day_in), runpod::format_tokens(day_out));
            let (all_in, all_out) = (
                runpod::format_tokens(totals.prompt_tokens),
                runpod::format_tokens(totals.predicted_tokens),
            );
            egui::Grid::new(("runpod", id))
                .num_columns(3)
                .spacing([10.0, 2.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("").small());
                    ui.label(RichText::new(tr!("eingelesen", "input")).small().weak());
                    ui.label(RichText::new(tr!("erzeugt", "output")).small().weak());
                    ui.end_row();
                    ui.label(RichText::new(tr!("Heute", "Today")).small());
                    ui.label(RichText::new(day_in).small());
                    ui.label(RichText::new(day_out).small());
                    ui.end_row();
                    ui.label(RichText::new(tr!("Insgesamt", "Total")).small());
                    ui.label(RichText::new(all_in).small());
                    ui.label(RichText::new(all_out).small());
                    ui.end_row();
                });
        }
        if let Some(m) = metrics.filter(|_| workers.is_some_and(|w| w.up())) {
            if let (Some(p), Some(g)) = (m.prompt_rate(), m.predicted_rate()) {
                let (p, g) = (p.round() as u64, g.round() as u64);
                let (p, g) = (runpod::format_tokens(p), runpod::format_tokens(g));
                ui.label(
                    RichText::new(tr!(
                        "Tempo dieses Workers: Einlesen {p} Tok/s · Antwort {g} Tok/s",
                        "Speed of this worker: input {p} tok/s · output {g} tok/s"
                    ))
                    .small()
                    .weak(),
                );
            }
        }
        let paused = state.is_some_and(|s| s.schedule.paused) && workers.is_some_and(|w| w.up());
        if paused {
            ui.label(
                RichText::new(tr!(
                    "Token-Abfrage pausiert, damit der Worker abschalten kann. \
                     \"Aktualisieren\" fragt einmal neu.",
                    "Token queries paused so the worker can shut down. \
                     \"Refresh\" queries once."
                ))
                .small()
                .weak(),
            );
        }
        let error = state.and_then(|s| s.health_error.as_ref().or(s.metrics_error.as_ref()));
        if let Some(err) = error {
            ui.label(RichText::new(format!("Runpod: {err}")).small().color(AMBER));
        }
    }

    fn receive(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Usage { key, result } => {
                    if let Some(cache) = &mut self.cache {
                        cache.record(&key, &result);
                    }
                    let state = self.usage.entry(key).or_default();
                    state.loading = false;
                    state.note = result.err().map(Note::Failed);
                }
                Msg::Login { slot, result } => self.login_finished(slot, result),
                Msg::RunpodHealth { id, result } => {
                    let state = self.runpod.entry(id).or_default();
                    state.health_loading = false;
                    match result {
                        Ok(workers) => {
                            state.schedule.health_result(&workers);
                            state.workers = Some(workers);
                            state.health_error = None;
                        }
                        Err(err) => state.health_error = Some(err.to_string()),
                    }
                }
                Msg::RunpodMetrics { id, result } => {
                    let state = self.runpod.entry(id.clone()).or_default();
                    state.metrics_loading = false;
                    match result {
                        Ok(metrics) => {
                            let new_worker = state.schedule.take_new_worker();
                            let grew = self
                                .runpod_stats
                                .as_mut()
                                .is_some_and(|stats| stats.record(&id, &metrics, new_worker));
                            let busy = grew || metrics.processing > 0.0 || metrics.deferred > 0.0;
                            state.schedule.metrics_result(Instant::now(), busy);
                            state.metrics = Some(metrics);
                            state.metrics_error = None;
                        }
                        Err(err) => {
                            // Fehler (z. B. Worker gerade weg) zählen als Ruhe.
                            state.schedule.metrics_result(Instant::now(), false);
                            state.metrics_error = Some(err.to_string());
                        }
                    }
                }
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
                    self.pending = Some(Pending::Login { slot, data: Box::new(data) });
                } else {
                    self.status = Some(self.store_login(slot, data));
                    self.reload();
                    self.check_requested = Some(false);
                }
            }
        }
    }

    /// Würde Slot `n` mit einem anderen Account überschrieben? Dann erst nachfragen.
    fn overwrites_other(&self, n: usize, new: Option<&Identity>) -> bool {
        if self.slot(n).is_some_and(|slot| slot.endpoint.is_some()) {
            return true;
        }
        match (self.slot(n).and_then(Slot::identity), new) {
            (Some(old), Some(new)) => !old.same_account(new),
            _ => false,
        }
    }

    fn capture(&self, n: usize) -> Status {
        match self.store().and_then(|store| slots::capture(store, n)) {
            Ok(id) => {
                let email = id.email;
                Status::ok(tr!("{email} in Slot {n} gespeichert.", "{email} stored in slot {n}."))
            }
            Err(err) => Status::error(err),
        }
    }

    fn store_login(&self, n: usize, data: Slot) -> Status {
        match self.store().and_then(|store| slots::store_login(store, n, data)) {
            Ok((id, false)) => {
                let email = id.email;
                Status::ok(tr!("{email} in Slot {n} eingeloggt.", "{email} logged in to slot {n}."))
            }
            Ok((id, true)) => {
                let email = id.email;
                Status::ok(tr!(
                    "{email} in Slot {n} eingeloggt. Claude Code nutzt den neuen Login ebenfalls.",
                    "{email} logged in to slot {n}. Claude Code uses the new login too."
                ))
            }
            Err(err) => Status::error(err),
        }
    }

    fn remove_slot(&self, n: usize) -> Status {
        match self.store().and_then(|store| store.remove_slot(n)) {
            Ok(()) => Status::ok(tr!("Slot {n} entfernt.", "Slot {n} removed.")),
            Err(err) => Status::error(err),
        }
    }

    fn run(&mut self, action: Action) {
        // Danach nachsehen, ob eine Abfrage fällig ist (z. B. für den neuen
        // aktiven Account). `Some(true)` = "Aktualisieren", nur 3 Minuten Abstand.
        let mut check = Some(false);
        match action {
            Action::Refresh => {
                check = Some(true);
                self.runpod_manual = true;
            }
            Action::Switch(n) => {
                let result = self.store().and_then(|store| slots::switch_to(store, n));
                self.status = Some(match result {
                    Ok(switched) if switched.restart_needed => {
                        let label = switched.label;
                        Status::ok(tr!(
                            "Gewechselt zu Slot {n} ({label}). Laufende Claude-Code-Sitzungen \
                             neu starten, damit sie die Änderung übernehmen.",
                            "Switched to slot {n} ({label}). Restart running Claude Code \
                             sessions so they pick up the change."
                        ))
                    }
                    Ok(switched) => {
                        let label = switched.label;
                        Status::ok(tr!(
                            "Gewechselt zu Slot {n} ({label}). Claude Code übernimmt den Account \
                             beim nächsten Prompt.",
                            "Switched to slot {n} ({label}). Claude Code picks up the account \
                             on the next prompt."
                        ))
                    }
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
            Action::EditEndpoint(n) => {
                let endpoint = self
                    .slot(n)
                    .and_then(|slot| slot.endpoint.clone())
                    .unwrap_or_default();
                self.endpoint_dialog = Some(EndpointDialog {
                    slot: n,
                    endpoint,
                    show_token: false,
                    error: None,
                });
                return;
            }
            Action::AddSlot => {
                self.status = match self.store().and_then(SlotStore::add_slot) {
                    Ok(_) => None,
                    Err(err) => Some(Status::error(err)),
                };
                check = None;
            }
            Action::RemoveSlot(n) => {
                // Ein leerer Slot geht ohne Rückfrage.
                if self.slot(n).is_some() {
                    self.pending = Some(Pending::Remove(n));
                    return;
                }
                self.status = Some(self.remove_slot(n));
                check = None;
            }
            Action::SetLanguage(lang) => {
                i18n::set(lang);
                if let Ok(store) = &self.store {
                    if let Err(err) = store.set_language(lang) {
                        self.status = Some(Status::error(err));
                        return;
                    }
                }
                // Meldungen stehen noch in der alten Sprache.
                self.status = None;
                return;
            }
            Action::Confirm => match self.pending.take() {
                Some(Pending::Capture(n)) => self.status = Some(self.capture(n)),
                Some(Pending::Login { slot, data }) => {
                    self.status = Some(self.store_login(slot, *data));
                }
                Some(Pending::Remove(n)) => {
                    self.status = Some(self.remove_slot(n));
                    check = None;
                }
                None => {}
            },
        }
        self.reload();
        if let Some(manual) = check {
            self.check_requested = Some(manual || self.check_requested == Some(true));
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
                let email = &live.identity.email;
                ui.colored_label(
                    AMBER,
                    tr!(
                        "Claude Code ist mit {email} eingeloggt, der Account liegt in keinem Slot. \
                         Bei einem Slot \"Login übernehmen\" klicken.",
                        "Claude Code is logged in as {email}, and that account is in no slot. \
                         Click \"Take over login\" on a slot."
                    ),
                );
            }
        } else if self.active_endpoint.is_none() {
            ui.colored_label(AMBER, tr!("Claude Code ist nicht eingeloggt.", "Claude Code is not logged in."));
        }
        ui.add_space(4.0);
        let account = self.account_slot();
        for n in 1..=self.slots.len() {
            // Eingeloggt, aber ein Endpoint liegt darüber.
            let paused = self.active_endpoint.is_some() && account == Some(n);
            self.slot_card(ui, n, active == Some(n), paused, action);
            ui.add_space(8.0);
        }
        let can_add = self.slots.len() < MAX_SLOTS;
        if ui
            .add_enabled(can_add, egui::Button::new(tr!("+ Slot hinzufügen", "+ Add slot")))
            .on_disabled_hover_text(tr!("Höchstens {MAX_SLOTS} Slots", "At most {MAX_SLOTS} slots"))
            .clicked()
        {
            *action = Some(Action::AddSlot);
        }
    }

    fn slot_card(&self, ui: &mut egui::Ui, n: usize, active: bool, paused: bool, action: &mut Option<Action>) {
        let slot = self.slot(n);
        let count = self.slots.len();
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.heading(format!("Slot {n}"));
                    if active {
                        ui.label(
                            RichText::new(tr!(" AKTIV ", " ACTIVE "))
                                .strong()
                                .color(Color32::BLACK)
                                .background_color(GREEN),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let hint = if active {
                            tr!("Der aktive Slot kann nicht entfernt werden", "The active slot cannot be removed")
                        } else if count <= 1 {
                            tr!("Der letzte Slot bleibt", "The last slot cannot be removed")
                        } else {
                            tr!("Slot entfernen", "Remove slot")
                        };
                        // "×" statt "✕": das zweite fehlt in egui's eingebauter Schrift.
                        let remove = egui::Button::new(RichText::new("×").size(20.0)).frame(false);
                        if ui
                            .add_enabled(!active && count > 1, remove)
                            .on_hover_text(&hint)
                            .on_disabled_hover_text(&hint)
                            .clicked()
                        {
                            *action = Some(Action::RemoveSlot(n));
                        }
                    });
                });
                let endpoint = slot.and_then(|s| s.endpoint.as_ref());
                if let Some(endpoint) = endpoint {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(endpoint.display_name()).strong());
                        ui.label(RichText::new(tr!("Eigener Endpoint", "Custom endpoint")).weak());
                    });
                    ui.label(RichText::new(&endpoint.base_url).small().weak());
                    let model = &endpoint.model;
                    ui.label(RichText::new(tr!("Modell: {model}", "Model: {model}")).small().weak());
                    if let Some(id) = runpod::endpoint_id(&endpoint.base_url) {
                        ui.add_space(4.0);
                        self.runpod_rows(ui, &id);
                    }
                } else {
                    match slot.and_then(|s| s.identity().map(|id| (s, id))) {
                        None => {
                            ui.label(RichText::new(tr!("Leer", "Empty")).weak());
                        }
                        Some((slot, id)) => {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(&id.email).strong());
                                if let Some(plan) = claude::plan_label(&slot.credentials) {
                                    ui.label(RichText::new(plan).weak());
                                }
                            });
                            if paused {
                                ui.label(
                                    RichText::new(tr!(
                                        "Claude Code ist mit diesem Account eingeloggt, nutzt aber \
                                         gerade den Endpoint.",
                                        "Claude Code is logged in with this account but currently \
                                         uses the endpoint."
                                    ))
                                    .small()
                                    .color(AMBER),
                                );
                            }
                            ui.add_space(4.0);
                            self.usage_rows(ui, n, &id);
                        }
                    }
                }
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    let can_switch = (slot.is_some() && !active) || paused;
                    if ui
                        .add_enabled(can_switch, egui::Button::new(tr!("Hierher wechseln", "Switch here")))
                        .clicked()
                    {
                        *action = Some(Action::Switch(n));
                    }
                    if endpoint.is_some() {
                        if ui.button(tr!("Bearbeiten", "Edit")).clicked() {
                            *action = Some(Action::EditEndpoint(n));
                        }
                        return;
                    }
                    if ui
                        .button(tr!("Login übernehmen", "Take over login"))
                        .on_hover_text(tr!(
                            "Speichert den Account, mit dem Claude Code gerade eingeloggt ist, \
                             in diesem Slot.",
                            "Stores the account Claude Code is currently logged in with in this slot."
                        ))
                        .clicked()
                    {
                        *action = Some(Action::Capture(n));
                    }
                    if ui
                        .button(tr!("Neu einloggen", "Log in"))
                        .on_hover_text(tr!(
                            "Mit einem Account einloggen, ohne Claude Code abzumelden. Jeder \
                             Login ist eigenständig, auch auf mehreren PCs gleichzeitig.",
                            "Log in to an account without logging Claude Code out. Every login \
                             is independent, even on several PCs at the same time."
                        ))
                        .clicked()
                    {
                        *action = Some(Action::Login(n));
                    }
                    if slot.is_none()
                        && ui
                            .button(tr!("Endpoint einrichten", "Set up endpoint"))
                            .on_hover_text(tr!(
                                "Eigener Anthropic-kompatibler Server statt eines Claude-Accounts, \
                                 z. B. ein eigenes Modell auf Runpod.",
                                "Your own Anthropic-compatible server instead of a Claude account, \
                                 e.g. your own model on Runpod."
                            ))
                            .clicked()
                    {
                        *action = Some(Action::EditEndpoint(n));
                    }
                });
            });
    }

    fn usage_rows(&self, ui: &mut egui::Ui, n: usize, id: &Identity) {
        let key = id.key();
        let entry = self.cache.as_ref().and_then(|cache| cache.get(&key));
        let state = self.usage.get(&key);
        let reading = entry.and_then(|e| e.usage.as_ref().map(|usage| (usage, e.fetched_at.as_deref())));
        match reading {
            None => {
                let text = if state.is_some_and(|s| s.loading) {
                    tr!("Verbrauch wird geladen …", "Loading usage …")
                } else {
                    tr!("Noch kein Verbrauch bekannt", "No usage known yet")
                };
                ui.label(RichText::new(text).weak());
            }
            Some((usage, fetched_at)) => {
                egui::Grid::new(("usage", n))
                    .num_columns(3)
                    .spacing([10.0, 4.0])
                    .show(ui, |ui| {
                        if let Some(window) = &usage.five_hour {
                            window_row(ui, &tr!("5 Std.", "5 h"), window);
                        }
                        if let Some(window) = &usage.seven_day {
                            window_row(ui, &tr!("7 Tage", "7 days"), window);
                        }
                        for model in &usage.models {
                            window_row(ui, &model.name, &model.window);
                        }
                    });
                if let Some(age) = fetched_at.and_then(age_text) {
                    ui.label(RichText::new(age).small().weak());
                }
            }
        }
        let note = state.and_then(|s| s.note.as_ref()).map(Note::text);
        // Pause nach 429 oder Fehlern, steht in der Cache-Datei (überlebt Neustarts).
        let paused = entry.and_then(|e| e.paused_until(chrono::Utc::now()));
        let line = match (note, paused) {
            (note, Some(until)) => {
                let clock = until.with_timezone(&chrono::Local).format("%H:%M").to_string();
                Some(match note {
                    Some(note) => tr!("{note}, nächste Abfrage um {clock}", "{note}, next query at {clock}"),
                    None => tr!("Abfragen pausiert bis {clock}", "Queries paused until {clock}"),
                })
            }
            (note, None) => note,
        };
        if let Some(line) = line {
            ui.label(RichText::new(line).small().color(AMBER));
        }
    }

    fn footer(&self, ui: &mut egui::Ui, action: &mut Option<Action>) {
        ui.add_space(6.0);
        if let Some(status) = &self.status {
            let color = if status.error { RED } else { GREEN };
            ui.colored_label(color, status.text.as_str());
        }
        ui.horizontal(|ui| {
            if ui
                .button(tr!("Aktualisieren", "Refresh"))
                .on_hover_text(tr!(
                    "Fragt den Verbrauch neu ab, höchstens alle 3 Minuten pro Account \
                     (die Schnittstelle erlaubt nur etwa 30 Abfragen pro Stunde).",
                    "Queries usage again, at most every 3 minutes per account \
                     (the endpoint allows only about 30 requests per hour)."
                ))
                .clicked()
            {
                *action = Some(Action::Refresh);
            }
            if let Ok(store) = &self.store {
                let dir = store.dir();
                if ui
                    .button(tr!("Ordner öffnen", "Open folder"))
                    .on_hover_text(dir.display().to_string())
                    .clicked()
                {
                    open_folder(dir);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Von rechts nach links: EN steht ganz rechts.
                for (lang, label) in [(Lang::En, "EN"), (Lang::De, "DE")] {
                    let current = i18n::get() == lang;
                    if ui.selectable_label(current, label).clicked() && !current {
                        *action = Some(Action::SetLanguage(lang));
                    }
                }
            });
        });
        ui.label(
            RichText::new(tr!(
                "Account einrichten: bei einem Slot \"Neu einloggen\", oder den aktuellen \
                 Login von Claude Code mit \"Login übernehmen\" speichern. Eigenes Modell \
                 (z. B. Runpod): bei einem leeren Slot \"Endpoint einrichten\".",
                "Add an account: click \"Log in\" on a slot, or store Claude Code's \
                 current login with \"Take over login\". Your own model (e.g. Runpod): \
                 \"Set up endpoint\" on an empty slot."
            ))
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
            let n = dialog.slot;
            ui.heading(tr!("Slot {n}: einloggen", "Slot {n}: log in"));
            ui.add_space(6.0);
            ui.label(tr!(
                "1. Link öffnen und mit dem Account anmelden, der in diesen Slot soll.",
                "1. Open the link and sign in with the account for this slot."
            ));
            ui.label(
                RichText::new(tr!(
                    "Ist der Browser schon mit einem anderen Claude-Account angemeldet: \
                     Link kopieren und in einem privaten Fenster öffnen.",
                    "If your browser is already signed in to another Claude account, copy \
                     the link and open it in a private window."
                ))
                .small()
                .weak(),
            );
            ui.horizontal(|ui| {
                if ui.button(tr!("Im Browser öffnen", "Open in browser")).clicked() {
                    open_url(&dialog.login.url);
                }
                let label = if dialog.copied {
                    tr!("Kopiert", "Copied")
                } else {
                    tr!("Link kopieren", "Copy link")
                };
                if ui.button(label).clicked() {
                    match clipboard::set(&dialog.login.url) {
                        Ok(()) => dialog.copied = true,
                        Err(err) => dialog.error = Some(format!("{err:#}")),
                    }
                }
            });
            ui.add_space(8.0);
            ui.label(tr!(
                "2. Nach der Anmeldung zeigt die Seite einen Code. Hier einfügen:",
                "2. After signing in, the page shows a code. Paste it here:"
            ));
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
                if ui
                    .add_enabled(!dialog.busy, egui::Button::new(tr!("Einfügen", "Paste")))
                    .clicked()
                {
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
                if ui.add_enabled(ready, egui::Button::new(tr!("Anmelden", "Sign in"))).clicked() {
                    submit = true;
                }
                if ui
                    .add_enabled(!dialog.busy, egui::Button::new(tr!("Abbrechen", "Cancel")))
                    .clicked()
                {
                    cancel = true;
                }
                if dialog.busy {
                    ui.spinner();
                    ui.label(tr!("Anmeldung läuft …", "Signing in …"));
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

    fn endpoint_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.endpoint_dialog else { return };
        let (mut save, mut cancel) = (false, false);
        let response = egui::Modal::new(egui::Id::unique("endpoint")).show(ctx, |ui| {
            ui.set_max_width(420.0);
            let n = dialog.slot;
            ui.heading(tr!("Slot {n}: eigener Endpoint", "Slot {n}: custom endpoint"));
            ui.label(
                RichText::new(tr!(
                    "Ein Anthropic-kompatibler Server (/v1/messages), z. B. llama.cpp auf \
                     Runpod. Claude Code nutzt ihn statt des Claude-Accounts, der Login \
                     bleibt erhalten.",
                    "An Anthropic-compatible server (/v1/messages), e.g. llama.cpp on \
                     Runpod. Claude Code uses it instead of the Claude account; the login \
                     is kept."
                ))
                .small()
                .weak(),
            );
            ui.add_space(6.0);
            let ep = &mut dialog.endpoint;
            egui::Grid::new("endpoint-fields")
                .num_columns(2)
                .spacing([8.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Name");
                    ui.add(
                        egui::TextEdit::singleline(&mut ep.name)
                            .hint_text("Runpod Qwen3-Coder")
                            .desired_width(280.0),
                    );
                    ui.end_row();
                    ui.label(tr!("Adresse", "Address"));
                    ui.add(
                        egui::TextEdit::singleline(&mut ep.base_url)
                            .hint_text("https://<ENDPOINT_ID>.api.runpod.ai")
                            .desired_width(280.0),
                    );
                    ui.end_row();
                    ui.label("Token");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut ep.auth_token)
                                .password(!dialog.show_token)
                                .hint_text(tr!("Runpod-API-Key", "Runpod API key"))
                                .desired_width(200.0),
                        );
                        ui.checkbox(&mut dialog.show_token, tr!("zeigen", "show"));
                    });
                    ui.end_row();
                    ui.label(tr!("Modell", "Model"));
                    ui.add(
                        egui::TextEdit::singleline(&mut ep.model)
                            .hint_text("qwen3-coder-next")
                            .desired_width(280.0),
                    );
                    ui.end_row();
                });
            ui.label(
                RichText::new(tr!(
                    "Gespeichert wie die Account-Tokens: im Klartext im Slot-Ordner. \
                     Beim Wechseln landen die Werte in ~/.claude/settings.json.",
                    "Stored like the account tokens: in plain text in the slot folder. \
                     Switching writes them to ~/.claude/settings.json."
                ))
                .small()
                .weak(),
            );
            if let Some(err) = &dialog.error {
                ui.colored_label(RED, err.as_str());
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                save = ui.button(tr!("Speichern", "Save")).clicked();
                cancel = ui.button(tr!("Abbrechen", "Cancel")).clicked();
            });
        });
        if save {
            let (n, endpoint) = (dialog.slot, dialog.endpoint.clone());
            let result = self.store().and_then(|store| slots::save_endpoint(store, n, endpoint));
            match result {
                Ok(reapplied) => {
                    self.endpoint_dialog = None;
                    self.status = Some(Status::ok(if reapplied {
                        tr!(
                            "Endpoint in Slot {n} gespeichert und neu eingetragen. Laufende \
                             Claude-Code-Sitzungen neu starten.",
                            "Endpoint in slot {n} saved and applied. Restart running Claude \
                             Code sessions."
                        )
                    } else {
                        tr!(
                            "Endpoint in Slot {n} gespeichert. Mit \"Hierher wechseln\" aktivieren.",
                            "Endpoint saved in slot {n}. Activate it with \"Switch here\"."
                        )
                    }));
                    self.reload();
                }
                Err(err) => {
                    if let Some(dialog) = &mut self.endpoint_dialog {
                        dialog.error = Some(format!("{err:#}"));
                    }
                }
            }
        } else if cancel || response.should_close() {
            self.endpoint_dialog = None;
        }
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context, action: &mut Option<Action>) {
        let Some(pending) = &self.pending else { return };
        let email_in = |n: usize| self.slot(n).map(Slot::label).unwrap_or_default();
        let (title, text, confirm_label) = match pending {
            Pending::Capture(n) | Pending::Login { slot: n, .. } => {
                let n = *n;
                let old = email_in(n);
                let new = match pending {
                    Pending::Login { data, .. } => {
                        let email = data.identity().map(|id| id.email).unwrap_or_default();
                        tr!("{email} (neu eingeloggt)", "{email} (new login)")
                    }
                    _ => self
                        .live
                        .as_ref()
                        .map(|live| live.identity.email.clone())
                        .unwrap_or_default(),
                };
                (
                    tr!("Slot {n} überschreiben?", "Overwrite slot {n}?"),
                    tr!(
                        "In Slot {n} liegt {old}. Mit {new} überschreiben? \
                         Der gespeicherte Login von {old} geht dabei verloren.",
                        "Slot {n} holds {old}. Overwrite it with {new}? \
                         The stored login of {old} will be lost."
                    ),
                    tr!("Überschreiben", "Overwrite"),
                )
            }
            Pending::Remove(n) => {
                let n = *n;
                let old = email_in(n);
                (
                    tr!("Slot {n} entfernen?", "Remove slot {n}?"),
                    tr!(
                        "Der gespeicherte Login von {old} wird gelöscht. \
                         Die folgenden Slots rücken nach.",
                        "The stored login of {old} will be deleted. \
                         The following slots move up."
                    ),
                    tr!("Entfernen", "Remove"),
                )
            }
        };
        let (mut confirmed, mut cancelled) = (false, false);
        let response = egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
            ui.set_max_width(380.0);
            ui.heading(title);
            ui.label(text);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                confirmed = ui.button(confirm_label).clicked();
                cancelled = ui.button(tr!("Abbrechen", "Cancel")).clicked();
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
        if let Some(manual) = self.check_requested.take() {
            self.poll_usage(ctx, manual);
        } else if self.last_check.is_none_or(|t| t.elapsed() >= CHECK_EVERY) {
            self.poll_usage(ctx, false);
        }
        self.poll_runpod(ctx);
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
        self.endpoint_dialog(&ctx);
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
        return Some(tr!("inzwischen zurückgesetzt", "has reset since"));
    }
    let (days, hours, mins) = (secs / 86_400, secs % 86_400 / 3600, secs % 3600 / 60);
    let rest = if days > 0 {
        tr!("{days} T. {hours} Std.", "{days} d {hours} h")
    } else if hours > 0 {
        tr!("{hours} Std. {mins} Min.", "{hours} h {mins} min")
    } else {
        let mins = mins.max(1);
        tr!("{mins} Min.", "{mins} min")
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
    let (de, en) = match day {
        Weekday::Mon => ("Mo", "Mon"),
        Weekday::Tue => ("Di", "Tue"),
        Weekday::Wed => ("Mi", "Wed"),
        Weekday::Thu => ("Do", "Thu"),
        Weekday::Fri => ("Fr", "Fri"),
        Weekday::Sat => ("Sa", "Sat"),
        Weekday::Sun => ("So", "Sun"),
    };
    match i18n::get() {
        Lang::De => de,
        Lang::En => en,
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
        tr!("Stand vor {mins} Min.", "Updated {mins} min ago")
    } else if mins < 48 * 60 {
        let hours = mins / 60;
        tr!("Stand vor {hours} Std.", "Updated {hours} h ago")
    } else {
        let days = mins / 1440;
        tr!("Stand vor {days} Tagen", "Updated {days} days ago")
    })
}
