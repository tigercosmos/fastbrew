//! Conditional download of API files into `$CACHE/api/` (`docs/COMPAT.md` 1.1).

use std::path::PathBuf;

use crate::config::Config;
use crate::error::Result;
use crate::platform::BottleTag;

/// Outcome of a fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    /// Server returned 304 or the local copy was younger than `stale_secs`.
    Unchanged,
    /// A new verified file replaced the cached one.
    Updated,
    /// Network failed but a cached copy exists (a warning was printed).
    Offline,
}

/// Path of the cached internal packages file for `tag`.
pub fn packages_path(cfg: &Config, tag: &BottleTag) -> PathBuf {
    cfg.cache_api()
        .join(format!("internal/packages.{tag}.jws.json"))
}

/// Download `internal/packages.<tag>.jws.json` if the cached copy is older
/// than `stale_secs` (None = always revalidate) using `If-Modified-Since`.
/// Verifies the JWS before atomically replacing the file and touches the
/// mtime after a successful revalidation. Fails with Homebrew's MITM message
/// on signature failure and deletes the bad file.
pub fn fetch_packages(
    _cfg: &Config,
    _tag: &BottleTag,
    _stale_secs: Option<u64>,
    _quiet: bool,
) -> Result<FetchOutcome> {
    todo!("api::fetch::fetch_packages")
}

/// Fetch a plain JSON endpoint such as `formula/<name>.json` (v2 schema),
/// cached at `$CACHE/api/<endpoint>`.
pub fn fetch_json_endpoint(
    _cfg: &Config,
    _endpoint: &str,
    _stale_secs: Option<u64>,
) -> Result<serde_json::Value> {
    todo!("api::fetch::fetch_json_endpoint")
}
