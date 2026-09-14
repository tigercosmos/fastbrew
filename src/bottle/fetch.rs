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

use std::path::PathBuf;

use crate::config::Config;
use crate::error::Result;

use super::{BottleRef, BottleTab};

#[derive(Debug, Clone)]
pub struct ManifestInfo {
    pub tab: BottleTab,
    pub bottle_size: Option<u64>,
    pub installed_size: Option<u64>,
    pub path: PathBuf,
}

pub fn fetch_manifest(_cfg: &Config, _bottle: &BottleRef, _quiet: bool) -> Result<ManifestInfo> {
    todo!("bottle::fetch::fetch_manifest")
}

/// Returns the path of the verified cached blob.
pub fn fetch_blob(_cfg: &Config, _bottle: &BottleRef, _quiet: bool) -> Result<PathBuf> {
    todo!("bottle::fetch::fetch_blob")
}

/// Cached blob path if present and complete (no checksum verification).
pub fn cached_blob_path(_cfg: &Config, _bottle: &BottleRef) -> Option<PathBuf> {
    todo!("bottle::fetch::cached_blob_path")
}

pub fn fetch_all(
    _cfg: &Config,
    _bottles: &[BottleRef],
    _quiet: bool,
) -> Vec<Result<(ManifestInfo, PathBuf)>> {
    todo!("bottle::fetch::fetch_all")
}
