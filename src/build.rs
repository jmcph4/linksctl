//! Artefact generation: the private and public directory pages
//! (links.md "Artefact generation", `linksctl build`). The pages follow the
//! exemplars `directory-private.html` and `directory-public.html`: one
//! HTML file each with the stylesheet, a pre-rendered board, the data as
//! embedded JSON and the script that re-renders the board from it.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
};

use eyre::{Context, Result, bail};
use serde_json::{Map, Value, json};
use tracing::{info, warn};

use crate::{
    labels::Labelling,
    resources::Entry,
    taxonomy::{Taxonomy, UNFILED_SUB, UNFILED_TOPIC},
};

/// The exemplar pages' stylesheet.
const CSS: &str = include_str!("assets/directory.css");
/// The exemplar pages' script. It reads the `#data` JSON, re-renders the
/// board on search, filter and sort, and reveals a domain's saved pages on
/// the private page.
const JS: &str = include_str!("assets/directory.js");
/// Domains shown per subcategory before the "more" button, as in the
/// script's `CAP`.
const CAP: usize = 14;
/// Output file names.
pub const PRIVATE_FILE: &str = "directory-private.html";
pub const PUBLIC_FILE: &str = "directory-public.html";

/// Which pages to build and where.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Build the private page.
    pub private: bool,
    /// Build the public page.
    pub public: bool,
    /// The page title.
    pub title: String,
    /// Directory the pages are written into.
    pub out_dir: PathBuf,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            private: true,
            public: true,
            title: "resources directory".to_owned(),
            out_dir: PathBuf::from("."),
        }
    }
}

/// URLs and domains to leave out of the pages (links.md `linksctl build`).
#[derive(Clone, Debug, Default)]
pub struct Banlist {
    urls: HashSet<String>,
    domains: Vec<String>,
}

impl Banlist {
    /// Read a banlist: one URL or domain name per line, blank lines and
    /// `#` comments ignored.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("reading {}", path.display()))?;
        Ok(Self::parse(&text))
    }

    /// Parse banlist text. A line with a scheme bans that exact URL; any
    /// other line bans a domain and every subdomain of it.
    pub fn parse(text: &str) -> Self {
        let mut list = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.contains("://") {
                list.urls.insert(line.trim_end_matches('/').to_owned());
            } else {
                list.domains
                    .push(normalise_host(line.trim_end_matches('/')));
            }
        }
        list
    }

    /// Whether an entry at `url` on `host` is banned.
    pub fn bans(&self, url: &str, host: &str) -> bool {
        self.urls.contains(url.trim_end_matches('/'))
            || self
                .domains
                .iter()
                .any(|d| host == d || host.ends_with(&format!(".{d}")))
    }
}

/// The taxonomy as it appears on a page: each topic with its non-empty
/// subcategories, each with its hosts in alphabetical order.
type Order = Vec<(String, Vec<(String, Vec<String>)>)>;

/// One saved page as it appears in the page data: `[title, url, date]`.
#[derive(Clone, Debug)]
struct Saved {
    title: String,
    url: String,
    date: String,
}

/// One domain's record in the page data.
#[derive(Clone, Debug)]
struct Domain {
    topic: String,
    sub: String,
    pages: Vec<Saved>,
}

impl Domain {
    fn n(&self) -> usize {
        self.pages.len()
    }
    fn first(&self) -> &str {
        self.pages.first().map(|p| p.date.as_str()).unwrap_or("")
    }
    fn last(&self) -> &str {
        self.pages.last().map(|p| p.date.as_str()).unwrap_or("")
    }
}

/// Build the requested pages and return the paths written.
pub fn build(
    entries: &[Entry],
    labelling: &Labelling,
    banlist: &Banlist,
    settings: &Settings,
) -> Result<Vec<PathBuf>> {
    if !settings.private && !settings.public {
        bail!("nothing to build: both pages are turned off");
    }
    let Some(taxonomy) = &labelling.taxonomy else {
        bail!("the labelling carries no taxonomy: run `linksctl categorise` first");
    };
    let domains = aggregate(entries, labelling, taxonomy, banlist);
    if domains.is_empty() {
        bail!("no entries left to list");
    }
    let order = order(&domains, taxonomy);
    std::fs::create_dir_all(&settings.out_dir)
        .wrap_err_with(|| format!("creating {}", settings.out_dir.display()))?;
    let mut written = Vec::new();
    for (enabled, public, file) in [
        (settings.private, false, PRIVATE_FILE),
        (settings.public, true, PUBLIC_FILE),
    ] {
        if !enabled {
            continue;
        }
        let html = render(&domains, &order, public, &settings.title);
        let path = settings.out_dir.join(file);
        std::fs::write(&path, html).wrap_err_with(|| format!("writing {}", path.display()))?;
        info!(path = %path.display(), domains = domains.len(), "wrote page");
        written.push(path);
    }
    Ok(written)
}

/// The host of `url`, lowercased and without a leading `www.`, which is
/// how the pages key domains.
pub fn host_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    parsed.host_str().map(normalise_host)
}

fn normalise_host(host: &str) -> String {
    let host = host.to_ascii_lowercase();
    host.strip_prefix("www.").map(str::to_owned).unwrap_or(host)
}

/// Group entries by domain, drop banned ones, order each domain's pages by
/// date and file the domain by majority vote over its pages' labels (ties
/// go to the most recently saved page's label). Pages without a label do
/// not vote; a domain with no votes is unfiled.
fn aggregate(
    entries: &[Entry],
    labelling: &Labelling,
    taxonomy: &Taxonomy,
    banlist: &Banlist,
) -> BTreeMap<String, Domain> {
    let labels: HashMap<&str, (&str, &str, Option<&str>)> = labelling
        .labels
        .iter()
        .map(|l| {
            (
                l.url.as_str(),
                (
                    l.category.as_str(),
                    l.subcategory.as_str(),
                    l.title.as_deref(),
                ),
            )
        })
        .collect();
    let mut by_host: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
    let mut banned = 0usize;
    for e in entries {
        let Some(host) = host_of(&e.url) else {
            warn!(url = e.url, "skipping entry with an unparsable URL");
            continue;
        };
        if banlist.bans(&e.url, &host) {
            banned += 1;
            continue;
        }
        by_host.entry(host).or_default().push(e);
    }
    if banned > 0 {
        info!(banned, "entries excluded by the banlist");
    }
    let mut unlabelled = 0usize;
    let domains = by_host
        .into_iter()
        .map(|(host, mut pages)| {
            pages.sort_by(|a, b| a.date().cmp(b.date()));
            let mut votes: HashMap<(&str, &str), usize> = HashMap::new();
            for p in &pages {
                match labels.get(p.url.as_str()) {
                    Some((t, s, _)) if taxonomy.contains(t, s) => {
                        *votes.entry((t, s)).or_default() += 1;
                    }
                    Some((t, s, _)) => {
                        warn!(
                            url = p.url,
                            t, s, "label not in the taxonomy, counted as unfiled"
                        );
                        *votes.entry((UNFILED_TOPIC, UNFILED_SUB)).or_default() += 1;
                    }
                    None => unlabelled += 1,
                }
            }
            let best = votes.values().copied().max().unwrap_or(0);
            let (topic, sub) = pages
                .iter()
                .rev()
                .filter_map(|p| labels.get(p.url.as_str()))
                .map(|(t, s, _)| {
                    if taxonomy.contains(t, s) {
                        (*t, *s)
                    } else {
                        (UNFILED_TOPIC, UNFILED_SUB)
                    }
                })
                .find(|key| votes.get(key).copied().unwrap_or(0) == best)
                .unwrap_or((UNFILED_TOPIC, UNFILED_SUB));
            let saved = pages
                .iter()
                .map(|p| Saved {
                    title: if p.title.is_empty() {
                        labels
                            .get(p.url.as_str())
                            .and_then(|(_, _, t)| *t)
                            .unwrap_or("")
                            .to_owned()
                    } else {
                        p.title.clone()
                    },
                    url: p.url.clone(),
                    date: p.date().to_owned(),
                })
                .collect();
            (
                host,
                Domain {
                    topic: topic.to_owned(),
                    sub: sub.to_owned(),
                    pages: saved,
                },
            )
        })
        .collect();
    if unlabelled > 0 {
        warn!(unlabelled, "saved pages had no label and did not vote");
    }
    domains
}

/// The taxonomy restricted to subcategories that have domains, each with
/// its hosts in alphabetical order: the `order` of the page data.
fn order(domains: &BTreeMap<String, Domain>, taxonomy: &Taxonomy) -> Order {
    let mut by_key: HashMap<(&str, &str), Vec<&str>> = HashMap::new();
    for (host, d) in domains {
        by_key.entry((&d.topic, &d.sub)).or_default().push(host);
    }
    taxonomy
        .topics
        .iter()
        .filter_map(|t| {
            let subs: Vec<(String, Vec<String>)> = t
                .subs
                .iter()
                .filter_map(|s| {
                    let hosts = by_key.get(&(t.name.as_str(), s.as_str()))?;
                    Some((s.clone(), hosts.iter().map(|h| (*h).to_owned()).collect()))
                })
                .collect();
            (!subs.is_empty()).then(|| (t.name.clone(), subs))
        })
        .collect()
}

/// Escape text for HTML content and double-quoted attributes, as the
/// script's `esc` does.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// An element id from a name, as the script's `slug` does: runs of
/// non-alphanumeric ASCII become one hyphen, trimmed, lowercased.
fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    out.trim_matches('-').to_owned()
}

/// `1234567` as `1,234,567`.
fn commas(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// JSON text safe to embed in a `<script>` element: a `</` inside a string
/// is escaped so it cannot close the element.
fn script_json(v: &Value) -> String {
    v.to_string().replace("</", "<\\/")
}

/// The `#data` JSON: `order`, `domains` and the `public` flag.
fn data(domains: &BTreeMap<String, Domain>, order: &Order, public: bool) -> Value {
    let order_json: Vec<Value> = order
        .iter()
        .map(|(name, subs)| {
            json!({
                "name": name,
                "subs": subs.iter().map(|(s, hosts)| json!({ "name": s, "hosts": hosts })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut doms = Map::new();
    for (host, d) in domains {
        let mut rec = Map::new();
        rec.insert("t".into(), json!(d.topic));
        rec.insert("s".into(), json!(d.sub));
        rec.insert("n".into(), json!(d.n()));
        rec.insert("first".into(), json!(d.first()));
        rec.insert("last".into(), json!(d.last()));
        if !public {
            rec.insert(
                "e".into(),
                json!(
                    d.pages
                        .iter()
                        .map(|p| json!([p.title, p.url, p.date]))
                        .collect::<Vec<_>>()
                ),
            );
        }
        doms.insert(host.clone(), Value::Object(rec));
    }
    json!({ "order": order_json, "domains": doms, "public": public })
}

/// The board as the script renders it at load: no search, every domain,
/// most saved first, `CAP` domains per subcategory then a "more" button.
fn board(domains: &BTreeMap<String, Domain>, order: &Order, public: bool) -> String {
    let mut html = String::from(r#"<main class="board" id="board">"#);
    for (topic, subs) in order {
        let mut topic_html = String::new();
        let (mut topic_domains, mut topic_pages) = (0usize, 0usize);
        for (sub, hosts) in subs {
            let mut hs: Vec<&str> = hosts.iter().map(String::as_str).collect();
            hs.sort_by(|a, b| domains[*b].n().cmp(&domains[*a].n()).then_with(|| a.cmp(b)));
            topic_domains += hs.len();
            topic_pages += hs.iter().map(|h| domains[*h].n()).sum::<usize>();
            let key = slug(&format!("{topic} {sub}"));
            let shown = hs.len().min(CAP);
            let mut li = String::new();
            for h in &hs[..shown] {
                let d = &domains[*h];
                let n = d.n();
                let href = format!("https://{}", esc(h));
                if public {
                    li.push_str(&format!(
                        r#"<li class="d"><a class="h" href="{href}" target="_blank" rel="noopener">{}</a><span class="n" title="{n} saved">{n}</span></li>"#,
                        esc(h)
                    ));
                } else {
                    let title = d.pages.first().map(|p| p.title.as_str()).unwrap_or("");
                    li.push_str(&format!(
                        r#"<li class="d"><a class="h" href="{href}" target="_blank" rel="noopener" title="{}">{}</a><button type="button" class="n" data-h="{}" aria-expanded="false" title="show saved pages">{n}</button></li>"#,
                        esc(title),
                        esc(h),
                        esc(h)
                    ));
                }
            }
            if hs.len() > shown {
                li.push_str(&format!(
                    r#"<li class="more"><button type="button" data-sub="{key}">+ {} more</button></li>"#,
                    hs.len() - shown
                ));
            }
            topic_html.push_str(&format!(
                r#"<section class="sub"><h3>{}<span class="n">{}</span></h3><ul>{li}</ul></section>"#,
                esc(sub),
                hs.len()
            ));
        }
        html.push_str(&format!(
            r#"<section class="topic" id="{}"><h2>{}<span class="n">{topic_domains} domains &middot; {topic_pages} pages</span></h2>{topic_html}</section>"#,
            slug(topic),
            esc(topic)
        ));
    }
    html.push_str("</main>");
    html
}

/// Render one complete page.
fn render(domains: &BTreeMap<String, Domain>, order: &Order, public: bool, title: &str) -> String {
    let n_domains = domains.len();
    let n_pages: usize = domains.values().map(Domain::n).sum();
    let first = domains
        .values()
        .map(Domain::first)
        .filter(|d| !d.is_empty())
        .min()
        .unwrap_or("");
    let last = domains
        .values()
        .map(Domain::last)
        .filter(|d| !d.is_empty())
        .max()
        .unwrap_or("");
    let topics: Vec<&str> = order.iter().map(|(t, _)| t.as_str()).collect();
    let topic_list = if topics.len() > 6 {
        format!("{} and more", topics[..6].join(", "))
    } else {
        topics.join(", ")
    };
    let what = if public {
        "websites"
    } else {
        "websites and saved pages"
    };
    let description = format!(
        "{} {what} from {} pages bookmarked {} to {}, filed by topic: {topic_list}.",
        commas(n_domains),
        commas(n_pages),
        first.get(..4).unwrap_or(first),
        last.get(..4).unwrap_or(last),
    );
    let ld = json!({
        "@context": "https://schema.org",
        "@type": "CollectionPage",
        "name": title,
        "description": description,
        "dateModified": last,
        "inLanguage": "en",
        "mainEntity": {
            "@type": "ItemList",
            "name": "Topics",
            "numberOfItems": topics.len(),
            "itemListElement": topics.iter().enumerate().map(|(i, t)| json!({
                "@type": "ListItem", "position": i + 1, "name": t
            })).collect::<Vec<_>>(),
        },
    });
    let badge = if public {
        r#" <span class="badge">domains only</span>"#
    } else {
        ""
    };
    let note = if public {
        "Built from the bookmark log in the wiki. Each domain is filed by reading the pages saved from it, so filing is approximate. Only domain names and save counts are published here; the saved pages themselves stay in the private build."
    } else {
        "Built from the bookmark log in the wiki. Each domain is filed by reading the pages saved from it, so filing is approximate. Click a count to see the saved pages."
    };
    let placeholder = if public {
        "domain"
    } else {
        "domain or page title"
    };
    page_html(&PageParts {
        title: &esc(title),
        description: &esc(&description),
        updated: &format!("{last}T00:00:00Z"),
        ld_json: &script_json(&ld),
        css: CSS,
        badge,
        placeholder,
        note,
        board: &board(domains, order, public),
        data: &script_json(&data(domains, order, public)),
        js: JS,
    })
}

/// The pieces that vary between pages.
struct PageParts<'a> {
    title: &'a str,
    description: &'a str,
    updated: &'a str,
    ld_json: &'a str,
    css: &'a str,
    badge: &'a str,
    placeholder: &'a str,
    note: &'a str,
    board: &'a str,
    data: &'a str,
    js: &'a str,
}

/// The page skeleton, after the exemplars.
fn page_html(p: &PageParts<'_>) -> String {
    let PageParts {
        title,
        description,
        updated,
        ld_json,
        css,
        badge,
        placeholder,
        note,
        board,
        data,
        js,
    } = p;
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<meta name="description" content="{description}">
<meta name="robots" content="index,follow,max-image-preview:large">
<meta name="theme-color" content="#5c2d91">
<meta name="generator" content="linksctl">
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='4' fill='%235c2d91'/%3E%3Ctext x='16' y='22' font-family='Helvetica,Arial,sans-serif' font-size='15' font-weight='700' fill='white' text-anchor='middle'%3Erd%3C/text%3E%3C/svg%3E">
<meta property="og:type" content="website">
<meta property="og:site_name" content="{title}">
<meta property="og:title" content="{title}">
<meta property="og:description" content="{description}">
<meta property="og:updated_time" content="{updated}">
<meta name="twitter:card" content="summary">
<meta name="twitter:title" content="{title}">
<meta name="twitter:description" content="{description}">
<script type="application/ld+json">{ld_json}</script>
<style>
{css}</style>
</head>
<body>
<div class="wrap">
  <header class="top">
    <h1><span class="cl">rd</span>{title}{badge}</h1>
    <span class="stats" id="stats"></span>
  </header>

  <div class="body">
    <aside class="rail" aria-label="filters and index">
      <h4><label for="q">search</label></h4>
      <input type="search" id="q" placeholder="{placeholder}" autocomplete="off">

      <h4>min saves</h4>
      <div class="seg" id="minseg" role="group" aria-label="minimum saved pages per domain">
        <button type="button" data-min="1" aria-pressed="true" id="min1">all</button>
        <button type="button" data-min="2" aria-pressed="false" id="min2">2+</button>
        <button type="button" data-min="5" aria-pressed="false" id="min5">5+</button>
      </div>

      <h4>order</h4>
      <div class="seg" id="sortseg" role="group" aria-label="domain order">
        <button type="button" data-sort="n" aria-pressed="true" id="sortn">most saved</button>
        <button type="button" data-sort="a" aria-pressed="false" id="sorta">a to z</button>
      </div>

      <h4>topics</h4>
      <ul id="toc"></ul>

      <h4>most saved</h4>
      <ul id="top"></ul>

      <p class="note">{note}</p>
    </aside>

    {board}
  </div>
</div>

<script type="application/json" id="data">{data}</script>
<script>
{js}</script>
</body>
</html>
"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labels::Label;

    fn entry(ts: &str, title: &str, url: &str) -> Entry {
        Entry {
            timestamp: ts.into(),
            title: title.into(),
            url: url.into(),
        }
    }

    fn label(url: &str, c: &str, s: &str) -> Label {
        Label {
            url: url.into(),
            category: c.into(),
            subcategory: s.into(),
            title: None,
        }
    }

    fn taxonomy() -> Taxonomy {
        use crate::taxonomy::Topic;
        Taxonomy {
            topics: vec![
                Topic {
                    name: "systems".into(),
                    subs: vec!["rust".into(), "performance".into()],
                },
                Topic {
                    name: "science".into(),
                    subs: vec!["physics & space".into()],
                },
            ],
        }
        .sanitise()
        .unwrap()
    }

    fn fixture() -> (Vec<Entry>, Labelling) {
        let entries = vec![
            entry("2024-01-02T00:00:00Z", "A1", "https://www.a.example/1"),
            entry("2024-01-01T00:00:00Z", "A2", "https://a.example/2"),
            entry("2024-01-03T00:00:00Z", "A3", "https://a.example/3"),
            entry("2024-02-01T00:00:00Z", "B", "https://b.example/x"),
            entry("2024-03-01T00:00:00Z", "", "https://sub.c.example/y"),
            entry("2024-03-01T00:00:00Z", "", "not a url"),
        ];
        let labelling = Labelling {
            taxonomy: Some(taxonomy()),
            labels: vec![
                label("https://www.a.example/1", "systems", "rust"),
                label("https://a.example/2", "systems", "performance"),
                label("https://a.example/3", "systems", "performance"),
                label("https://b.example/x", "science", "physics & space"),
                Label {
                    title: Some("C title".into()),
                    ..label("https://sub.c.example/y", "bogus", "pair")
                },
            ],
        };
        (entries, labelling)
    }

    #[test]
    fn host_normalisation() {
        assert_eq!(
            host_of("https://www.Example.COM/p?q").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            host_of("http://sub.example.com").as_deref(),
            Some("sub.example.com")
        );
        assert_eq!(host_of("nope"), None);
    }

    #[test]
    fn banlist_matches_urls_and_domains() {
        let b = Banlist::parse("# c\nhttps://a.example/1/\nb.example\nWWW.C.example\n");
        assert!(b.bans("https://a.example/1", "a.example"));
        assert!(!b.bans("https://a.example/2", "a.example"));
        assert!(b.bans("https://b.example/x", "b.example"));
        assert!(b.bans("https://x.b.example/x", "x.b.example"));
        assert!(!b.bans("https://notb.example/x", "notb.example"));
        assert!(b.bans("https://c.example", "c.example"));
        assert!(!Banlist::parse("\n# only comments\n").bans("https://a.example", "a.example"));
    }

    #[test]
    fn aggregate_votes_and_orders_pages() {
        let (entries, labelling) = fixture();
        let t = taxonomy();
        let d = aggregate(&entries, &labelling, &t, &Banlist::default());
        assert_eq!(d.len(), 3);
        let a = &d["a.example"];
        assert_eq!(
            (a.topic.as_str(), a.sub.as_str()),
            ("systems", "performance")
        );
        assert_eq!(
            a.pages.iter().map(|p| p.title.as_str()).collect::<Vec<_>>(),
            ["A2", "A1", "A3"]
        );
        assert_eq!((a.first(), a.last()), ("2024-01-01", "2024-01-03"));
        let c = &d["sub.c.example"];
        assert_eq!(c.topic, UNFILED_TOPIC);
        assert_eq!(c.pages[0].title, "C title");
        // Ties go to the most recent page's label.
        let tie = vec![
            entry("2024-01-01T00:00:00Z", "", "https://t.example/old"),
            entry("2024-01-02T00:00:00Z", "", "https://t.example/new"),
        ];
        let l = Labelling {
            taxonomy: Some(taxonomy()),
            labels: vec![
                label("https://t.example/old", "systems", "rust"),
                label("https://t.example/new", "science", "physics & space"),
            ],
        };
        let d = aggregate(&tie, &l, &t, &Banlist::default());
        assert_eq!(d["t.example"].topic, "science");
    }

    #[test]
    fn pages_differ_in_what_they_reveal() {
        let (entries, labelling) = fixture();
        let dir = std::env::temp_dir().join(format!("linksctl-build-{}", std::process::id()));
        let settings = Settings {
            out_dir: dir.clone(),
            ..Settings::default()
        };
        let banlist = Banlist::parse("b.example\n");
        let written = build(&entries, &labelling, &banlist, &settings).unwrap();
        let bare = Labelling {
            taxonomy: None,
            ..labelling.clone()
        };
        assert!(build(&entries, &bare, &banlist, &settings).is_err());
        assert_eq!(written.len(), 2);
        let private = std::fs::read_to_string(dir.join(PRIVATE_FILE)).unwrap();
        let public = std::fs::read_to_string(dir.join(PUBLIC_FILE)).unwrap();
        for html in [&private, &public] {
            assert!(!html.contains("b.example"), "banned domain leaked");
            assert!(html.contains(r#"<section class="topic" id="systems">"#));
            assert!(html.contains(r#"<section class="topic" id="everything-else">"#));
        }
        assert!(private.contains("https://a.example/2"));
        assert!(!public.contains("https://a.example/2"));
        assert!(public.contains("domains only"));
        assert!(!private.contains("domains only"));
        let extract = |html: &str| -> Value {
            let start = html
                .find(r#"<script type="application/json" id="data">"#)
                .unwrap();
            let rest = &html[start..];
            let open = rest.find('>').unwrap() + 1;
            let close = rest.find("</script>").unwrap();
            serde_json::from_str(&rest[open..close].replace("<\\/", "</")).unwrap()
        };
        let pd = extract(&private);
        let pu = extract(&public);
        assert_eq!(pd["public"], false);
        assert_eq!(pu["public"], true);
        assert_eq!(pd["domains"]["a.example"]["n"], 3);
        assert_eq!(pd["domains"]["a.example"]["e"].as_array().unwrap().len(), 3);
        assert!(pu["domains"]["a.example"].get("e").is_none());
        assert_eq!(pd["order"][0]["name"], "systems");
        assert_eq!(pd["order"][0]["subs"][0]["hosts"], json!(["a.example"]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn helpers() {
        assert_eq!(
            slug("crypto & mev ethereum & rollups"),
            "crypto-mev-ethereum-rollups"
        );
        assert_eq!(slug("c & c++"), "c-c");
        assert_eq!(commas(7675), "7,675");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1_234_567), "1,234,567");
        assert_eq!(esc(r#"a<b>&"c""#), "a&lt;b&gt;&amp;&quot;c&quot;");
        assert_eq!(script_json(&json!("</script>")), r#""<\/script>""#);
    }
}
