# linksctl #

`linksctl` turns a Markdown bookmark log (the *resources file*) into two
static directory pages: a **private** page listing every saved URL by topic,
and a **public** page listing only domains and save counts. It is the
reference implementation of the `links` spec (`../links-spec/links.md`).

## Usage ##

Build and install with cargo (stable Rust, edition 2024):

```sh
cargo install --path .
```

You will want a Chrome or Chromium binary on `PATH` (or named with
`--chrome`) so pages are rendered before they are stored, and one of:

- an Anthropic API key, in `linksctl.toml` under `[llm]` as `api_key` or
  `api_key_file`, for the default `--llm api` mode; or
- the `claude` CLI on `PATH`, for `--llm agent` mode, which needs no key.

The simplest invocation runs the whole pipeline from a resources file and
writes `index.db`, `categories.json`, `directory-private.html` and
`directory-public.html` into the working directory:

```sh
linksctl run resources.md
linksctl run resources.md --llm agent --out site/     # no API key, pages into site/
```

Re-running is incremental: pages already fetched are re-requested
conditionally, and only pages without a label are sent to the model.

The pipeline has four stages, each a subcommand, and `run` does them all:

| Stage                | Subcommand                       | Input           | Output                         |
| -------------------- | -------------------------------- | --------------- | ------------------------------ |
| URL extraction       | `linksctl urls <RESOURCESFILE>`  | resources file  | one URL per line               |
| Web retrieval        | `linksctl fetch <URLS>`          | URL list        | page index (`index.db`)        |
| Categorisation       | `linksctl categorise <PAGEINDEX>`| page index      | labelling (JSON + table)       |
| Artefact generation  | `linksctl build <CATEGORIES>`    | labelling       | `directory-{private,public}.html` |

Run them one at a time to inspect or redo a single stage:

```sh
linksctl urls resources.md -o urls.txt
linksctl fetch urls.txt --index index.db
linksctl categorise index.db --resources resources.md -o categories.json
linksctl build categories.json --resources resources.md --banlist banlist.txt
linksctl build index.db --resources resources.md --public-only   # labels from the index's `categories` table
```

`linksctl <SUBCOMMAND> --help` lists every option; `-v` logs at debug
level and `-q` errors only.

## Configuration ##

Every option can be set on the command line or in a TOML file
(`linksctl.toml` in the working directory, or `--config <PATH>`); the
command line wins over the file and the file over the default. No
environment variable is read. See `linksctl.example.toml` for every key.

## Stages ##

**Web retrieval** uses the [Webtronix](https://github.com/Alpine-Computing-Research/webtronix)
library with its `render = always` and `format = html` settings fixed:
every HTML page is loaded in a headless Chrome (`--chrome` to name the
binary) so its JavaScript runs, and the rendered HTML is stored as is.
Text is never extracted from HTML; PDFs are converted to text, as Webtronix
always does. The index is a SQLite file with Webtronix's `pages` table;
re-running sends conditional requests so unchanged pages are not
downloaded again. Without a Chrome or Chromium binary Webtronix warns once
and stores pages as served.

**Categorisation** has the model do two things. First it is shown the
whole collection (the domain and title of every page; collections larger
than `--taxonomy-chars` are sampled evenly) and proposes the taxonomy:
categories, each with subcategories, specific to what the collection
contains. An *everything else / unfiled* bucket is appended for pages it
cannot place. Then every page is labelled against that taxonomy in
batches. The taxonomy is never supplied by the user; it is stored with the
labels, so a later run labels only new pages against the same taxonomy.
`--force` discards the stored taxonomy and labels and starts over.

Two modes, as the spec requires:

- `--llm api` (default) calls the Anthropic Messages API with structured
  output. It needs a key from `api_key` or `api_key_file` in the config
  file, or `--api-key-file`. The model defaults to `claude-opus-5-5`.
- `--llm agent` runs a local harness, `claude -p` by default
  (`--agent-command` to change), with the prompt on stdin and
  `--json-schema` for the same structured output.

Pages are sent in batches (`--batch-size`, `--max-chars` each of the
visible text of the stored HTML, markup removed for the prompt only) with
`--llm-concurrency` requests in flight. The taxonomy and labels are written
to `taxonomy` and `categories` tables in the page index (unless
`--no-table`) and the complete labelling to a JSON file (`-o`). Pass
`--resources` so pages the index has no row for (fetch failures) are
labelled from their title.

**Build** reads a labelling (the JSON file or the page index), which
carries the taxonomy that orders the page, and the resources file
(`--resources`, for titles and dates), groups entries by
domain (lowercased, `www.` stripped), files each domain by majority vote
over its pages' labels (ties go to the most recently saved page) and writes
both pages, or one with `--private-only` / `--public-only`. A `--banlist`
file lists URLs (exact) or domain names (the domain and its subdomains),
one per line, to leave out of every page built.

The pages embed their data as JSON (`order`, `domains`, `public`) and the
exemplar stylesheet and script; the public page omits each domain's page
list and titles.

## Development ##

```sh
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --document-private-items
```

Tests are hermetic; `tests/fixtures/resources-exemplar.md` is the spec's
sample resources file. The `.cargo/config.toml` makes cargo fetch the
Webtronix git dependency with the `git` binary so a global
`insteadOf` SSH rewrite works.
