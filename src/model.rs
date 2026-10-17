use chrono::{DateTime, Duration, Local, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum Source {
    Canvas,
    Gradescope,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Canvas => "Canvas",
            Source::Gradescope => "Gradescope",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Source::Canvas => "CV",
            Source::Gradescope => "GS",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Status {
    /// Open and not yet submitted.
    Todo,
    /// Past due (and past any late deadline) without a submission.
    Missing,
    Submitted,
    /// Submitted after the due date.
    Late,
    Graded {
        score: String,
    },
    Excused,
    /// Nothing to turn in online (on paper, in class, ...).
    Offline,
}

impl Status {
    pub fn is_done(&self) -> bool {
        matches!(
            self,
            Status::Submitted | Status::Late | Status::Graded { .. } | Status::Excused
        )
    }

    pub fn label(&self) -> String {
        match self {
            Status::Todo => "To do".into(),
            Status::Missing => "Missing".into(),
            Status::Submitted => "Submitted".into(),
            Status::Late => "Submitted late".into(),
            Status::Graded { score } => format!("Graded {score}"),
            Status::Excused => "Excused".into(),
            Status::Offline => "Offline".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Assignment {
    pub source: Source,
    pub id: String,
    pub course: String,
    pub name: String,
    pub due: Option<DateTime<Utc>>,
    /// Gradescope late-submission deadline, if one is set.
    pub late_due: Option<DateTime<Utc>>,
    pub status: Status,
    pub points: Option<f64>,
    pub url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Done,
    Overdue,
    Day,
    Soon,
    Later,
    Undated,
}

impl Assignment {
    pub fn urgency(&self, now: DateTime<Utc>) -> Urgency {
        if self.status.is_done() {
            return Urgency::Done;
        }
        let Some(due) = self.due else {
            return Urgency::Undated;
        };
        let left = due - now;
        if left < Duration::zero() {
            Urgency::Overdue
        } else if left < Duration::hours(24) {
            Urgency::Day
        } else if left < Duration::days(3) {
            Urgency::Soon
        } else {
            Urgency::Later
        }
    }

    /// Whether this item matters right now: not done, or due recently enough
    /// that it's still worth seeing.
    pub fn is_current(&self, now: DateTime<Utc>, keep_days: i64) -> bool {
        match self.due {
            None => !self.status.is_done(),
            Some(due) => due > now - Duration::days(keep_days),
        }
    }

    pub fn matches(&self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let q = query.to_lowercase();
        self.name.to_lowercase().contains(&q) || self.course.to_lowercase().contains(&q)
    }
}

/// Sort by due date (undated last), then course, then name.
pub fn sort(items: &mut [Assignment]) {
    items.sort_by(|a, b| {
        let key = |x: &Assignment| (x.due.is_none(), x.due);
        key(a)
            .cmp(&key(b))
            .then_with(|| a.course.cmp(&b.course))
            .then_with(|| a.name.cmp(&b.name))
    });
}

fn normalize(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Canvas often mirrors Gradescope assignments (via the LTI integration) and
/// reports them as missing because the submission happens on Gradescope.
/// Drop Canvas items that have a same-named Gradescope item due around the
/// same time.
pub fn dedupe(items: Vec<Assignment>) -> Vec<Assignment> {
    let gs: Vec<(String, Option<DateTime<Utc>>)> = items
        .iter()
        .filter(|a| a.source == Source::Gradescope)
        .map(|a| (normalize(&a.name), a.due))
        .collect();
    items
        .into_iter()
        .filter(|a| {
            if a.source != Source::Canvas {
                return true;
            }
            let name = normalize(&a.name);
            !gs.iter().any(|(gname, gdue)| {
                *gname == name
                    && match (a.due, gdue) {
                        (Some(x), Some(y)) => (x - *y).num_hours().abs() <= 48,
                        _ => true,
                    }
            })
        })
        .collect()
}

/// "3d 4h", "5h 12m", "12m"; prefixed with "-" when in the past.
pub fn format_left(due: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let d = due - now;
    let (sign, d) = if d < Duration::zero() {
        ("-", -d)
    } else {
        ("", d)
    };
    let days = d.num_days();
    let hours = d.num_hours() % 24;
    let mins = d.num_minutes() % 60;
    let body = if days >= 7 {
        format!("{days}d")
    } else if days > 0 {
        format!("{days}d {hours}h")
    } else if d.num_hours() > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    };
    format!("{sign}{body}")
}

pub fn format_due(due: DateTime<Utc>) -> String {
    due.with_timezone(&Local)
        .format("%a %b %d %H:%M")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn item(source: Source, name: &str, due: Option<DateTime<Utc>>) -> Assignment {
        Assignment {
            source,
            id: name.into(),
            course: "C".into(),
            name: name.into(),
            due,
            late_due: None,
            status: Status::Todo,
            points: None,
            url: None,
        }
    }

    #[test]
    fn dedupe_drops_canvas_mirror() {
        let t = Utc.with_ymd_and_hms(2026, 10, 20, 4, 59, 0).unwrap();
        let items = vec![
            item(Source::Canvas, "Homework 3", Some(t)),
            item(
                Source::Gradescope,
                "Homework #3",
                Some(t + Duration::hours(1)),
            ),
            item(Source::Canvas, "Homework 4", Some(t)),
        ];
        let out = dedupe(items);
        let names: Vec<_> = out.iter().map(|a| (a.source, a.name.as_str())).collect();
        assert_eq!(
            names,
            vec![
                (Source::Gradescope, "Homework #3"),
                (Source::Canvas, "Homework 4")
            ]
        );
    }

    #[test]
    fn dedupe_keeps_far_apart() {
        let t = Utc.with_ymd_and_hms(2026, 10, 20, 4, 59, 0).unwrap();
        let items = vec![
            item(Source::Canvas, "Quiz", Some(t)),
            item(Source::Gradescope, "Quiz", Some(t + Duration::days(7))),
        ];
        assert_eq!(dedupe(items).len(), 2);
    }

    #[test]
    fn left_formatting() {
        let now = Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap();
        assert_eq!(format_left(now + Duration::minutes(5), now), "5m");
        assert_eq!(format_left(now + Duration::minutes(125), now), "2h 5m");
        assert_eq!(format_left(now + Duration::hours(50), now), "2d 2h");
        assert_eq!(format_left(now + Duration::days(9), now), "9d");
        assert_eq!(format_left(now - Duration::hours(3), now), "-3h 0m");
    }

    #[test]
    fn sorting_puts_undated_last() {
        let t = Utc.with_ymd_and_hms(2026, 10, 20, 0, 0, 0).unwrap();
        let mut items = vec![
            item(Source::Canvas, "b", None),
            item(Source::Canvas, "a", Some(t + Duration::days(1))),
            item(Source::Canvas, "c", Some(t)),
        ];
        sort(&mut items);
        let names: Vec<_> = items.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["c", "a", "b"]);
    }
}
