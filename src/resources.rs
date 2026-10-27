//! The *resources file*: a Markdown bookmark log of `---`-delimited entries,
//! each a list item of the form `- [<timestamp>] [<title>](<url>)`
//! (links.md "Resources file"; see `resources-exemplar.md`).

use std::path::Path;

use eyre::{Context, Result};
use tracing::debug;

/// One bookmark from the resources file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// When the entry was added, as written in the file (RFC 3339).
    pub timestamp: String,
    /// The resource's title. May be empty.
    pub title: String,
    /// The resource's location.
    pub url: String,
}

impl Entry {
    /// The calendar date of the entry, `YYYY-MM-DD`, taken from the front of
    /// the timestamp so no time-zone conversion is done.
    pub fn date(&self) -> &str {
        self.timestamp.get(..10).unwrap_or(&self.timestamp)
    }
}

/// Read and parse a resources file. Lines that are not entries (the front
/// matter, headings, `---` separators) are skipped.
pub fn read(path: &Path) -> Result<Vec<Entry>> {
    let text =
        std::fs::read_to_string(path).wrap_err_with(|| format!("reading {}", path.display()))?;
    Ok(parse(&text))
}

/// Parse the entries out of the text of a resources file.
pub fn parse(text: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|line| {
            let entry = parse_line(line);
            if entry.is_none() && line.trim_start().starts_with('-') && line.contains("](") {
                debug!(line, "skipping line that is not a resource entry");
            }
            entry
        })
        .collect()
}

/// Parse one `- [<timestamp>] [<title>](<url>)` line. The title may itself
/// contain square brackets (arXiv titles do), so the link is split at the
/// last `](` rather than the first `]`.
pub fn parse_line(line: &str) -> Option<Entry> {
    let s = line.trim();
    let s = s
        .strip_prefix('-')
        .or_else(|| s.strip_prefix('*'))?
        .trim_start();
    let s = s.strip_prefix('[')?;
    let (timestamp, rest) = s.split_once(']')?;
    let timestamp = timestamp.trim();
    // A timestamp starts with a four-digit year; anything else is a plain
    // Markdown link, not an entry.
    if timestamp.len() < 4 || !timestamp.bytes().take(4).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let rest = rest.trim_start().strip_prefix('[')?.strip_suffix(')')?;
    let split = rest.rfind("](")?;
    let title = rest[..split].trim();
    let url = rest[split + 2..].trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    Some(Entry {
        timestamp: timestamp.to_owned(),
        title: title.to_owned(),
        url: url.to_owned(),
    })
}

/// The URLs of `entries` in first-seen order, without duplicates: the
/// output of the URL-extraction stage.
pub fn urls(entries: &[Entry]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    entries
        .iter()
        .filter(|e| seen.insert(e.url.as_str()))
        .map(|e| e.url.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXEMPLAR: &str = include_str!("../tests/fixtures/resources-exemplar.md");

    #[test]
    fn parses_exemplar_entries() {
        let entries = parse(EXEMPLAR);
        assert_eq!(entries.len(), 12);
        assert_eq!(entries[0].timestamp, "2026-10-27T10:44:46+10:00");
        assert_eq!(entries[0].date(), "2026-10-27");
        assert_eq!(
            entries[0].title,
            "[2610.30188] Agentic Verified Optimization of Real-World Software"
        );
        assert_eq!(entries[0].url, "https://arxiv.org/pdf/2610.30188");
        assert_eq!(entries[11].url, "https://already-clean.goodfire.com");
    }

    #[test]
    fn tolerates_leading_space_and_asterisks() {
        let a = parse_line(" - [2026-01-01T00:00:00Z] [t](https://a.example)").unwrap();
        let b = parse_line("* [2026-01-01T00:00:00Z] [t](https://a.example)").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn rejects_links_without_timestamp_or_scheme() {
        assert!(parse_line("- [Aqueduct](https://aqueduct.verina.io)").is_none());
        assert!(parse_line("- [2026-01-01] [t](ftp://a.example)").is_none());
        assert!(parse_line("---").is_none());
        assert!(parse_line("created: 2021-08-19T01:20:15+00:00").is_none());
    }

    #[test]
    fn urls_are_deduplicated_in_order() {
        let entries = parse(
            "- [2026-01-01] [a](https://a.example)\n- [2026-01-02] [b](https://b.example)\n- [2026-01-03] [a again](https://a.example)\n",
        );
        assert_eq!(
            urls(&entries),
            vec!["https://a.example", "https://b.example"]
        );
    }
}
