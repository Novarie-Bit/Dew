use std::fs;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::canvas::Canvas;
use crate::config::{Config, Credentials, Paths, write_file};
use crate::gradescope::Gradescope;
use crate::model::{self, Assignment, Source};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cache {
    pub fetched_at: Option<DateTime<Utc>>,
    pub assignments: Vec<Assignment>,
}

impl Cache {
    pub fn load(paths: &Paths) -> Cache {
        fs::read_to_string(&paths.cache)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_file(&paths.cache, &serde_json::to_string(self)?, 0o600)
    }
}

pub struct Report {
    pub cache: Cache,
    pub errors: Vec<String>,
}

/// Fetches both services in parallel. If one fails, its previously cached
/// items are kept so the list doesn't go blank on a flaky connection.
pub fn sync(cfg: &Config, paths: &Paths) -> Report {
    let creds = Credentials::load(paths).unwrap_or_default();
    let previous = Cache::load(paths);

    let (canvas, gradescope) = std::thread::scope(|s| {
        let canvas = s.spawn(|| -> Option<Result<Vec<Assignment>>> {
            let c = creds.canvas.as_ref()?;
            Some(Canvas::from_creds(c, paths).and_then(|c| c.fetch_all()))
        });
        let gradescope = s.spawn(|| -> Option<Result<Vec<Assignment>>> {
            let gs = match Gradescope::load(&paths.gradescope_cookies) {
                Ok(gs) => gs,
                Err(e) => return Some(Err(e)),
            };
            if !gs.has_session() {
                return None;
            }
            Some(gs.fetch_all(cfg.gradescope_all_terms))
        });
        (
            canvas.join().ok().flatten(),
            gradescope.join().ok().flatten(),
        )
    });

    let mut items = Vec::new();
    let mut errors = Vec::new();
    for (source, result) in [(Source::Canvas, canvas), (Source::Gradescope, gradescope)] {
        match result {
            None => {}
            Some(Ok(v)) => items.extend(v),
            Some(Err(e)) => {
                errors.push(format!("{}: {e:#}", source.label()));
                items.extend(
                    previous
                        .assignments
                        .iter()
                        .filter(|a| a.source == source)
                        .cloned(),
                );
            }
        }
    }
    if cfg.dedupe {
        items = model::dedupe(items);
    }
    model::sort(&mut items);
    let mut hidden = crate::hidden::Hidden::load(paths);
    let before = hidden.len();
    hidden.retain_existing(&items);
    if hidden.len() != before {
        let _ = hidden.save(paths);
    }

    let cache = Cache {
        fetched_at: Some(Utc::now()),
        assignments: items,
    };
    if let Err(e) = cache.save(paths) {
        errors.push(format!("saving cache: {e:#}"));
    }
    Report { cache, errors }
}
