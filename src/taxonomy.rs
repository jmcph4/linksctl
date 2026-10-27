//! The categories and subcategories pages are filed under.
//!
//! The taxonomy is never supplied by the user: the categoriser has the
//! model derive one from the collection itself (see
//! [`crate::categorise`]), and it is stored with the labels so later runs
//! and the build step use the same one.

use eyre::{Result, bail};
use serde::{Deserialize, Serialize};

/// The category a page cannot be filed under with confidence. Appended to
/// every taxonomy, never proposed by the model.
pub const UNFILED_TOPIC: &str = "everything else";
/// Its subcategory, worded as on the exemplar pages.
pub const UNFILED_SUB: &str = "unfiled (too little signal in titles or saved pages)";

/// A top-level category and its subcategories.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Topic {
    /// The category label.
    pub name: String,
    /// Its subcategory labels, in display order.
    pub subs: Vec<String>,
}

/// The whole taxonomy, in display order.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Taxonomy {
    /// The categories, in the order the directory pages list them.
    pub topics: Vec<Topic>,
}

impl Taxonomy {
    /// Clean a model-proposed taxonomy: labels are trimmed, empty ones and
    /// duplicates (ignoring case) dropped along with any catch-all the
    /// model added anyway, and the unfiled bucket appended last so every
    /// page has somewhere to go. Fails if nothing usable is left.
    pub fn sanitise(self) -> Result<Self> {
        let mut seen_topics = std::collections::HashSet::new();
        let mut topics = Vec::new();
        for t in self.topics {
            let name = t.name.trim().to_owned();
            if name.is_empty()
                || name.eq_ignore_ascii_case(UNFILED_TOPIC)
                || !seen_topics.insert(name.to_lowercase())
            {
                continue;
            }
            let mut seen_subs = std::collections::HashSet::new();
            let subs: Vec<String> = t
                .subs
                .into_iter()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty() && seen_subs.insert(s.to_lowercase()))
                .collect();
            if subs.is_empty() {
                continue;
            }
            topics.push(Topic { name, subs });
        }
        if topics.is_empty() {
            bail!("the model proposed no usable categories");
        }
        topics.push(Topic {
            name: UNFILED_TOPIC.to_owned(),
            subs: vec![UNFILED_SUB.to_owned()],
        });
        Ok(Self { topics })
    }

    /// Whether `(topic, sub)` is a pair in this taxonomy.
    pub fn contains(&self, topic: &str, sub: &str) -> bool {
        self.topics
            .iter()
            .any(|t| t.name == topic && t.subs.iter().any(|s| s == sub))
    }

    /// The taxonomy as one line per topic, for a prompt.
    pub fn describe(&self) -> String {
        self.topics
            .iter()
            .map(|t| format!("- {}: {}", t.name, t.subs.join("; ")))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn topic(name: &str, subs: &[&str]) -> Topic {
        Topic {
            name: name.into(),
            subs: subs.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn sanitise_cleans_and_appends_unfiled() {
        let raw = Taxonomy {
            topics: vec![
                topic(" systems ", &["rust", " Rust", "", "networking"]),
                topic("Systems", &["dup"]),
                topic("empty", &["", "  "]),
                topic("Everything Else", &["misc"]),
                topic("science", &["physics"]),
            ],
        };
        let t = raw.sanitise().unwrap();
        assert_eq!(t.topics.len(), 3);
        assert_eq!(t.topics[0], topic("systems", &["rust", "networking"]));
        assert_eq!(t.topics[1], topic("science", &["physics"]));
        assert_eq!(t.topics[2], topic(UNFILED_TOPIC, &[UNFILED_SUB]));
        assert!(t.contains("systems", "rust"));
        assert!(!t.contains("systems", "physics"));
        assert!(t.contains(UNFILED_TOPIC, UNFILED_SUB));
        assert!(Taxonomy::default().sanitise().is_err());
        assert!(
            Taxonomy {
                topics: vec![topic("x", &[""])]
            }
            .sanitise()
            .is_err()
        );
    }

    #[test]
    fn describe_lists_one_topic_per_line() {
        let t = Taxonomy {
            topics: vec![topic("a", &["x", "y"]), topic("b", &["z"])],
        };
        assert_eq!(t.describe(), "- a: x; y\n- b: z");
    }
}
