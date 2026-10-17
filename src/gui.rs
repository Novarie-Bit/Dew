use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration as StdDuration;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, Local, Utc};
use eframe::egui::{
    self, Align, Color32, CornerRadius, CursorIcon, FontId, Frame, Id, Key, Layout, Margin, Rect,
    Response, RichText, Sense, TextEdit, TextStyle, Ui, UiBuilder, vec2,
};

use crate::auth;
use crate::browser_login::{self, Site};
use crate::canvas::normalize_url;
use crate::config::{Config, Credentials, DEFAULT_CANVAS_URL, Paths};
use crate::hidden::{self, Hidden};
use crate::model::{self, Assignment, Filter, Source, Status, Urgency};
use crate::sync::{self, Cache, Report};

pub fn run(cfg: Config, paths: Paths) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Dew")
            .with_app_id("dew")
            .with_inner_size([860.0, 700.0])
            .with_min_inner_size([520.0, 380.0]),
        ..Default::default()
    };
    eframe::run_native(
        "dew",
        options,
        Box::new(|cc| Ok(Box::new(Gui::new(cc, cfg, paths)))),
    )
    .map_err(|e| anyhow!("couldn't open a window ({e}); try `dew tui`"))
}

enum Msg {
    Synced(Report),
    Auth { ok: bool, text: String },
}

enum AuthJob {
    Browser(Site),
    CanvasToken { url: String, token: String },
    CanvasCookies { url: String, cookies: String },
    GradescopePassword { email: String, password: String },
    GradescopeCookies(String),
}

#[derive(Default)]
struct AccountsForm {
    canvas_url: String,
    canvas_token: String,
    canvas_cookies: String,
    gs_email: String,
    gs_password: String,
    gs_cookies: String,
    busy: bool,
    message: Option<(bool, String)>,
}

struct Gui {
    cfg: Config,
    paths: Paths,
    creds: Credentials,
    cache: Cache,
    filter: Filter,
    hidden: Hidden,
    /// The last "Hide all overdue", so it can be undone.
    undo: Option<Vec<String>>,
    syncing: bool,
    errors: Vec<String>,
    accounts_open: bool,
    form: AccountsForm,
    ctx: egui::Context,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Gui {
    fn new(cc: &eframe::CreationContext<'_>, cfg: Config, paths: Paths) -> Gui {
        cc.egui_ctx.all_styles_mut(|s| {
            // Rows are clickable as a whole; selectable text would steal clicks.
            s.interaction.selectable_labels = false;
            s.spacing.item_spacing = vec2(8.0, 6.0);
            s.spacing.button_padding = vec2(10.0, 4.0);
            s.text_styles
                .insert(TextStyle::Body, FontId::proportional(14.5));
            s.text_styles
                .insert(TextStyle::Button, FontId::proportional(14.0));
            s.text_styles
                .insert(TextStyle::Small, FontId::proportional(12.0));
        });
        match cfg.theme.as_str() {
            "light" => cc.egui_ctx.set_theme(egui::Theme::Light),
            "dark" => cc.egui_ctx.set_theme(egui::Theme::Dark),
            _ => {}
        }
        let (tx, rx) = channel();
        let mut gui = Gui {
            creds: Credentials::load(&paths).unwrap_or_default(),
            cache: Cache::load(&paths),
            hidden: Hidden::load(&paths),
            undo: None,
            cfg,
            paths,
            filter: Filter::default(),
            syncing: false,
            errors: Vec::new(),
            accounts_open: false,
            form: AccountsForm::default(),
            ctx: cc.egui_ctx.clone(),
            tx,
            rx,
        };
        if gui.signed_in() {
            gui.start_sync();
        } else {
            gui.open_accounts();
        }
        gui
    }

    fn signed_in(&self) -> bool {
        self.creds.canvas.is_some() || self.creds.gradescope.is_some()
    }

    fn start_sync(&mut self) {
        if self.syncing || !self.signed_in() {
            return;
        }
        self.syncing = true;
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        let (cfg, paths) = (self.cfg.clone(), self.paths.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Synced(sync::sync(&cfg, &paths)));
            ctx.request_repaint();
        });
    }

    fn maybe_auto_sync(&mut self) {
        let stale = self
            .cache
            .fetched_at
            .is_none_or(|t| Utc::now() - t > Duration::minutes(self.cfg.refresh_minutes));
        if stale && self.errors.is_empty() {
            self.start_sync();
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Synced(report) => {
                self.syncing = false;
                self.errors = report.errors;
                self.cache = report.cache;
            }
            Msg::Auth { ok, text } => {
                let f = &mut self.form;
                f.busy = false;
                f.message = Some((ok, text));
                if ok {
                    f.canvas_token.clear();
                    f.canvas_cookies.clear();
                    f.gs_password.clear();
                    f.gs_cookies.clear();
                    self.creds = Credentials::load(&self.paths).unwrap_or_default();
                    self.errors.clear();
                    self.start_sync();
                }
            }
        }
    }

    fn open_accounts(&mut self) {
        let f = &mut self.form;
        f.canvas_url = self
            .creds
            .canvas
            .as_ref()
            .map_or_else(|| DEFAULT_CANVAS_URL.into(), |c| c.url.clone());
        if let Some(g) = self
            .creds
            .gradescope
            .as_ref()
            .filter(|g| !g.email.starts_with('('))
        {
            f.gs_email = g.email.clone();
        }
        f.message = None;
        self.accounts_open = true;
    }

    fn run_auth(&mut self, job: AuthJob) {
        self.form.busy = true;
        self.form.message = None;
        let (tx, ctx, paths) = (self.tx.clone(), self.ctx.clone(), self.paths.clone());
        std::thread::spawn(move || {
            let (ok, text) = match job {
                AuthJob::Browser(site) => match browser_login::sign_in(&site) {
                    Ok(msg) => (true, msg),
                    Err(e) => (false, format!("{e:#}")),
                },
                AuthJob::CanvasToken { url, token } => {
                    match auth::login_canvas(&paths, &url, &token) {
                        Ok(user) => (true, format!("Signed in to Canvas as {user}")),
                        Err(e) => (false, format!("Canvas: {e:#}")),
                    }
                }
                AuthJob::CanvasCookies { url, cookies } => {
                    match auth::login_canvas_cookies(&paths, &url, &cookies) {
                        Ok(user) => (true, format!("Signed in to Canvas as {user}")),
                        Err(e) => (false, format!("Canvas: {e:#}")),
                    }
                }
                AuthJob::GradescopePassword { email, password } => {
                    match auth::login_gradescope(&paths, &email, &password) {
                        Ok(()) => (true, format!("Signed in to Gradescope as {email}")),
                        Err(e) => (false, format!("Gradescope: {e:#}")),
                    }
                }
                AuthJob::GradescopeCookies(c) => match auth::login_gradescope_cookies(&paths, &c) {
                    Ok(()) => (true, "Signed in to Gradescope".into()),
                    Err(e) => (false, format!("Gradescope: {e:#}")),
                },
            };
            let _ = tx.send(Msg::Auth { ok, text });
            ctx.request_repaint();
        });
    }

    fn sign_out(&mut self, source: Source) {
        let r = match source {
            Source::Canvas => auth::logout_canvas(&self.paths),
            Source::Gradescope => auth::logout_gradescope(&self.paths),
        };
        self.form.message = Some(match r {
            Ok(()) => (true, format!("Signed out of {}", source.label())),
            Err(e) => (false, format!("{e:#}")),
        });
        self.creds = Credentials::load(&self.paths).unwrap_or_default();
        self.cache.assignments.retain(|a| a.source != source);
        let _ = self.cache.save(&self.paths);
    }

    fn open(&mut self, url: &str) {
        if let Err(e) = crate::open_url(&self.cfg.opener, url) {
            self.errors
                .push(format!("couldn't run {}: {e}", self.cfg.opener));
        }
    }

    // ---- toolbar ----

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("dew").size(22.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Accounts").clicked() {
                    self.open_accounts();
                }
                let refresh = ui
                    .add_enabled(
                        self.signed_in() && !self.syncing,
                        egui::Button::new("Refresh"),
                    )
                    .on_hover_text("F5");
                if refresh.clicked() {
                    self.errors.clear();
                    self.start_sync();
                }
                if self.syncing {
                    ui.add(egui::Spinner::new());
                    ui.label(RichText::new("Syncing…").weak());
                } else if let Some(t) = self.cache.fetched_at {
                    let mins = (Utc::now() - t).num_minutes();
                    let text = if mins < 1 {
                        "Synced just now".to_string()
                    } else {
                        format!("Synced {mins} min ago")
                    };
                    ui.label(RichText::new(text).weak());
                }
            });
        });
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            for (label, value) in [
                ("All", None),
                ("Canvas", Some(Source::Canvas)),
                ("Gradescope", Some(Source::Gradescope)),
            ] {
                ui.selectable_value(&mut self.filter.source, value, label);
            }
            ui.separator();
            ui.checkbox(&mut self.filter.show_done, "Finished");
            ui.checkbox(&mut self.filter.show_past, "Older");
            if self.hidden.len() > 0 || self.filter.show_hidden {
                let label = format!("Hidden ({})", self.hidden.len());
                ui.checkbox(&mut self.filter.show_hidden, label)
                    .on_hover_text("Show assignments you've hidden");
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let search = ui.add(
                    TextEdit::singleline(&mut self.filter.query)
                        .hint_text("Search  (Ctrl+F)")
                        .desired_width(ui.available_width().clamp(120.0, 260.0)),
                );
                if ui.input(|i| i.modifiers.command && i.key_pressed(Key::F)) {
                    search.request_focus();
                }
                if search.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                    self.filter.query.clear();
                }
            });
        });
        ui.add_space(4.0);
    }

    fn error_banner(&mut self, ui: &mut Ui) {
        let mut dismiss = false;
        Frame::new()
            .fill(Color32::from_rgba_unmultiplied(220, 50, 50, 40))
            .stroke((1.0, Color32::from_rgb(200, 60, 60)))
            .corner_radius(6)
            .inner_margin(Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                        dismiss = ui.small_button("×").on_hover_text("Dismiss").clicked();
                        ui.vertical(|ui| {
                            for e in &self.errors {
                                ui.label(RichText::new(e).color(Color32::from_rgb(220, 80, 80)));
                            }
                        });
                    });
                });
            });
        if dismiss {
            self.errors.clear();
        }
    }

    // ---- assignment list ----

    fn save_hidden(&mut self) {
        if let Err(e) = self.hidden.save(&self.paths) {
            self.errors.push(format!("saving hidden list: {e:#}"));
        }
    }

    fn undo_bar(&mut self, ui: &mut Ui) {
        let Some(keys) = &self.undo else { return };
        let (mut undo, mut dismiss) = (false, false);
        Frame::new()
            .fill(ui.visuals().faint_bg_color)
            .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
            .corner_radius(6)
            .inner_margin(Margin::symmetric(10, 4))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let n = keys.len();
                    let s = if n == 1 { "" } else { "s" };
                    ui.label(format!("Hid {n} overdue assignment{s}."));
                    undo = ui.button("Undo").clicked();
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        dismiss = ui.small_button("×").on_hover_text("Dismiss").clicked();
                    });
                });
            });
        ui.add_space(6.0);
        if undo {
            if let Some(keys) = self.undo.take() {
                self.hidden.unhide_keys(&keys);
                self.save_hidden();
            }
        } else if dismiss {
            self.undo = None;
        }
    }

    fn list(&mut self, ui: &mut Ui) {
        let now = Utc::now();
        let mut items: Vec<&Assignment> = self
            .cache
            .assignments
            .iter()
            .filter(|a| {
                let hidden = self.hidden.contains(a);
                self.filter.keep(a, now, self.cfg.keep_past_days, hidden)
            })
            .collect();
        // Already sorted by due date; a stable sort keeps that within each bucket.
        items.sort_by_key(|a| section_of(a, now).0);

        if items.is_empty() {
            self.empty_state(ui);
            return;
        }

        enum Action {
            Open(String),
            Toggle(Assignment),
            HideOverdue,
        }
        let mut action = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                let mut current: Option<String> = None;
                for a in items {
                    let is_hidden = self.hidden.contains(a);
                    let (_, title) = section_of(a, now);
                    if current.as_ref() != Some(&title) {
                        ui.add_space(if current.is_none() { 4.0 } else { 12.0 });
                        ui.horizontal(|ui| {
                            let overdue = title == "Overdue";
                            let color = if overdue {
                                urgency_color(Urgency::Overdue)
                            } else {
                                ui.visuals().strong_text_color()
                            };
                            ui.label(RichText::new(&title).strong().size(13.5).color(color));
                            if overdue {
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    let b = ui.small_button("Hide all overdue").on_hover_text(
                                        "Remove every overdue assignment from the list",
                                    );
                                    if b.clicked() {
                                        action = Some(Action::HideOverdue);
                                    }
                                });
                            }
                        });
                        current = Some(title);
                    }
                    let (resp, toggle) = row(ui, a, now, is_hidden);
                    if toggle {
                        action = Some(Action::Toggle(a.clone()));
                    } else if resp.clicked() {
                        action = a.url.clone().map(Action::Open);
                    }
                    resp.context_menu(|ui| {
                        if let Some(url) = &a.url {
                            if ui.button("Open").clicked() {
                                action = Some(Action::Open(url.clone()));
                                ui.close();
                            }
                            if ui.button("Copy link").clicked() {
                                ui.ctx().copy_text(url.clone());
                                ui.close();
                            }
                        }
                        let label = if is_hidden { "Unhide" } else { "Hide" };
                        if ui.button(label).clicked() {
                            action = Some(Action::Toggle(a.clone()));
                            ui.close();
                        }
                    });
                }
                ui.add_space(12.0);
            });
        match action {
            Some(Action::Open(url)) => self.open(&url),
            Some(Action::Toggle(a)) => {
                self.hidden.toggle(&a);
                self.undo = None;
                self.save_hidden();
            }
            Some(Action::HideOverdue) => {
                let keys: Vec<String> = self
                    .cache
                    .assignments
                    .iter()
                    .filter(|a| a.urgency(now) == Urgency::Overdue && !self.hidden.contains(a))
                    .map(hidden::key)
                    .collect();
                self.hidden.hide_keys(keys.clone());
                self.undo = Some(keys);
                self.save_hidden();
            }
            None => {}
        }
    }

    fn empty_state(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            if !self.signed_in() {
                ui.label(RichText::new("Not signed in").size(20.0).strong());
                ui.label("Sign in to Canvas and Gradescope to see your assignments.");
                ui.add_space(8.0);
                if ui.button("Sign in…").clicked() {
                    self.open_accounts();
                }
            } else if self.syncing && self.cache.assignments.is_empty() {
                ui.add(egui::Spinner::new().size(28.0));
                ui.label("Fetching assignments…");
            } else if self.cache.assignments.is_empty() {
                ui.label(RichText::new("No assignments found").size(20.0).strong());
            } else {
                ui.label(RichText::new("Nothing due").size(22.0).strong());
                ui.label(RichText::new("Tick “Finished” or “Older” to see everything.").weak());
            }
        });
    }

    // ---- accounts dialog ----

    fn accounts(&mut self, ctx: &egui::Context) {
        let mut job = None;
        let mut sign_out = None;
        let mut close = false;
        let modal = egui::Modal::new(Id::unique("accounts")).show(ctx, |ui| {
            ui.set_width(460.0);
            let f = &mut self.form;
            let idle = !f.busy;
            ui.heading("Accounts");
            ui.label(
                RichText::new(
                    "“Sign in with browser” opens a window where you log in like on the \
                     website (WashU SSO and Duo work). dew only keeps the session cookie.",
                )
                .small()
                .weak(),
            );
            ui.add_space(8.0);

            // Canvas
            account_header(
                ui,
                "Canvas",
                self.creds
                    .canvas
                    .as_ref()
                    .map(|c| format!("{} · {}", c.user, c.url.trim_start_matches("https://"))),
            );
            ui.horizontal(|ui| {
                ui.label("URL");
                ui.add(TextEdit::singleline(&mut f.canvas_url).desired_width(220.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if self.creds.canvas.is_some() && ui.button("Sign out").clicked() {
                        sign_out = Some(Source::Canvas);
                    }
                    let browser = egui::Button::new(RichText::new("Sign in with browser").strong());
                    if ui.add_enabled(idle, browser).clicked() {
                        job = Some(AuthJob::Browser(Site::Canvas {
                            url: f.canvas_url.clone(),
                        }));
                    }
                });
            });
            egui::CollapsingHeader::new("Other ways to sign in")
                .id_salt("canvas-other")
                .show(ui, |ui| {
                    let settings = format!("{}/profile/settings", normalize_url(&f.canvas_url));
                    if secret_row(
                        ui,
                        "Access token",
                        &mut f.canvas_token,
                        "paste a token",
                        idle,
                    ) {
                        job = Some(AuthJob::CanvasToken {
                            url: f.canvas_url.clone(),
                            token: f.canvas_token.clone(),
                        });
                    }
                    if ui
                        .link("Create a token (if your school allows it)")
                        .on_hover_text(&settings)
                        .clicked()
                    {
                        let _ = crate::open_url(&self.cfg.opener, &settings);
                    }
                    if secret_row(
                        ui,
                        "Cookies",
                        &mut f.canvas_cookies,
                        "canvas_session=…",
                        idle,
                    ) {
                        job = Some(AuthJob::CanvasCookies {
                            url: f.canvas_url.clone(),
                            cookies: f.canvas_cookies.clone(),
                        });
                    }
                    ui.label(
                        RichText::new(
                            "Cookies: copy canvas_session from a signed-in browser \
                             (DevTools > Storage or Application > Cookies).",
                        )
                        .small()
                        .weak(),
                    );
                });

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);

            // Gradescope
            account_header(
                ui,
                "Gradescope",
                self.creds.gradescope.as_ref().map(|g| g.email.clone()),
            );
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Pick “School Credentials” for WashU SSO.")
                        .small()
                        .weak(),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if self.creds.gradescope.is_some() && ui.button("Sign out").clicked() {
                        sign_out = Some(Source::Gradescope);
                    }
                    let browser = egui::Button::new(RichText::new("Sign in with browser").strong());
                    if ui.add_enabled(idle, browser).clicked() {
                        job = Some(AuthJob::Browser(Site::Gradescope));
                    }
                });
            });
            egui::CollapsingHeader::new("Other ways to sign in")
                .id_salt("gs-other")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add_sized([96.0, 20.0], egui::Label::new("Email"));
                        ui.add(TextEdit::singleline(&mut f.gs_email).desired_width(240.0));
                    });
                    let has_email = !f.gs_email.trim().is_empty();
                    if secret_row(ui, "Password", &mut f.gs_password, "", idle && has_email) {
                        job = Some(AuthJob::GradescopePassword {
                            email: f.gs_email.clone(),
                            password: f.gs_password.clone(),
                        });
                    }
                    let hint = "signed_token=…; _gradescope_session=…";
                    if secret_row(ui, "Cookies", &mut f.gs_cookies, hint, idle) {
                        job = Some(AuthJob::GradescopeCookies(f.gs_cookies.clone()));
                    }
                });

            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                if f.busy {
                    ui.add(egui::Spinner::new());
                    ui.label("Finish signing in in the other window…");
                } else if let Some((ok, msg)) = &f.message {
                    let color = if *ok {
                        Color32::from_rgb(60, 170, 90)
                    } else {
                        Color32::from_rgb(220, 80, 80)
                    };
                    ui.add(egui::Label::new(RichText::new(msg).color(color)).wrap());
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Done").clicked() {
                        close = true;
                    }
                });
            });
        });
        if let Some(job) = job {
            self.run_auth(job);
        }
        if let Some(source) = sign_out {
            self.sign_out(source);
        }
        if (close || modal.should_close()) && !self.form.busy {
            self.accounts_open = false;
        }
    }
}

/// A masked input with a "Sign in" button. True when submitted (click or Enter).
fn secret_row(ui: &mut Ui, label: &str, value: &mut String, hint: &str, enabled: bool) -> bool {
    ui.horizontal(|ui| {
        ui.add_sized([96.0, 20.0], egui::Label::new(label));
        let field = ui.add(
            TextEdit::singleline(value)
                .password(true)
                .hint_text(hint)
                .desired_width(240.0),
        );
        let ready = enabled && !value.trim().is_empty();
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        let clicked = ui
            .add_enabled(ready, egui::Button::new("Sign in"))
            .clicked();
        ready && (clicked || enter)
    })
    .inner
}

impl eframe::App for Gui {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
        self.maybe_auto_sync();
        // Keeps the "due in" times and the sync label current.
        ui.ctx().request_repaint_after(StdDuration::from_secs(30));
        if ui.input(|i| i.key_pressed(Key::F5)) && !self.syncing {
            self.errors.clear();
            self.start_sync();
        }

        egui::Panel::top("toolbar")
            .frame(Frame::side_top_panel(ui.style()).inner_margin(Margin::symmetric(14, 6)))
            .show(ui, |ui| self.toolbar(ui));
        egui::CentralPanel::default()
            .frame(Frame::central_panel(ui.style()).inner_margin(Margin::symmetric(14, 8)))
            .show(ui, |ui| {
                if !self.errors.is_empty() {
                    self.error_banner(ui);
                    ui.add_space(6.0);
                }
                self.undo_bar(ui);
                self.list(ui);
            });
        if self.accounts_open {
            let ctx = ui.ctx().clone();
            self.accounts(&ctx);
        }
    }
}

fn account_header(ui: &mut Ui, name: &str, signed_in_as: Option<String>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(name).size(16.0).strong());
        match signed_in_as {
            Some(who) => ui.label(
                RichText::new(format!("signed in · {who}")).color(Color32::from_rgb(60, 170, 90)),
            ),
            None => ui.label(RichText::new("not signed in").weak()),
        };
    });
    ui.add_space(2.0);
}

/// Sort rank and heading for the day group an item belongs to.
fn section_of(a: &Assignment, now: DateTime<Utc>) -> (u8, String) {
    let Some(due) = a.due else {
        return (3, "No due date".into());
    };
    if due < now && !a.status.is_done() {
        return (1, "Overdue".into());
    }
    let today = now.with_timezone(&Local).date_naive();
    let day = due.with_timezone(&Local).date_naive();
    let title = match (day - today).num_days() {
        ..0 => return (0, "Earlier".into()),
        0 => "Today".into(),
        1 => "Tomorrow".into(),
        2..=6 => day.format("%A").to_string(),
        _ => day.format("%A, %B %-d").to_string(),
    };
    (2, title)
}

fn urgency_color(u: Urgency) -> Color32 {
    match u {
        Urgency::Overdue => Color32::from_rgb(229, 72, 77),
        Urgency::Day => Color32::from_rgb(240, 110, 50),
        Urgency::Soon => Color32::from_rgb(232, 170, 40),
        Urgency::Later => Color32::from_rgb(90, 140, 230),
        Urgency::Undated => Color32::from_rgb(140, 140, 150),
        Urgency::Done => Color32::from_rgb(70, 170, 100),
    }
}

fn source_color(s: Source) -> Color32 {
    match s {
        Source::Canvas => Color32::from_rgb(226, 75, 60),
        Source::Gradescope => Color32::from_rgb(30, 160, 160),
    }
}

fn status_chip(ui: &mut Ui, status: &Status) {
    let (fg, bg) = match status {
        Status::Missing => (Color32::WHITE, Color32::from_rgb(210, 60, 65)),
        Status::Late => (Color32::BLACK, Color32::from_rgb(235, 180, 60)),
        Status::Submitted | Status::Graded { .. } | Status::Excused => {
            (Color32::WHITE, Color32::from_rgb(55, 150, 85))
        }
        Status::Todo | Status::Offline => (
            ui.visuals().text_color(),
            ui.visuals().widgets.inactive.bg_fill,
        ),
    };
    // Painted by hand so the pill hugs the text instead of filling the row height.
    let galley = ui
        .painter()
        .layout_no_wrap(status.label(), FontId::proportional(12.5), fg);
    let size = galley.size() + vec2(18.0, 6.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(10), bg);
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, fg);
}

fn due_text(a: &Assignment, now: DateTime<Utc>) -> (String, String) {
    let Some(due) = a.due else {
        return ("No due date".into(), String::new());
    };
    let left = model::format_left(due, now);
    let rel = match left.strip_prefix('-') {
        Some(ago) if a.status.is_done() => format!("{ago} ago"),
        Some(ago) => format!("{ago} overdue"),
        None => format!("in {left}"),
    };
    let abs = due
        .with_timezone(&Local)
        .format("%a %b %-d · %-I:%M %p")
        .to_string();
    (rel, abs)
}

/// One clickable assignment card. The bool is true when its Hide/Restore
/// button was clicked.
fn row(ui: &mut Ui, a: &Assignment, now: DateTime<Utc>, is_hidden: bool) -> (Response, bool) {
    const HEIGHT: f32 = 40.0;
    let mut toggle = false;
    let urgency = a.urgency(now);
    let accent = urgency_color(urgency);
    let done = a.status.is_done();

    let builder = UiBuilder::new()
        .id_salt((a.source, &a.id))
        .sense(Sense::click());
    let resp = ui
        .scope_builder(builder, |ui| {
            if is_hidden {
                ui.multiply_opacity(0.55);
            }
            let hovered = ui.response().hovered() || ui.response().contains_pointer();
            let fill = if hovered {
                ui.visuals().widgets.hovered.weak_bg_fill
            } else {
                ui.visuals().faint_bg_color
            };
            let frame = Frame::new()
                .fill(fill)
                .corner_radius(8)
                .inner_margin(Margin {
                    left: 16,
                    right: 12,
                    top: 6,
                    bottom: 6,
                })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_height(HEIGHT);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (rel, abs) = due_text(a, now);
                        ui.allocate_ui_with_layout(
                            vec2(150.0, HEIGHT),
                            Layout::top_down(Align::Max),
                            |ui| {
                                ui.set_min_width(150.0);
                                let mut rel = RichText::new(rel).strong();
                                rel = if done { rel.weak() } else { rel.color(accent) };
                                ui.label(rel);
                                ui.label(RichText::new(abs).small().weak());
                            },
                        );
                        ui.allocate_ui_with_layout(
                            vec2(130.0, HEIGHT),
                            Layout::right_to_left(Align::Center),
                            |ui| {
                                ui.set_min_width(130.0);
                                status_chip(ui, &a.status);
                            },
                        );
                        // Only shown on hover (always for hidden items), but the
                        // space is kept so names don't shift around.
                        ui.allocate_ui_with_layout(
                            vec2(70.0, HEIGHT),
                            Layout::right_to_left(Align::Center),
                            |ui| {
                                ui.set_min_width(70.0);
                                if hovered || is_hidden {
                                    let (label, tip) = if is_hidden {
                                        ("Restore", "Put this back in the list")
                                    } else {
                                        ("Hide", "Remove this from the list")
                                    };
                                    toggle = ui.small_button(label).on_hover_text(tip).clicked();
                                }
                            },
                        );
                        ui.with_layout(Layout::top_down(Align::Min), |ui| {
                            let mut name = RichText::new(&a.name).size(15.0).strong();
                            if done {
                                name = name.weak();
                            }
                            ui.add(egui::Label::new(name).truncate());
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                ui.label(
                                    RichText::new(a.source.label())
                                        .small()
                                        .strong()
                                        .color(source_color(a.source)),
                                );
                                ui.label(RichText::new("·").small().weak());
                                ui.add(
                                    egui::Label::new(RichText::new(&a.course).small().weak())
                                        .truncate(),
                                );
                            });
                        });
                    });
                });
            // Urgency stripe down the left edge.
            let r = frame.response.rect;
            ui.painter().rect_filled(
                Rect::from_min_size(r.min, vec2(5.0, r.height())),
                CornerRadius {
                    nw: 8,
                    sw: 8,
                    ne: 0,
                    se: 0,
                },
                accent,
            );
        })
        .response;
    ui.add_space(2.0);

    let resp = if a.url.is_some() {
        resp.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        resp
    };
    let resp = match &a.url {
        Some(url) => resp.on_hover_text_at_pointer(format!("Open {url}")),
        None => resp,
    };
    (resp, toggle)
}
