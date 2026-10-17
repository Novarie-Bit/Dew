mod auth;
mod canvas;
mod config;
mod gradescope;
mod gui;
mod model;
mod sync;
mod tui;

use std::io::{self, BufRead, IsTerminal, Write};

use anyhow::{Result, bail};
use chrono::{Duration, Utc};
use clap::{Parser, Subcommand, ValueEnum};

use config::{Config, Credentials, DEFAULT_CANVAS_URL, Paths};
use model::{Assignment, Urgency};
use sync::Cache;

/// Canvas + Gradescope assignment tracker.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the window (the default when a display is available).
    Gui,
    /// Open the terminal view (the default without a display).
    Tui,
    /// Fetch everything and print what's due.
    List {
        /// Include finished and long-past items.
        #[arg(short, long)]
        all: bool,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
        /// Use the last fetched data instead of syncing.
        #[arg(short, long)]
        cached: bool,
    },
    /// Fetch everything and update the cache.
    Sync,
    /// JSON for a Waybar custom module.
    Waybar,
    /// Sign in to a service.
    Login {
        service: Service,
        /// Canvas base URL.
        #[arg(long, default_value = DEFAULT_CANVAS_URL)]
        url: String,
        /// Gradescope: paste browser cookies instead of using a password (for SSO accounts).
        #[arg(long)]
        cookies: bool,
    },
    /// Forget saved credentials.
    Logout { service: Option<Service> },
    /// Show sign-in state and file locations.
    Status,
}

#[derive(Clone, Copy, ValueEnum)]
enum Service {
    Canvas,
    Gradescope,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::new()?;
    let cfg = Config::load(&paths)?;
    let has_display = ["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|x| !x.is_empty()));
    let default = if has_display {
        Command::Gui
    } else {
        Command::Tui
    };
    match cli.command.unwrap_or(default) {
        Command::Gui => gui::run(cfg, paths),
        Command::Tui => tui::run(cfg, paths),
        Command::List { all, json, cached } => list(&cfg, &paths, all, json, cached),
        Command::Sync => {
            let report = sync::sync(&cfg, &paths);
            for e in &report.errors {
                eprintln!("error: {e}");
            }
            println!("{} assignments fetched", report.cache.assignments.len());
            Ok(())
        }
        Command::Waybar => waybar(&cfg, &paths),
        Command::Login {
            service,
            url,
            cookies,
        } => login(&paths, service, &url, cookies),
        Command::Logout { service } => {
            if matches!(service, None | Some(Service::Canvas)) {
                auth::logout_canvas(&paths)?;
                println!("Signed out of Canvas");
            }
            if matches!(service, None | Some(Service::Gradescope)) {
                auth::logout_gradescope(&paths)?;
                println!("Signed out of Gradescope");
            }
            Ok(())
        }
        Command::Status => status(&paths),
    }
}

/// Opens `url` with the configured opener (xdg-open by default), detached.
pub fn open_url(opener: &str, url: &str) -> io::Result<()> {
    std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        // Reap the child so a long-running GUI doesn't collect zombies.
        .map(|mut child| drop(std::thread::spawn(move || child.wait())))
}

/// Turns connection failures into a short message; other errors keep their detail.
pub fn network_error(e: reqwest::Error, site: &str) -> anyhow::Error {
    let host = site
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    if e.is_connect() {
        anyhow::anyhow!("couldn't connect to {host} (offline?)")
    } else if e.is_timeout() {
        anyhow::anyhow!("{host} took too long to respond")
    } else {
        anyhow::Error::new(e).context(format!("requesting {host}"))
    }
}

/// Builder shared by both services.
pub fn http_client() -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder()
        .user_agent(concat!(
            "dew/",
            env!("CARGO_PKG_VERSION"),
            " (assignment tracker)"
        ))
        .timeout(std::time::Duration::from_secs(30))
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn login(paths: &Paths, service: Service, url: &str, cookies: bool) -> Result<()> {
    match service {
        Service::Canvas => {
            let url = canvas::normalize_url(url);
            println!("Create a token at {url}/profile/settings -> \"+ New Access Token\".");
            let token = rpassword::prompt_password("Canvas access token: ")?;
            let user = auth::login_canvas(paths, &url, &token)?;
            println!("Signed in to Canvas as {user}");
        }
        Service::Gradescope if cookies => {
            println!(
                "In a browser signed in to gradescope.com, open DevTools -> Storage/Application -> \
                 Cookies and copy signed_token and _gradescope_session."
            );
            let c = rpassword::prompt_password(
                "Cookies (signed_token=...; _gradescope_session=...): ",
            )?;
            auth::login_gradescope_cookies(paths, &c)?;
            println!("Signed in to Gradescope");
        }
        Service::Gradescope => {
            let email = prompt("Gradescope email: ")?;
            let password = rpassword::prompt_password("Gradescope password: ")?;
            auth::login_gradescope(paths, &email, &password)?;
            println!("Signed in to Gradescope as {email}");
        }
    }
    Ok(())
}

fn status(paths: &Paths) -> Result<()> {
    let creds = Credentials::load(paths)?;
    match &creds.canvas {
        Some(c) => println!("Canvas:     {} at {}", c.user, c.url),
        None => println!("Canvas:     not signed in"),
    }
    match &creds.gradescope {
        Some(g) => println!("Gradescope: {}", g.email),
        None => println!("Gradescope: not signed in"),
    }
    let cache = Cache::load(paths);
    if let Some(t) = cache.fetched_at {
        println!(
            "Last sync:  {} ({} items)",
            model::format_due(t),
            cache.assignments.len()
        );
    }
    println!();
    println!("Config:      {}", paths.config.display());
    println!("Credentials: {}", paths.credentials.display());
    println!("Cookies:     {}", paths.gradescope_cookies.display());
    println!("Cache:       {}", paths.cache.display());
    Ok(())
}

fn load_or_sync(cfg: &Config, paths: &Paths, cached: bool) -> Result<Cache> {
    if cached {
        return Ok(Cache::load(paths));
    }
    let creds = Credentials::load(paths)?;
    if creds.canvas.is_none() && creds.gradescope.is_none() {
        bail!("not signed in to anything yet; run `dew login canvas` or `dew login gradescope`");
    }
    let report = sync::sync(cfg, paths);
    for e in &report.errors {
        eprintln!("error: {e}");
    }
    Ok(report.cache)
}

fn visible(cache: &Cache, cfg: &Config, all: bool) -> Vec<Assignment> {
    let now = Utc::now();
    cache
        .assignments
        .iter()
        .filter(|a| all || (!a.status.is_done() && a.is_current(now, cfg.keep_past_days)))
        .cloned()
        .collect()
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        format!("{s:width$}")
    } else {
        let t: String = s.chars().take(width - 1).collect();
        format!("{t}…")
    }
}

fn list(cfg: &Config, paths: &Paths, all: bool, json: bool, cached: bool) -> Result<()> {
    let cache = load_or_sync(cfg, paths, cached)?;
    let items = visible(&cache, cfg, all);
    if json {
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }
    if items.is_empty() {
        println!("Nothing due. 🎉");
        return Ok(());
    }
    let color = io::stdout().is_terminal();
    let now = Utc::now();
    for a in &items {
        let (due, left) = match a.due {
            Some(d) => (model::format_due(d), model::format_left(d, now)),
            None => ("no due date".into(), String::new()),
        };
        let line = format!(
            "{:<16} {:>7}  {}  {} {}  {}",
            due,
            left,
            a.source.short(),
            truncate(&a.course, 14),
            truncate(&a.name, 42),
            a.status.label()
        );
        let code = match a.urgency(now) {
            Urgency::Overdue => "31",
            Urgency::Day => "1;31",
            Urgency::Soon => "33",
            Urgency::Done => "2",
            _ => "",
        };
        if color && !code.is_empty() {
            println!("\x1b[{code}m{line}\x1b[0m");
        } else {
            println!("{line}");
        }
    }
    Ok(())
}

fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn waybar(cfg: &Config, paths: &Paths) -> Result<()> {
    let mut cache = Cache::load(paths);
    let stale = cache
        .fetched_at
        .is_none_or(|t| Utc::now() - t > Duration::minutes(cfg.refresh_minutes));
    if stale {
        cache = sync::sync(cfg, paths).cache;
    }
    let now = Utc::now();
    let items = visible(&cache, cfg, false);
    let week: Vec<&Assignment> = items
        .iter()
        .filter(|a| a.due.is_some_and(|d| d - now < Duration::days(7)))
        .collect();
    let worst = week.iter().map(|a| a.urgency(now)).min();
    let class = match worst {
        Some(Urgency::Overdue | Urgency::Day) => "urgent",
        Some(Urgency::Soon) => "warning",
        Some(_) => "normal",
        None => "empty",
    };
    let tooltip = if week.is_empty() {
        "Nothing due this week".to_string()
    } else {
        week.iter()
            .take(15)
            .map(|a| {
                let left = a
                    .due
                    .map(|d| model::format_left(d, now))
                    .unwrap_or_default();
                escape_markup(&format!("{left:>7}  {}: {}", a.course, a.name))
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let out = serde_json::json!({
        "text": week.len().to_string(),
        "alt": class,
        "class": class,
        "tooltip": tooltip,
    });
    println!("{out}");
    Ok(())
}
