# AGENTS.md — guidance for coding agents working on `linksctl` #

This file is for AI coding agents (and new human contributors). It says what
the project is, where things live, how to verify a change, and the rules
that are easy to break without noticing. The `README.md` is the user-facing
description; read it first if you have not.

## What this is ##

`linksctl` is a single Rust binary (edition 2024) that turns a Markdown
bookmark log (the *resources file*) into two static HTML pages: a
**private** directory listing every saved URL by topic, and a **public**
directory listing only domains and save counts.

It is the reference implementation of the **`links` spec**, which lives in
the sibling directory `../links-spec/`:

| File                               | Role                                                            |
| ---------------------------------- | --------------------------------------------------------------- |
| `../links-spec/links.md`           | The normative spec (RFC 2119 language). Read it before changing behaviour. |
| `../links-spec/resources-exemplar.md` | Sample resources file. Vendored as `tests/fixtures/resources-exemplar.md`. |
| `../links-spec/directory-private.html` | Exemplar private page. Its CSS/JS are vendored under `src/assets/`. |
| `../links-spec/directory-public.html`  | Exemplar public page.                                       |

Doc comments throughout `src/` cite spec sections as `links.md "<section>"`.
Keep doing that when you add behaviour the spec calls for. If the spec
directory is missing on your machine, the vendored copies and the citations
in the code are enough to work from; do not invent spec requirements.

## Pipeline and module map ##

Four stages, each a subcommand; `run` does all four in order and must stay
equivalent to running them one after another (spec: `linksctl run`).

| Stage               | Subcommand                        | Module             | Input → Output                                  |
| ------------------- | --------------------------------- | ------------------ | ----------------------------------------------- |
| URL extraction      | `linksctl urls <RESOURCESFILE>`   | `src/resources.rs` | resources file → one URL per line (deduplicated, first-seen order) |
| Web retrieval       | `linksctl fetch <URLS>`           | `src/fetch.rs`     | URL list → page index (`index.db`, SQLite, via Webtronix) |
| Categorisation      | `linksctl categorise <PAGEINDEX>` | `src/categorise.rs`, `src/llm.rs`, `src/taxonomy.rs`, `src/labels.rs` | page index → labelling (JSON file and/or `taxonomy` + `categories` tables in the index) |
| Artefact generation | `linksctl build <CATEGORIES>`     | `src/build.rs`     | labelling (+ resources file, optional banlist) → `directory-private.html`, `directory-public.html` |

Module responsibilities:

- `src/main.rs` — clap CLI (derive API), TOML config loading, the
  CLI-over-config-over-default merge functions (`fetch_options`,
  `llm_settings`, `categorise_settings`, `build_settings`), logging setup,
  and the five subcommand entry points. All option plumbing lives here.
- `src/resources.rs` — parses `- [<timestamp>] [<title>](<url>)` lines.
  Titles may contain `[...]` (arXiv), so the link is split at the *last*
  `](`. Only `http(s)://` URLs count; a line without a leading four-digit
  year is not an entry.
- `src/fetch.rs` — thin wrapper over `webtronix::Index::update` with
  progress logging. Rendering (`Render::Always`) and storage format
  (`Format::Html`) are fixed in `main.rs` and deliberately **not**
  configurable; tests assert the config rejects `format`/`render` keys.
- `src/llm.rs` — the two LLM modes the spec requires. **API mode** posts to
  the Anthropic Messages API with structured output (JSON schema), retries
  on 429/5xx/529 with backoff and `retry-after`. **Agent mode** runs
  `claude -p --output-format json --json-schema ... --tools ''` with the
  prompt on stdin and reads `structured_output`. Both return a
  `serde_json::Value` matching a caller-supplied schema.
- `src/taxonomy.rs` — `Taxonomy { topics: Vec<Topic { name, subs }> }`.
  `sanitise()` trims, dedupes case-insensitively, drops any catch-all the
  model proposed, and appends the fixed *everything else / unfiled* bucket.
  The user never supplies a taxonomy (spec: Categorisation).
- `src/categorise.rs` — two phases: (1) *discover*: show the model the
  whole collection (host + title per page, evenly sampled down to
  `taxonomy_chars`) and take its taxonomy; (2) *label*: batch pages against
  that taxonomy. Page text for prompts is the stored HTML with markup
  stripped (`strip_html`) and bounded (`trim_text`); the index itself is
  never rewritten with extracted text. Entries the index has no row for
  (fetch failures) are labelled from their title alone.
- `src/labels.rs` — `Labelling { taxonomy, labels }`. Reads/writes the JSON
  file and the `categories` + `taxonomy` SQLite tables. `Labelling::read`
  sniffs the SQLite magic bytes, so `build` accepts either a JSON file or
  the page index as its `<CATEGORIES>` argument.
- `src/build.rs` — groups entries by host (lowercased, `www.` stripped),
  files each domain by majority vote over its pages' labels (ties go to the
  most recently saved page), applies the banlist, and renders both pages
  with the vendored stylesheet and script. The page embeds its data as JSON
  under `#data` with keys `order`, `domains`, `public`; the script in
  `src/assets/directory.js` re-renders from that JSON, so the two must
  agree on shape.
- `src/assets/directory.{css,js}` — copied from the spec's exemplar pages.
  Treat as vendored: change them only to match an updated exemplar, and
  keep `CAP` in `build.rs` equal to `CAP` in the script.

## Configuration rules (spec-mandated) ##

These come straight from the spec's "Components" section and are tested.
Do not relax them.

1. **CLI flag > config file > built-in default.** Every new option must be
   settable both ways and merged in `main.rs` in that order.
2. **No environment variables are read for configuration.** Not for the
   API key, not for the log filter (`EnvFilter` is built from `--log` /
   config / verbosity flags, never from `RUST_LOG`). The only exception is
   whatever Webtronix/chromiumoxide read internally; do not add more.
3. **Config file is TOML**, `linksctl.toml` in the working directory or
   `--config <PATH>`. `Config` uses `#[serde(deny_unknown_fields)]`, so a
   new option needs a field in `Config`/`FetchConfig`/`LlmConfig`, a flag
   in the matching `*Opts` struct, a line in `linksctl.example.toml`, and
   usually a line in the README.
4. **Keys in the config match the long flag they default**, with the
   exceptions noted in `linksctl.example.toml` (`[llm] mode` ↔ `--llm`,
   `[llm] concurrency` ↔ `--llm-concurrency`, `[fetch] user_agent` and
   `[llm] api_key` / `[llm] timeout` are config-only).
5. Libraries are fixed by the spec: clap v4 derive, eyre, tracing +
   tracing-subscriber, Webtronix for retrieval. Use `eyre::Result` and
   `wrap_err_with` for context; do not introduce `anyhow` or `thiserror`.

`linksctl.toml` in the repo root is the developer's **local** config (it
sets `mode = "agent"`); `linksctl.example.toml` is the documented template.
Edit the example when adding keys; leave the local file's values alone
unless asked. `.gitignore` excludes the local config, the page index,
`categories.json`, `urls.txt`, the generated HTML, `banlist.txt` and key
files; if a run writes elsewhere (`--out`, `-o`), check `git status` before
staging.

## Build, test, lint ##

Toolchain: stable Rust (CI uses `dtolnay/rust-toolchain@stable`; edition
2024 needs 1.85+). The crate has no features, no benches, no examples.

```sh
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --document-private-items
```

All four must pass before you consider a change done; CI
(`.github/workflows/main.yml`) runs them plus `cargo build`, `cargo check`,
`cargo bench` (a no-op build here), a `--no-default-features` clippy pass
(same as the default, there are no features) and `cargo audit`. Rustdoc
warnings are errors, so every `pub` item and every private item with a doc
comment must have valid intra-doc links.

Tests are **hermetic**: unit tests live in `#[cfg(test)] mod tests` inside
each module, nothing touches the network or a browser, and the only fixture
is `tests/fixtures/resources-exemplar.md` (pulled in with `include_str!`).
Keep it that way:

- Do not add tests that call the Anthropic API or spawn `claude`. Test the
  pure helpers (`message_json`, `agent_json`, `parse_labels`,
  `label_schema`, `strip_html`, ...) instead.
- Do not add tests that need Chrome. Fetching is exercised by Webtronix's
  own tests.
- Tests that need a SQLite file write under `std::env::temp_dir()` with the
  process id in the name and remove it afterwards (see `labels.rs`).

The first `cargo build` is slow because Webtronix pulls in chromiumoxide
and rusqlite is compiled with `bundled`. Subsequent builds are incremental.

### Running the tool end to end ###

Only do this when asked, or when a change affects output that unit tests
cannot cover (page rendering, LLM prompts). It needs network access, and
`categorise`/`run` need either an API key file (never commit one) or a
working `claude` binary on `PATH` for agent mode. Point outputs at a
scratch directory with `--out`, `--index`, `--categories` so you do not
clobber the developer's `index.db`.

```sh
cargo run -- urls tests/fixtures/resources-exemplar.md
cargo run -- fetch urls.txt --index /tmp/x/index.db
cargo run -- categorise /tmp/x/index.db --resources tests/fixtures/resources-exemplar.md --llm agent -o /tmp/x/c.json
cargo run -- build /tmp/x/c.json --resources tests/fixtures/resources-exemplar.md --out /tmp/x
```

## Dependencies ##

- **Webtronix** is a git dependency
  (`https://github.com/Alpine-Computing-Research/webtronix`). It is the
  spec-mandated retrieval layer; do not replace it with reqwest/scraper.
  `.cargo/config.toml` sets `net.git-fetch-with-cli = true` so cargo uses
  the `git` binary and honours the developer's SSH `insteadOf` rewrite. If
  a fetch fails in your environment, that config is why: do not remove it,
  work around it locally (e.g. `CARGO_NET_GIT_FETCH_WITH_CLI=false` for one
  command) and say so.
- `reqwest` is built with `default-features = false` and rustls; do not
  enable native-tls.
- `toml` is parse-only (`default-features = false`). Nothing serialises
  config.
- Adding a dependency: justify it in the PR, keep `Cargo.lock` updated, and
  remember `cargo audit` runs in CI.

## LLM integration notes ##

- The Messages API request pins `anthropic-version: 2026-10-21`, uses
  `output_config.format = {type: json_schema}` for structured output,
  passes `output_config.effort` when `--effort` is set, and sets
  `fallbacks: "default"` with the `server-side-fallback-2026-07-01` beta.
  Default model is `claude-opus-5-5` (`llm::DEFAULT_MODEL`). If you change
  the API shape, update the unit tests in `llm.rs` and the README.
- `stop_reason == "refusal"` and `"max_tokens"` are hard errors, not
  retried.
- Agent mode invokes `<agent_command> -p --output-format json --json-schema
  <schema> --tools "" --no-session-persistence --system-prompt <system>`
  and reads `structured_output` from the JSON envelope, falling back to the
  `result` text (code fences tolerated). Keep tool use disabled.
- Prompt text lives in `categorise.rs` (`DISCOVER_SYSTEM`, `label_system`,
  `label_user`). The label schema is built from the taxonomy so the model
  can only return valid `(category, subcategory)` pairs; `parse_labels`
  still validates and files anything off-taxonomy under the unfiled bucket.
- Incremental runs: a stored taxonomy is reused and only unlabelled URLs
  are sent. `--force` discards taxonomy and labels. Labels stored without a
  taxonomy are treated as stale and redone. Preserve these semantics when
  touching `categorise_into` in `main.rs`.

## Conventions ##

- British spelling in identifiers, docs and user-facing text
  (`categorise`, `sanitise`, `normalise`, `artefact`, `labelling`). Match
  it.
- Every module starts with a `//!` doc that names the spec section it
  implements. Every public item has a doc comment. Comments explain *why*,
  not what.
- Errors: `eyre::Result`, `bail!` for user-facing failures, and
  `wrap_err_with(|| format!("reading {}", path.display()))` around I/O so
  the path is in the message.
- Logging: `tracing` macros with structured fields (`info!(done, total,
  "fetch progress")`), written to stderr. Stdout is reserved for data
  (`linksctl urls` without `-o`). `chromiumoxide` is pinned to `error`
  unless the filter names it.
- No `unwrap()` outside tests; use `?` or `ok_or_else`.
- Keep `README.md`, `linksctl.example.toml`, the clap doc comments and this
  file consistent when behaviour or options change. The README's option
  defaults are the source users read.
- Commit messages: imperative subject line; no `git commit` or `git push`
  unless asked. The repo currently has no commits; `master` is the working
  branch and CI also watches `main`.

## Things that look like bugs but are not ##

- `fetch` stores rendered HTML and never extracts text, even though the
  model only sees text. This is deliberate (README "Web retrieval"); text
  extraction happens at prompt time in `categorise.rs`.
- Pages whose fetch failed still get labelled, from their title alone, when
  `--resources` is passed. That is why `categorise` accepts a resources
  file.
- `build` accepts the SQLite index as `<CATEGORIES>`; it is not a mistaken
  argument order.
- `www.` is stripped and hosts are lowercased before grouping, so
  `www.example.com` and `example.com` are one domain on purpose.
- Domain ties in the majority vote resolve toward the most recently saved
  page, not alphabetically.
- Clippy's `--no-default-features` step and the `cargo bench` job in CI
  are no-ops here (the crate has no features and no benches). Leave them;
  they are harmless and will matter if either is added.

## When in doubt ##

Read the relevant section of `../links-spec/links.md`, then the module doc
comment, then the tests in that module. If the spec is silent, follow the
README's description of current behaviour and note the decision in a doc
comment citing the spec gap.
