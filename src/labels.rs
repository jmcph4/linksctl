//! The *category labelling*: URL to category and subcategory (links.md
//! "Category labelling"). Stored either as a JSON file or as a `categories`
//! table alongside Webtronix's `pages` table in the page index.

use std::path::Path;

use eyre::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::taxonomy::Taxonomy;

/// The first bytes of every SQLite database file.
const SQLITE_MAGIC: &[u8] = b"SQLite format 3\0";

/// One page's label.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Label {
    /// The page's URL exactly as in the resources file.
    pub url: String,
    /// Its category.
    pub category: String,
    /// Its subcategory.
    pub subcategory: String,
    /// The page's title as fetched, when known. Carried along so the build
    /// step can fall back to it for entries whose resources-file title is
    /// empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// A set of labels and the taxonomy they were drawn from: the output of
/// the categorisation stage.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Labelling {
    /// The taxonomy the model proposed for the collection. Absent only in
    /// a labelling that has never been categorised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taxonomy: Option<Taxonomy>,
    /// The labels, one per URL.
    pub labels: Vec<Label>,
}

impl Labelling {
    /// Read a labelling from `path`, which is either a JSON file written by
    /// [`Labelling::write_json`] or a page index holding a `categories`
    /// table.
    pub fn read(path: &Path) -> Result<Self> {
        if is_sqlite(path)? {
            Self::read_table(path)
        } else {
            let text = std::fs::read_to_string(path)
                .wrap_err_with(|| format!("reading {}", path.display()))?;
            serde_json::from_str(&text).wrap_err_with(|| format!("parsing {}", path.display()))
        }
    }

    /// Write the labelling as pretty-printed JSON.
    pub fn write_json(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text).wrap_err_with(|| format!("writing {}", path.display()))
    }

    /// Upsert the labels into the `categories` table of the database at
    /// `path` and store the taxonomy, creating the tables when needed.
    /// Rows for other URLs are left alone unless `replace`, which clears
    /// them first (the taxonomy they were drawn from is gone).
    pub fn write_table(&self, path: &Path, replace: bool) -> Result<()> {
        let mut db = open(path)?;
        let tx = db.transaction()?;
        if replace {
            tx.execute("DELETE FROM categories", [])?;
        }
        if let Some(t) = &self.taxonomy {
            tx.execute(
                "INSERT INTO taxonomy (id, json) VALUES (1, ?1)
                 ON CONFLICT(id) DO UPDATE SET json = excluded.json",
                params![serde_json::to_string(t)?],
            )?;
        }
        {
            let mut stmt = tx.prepare(
                "INSERT INTO categories (url, category, subcategory, title)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(url) DO UPDATE SET
                     category = excluded.category,
                     subcategory = excluded.subcategory,
                     title = excluded.title",
            )?;
            for l in &self.labels {
                stmt.execute(params![l.url, l.category, l.subcategory, l.title])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Read the taxonomy and every row of the `categories` table of the
    /// database at `path`.
    pub fn read_table(path: &Path) -> Result<Self> {
        let db = open(path)?;
        let taxonomy = db
            .query_row("SELECT json FROM taxonomy WHERE id = 1", [], |row| {
                row.get::<_, String>(0)
            })
            .optional()?
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .wrap_err("parsing the stored taxonomy")?;
        let mut stmt =
            db.prepare("SELECT url, category, subcategory, title FROM categories ORDER BY url")?;
        let labels = stmt
            .query_map([], |row| {
                Ok(Label {
                    url: row.get(0)?,
                    category: row.get(1)?,
                    subcategory: row.get(2)?,
                    title: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { taxonomy, labels })
    }
}

/// Whether the file at `path` is a SQLite database (rather than JSON).
fn is_sqlite(path: &Path) -> Result<bool> {
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).wrap_err_with(|| format!("opening {}", path.display()))?;
    let mut head = [0u8; 16];
    let n = file.read(&mut head)?;
    Ok(&head[..n] == SQLITE_MAGIC)
}

/// Open the database at `path`, creating the `categories` and `taxonomy`
/// tables if needed.
fn open(path: &Path) -> Result<Connection> {
    let db = Connection::open(path).wrap_err_with(|| format!("opening {}", path.display()))?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS categories (
             url         TEXT PRIMARY KEY,
             category    TEXT NOT NULL,
             subcategory TEXT NOT NULL,
             title       TEXT
         );
         CREATE TABLE IF NOT EXISTS taxonomy (
             id   INTEGER PRIMARY KEY CHECK (id = 1),
             json TEXT NOT NULL
         );",
    )?;
    Ok(db)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Labelling {
        Labelling {
            taxonomy: Some(Taxonomy {
                topics: vec![crate::taxonomy::Topic {
                    name: "systems".into(),
                    subs: vec!["performance".into()],
                }],
            }),
            labels: vec![
                Label {
                    url: "https://a.example/1".into(),
                    category: "systems".into(),
                    subcategory: "performance".into(),
                    title: Some("A".into()),
                },
                Label {
                    url: "https://b.example".into(),
                    category: "science".into(),
                    subcategory: "physics & space".into(),
                    title: None,
                },
            ],
        }
    }

    #[test]
    fn json_and_table_round_trip() {
        let dir = std::env::temp_dir().join(format!("linksctl-labels-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let json = dir.join("c.json");
        let db = dir.join("c.db");
        sample().write_json(&json).unwrap();
        sample().write_table(&db, false).unwrap();
        assert_eq!(Labelling::read(&json).unwrap(), sample());
        assert_eq!(Labelling::read(&db).unwrap(), sample());
        // A second write updates rather than duplicates.
        let mut again = sample();
        again.labels[0].category = "security".into();
        again.write_table(&db, false).unwrap();
        let read = Labelling::read(&db).unwrap();
        assert_eq!(read.labels.len(), 2);
        assert_eq!(read.labels[0].category, "security");
        // A replacing write drops the rows it is not given.
        let mut one = sample();
        one.labels.truncate(1);
        one.write_table(&db, true).unwrap();
        assert_eq!(Labelling::read(&db).unwrap().labels.len(), 1);
        // A labelling without a taxonomy keeps the stored one.
        Labelling::default().write_table(&db, false).unwrap();
        assert_eq!(Labelling::read(&db).unwrap().taxonomy, sample().taxonomy);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
