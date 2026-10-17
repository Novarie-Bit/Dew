//! Assignments the user has hidden ("removed") from the list.

use std::collections::BTreeSet;
use std::fs;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::{Paths, write_file};
use crate::model::Assignment;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Hidden {
    keys: BTreeSet<String>,
}

pub fn key(a: &Assignment) -> String {
    format!("{}:{}", a.source.short(), a.id)
}

impl Hidden {
    pub fn load(paths: &Paths) -> Hidden {
        fs::read_to_string(&paths.hidden)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        write_file(&paths.hidden, &serde_json::to_string_pretty(self)?, 0o644)
    }

    pub fn contains(&self, a: &Assignment) -> bool {
        self.keys.contains(&key(a))
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Hides `a`, or unhides it if it was hidden. Returns whether it's now hidden.
    pub fn toggle(&mut self, a: &Assignment) -> bool {
        let k = key(a);
        if self.keys.remove(&k) {
            false
        } else {
            self.keys.insert(k);
            true
        }
    }

    pub fn hide_keys(&mut self, keys: impl IntoIterator<Item = String>) {
        self.keys.extend(keys);
    }

    pub fn unhide_keys(&mut self, keys: &[String]) {
        for k in keys {
            self.keys.remove(k);
        }
    }

    /// Forgets hidden items that no longer exist on either site.
    pub fn retain_existing(&mut self, items: &[Assignment]) {
        let live: BTreeSet<String> = items.iter().map(key).collect();
        self.keys.retain(|k| live.contains(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Source, Status};

    fn item(source: Source, id: &str) -> Assignment {
        Assignment {
            source,
            id: id.into(),
            course: String::new(),
            name: id.into(),
            due: None,
            late_due: None,
            status: Status::Missing,
            points: None,
            url: None,
        }
    }

    #[test]
    fn toggle_and_prune() {
        let (a, b) = (item(Source::Canvas, "1"), item(Source::Gradescope, "1"));
        let mut h = Hidden::default();
        assert!(h.toggle(&a));
        assert!(h.contains(&a));
        // Same id on the other site is a different assignment.
        assert!(!h.contains(&b));
        h.hide_keys([key(&b)]);
        assert_eq!(h.len(), 2);
        h.retain_existing(std::slice::from_ref(&b));
        assert!(!h.contains(&a) && h.contains(&b));
        assert!(!h.toggle(&b));
        assert_eq!(h.len(), 0);
    }
}
