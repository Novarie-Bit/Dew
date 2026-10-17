use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

pub const DEFAULT_CANVAS_URL: &str = "https://canvas.wustl.edu";

#[derive(Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub credentials: PathBuf,
    pub gradescope_cookies: PathBuf,
    pub canvas_cookies: PathBuf,
    /// Browser profile for the sign-in window (keeps SSO / Duo "remember me").
    pub webview_data: PathBuf,
    pub cache: PathBuf,
    pub hidden: PathBuf,
}

impl Paths {
    pub fn new() -> Result<Self> {
        let dirs =
            ProjectDirs::from("", "", "dew").context("could not determine home directory")?;
        Ok(Paths {
            config: dirs.config_dir().join("config.toml"),
            credentials: dirs.config_dir().join("credentials.toml"),
            gradescope_cookies: dirs.data_dir().join("gradescope_cookies.json"),
            canvas_cookies: dirs.data_dir().join("canvas_cookies.json"),
            webview_data: dirs.data_dir().join("webview"),
            cache: dirs.cache_dir().join("assignments.json"),
            hidden: dirs.data_dir().join("hidden.json"),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Keep showing items whose due date passed less than this many days ago.
    pub keep_past_days: i64,
    /// Hide Canvas items that mirror a Gradescope assignment.
    pub dedupe: bool,
    /// Fetch every Gradescope term, not just the most recent one.
    pub gradescope_all_terms: bool,
    /// The TUI and `dew waybar` re-sync when data is older than this.
    pub refresh_minutes: i64,
    /// Command used to open links.
    pub opener: String,
    /// GUI theme: "system", "light" or "dark".
    pub theme: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            keep_past_days: 14,
            dedupe: true,
            gradescope_all_terms: false,
            refresh_minutes: 30,
            opener: "xdg-open".into(),
            theme: "system".into(),
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        match fs::read_to_string(&paths.config) {
            Ok(s) => {
                toml::from_str(&s).with_context(|| format!("parsing {}", paths.config.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Config::default();
                // Write the defaults out so they're easy to discover and edit.
                let _ = write_file(&paths.config, &toml::to_string_pretty(&cfg)?, 0o644);
                Ok(cfg)
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", paths.config.display())),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CanvasCreds {
    pub url: String,
    /// Empty when signed in through the browser (session cookies).
    #[serde(default)]
    pub token: String,
    /// Display name reported by Canvas when the token was verified.
    #[serde(default)]
    pub user: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GradescopeCreds {
    /// Only for display; the session itself lives in the cookie jar.
    pub email: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Credentials {
    pub canvas: Option<CanvasCreds>,
    pub gradescope: Option<GradescopeCreds>,
}

impl Credentials {
    pub fn load(paths: &Paths) -> Result<Self> {
        match fs::read_to_string(&paths.credentials) {
            Ok(s) => toml::from_str(&s)
                .with_context(|| format!("parsing {}", paths.credentials.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Credentials::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", paths.credentials.display())),
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_file(&paths.credentials, &toml::to_string_pretty(self)?, 0o600)
    }
}

/// Write `contents` to `path` with the given mode, creating parent dirs.
/// The mode is enforced even when the file already exists.
pub fn write_file(path: &Path, contents: &str, mode: u32) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.set_permissions(fs::Permissions::from_mode(mode))?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}
