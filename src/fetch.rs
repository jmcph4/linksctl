//! Web retrieval: bring a Webtronix page index in line with a URL list
//! (links.md "Web retrieval", `linksctl fetch`).

use std::path::Path;

use eyre::{Context, Result};
use tracing::{debug, info};
use webtronix::{Index, Options, Summary};

/// Fetch every URL in `urls` into the index at `index`, creating it when
/// needed. URLs already indexed are re-requested conditionally, so a rerun
/// is cheap. Webtronix logs and skips a URL that fails; only a failure of
/// the index itself is an error.
pub async fn fetch(urls: &[String], index: &Path, options: &Options) -> Result<Summary> {
    let index = Index::open(index).wrap_err("opening page index")?;
    let total = urls.len();
    let mut done = 0usize;
    let summary = index
        .update(urls, options, |url| {
            done += 1;
            debug!(url, done, total, "fetched");
            if done.is_multiple_of(100) || done == total {
                info!(done, total, "fetch progress");
            }
        })
        .await
        .wrap_err("updating page index")?;
    info!(
        written = summary.written,
        pruned = summary.pruned,
        "index updated"
    );
    Ok(summary)
}
