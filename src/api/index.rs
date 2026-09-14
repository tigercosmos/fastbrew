//! Fast, memory-mapped index over the internal packages payload.
//!
//! Built once per API download at `$CACHE/fastbrew/index/<tag>-<size>-<mtime_ns>.fbi`
//! and loaded with `mmap`. Loading must take under 5 ms; a scan of all
//! formulae under 15 ms. Third-party tap formulae parsed by `rubylite` are
//! merged in at load time from their own per-tap cache.
//!
//! ## Format
//!
//! A hand-rolled little-endian container: a header with a fingerprint of the
//! source JWS file, a directory of named sections, and the sections
//! themselves. Strings live in concatenated blobs with `u32` offset tables, so
//! every lookup is a binary search over a sorted name table followed by a
//! slice of the mapped bytes. Full `FormulaEntry`/`CaskEntry` values are
//! decoded on demand from the entry's original JSON, which is stored verbatim;
//! the fields needed by the scanning commands (`search`, `uses`, `outdated`,
//! `update`) have their own compact tables so those paths never touch JSON.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{CaskEntry, FormulaEntry};
use crate::platform::BottleTag;

const MAGIC: &[u8; 8] = b"FBIDX\0\0\0";
/// Bump whenever the on-disk layout or the stored fields change.
const FORMAT_VERSION: u32 = 3;
const HEADER_LEN: usize = 48;
const SECTION_NAME_LEN: usize = 16;
const SECTION_RECORD_LEN: usize = SECTION_NAME_LEN + 16;

/// Formula flag bits in the `f_flags` section.
mod fflag {
    pub const BOTTLE: u8 = 1 << 0;
    pub const KEG_ONLY: u8 = 1 << 1;
    pub const DEPRECATED: u8 = 1 << 2;
    pub const DISABLED: u8 = 1 << 3;
}

/// Cask flag bits in the `c_flags` section.
mod cflag {
    pub const AUTO_UPDATES: u8 = 1 << 0;
    pub const LATEST: u8 = 1 << 1;
    pub const DEPRECATED: u8 = 1 << 2;
    pub const DISABLED: u8 = 1 << 3;
}

/// Dependency flag bits in the `f_deps` records.
pub mod depflag {
    pub const BUILD: u8 = 1 << 0;
    pub const TEST: u8 = 1 << 1;
    pub const OPTIONAL: u8 = 1 << 2;
    pub const RECOMMENDED: u8 = 1 << 3;
    pub const IMPLICIT: u8 = 1 << 4;
    /// The entry came from `uses_from_macos` rather than `depends_on`.
    pub const USES_FROM_MACOS: u8 = 1 << 5;
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct IndexMetadata {
    pub homebrew_version: String,
    pub bottle_tag: String,
    pub generated_at: u64,
    pub formula_tap_git_head: String,
    pub cask_tap_git_head: String,
}

/// Loaded index. Cheap to clone handles are not required; pass `&Index`.
pub struct Index {
    map: Mmap,
    sections: BTreeMap<[u8; SECTION_NAME_LEN], (usize, usize)>,
    meta: IndexMetadata,
    n_formulae: usize,
    n_casks: usize,
    path: PathBuf,
}

// ---------------------------------------------------------------------------
// Reading primitives
// ---------------------------------------------------------------------------

fn section_key(name: &str) -> [u8; SECTION_NAME_LEN] {
    let mut key = [0u8; SECTION_NAME_LEN];
    let b = name.as_bytes();
    assert!(b.len() <= SECTION_NAME_LEN, "section name too long: {name}");
    key[..b.len()].copy_from_slice(b);
    key
}

impl Index {
    fn section(&self, name: &str) -> &[u8] {
        match self.sections.get(&section_key(name)) {
            Some(&(off, len)) => &self.map[off..off + len],
            None => &[],
        }
    }

    /// A `u32` offset table (`n + 1` entries) as raw bytes.
    fn offsets(&self, name: &str) -> &[u8] {
        self.section(name)
    }

    fn blob_item(&self, blob: &str, idx_name: &str, i: usize) -> &[u8] {
        let idx = self.offsets(idx_name);
        if (i + 1) * 4 + 4 > idx.len() {
            return &[];
        }
        let start = read_u32(idx, i * 4) as usize;
        let end = read_u32(idx, (i + 1) * 4) as usize;
        let data = self.section(blob);
        if end > data.len() || start > end {
            return &[];
        }
        &data[start..end]
    }

    fn blob_str(&self, blob: &str, idx_name: &str, i: usize) -> &str {
        std::str::from_utf8(self.blob_item(blob, idx_name, i)).unwrap_or("")
    }

    fn blob_len(&self, idx_name: &str) -> usize {
        let idx = self.offsets(idx_name);
        if idx.len() < 8 { 0 } else { idx.len() / 4 - 1 }
    }

    /// Binary search a sorted string table, returning its row index.
    fn find_in_table(&self, blob: &str, idx_name: &str, key: &str) -> Option<usize> {
        let n = self.blob_len(idx_name);
        let mut lo = 0usize;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.blob_str(blob, idx_name, mid).cmp(key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }
}

fn read_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn read_u64(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Fingerprint of the source JWS file: size plus mtime in nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFingerprint {
    pub size: u64,
    pub mtime_ns: u128,
}

impl SourceFingerprint {
    pub fn of(path: &Path) -> Result<Self> {
        let md = std::fs::metadata(path)?;
        let mtime = md.modified()?;
        let mtime_ns = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Ok(SourceFingerprint {
            size: md.len(),
            mtime_ns,
        })
    }
}

impl Index {
    /// Load (building if needed) the index for `tag` from the cached API file.
    /// Errors if no cached API file exists (callers run `update` first).
    pub fn load(cfg: &Config, tag: &BottleTag) -> Result<Index> {
        let source = super::fetch::packages_path(cfg, tag);
        if !source.is_file() {
            return Err(Error::user(format!(
                "No cached Homebrew API data at {}.\nRun `fastbrew update` first.",
                source.display()
            )));
        }
        let fp = SourceFingerprint::of(&source)?;
        let path = index_path(cfg, tag, &fp);
        if path.is_file()
            && let Ok(index) = Self::open(&path, &fp)
        {
            return Ok(index);
        }
        Self::build(cfg, tag, &source, &fp)
    }

    /// Rebuild from the cached JWS file regardless of staleness.
    pub fn rebuild(cfg: &Config, tag: &BottleTag) -> Result<Index> {
        let source = super::fetch::packages_path(cfg, tag);
        if !source.is_file() {
            return Err(Error::user(format!(
                "No cached Homebrew API data at {}.\nRun `fastbrew update` first.",
                source.display()
            )));
        }
        let fp = SourceFingerprint::of(&source)?;
        Self::build(cfg, tag, &source, &fp)
    }

    fn open(path: &Path, expect: &SourceFingerprint) -> Result<Index> {
        let file = std::fs::File::open(path)?;
        // SAFETY: the index file is written atomically and only replaced by a
        // rename, so the mapping stays valid for the life of this `Index`.
        let map = unsafe { Mmap::map(&file)? };
        if map.len() < HEADER_LEN || &map[0..8] != MAGIC {
            return Err(Error::user("stale index"));
        }
        if read_u32(&map, 8) != FORMAT_VERSION {
            return Err(Error::user("index format mismatch"));
        }
        let size = read_u64(&map, 16);
        let mtime_lo = read_u64(&map, 24);
        let mtime_hi = read_u64(&map, 32);
        let mtime_ns = ((mtime_hi as u128) << 64) | mtime_lo as u128;
        if size != expect.size || mtime_ns != expect.mtime_ns {
            return Err(Error::user("stale index"));
        }
        let n_sections = read_u32(&map, 40) as usize;
        let mut sections = BTreeMap::new();
        for i in 0..n_sections {
            let base = HEADER_LEN + i * SECTION_RECORD_LEN;
            if base + SECTION_RECORD_LEN > map.len() {
                return Err(Error::user("truncated index"));
            }
            let mut key = [0u8; SECTION_NAME_LEN];
            key.copy_from_slice(&map[base..base + SECTION_NAME_LEN]);
            let off = read_u64(&map, base + SECTION_NAME_LEN) as usize;
            let len = read_u64(&map, base + SECTION_NAME_LEN + 8) as usize;
            if off + len > map.len() {
                return Err(Error::user("truncated index"));
            }
            sections.insert(key, (off, len));
        }
        let mut index = Index {
            map,
            sections,
            meta: IndexMetadata::default(),
            n_formulae: 0,
            n_casks: 0,
            path: path.to_path_buf(),
        };
        index.meta = serde_json::from_slice(index.section("meta")).unwrap_or_default();
        index.n_formulae = index.blob_len("f_names_idx");
        index.n_casks = index.blob_len("c_tokens_idx");
        Ok(index)
    }

    fn build(
        cfg: &Config,
        tag: &BottleTag,
        source: &Path,
        fp: &SourceFingerprint,
    ) -> Result<Index> {
        let bytes = std::fs::read(source)?;
        let payload = super::jws::verify(&bytes).map_err(|reason| {
            Error::user(format!(
                "Failed to verify integrity ({reason}) of:\n  {}\nPotential MITM attempt detected. Please run `brew update` and try again.",
                source.display()
            ))
        })?;
        let encoded = encode_payload(&payload, fp)?;
        let path = index_path(cfg, tag, fp);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
            prune_old_indexes(dir, tag, &path);
        }
        crate::keg::atomic_write(&path, &encoded)?;
        Self::open(&path, fp)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn metadata(&self) -> &IndexMetadata {
        &self.meta
    }

    pub fn formula_count(&self) -> usize {
        self.n_formulae
    }

    pub fn cask_count(&self) -> usize {
        self.n_casks
    }

    fn formula_index(&self, name: &str) -> Option<usize> {
        self.find_in_table("f_names", "f_names_idx", name)
    }

    fn cask_index(&self, token: &str) -> Option<usize> {
        self.find_in_table("c_tokens", "c_tokens_idx", token)
    }

    /// Exact lookup by canonical name (no alias resolution; see `resolve`).
    pub fn formula(&self, name: &str) -> Option<FormulaEntry> {
        let i = self.formula_index(name)?;
        self.formula_at(i)
    }

    pub fn formula_at(&self, i: usize) -> Option<FormulaEntry> {
        let json = self.blob_item("f_json", "f_json_idx", i);
        let mut entry: FormulaEntry = serde_json::from_slice(json).ok()?;
        entry.name = self.blob_str("f_names", "f_names_idx", i).to_string();
        entry.tap = "homebrew/core".to_string();
        Some(entry)
    }

    pub fn cask(&self, token: &str) -> Option<CaskEntry> {
        let i = self.cask_index(token)?;
        self.cask_at(i)
    }

    pub fn cask_at(&self, i: usize) -> Option<CaskEntry> {
        let json = self.blob_item("c_json", "c_json_idx", i);
        let mut entry: CaskEntry = serde_json::from_slice(json).ok()?;
        entry.token = self.blob_str("c_tokens", "c_tokens_idx", i).to_string();
        Some(entry)
    }

    pub fn has_formula(&self, name: &str) -> bool {
        self.formula_index(name).is_some()
    }

    pub fn has_cask(&self, token: &str) -> bool {
        self.cask_index(token).is_some()
    }

    fn map_lookup(
        &self,
        keys: &str,
        idx: &str,
        values: &str,
        values_idx: &str,
        k: &str,
    ) -> Option<String> {
        let i = self.find_in_table(keys, idx, k)?;
        Some(self.blob_str(values, values_idx, i).to_string())
    }

    pub fn formula_alias(&self, alias: &str) -> Option<String> {
        self.map_lookup("fa_keys", "fa_keys_idx", "fa_vals", "fa_vals_idx", alias)
    }

    pub fn formula_rename(&self, old: &str) -> Option<String> {
        self.map_lookup("fr_keys", "fr_keys_idx", "fr_vals", "fr_vals_idx", old)
    }

    pub fn cask_rename(&self, old: &str) -> Option<String> {
        self.map_lookup("cr_keys", "cr_keys_idx", "cr_vals", "cr_vals_idx", old)
    }

    pub fn formula_tap_migration(&self, name: &str) -> Option<String> {
        self.map_lookup("fm_keys", "fm_keys_idx", "fm_vals", "fm_vals_idx", name)
    }

    pub fn cask_tap_migration(&self, token: &str) -> Option<String> {
        self.map_lookup("cm_keys", "cm_keys_idx", "cm_vals", "cm_vals_idx", token)
    }

    /// `oldnames` of every formula, mapped to the current name.
    pub fn formula_oldname(&self, old: &str) -> Option<String> {
        self.map_lookup("fo_keys", "fo_keys_idx", "fo_vals", "fo_vals_idx", old)
    }

    /// All formula names, sorted.
    pub fn formula_names(&self) -> Vec<String> {
        (0..self.n_formulae)
            .map(|i| self.blob_str("f_names", "f_names_idx", i).to_string())
            .collect()
    }

    pub fn formula_name_at(&self, i: usize) -> &str {
        self.blob_str("f_names", "f_names_idx", i)
    }

    pub fn cask_tokens(&self) -> Vec<String> {
        (0..self.n_casks)
            .map(|i| self.blob_str("c_tokens", "c_tokens_idx", i).to_string())
            .collect()
    }

    pub fn cask_token_at(&self, i: usize) -> &str {
        self.blob_str("c_tokens", "c_tokens_idx", i)
    }

    /// Alias -> name map.
    pub fn formula_aliases(&self) -> BTreeMap<String, String> {
        let n = self.blob_len("fa_keys_idx");
        (0..n)
            .map(|i| {
                (
                    self.blob_str("fa_keys", "fa_keys_idx", i).to_string(),
                    self.blob_str("fa_vals", "fa_vals_idx", i).to_string(),
                )
            })
            .collect()
    }

    pub fn formula_renames(&self) -> BTreeMap<String, String> {
        let n = self.blob_len("fr_keys_idx");
        (0..n)
            .map(|i| {
                (
                    self.blob_str("fr_keys", "fr_keys_idx", i).to_string(),
                    self.blob_str("fr_vals", "fr_vals_idx", i).to_string(),
                )
            })
            .collect()
    }

    pub fn cask_renames(&self) -> BTreeMap<String, String> {
        let n = self.blob_len("cr_keys_idx");
        (0..n)
            .map(|i| {
                (
                    self.blob_str("cr_keys", "cr_keys_idx", i).to_string(),
                    self.blob_str("cr_vals", "cr_vals_idx", i).to_string(),
                )
            })
            .collect()
    }

    /// Iterate every formula (decoded in parallel; a full scan stays in the
    /// low tens of milliseconds).
    pub fn all_formulae(&self) -> Vec<FormulaEntry> {
        (0..self.n_formulae)
            .into_par_iter()
            .filter_map(|i| self.formula_at(i))
            .collect()
    }

    pub fn all_casks(&self) -> Vec<CaskEntry> {
        (0..self.n_casks)
            .into_par_iter()
            .filter_map(|i| self.cask_at(i))
            .collect()
    }

    // -- compact scanning tables --------------------------------------------

    /// `<version>` or `<version>_<revision>` of formula `i`.
    pub fn formula_pkg_version(&self, name: &str) -> Option<String> {
        let i = self.formula_index(name)?;
        Some(self.formula_pkg_version_at(i).to_string())
    }

    pub fn formula_pkg_version_at(&self, i: usize) -> &str {
        self.blob_str("f_pkgver", "f_pkgver_idx", i)
    }

    pub fn formula_version_scheme_at(&self, i: usize) -> u32 {
        let s = self.section("f_scheme");
        if i * 4 + 4 <= s.len() {
            read_u32(s, i * 4)
        } else {
            0
        }
    }

    pub fn formula_version_scheme(&self, name: &str) -> u32 {
        self.formula_index(name)
            .map(|i| self.formula_version_scheme_at(i))
            .unwrap_or(0)
    }

    pub fn formula_desc_at(&self, i: usize) -> &str {
        self.blob_str("f_desc", "f_desc_idx", i)
    }

    pub fn formula_desc(&self, name: &str) -> Option<String> {
        let i = self.formula_index(name)?;
        let d = self.formula_desc_at(i);
        (!d.is_empty()).then(|| d.to_string())
    }

    pub fn cask_desc_at(&self, i: usize) -> &str {
        self.blob_str("c_desc", "c_desc_idx", i)
    }

    pub fn cask_desc(&self, token: &str) -> Option<String> {
        let i = self.cask_index(token)?;
        let d = self.cask_desc_at(i);
        (!d.is_empty()).then(|| d.to_string())
    }

    /// Display names (`names` in the API), tab-separated, of cask `i`.
    pub fn cask_names_at(&self, i: usize) -> Vec<String> {
        let s = self.blob_str("c_names", "c_names_idx", i);
        if s.is_empty() {
            vec![]
        } else {
            s.split('\u{1f}').map(str::to_string).collect()
        }
    }

    pub fn cask_version_at(&self, i: usize) -> &str {
        self.blob_str("c_version", "c_version_idx", i)
    }

    pub fn cask_version(&self, token: &str) -> Option<String> {
        let i = self.cask_index(token)?;
        Some(self.cask_version_at(i).to_string())
    }

    pub fn cask_auto_updates_at(&self, i: usize) -> bool {
        self.cask_flags_at(i) & cflag::AUTO_UPDATES != 0
    }

    pub fn cask_is_latest_at(&self, i: usize) -> bool {
        self.cask_flags_at(i) & cflag::LATEST != 0
    }

    fn cask_flags_at(&self, i: usize) -> u8 {
        self.section("c_flags").get(i).copied().unwrap_or(0)
    }

    fn formula_flags_at(&self, i: usize) -> u8 {
        self.section("f_flags").get(i).copied().unwrap_or(0)
    }

    pub fn formula_has_bottle_at(&self, i: usize) -> bool {
        self.formula_flags_at(i) & fflag::BOTTLE != 0
    }

    pub fn formula_is_keg_only_at(&self, i: usize) -> bool {
        self.formula_flags_at(i) & fflag::KEG_ONLY != 0
    }

    pub fn formula_is_deprecated_at(&self, i: usize) -> bool {
        self.formula_flags_at(i) & fflag::DEPRECATED != 0
    }

    pub fn formula_is_disabled_at(&self, i: usize) -> bool {
        self.formula_flags_at(i) & fflag::DISABLED != 0
    }

    /// Compact dependency records of formula `i`: `(name, flags, since_major)`.
    /// `flags` uses the [`depflag`] bits; `since_major` is the macOS major
    /// version of a `uses_from_macos` `since:` bound (0 when unbounded).
    pub fn formula_deps_at(&self, i: usize) -> Vec<(&str, u8, u8)> {
        decode_dep_records(self.blob_item("f_deps", "f_deps_idx", i))
    }

    pub fn formula_deps(&self, name: &str) -> Vec<(&str, u8, u8)> {
        match self.formula_index(name) {
            Some(i) => self.formula_deps_at(i),
            None => vec![],
        }
    }

    /// Names (and aliases) whose simplified form contains the simplified
    /// query (`Search.simplify_string`: lowercase, keep `[a-z0-9@+]`).
    pub fn search_formula_names(&self, query: &str) -> Vec<String> {
        let q = simplify(query);
        let mut out: Vec<String> = (0..self.n_formulae)
            .filter(|&i| self.blob_str("f_simple", "f_simple_idx", i).contains(&q))
            .map(|i| self.formula_name_at(i).to_string())
            .collect();
        let n_alias = self.blob_len("fa_keys_idx");
        for i in 0..n_alias {
            let alias = self.blob_str("fa_keys", "fa_keys_idx", i);
            if simplify(alias).contains(&q) {
                out.push(alias.to_string());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Formula names matching a regex (`brew search /re/`).
    pub fn search_formula_names_regex(&self, re: &regex::Regex) -> Vec<String> {
        let mut out: Vec<String> = (0..self.n_formulae)
            .filter(|&i| re.is_match(self.formula_name_at(i)))
            .map(|i| self.formula_name_at(i).to_string())
            .collect();
        let n_alias = self.blob_len("fa_keys_idx");
        for i in 0..n_alias {
            let alias = self.blob_str("fa_keys", "fa_keys_idx", i);
            if re.is_match(alias) {
                out.push(alias.to_string());
            }
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn search_cask_tokens(&self, query: &str) -> Vec<String> {
        let q = simplify(query);
        (0..self.n_casks)
            .filter(|&i| self.blob_str("c_simple", "c_simple_idx", i).contains(&q))
            .map(|i| self.cask_token_at(i).to_string())
            .collect()
    }

    pub fn search_cask_tokens_regex(&self, re: &regex::Regex) -> Vec<String> {
        (0..self.n_casks)
            .filter(|&i| re.is_match(self.cask_token_at(i)))
            .map(|i| self.cask_token_at(i).to_string())
            .collect()
    }

    /// `(name, desc)` pairs whose name or description matches (case-insensitive substring or regex).
    pub fn search_formula_descriptions(&self, query: &SearchQuery) -> Vec<(String, String)> {
        (0..self.n_formulae)
            .filter_map(|i| {
                let desc = self.formula_desc_at(i);
                if desc.is_empty() {
                    return None;
                }
                let name = self.formula_name_at(i);
                let lower = self.blob_str("f_desc_lc", "f_desc_lc_idx", i);
                query
                    .matches(name, desc, lower)
                    .then(|| (name.to_string(), desc.to_string()))
            })
            .collect()
    }

    pub fn search_cask_descriptions(&self, query: &SearchQuery) -> Vec<(String, String)> {
        (0..self.n_casks)
            .filter_map(|i| {
                let desc = self.cask_desc_at(i);
                if desc.is_empty() {
                    return None;
                }
                let token = self.cask_token_at(i);
                let lower = self.blob_str("c_desc_lc", "c_desc_lc_idx", i);
                query
                    .matches(token, desc, lower)
                    .then(|| (token.to_string(), desc.to_string()))
            })
            .collect()
    }

    /// Names that provide an executable (from `executables`), for `which-formula`.
    pub fn formulae_providing_executable(&self, exe: &str) -> Vec<String> {
        (0..self.n_formulae)
            .filter(|&i| {
                self.blob_str("f_exec", "f_exec_idx", i)
                    .split('\u{1f}')
                    .any(|e| e == exe)
            })
            .map(|i| self.formula_name_at(i).to_string())
            .collect()
    }

    /// Executables shipped by formula `i`.
    pub fn formula_executables_at(&self, i: usize) -> Vec<&str> {
        let s = self.blob_str("f_exec", "f_exec_idx", i);
        if s.is_empty() {
            vec![]
        } else {
            s.split('\u{1f}').collect()
        }
    }
}

fn decode_dep_records(mut b: &[u8]) -> Vec<(&str, u8, u8)> {
    let mut out = Vec::new();
    while b.len() >= 4 {
        let flags = b[0];
        let since = b[1];
        let len = u16::from_le_bytes([b[2], b[3]]) as usize;
        if b.len() < 4 + len {
            break;
        }
        if let Ok(name) = std::str::from_utf8(&b[4..4 + len]) {
            out.push((name, flags, since));
        }
        b = &b[4 + len..];
    }
    out
}

/// A search query: plain text or `/regex/`.
#[derive(Debug, Clone)]
pub enum SearchQuery {
    Text(String),
    Regex(regex::Regex),
}

impl SearchQuery {
    /// Parse `brew search` syntax: `/re/` is a regex, anything else is text.
    pub fn parse(q: &str) -> Result<SearchQuery> {
        if q.len() >= 2 && q.starts_with('/') && q.ends_with('/') {
            let re = regex::Regex::new(&q[1..q.len() - 1])
                .map_err(|_| crate::error::Error::user(format!("{q} is not a valid regex.")))?;
            Ok(SearchQuery::Regex(re))
        } else {
            Ok(SearchQuery::Text(q.to_string()))
        }
    }

    /// `Descriptions.search`: a regex matches name or description; plain text
    /// matches case-insensitively against either.
    fn matches(&self, name: &str, desc: &str, desc_lower: &str) -> bool {
        match self {
            SearchQuery::Regex(re) => re.is_match(name) || re.is_match(desc),
            SearchQuery::Text(t) => {
                let t = t.to_lowercase();
                desc_lower.contains(&t) || name.to_lowercase().contains(&t)
            }
        }
    }
}

/// `Search.simplify_string`: lowercase and strip everything but `[a-z0-9@+]`.
pub fn simplify(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '@' || *c == '+')
        .flat_map(|c| c.to_lowercase())
        .collect()
}

// ---------------------------------------------------------------------------
// Building
// ---------------------------------------------------------------------------

fn index_dir(cfg: &Config) -> PathBuf {
    cfg.cache_fastbrew().join("index")
}

fn index_path(cfg: &Config, tag: &BottleTag, fp: &SourceFingerprint) -> PathBuf {
    index_dir(cfg).join(format!("{tag}-{}-{}.fbi", fp.size, fp.mtime_ns))
}

fn prune_old_indexes(dir: &Path, tag: &BottleTag, keep: &Path) {
    let prefix = format!("{tag}-");
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path == keep {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(".fbi") {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Deserialize)]
struct Payload<'a> {
    #[serde(default)]
    metadata: PayloadMetadata,
    #[serde(borrow, default)]
    formulae: BTreeMap<String, &'a RawValue>,
    #[serde(borrow, default)]
    casks: BTreeMap<String, &'a RawValue>,
    #[serde(default)]
    formula_aliases: BTreeMap<String, String>,
    #[serde(default)]
    formula_renames: BTreeMap<String, String>,
    #[serde(default)]
    cask_renames: BTreeMap<String, String>,
    #[serde(default)]
    formula_tap_git_head: String,
    #[serde(default)]
    cask_tap_git_head: String,
    #[serde(default)]
    formula_tap_migrations: BTreeMap<String, String>,
    #[serde(default)]
    cask_tap_migrations: BTreeMap<String, String>,
}

#[derive(Deserialize, Default)]
struct PayloadMetadata {
    #[serde(default)]
    homebrew_version: String,
    #[serde(default)]
    bottle_tag: String,
    #[serde(default)]
    generated_at: u64,
}

/// The subset of a formula entry the compact tables need.
#[derive(Deserialize, Default)]
#[serde(default)]
struct LiteFormula {
    desc: Option<String>,
    stable_version: Option<String>,
    revision: u32,
    version_scheme: u32,
    bottle_checksum: Option<String>,
    keg_only_args: Vec<serde_json::Value>,
    deprecate_args: Option<serde_json::Value>,
    disable_args: Option<serde_json::Value>,
    stable_dependencies: Vec<serde_json::Value>,
    stable_uses_from_macos: Vec<serde_json::Value>,
    executables: Vec<String>,
    oldnames: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LiteCask {
    desc: Option<String>,
    version: Option<String>,
    names: Vec<String>,
    auto_updates: bool,
    deprecate_args: Option<serde_json::Value>,
    disable_args: Option<serde_json::Value>,
}

struct FormulaRow {
    name: String,
    json: Vec<u8>,
    simple: String,
    desc: String,
    desc_lc: String,
    pkg_version: String,
    version_scheme: u32,
    flags: u8,
    deps: Vec<u8>,
    exec: String,
    oldnames: Vec<String>,
}

struct CaskRow {
    token: String,
    json: Vec<u8>,
    simple: String,
    desc: String,
    desc_lc: String,
    version: String,
    names: String,
    flags: u8,
}

/// Build the on-disk index bytes from a verified payload string.
pub fn encode_payload(payload: &str, fp: &SourceFingerprint) -> Result<Vec<u8>> {
    let parsed: Payload = serde_json::from_str(payload).map_err(|e| {
        Error::user(format!(
            "Cannot parse the Homebrew API payload: {e}. Run `fastbrew update` and try again."
        ))
    })?;

    let formula_rows: Vec<FormulaRow> = parsed
        .formulae
        .par_iter()
        .map(|(name, raw)| build_formula_row(name, raw))
        .collect();
    let cask_rows: Vec<CaskRow> = parsed
        .casks
        .par_iter()
        .map(|(token, raw)| build_cask_row(token, raw))
        .collect();

    let mut writer = Writer::default();

    let meta = IndexMetadata {
        homebrew_version: parsed.metadata.homebrew_version.clone(),
        bottle_tag: parsed.metadata.bottle_tag.clone(),
        generated_at: parsed.metadata.generated_at,
        formula_tap_git_head: parsed.formula_tap_git_head.clone(),
        cask_tap_git_head: parsed.cask_tap_git_head.clone(),
    };
    writer.raw("meta", serde_json::to_vec(&meta).unwrap_or_default());

    writer.strings("f_names", formula_rows.iter().map(|r| r.name.as_str()));
    writer.blobs("f_json", formula_rows.iter().map(|r| r.json.as_slice()));
    writer.strings("f_simple", formula_rows.iter().map(|r| r.simple.as_str()));
    writer.strings("f_desc", formula_rows.iter().map(|r| r.desc.as_str()));
    writer.strings("f_desc_lc", formula_rows.iter().map(|r| r.desc_lc.as_str()));
    writer.strings(
        "f_pkgver",
        formula_rows.iter().map(|r| r.pkg_version.as_str()),
    );
    writer.raw(
        "f_scheme",
        formula_rows
            .iter()
            .flat_map(|r| r.version_scheme.to_le_bytes())
            .collect(),
    );
    writer.raw("f_flags", formula_rows.iter().map(|r| r.flags).collect());
    writer.blobs("f_deps", formula_rows.iter().map(|r| r.deps.as_slice()));
    writer.strings("f_exec", formula_rows.iter().map(|r| r.exec.as_str()));

    writer.strings("c_tokens", cask_rows.iter().map(|r| r.token.as_str()));
    writer.blobs("c_json", cask_rows.iter().map(|r| r.json.as_slice()));
    writer.strings("c_simple", cask_rows.iter().map(|r| r.simple.as_str()));
    writer.strings("c_desc", cask_rows.iter().map(|r| r.desc.as_str()));
    writer.strings("c_desc_lc", cask_rows.iter().map(|r| r.desc_lc.as_str()));
    writer.strings("c_version", cask_rows.iter().map(|r| r.version.as_str()));
    writer.strings("c_names", cask_rows.iter().map(|r| r.names.as_str()));
    writer.raw("c_flags", cask_rows.iter().map(|r| r.flags).collect());

    writer.map("fa", &parsed.formula_aliases);
    writer.map("fr", &parsed.formula_renames);
    writer.map("cr", &parsed.cask_renames);
    writer.map("fm", &parsed.formula_tap_migrations);
    writer.map("cm", &parsed.cask_tap_migrations);

    let mut oldnames: BTreeMap<String, String> = BTreeMap::new();
    for row in &formula_rows {
        for old in &row.oldnames {
            oldnames.insert(old.clone(), row.name.clone());
        }
    }
    writer.map("fo", &oldnames);

    Ok(writer.finish(fp))
}

fn build_formula_row(name: &str, raw: &RawValue) -> FormulaRow {
    let lite: LiteFormula = serde_json::from_str(raw.get()).unwrap_or_default();
    let desc = lite.desc.clone().unwrap_or_default();
    let version = lite.stable_version.clone().unwrap_or_default();
    let pkg_version = if lite.revision > 0 {
        format!("{version}_{}", lite.revision)
    } else {
        version
    };
    let mut flags = 0u8;
    if lite.bottle_checksum.is_some() {
        flags |= fflag::BOTTLE;
    }
    if !lite.keg_only_args.is_empty() {
        flags |= fflag::KEG_ONLY;
    }
    if lite.deprecate_args.is_some() {
        flags |= fflag::DEPRECATED;
    }
    if lite.disable_args.is_some() {
        flags |= fflag::DISABLED;
    }
    let mut deps = Vec::new();
    for v in &lite.stable_dependencies {
        if let Some(dep) = crate::model::formula::parse_dependency(v) {
            push_dep_record(&mut deps, &dep.name, dep_flags(&dep), 0);
        }
    }
    for v in &lite.stable_uses_from_macos {
        if let Some(ufm) = crate::model::formula::parse_uses_from_macos(v) {
            let since = ufm
                .since
                .as_deref()
                .and_then(crate::platform::MacOsVersion::major_for_symbol)
                .unwrap_or(0) as u8;
            push_dep_record(
                &mut deps,
                &ufm.dep.name,
                dep_flags(&ufm.dep) | depflag::USES_FROM_MACOS,
                since,
            );
        }
    }
    FormulaRow {
        name: name.to_string(),
        json: raw.get().as_bytes().to_vec(),
        simple: simplify(name),
        desc_lc: desc.to_lowercase(),
        desc,
        pkg_version,
        version_scheme: lite.version_scheme,
        flags,
        deps,
        exec: lite.executables.join("\u{1f}"),
        oldnames: lite.oldnames,
    }
}

fn dep_flags(dep: &crate::model::Dependency) -> u8 {
    use crate::model::DependencyTag as T;
    let mut f = 0u8;
    for t in &dep.tags {
        f |= match t {
            T::Build => depflag::BUILD,
            T::Test => depflag::TEST,
            T::Optional => depflag::OPTIONAL,
            T::Recommended => depflag::RECOMMENDED,
            T::Implicit => depflag::IMPLICIT,
        };
    }
    f
}

fn push_dep_record(out: &mut Vec<u8>, name: &str, flags: u8, since: u8) {
    let b = name.as_bytes();
    let len = b.len().min(u16::MAX as usize);
    out.push(flags);
    out.push(since);
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&b[..len]);
}

fn build_cask_row(token: &str, raw: &RawValue) -> CaskRow {
    let lite: LiteCask = serde_json::from_str(raw.get()).unwrap_or_default();
    let desc = lite.desc.clone().unwrap_or_default();
    let version = lite.version.clone().unwrap_or_default();
    let mut flags = 0u8;
    if lite.auto_updates {
        flags |= cflag::AUTO_UPDATES;
    }
    if version == "latest" {
        flags |= cflag::LATEST;
    }
    if lite.deprecate_args.is_some() {
        flags |= cflag::DEPRECATED;
    }
    if lite.disable_args.is_some() {
        flags |= cflag::DISABLED;
    }
    CaskRow {
        token: token.to_string(),
        json: raw.get().as_bytes().to_vec(),
        simple: simplify(token),
        desc_lc: desc.to_lowercase(),
        desc,
        version,
        names: lite.names.join("\u{1f}"),
        flags,
    }
}

#[derive(Default)]
struct Writer {
    sections: Vec<([u8; SECTION_NAME_LEN], Vec<u8>)>,
}

impl Writer {
    fn raw(&mut self, name: &str, data: Vec<u8>) {
        self.sections.push((section_key(name), data));
    }

    fn blobs<'a, I: Iterator<Item = &'a [u8]>>(&mut self, name: &str, items: I) {
        let mut blob = Vec::new();
        let mut idx: Vec<u8> = 0u32.to_le_bytes().to_vec();
        for item in items {
            blob.extend_from_slice(item);
            idx.extend_from_slice(&(blob.len() as u32).to_le_bytes());
        }
        self.raw(name, blob);
        self.raw(&format!("{name}_idx"), idx);
    }

    fn strings<'a, I: Iterator<Item = &'a str>>(&mut self, name: &str, items: I) {
        self.blobs(name, items.map(|s| s.as_bytes()));
    }

    fn map(&mut self, prefix: &str, m: &BTreeMap<String, String>) {
        self.strings(&format!("{prefix}_keys"), m.keys().map(String::as_str));
        self.strings(&format!("{prefix}_vals"), m.values().map(String::as_str));
    }

    fn finish(self, fp: &SourceFingerprint) -> Vec<u8> {
        let n = self.sections.len();
        let dir_len = n * SECTION_RECORD_LEN;
        let mut out = Vec::with_capacity(HEADER_LEN + dir_len + 24 * 1024 * 1024);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&fp.size.to_le_bytes());
        out.extend_from_slice(&(fp.mtime_ns as u64).to_le_bytes());
        out.extend_from_slice(&((fp.mtime_ns >> 64) as u64).to_le_bytes());
        out.extend_from_slice(&(n as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        debug_assert_eq!(out.len(), HEADER_LEN);

        let mut offset = HEADER_LEN + dir_len;
        // Keep every section 8-byte aligned so `u32`/`u64` reads stay cheap.
        let mut aligned: Vec<(usize, usize)> = Vec::with_capacity(n);
        for (_, data) in &self.sections {
            let pad = (8 - (offset % 8)) % 8;
            offset += pad;
            aligned.push((offset, data.len()));
            offset += data.len();
        }
        for (i, (key, _)) in self.sections.iter().enumerate() {
            out.extend_from_slice(key);
            out.extend_from_slice(&(aligned[i].0 as u64).to_le_bytes());
            out.extend_from_slice(&(aligned[i].1 as u64).to_le_bytes());
        }
        for (i, (_, data)) in self.sections.iter().enumerate() {
            while out.len() < aligned[i].0 {
                out.push(0);
            }
            out.extend_from_slice(data);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn sample_payload() -> String {
        serde_json::json!({
            "metadata": {"homebrew_version": "6.0.22", "bottle_tag": "arm64_tahoe", "generated_at": 1},
            "formulae": {
                "hello": {
                    "desc": "Program providing model for GNU coding standards",
                    "homepage": "https://www.gnu.org/software/hello/",
                    "license": "GPL-3.0-or-later",
                    "stable_version": "2.12.3",
                    "bottle_checksum": "abc",
                    "executables": ["hello"]
                },
                "jq": {
                    "desc": "Lightweight and flexible command-line JSON processor",
                    "stable_version": "1.8.2",
                    "bottle_rebuild": 1,
                    "bottle_checksum": "def",
                    "stable_dependencies": ["oniguruma", {"pkgconf": ":build"}],
                    "stable_uses_from_macos": [["bzip2"], ["expat", {":since": ":sequoia"}]],
                    "executables": ["jq"],
                    "oldnames": ["jaq-old"]
                },
                "oniguruma": {"desc": "Regular expressions library", "stable_version": "6.9.10", "revision": 1}
            },
            "casks": {
                "ghostty": {
                    "desc": "Terminal emulator",
                    "names": ["Ghostty"],
                    "version": "1.3.1",
                    "auto_updates": true
                },
                "firefox": {"desc": "Web browser", "names": ["Mozilla Firefox"], "version": "latest"}
            },
            "formula_aliases": {"jaq": "jq"},
            "formula_renames": {"oldjq": "jq"},
            "cask_renames": {"oldfox": "firefox"},
            "formula_tap_git_head": "deadbeef",
            "cask_tap_git_head": "cafebabe",
            "formula_tap_migrations": {"android-ndk": "homebrew/cask"},
            "cask_tap_migrations": {"azure-cli": "homebrew/core"}
        })
        .to_string()
    }

    fn build_sample() -> (tempfile::TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        let fp = SourceFingerprint {
            size: 1234,
            mtime_ns: 5_678_000_000_000,
        };
        let bytes = encode_payload(&sample_payload(), &fp).unwrap();
        let path = dir.path().join("sample.fbi");
        std::fs::write(&path, &bytes).unwrap();
        let index = Index::open(&path, &fp).unwrap();
        (dir, index)
    }

    #[test]
    fn round_trips_entries() {
        let (_d, index) = build_sample();
        assert_eq!(index.formula_count(), 3);
        assert_eq!(index.cask_count(), 2);
        assert_eq!(index.metadata().bottle_tag, "arm64_tahoe");
        assert_eq!(index.metadata().formula_tap_git_head, "deadbeef");

        let jq = index.formula("jq").unwrap();
        assert_eq!(jq.name, "jq");
        assert_eq!(jq.tap, "homebrew/core");
        assert_eq!(jq.stable_version.as_deref(), Some("1.8.2"));
        assert_eq!(jq.bottle_rebuild, 1);
        assert_eq!(jq.dependencies().len(), 2);
        assert!(index.has_formula("hello"));
        assert!(!index.has_formula("nope"));

        assert_eq!(
            index.formula_pkg_version("oniguruma").as_deref(),
            Some("6.9.10_1")
        );
        assert_eq!(index.formula_alias("jaq").as_deref(), Some("jq"));
        assert_eq!(index.formula_rename("oldjq").as_deref(), Some("jq"));
        assert_eq!(index.cask_rename("oldfox").as_deref(), Some("firefox"));
        assert_eq!(
            index.formula_tap_migration("android-ndk").as_deref(),
            Some("homebrew/cask")
        );
        assert_eq!(
            index.cask_tap_migration("azure-cli").as_deref(),
            Some("homebrew/core")
        );
        assert_eq!(index.formula_oldname("jaq-old").as_deref(), Some("jq"));

        let ghostty = index.cask("ghostty").unwrap();
        assert_eq!(ghostty.token, "ghostty");
        assert_eq!(ghostty.version.as_deref(), Some("1.3.1"));
        assert!(index.cask_auto_updates_at(index.cask_index("ghostty").unwrap()));
        assert!(index.cask_is_latest_at(index.cask_index("firefox").unwrap()));
        assert_eq!(
            index.cask_names_at(index.cask_index("ghostty").unwrap()),
            vec!["Ghostty".to_string()]
        );
    }

    #[test]
    fn dependency_records() {
        let (_d, index) = build_sample();
        let deps = index.formula_deps("jq");
        assert_eq!(deps[0].0, "oniguruma");
        assert_eq!(deps[0].1, 0);
        assert_eq!(deps[1].0, "pkgconf");
        assert_eq!(deps[1].1 & depflag::BUILD, depflag::BUILD);
        assert_eq!(deps[2].0, "bzip2");
        assert_eq!(
            deps[2].1 & depflag::USES_FROM_MACOS,
            depflag::USES_FROM_MACOS
        );
        assert_eq!(deps[3].0, "expat");
        assert_eq!(deps[3].2, 15); // sequoia
    }

    #[test]
    fn searching() {
        let (_d, index) = build_sample();
        assert_eq!(index.search_formula_names("jq"), vec!["jq"]);
        // Aliases participate in name search (`simplify("jaq")` contains "aq").
        assert_eq!(index.search_formula_names("aq"), vec!["jaq"]);
        assert_eq!(index.search_cask_tokens("fox"), vec!["firefox"]);
        let q = SearchQuery::parse("json").unwrap();
        let hits = index.search_formula_descriptions(&q);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "jq");
        let re = SearchQuery::parse("/^Web/").unwrap();
        assert_eq!(index.search_cask_descriptions(&re).len(), 1);
        assert_eq!(index.formulae_providing_executable("jq"), vec!["jq"]);
        assert!(index.formulae_providing_executable("nope").is_empty());
    }

    #[test]
    fn scans_everything() {
        let (_d, index) = build_sample();
        let all = index.all_formulae();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].name, "hello");
        assert_eq!(index.all_casks().len(), 2);
        assert_eq!(index.formula_names(), vec!["hello", "jq", "oniguruma"]);
    }

    #[test]
    fn detects_stale_fingerprint() {
        let (dir, _index) = build_sample();
        let path = dir.path().join("sample.fbi");
        let other = SourceFingerprint {
            size: 999,
            mtime_ns: 1,
        };
        assert!(Index::open(&path, &other).is_err());
    }

    /// Prints load and scan timings for the real cached API file when running
    /// inside the sandbox (`scripts/sandbox.sh run -- cargo test -- --nocapture`).
    #[test]
    fn real_index_is_fast() {
        let Some(source) = crate::api::fetch::sandbox_packages_file_for_tests() else {
            eprintln!("no cached packages file; skipping index benchmark");
            return;
        };
        let cfg = crate::config::Config::from_env().expect("sandbox config");
        let tag = crate::platform::Host::detect().bottle_tag();

        let t0 = Instant::now();
        let index = Index::rebuild(&cfg, &tag).expect("build index");
        let build_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let t1 = Instant::now();
        let index = Index::load(&cfg, &tag).expect("load index");
        let load_ms = t1.elapsed().as_secs_f64() * 1000.0;

        let t2 = Instant::now();
        let all = index.all_formulae();
        let scan_ms = t2.elapsed().as_secs_f64() * 1000.0;

        let t3 = Instant::now();
        let names = index.formula_names();
        let names_ms = t3.elapsed().as_secs_f64() * 1000.0;

        let t4 = Instant::now();
        for _ in 0..1000 {
            std::hint::black_box(index.formula("jq"));
        }
        let lookup_us = t4.elapsed().as_secs_f64() * 1000.0;

        println!(
            "index({}): build {build_ms:.1} ms, load {load_ms:.2} ms, scan {} formulae {scan_ms:.1} ms, \
             names {names_ms:.2} ms, 1000 lookups {lookup_us:.2} ms",
            source.display(),
            all.len()
        );
        assert!(all.len() > 1000, "expected a full API payload");
        assert_eq!(names.len(), all.len());
        // Generous bounds: debug builds are several times slower than release.
        assert!(load_ms < 50.0, "index load took {load_ms} ms");
        assert!(scan_ms < 2000.0, "full scan took {scan_ms} ms");
    }
}
