//! Fast, memory-mapped index over the internal packages payload.
//!
//! Built once per API download at `$CACHE/fastbrew/index/<tag>-<size>-<mtime>.fbi`
//! and loaded with `mmap`. Loading must take under 5 ms; a scan of all
//! formulae under 15 ms. Third-party tap formulae parsed by `rubylite` are
//! merged in at load time from their own per-tap cache.

use std::collections::BTreeMap;

use crate::config::Config;
use crate::error::Result;
use crate::model::{CaskEntry, FormulaEntry};
use crate::platform::BottleTag;

/// Loaded index. Cheap to clone handles are not required; pass `&Index`.
pub struct Index {
    _private: (),
}

#[derive(Debug, Clone, Default)]
pub struct IndexMetadata {
    pub homebrew_version: String,
    pub bottle_tag: String,
    pub generated_at: u64,
    pub formula_tap_git_head: String,
    pub cask_tap_git_head: String,
}

impl Index {
    /// Load (building if needed) the index for `tag` from the cached API file.
    /// Errors if no cached API file exists (callers run `update` first).
    pub fn load(_cfg: &Config, _tag: &BottleTag) -> Result<Index> {
        todo!("api::index::Index::load")
    }

    /// Rebuild from the cached JWS file regardless of staleness.
    pub fn rebuild(_cfg: &Config, _tag: &BottleTag) -> Result<Index> {
        todo!("api::index::Index::rebuild")
    }

    pub fn metadata(&self) -> &IndexMetadata {
        todo!()
    }

    /// Exact lookup by canonical name (no alias resolution; see `resolve`).
    pub fn formula(&self, _name: &str) -> Option<FormulaEntry> {
        todo!()
    }

    pub fn cask(&self, _token: &str) -> Option<CaskEntry> {
        todo!()
    }

    pub fn has_formula(&self, _name: &str) -> bool {
        todo!()
    }

    pub fn has_cask(&self, _token: &str) -> bool {
        todo!()
    }

    pub fn formula_alias(&self, _alias: &str) -> Option<String> {
        todo!()
    }

    pub fn formula_rename(&self, _old: &str) -> Option<String> {
        todo!()
    }

    pub fn cask_rename(&self, _old: &str) -> Option<String> {
        todo!()
    }

    pub fn formula_tap_migration(&self, _name: &str) -> Option<String> {
        todo!()
    }

    pub fn cask_tap_migration(&self, _token: &str) -> Option<String> {
        todo!()
    }

    /// All formula names, sorted.
    pub fn formula_names(&self) -> Vec<String> {
        todo!()
    }

    pub fn cask_tokens(&self) -> Vec<String> {
        todo!()
    }

    /// Alias -> name map.
    pub fn formula_aliases(&self) -> BTreeMap<String, String> {
        todo!()
    }

    /// Iterate every formula (decoded lazily; cheap enough for a full scan).
    pub fn all_formulae(&self) -> Vec<FormulaEntry> {
        todo!()
    }

    pub fn all_casks(&self) -> Vec<CaskEntry> {
        todo!()
    }

    /// Names (and aliases) whose simplified form contains the simplified
    /// query (`Search.simplify_string`: lowercase, keep `[a-z0-9@+]`).
    pub fn search_formula_names(&self, _query: &str) -> Vec<String> {
        todo!()
    }

    pub fn search_cask_tokens(&self, _query: &str) -> Vec<String> {
        todo!()
    }

    /// `(name, desc)` pairs whose name or description matches (case-insensitive substring or regex).
    pub fn search_formula_descriptions(&self, _query: &SearchQuery) -> Vec<(String, String)> {
        todo!()
    }

    pub fn search_cask_descriptions(&self, _query: &SearchQuery) -> Vec<(String, String)> {
        todo!()
    }

    /// Names that provide an executable (from `executables`), for `which-formula`.
    pub fn formulae_providing_executable(&self, _exe: &str) -> Vec<String> {
        todo!()
    }
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
}

/// `Search.simplify_string`: lowercase and strip everything but `[a-z0-9@+]`.
pub fn simplify(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '@' || *c == '+')
        .flat_map(|c| c.to_lowercase())
        .collect()
}
