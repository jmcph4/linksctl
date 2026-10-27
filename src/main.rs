//! `linksctl`: turn a Markdown bookmark log (the *resources file*) into a
//! private and a public directory page, through the four pipeline stages
//! the spec names (links.md "Pipeline"): URL extraction (`urls`), web
//! retrieval (`fetch`), categorisation (`categorise`) and artefact
//! generation (`build`). `run` does all four in order.
//!
//! Every option can be set on the command line or in a TOML config file;
//! the command line wins over the file and the file over the default.
//! Environment variables are never read for configuration (links.md
//! "Components").

mod build;
mod categorise;
mod fetch;
mod labels;
mod llm;
mod resources;
mod taxonomy;

use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{Args, Parser, Subcommand};
use eyre::{Context, Result, bail};
use serde::Deserialize;
use tracing::info;
use tracing_subscriber::EnvFilter;
use webtronix::{Format, Options, Render};

use crate::{build::Banlist, labels::Labelling};

/// Config file read from the working directory when `--config` is not given.
const DEFAULT_CONFIG: &str = "linksctl.toml";
/// Page index written when none is configured.
const DEFAULT_INDEX: &str = "index.db";
/// Category labelling JSON written when none is configured.
const DEFAULT_CATEGORIES: &str = "categories.json";

#[derive(Parser, Debug)]
#[command(name = "linksctl", version, about, long_about = None)]
struct Cli {
    /// TOML file of defaults for the options below. [default: linksctl.toml
    /// in the working directory, when it exists]
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Log filter, as directives such as `info` or `linksctl=debug,warn`.
    /// Replaces the level chosen by `--verbose` and `--quiet`.
    #[arg(long, global = true, value_name = "FILTER")]
    log: Option<String>,
    /// Log at debug level.
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Log errors only.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    quiet: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the whole pipeline: extract URLs, fetch them, categorise and
    /// build both directory pages.
    Run(RunArgs),
    /// Extract the URLs from a resources file, one per line.
    Urls(UrlsArgs),
    /// Fetch a list of URLs into a page index with Webtronix.
    Fetch(FetchArgs),
    /// Label every page in a page index with a category and subcategory.
    Categorise(CategoriseArgs),
    /// Build the directory pages from a category labelling.
    Build(BuildArgs),
}

#[derive(Args, Debug)]
struct RunArgs {
    /// The resources file.
    resourcesfile: PathBuf,
    /// SQLite page index to write. [default: index.db]
    #[arg(long, value_name = "DB")]
    index: Option<PathBuf>,
    /// JSON file the labelling is written to. [default: categories.json]
    #[arg(long, value_name = "JSON")]
    categories: Option<PathBuf>,
    #[command(flatten)]
    fetch: FetchOpts,
    #[command(flatten)]
    llm: LlmOpts,
    #[command(flatten)]
    build: BuildOpts,
}

#[derive(Args, Debug)]
struct UrlsArgs {
    /// The resources file.
    resourcesfile: PathBuf,
    /// File to write the URLs to instead of standard output.
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct FetchArgs {
    /// File containing one URL per line. Blank lines and `#` comments are
    /// ignored.
    urls: PathBuf,
    /// SQLite page index to write. An existing index is reused so unchanged
    /// pages are not re-downloaded. [default: index.db]
    #[arg(long, value_name = "DB")]
    index: Option<PathBuf>,
    #[command(flatten)]
    fetch: FetchOpts,
}

#[derive(Args, Debug)]
struct CategoriseArgs {
    /// The page index.
    pageindex: PathBuf,
    /// JSON file the complete labelling is written to. [default:
    /// categories.json]
    #[arg(short, long, value_name = "JSON")]
    output: Option<PathBuf>,
    /// Do not write the labels to a `categories` table in the page index.
    #[arg(long)]
    no_table: bool,
    /// The resources file, for titles and for URLs the index has no page
    /// for.
    #[arg(long, value_name = "FILE")]
    resources: Option<PathBuf>,
    #[command(flatten)]
    llm: LlmOpts,
}

#[derive(Args, Debug)]
struct BuildArgs {
    /// The category labelling: a JSON file from `categorise`, or a page
    /// index holding its `categories` table.
    categories: PathBuf,
    /// The resources file, for the titles and timestamps of the entries.
    #[arg(long, value_name = "FILE")]
    resources: Option<PathBuf>,
    #[command(flatten)]
    build: BuildOpts,
}

/// Web retrieval options, mirroring Webtronix's.
#[derive(Args, Debug, Default)]
struct FetchOpts {
    /// Maximum number of in-flight requests. [default: 16]
    #[arg(long, value_name = "N")]
    concurrency: Option<usize>,
    /// Per-request timeout in seconds. [default: 30]
    #[arg(long, value_name = "SECONDS")]
    timeout: Option<u64>,
    /// Chrome or Chromium binary to render pages with.
    #[arg(long, value_name = "PATH")]
    chrome: Option<PathBuf>,
}

/// Categorisation options.
#[derive(Args, Debug, Default)]
struct LlmOpts {
    /// How to reach the model: the Anthropic Messages API, or a local
    /// headless harness. [default: api]
    #[arg(long, value_name = "MODE")]
    llm: Option<llm::Mode>,
    /// Model name. [default: claude-opus-5-5 in API mode; the harness's
    /// default in agent mode]
    #[arg(long, value_name = "MODEL")]
    model: Option<String>,
    /// File holding the API key, for API mode.
    #[arg(long, value_name = "PATH")]
    api_key_file: Option<PathBuf>,
    /// The harness binary run in agent mode. [default: claude]
    #[arg(long, value_name = "COMMAND")]
    agent_command: Option<String>,
    /// Reasoning effort in API mode: low, medium, high, xhigh or max.
    #[arg(long, value_name = "EFFORT")]
    effort: Option<String>,
    /// Pages sent to the model in one request. [default: 20]
    #[arg(long, value_name = "N")]
    batch_size: Option<usize>,
    /// Characters of page text sent per page. [default: 2000]
    #[arg(long, value_name = "N")]
    max_chars: Option<usize>,
    /// Model requests in flight at once. [default: 8]
    #[arg(long, value_name = "N")]
    llm_concurrency: Option<usize>,
    /// Characters of the collection listing shown to the model when it
    /// proposes the taxonomy; larger collections are sampled. [default:
    /// 300000]
    #[arg(long, value_name = "N")]
    taxonomy_chars: Option<usize>,
    /// Discard the stored taxonomy and labels: propose a new taxonomy and
    /// label every page again.
    #[arg(long)]
    force: bool,
}

/// Artefact generation options.
#[derive(Args, Debug, Default)]
struct BuildOpts {
    /// Directory the pages are written into. [default: .]
    #[arg(long, value_name = "DIR")]
    out: Option<PathBuf>,
    /// Build only the private page.
    #[arg(long, conflicts_with = "public_only")]
    private_only: bool,
    /// Build only the public page.
    #[arg(long)]
    public_only: bool,
    /// File of URLs or domain names to leave out, one per line.
    #[arg(long, value_name = "FILE")]
    banlist: Option<PathBuf>,
    /// The page title. [default: resources directory]
    #[arg(long, value_name = "TITLE")]
    title: Option<String>,
}

/// The config file. Keys match the long flags they default.
#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct Config {
    log: Option<String>,
    index: Option<PathBuf>,
    categories: Option<PathBuf>,
    resources: Option<PathBuf>,
    banlist: Option<PathBuf>,
    out: Option<PathBuf>,
    title: Option<String>,
    #[serde(default)]
    fetch: FetchConfig,
    #[serde(default)]
    llm: LlmConfig,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct FetchConfig {
    concurrency: Option<usize>,
    timeout: Option<u64>,
    chrome: Option<PathBuf>,
    user_agent: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct LlmConfig {
    mode: Option<llm::Mode>,
    model: Option<String>,
    api_key: Option<String>,
    api_key_file: Option<PathBuf>,
    agent_command: Option<String>,
    effort: Option<String>,
    timeout: Option<u64>,
    batch_size: Option<usize>,
    max_chars: Option<usize>,
    concurrency: Option<usize>,
    taxonomy_chars: Option<usize>,
}

impl Config {
    /// Read `--config`, or the default file when it exists.
    fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None if Path::new(DEFAULT_CONFIG).exists() => PathBuf::from(DEFAULT_CONFIG),
            None => return Ok(Self::default()),
        };
        let text = std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).wrap_err_with(|| format!("parsing {}", path.display()))
    }
}

/// Webtronix options from the command line, then the config, then defaults.
/// Every page's JavaScript is run in the headless browser and the rendered
/// HTML is stored as is: no text extraction, and no option to change that.
fn fetch_options(opts: &FetchOpts, cfg: &FetchConfig) -> Result<Options> {
    let mut o = Options {
        format: Format::Html,
        render: Render::Always,
        ..Options::default()
    };
    if let Some(c) = opts.concurrency.or(cfg.concurrency) {
        o.concurrency = c;
    }
    if let Some(t) = opts.timeout.or(cfg.timeout) {
        o.timeout = Duration::from_secs(t);
    }
    if let Some(c) = opts.chrome.clone().or_else(|| cfg.chrome.clone()) {
        o.chrome = Some(c);
    }
    if let Some(ua) = &cfg.user_agent {
        o.user_agent = ua.clone();
    }
    Ok(o)
}

/// LLM settings from the command line, then the config, then defaults.
fn llm_settings(opts: &LlmOpts, cfg: &LlmConfig) -> llm::Settings {
    let d = llm::Settings::default();
    llm::Settings {
        mode: opts.llm.or(cfg.mode).unwrap_or(d.mode),
        model: opts.model.clone().or_else(|| cfg.model.clone()),
        api_key: cfg.api_key.clone(),
        api_key_file: opts
            .api_key_file
            .clone()
            .or_else(|| cfg.api_key_file.clone()),
        agent_command: opts
            .agent_command
            .clone()
            .or_else(|| cfg.agent_command.clone())
            .unwrap_or(d.agent_command),
        effort: opts.effort.clone().or_else(|| cfg.effort.clone()),
        timeout: cfg.timeout.map(Duration::from_secs).unwrap_or(d.timeout),
    }
}

/// Categoriser settings from the command line, then the config, then defaults.
fn categorise_settings(opts: &LlmOpts, cfg: &LlmConfig) -> categorise::Settings {
    let d = categorise::Settings::default();
    categorise::Settings {
        batch_size: opts.batch_size.or(cfg.batch_size).unwrap_or(d.batch_size),
        max_chars: opts.max_chars.or(cfg.max_chars).unwrap_or(d.max_chars),
        concurrency: opts
            .llm_concurrency
            .or(cfg.concurrency)
            .unwrap_or(d.concurrency),
        taxonomy_chars: opts
            .taxonomy_chars
            .or(cfg.taxonomy_chars)
            .unwrap_or(d.taxonomy_chars),
        force: opts.force,
    }
}

/// Build settings from the command line, then the config, then defaults.
fn build_settings(opts: &BuildOpts, cfg: &Config) -> build::Settings {
    let d = build::Settings::default();
    build::Settings {
        private: !opts.public_only,
        public: !opts.private_only,
        title: opts
            .title
            .clone()
            .or_else(|| cfg.title.clone())
            .unwrap_or(d.title),
        out_dir: opts
            .out
            .clone()
            .or_else(|| cfg.out.clone())
            .unwrap_or(d.out_dir),
    }
}

/// The banlist named on the command line or in the config, else none.
fn load_banlist(explicit: Option<&Path>, cfg: &Config) -> Result<Banlist> {
    match explicit.or(cfg.banlist.as_deref()) {
        Some(p) => Banlist::load(p),
        None => Ok(Banlist::default()),
    }
}

/// Logging from `--log`, the config, or the verbosity flags. The filter is
/// never read from the environment.
fn init_logging(cli: &Cli, cfg: &Config) -> Result<()> {
    let mut filter = cli
        .log
        .clone()
        .or_else(|| cfg.log.clone())
        .unwrap_or_else(|| {
            if cli.verbose {
                "debug".into()
            } else if cli.quiet {
                "error".into()
            } else {
                "info".into()
            }
        });
    // The headless browser's websocket layer warns on every message it
    // does not model, so it is kept to errors unless the filter names it.
    if !filter.contains("chromiumoxide") {
        filter.push_str(",chromiumoxide=error");
    }
    let filter =
        EnvFilter::try_new(&filter).wrap_err_with(|| format!("bad log filter {filter:?}"))?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(cli.config.as_deref())?;
    init_logging(&cli, &cfg)?;
    match &cli.command {
        Command::Urls(a) => urls(a),
        Command::Fetch(a) => fetch_cmd(a, &cfg).await,
        Command::Categorise(a) => categorise_cmd(a, &cfg).await,
        Command::Build(a) => build_cmd(a, &cfg),
        Command::Run(a) => run(a, &cfg).await,
    }
}

/// `linksctl urls`: the URL-extraction stage.
fn urls(a: &UrlsArgs) -> Result<()> {
    let entries = resources::read(&a.resourcesfile)?;
    let urls = resources::urls(&entries);
    let mut text = urls.join("\n");
    text.push('\n');
    match &a.output {
        Some(p) => std::fs::write(p, text).wrap_err_with(|| format!("writing {}", p.display()))?,
        None => std::io::stdout().write_all(text.as_bytes())?,
    }
    info!(entries = entries.len(), urls = urls.len(), "extracted");
    Ok(())
}

/// `linksctl fetch`: the web-retrieval stage.
async fn fetch_cmd(a: &FetchArgs, cfg: &Config) -> Result<()> {
    let urls = webtronix::read_urls(&a.urls).await?;
    let index = a
        .index
        .clone()
        .or_else(|| cfg.index.clone())
        .unwrap_or(DEFAULT_INDEX.into());
    let options = fetch_options(&a.fetch, &cfg.fetch)?;
    fetch::fetch(&urls, &index, &options).await?;
    Ok(())
}

/// Label what the index at `index` holds, merge with the labels it already
/// has, and write the JSON file and (unless told not to) the table.
async fn categorise_into(
    index: &Path,
    entries: &[resources::Entry],
    json: &Path,
    table: bool,
    llm: &llm::Llm,
    settings: &categorise::Settings,
) -> Result<Labelling> {
    // A forced run starts from nothing: the stored taxonomy is replaced,
    // so the labels drawn from it are meaningless. So are labels stored
    // without a taxonomy (an index written before one was kept).
    let mut existing = if index.exists() && !settings.force {
        Labelling::read_table(index)?
    } else {
        Labelling::default()
    };
    if existing.taxonomy.is_none() && !existing.labels.is_empty() {
        info!(
            stale = existing.labels.len(),
            "stored labels have no taxonomy, relabelling"
        );
        existing = Labelling::default();
    }
    let fresh_start = existing.taxonomy.is_none();
    let fresh = categorise::categorise(index, entries, &existing, llm, settings).await?;
    if table {
        fresh.write_table(index, fresh_start)?;
    }
    // The JSON file holds the complete labelling, old and new.
    let mut all: std::collections::BTreeMap<String, labels::Label> = existing
        .labels
        .into_iter()
        .map(|l| (l.url.clone(), l))
        .collect();
    for l in fresh.labels {
        all.insert(l.url.clone(), l);
    }
    let all = Labelling {
        taxonomy: fresh.taxonomy,
        labels: all.into_values().collect(),
    };
    all.write_json(json)?;
    info!(labels = all.labels.len(), path = %json.display(), "wrote labelling");
    Ok(all)
}

/// `linksctl categorise`: the categorisation stage.
async fn categorise_cmd(a: &CategoriseArgs, cfg: &Config) -> Result<()> {
    if !a.pageindex.exists() {
        bail!("page index {} does not exist", a.pageindex.display());
    }
    let entries = match a.resources.as_deref().or(cfg.resources.as_deref()) {
        Some(p) => resources::read(p)?,
        None => Vec::new(),
    };
    let json = a
        .output
        .clone()
        .or_else(|| cfg.categories.clone())
        .unwrap_or(DEFAULT_CATEGORIES.into());
    let llm = llm::Llm::new(llm_settings(&a.llm, &cfg.llm))?;
    let settings = categorise_settings(&a.llm, &cfg.llm);
    categorise_into(&a.pageindex, &entries, &json, !a.no_table, &llm, &settings).await?;
    Ok(())
}

/// `linksctl build`: the artefact-generation stage.
fn build_cmd(a: &BuildArgs, cfg: &Config) -> Result<()> {
    let Some(resources_path) = a.resources.as_deref().or(cfg.resources.as_deref()) else {
        bail!(
            "build needs the resources file for titles and dates: pass --resources or set `resources` in the config"
        );
    };
    let entries = resources::read(resources_path)?;
    let labelling = Labelling::read(&a.categories)?;
    let banlist = load_banlist(a.build.banlist.as_deref(), cfg)?;
    build::build(
        &entries,
        &labelling,
        &banlist,
        &build_settings(&a.build, cfg),
    )?;
    Ok(())
}

/// `linksctl run`: every stage in order, equivalent to running the four
/// subcommands one after another.
async fn run(a: &RunArgs, cfg: &Config) -> Result<()> {
    let entries = resources::read(&a.resourcesfile)?;
    // Everything that can be checked up front is, so a missing API key
    // fails before any page is fetched.
    let llm = llm::Llm::new(llm_settings(&a.llm, &cfg.llm))?;
    let settings = categorise_settings(&a.llm, &cfg.llm);
    let banlist = load_banlist(a.build.banlist.as_deref(), cfg)?;
    let urls = resources::urls(&entries);
    info!(entries = entries.len(), urls = urls.len(), "extracted URLs");
    let index = a
        .index
        .clone()
        .or_else(|| cfg.index.clone())
        .unwrap_or(DEFAULT_INDEX.into());
    fetch::fetch(&urls, &index, &fetch_options(&a.fetch, &cfg.fetch)?).await?;
    let json = a
        .categories
        .clone()
        .or_else(|| cfg.categories.clone())
        .unwrap_or(DEFAULT_CATEGORIES.into());
    let labelling = categorise_into(&index, &entries, &json, true, &llm, &settings).await?;
    build::build(
        &entries,
        &labelling,
        &banlist,
        &build_settings(&a.build, cfg),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_overrides_config() {
        let cfg: Config = toml::from_str(
            "index = \"x.db\"\n[fetch]\nconcurrency = 3\n[llm]\nmode = \"agent\"\nbatch_size = 5\n",
        )
        .unwrap();
        let o = fetch_options(
            &FetchOpts {
                concurrency: Some(9),
                ..Default::default()
            },
            &cfg.fetch,
        )
        .unwrap();
        assert_eq!(o.concurrency, 9);
        assert_eq!(o.format, Format::Html);
        assert_eq!(o.render, Render::Always);
        let o = fetch_options(&FetchOpts::default(), &FetchConfig::default()).unwrap();
        assert_eq!((o.format, o.render), (Format::Html, Render::Always));
        let s = llm_settings(&LlmOpts::default(), &cfg.llm);
        assert_eq!(s.mode, llm::Mode::Agent);
        let s = llm_settings(
            &LlmOpts {
                llm: Some(llm::Mode::Api),
                ..Default::default()
            },
            &cfg.llm,
        );
        assert_eq!(s.mode, llm::Mode::Api);
        let c = categorise_settings(&LlmOpts::default(), &cfg.llm);
        assert_eq!((c.batch_size, c.max_chars), (5, 2000));
    }

    #[test]
    fn config_rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("indx = \"x\"\n").is_err());
        // Rendering and the stored format are fixed, not configurable.
        assert!(toml::from_str::<Config>("[fetch]\nformat = \"text\"\n").is_err());
        assert!(toml::from_str::<Config>("[fetch]\nrender = \"never\"\n").is_err());
        assert!(toml::from_str::<Config>("[llm]\napi_key = \"k\"\n").is_ok());
    }

    #[test]
    fn cli_parses_every_subcommand() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        let cli = Cli::parse_from([
            "linksctl",
            "build",
            "c.json",
            "--resources",
            "r.md",
            "--public-only",
        ]);
        let Command::Build(b) = cli.command else {
            panic!()
        };
        assert!(b.build.public_only);
        let cli = Cli::parse_from([
            "linksctl",
            "-v",
            "run",
            "r.md",
            "--llm",
            "agent",
            "--banlist",
            "b.txt",
        ]);
        let Command::Run(r) = cli.command else {
            panic!()
        };
        assert_eq!(r.llm.llm, Some(llm::Mode::Agent));
        assert_eq!(r.build.banlist.as_deref(), Some(Path::new("b.txt")));
    }
}
