//! A cookie jar persisted to disk, used for browser-style sessions.

use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::{Arc, MutexGuard};

use anyhow::{Result, anyhow, bail};
use reqwest::Url;
use reqwest_cookie_store::{CookieStore, CookieStoreMutex};

use crate::config::write_file;

pub struct Jar {
    store: Arc<CookieStoreMutex>,
    path: PathBuf,
}

/// One cookie to add to the jar.
pub struct NewCookie {
    pub name: String,
    pub value: String,
    /// Leave empty to scope the cookie to the site's host.
    pub domain: String,
}

impl Jar {
    pub fn load(path: &Path) -> Result<Jar> {
        let store = match fs::File::open(path) {
            Ok(f) => cookie_store::serde::json::load_all(BufReader::new(f))
                .map_err(|e| anyhow!("reading {}: {e}", path.display()))?,
            Err(_) => CookieStore::new(),
        };
        Ok(Jar {
            store: Arc::new(CookieStoreMutex::new(store)),
            path: path.to_path_buf(),
        })
    }

    pub fn provider(&self) -> Arc<CookieStoreMutex> {
        self.store.clone()
    }

    fn lock(&self) -> Result<MutexGuard<'_, CookieStore>> {
        self.store
            .lock()
            .map_err(|_| anyhow!("cookie jar poisoned"))
    }

    pub fn has_any(&self) -> bool {
        self.lock()
            .map(|s| s.iter_unexpired().next().is_some())
            .unwrap_or(false)
    }

    pub fn save(&self) -> Result<()> {
        let mut buf = Vec::new();
        cookie_store::serde::json::save_incl_expired_and_nonpersistent(&*self.lock()?, &mut buf)
            .map_err(|e| anyhow!("serializing cookies: {e}"))?;
        write_file(&self.path, &String::from_utf8(buf)?, 0o600)
    }

    pub fn clear_memory(&self) -> Result<()> {
        self.lock()?.clear();
        Ok(())
    }

    /// Empties the jar and deletes its file.
    pub fn clear(&self) -> Result<()> {
        self.clear_memory()?;
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Replaces the jar's contents with `cookies`, as if `site` had set them.
    pub fn replace(&self, site: &Url, cookies: &[NewCookie]) -> Result<()> {
        if cookies.is_empty() {
            bail!("no cookies to save");
        }
        let mut store = self.lock()?;
        store.clear();
        let host = site.host_str().unwrap_or_default();
        let secure = if site.scheme() == "https" {
            "; Secure"
        } else {
            ""
        };
        for c in cookies {
            let domain = c.domain.trim_start_matches('.');
            let domain = if domain.is_empty() { host } else { domain };
            // A far-future expiry makes session cookies persistent so they're
            // saved; the server still decides when the session ends.
            let raw = format!(
                "{}={}; Domain={domain}; Path=/{secure}; Expires=Fri, 31 Dec 2100 23:59:59 GMT",
                c.name, c.value
            );
            store
                .parse(&raw, site)
                .map_err(|e| anyhow!("bad cookie {:?}: {e}", c.name))?;
        }
        Ok(())
    }
}

/// Parses a `Cookie:` header style string (`a=1; b=2`).
pub fn parse_header(header: &str, domain: &str) -> Vec<NewCookie> {
    header
        .trim()
        .trim_start_matches("Cookie:")
        .split(';')
        .filter_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            let name = name.trim();
            (!name.is_empty()).then(|| NewCookie {
                name: name.into(),
                value: value.trim().into(),
                domain: domain.into(),
            })
        })
        .collect()
}
