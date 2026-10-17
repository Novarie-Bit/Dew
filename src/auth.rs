use anyhow::{Result, bail};

use crate::canvas::{self, Canvas};
use crate::config::{CanvasCreds, Credentials, GradescopeCreds, Paths};
use crate::gradescope::Gradescope;
use crate::jar::{self, Jar, NewCookie};

/// Verifies a Canvas access token and saves it. Returns the user's name.
pub fn login_canvas(paths: &Paths, url: &str, token: &str) -> Result<String> {
    if token.trim().is_empty() {
        bail!("Canvas access token is empty");
    }
    let url = canvas::normalize_url(url);
    let user = Canvas::new(&url, token)?.whoami()?;
    let mut creds = Credentials::load(paths)?;
    creds.canvas = Some(CanvasCreds {
        url,
        token: token.trim().into(),
        user: user.clone(),
    });
    creds.save(paths)?;
    Ok(user)
}

/// Saves a Canvas browser session if Canvas accepts it. Returns the user's name.
pub fn login_canvas_session(paths: &Paths, url: &str, cookies: &[NewCookie]) -> Result<String> {
    let url = canvas::normalize_url(url);
    let jar = Jar::load(&paths.canvas_cookies)?;
    jar.replace(&url.parse()?, cookies)?;
    let user = Canvas::with_session(&url, &jar)?.whoami()?;
    jar.save()?;
    let mut creds = Credentials::load(paths)?;
    creds.canvas = Some(CanvasCreds {
        url,
        token: String::new(),
        user: user.clone(),
    });
    creds.save(paths)?;
    Ok(user)
}

/// Like [`login_canvas_session`], from a pasted `Cookie:` header.
pub fn login_canvas_cookies(paths: &Paths, url: &str, header: &str) -> Result<String> {
    let site: reqwest::Url = canvas::normalize_url(url).parse()?;
    let cookies = jar::parse_header(header, site.host_str().unwrap_or_default());
    if cookies.is_empty() {
        bail!("no cookies found; expected something like canvas_session=...");
    }
    login_canvas_session(paths, url, &cookies)
}

pub fn login_gradescope_session(paths: &Paths, cookies: &[NewCookie]) -> Result<()> {
    Gradescope::load(&paths.gradescope_cookies)?.use_cookies(cookies)?;
    let mut creds = Credentials::load(paths)?;
    creds.gradescope = Some(GradescopeCreds {
        email: "(browser sign-in)".into(),
    });
    creds.save(paths)
}

pub fn login_gradescope(paths: &Paths, email: &str, password: &str) -> Result<()> {
    if email.trim().is_empty() || password.is_empty() {
        bail!("Gradescope email and password are both required");
    }
    Gradescope::load(&paths.gradescope_cookies)?.login(email, password)?;
    let mut creds = Credentials::load(paths)?;
    creds.gradescope = Some(GradescopeCreds {
        email: email.trim().into(),
    });
    creds.save(paths)
}

pub fn login_gradescope_cookies(paths: &Paths, cookies: &str) -> Result<()> {
    Gradescope::load(&paths.gradescope_cookies)?.import_cookies(cookies)?;
    let mut creds = Credentials::load(paths)?;
    creds.gradescope = Some(GradescopeCreds {
        email: "(browser session)".into(),
    });
    creds.save(paths)
}

pub fn logout_canvas(paths: &Paths) -> Result<()> {
    Jar::load(&paths.canvas_cookies)?.clear()?;
    let mut creds = Credentials::load(paths)?;
    creds.canvas = None;
    creds.save(paths)
}

pub fn logout_gradescope(paths: &Paths) -> Result<()> {
    Gradescope::load(&paths.gradescope_cookies)?.clear()?;
    let mut creds = Credentials::load(paths)?;
    creds.gradescope = None;
    creds.save(paths)
}
