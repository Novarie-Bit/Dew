//! "Sign in with browser": a small WebKitGTK window where you sign in the
//! normal way (school SSO, Duo, ...). Once the site shows a signed-in page,
//! the session cookies are checked against the site and saved.
//!
//! The window runs in its own process (`dew browser-login ...`) because it
//! needs the main thread's event loop, which the GUI already owns.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Url;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::{PageLoadEvent, WebContext, WebViewBuilder};

use crate::auth;
use crate::canvas::normalize_url;
use crate::config::Paths;
use crate::gradescope;
use crate::jar::NewCookie;

#[derive(Clone, Debug)]
pub enum Site {
    Canvas { url: String },
    Gradescope,
}

/// Exit code the window uses when it's closed before signing in.
const CANCELLED: i32 = 2;

impl Site {
    fn name(&self) -> &'static str {
        match self {
            Site::Canvas { .. } => "Canvas",
            Site::Gradescope => "Gradescope",
        }
    }

    fn start_url(&self) -> String {
        match self {
            Site::Canvas { url } => format!("{}/login", normalize_url(url)),
            Site::Gradescope => format!("{}/login", gradescope::BASE),
        }
    }

    fn cookie_url(&self) -> String {
        match self {
            Site::Canvas { url } => normalize_url(url),
            Site::Gradescope => gradescope::BASE.into(),
        }
    }

    /// Whether `url` is a page you only see once signed in.
    fn is_signed_in_page(&self, url: &str) -> bool {
        let Ok(u) = Url::parse(url) else { return false };
        let path = u.path();
        match self {
            Site::Canvas { url: base } => {
                let host = Url::parse(&normalize_url(base)).ok();
                u.host_str().is_some()
                    && u.host_str() == host.as_ref().and_then(|h| h.host_str())
                    && !["/login", "/saml", "/logout"]
                        .iter()
                        .any(|p| path.starts_with(p))
            }
            Site::Gradescope => {
                u.host_str().is_some_and(|h| h.ends_with("gradescope.com"))
                    && (path.starts_with("/account") || path.starts_with("/courses"))
            }
        }
    }

    fn args(&self) -> Vec<String> {
        match self {
            Site::Canvas { url } => vec!["canvas".into(), "--url".into(), url.clone()],
            Site::Gradescope => vec!["gradescope".into()],
        }
    }
}

/// Opens the sign-in window in a child process and waits for it.
/// Returns a short success message.
pub fn sign_in(site: &Site) -> Result<String> {
    let exe = std::env::current_exe().context("locating the dew executable")?;
    let mut cmd = Command::new(exe);
    cmd.arg("browser-login")
        .args(site.args())
        .stdin(Stdio::null());
    // WebKitGTK's DMA-BUF renderer shows a blank window on some NVIDIA and
    // Wayland setups; a login page doesn't need it.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        cmd.env("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
    let out = cmd.output().context("starting the sign-in window")?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&out.stderr);
    // WebKit is chatty on stderr; the last line is ours.
    let last = stderr
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    match out.status.code() {
        Some(0) => Ok(stdout),
        Some(CANCELLED) => bail!("sign-in window closed before finishing"),
        _ if last.is_empty() => bail!("the sign-in window failed ({})", out.status),
        _ => bail!("{last}"),
    }
}

enum UserEvent {
    Loaded(String),
    Verified(Result<String, String>),
}

fn verify(paths: &Paths, site: &Site, cookies: &[NewCookie]) -> Result<String> {
    match site {
        Site::Canvas { url } => {
            let user = auth::login_canvas_session(paths, url, cookies)?;
            Ok(format!("Signed in to Canvas as {user}"))
        }
        Site::Gradescope => {
            auth::login_gradescope_session(paths, cookies)?;
            Ok("Signed in to Gradescope".into())
        }
    }
}

/// Runs the sign-in window in this process. Exits the process when done:
/// 0 after saving a session, 2 if the window is closed, 1 on errors.
pub fn run_window(paths: Paths, site: Site) -> Result<()> {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let title = format!("Sign in to {} - dew", site.name());
    let window = WindowBuilder::new()
        .with_title(&title)
        .with_inner_size(LogicalSize::new(560.0, 760.0))
        .build(&event_loop)
        .context("opening the sign-in window")?;

    std::fs::create_dir_all(&paths.webview_data).ok();
    let mut context = WebContext::new(Some(paths.webview_data.clone()));
    let loaded = proxy.clone();
    let builder = WebViewBuilder::new_with_web_context(&mut context)
        .with_url(site.start_url())
        .with_on_page_load_handler(move |event, url| {
            if matches!(event, PageLoadEvent::Finished) {
                let _ = loaded.send_event(UserEvent::Loaded(url));
            }
        });
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window
            .default_vbox()
            .ok_or_else(|| anyhow!("no GTK container"))?;
        builder.build_gtk(vbox).context("creating the web view")?
    };

    let mut checking = false;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        // Keep these alive for as long as the loop runs.
        let _ = (&window, &context);
        match event {
            Event::UserEvent(UserEvent::Loaded(url)) => {
                if checking || !site.is_signed_in_page(&url) {
                    return;
                }
                let cookies = match webview.cookies_for_url(&site.cookie_url()) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("couldn't read cookies: {e}");
                        return;
                    }
                };
                let cookies: Vec<NewCookie> = cookies
                    .iter()
                    .map(|c| NewCookie {
                        name: c.name().into(),
                        value: c.value().into(),
                        domain: c.domain().unwrap_or_default().into(),
                    })
                    .collect();
                checking = true;
                window.set_title(&format!("Checking your {} session... - dew", site.name()));
                let (proxy, paths, site) = (proxy.clone(), paths.clone(), site.clone());
                std::thread::spawn(move || {
                    let result = verify(&paths, &site, &cookies).map_err(|e| format!("{e:#}"));
                    let _ = proxy.send_event(UserEvent::Verified(result));
                });
            }
            Event::UserEvent(UserEvent::Verified(Ok(msg))) => {
                println!("{msg}");
                std::process::exit(0);
            }
            Event::UserEvent(UserEvent::Verified(Err(e))) => {
                // Not signed in after all (or the check failed); let the user keep going.
                checking = false;
                eprintln!("{e}");
                window.set_title(&title);
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                std::process::exit(CANCELLED);
            }
            _ => {}
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_signed_in_pages() {
        let canvas = Site::Canvas {
            url: "canvas.wustl.edu".into(),
        };
        assert!(canvas.is_signed_in_page("https://canvas.wustl.edu/?login_success=1"));
        assert!(canvas.is_signed_in_page("https://canvas.wustl.edu/courses/123"));
        assert!(!canvas.is_signed_in_page("https://canvas.wustl.edu/login/saml"));
        assert!(!canvas.is_signed_in_page("https://login.wustl.edu/idp/profile/SAML2"));
        assert!(!canvas.is_signed_in_page("https://api-123.duosecurity.com/frame/prompt"));

        let gs = Site::Gradescope;
        assert!(gs.is_signed_in_page("https://www.gradescope.com/account"));
        assert!(!gs.is_signed_in_page("https://www.gradescope.com/login"));
        assert!(!gs.is_signed_in_page("https://www.gradescope.com/auth/saml/wustl"));
        assert!(!gs.is_signed_in_page("https://www.gradescope.com/"));
    }
}
