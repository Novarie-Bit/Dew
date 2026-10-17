use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration as StdDuration;

use anyhow::Result;
use chrono::{Duration, Utc};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::auth;
use crate::config::{Config, Credentials, DEFAULT_CANVAS_URL, Paths};
use crate::model::{self, Assignment, Filter, Source, Status, Urgency};
use crate::sync::{self, Cache, Report};

enum Msg {
    Synced(Report),
    LoggedIn { ok: bool, text: String },
}

#[derive(PartialEq)]
enum Mode {
    List,
    Search,
    Login,
    Help,
}

const FIELDS: [(&str, bool); 5] = [
    ("Canvas URL", false),
    ("Canvas access token", true),
    ("Gradescope email", false),
    ("Gradescope password", true),
    ("…or Gradescope cookies", true),
];
const URL: usize = 0;
const TOKEN: usize = 1;
const EMAIL: usize = 2;
const PASSWORD: usize = 3;
const COOKIES: usize = 4;

struct LoginForm {
    fields: [String; 5],
    focus: usize,
    busy: bool,
    message: Option<(bool, String)>,
}

struct App {
    cfg: Config,
    paths: Paths,
    creds: Credentials,
    cache: Cache,
    view: Vec<usize>,
    table: TableState,
    filter: Filter,
    mode: Mode,
    form: LoginForm,
    syncing: bool,
    errors: Vec<String>,
    flash: Option<String>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    quit: bool,
}

pub fn run(cfg: Config, paths: Paths) -> Result<()> {
    let mut app = App::new(cfg, paths);
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let result = app.main_loop(&mut terminal);
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

impl App {
    fn new(cfg: Config, paths: Paths) -> App {
        let creds = Credentials::load(&paths).unwrap_or_default();
        let cache = Cache::load(&paths);
        let (tx, rx) = channel();
        let form = LoginForm {
            fields: Default::default(),
            focus: TOKEN,
            busy: false,
            message: None,
        };
        let mut app = App {
            cfg,
            paths,
            creds,
            cache,
            view: Vec::new(),
            table: TableState::default(),
            filter: Filter::default(),
            mode: Mode::List,
            form,
            syncing: false,
            errors: Vec::new(),
            flash: None,
            tx,
            rx,
            quit: false,
        };
        app.rebuild_view();
        if app.signed_in() {
            app.start_sync();
        } else {
            app.open_login();
        }
        app
    }

    fn signed_in(&self) -> bool {
        self.creds.canvas.is_some() || self.creds.gradescope.is_some()
    }

    fn main_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            while let Ok(msg) = self.rx.try_recv() {
                self.handle_msg(msg);
            }
            self.maybe_auto_sync();
            terminal.draw(|f| self.draw(f))?;
            if event::poll(StdDuration::from_millis(250))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => self.on_key(k),
                    Event::Paste(s) if self.mode == Mode::Login => {
                        self.form.fields[self.form.focus].push_str(s.trim());
                    }
                    Event::Paste(s) if self.mode == Mode::Search => {
                        self.filter.query.push_str(&s);
                        self.rebuild_view();
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn maybe_auto_sync(&mut self) {
        if self.syncing || !self.signed_in() {
            return;
        }
        let stale = self
            .cache
            .fetched_at
            .is_none_or(|t| Utc::now() - t > Duration::minutes(self.cfg.refresh_minutes));
        if stale && self.errors.is_empty() {
            self.start_sync();
        }
    }

    fn start_sync(&mut self) {
        if self.syncing {
            return;
        }
        self.syncing = true;
        let tx = self.tx.clone();
        let cfg = self.cfg.clone();
        let paths = self.paths.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Synced(sync::sync(&cfg, &paths)));
        });
    }

    fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Synced(report) => {
                self.syncing = false;
                self.errors = report.errors;
                self.cache = report.cache;
                self.rebuild_view();
            }
            Msg::LoggedIn { ok, text } => {
                self.form.busy = false;
                self.creds = Credentials::load(&self.paths).unwrap_or_default();
                for i in [TOKEN, PASSWORD, COOKIES] {
                    self.form.fields[i].clear();
                }
                if ok {
                    self.flash = Some(text);
                    self.errors.clear();
                    self.mode = Mode::List;
                    self.start_sync();
                } else {
                    self.form.message = Some((false, text));
                }
            }
        }
    }

    fn rebuild_view(&mut self) {
        let selected_id = self.selected().map(|a| (a.source, a.id.clone()));
        let now = Utc::now();
        self.view = self
            .cache
            .assignments
            .iter()
            .enumerate()
            .filter(|(_, a)| self.filter.keep(a, now, self.cfg.keep_past_days))
            .map(|(i, _)| i)
            .collect();
        let pos = selected_id
            .and_then(|(src, id)| {
                self.view.iter().position(|&i| {
                    let a = &self.cache.assignments[i];
                    a.source == src && a.id == id
                })
            })
            .or(if self.view.is_empty() { None } else { Some(0) });
        self.table.select(pos);
    }

    fn selected(&self) -> Option<&Assignment> {
        let i = *self.view.get(self.table.selected()?)?;
        self.cache.assignments.get(i)
    }

    fn move_by(&mut self, delta: isize) {
        if self.view.is_empty() {
            return;
        }
        let cur = self.table.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, self.view.len() as isize - 1);
        self.table.select(Some(next as usize));
    }

    fn open_selected(&mut self) {
        let Some(url) = self.selected().and_then(|a| a.url.clone()) else {
            self.flash = Some("No link for this item".into());
            return;
        };
        self.flash = Some(match crate::open_url(&self.cfg.opener, &url) {
            Ok(_) => format!("Opened {url}"),
            Err(e) => format!("Couldn't run {}: {e}", self.cfg.opener),
        });
    }

    fn open_login(&mut self) {
        let f = &mut self.form;
        f.fields[URL] = self
            .creds
            .canvas
            .as_ref()
            .map(|c| c.url.clone())
            .unwrap_or_else(|| DEFAULT_CANVAS_URL.into());
        if let Some(g) = &self.creds.gradescope
            && !g.email.starts_with('(')
        {
            f.fields[EMAIL] = g.email.clone();
        }
        f.focus = if self.creds.canvas.is_none() {
            TOKEN
        } else {
            EMAIL
        };
        f.message = None;
        self.mode = Mode::Login;
    }

    fn submit_login(&mut self) {
        let [url, token, email, password, cookies] = self.form.fields.clone();
        let do_canvas = !token.trim().is_empty();
        let do_gs_password = !password.is_empty();
        let do_gs_cookies = !do_gs_password && !cookies.trim().is_empty();
        if !do_canvas && !do_gs_password && !do_gs_cookies {
            self.form.message = Some((
                false,
                "Enter a Canvas token and/or Gradescope email + password (or cookies).".into(),
            ));
            return;
        }
        self.form.busy = true;
        self.form.message = Some((true, "Signing in…".into()));
        let tx = self.tx.clone();
        let paths = self.paths.clone();
        std::thread::spawn(move || {
            let mut ok = true;
            let mut lines = Vec::new();
            if do_canvas {
                match auth::login_canvas(&paths, &url, &token) {
                    Ok(user) => lines.push(format!("Canvas: signed in as {user}")),
                    Err(e) => {
                        ok = false;
                        lines.push(format!("Canvas: {e:#}"));
                    }
                }
            }
            if do_gs_password || do_gs_cookies {
                let r = if do_gs_password {
                    auth::login_gradescope(&paths, &email, &password)
                } else {
                    auth::login_gradescope_cookies(&paths, &cookies)
                };
                match r {
                    Ok(()) => lines.push("Gradescope: signed in".into()),
                    Err(e) => {
                        ok = false;
                        lines.push(format!("Gradescope: {e:#}"));
                    }
                }
            }
            let _ = tx.send(Msg::LoggedIn {
                ok,
                text: lines.join("\n"),
            });
        });
    }

    fn on_key(&mut self, k: KeyEvent) {
        // Alt counts as a modifier too, so Alt-chords don't type into fields.
        let ctrl = k
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match self.mode {
            Mode::Help => self.mode = Mode::List,
            Mode::Search => match k.code {
                KeyCode::Esc => {
                    self.filter.query.clear();
                    self.mode = Mode::List;
                    self.rebuild_view();
                }
                KeyCode::Enter => self.mode = Mode::List,
                KeyCode::Backspace => {
                    self.filter.query.pop();
                    self.rebuild_view();
                }
                KeyCode::Char('u') if ctrl => {
                    self.filter.query.clear();
                    self.rebuild_view();
                }
                KeyCode::Char(c) if !ctrl => {
                    self.filter.query.push(c);
                    self.rebuild_view();
                }
                _ => {}
            },
            Mode::Login => self.on_login_key(k, ctrl),
            Mode::List => self.on_list_key(k, ctrl),
        }
    }

    fn on_list_key(&mut self, k: KeyEvent, ctrl: bool) {
        self.flash = None;
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc if !self.filter.query.is_empty() => {
                self.filter.query.clear();
                self.rebuild_view();
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Char('d') if ctrl => self.move_by(10),
            KeyCode::Char('u') if ctrl => self.move_by(-10),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::Home | KeyCode::Char('g') => self.move_by(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_by(isize::MAX / 2),
            KeyCode::Enter | KeyCode::Char('o') => self.open_selected(),
            KeyCode::Char('r') => {
                self.errors.clear();
                self.start_sync();
            }
            KeyCode::Char('d') => {
                self.filter.show_done = !self.filter.show_done;
                self.rebuild_view();
            }
            KeyCode::Char('p') => {
                self.filter.show_past = !self.filter.show_past;
                self.rebuild_view();
            }
            KeyCode::Char('s') | KeyCode::Tab => {
                self.filter.source = match self.filter.source {
                    None => Some(Source::Canvas),
                    Some(Source::Canvas) => Some(Source::Gradescope),
                    Some(Source::Gradescope) => None,
                };
                self.rebuild_view();
            }
            KeyCode::Char('/') => self.mode = Mode::Search,
            KeyCode::Char('L') | KeyCode::Char('l') => self.open_login(),
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
    }

    fn on_login_key(&mut self, k: KeyEvent, ctrl: bool) {
        if self.form.busy {
            return;
        }
        let f = &mut self.form;
        match k.code {
            KeyCode::Esc => self.mode = Mode::List,
            KeyCode::Tab | KeyCode::Down => f.focus = (f.focus + 1) % FIELDS.len(),
            KeyCode::BackTab | KeyCode::Up => f.focus = (f.focus + FIELDS.len() - 1) % FIELDS.len(),
            KeyCode::Enter => self.submit_login(),
            KeyCode::Backspace => {
                f.fields[f.focus].pop();
            }
            KeyCode::Char('u') if ctrl => f.fields[f.focus].clear(),
            KeyCode::Char('x') if ctrl => {
                let canvas = f.focus <= TOKEN;
                let r = if canvas {
                    auth::logout_canvas(&self.paths)
                } else {
                    auth::logout_gradescope(&self.paths)
                };
                let name = if canvas { "Canvas" } else { "Gradescope" };
                f.message = Some(match r {
                    Ok(()) => (true, format!("Signed out of {name}")),
                    Err(e) => (false, format!("{e:#}")),
                });
                self.creds = Credentials::load(&self.paths).unwrap_or_default();
                let src = if canvas {
                    Source::Canvas
                } else {
                    Source::Gradescope
                };
                self.cache.assignments.retain(|a| a.source != src);
                let _ = self.cache.save(&self.paths);
                self.rebuild_view();
            }
            KeyCode::Char(c) if !ctrl => f.fields[f.focus].push(c),
            _ => {}
        }
    }

    // ---- drawing ----

    fn draw(&mut self, f: &mut Frame) {
        let [header, body, detail, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(6),
            Constraint::Length(1),
        ])
        .areas(f.area());
        self.draw_header(f, header);
        self.draw_table(f, body);
        self.draw_detail(f, detail);
        self.draw_footer(f, footer);
        match self.mode {
            Mode::Login => self.draw_login(f),
            Mode::Help => draw_help(f),
            _ => {}
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let mut spans = vec![" dew ".bold().black().on_cyan(), " ".into()];
        let tag = |on: bool, label: &str| {
            let s = format!("[{label}] ");
            if on { s.cyan() } else { s.dark_gray() }
        };
        spans.push(tag(
            true,
            self.filter.source.map_or("all sources", |s| s.label()),
        ));
        spans.push(tag(
            self.filter.show_done,
            if self.filter.show_done {
                "+done"
            } else {
                "-done"
            },
        ));
        spans.push(tag(
            self.filter.show_past,
            if self.filter.show_past {
                "+past"
            } else {
                "-past"
            },
        ));
        if !self.filter.query.is_empty() || self.mode == Mode::Search {
            spans.push(format!("/{}", self.filter.query).yellow());
            if self.mode == Mode::Search {
                spans.push("█".yellow());
            }
            spans.push(" ".into());
        }
        let right = if self.syncing {
            "syncing…".to_string()
        } else if let Some(t) = self.cache.fetched_at {
            let mins = (Utc::now() - t).num_minutes();
            if mins < 1 {
                "synced just now".into()
            } else {
                format!("synced {mins}m ago")
            }
        } else {
            "never synced".into()
        };
        let left = Line::from(spans);
        let [l, r] = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(right.len() as u16 + 1),
        ])
        .areas(area);
        f.render_widget(left, l);
        f.render_widget(Line::from(right.dark_gray()).right_aligned(), r);
    }

    fn draw_table(&mut self, f: &mut Frame, area: Rect) {
        let now = Utc::now();
        if self.view.is_empty() {
            let msg = if !self.signed_in() {
                "Not signed in. Press L to sign in to Canvas and Gradescope."
            } else if self.syncing && self.cache.assignments.is_empty() {
                "Fetching assignments…"
            } else if self.cache.assignments.is_empty() {
                "No assignments found."
            } else {
                "Nothing due 🎉   (d: show done, p: show past)"
            };
            let p = Paragraph::new(msg).centered().block(Block::bordered());
            f.render_widget(p, area);
            return;
        }
        let rows = self.view.iter().map(|&i| {
            let a = &self.cache.assignments[i];
            let (due, left) = match a.due {
                Some(d) => (model::format_due(d), model::format_left(d, now)),
                None => ("—".into(), String::new()),
            };
            let style = match a.urgency(now) {
                Urgency::Done => Style::new().fg(Color::DarkGray),
                Urgency::Overdue => Style::new().fg(Color::Red),
                Urgency::Day => Style::new()
                    .fg(Color::LightRed)
                    .add_modifier(Modifier::BOLD),
                Urgency::Soon => Style::new().fg(Color::Yellow),
                Urgency::Later | Urgency::Undated => Style::new(),
            };
            let status_style = match a.status {
                Status::Missing => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                Status::Graded { .. } | Status::Submitted | Status::Excused => {
                    Style::new().fg(Color::Green)
                }
                Status::Late => Style::new().fg(Color::Yellow),
                _ => Style::new(),
            };
            let src = match a.source {
                Source::Canvas => a.source.short().red(),
                Source::Gradescope => a.source.short().cyan(),
            };
            Row::new(vec![
                Cell::from(due),
                Cell::from(Line::from(left).right_aligned()),
                Cell::from(src),
                Cell::from(a.course.clone()),
                Cell::from(a.name.clone()),
                Cell::from(a.status.label()).style(status_style),
            ])
            .style(style)
        });
        let header = Row::new(["Due", "Left", "", "Course", "Assignment", "Status"])
            .style(Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
        let widths = [
            Constraint::Length(16),
            Constraint::Length(7),
            Constraint::Length(2),
            Constraint::Max(18),
            Constraint::Fill(1),
            Constraint::Length(16),
        ];
        let title = format!(" {} of {} ", self.view.len(), self.cache.assignments.len());
        let table = Table::new(rows, widths)
            .header(header)
            .column_spacing(1)
            .block(Block::bordered().title(title))
            .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▌");
        f.render_stateful_widget(table, area, &mut self.table);
    }

    fn draw_detail(&self, f: &mut Frame, area: Rect) {
        let block = Block::bordered().title(" details ");
        let Some(a) = self.selected() else {
            f.render_widget(block, area);
            return;
        };
        let now = Utc::now();
        let due = match a.due {
            Some(d) => format!("{} ({})", model::format_due(d), model::format_left(d, now)),
            None => "no due date".into(),
        };
        let mut when = vec!["Due ".dark_gray(), due.into()];
        if let Some(l) = a.late_due {
            when.push("   Late until ".dark_gray());
            when.push(model::format_due(l).into());
        }
        let mut meta = vec![
            a.course.clone().into(),
            " · ".dark_gray(),
            a.source.label().into(),
            " · ".dark_gray(),
            a.status.label().into(),
        ];
        if let (Some(p), false) = (a.points, matches!(a.status, Status::Graded { .. })) {
            meta.push(" · ".dark_gray());
            meta.push(format!("{p} pts").into());
        }
        let text = Text::from(vec![
            Line::from(a.name.clone().bold()),
            Line::from(meta),
            Line::from(when),
            Line::from(a.url.clone().unwrap_or_default().dark_gray()),
        ]);
        f.render_widget(
            Paragraph::new(text).block(block).wrap(Wrap { trim: true }),
            area,
        );
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let line = if let Some(e) = self.errors.first() {
            let more = if self.errors.len() > 1 {
                format!(" (+{} more)", self.errors.len() - 1)
            } else {
                String::new()
            };
            Line::from(vec![
                " ! ".bold().white().on_red(),
                format!(" {e}{more}").red(),
            ])
        } else if let Some(msg) = &self.flash {
            Line::from(format!(" {msg}").green())
        } else {
            let keys = [
                ("j/k", "move"),
                ("o", "open"),
                ("r", "refresh"),
                ("d", "done"),
                ("p", "past"),
                ("s", "source"),
                ("/", "search"),
                ("L", "accounts"),
                ("?", "help"),
                ("q", "quit"),
            ];
            let mut spans = vec![" ".into()];
            for (k, v) in keys {
                spans.push(k.bold().cyan());
                spans.push(format!(" {v}  ").dark_gray());
            }
            Line::from(spans)
        };
        f.render_widget(line, area);
    }

    fn draw_login(&self, f: &mut Frame) {
        let area = f
            .area()
            .centered(Constraint::Max(76), Constraint::Length(22));
        f.render_widget(Clear, area);
        let block = Block::bordered().title(" accounts ").title_bottom(
            Line::from(" Enter sign in · Tab next · Ctrl-U clear · Ctrl-X sign out · Esc close ")
                .centered(),
        );
        let inner = block.inner(area);
        f.render_widget(block, area);

        let check = |ok: bool| if ok { "✓ ".green() } else { "✗ ".red() };
        let canvas_state = match &self.creds.canvas {
            Some(c) => Line::from(vec![
                check(true),
                format!("Canvas: {} ({})", c.user, c.url).into(),
            ]),
            None => Line::from(vec![check(false), "Canvas: not signed in".into()]),
        };
        let gs_state = match &self.creds.gradescope {
            Some(g) => Line::from(vec![check(true), format!("Gradescope: {}", g.email).into()]),
            None => Line::from(vec![check(false), "Gradescope: not signed in".into()]),
        };

        let mut lines = vec![canvas_state, gs_state, Line::default()];
        for (i, (label, secret)) in FIELDS.iter().enumerate() {
            if i == EMAIL {
                lines.push(Line::default());
            }
            let focused = i == self.form.focus;
            let value = &self.form.fields[i];
            let shown = if *secret {
                "•".repeat(value.chars().count().min(40))
            } else {
                value.clone()
            };
            let label = format!(" {label:<24}");
            let mut spans = vec![if focused {
                label.bold().yellow()
            } else {
                label.into()
            }];
            spans.push(shown.into());
            if focused {
                spans.push("█".yellow());
            }
            lines.push(Line::from(spans));
            if i == TOKEN {
                let hint = format!(
                    "   create one at {}/profile/settings → New Access Token",
                    crate::canvas::normalize_url(&self.form.fields[URL])
                );
                lines.push(Line::from(hint.dark_gray()));
            }
        }
        lines.push(Line::from(
            "   SSO account? Set a password via Gradescope's \"Forgot password\",".dark_gray(),
        ));
        lines.push(Line::from(
            "   or paste browser cookies: signed_token=…; _gradescope_session=…".dark_gray(),
        ));
        lines.push(Line::default());
        if let Some((ok, msg)) = &self.form.message {
            for l in msg.lines() {
                lines.push(Line::from(if *ok {
                    l.to_string().green()
                } else {
                    l.to_string().red()
                }));
            }
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}

fn draw_help(f: &mut Frame) {
    let area = f
        .area()
        .centered(Constraint::Max(60), Constraint::Length(20));
    f.render_widget(Clear, area);
    let rows = [
        ("j / k, ↑ / ↓", "move"),
        ("g / G", "first / last"),
        ("Ctrl-D / Ctrl-U", "page down / up"),
        ("Enter, o", "open in browser"),
        ("r", "refresh now"),
        ("d", "show / hide finished work"),
        ("p", "show / hide long-past items"),
        ("s, Tab", "cycle Canvas / Gradescope / all"),
        ("/", "search (Esc clears)"),
        ("L", "accounts: sign in / out"),
        ("q, Ctrl-C", "quit"),
    ];
    let mut lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| Line::from(vec![format!("  {k:<18}").bold().cyan(), (*v).into()]))
        .collect();
    lines.push(Line::default());
    lines.push(Line::from(
        "  Colors: red = overdue, bold red = <24h, yellow = <3 days".dark_gray(),
    ));
    lines.push(Line::from("  Press any key to close".dark_gray()));
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" help ")),
        area,
    );
}
