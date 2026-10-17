//! Gradescope has no public API, so this logs in with the regular web form
//! and scrapes the course pages.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, NaiveDateTime, Utc};
use reqwest::Url;
use reqwest::blocking::Client;
use scraper::{ElementRef, Html, Selector};

use crate::jar::{self, Jar, NewCookie};
use crate::model::{Assignment, Source, Status};

pub const BASE: &str = "https://www.gradescope.com";

pub struct Gradescope {
    http: Client,
    jar: Jar,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Course {
    pub id: String,
    pub short_name: String,
    pub term: String,
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("valid selector")
}

fn text_of(el: ElementRef<'_>) -> String {
    el.text()
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

impl Gradescope {
    /// Creates a client, restoring any saved session cookies.
    pub fn load(cookie_path: &Path) -> Result<Self> {
        let jar = Jar::load(cookie_path)?;
        let http = crate::http_client()
            .cookie_provider(jar.provider())
            .build()?;
        Ok(Gradescope { http, jar })
    }

    pub fn has_session(&self) -> bool {
        self.jar.has_any()
    }

    pub fn save(&self) -> Result<()> {
        self.jar.save()
    }

    pub fn clear(&self) -> Result<()> {
        self.jar.clear()
    }

    fn get_html(&self, path: &str) -> Result<(Url, String)> {
        let resp = self
            .http
            .get(format!("{BASE}{path}"))
            .send()
            .map_err(|e| crate::network_error(e, BASE))?;
        if !resp.status().is_success() {
            bail!("Gradescope returned {} for {path}", resp.status());
        }
        let url = resp.url().clone();
        Ok((url, resp.text()?))
    }

    /// Email/password login (the "Log In" form, not school SSO).
    pub fn login(&self, email: &str, password: &str) -> Result<()> {
        self.jar.clear_memory()?;
        let (_, page) = self.get_html("/login")?;
        let token = parse_authenticity_token(&page)
            .context("couldn't find the login form on gradescope.com/login")?;
        let resp = self
            .http
            .post(format!("{BASE}/login"))
            .form(&[
                ("utf8", "✓"),
                ("authenticity_token", token.as_str()),
                ("session[email]", email.trim()),
                ("session[password]", password),
                ("session[remember_me]", "1"),
                ("commit", "Log In"),
                ("session[remember_me_sso]", "0"),
            ])
            .send()
            .context("submitting Gradescope login")?;
        let url = resp.url().clone();
        let body = resp.text()?;
        if !looks_logged_in(&url, &body) {
            bail!(
                "Gradescope login failed. Check your email/password; if you normally use \
                 school SSO, set a Gradescope password via \"Forgot password\" or import \
                 browser cookies instead"
            );
        }
        self.save()
    }

    /// Imports a browser session from a `Cookie:` header style string, e.g.
    /// `signed_token=...; _gradescope_session=...`.
    pub fn import_cookies(&self, header: &str) -> Result<()> {
        let cookies = jar::parse_header(header, "gradescope.com");
        if cookies.is_empty() {
            bail!(
                "no cookies found; expected something like signed_token=...; _gradescope_session=..."
            );
        }
        self.use_cookies(&cookies)
    }

    /// Replaces the session with `cookies`, keeping them only if Gradescope
    /// accepts them.
    pub fn use_cookies(&self, cookies: &[NewCookie]) -> Result<()> {
        self.jar.replace(&BASE.parse()?, cookies)?;
        if !self.check_session()? {
            self.jar.clear_memory()?;
            bail!("Gradescope didn't accept that session (expired or incomplete)");
        }
        self.save()
    }

    pub fn check_session(&self) -> Result<bool> {
        let (url, body) = self.get_html("/account")?;
        Ok(looks_logged_in(&url, &body))
    }

    pub fn fetch_all(&self, all_terms: bool) -> Result<Vec<Assignment>> {
        let (url, body) = self.get_html("/account")?;
        if !looks_logged_in(&url, &body) {
            bail!("Gradescope session expired; sign in again");
        }
        let mut courses = parse_courses(&body);
        if !all_terms && let Some(term) = courses.first().map(|c| c.term.clone()) {
            courses.retain(|c| c.term == term);
        }
        let results: Vec<Result<Vec<Assignment>>> = std::thread::scope(|s| {
            let handles: Vec<_> = courses
                .iter()
                .map(|c| {
                    s.spawn(move || {
                        let (_, html) = self.get_html(&format!("/courses/{}", c.id))?;
                        Ok(parse_assignments(&html, c, Utc::now()))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow!("fetch thread panicked")))
                })
                .collect()
        });
        // Persist any refreshed session cookies.
        let _ = self.save();
        let mut out = Vec::new();
        for r in results {
            out.extend(r?);
        }
        Ok(out)
    }
}

pub fn parse_authenticity_token(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let input = sel(r#"form input[name="authenticity_token"]"#);
    let meta = sel(r#"meta[name="csrf-token"]"#);
    doc.select(&input)
        .find_map(|e| e.value().attr("value"))
        .or_else(|| doc.select(&meta).find_map(|e| e.value().attr("content")))
        .map(str::to_string)
}

fn looks_logged_in(url: &Url, html: &str) -> bool {
    let doc = Html::parse_document(html);
    if doc
        .select(&sel(
            r#"a[href="/logout"], [data-method="delete"][href="/logout"]"#,
        ))
        .next()
        .is_some()
    {
        return true;
    }
    url.path().starts_with("/account")
        && doc
            .select(&sel(r#"input[name="session[password]"]"#))
            .next()
            .is_none()
}

/// Student courses from /account, in page order (newest term first).
pub fn parse_courses(html: &str) -> Vec<Course> {
    let doc = Html::parse_document(html);
    let walker = sel("h1.pageHeading, .courseList--term, a.courseBox");
    let shortname = sel(".courseBox--shortname");
    let mut heading = String::new();
    let mut term = String::new();
    let mut out = Vec::new();
    for el in doc.select(&walker) {
        let classes: Vec<&str> = el.value().classes().collect();
        if el.value().name() == "h1" {
            heading = text_of(el);
        } else if classes.contains(&"courseList--term") {
            term = text_of(el);
        } else if classes.contains(&"courseBox") {
            if heading.to_lowercase().contains("instructor") {
                continue;
            }
            let Some(href) = el.value().attr("href") else {
                continue;
            };
            let Some(id) = href
                .strip_prefix("/courses/")
                .map(|s| s.trim_end_matches('/'))
            else {
                continue;
            };
            // Skips the "Add a course" box (/courses/new) and similar.
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let short_name = el
                .select(&shortname)
                .next()
                .map(text_of)
                .unwrap_or_else(|| text_of(el));
            out.push(Course {
                id: id.to_string(),
                short_name,
                term: term.clone(),
            });
        }
    }
    out
}

pub fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S %z")
        .or_else(|_| DateTime::parse_from_rfc3339(s))
        .map(|d| d.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|n| n.and_utc())
        })
}

fn has_class_in_ancestry(el: ElementRef<'_>, needle: &str) -> bool {
    std::iter::once(el)
        .chain(el.ancestors().filter_map(ElementRef::wrap))
        .take(4)
        .any(|e| e.value().classes().any(|c| c.contains(needle)))
}

/// Assignment rows from a student course page.
pub fn parse_assignments(html: &str, course: &Course, now: DateTime<Utc>) -> Vec<Assignment> {
    let doc = Html::parse_document(html);
    let row_sel = sel("#assignments-student-table tbody tr");
    let name_sel = sel("th.table--primaryLink, th");
    let link_sel = sel("a[href]");
    let button_sel = sel("button[data-assignment-id]");
    let time_sel = sel("time[datetime]");
    let status_sel = sel(".submissionStatus");
    let score_sel = sel(".submissionStatus--score");

    let mut out = Vec::new();
    for row in doc.select(&row_sel) {
        let Some(th) = row.select(&name_sel).next() else {
            continue;
        };
        let name = text_of(th);
        if name.is_empty() {
            continue;
        }

        let link = th
            .select(&link_sel)
            .next()
            .and_then(|a| a.value().attr("href"));
        let button_id = th
            .select(&button_sel)
            .next()
            .and_then(|b| b.value().attr("data-assignment-id"));
        let id = link
            .and_then(|h| h.split("/assignments/").nth(1))
            .and_then(|rest| rest.split('/').next())
            .or(button_id)
            .map(str::to_string)
            .unwrap_or_else(|| name.clone());
        let url = match (link, button_id) {
            (Some(h), _) => format!("{BASE}{h}"),
            (None, Some(aid)) => format!("{BASE}/courses/{}/assignments/{aid}", course.id),
            _ => format!("{BASE}/courses/{}", course.id),
        };

        // Release date, due date, and optional late due date.
        let mut release_seen = false;
        let mut dues = Vec::new();
        let mut unlabeled = Vec::new();
        for t in row.select(&time_sel) {
            let Some(when) = t.value().attr("datetime").and_then(parse_time) else {
                continue;
            };
            if has_class_in_ancestry(t, "releaseDate") {
                release_seen = true;
            } else if has_class_in_ancestry(t, "dueDate") {
                dues.push(when);
            } else {
                unlabeled.push(when);
            }
        }
        if dues.is_empty() {
            // No class hints: assume [release, due, late due] order.
            let skip = if release_seen || unlabeled.len() == 1 {
                0
            } else {
                1
            };
            dues = unlabeled.into_iter().skip(skip).collect();
        }
        let due = dues.first().copied();
        let late_due = dues.get(1).copied();

        let status_text = row
            .select(&status_sel)
            .next()
            .map(text_of)
            .unwrap_or_default();
        let score = row.select(&score_sel).next().map(text_of);
        let final_deadline = late_due.or(due);
        let status = if let Some(score) = score.filter(|s| !s.is_empty()) {
            Status::Graded {
                score: score.replace(' ', ""),
            }
        } else if status_text.contains("No Submission") || status_text.is_empty() {
            if final_deadline.is_some_and(|d| d < now) {
                Status::Missing
            } else {
                Status::Todo
            }
        } else if status_text.contains("Late") {
            Status::Late
        } else {
            Status::Submitted
        };

        let points = match &status {
            Status::Graded { score } => score.split('/').nth(1).and_then(|p| p.trim().parse().ok()),
            _ => None,
        };

        out.push(Assignment {
            source: Source::Gradescope,
            id: format!("{}:{id}", course.id),
            course: course.short_name.clone(),
            name,
            due,
            late_due,
            status,
            points,
            url: Some(url),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT: &str = include_str!("../tests/fixtures/gradescope_account.html");
    const COURSE: &str = include_str!("../tests/fixtures/gradescope_course.html");
    const LOGIN: &str = include_str!("../tests/fixtures/gradescope_login.html");

    #[test]
    fn finds_login_token() {
        assert_eq!(parse_authenticity_token(LOGIN).as_deref(), Some("tok3n=="));
    }

    #[test]
    fn login_page_is_not_logged_in() {
        let url: Url = "https://www.gradescope.com/login".parse().unwrap();
        assert!(!looks_logged_in(&url, LOGIN));
        let url: Url = "https://www.gradescope.com/account".parse().unwrap();
        assert!(looks_logged_in(&url, ACCOUNT));
    }

    #[test]
    fn parses_student_courses_only() {
        let courses = parse_courses(ACCOUNT);
        assert_eq!(
            courses,
            vec![
                Course {
                    id: "1001".into(),
                    short_name: "CSE 247".into(),
                    term: "Fall 2026".into()
                },
                Course {
                    id: "1002".into(),
                    short_name: "Math 3200".into(),
                    term: "Fall 2026".into()
                },
                Course {
                    id: "900".into(),
                    short_name: "CSE 132".into(),
                    term: "Spring 2026".into()
                },
            ]
        );
    }

    #[test]
    fn parses_assignment_rows() {
        let course = Course {
            id: "1001".into(),
            short_name: "CSE 247".into(),
            term: String::new(),
        };
        let now = parse_time("2026-10-17 12:00:00 -0500").unwrap();
        let items = parse_assignments(COURSE, &course, now);
        let summary: Vec<_> = items
            .iter()
            .map(|a| (a.id.as_str(), a.name.as_str(), a.status.clone()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    "1001:501",
                    "Homework 1",
                    Status::Graded {
                        score: "9.5/10.0".into()
                    }
                ),
                ("1001:502", "Homework 2", Status::Submitted),
                ("1001:503", "Homework 3", Status::Todo),
                ("1001:Quiz 0", "Quiz 0", Status::Missing),
                ("1001:505", "Lab 4", Status::Late),
            ]
        );
        let hw3 = &items[2];
        assert_eq!(hw3.due, parse_time("2026-10-20 23:59:00 -0500"));
        assert_eq!(hw3.late_due, parse_time("2026-10-22 23:59:00 -0500"));
        assert_eq!(
            hw3.url.as_deref(),
            Some("https://www.gradescope.com/courses/1001/assignments/503")
        );
        assert_eq!(items[0].points, Some(10.0));
        assert_eq!(
            items[0].url.as_deref(),
            Some("https://www.gradescope.com/courses/1001/assignments/501/submissions/7001")
        );
    }

    #[test]
    fn cookie_jar_round_trips() {
        let dir = std::env::temp_dir().join(format!("dew-test-{}", std::process::id()));
        let path = dir.join("cookies.json");
        let gs = Gradescope::load(&path).unwrap();
        assert!(!gs.has_session());
        assert!(jar::parse_header("  ; ", "gradescope.com").is_empty());
        let cookies = jar::parse_header(
            "Cookie: signed_token=abc%3D%3D; _gradescope_session=xyz",
            "gradescope.com",
        );
        gs.jar.replace(&BASE.parse().unwrap(), &cookies).unwrap();
        gs.save().unwrap();

        let reloaded = Gradescope::load(&path).unwrap();
        assert!(reloaded.has_session());
        let url: Url = "https://www.gradescope.com/account".parse().unwrap();
        let provider = reloaded.jar.provider();
        let store = provider.lock().unwrap();
        let mut sent: Vec<_> = store.get_request_values(&url).collect();
        sent.sort();
        assert_eq!(
            sent,
            vec![
                ("_gradescope_session", "xyz"),
                ("signed_token", "abc%3D%3D")
            ]
        );
        drop(store);
        reloaded.clear().unwrap();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn time_formats() {
        assert!(parse_time("2026-10-20 23:59:00 -0500").is_some());
        assert!(parse_time("2026-10-20T23:59:00-05:00").is_some());
        assert!(parse_time("garbage").is_none());
    }
}
