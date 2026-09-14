//! Pour-time relocation (`docs/COMPAT.md` 4).
//!
//! `relocate_keg` applies, in order: placeholder text replacement in
//! `changed_files` (or a scan of text and libtool files when the tab has no
//! list), Mach-O placeholder rewriting in `linkage_files` (or all Mach-O
//! files) followed by re-signing, build-prefix rewriting in
//! `binary_relocation_files` when the prefix differs from the built prefix,
//! and symlink relativization. Returns what was changed so the receipt can
//! record `relocated_build_prefix`/`relocated_files`.

use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::model::formula::BottleCellar;

use super::BottleTab;

pub const PREFIX_PLACEHOLDER: &str = "@@HOMEBREW_PREFIX@@";
pub const CELLAR_PLACEHOLDER: &str = "@@HOMEBREW_CELLAR@@";
pub const REPOSITORY_PLACEHOLDER: &str = "@@HOMEBREW_REPOSITORY@@";
pub const LIBRARY_PLACEHOLDER: &str = "@@HOMEBREW_LIBRARY@@";
pub const PERL_PLACEHOLDER: &str = "@@HOMEBREW_PERL@@";
pub const JAVA_PLACEHOLDER: &str = "@@HOMEBREW_JAVA@@";

#[derive(Debug, Default)]
pub struct RelocationReport {
    pub text_files_changed: Vec<String>,
    pub macho_files_changed: Vec<String>,
    pub relocated_build_prefix: Option<String>,
    pub relocated_files: Vec<String>,
}

pub struct RelocateArgs<'a> {
    pub keg_path: &'a Path,
    pub cellar_kind: &'a BottleCellar,
    pub tab: &'a BottleTab,
    /// Name of an `openjdk*` runtime dependency, for `@@HOMEBREW_JAVA@@`.
    pub openjdk_dep: Option<&'a str>,
}

pub fn relocate_keg(_cfg: &Config, _args: RelocateArgs<'_>) -> Result<RelocationReport> {
    todo!("bottle::relocate::relocate_keg")
}

/// Replace placeholders in one text buffer; returns whether anything changed.
pub fn replace_placeholders(
    _cfg: &Config,
    _text: &mut Vec<u8>,
    _openjdk_dep: Option<&str>,
) -> bool {
    todo!("bottle::relocate::replace_placeholders")
}
