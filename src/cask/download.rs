//! Cask downloads with `url_kwargs` (user agent, referer, cookies, headers),
//! sha256 verification (`:no_check` skips), and Homebrew's cache naming
//! (`$CACHE/downloads/<sha256(url)>--<basename>`, symlink
//! `$CACHE/Cask/<token>--<version>.<ext>`).
//!
//! Port of `download_strategy/abstract_file_download_strategy.rb`,
//! `download_strategy/curl_download_strategy.rb` and `cask/download.rb`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{CaskEntry, sym};
use crate::output;

/// `HOMEBREW_USER_AGENT_FAKE_SAFARI` (`Library/Homebrew/global.rb`).
pub const FAKE_SAFARI_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 \
     (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

/// Request options decoded from the API's `url_kwargs`.
#[derive(Debug, Clone, Default)]
pub struct UrlKwargs {
    pub user_agent: Option<String>,
    pub referer: Option<String>,
    pub cookies: Vec<(String, String)>,
    pub headers: Vec<String>,
    pub data: Vec<(String, String)>,
    /// `:using` download strategy symbol (`post`, `nounzip`, ...).
    pub using: Option<String>,
    /// `:only_path`: subdirectory of the staged container holding the artifacts.
    pub only_path: Option<String>,
}

impl UrlKwargs {
    pub fn from_value(value: Option<&Value>) -> UrlKwargs {
        let mut out = UrlKwargs::default();
        let Some(map) = value.and_then(Value::as_object) else {
            return out;
        };
        for (key, value) in map {
            match sym(key) {
                "user_agent" => {
                    out.user_agent = value.as_str().map(|s| match sym(s) {
                        "fake" | "browser" => FAKE_SAFARI_USER_AGENT.to_string(),
                        "default" | "curl" => default_user_agent(),
                        other if s.starts_with(':') => other.to_string(),
                        _ => s.to_string(),
                    });
                }
                "referer" => out.referer = value.as_str().map(str::to_string),
                "cookies" => {
                    if let Some(obj) = value.as_object() {
                        out.cookies = obj
                            .iter()
                            .map(|(k, v)| {
                                (
                                    sym(k).to_string(),
                                    v.as_str().unwrap_or_default().to_string(),
                                )
                            })
                            .collect();
                    }
                }
                "header" | "headers" => match value {
                    Value::String(s) => out.headers.push(s.clone()),
                    Value::Array(a) => out
                        .headers
                        .extend(a.iter().filter_map(Value::as_str).map(str::to_string)),
                    _ => {}
                },
                "data" => {
                    if let Some(obj) = value.as_object() {
                        out.data = obj
                            .iter()
                            .map(|(k, v)| {
                                (
                                    sym(k).to_string(),
                                    v.as_str().unwrap_or_default().to_string(),
                                )
                            })
                            .collect();
                    }
                }
                "using" => out.using = value.as_str().map(|s| sym(s).to_string()),
                "only_path" => out.only_path = value.as_str().map(str::to_string),
                _ => {}
            }
        }
        out
    }

    fn is_post(&self) -> bool {
        self.using.as_deref() == Some("post") || !self.data.is_empty()
    }
}

/// `HOMEBREW_USER_AGENT_CURL`-shaped default agent (`brew.sh`).
pub fn default_user_agent() -> String {
    let host = crate::platform::Host::detect();
    let version = host
        .macos
        .map(|v| v.to_string())
        .unwrap_or_else(|| "0".into());
    format!(
        "Homebrew/{} ({}; {} Mac OS X {version})",
        crate::HOMEBREW_COMPAT_VERSION,
        host.system_name(),
        host.arch.as_str(),
    )
}

/// `Utils.safe_filename`: strip control characters and path separators.
pub fn safe_filename(basename: &str) -> String {
    basename
        .chars()
        .filter(|c| !c.is_control() && *c != '/')
        .collect()
}

/// Homebrew's `Pathname#extname` monkeypatch: keeps double archive extensions
/// and refuses to treat a trailing version number as an extension.
pub fn extname(basename: &str) -> String {
    for ext in [
        ".tar.gz",
        ".tar.bz2",
        ".tar.lz",
        ".tar.xz",
        ".tar.zst",
        ".tar.Z",
        ".cpio.gz",
        ".cpio.bz2",
        ".cpio.lz",
        ".cpio.xz",
        ".cpio.zst",
        ".cpio.Z",
        ".pax.gz",
        ".pax.bz2",
        ".pax.lz",
        ".pax.xz",
        ".pax.zst",
        ".pax.Z",
    ] {
        if basename.ends_with(ext) {
            return ext.to_string();
        }
    }
    // `return "" if basename.match?(/\b\d+\.\d+[^.]*\Z/) && !basename.end_with?(".7z")`
    if !basename.ends_with(".7z") && looks_like_trailing_version(basename) {
        return String::new();
    }
    match basename.rfind('.') {
        Some(0) | None => String::new(),
        Some(idx) => basename[idx..].to_string(),
    }
}

fn looks_like_trailing_version(basename: &str) -> bool {
    // `\b\d+\.\d+[^.]*\Z`: digits, a dot, digits, then no further dot.
    let Some(last_dot) = basename.rfind('.') else {
        return false;
    };
    let tail = &basename[last_dot + 1..];
    if tail.is_empty() || !tail.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    let head = &basename[..last_dot];
    let digits: String = head
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        return false;
    }
    // `\b` before the digits: start of string or a non-word character.
    let before = head[..head.len() - digits.len()].chars().next_back();
    !matches!(before, Some(c) if c.is_alphanumeric() || c == '_')
}

/// `AbstractFileDownloadStrategy#parse_basename`.
pub fn parse_basename(url: &str, search_query: bool) -> String {
    let mut path_parts: Vec<String> = Vec::new();
    let mut query_parts: Vec<String> = Vec::new();
    let mut file_url = false;

    match url::Url::parse(url) {
        Ok(parsed) => {
            file_url = parsed.scheme() == "file";
            if let Some(query) = parsed.query().filter(|q| !q.is_empty()) {
                for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
                    if search_query {
                        query_parts.push(value.to_string());
                    }
                    if key != "response-content-disposition" {
                        continue;
                    }
                    if let Some(name) = content_disposition_filename(&value) {
                        return basename_of(&name);
                    }
                }
            }
            path_parts = parsed
                .path()
                .split('/')
                .filter(|p| !p.is_empty())
                .map(percent_decode)
                .collect();
        }
        Err(_) => path_parts.push(url.to_string()),
    }

    if !file_url {
        for part in path_parts.iter().chain(query_parts.iter()).rev() {
            if !extname(&basename_of(part)).is_empty() {
                return basename_of(part);
            }
        }
    }
    match path_parts.last() {
        Some(last) if !last.is_empty() => basename_of(last),
        _ => String::new(),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn basename_of(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

/// Extract `filename`/`filename*` from a `Content-Disposition` header value.
pub fn content_disposition_filename(header: &str) -> Option<String> {
    let mut fallback = None;
    for part in header.split(';') {
        let part = part.trim();
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().trim_matches('"');
        if key == "filename*" {
            // `<charset>''<percent-encoded name>`
            if let Some((_charset, encoded)) = value.split_once("''")
                && !encoded.is_empty()
            {
                let decoded = percent_decode(encoded);
                if !decoded.trim_matches('"').is_empty() {
                    return Some(basename_of(decoded.trim_matches('"')));
                }
            }
        } else if key == "filename" && !value.is_empty() {
            fallback = Some(basename_of(value));
        }
    }
    fallback
}

/// `$CACHE/downloads/<sha256 of the url>--<basename>`, reusing an existing
/// download whose basename differs (`AbstractFileDownloadStrategy#cached_location`).
pub fn cached_location(cfg: &Config, url: &str, resolved_basename: &str) -> PathBuf {
    let digest = hex::encode(Sha256::digest(url.as_bytes()));
    let downloads = cfg.cache_downloads();
    let existing: Vec<PathBuf> = std::fs::read_dir(&downloads)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| {
                            n.starts_with(&format!("{digest}--")) && !n.ends_with(".incomplete")
                        })
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    if existing.len() == 1 {
        return existing.into_iter().next().expect("one entry");
    }
    downloads.join(format!("{digest}--{}", safe_filename(resolved_basename)))
}

/// `$CACHE/Cask/<token>--<version><ext>` (`#symlink_location`).
pub fn symlink_location(cfg: &Config, token: &str, version: &str, url: &str) -> PathBuf {
    let ext = extname(&parse_basename(url, true));
    cfg.cache
        .join("Cask")
        .join(safe_filename(&format!("{token}--{version}{ext}")))
}

/// Strip the `<sha256>--` prefix from a cached download's file name
/// (`AbstractFileDownloadStrategy#basename`).
pub fn cached_basename(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    match name.split_once("--") {
        Some((digest, rest))
            if digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()) =>
        {
            rest.to_string()
        }
        _ => name.to_string(),
    }
}

/// Download the cask's `url`, verify its checksum and return the cached path.
pub fn download_cask(cfg: &Config, cask: &CaskEntry, quiet: bool) -> Result<PathBuf> {
    let url = cask
        .url()
        .ok_or_else(|| Error::user(format!("Cask '{}' has no url.", cask.token)))?;
    let kwargs = UrlKwargs::from_value(cask.url_kwargs.as_ref());
    let version = cask.version.as_deref().unwrap_or("latest");

    let path = fetch(cfg, url, &kwargs, quiet)?;

    // Symlink under `$CACHE/Cask` so `brew` and fastbrew share the download.
    let link = symlink_location(cfg, &cask.token, version, url);
    let _ = std::fs::create_dir_all(link.parent().unwrap_or(Path::new("/")));
    let _ = std::fs::remove_file(&link);
    let _ = crate::keg::make_relative_symlink(&link, &path);

    verify_checksum(cask, &path)?;
    Ok(path)
}

/// Verify the download against the cask's `sha256` (`:no_check` skips).
pub fn verify_checksum(cask: &CaskEntry, path: &Path) -> Result<()> {
    if cask.sha256_no_check() {
        return Ok(());
    }
    let Some(expected) = cask.sha256.as_deref().filter(|s| !s.starts_with(':')) else {
        return Ok(());
    };
    let actual = file_sha256(path)?;
    if actual.eq_ignore_ascii_case(expected) {
        return Ok(());
    }
    let _ = std::fs::remove_file(path);
    Err(Error::user(format!(
        "SHA-256 mismatch\nExpected: {}\n  Actual: {}\n    File: {}\nTo retry an incomplete download, remove the file above.",
        output::green(expected),
        output::red(&actual),
        path.display(),
    )))
}

pub fn file_sha256(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Fetch `url` into the shared download cache, reusing a complete download.
pub fn fetch(cfg: &Config, url: &str, kwargs: &UrlKwargs, quiet: bool) -> Result<PathBuf> {
    if !quiet {
        output::ohai(&format!("Downloading {url}"));
    }

    let guessed = parse_basename(url, true);
    let cached = cached_location(cfg, url, &guessed);
    if cached.exists() {
        if !quiet {
            println!("Already downloaded: {}", cached.display());
        }
        return Ok(cached);
    }

    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(None)
        .connect_timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Other(e.into()))?;

    let mut request = if kwargs.is_post() {
        client.post(url)
    } else {
        client.get(url)
    };
    request = request.header(
        "User-Agent",
        kwargs.user_agent.clone().unwrap_or_else(default_user_agent),
    );
    request = request.header("Accept-Language", "en");
    if let Some(referer) = &kwargs.referer {
        request = request.header("Referer", referer);
    }
    if !kwargs.cookies.is_empty() {
        let cookie = kwargs
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(";");
        request = request.header("Cookie", cookie);
    }
    for header in &kwargs.headers {
        if let Some((name, value)) = header.split_once(':') {
            request = request.header(name.trim(), value.trim());
        }
    }
    if !kwargs.data.is_empty() {
        request = request.form(&kwargs.data);
    }

    let mut response = request
        .send()
        .map_err(|e| Error::user(format!("Failed to download resource \"{url}\"\n{e}")))?;
    if !response.status().is_success() {
        return Err(Error::user(format!(
            "Failed to download resource \"{url}\"\nDownload failed: {}",
            response.status()
        )));
    }

    let final_url = response.url().to_string();
    let disposition = response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(content_disposition_filename);
    let total = response.content_length();
    let basename = disposition.unwrap_or_else(|| parse_basename(&final_url, final_url == url));
    let basename = if basename.is_empty() {
        guessed
    } else {
        basename
    };

    let target = cached_location(cfg, url, &basename);
    std::fs::create_dir_all(target.parent().unwrap_or(Path::new("/")))?;
    let temporary = PathBuf::from(format!("{}.incomplete", target.display()));

    let bar = (!quiet && output::stdout_is_tty()).then(|| {
        let bar = match total {
            Some(len) => indicatif::ProgressBar::new(len),
            None => indicatif::ProgressBar::new_spinner(),
        };
        bar.set_style(
            indicatif::ProgressStyle::with_template(
                "{bar:40.cyan/blue} {bytes}/{total_bytes} ({eta})",
            )
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar()),
        );
        bar
    });

    {
        let mut out = std::fs::File::create(&temporary)?;
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = response.read(&mut buf).map_err(|e| {
                let _ = std::fs::remove_file(&temporary);
                Error::user(format!("Failed to download resource \"{url}\"\n{e}"))
            })?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            if let Some(bar) = &bar {
                bar.inc(n as u64);
            }
        }
        out.flush()?;
    }
    if let Some(bar) = bar {
        bar.finish_and_clear();
    }

    std::fs::rename(&temporary, &target)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extnames_match_homebrew() {
        assert_eq!(extname("foo-1.0.tar.gz"), ".tar.gz");
        assert_eq!(extname("Ghostty.dmg"), ".dmg");
        assert_eq!(extname("NotoSans-v2.015.zip"), ".zip");
        // A trailing version is not an extension.
        assert_eq!(extname("google-cloud-sdk-540.0.0"), "");
        assert_eq!(extname("app-1.2"), "");
        assert_eq!(extname("archive.7z"), ".7z");
    }

    #[test]
    fn basenames_from_urls() {
        assert_eq!(
            parse_basename("https://release.files.ghostty.org/1.3.1/Ghostty.dmg", true),
            "Ghostty.dmg"
        );
        assert_eq!(
            parse_basename("https://example.com/download.php?file=foo-1.0.tar.gz", true),
            "foo-1.0.tar.gz"
        );
        assert_eq!(
            parse_basename("https://example.com/a/b/Some%20App.zip", true),
            "Some App.zip"
        );
        assert_eq!(
            parse_basename(
                "https://example.com/get?response-content-disposition=attachment;%20filename=%22real.pkg%22",
                true
            ),
            "real.pkg"
        );
    }

    #[test]
    fn content_disposition_names() {
        assert_eq!(
            content_disposition_filename("attachment; filename=\"myapp-1.2.3.pkg\""),
            Some("myapp-1.2.3.pkg".to_string())
        );
        assert_eq!(
            content_disposition_filename(
                "attachment; filename=\"fallback.pkg\"; filename*=UTF-8''real%20name.pkg"
            ),
            Some("real name.pkg".to_string())
        );
        // Directory traversal is reduced to the basename.
        assert_eq!(
            content_disposition_filename("attachment; filename=\"../../etc/passwd\""),
            Some("passwd".to_string())
        );
    }

    #[test]
    fn url_kwargs_decode_fake_user_agent() {
        let kwargs = UrlKwargs::from_value(Some(&serde_json::json!({
            ":user_agent": ":fake",
            ":referer": "https://example.com/",
            ":cookies": {"a": "b"},
            ":header": ["X-Test: 1"],
            ":using": ":post",
            ":data": {"k": "v"}
        })));
        assert_eq!(kwargs.user_agent.as_deref(), Some(FAKE_SAFARI_USER_AGENT));
        assert_eq!(kwargs.referer.as_deref(), Some("https://example.com/"));
        assert_eq!(kwargs.cookies, vec![("a".into(), "b".into())]);
        assert_eq!(kwargs.headers, vec!["X-Test: 1".to_string()]);
        assert!(kwargs.is_post());
    }

    #[test]
    fn cache_names() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::cask::tests_support::config(tmp.path());
        let url = "https://example.com/Demo-1.0.zip";
        let digest = hex::encode(Sha256::digest(url.as_bytes()));
        assert_eq!(
            cached_location(&cfg, url, "Demo-1.0.zip"),
            cfg.cache_downloads()
                .join(format!("{digest}--Demo-1.0.zip"))
        );
        assert_eq!(
            symlink_location(&cfg, "demo", "1.0", url),
            cfg.cache.join("Cask/demo--1.0.zip")
        );
        assert_eq!(
            cached_basename(
                &cfg.cache_downloads()
                    .join(format!("{digest}--Demo-1.0.zip"))
            ),
            "Demo-1.0.zip"
        );
    }
}
