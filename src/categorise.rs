//! Categorisation: have an LLM file each page in the index under a
//! category and subcategory (links.md "Categorisation",
//! `linksctl categorise`).
//!
//! Two phases. First the model is shown the whole collection (domain and
//! title of every page) and proposes the taxonomy; then every page is
//! labelled against that taxonomy in batches. The taxonomy is stored with
//! the labels so a later run labels new pages against the same one.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use eyre::{Context, Result};
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use tracing::{info, warn};
use webtronix::Index;

use crate::{
    build::host_of,
    labels::{Label, Labelling},
    llm::Llm,
    resources::Entry,
    taxonomy::{Taxonomy, UNFILED_SUB, UNFILED_TOPIC},
};

/// How the categoriser batches and trims its input.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Pages sent in one labelling request.
    pub batch_size: usize,
    /// Characters of page text sent per page when labelling, taken from
    /// the stored HTML with its markup removed.
    pub max_chars: usize,
    /// Labelling requests in flight at once.
    pub concurrency: usize,
    /// Characters of the collection listing sent when proposing the
    /// taxonomy; a larger collection is sampled evenly down to this.
    pub taxonomy_chars: usize,
    /// Discard the stored taxonomy and labels and start over.
    pub force: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            batch_size: 20,
            max_chars: 2000,
            concurrency: 8,
            taxonomy_chars: 300_000,
            force: false,
        }
    }
}

/// What the model sees of one page.
#[derive(Clone, Debug)]
struct Item {
    url: String,
    title: String,
    text: String,
}

/// Label the pages in the index at `index` (plus any entry in `entries`
/// the index lacks, from its title alone). The taxonomy comes from
/// `existing` when it has one and the run is not forced; otherwise the
/// model proposes one from the whole collection. URLs already labelled in
/// `existing` are skipped unless forced. The result carries the taxonomy
/// used and the new labels only.
pub async fn categorise(
    index: &Path,
    entries: &[Entry],
    existing: &Labelling,
    llm: &Llm,
    settings: &Settings,
) -> Result<Labelling> {
    let all = collect_items(index, entries, settings)?;
    if all.is_empty() {
        info!("nothing to categorise");
        return Ok(Labelling {
            taxonomy: existing.taxonomy.clone(),
            labels: Vec::new(),
        });
    }
    let taxonomy = match &existing.taxonomy {
        Some(t) if !settings.force => {
            info!(topics = t.topics.len(), "using the stored taxonomy");
            t.clone()
        }
        _ => discover(&all, llm, settings).await?,
    };
    let done: HashSet<&str> = if settings.force {
        HashSet::new()
    } else {
        existing.labels.iter().map(|l| l.url.as_str()).collect()
    };
    let todo: Vec<Item> = all
        .into_iter()
        .filter(|i| !done.contains(i.url.as_str()))
        .collect();
    let labels = label(&todo, &taxonomy, llm, settings).await?;
    Ok(Labelling {
        taxonomy: Some(taxonomy),
        labels,
    })
}

/// Phase one: show the model the collection and take its taxonomy.
async fn discover(items: &[Item], llm: &Llm, settings: &Settings) -> Result<Taxonomy> {
    let lines: Vec<String> = items.iter().map(listing_line).collect();
    let total: usize = lines.iter().map(|l| l.len() + 1).sum();
    let budget = settings.taxonomy_chars.max(1);
    let step = total.div_ceil(budget).max(1);
    let shown: Vec<&str> = lines.iter().step_by(step).map(String::as_str).collect();
    info!(
        pages = items.len(),
        shown = shown.len(),
        "asking the model for a taxonomy"
    );
    let sampled = if step > 1 {
        format!(
            " Every {step}th page is listed below, {} in all.",
            shown.len()
        )
    } else {
        String::new()
    };
    let user = format!(
        "The collection has {} saved pages.{sampled} One per line, as `domain — title`:\n\n{}",
        items.len(),
        shown.join("\n")
    );
    let value = llm
        .complete_json(DISCOVER_SYSTEM, &user, &discover_schema())
        .await
        .wrap_err("proposing the taxonomy")?;
    let taxonomy: Taxonomy =
        serde_json::from_value(value).wrap_err("reading the proposed taxonomy")?;
    let taxonomy = taxonomy.sanitise()?;
    info!(
        topics = taxonomy.topics.len() - 1,
        subcategories = taxonomy.topics.iter().map(|t| t.subs.len()).sum::<usize>() - 1,
        "taxonomy proposed:\n{}",
        taxonomy.describe()
    );
    Ok(taxonomy)
}

/// Instructions for proposing the taxonomy.
const DISCOVER_SYSTEM: &str = "You are designing the topical taxonomy for a directory of one \
person's saved web pages. From the collection you are given, propose the categories and, under \
each, the subcategories, so that every page has a natural home and the directory is pleasant to \
browse. Use short lowercase English labels of one to four words, with `&` for and, specific to \
what this collection actually contains rather than generic. Choose as many categories as the \
collection needs, typically 4 to 15, each with 2 to 8 subcategories, and order the categories \
from the largest share of the collection to the smallest. Do not include a catch-all such as \
other or miscellaneous; one is added afterwards for pages that cannot be filed.";

/// The response schema for the taxonomy.
fn discover_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "topics": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "subs": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["name", "subs"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["topics"],
        "additionalProperties": false
    })
}

/// One page as listed for the taxonomy: its domain and title, or its URL
/// when it has no title.
fn listing_line(item: &Item) -> String {
    let host = host_of(&item.url).unwrap_or_default();
    if item.title.trim().is_empty() {
        format!("{host} — {}", item.url)
    } else {
        format!("{host} — {}", item.title.trim())
    }
}

/// Phase two: label `items` against `taxonomy` in concurrent batches.
async fn label(
    items: &[Item],
    taxonomy: &Taxonomy,
    llm: &Llm,
    settings: &Settings,
) -> Result<Vec<Label>> {
    if items.is_empty() {
        info!("every page is already labelled");
        return Ok(Vec::new());
    }
    let batches: Vec<Vec<Item>> = items
        .chunks(settings.batch_size.max(1))
        .map(<[Item]>::to_vec)
        .collect();
    info!(pages = items.len(), batches = batches.len(), "labelling");
    let system = label_system(taxonomy);
    let schema = label_schema(taxonomy);
    let total = batches.len();
    let mut results = stream::iter(batches.into_iter().enumerate())
        .map(|(i, batch)| {
            let system = &system;
            let schema = &schema;
            async move {
                let user = label_user(&batch);
                let out = llm.complete_json(system, &user, schema).await;
                (i, batch, out)
            }
        })
        .buffer_unordered(settings.concurrency.max(1));
    let mut labels = Vec::new();
    let mut done = 0usize;
    while let Some((i, batch, out)) = results.next().await {
        done += 1;
        match out {
            Ok(value) => {
                let got = parse_labels(&value, &batch, taxonomy);
                info!(
                    batch = i + 1,
                    done,
                    total,
                    labelled = got.len(),
                    "batch done"
                );
                labels.extend(got);
            }
            Err(e) => warn!(
                batch = i + 1,
                "batch failed, its pages stay unlabelled: {e:#}"
            ),
        }
    }
    labels.sort_by(|a, b| a.url.cmp(&b.url));
    Ok(labels)
}

/// Gather every page: index rows first, then resources-file entries the
/// index has no row for (fetch failed, or never run), title only.
fn collect_items(index: &Path, entries: &[Entry], settings: &Settings) -> Result<Vec<Item>> {
    let titles: HashMap<&str, &str> = entries
        .iter()
        .map(|e| (e.url.as_str(), e.title.as_str()))
        .collect();
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    if index.exists() {
        let pages = Index::open(index)
            .wrap_err("opening page index")?
            .pages()
            .wrap_err("reading page index")?;
        for page in pages {
            seen.insert(page.url.clone());
            let title = page
                .title
                .filter(|t| !t.trim().is_empty())
                .or_else(|| titles.get(page.url.as_str()).map(|t| (*t).to_owned()))
                .unwrap_or_default();
            // The index holds rendered HTML for pages (and text for PDFs);
            // the model gets its visible text, bounded.
            let text = if page.mime.ends_with("html") || page.mime.ends_with("xhtml+xml") {
                strip_html(&page.text)
            } else {
                page.text
            };
            items.push(Item {
                url: page.url,
                title,
                text: trim_text(&text, settings.max_chars),
            });
        }
    }
    for e in entries {
        if seen.insert(e.url.clone()) {
            items.push(Item {
                url: e.url.clone(),
                title: e.title.clone(),
                text: String::new(),
            });
        }
    }
    Ok(items)
}

/// The visible text of an HTML document, roughly: comments and the
/// contents of `script`, `style`, `noscript`, `svg` and `template`
/// elements dropped, every other tag replaced by a space, and the common
/// character entities decoded. This is only to give the model something
/// readable within its character budget; the index keeps the HTML.
fn strip_html(html: &str) -> String {
    const OPAQUE: [&str; 5] = ["script", "style", "noscript", "svg", "template"];
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len() / 4);
    let mut i = 0;
    while i < html.len() {
        if bytes[i] != b'<' {
            let c = html[i..].chars().next().expect("at a char boundary");
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        let rest = &lower[i..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(html.len(), |j| i + j + 3);
            continue;
        }
        let opaque = OPAQUE.iter().find(|tag| {
            rest[1..].starts_with(*tag)
                && !rest[1 + tag.len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric())
        });
        if let Some(tag) = opaque {
            let close = format!("</{tag}");
            i = rest
                .find(&close)
                .map_or(html.len(), |j| i + j + close.len());
        }
        i = lower[i..].find('>').map_or(html.len(), |j| i + j + 1);
        out.push(' ');
    }
    for (entity, c) in [
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
        ("&nbsp;", " "),
        ("&amp;", "&"),
    ] {
        out = out.replace(entity, c);
    }
    out
}

/// The first `max_chars` characters of `text` with whitespace collapsed.
fn trim_text(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut n = 0;
    for word in text.split_whitespace() {
        let cost = word.chars().count() + usize::from(!out.is_empty());
        if n + cost > max_chars {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        n += cost;
    }
    out
}

/// The labelling instructions, including the taxonomy to pick from.
fn label_system(taxonomy: &Taxonomy) -> String {
    format!(
        "You file saved web pages into a fixed topical taxonomy. For each page you are given \
         its URL, its title and the beginning of its text (the text may be missing). Choose \
         exactly one category and one subcategory for each page from the taxonomy below, \
         copying both names exactly as written. File a page by what it is about, not by the \
         site that hosts it. When the title and text give too little signal to decide, use \
         \"{UNFILED_TOPIC}\" / \"{UNFILED_SUB}\". Return one label per page, with the url \
         copied exactly as given.\n\nTaxonomy (category: subcategories):\n{}",
        taxonomy.describe()
    )
}

/// The pages of one batch, numbered.
fn label_user(batch: &[Item]) -> String {
    let mut s = format!("Label these {} pages.\n", batch.len());
    for (i, item) in batch.iter().enumerate() {
        s.push_str(&format!(
            "\n### page {}\nurl: {}\ntitle: {}\n",
            i + 1,
            item.url,
            item.title
        ));
        if !item.text.is_empty() {
            s.push_str("text: ");
            s.push_str(&item.text);
            s.push('\n');
        }
    }
    s
}

/// The labelling schema: a list of labels whose category and subcategory
/// are a valid pair, expressed as one `anyOf` branch per category.
fn label_schema(taxonomy: &Taxonomy) -> Value {
    let branches: Vec<Value> = taxonomy
        .topics
        .iter()
        .map(|t| {
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "category": { "const": t.name },
                    "subcategory": { "enum": t.subs },
                },
                "required": ["url", "category", "subcategory"],
                "additionalProperties": false,
            })
        })
        .collect();
    json!({
        "type": "object",
        "properties": {
            "labels": { "type": "array", "items": { "anyOf": branches } }
        },
        "required": ["labels"],
        "additionalProperties": false,
    })
}

/// Turn a response into labels for the pages of `batch`. Pages the model
/// skipped stay unlabelled; a pair outside the taxonomy becomes unfiled.
fn parse_labels(value: &Value, batch: &[Item], taxonomy: &Taxonomy) -> Vec<Label> {
    let by_url: HashMap<&str, &Item> = batch.iter().map(|i| (i.url.as_str(), i)).collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for l in value["labels"].as_array().into_iter().flatten() {
        let Some(url) = l["url"].as_str() else {
            continue;
        };
        let Some(item) = by_url.get(url) else {
            warn!(url, "model returned a label for a URL it was not given");
            continue;
        };
        if !seen.insert(url) {
            continue;
        }
        let (mut category, mut subcategory) = (
            l["category"].as_str().unwrap_or("").to_owned(),
            l["subcategory"].as_str().unwrap_or("").to_owned(),
        );
        if !taxonomy.contains(&category, &subcategory) {
            warn!(
                url,
                category, subcategory, "label outside the taxonomy, filing as unfiled"
            );
            category = UNFILED_TOPIC.to_owned();
            subcategory = UNFILED_SUB.to_owned();
        }
        out.push(Label {
            url: url.to_owned(),
            category,
            subcategory,
            title: Some(item.title.clone()).filter(|t| !t.is_empty()),
        });
    }
    for item in batch {
        if !seen.contains(item.url.as_str()) {
            warn!(url = item.url, "model returned no label for this page");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::taxonomy::Topic;

    fn item(url: &str) -> Item {
        Item {
            url: url.into(),
            title: "t".into(),
            text: String::new(),
        }
    }

    fn taxonomy() -> Taxonomy {
        Taxonomy {
            topics: vec![
                Topic {
                    name: "systems".into(),
                    subs: vec!["performance".into()],
                },
                Topic {
                    name: "science".into(),
                    subs: vec!["physics".into()],
                },
            ],
        }
        .sanitise()
        .unwrap()
    }

    #[test]
    fn trim_text_collapses_and_bounds() {
        assert_eq!(trim_text("a  b\n\nc", 100), "a b c");
        assert_eq!(trim_text("aaaa bbbb cccc", 9), "aaaa bbbb");
        assert_eq!(trim_text("", 10), "");
    }

    #[test]
    fn strip_html_keeps_visible_text_only() {
        let html = "<!doctype html><html><head><title>T</title><style>p{}</style>\
                    <script>var x = '<b>no</b>';</script></head><body><!-- c -->\
                    <p class=\"a\">Hello&nbsp;<b>world</b> &amp; friends &lt;3</p>\
                    <noscript>enable js</noscript><svg><text>icon</text></svg>\
                    <scripts>not a script tag</scripts></body></html>";
        assert_eq!(
            trim_text(&strip_html(html), 1000),
            "T Hello world & friends <3 not a script tag"
        );
        assert_eq!(strip_html("plain < text"), "plain  ");
        assert_eq!(strip_html("<p>unterminated"), " unterminated");
        assert_eq!(strip_html("<script>never closed"), " ");
    }

    #[test]
    fn listing_line_uses_host_and_title_or_url() {
        let mut i = item("https://www.a.example/p");
        assert_eq!(listing_line(&i), "a.example — t");
        i.title = " ".into();
        assert_eq!(listing_line(&i), "a.example — https://www.a.example/p");
    }

    #[test]
    fn parse_labels_validates_pairs_and_urls() {
        let t = taxonomy();
        let batch = vec![item("https://a"), item("https://b"), item("https://c")];
        let v = json!({"labels": [
            {"url": "https://a", "category": "systems", "subcategory": "physics"},
            {"url": "https://b", "category": "systems", "subcategory": "performance"},
            {"url": "https://z", "category": "systems", "subcategory": "performance"},
        ]});
        let got = parse_labels(&v, &batch, &t);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].category, UNFILED_TOPIC);
        assert_eq!(got[1].subcategory, "performance");
        assert_eq!(got[1].title.as_deref(), Some("t"));
    }

    #[test]
    fn schemas_have_the_expected_shape() {
        let t = taxonomy();
        let s = label_schema(&t);
        assert_eq!(
            s["properties"]["labels"]["items"]["anyOf"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let d = discover_schema();
        assert_eq!(
            d["properties"]["topics"]["items"]["required"],
            json!(["name", "subs"])
        );
    }
}
