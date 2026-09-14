//! Metadata for third-party taps, cached per tap.
//!
//! Formulae and casks in a tap have no API entry: their metadata is extracted
//! from the `.rb` files by `rubylite`. Parsing a whole tap on every command
//! would be slow, so the result is cached at
//! `$CACHE/fastbrew/taps/<user>-<repo>.json`, keyed per file by its size and
//! mtime. A command only re-parses the files that changed since the cache was
//! written; an unchanged tap costs one `stat` per file plus one small JSON
//! read.
//!
//! Files `rubylite` cannot handle are cached as failures, with the reason, so
//! the command can raise `Error::NeedsDelegation` without re-parsing them.
//!
//! Official taps (`homebrew/core`, `homebrew/cask`) are never read here: the
//! packages API is their source of truth, exactly as in `Formulary`'s
//! `FromAPILoader`, which runs before any tap loader.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::model::{CaskEntry, FormulaEntry};
use crate::platform::BottleTag;
use crate::tap::{self, Tap};

/// Bump when the cached shape changes; older files are discarded.
const CACHE_VERSION: u32 = 1;

/// Size and mtime of a source file, the cache key for its parsed metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    size: u64,
    mtime_ns: u128,
}

impl Stamp {
    fn of(path: &Path) -> Option<Stamp> {
        let meta = std::fs::metadata(path).ok()?;
        let mtime = meta.modified().ok()?;
        let since = mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        Some(Stamp {
            size: meta.len(),
            mtime_ns: since.as_nanos(),
        })
    }
}

/// One cached formula: either its metadata or why `rubylite` gave up.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FormulaRecord {
    stamp: Stamp,
    /// Path inside the tap, e.g. `Formula/f/foo.rb`.
    relative_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entry: Option<FormulaEntry>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    has_install_method: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CaskRecord {
    stamp: Stamp,
    relative_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entry: Option<CaskEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    bottle_tag: String,
    formulae: BTreeMap<String, FormulaRecord>,
    casks: BTreeMap<String, CaskRecord>,
}

/// A tap formula, resolved.
#[derive(Debug, Clone)]
pub struct TapFormula {
    pub entry: FormulaEntry,
    /// Absolute path of the `.rb` file.
    pub path: PathBuf,
    /// `def install` exists, so a source build is possible via `brew`.
    pub has_install_method: bool,
}

/// Everything one tap provides, with each file's metadata parsed or refused.
#[derive(Debug, Clone)]
pub struct TapMetadata {
    pub tap: Tap,
    root: PathBuf,
    formulae: BTreeMap<String, FormulaRecord>,
    casks: BTreeMap<String, CaskRecord>,
    /// `Aliases/<alias>` -> formula name.
    aliases: BTreeMap<String, String>,
}

impl TapMetadata {
    pub fn formula_names(&self) -> Vec<String> {
        self.formulae.keys().cloned().collect()
    }

    pub fn cask_tokens(&self) -> Vec<String> {
        self.casks.keys().cloned().collect()
    }

    pub fn aliases(&self) -> &BTreeMap<String, String> {
        &self.aliases
    }

    pub fn has_formula(&self, name: &str) -> bool {
        self.formulae.contains_key(name) || self.aliases.contains_key(name)
    }

    pub fn has_cask(&self, token: &str) -> bool {
        self.casks.contains_key(token)
    }

    /// The formula, or the reason it needs the Ruby `brew`. `None` when the
    /// tap does not carry that name at all.
    pub fn formula(&self, name: &str) -> Option<std::result::Result<TapFormula, String>> {
        let canonical = match self.formulae.contains_key(name) {
            true => name,
            false => self.aliases.get(name).map(String::as_str)?,
        };
        let record = self.formulae.get(canonical)?;
        Some(match (&record.entry, &record.error) {
            (Some(entry), _) => Ok(TapFormula {
                entry: entry.clone(),
                path: self.root.join(&record.relative_path),
                has_install_method: record.has_install_method,
            }),
            (None, Some(reason)) => Err(reason.clone()),
            (None, None) => Err(format!("{canonical} has no cached metadata")),
        })
    }

    pub fn cask(&self, token: &str) -> Option<std::result::Result<CaskEntry, String>> {
        let record = self.casks.get(token)?;
        Some(match (&record.entry, &record.error) {
            (Some(entry), _) => Ok(entry.clone()),
            (None, Some(reason)) => Err(reason.clone()),
            (None, None) => Err(format!("{token} has no cached metadata")),
        })
    }

    /// Every formula whose metadata parsed, for `search`, `info` and `deps`.
    pub fn formulae(&self) -> Vec<FormulaEntry> {
        self.formulae
            .values()
            .filter_map(|r| r.entry.clone())
            .collect()
    }

    pub fn casks(&self) -> Vec<CaskEntry> {
        self.casks
            .values()
            .filter_map(|r| r.entry.clone())
            .collect()
    }
}

/// Every installed third-party tap's metadata.
#[derive(Debug, Clone, Default)]
pub struct TapIndex {
    taps: Vec<TapMetadata>,
}

impl TapIndex {
    /// Load (refreshing where files changed) every installed non-official tap.
    ///
    /// Never fails: a tap whose cache cannot be written is still usable, it
    /// just costs a parse next time.
    pub fn load(cfg: &Config, tag: &BottleTag) -> TapIndex {
        let taps: Vec<Tap> = tap::installed_taps(cfg)
            .into_iter()
            .filter(|t| !t.is_core() && !t.is_cask())
            .collect();
        if taps.is_empty() {
            return TapIndex::default();
        }
        let mut taps: Vec<TapMetadata> = taps
            .par_iter()
            .map(|t| load_tap(cfg, t, tag))
            .collect::<Vec<_>>();
        taps.sort_by_key(|m| m.tap.name());
        TapIndex { taps }
    }

    pub fn is_empty(&self) -> bool {
        self.taps.is_empty()
    }

    pub fn taps(&self) -> &[TapMetadata] {
        &self.taps
    }

    pub fn get(&self, name: &str) -> Option<&TapMetadata> {
        self.taps.iter().find(|m| m.tap.name() == name)
    }

    /// Taps providing a formula by this name, in tap-name order
    /// (`FromNameLoader` raises when there is more than one).
    pub fn taps_with_formula(&self, name: &str) -> Vec<&TapMetadata> {
        self.taps.iter().filter(|m| m.has_formula(name)).collect()
    }

    pub fn taps_with_cask(&self, token: &str) -> Vec<&TapMetadata> {
        self.taps.iter().filter(|m| m.has_cask(token)).collect()
    }

    /// Every parsed tap formula, for whole-index scans (`search`, `uses`).
    pub fn all_formulae(&self) -> Vec<FormulaEntry> {
        self.taps.iter().flat_map(TapMetadata::formulae).collect()
    }

    pub fn all_casks(&self) -> Vec<CaskEntry> {
        self.taps.iter().flat_map(TapMetadata::casks).collect()
    }
}

/// `$CACHE/fastbrew/taps/<user>-<repo>.json`.
pub fn cache_path(cfg: &Config, tap: &Tap) -> PathBuf {
    cfg.cache_fastbrew()
        .join("taps")
        .join(format!("{}-{}.json", tap.user.to_lowercase(), tap.repo))
}

fn read_cache(path: &Path, tag: &BottleTag) -> CacheFile {
    let Ok(data) = std::fs::read(path) else {
        return CacheFile::default();
    };
    let Ok(cache) = serde_json::from_slice::<CacheFile>(&data) else {
        return CacheFile::default();
    };
    // A different host tag selects different `bottle do` checksums and
    // `on_macos` branches, so the entries cannot be reused.
    if cache.version != CACHE_VERSION || cache.bottle_tag != tag.to_string() {
        return CacheFile::default();
    }
    cache
}

fn write_cache(path: &Path, cache: &CacheFile) {
    let Ok(data) = serde_json::to_vec(cache) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = crate::keg::atomic_write(path, &data);
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Refresh one tap's cache and return its metadata.
pub fn load_tap(cfg: &Config, tap: &Tap, tag: &BottleTag) -> TapMetadata {
    let root = tap.path(cfg);
    let path = cache_path(cfg, tap);
    let cached = read_cache(&path, tag);
    let tap_name = tap.name();

    let formula_files = tap::formula_files(cfg, tap);
    let cask_files = tap::cask_files(cfg, tap);

    let formulae: BTreeMap<String, FormulaRecord> = formula_files
        .par_iter()
        .filter_map(|(name, file)| {
            let stamp = Stamp::of(file)?;
            if let Some(old) = cached.formulae.get(name)
                && old.stamp == stamp
            {
                return Some((name.clone(), old.clone()));
            }
            let relative_path = relative(&root, file);
            let record = match crate::rubylite::parse_formula_file(file, &tap_name, tag) {
                Ok(parsed) => {
                    let mut entry = parsed.entry;
                    entry.bottle_root_url = parsed.bottle_root_url;
                    entry.ruby_source_path = Some(relative_path.clone());
                    FormulaRecord {
                        stamp,
                        relative_path,
                        entry: Some(entry),
                        has_install_method: parsed.has_install_method,
                        error: None,
                    }
                }
                Err(e) => FormulaRecord {
                    stamp,
                    relative_path,
                    entry: None,
                    has_install_method: false,
                    error: Some(e.to_string()),
                },
            };
            Some((name.clone(), record))
        })
        .collect();

    let casks: BTreeMap<String, CaskRecord> = cask_files
        .par_iter()
        .filter_map(|(token, file)| {
            let stamp = Stamp::of(file)?;
            if let Some(old) = cached.casks.get(token)
                && old.stamp == stamp
            {
                return Some((token.clone(), old.clone()));
            }
            let relative_path = relative(&root, file);
            let record = match crate::rubylite::parse_cask_file_for(file, &tap_name, tag) {
                Ok(mut entry) => {
                    entry.tap_string = Some(tap_name.clone());
                    CaskRecord {
                        stamp,
                        relative_path,
                        entry: Some(entry),
                        error: None,
                    }
                }
                Err(e) => CaskRecord {
                    stamp,
                    relative_path,
                    entry: None,
                    error: Some(e.to_string()),
                },
            };
            Some((token.clone(), record))
        })
        .collect();

    let refreshed = CacheFile {
        version: CACHE_VERSION,
        bottle_tag: tag.to_string(),
        formulae,
        casks,
    };
    if refreshed.formulae != cached.formulae || refreshed.casks != cached.casks {
        write_cache(&path, &refreshed);
    }

    let mut metadata = TapMetadata {
        tap: tap.clone(),
        root,
        formulae: refreshed.formulae,
        casks: refreshed.casks,
        aliases: tap::aliases(cfg, tap),
    };
    // `name` and `tap` are not part of the wire format, so restore them.
    for (name, record) in metadata.formulae.iter_mut() {
        if let Some(entry) = record.entry.as_mut() {
            entry.name = name.clone();
            entry.tap = tap_name.clone();
        }
    }
    for (token, record) in metadata.casks.iter_mut() {
        if let Some(entry) = record.entry.as_mut() {
            entry.token = token.clone();
            entry.tap_string = Some(tap_name.clone());
        }
    }
    metadata
}

/// `FormulaRecord`/`CaskRecord` compare by their cached content so that an
/// unchanged tap never rewrites its cache file.
impl PartialEq for FormulaRecord {
    fn eq(&self, other: &Self) -> bool {
        self.stamp == other.stamp
            && self.relative_path == other.relative_path
            && self.entry == other.entry
            && self.has_install_method == other.has_install_method
            && self.error == other.error
    }
}

impl PartialEq for CaskRecord {
    fn eq(&self, other: &Self) -> bool {
        self.stamp == other.stamp
            && self.relative_path == other.relative_path
            && self.entry == other.entry
            && self.error == other.error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOO: &str = r#"class Foo < Formula
  desc "A tap formula"
  homepage "https://example.com/foo"
  url "https://example.com/foo-1.0.tar.gz"
  sha256 "1111111111111111111111111111111111111111111111111111111111111111"
end
"#;

    fn tag() -> BottleTag {
        BottleTag::parse("arm64_tahoe").unwrap()
    }

    fn make_tap(cfg: &Config) -> Tap {
        let tap = Tap::parse("someone/tools").unwrap();
        let dir = tap.path(cfg).join("Formula");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("foo.rb"), FOO).unwrap();
        tap
    }

    #[test]
    fn caches_parsed_metadata_and_reuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(dir.path());
        let tap = make_tap(&cfg);

        let meta = load_tap(&cfg, &tap, &tag());
        assert_eq!(meta.formula_names(), vec!["foo".to_string()]);
        let f = meta.formula("foo").unwrap().expect("parsed");
        assert_eq!(f.entry.name, "foo");
        assert_eq!(f.entry.tap, "someone/tools");
        assert_eq!(f.entry.full_name(), "someone/tools/foo");
        assert_eq!(f.entry.stable_version.as_deref(), Some("1.0"));
        assert_eq!(f.entry.ruby_source_path.as_deref(), Some("Formula/foo.rb"));

        let cache = cache_path(&cfg, &tap);
        assert!(cache.is_file(), "cache written to {}", cache.display());
        let before = std::fs::metadata(&cache).unwrap().modified().unwrap();

        // A second load reuses the cache byte for byte.
        let again = load_tap(&cfg, &tap, &tag());
        assert_eq!(
            again.formula("foo").unwrap().unwrap().entry,
            f.entry,
            "cached entry round-trips"
        );
        assert_eq!(
            std::fs::metadata(&cache).unwrap().modified().unwrap(),
            before,
            "an unchanged tap must not rewrite its cache"
        );
    }

    #[test]
    fn a_changed_file_is_reparsed() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(dir.path());
        let tap = make_tap(&cfg);
        let _ = load_tap(&cfg, &tap, &tag());

        let file = tap.path(&cfg).join("Formula/foo.rb");
        std::fs::write(&file, FOO.replace("foo-1.0", "foo-2.0")).unwrap();
        // Make the mtime differ even on a coarse clock.
        filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(1_800_000_000, 0))
            .unwrap();

        let meta = load_tap(&cfg, &tap, &tag());
        assert_eq!(
            meta.formula("foo").unwrap().unwrap().entry.stable_version,
            Some("2.0".to_string())
        );
    }

    #[test]
    fn unparseable_files_cache_their_reason() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(dir.path());
        let tap = Tap::parse("someone/tools").unwrap();
        let formula_dir = tap.path(&cfg).join("Formula");
        std::fs::create_dir_all(&formula_dir).unwrap();
        std::fs::write(formula_dir.join("weird.rb"), "# not a formula at all\n").unwrap();

        let meta = load_tap(&cfg, &tap, &tag());
        let reason = meta.formula("weird").unwrap().expect_err("cannot parse");
        assert!(reason.contains("weird.rb"), "{reason}");
        // The failure survives a reload without re-parsing.
        let meta = load_tap(&cfg, &tap, &tag());
        assert!(meta.formula("weird").unwrap().is_err());
        assert!(meta.formula("nope").is_none());
    }

    #[test]
    fn official_taps_are_not_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(dir.path());
        for name in ["homebrew/core", "homebrew/cask"] {
            let t = Tap::parse(name).unwrap();
            std::fs::create_dir_all(t.path(&cfg).join("Formula")).unwrap();
        }
        make_tap(&cfg);
        let index = TapIndex::load(&cfg, &tag());
        let names: Vec<String> = index.taps().iter().map(|m| m.tap.name()).collect();
        assert_eq!(names, vec!["someone/tools".to_string()]);
        assert_eq!(index.taps_with_formula("foo").len(), 1);
        assert!(index.taps_with_formula("jq").is_empty());
    }
}
