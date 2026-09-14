//! Conditional download of API files into `$CACHE/api/` (`docs/COMPAT.md` 1.1).
//!
//! Port of `Homebrew::API.fetch_json_api_file`: `curl --compressed
//! --time-cond <cached file>` becomes a conditional GET with
//! `If-Modified-Since`, the cached file's mtime is touched after a successful
//! revalidation, and `.jws.json` endpoints are verified before the cached copy
//! is replaced.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::output;
use crate::platform::BottleTag;

/// Homebrew's `DEFAULT_API_STALE_SECONDS` (7 days).
pub const DEFAULT_API_STALE_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Staleness for the per-package JSON endpoints used by `info --json`.
pub const JSON_ENDPOINT_STALE_SECONDS: u64 = 24 * 60 * 60;

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

/// Endpoint name of the internal packages file for `tag`.
pub fn packages_endpoint(tag: &BottleTag) -> String {
    format!("{}{tag}.jws.json", super::INTERNAL_PACKAGES_ENDPOINT_PREFIX)
}

/// `Homebrew::API.skip_download?`: a non-empty cached file younger than
/// `stale_secs` needs no request. `None` always revalidates.
fn skip_download(target: &Path, stale_secs: Option<u64>) -> bool {
    let Ok(md) = std::fs::metadata(target) else {
        return false;
    };
    if md.len() == 0 {
        return false;
    }
    let Some(stale) = stale_secs else {
        return false;
    };
    let Ok(mtime) = md.modified() else {
        return false;
    };
    SystemTime::now()
        .checked_sub(Duration::from_secs(stale))
        .is_some_and(|threshold| threshold < mtime)
}

/// Download `internal/packages.<tag>.jws.json` if the cached copy is older
/// than `stale_secs` (None = always revalidate) using `If-Modified-Since`.
/// Verifies the JWS before atomically replacing the file and touches the
/// mtime after a successful revalidation. Fails with Homebrew's MITM message
/// on signature failure and deletes the bad file.
pub fn fetch_packages(
    cfg: &Config,
    tag: &BottleTag,
    stale_secs: Option<u64>,
    quiet: bool,
) -> Result<FetchOutcome> {
    let target = packages_path(cfg, tag);
    fetch_api_file(
        cfg,
        &packages_endpoint(tag),
        &target,
        stale_secs,
        quiet,
        true,
    )
}

/// Fetch a plain JSON endpoint such as `formula/<name>.json` (v2 schema),
/// cached at `$CACHE/api/<endpoint>`.
pub fn fetch_json_endpoint(
    cfg: &Config,
    endpoint: &str,
    stale_secs: Option<u64>,
) -> Result<serde_json::Value> {
    let target = cfg.cache_api().join(endpoint);
    let outcome = fetch_api_file(cfg, endpoint, &target, stale_secs, true, false);
    match outcome {
        Ok(_) => {}
        Err(e) => {
            // A cached copy is still usable when the network is unavailable.
            if !target.is_file() {
                return Err(e);
            }
        }
    }
    let text = std::fs::read(&target)
        .map_err(|e| Error::user(format!("Failed to read {}: {e}", target.display())))?;
    serde_json::from_slice(&text).map_err(|e| {
        Error::user(format!(
            "Cannot parse {}: {e}. Run `fastbrew update` and try again.",
            target.display()
        ))
    })
}

/// The shared conditional-fetch body. `verify_jws` enables the signature check
/// (and the MITM error) for `.jws.json` endpoints.
fn fetch_api_file(
    cfg: &Config,
    endpoint: &str,
    target: &Path,
    stale_secs: Option<u64>,
    quiet: bool,
    verify_jws: bool,
) -> Result<FetchOutcome> {
    if skip_download(target, stale_secs) {
        return Ok(FetchOutcome::Unchanged);
    }
    let url = format!("{}/{endpoint}", cfg.api_domain.trim_end_matches('/'));

    if !quiet && output::stdout_is_tty() {
        output::ohai(&format!("Downloading {url}"));
    }

    let cached_mtime = std::fs::metadata(target)
        .ok()
        .filter(|m| m.len() > 0)
        .and_then(|m| m.modified().ok());

    match request(&url, cached_mtime, cfg.curl_retries) {
        Ok(Response::NotModified) => {
            touch_now(target);
            Ok(FetchOutcome::Unchanged)
        }
        Ok(Response::Body(body)) => {
            if verify_jws && let Err(reason) = crate::api::jws::verify(&body) {
                let _ = std::fs::remove_file(target);
                return Err(Error::user(format!(
                    "Failed to verify integrity ({reason}) of:\n  {url}\nPotential MITM attempt detected. Please run `brew update` and try again."
                )));
            }
            atomic_replace(target, &body)?;
            touch_now(target);
            Ok(FetchOutcome::Updated)
        }
        Err(e) => {
            if target.is_file() {
                let name = target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| endpoint.to_string());
                output::opoo(&format!(
                    "{name}: update failed, falling back to cached version."
                ));
                Ok(FetchOutcome::Offline)
            } else {
                Err(Error::user(format!("Failed to download {url}: {e}")))
            }
        }
    }
}

enum Response {
    NotModified,
    Body(Vec<u8>),
}

fn request(
    url: &str,
    if_modified_since: Option<SystemTime>,
    retries: u32,
) -> std::result::Result<Response, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(user_agent())
        .gzip(true)
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    let mut last_error = String::new();
    for attempt in 0..=retries.max(0) {
        let mut req = client.get(url);
        if let Some(t) = if_modified_since {
            req = req.header(reqwest::header::IF_MODIFIED_SINCE, http_date(t));
        }
        match req.send() {
            Ok(resp) if resp.status() == reqwest::StatusCode::NOT_MODIFIED => {
                return Ok(Response::NotModified);
            }
            Ok(resp) if resp.status().is_success() => {
                return resp
                    .bytes()
                    .map(|b| Response::Body(b.to_vec()))
                    .map_err(|e| e.to_string());
            }
            Ok(resp) => last_error = format!("HTTP status: {}", resp.status().as_u16()),
            Err(e) => last_error = e.to_string(),
        }
        if attempt < retries {
            std::thread::sleep(Duration::from_millis(250 * (attempt as u64 + 1)));
        }
    }
    Err(last_error)
}

fn user_agent() -> String {
    format!(
        "Homebrew/{} (Macintosh) fastbrew/{}",
        crate::HOMEBREW_COMPAT_VERSION,
        crate::FASTBREW_VERSION
    )
}

/// IMF-fixdate, the format `curl --time-cond` sends.
fn http_date(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_default()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

fn touch_now(path: &Path) {
    let _ = filetime::set_file_mtime(path, filetime::FileTime::now());
}

fn atomic_replace(target: &Path, data: &[u8]) -> Result<()> {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(data)?;
    tmp.flush()?;
    tmp.persist(target).map_err(|e| e.error)?;
    Ok(())
}

/// Path of a cached internal packages file inside the test sandbox, if the
/// sandbox has one. Never looks at the host Homebrew cache.
#[cfg(test)]
pub fn sandbox_packages_file_for_tests() -> Option<PathBuf> {
    let cache = std::env::var_os("HOMEBREW_CACHE")?;
    let tag = crate::platform::Host::detect().bottle_tag();
    let path = PathBuf::from(cache).join(format!("api/internal/packages.{tag}.jws.json"));
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_date_format() {
        let t = UNIX_EPOCH + Duration::from_secs(784_111_777);
        assert_eq!(http_date(t), "Sun, 06 Nov 1994 08:49:37 GMT");
    }

    #[test]
    fn skip_download_rules() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x");
        assert!(!skip_download(&f, Some(60)));
        std::fs::write(&f, b"hi").unwrap();
        assert!(skip_download(&f, Some(600)));
        assert!(!skip_download(&f, None));
        filetime::set_file_mtime(&f, filetime::FileTime::from_unix_time(1_000_000, 0)).unwrap();
        assert!(!skip_download(&f, Some(600)));
    }

    #[test]
    fn packages_path_matches_homebrew() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::from_env().unwrap();
        cfg.cache = dir.path().to_path_buf();
        let tag = BottleTag::parse("arm64_tahoe").unwrap();
        assert!(packages_path(&cfg, &tag).ends_with("api/internal/packages.arm64_tahoe.jws.json"));
        assert_eq!(
            packages_endpoint(&tag),
            "internal/packages.arm64_tahoe.jws.json"
        );
    }
}
