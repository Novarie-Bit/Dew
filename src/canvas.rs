use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, LINK};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::model::{Assignment, Source, Status};

pub struct Canvas {
    base: String,
    token: String,
    http: Client,
}

#[derive(Deserialize)]
struct User {
    name: String,
}

#[derive(Deserialize)]
struct Course {
    id: u64,
    name: Option<String>,
    course_code: Option<String>,
}

#[derive(Deserialize)]
struct RawAssignment {
    id: u64,
    name: String,
    due_at: Option<DateTime<Utc>>,
    points_possible: Option<f64>,
    html_url: Option<String>,
    #[serde(default)]
    submission_types: Vec<String>,
    submission: Option<Submission>,
}

#[derive(Deserialize)]
struct Submission {
    workflow_state: Option<String>,
    submitted_at: Option<DateTime<Utc>>,
    score: Option<f64>,
    #[serde(default)]
    late: bool,
    #[serde(default)]
    missing: bool,
    #[serde(default)]
    excused: Option<bool>,
}

/// Accepts "canvas.wustl.edu", "https://canvas.wustl.edu/", etc.
pub fn normalize_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("https://{url}")
    }
}

impl Canvas {
    pub fn new(base: &str, token: &str) -> Result<Self> {
        let http = crate::http_client().build()?;
        Ok(Canvas {
            base: normalize_url(base),
            token: token.trim().to_string(),
            http,
        })
    }

    fn get(&self, url: &str) -> Result<Response> {
        let resp = self
            .http
            .get(url)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .send()
            .map_err(|e| crate::network_error(e, &self.base))?;
        match resp.status() {
            s if s.is_success() => Ok(resp),
            StatusCode::UNAUTHORIZED => {
                bail!("the access token was rejected (401); create a new one and sign in again")
            }
            s => bail!("Canvas returned {s} for {url}"),
        }
    }

    /// GET a list endpoint and follow `Link: <...>; rel="next"` pagination.
    fn get_all<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>> {
        let mut out = Vec::new();
        let mut next = Some(format!("{}{}", self.base, path));
        while let Some(url) = next.take() {
            let resp = self.get(&url)?;
            next = resp
                .headers()
                .get(LINK)
                .and_then(|v| v.to_str().ok())
                .and_then(next_link);
            let page: Vec<T> = resp.json().with_context(|| format!("decoding {url}"))?;
            out.extend(page);
        }
        Ok(out)
    }

    /// Verifies the token and returns the user's display name.
    pub fn whoami(&self) -> Result<String> {
        let user: User = self
            .get(&format!("{}/api/v1/users/self", self.base))?
            .json()
            .context("decoding Canvas user")?;
        Ok(user.name)
    }

    pub fn fetch_all(&self) -> Result<Vec<Assignment>> {
        let courses: Vec<Course> = self.get_all(
            "/api/v1/courses?enrollment_state=active&enrollment_type=student&per_page=100",
        )?;
        // Courses with access_restricted_by_date come back without a name.
        let courses: Vec<Course> = courses.into_iter().filter(|c| c.name.is_some()).collect();

        let results: Vec<Result<Vec<Assignment>>> = std::thread::scope(|s| {
            let handles: Vec<_> = courses
                .iter()
                .map(|c| s.spawn(move || self.course_assignments(c)))
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(anyhow!("fetch thread panicked")))
                })
                .collect()
        });
        let mut out = Vec::new();
        for r in results {
            out.extend(r?);
        }
        Ok(out)
    }

    fn course_assignments(&self, course: &Course) -> Result<Vec<Assignment>> {
        let raw: Vec<RawAssignment> = self.get_all(&format!(
            "/api/v1/courses/{}/assignments?include[]=submission&per_page=100",
            course.id
        ))?;
        let label = course
            .course_code
            .clone()
            .or_else(|| course.name.clone())
            .unwrap_or_default();
        let now = Utc::now();
        Ok(raw.into_iter().map(|a| convert(a, &label, now)).collect())
    }
}

fn convert(a: RawAssignment, course: &str, now: DateTime<Utc>) -> Assignment {
    Assignment {
        source: Source::Canvas,
        id: a.id.to_string(),
        course: course.to_string(),
        status: status_of(&a, now),
        name: a.name,
        due: a.due_at,
        late_due: None,
        points: a.points_possible,
        url: a.html_url,
    }
}

fn status_of(a: &RawAssignment, now: DateTime<Utc>) -> Status {
    let offline = !a.submission_types.is_empty()
        && a.submission_types
            .iter()
            .all(|t| matches!(t.as_str(), "none" | "on_paper" | "not_graded"));
    let Some(sub) = &a.submission else {
        return if offline {
            Status::Offline
        } else {
            Status::Todo
        };
    };
    let state = sub.workflow_state.as_deref().unwrap_or("");
    if sub.excused == Some(true) {
        return Status::Excused;
    }
    if state == "graded"
        && let Some(score) = sub.score
    {
        let score = match a.points_possible {
            Some(p) if p > 0.0 => format!("{}/{}", fmt_num(score), fmt_num(p)),
            _ => fmt_num(score),
        };
        return Status::Graded { score };
    }
    if sub.submitted_at.is_some() || matches!(state, "submitted" | "pending_review" | "graded") {
        return if sub.late {
            Status::Late
        } else {
            Status::Submitted
        };
    }
    if offline {
        return Status::Offline;
    }
    if sub.missing || a.due_at.is_some_and(|d| d < now) {
        return Status::Missing;
    }
    Status::Todo
}

fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 {
        format!("{x:.0}")
    } else {
        format!("{x:.2}").trim_end_matches('0').to_string()
    }
}

fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        params
            .split(';')
            .any(|p| p.trim() == r#"rel="next""#)
            .then(|| {
                url.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string()
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_link_header() {
        let h = r#"<https://c.edu/api/v1/courses?page=1&per_page=100>; rel="current",<https://c.edu/api/v1/courses?page=2&per_page=100>; rel="next",<https://c.edu/api/v1/courses?page=1&per_page=100>; rel="first""#;
        assert_eq!(
            next_link(h).as_deref(),
            Some("https://c.edu/api/v1/courses?page=2&per_page=100")
        );
        assert_eq!(next_link(r#"<https://x>; rel="last""#), None);
    }

    #[test]
    fn normalizes_urls() {
        assert_eq!(
            normalize_url("canvas.wustl.edu/"),
            "https://canvas.wustl.edu"
        );
        assert_eq!(
            normalize_url(" https://x.instructure.com "),
            "https://x.instructure.com"
        );
    }

    fn parse(json: &str) -> Status {
        let a: RawAssignment = serde_json::from_str(json).unwrap();
        let now = "2026-10-17T12:00:00Z".parse().unwrap();
        status_of(&a, now)
    }

    #[test]
    fn statuses() {
        let base = |due: &str, sub: &str, types: &str| {
            format!(
                r#"{{"id":1,"name":"A","due_at":{due},"points_possible":10.0,"html_url":"u","submission_types":{types},"submission":{sub}}}"#
            )
        };
        let unsub = r#"{"workflow_state":"unsubmitted","submitted_at":null,"score":null,"late":false,"missing":false}"#;
        assert_eq!(
            parse(&base(
                r#""2026-10-20T04:59:59Z""#,
                unsub,
                r#"["online_upload"]"#
            )),
            Status::Todo
        );
        assert_eq!(
            parse(&base(
                r#""2026-10-10T04:59:59Z""#,
                unsub,
                r#"["online_upload"]"#
            )),
            Status::Missing
        );
        assert_eq!(
            parse(&base(r#""2026-10-10T04:59:59Z""#, unsub, r#"["on_paper"]"#)),
            Status::Offline
        );
        let graded = r#"{"workflow_state":"graded","submitted_at":"2026-10-01T00:00:00Z","score":9.5,"late":false,"missing":false}"#;
        assert_eq!(
            parse(&base("null", graded, r#"["online_upload"]"#)),
            Status::Graded {
                score: "9.5/10".into()
            }
        );
        let late = r#"{"workflow_state":"submitted","submitted_at":"2026-10-11T00:00:00Z","score":null,"late":true,"missing":false}"#;
        assert_eq!(
            parse(&base(
                r#""2026-10-10T04:59:59Z""#,
                late,
                r#"["online_upload"]"#
            )),
            Status::Late
        );
        let excused = r#"{"workflow_state":"unsubmitted","excused":true}"#;
        assert_eq!(parse(&base("null", excused, "[]")), Status::Excused);
    }
}
