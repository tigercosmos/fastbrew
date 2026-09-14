//! Manifest and blob downloads with Homebrew's cache layout (`docs/COMPAT.md` 2).
//!
//! - `fetch_manifest`: GET the OCI index with `Authorization: Bearer QQ==`
//!   (or the configured token) and the OCI index `Accept` header; cache it at
//!   `$CACHE/downloads/<sha256(url)>--<name>-<tag>.bottle_manifest.json` with
//!   the `$CACHE/<name>_bottle_manifest--<tag>` symlink; select the platform
//!   entry by `ref_name` and return its annotations parsed into `BottleTab`
//!   plus size information.
//! - `fetch_blob`: stream the blob to `$CACHE/downloads/<sha256(url)>--<filename>`
//!   (`.incomplete` while in flight), verifying sha256 as it streams, with a
//!   progress bar on a TTY; create the `$CACHE/<name>--<pkg_version>` symlink.
//!   Reuse an existing file whose checksum matches ("Already downloaded: ...").
//! - `fetch_all`: manifests then blobs for many bottles concurrently
//!   (`cfg.download_concurrency`), returning per-bottle results in input order.
//!
//! Ported from `download_strategy/curl_download_strategy.rb`,
//! `download_strategy/curl_github_packages_download_strategy.rb`,
//! `download_strategy/abstract_file_download_strategy.rb` and
//! `Resource::BottleManifest` (`resource.rb`). The cache file name is always
//! derived from the canonical ghcr.io URL, never from the
//! `HOMEBREW_ARTIFACT_DOMAIN` rewrite, so fastbrew and Homebrew share a cache.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rayon::prelude::*;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::output;

use super::{BottleRef, BottleTab};

/// Media type Homebrew asks ghcr.io for.
const OCI_INDEX_ACCEPT: &str = "application/vnd.oci.image.index.v1+json";
/// The anonymous ghcr.io token Homebrew uses (`brew.sh`: `Bearer QQ==`).
const ANONYMOUS_AUTH: &str = "Bearer QQ==";
/// Host whose URLs `HOMEBREW_ARTIFACT_DOMAIN` replaces.
const GITHUB_PACKAGES_HOST: &str = "ghcr.io";

#[derive(Debug, Clone)]
pub struct ManifestInfo {
    pub tab: BottleTab,
    pub bottle_size: Option<u64>,
    pub installed_size: Option<u64>,
    pub path: PathBuf,
}

/// Download (or reuse) the OCI index and return the entry for this bottle.
pub fn fetch_manifest(cfg: &Config, bottle: &BottleRef, quiet: bool) -> Result<ManifestInfo> {
    let url = bottle.manifest_url();
    let path = cfg.cache_downloads().join(format!(
        "{}--{}",
        url_hash(&url),
        safe_filename(&bottle.manifest_filename())
    ));

    if !quiet {
        println!("{}", output::format_ohai(&format!("Downloading {url}")));
    }
    let cached = path.is_file() && parse_manifest(&path, bottle).is_ok();
    if cached {
        if !quiet {
            println!("Already downloaded: {}", path.display());
        }
    } else {
        let body = http_get(cfg, &url, Some(OCI_INDEX_ACCEPT), cfg.curl_retries)?;
        write_via_incomplete(&path, &body)?;
        // A corrupt or unexpected manifest must not stay in the cache.
        if let Err(e) = parse_manifest(&path, bottle) {
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
    }
    link_into_cache(
        cfg,
        &path,
        &format!("{}_bottle_manifest--{}", bottle.name, bottle.manifest_tag()),
    )?;
    let (tab, bottle_size, installed_size) = parse_manifest(&path, bottle)?;
    Ok(ManifestInfo {
        tab,
        bottle_size,
        installed_size,
        path,
    })
}

/// Returns the path of the verified cached blob.
pub fn fetch_blob(cfg: &Config, bottle: &BottleRef, quiet: bool) -> Result<PathBuf> {
    let url = bottle.blob_url();
    let path = cfg.cache_downloads().join(format!(
        "{}--{}",
        url_hash(&url),
        safe_filename(&bottle.filename())
    ));

    if !quiet {
        println!("{}", output::format_ohai(&format!("Downloading {url}")));
    }
    let reusable = path.is_file() && file_sha256(&path)? == bottle.sha256.to_lowercase();
    if reusable {
        if !quiet {
            println!("Already downloaded: {}", path.display());
        }
    } else {
        download_blob(cfg, bottle, &url, &path, quiet)?;
    }
    link_into_cache(
        cfg,
        &path,
        &format!("{}--{}", bottle.name, bottle.pkg_version),
    )?;
    Ok(path)
}

/// Cached blob path if present and complete (no checksum verification).
pub fn cached_blob_path(cfg: &Config, bottle: &BottleRef) -> Option<PathBuf> {
    let url = bottle.blob_url();
    let path = cfg.cache_downloads().join(format!(
        "{}--{}",
        url_hash(&url),
        safe_filename(&bottle.filename())
    ));
    path.is_file().then_some(path)
}

/// Fetch every manifest, then every blob, at most `cfg.download_concurrency`
/// requests in flight. Results keep the input order.
pub fn fetch_all(
    cfg: &Config,
    bottles: &[BottleRef],
    quiet: bool,
) -> Vec<Result<(ManifestInfo, PathBuf)>> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cfg.download_concurrency.max(1))
        .build();
    let run = || {
        let manifests: Vec<Result<ManifestInfo>> = bottles
            .par_iter()
            .map(|b| fetch_manifest(cfg, b, quiet))
            .collect();
        let blobs: Vec<Result<PathBuf>> = bottles
            .par_iter()
            .zip(manifests.par_iter())
            .map(|(b, m)| match m {
                // A failed manifest means we do not know the bottle is there;
                // skip its blob rather than reporting a second failure.
                Err(_) => Err(Error::user("manifest download failed")),
                Ok(_) => fetch_blob(cfg, b, quiet),
            })
            .collect();
        manifests
            .into_iter()
            .zip(blobs)
            .map(|(m, b)| Ok((m?, b?)))
            .collect()
    };
    match pool {
        Ok(pool) => pool.install(run),
        Err(_) => run(),
    }
}

// ------------------------------------------------------------- manifests

/// Parse the cached index and pick this bottle's entry.
fn parse_manifest(
    path: &Path,
    bottle: &BottleRef,
) -> Result<(BottleTab, Option<u64>, Option<u64>)> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        Error::user(format!(
            "The downloaded GitHub Packages manifest could not be read: {}\n{e}",
            path.display()
        ))
    })?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|_| {
        Error::user(format!(
            "The downloaded GitHub Packages manifest was corrupted or modified (it is not valid JSON): \n{}",
            path.display()
        ))
    })?;
    let manifests = json
        .get("manifests")
        .and_then(|m| m.as_array())
        .ok_or_else(|| Error::user("Missing 'manifests' section."))?;
    let want = bottle.ref_name();
    let entry = manifests
        .iter()
        .filter_map(|m| m.get("annotations"))
        .find(|a| {
            a.get("org.opencontainers.image.ref.name")
                .and_then(|v| v.as_str())
                == Some(want.as_str())
        })
        .ok_or_else(|| {
            Error::user(format!(
                "Couldn't find a bottle manifest for {want} in {}",
                path.display()
            ))
        })?;

    let string = |key: &str| entry.get(key).and_then(|v| v.as_str());
    let number = |key: &str| string(key).and_then(|v| v.parse::<u64>().ok());

    if !bottle.sha256.is_empty()
        && let Some(digest) = string("sh.brew.bottle.digest")
        && !digest.eq_ignore_ascii_case(&bottle.sha256)
    {
        return Err(Error::user(format!(
            "Couldn't find manifest matching bottle checksum.\nExpected: {}\n  Actual: {digest}",
            bottle.sha256
        )));
    }

    let tab_json =
        string("sh.brew.tab").ok_or_else(|| Error::user("Couldn't find tab from manifest."))?;
    let tab: BottleTab =
        serde_json::from_str(tab_json).map_err(|_| Error::user("Couldn't parse tab JSON."))?;
    Ok((
        tab,
        number("sh.brew.bottle.size"),
        number("sh.brew.bottle.installed_size"),
    ))
}

// ----------------------------------------------------------------- blobs

fn download_blob(
    cfg: &Config,
    bottle: &BottleRef,
    url: &str,
    path: &Path,
    quiet: bool,
) -> Result<()> {
    let expected = bottle.sha256.to_lowercase();
    let incomplete = incomplete_path(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut last_error = None;
    for attempt in 0..=cfg.curl_retries {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(250 * u64::from(attempt)));
        }
        let _ = std::fs::remove_file(&incomplete);
        match stream_to_file(cfg, url, &incomplete, quiet, bottle) {
            Ok(actual) if actual == expected => {
                std::fs::rename(&incomplete, path)?;
                return Ok(());
            }
            Ok(actual) => {
                last_error = Some(Error::user(format!(
                    "SHA-256 mismatch\nExpected: {expected}\n  Actual: {actual}\n          File: {}\nTo retry an incomplete download, remove the file above.",
                    incomplete.display()
                )));
            }
            Err(e) => last_error = Some(e),
        }
    }
    let _ = std::fs::remove_file(&incomplete);
    Err(last_error.unwrap_or_else(|| Error::user(format!("Failed to download {url}"))))
}

/// Stream the response into `dest`, returning the hex sha256 of the bytes.
fn stream_to_file(
    cfg: &Config,
    url: &str,
    dest: &Path,
    quiet: bool,
    bottle: &BottleRef,
) -> Result<String> {
    let mut response = request(cfg, url, None)?;
    let total = response.content_length();
    let bar = progress_bar(quiet, total, &bottle.filename());

    let mut file = std::fs::File::create(dest)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    let mut done = 0u64;
    loop {
        let n = response
            .read(&mut buf)
            .map_err(|e| Error::Other(anyhow::Error::new(e).context(format!("reading {url}"))))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        done += n as u64;
        if let Some(bar) = &bar {
            bar.set_position(done);
        }
    }
    file.flush()?;
    if let Some(bar) = bar {
        bar.finish_and_clear();
    }
    Ok(hex::encode(hasher.finalize()))
}

fn progress_bar(quiet: bool, total: Option<u64>, label: &str) -> Option<indicatif::ProgressBar> {
    if quiet || !output::stdout_is_tty() {
        return None;
    }
    let bar = match total {
        Some(total) => {
            let bar = indicatif::ProgressBar::new(total);
            bar.set_style(
                indicatif::ProgressStyle::with_template(
                    "{msg:<38} {bar:30.cyan/blue} {bytes:>10}/{total_bytes:<10} {bytes_per_sec}",
                )
                .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar())
                .progress_chars("##-"),
            );
            bar
        }
        None => {
            let bar = indicatif::ProgressBar::new_spinner();
            bar.set_style(
                indicatif::ProgressStyle::with_template("{msg:<38} {spinner} {bytes:>10}")
                    .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner()),
            );
            bar
        }
    };
    bar.set_message(label.to_string());
    Some(bar)
}

// ------------------------------------------------------------------ http

fn http_get(cfg: &Config, url: &str, accept: Option<&str>, retries: u32) -> Result<Vec<u8>> {
    let mut last_error = None;
    for attempt in 0..=retries {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(250 * u64::from(attempt)));
        }
        match request(cfg, url, accept).and_then(|mut r| {
            let mut body = Vec::new();
            r.read_to_end(&mut body)?;
            Ok(body)
        }) {
            Ok(body) => return Ok(body),
            Err(e) => last_error = Some(e),
        }
    }
    Err(last_error.unwrap_or_else(|| Error::user(format!("Failed to download {url}"))))
}

fn request(cfg: &Config, url: &str, accept: Option<&str>) -> Result<reqwest::blocking::Response> {
    let request_url = artifact_domain_url(cfg, url);
    let client = client()?;
    let mut req = client
        .get(&request_url)
        .header(reqwest::header::USER_AGENT, user_agent());
    if let Some(auth) = authorization_for(cfg, url) {
        req = req.header(reqwest::header::AUTHORIZATION, auth);
    }
    if let Some(accept) = accept {
        req = req.header(reqwest::header::ACCEPT, accept);
    }
    let response = req
        .send()
        .map_err(|e| Error::user(format!("Failed to download {request_url}: {e}")))?;
    if !response.status().is_success() {
        return Err(Error::user(format!(
            "Failed to download {request_url}: HTTP {}",
            response.status().as_u16()
        )));
    }
    Ok(response)
}

fn client() -> Result<&'static reqwest::blocking::Client> {
    static CLIENT: std::sync::OnceLock<std::result::Result<reqwest::blocking::Client, String>> =
        std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(None)
                .connect_timeout(Duration::from_secs(30))
                // Homebrew's curl follows redirects to the CDN; reqwest drops
                // `Authorization` when the redirect crosses hosts, as curl's
                // `--location` does without `--location-trusted`.
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .map_err(|e| e.to_string())
        })
        .as_ref()
        .map_err(|e| Error::user(format!("Could not create an HTTP client: {e}")))
}

fn user_agent() -> String {
    format!(
        "Homebrew/{} (fastbrew/{})",
        crate::HOMEBREW_COMPAT_VERSION,
        crate::FASTBREW_VERSION
    )
}

/// `HOMEBREW_GITHUB_PACKAGES_AUTH` (`brew.sh`): basic auth when a user and a
/// token are configured, the bare token otherwise, else the anonymous bearer.
/// The `Authorization` header for one URL, before any artifact-domain rewrite.
///
/// Homebrew attaches `HOMEBREW_GITHUB_PACKAGES_AUTH` from
/// `CurlGitHubPackagesDownloadStrategy`, which `DownloadStrategyDetector` picks
/// only for `https://ghcr.io/v2/...` (plus the manifest of a user-configured
/// `HOMEBREW_BOTTLE_DOMAIN`). Everything else — a third-party tap's own
/// `bottle do root_url`, a mirror named in a formula — downloads with the plain
/// curl strategy and no credentials, so the registry token never reaches a host
/// the tap chose.
fn authorization_for(cfg: &Config, url: &str) -> Option<String> {
    if !is_credentialed_host(cfg, url) {
        return None;
    }
    authorization(cfg)
}

/// Whether `url` is one of the hosts registry credentials belong to: GitHub
/// Packages itself, or the bottle domain the user configured.
fn is_credentialed_host(cfg: &Config, url: &str) -> bool {
    let host_matches = |base: &str| -> bool {
        // Compare scheme and authority, so `https://ghcr.io.evil.test/` and a
        // path that merely starts with the domain never count.
        let Some((_, rest)) = base.split_once("://") else {
            return false;
        };
        let host = rest.split('/').next().unwrap_or(rest);
        ["https://", "http://"]
            .iter()
            .any(|scheme| url.starts_with(&format!("{scheme}{host}/")))
    };
    if host_matches(&format!("https://{GITHUB_PACKAGES_HOST}")) {
        return true;
    }
    cfg.bottle_domain != crate::config::DEFAULT_BOTTLE_DOMAIN && host_matches(&cfg.bottle_domain)
}

fn authorization(cfg: &Config) -> Option<String> {
    match (
        cfg.github_packages_user.as_deref(),
        cfg.github_packages_token.as_deref(),
    ) {
        (Some(user), Some(token)) => {
            use base64::Engine;
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"));
            Some(format!("Basic {encoded}"))
        }
        (None, Some(token)) => Some(format!("Bearer {token}")),
        // A mirror without credentials gets no header at all, as Homebrew does
        // when `HOMEBREW_DOCKER_REGISTRY_BASIC_AUTH_TOKEN=none`.
        _ if cfg.artifact_domain.is_some() => None,
        _ => Some(ANONYMOUS_AUTH.to_string()),
    }
}

/// `HOMEBREW_ARTIFACT_DOMAIN` replaces the ghcr.io host (keeping `/v2/` unless
/// the domain already contains it), as `CurlDownloadStrategy#fetch` does.
pub fn artifact_domain_url(cfg: &Config, url: &str) -> String {
    let Some(domain) = cfg.artifact_domain.as_deref() else {
        return url.to_string();
    };
    let domain = domain.trim_end_matches('/');
    let contains_v2 = domain
        .split_once("://")
        .map(|(_, rest)| {
            let path = rest.split_once('/').map(|(_, p)| p).unwrap_or("");
            path == "v2" || path.starts_with("v2/")
        })
        .unwrap_or(false);
    for scheme in ["https://", "http://"] {
        let prefix = format!("{scheme}{GITHUB_PACKAGES_HOST}/");
        if let Some(rest) = url.strip_prefix(&prefix) {
            return if contains_v2 {
                match rest.strip_prefix("v2/") {
                    Some(rest) => format!("{domain}/{rest}"),
                    None => format!("{domain}/{rest}"),
                }
            } else {
                format!("{domain}/{rest}")
            };
        }
    }
    url.to_string()
}

// ----------------------------------------------------------------- cache

/// `Digest::SHA256.hexdigest(url)`, the first half of a cached file's name.
pub fn url_hash(url: &str) -> String {
    hex::encode(Sha256::digest(url.as_bytes()))
}

/// `Utils.safe_filename`: drop control characters and path separators.
pub fn safe_filename(basename: &str) -> String {
    basename
        .chars()
        .filter(|c| *c != '/' && !c.is_control())
        .collect()
}

fn incomplete_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".incomplete");
    PathBuf::from(s)
}

fn write_via_incomplete(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let incomplete = incomplete_path(path);
    std::fs::write(&incomplete, data)?;
    std::fs::rename(&incomplete, path)?;
    Ok(())
}

/// `$CACHE/<link_name>` -> `downloads/<file>` (relative), replacing any old link.
fn link_into_cache(cfg: &Config, target: &Path, link_name: &str) -> Result<()> {
    std::fs::create_dir_all(&cfg.cache)?;
    let link = cfg.cache.join(safe_filename(link_name));
    let relative = crate::keg::relative_path(target, &cfg.cache);
    if let Ok(existing) = std::fs::read_link(&link)
        && existing == relative
    {
        return Ok(());
    }
    if link.is_symlink() || link.exists() {
        std::fs::remove_file(&link)?;
    }
    std::os::unix::fs::symlink(relative, &link)?;
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::BottleTag;

    fn bottle(name: &str, version: &str, rebuild: u32, sha: &str) -> BottleRef {
        BottleRef {
            name: name.into(),
            pkg_version: version.into(),
            rebuild,
            tag: BottleTag::parse("arm64_tahoe").unwrap(),
            root_url: "https://ghcr.io/v2/homebrew/core".into(),
            sha256: sha.into(),
        }
    }

    #[test]
    fn cache_names_match_homebrew() {
        // Verified against the host cache:
        //   hello--2.12.3 -> downloads/1a7b16...--hello--2.12.3.arm64_tahoe.bottle.1.tar.gz
        //   hello_bottle_manifest--2.12.3-1 -> downloads/8f283b...--hello-2.12.3-1.bottle_manifest.json
        let b = bottle(
            "hello",
            "2.12.3",
            1,
            "ae6237e3001bd354783f469d754cee875ee9828910461b85a5803f5990213dde",
        );
        assert_eq!(
            url_hash(&b.blob_url()),
            "1a7b1636951dd9c1b9236fdbf644f194f7c3f967935cd67e443a2111047d0576"
        );
        assert_eq!(
            url_hash(&b.manifest_url()),
            "8f283b04c650ce1898280cd4725fa165f51524dde5176821b43cc43bf131a1e6"
        );
        assert_eq!(b.filename(), "hello--2.12.3.arm64_tahoe.bottle.1.tar.gz");
        assert_eq!(b.manifest_filename(), "hello-2.12.3-1.bottle_manifest.json");
    }

    #[test]
    fn safe_filenames_drop_separators_and_control_characters() {
        assert_eq!(safe_filename("a/b\u{1}c"), "abc");
        assert_eq!(safe_filename("jq--1.8.2"), "jq--1.8.2");
    }

    #[test]
    fn artifact_domain_rewrites_the_ghcr_host() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_test(tmp.path());
        let url = "https://ghcr.io/v2/homebrew/core/jq/manifests/1.8.2-1";
        assert_eq!(artifact_domain_url(&cfg, url), url);

        cfg.artifact_domain = Some("https://mirror.example.com/".into());
        assert_eq!(
            artifact_domain_url(&cfg, url),
            "https://mirror.example.com/v2/homebrew/core/jq/manifests/1.8.2-1"
        );

        cfg.artifact_domain = Some("https://mirror.example.com/v2/ghcr-io".into());
        assert_eq!(
            artifact_domain_url(&cfg, url),
            "https://mirror.example.com/v2/ghcr-io/homebrew/core/jq/manifests/1.8.2-1"
        );

        // Non-ghcr URLs are left alone.
        assert_eq!(
            artifact_domain_url(&cfg, "https://example.org/a.tar.gz"),
            "https://example.org/a.tar.gz"
        );
    }

    #[test]
    fn authorization_headers() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_test(tmp.path());
        assert_eq!(authorization(&cfg).as_deref(), Some("Bearer QQ=="));
        cfg.github_packages_token = Some("tok".into());
        assert_eq!(authorization(&cfg).as_deref(), Some("Bearer tok"));
        cfg.github_packages_user = Some("me".into());
        // base64("me:tok")
        assert_eq!(authorization(&cfg).as_deref(), Some("Basic bWU6dG9r"));
        cfg.github_packages_user = None;
        cfg.github_packages_token = None;
        cfg.artifact_domain = Some("https://mirror.example.com".into());
        assert_eq!(authorization(&cfg), None);
    }

    #[test]
    fn credentials_only_reach_github_packages_and_the_configured_domain() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::for_test(tmp.path());
        cfg.github_packages_token = Some("secret".into());
        let ghcr = "https://ghcr.io/v2/homebrew/core/jq/manifests/1.8.2";
        assert_eq!(
            authorization_for(&cfg, ghcr).as_deref(),
            Some("Bearer secret")
        );

        // A third-party tap's `root_url` is just another host.
        for url in [
            "http://127.0.0.1:8080/v2/review/fixture/jq/blobs/sha256:aa",
            "https://bottles.example.com/v2/review/fixture/jq/manifests/99.0",
            // A host that only looks like ghcr.io.
            "https://ghcr.io.evil.test/v2/homebrew/core/jq/blobs/sha256:aa",
            // ... and one that only has it in the path.
            "https://evil.test/ghcr.io/v2/homebrew/core/jq/blobs/sha256:aa",
        ] {
            assert_eq!(authorization_for(&cfg, url), None, "{url}");
        }

        // A bottle domain the user configured does get them, the way
        // `github_packages_manifest_resource` keeps the strategy for it.
        cfg.bottle_domain = "https://mirror.example.com/v2/homebrew/core".into();
        assert_eq!(
            authorization_for(
                &cfg,
                "https://mirror.example.com/v2/homebrew/core/jq/blobs/sha256:aa"
            )
            .as_deref(),
            Some("Bearer secret")
        );
        assert_eq!(
            authorization_for(&cfg, ghcr).as_deref(),
            Some("Bearer secret")
        );
        assert_eq!(
            authorization_for(&cfg, "https://bottles.example.com/v2/x/y"),
            None
        );
    }

    #[test]
    fn parses_and_selects_the_platform_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("m.json");
        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": [
                {"annotations": {
                    "org.opencontainers.image.ref.name": "1.8.2.arm64_sonoma.1",
                    "sh.brew.bottle.digest": "aa",
                    "sh.brew.tab": "{\"changed_files\":[\"other\"],\"source_modified_time\":1}"
                }},
                {"annotations": {
                    "org.opencontainers.image.ref.name": "1.8.2.arm64_tahoe.1",
                    "sh.brew.bottle.digest": "bb",
                    "sh.brew.bottle.size": "439895",
                    "sh.brew.bottle.installed_size": "1235416",
                    "sh.brew.tab": "{\"homebrew_version\":\"6.0.18\",\"changed_files\":[\"lib/pkgconfig/libjq.pc\"],\"source_modified_time\":1781962615,\"compiler\":\"clang\",\"arch\":\"arm64\"}"
                }}
            ]
        });
        std::fs::write(&path, serde_json::to_vec(&index).unwrap()).unwrap();

        let b = bottle("jq", "1.8.2", 1, "BB");
        let (tab, size, installed) = parse_manifest(&path, &b).unwrap();
        assert_eq!(
            tab.changed_files.as_deref(),
            Some(&["lib/pkgconfig/libjq.pc".to_string()][..])
        );
        assert_eq!(tab.source_modified_time, 1_781_962_615);
        assert_eq!(tab.compiler.as_deref(), Some("clang"));
        assert_eq!(tab.linkage_files, None);
        assert_eq!(size, Some(439_895));
        assert_eq!(installed, Some(1_235_416));

        // A checksum that disagrees with the manifest is an error.
        let wrong = bottle("jq", "1.8.2", 1, "cc");
        let err = parse_manifest(&path, &wrong).unwrap_err();
        assert!(err.to_string().contains("bottle checksum"), "{err}");

        // A tag with no entry is an error.
        let mut other = bottle("jq", "1.8.2", 1, "");
        other.tag = BottleTag::parse("arm64_ventura").unwrap();
        let err = parse_manifest(&path, &other).unwrap_err();
        assert!(err.to_string().contains("1.8.2.arm64_ventura.1"), "{err}");
    }

    #[test]
    fn selects_the_all_tag_for_all_bottles() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("m.json");
        let index = serde_json::json!({
            "manifests": [
                {"annotations": {
                    "org.opencontainers.image.ref.name": "1.0.all",
                    "sh.brew.tab": "{\"source_modified_time\":7}"
                }}
            ]
        });
        std::fs::write(&path, serde_json::to_vec(&index).unwrap()).unwrap();
        let mut b = bottle("noarch", "1.0", 0, "");
        b.tag = BottleTag::all();
        assert_eq!(b.ref_name(), "1.0.all");
        let (tab, _, _) = parse_manifest(&path, &b).unwrap();
        assert_eq!(tab.source_modified_time, 7);
    }

    #[test]
    fn cached_blob_path_reports_only_complete_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let b = bottle("jq", "1.8.2", 1, "ca");
        assert_eq!(cached_blob_path(&cfg, &b), None);
        let path =
            cfg.cache_downloads()
                .join(format!("{}--{}", url_hash(&b.blob_url()), b.filename()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(incomplete_path(&path), b"partial").unwrap();
        assert_eq!(
            cached_blob_path(&cfg, &b),
            None,
            "incomplete must not count"
        );
        std::fs::write(&path, b"done").unwrap();
        assert_eq!(cached_blob_path(&cfg, &b), Some(path));
    }

    #[test]
    fn cache_symlink_is_relative_and_replaceable() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let target = cfg.cache_downloads().join("abc--jq--1.8.2.tar.gz");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"x").unwrap();
        link_into_cache(&cfg, &target, "jq--1.8.2").unwrap();
        let link = cfg.cache.join("jq--1.8.2");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("downloads/abc--jq--1.8.2.tar.gz")
        );
        // Re-linking an existing link is a no-op, and a stale link is replaced.
        link_into_cache(&cfg, &target, "jq--1.8.2").unwrap();
        let other = cfg.cache_downloads().join("def--jq--1.8.2.tar.gz");
        std::fs::write(&other, b"y").unwrap();
        link_into_cache(&cfg, &other, "jq--1.8.2").unwrap();
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("downloads/def--jq--1.8.2.tar.gz")
        );
    }
}
