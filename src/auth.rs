use anyhow::{Result, bail};

use crate::canvas::{self, Canvas};
use crate::config::{CanvasCreds, Credentials, GradescopeCreds, Paths};
use crate::gradescope::Gradescope;

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
