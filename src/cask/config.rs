//! Cask target directories (`Cask::Config`): defaults, `HOMEBREW_CASK_OPTS`,
//! command-line overrides, and `.metadata/config.json` persistence.

use std::path::PathBuf;

use crate::config::Config;

#[derive(Debug, Clone)]
pub struct CaskDirs {
    pub appdir: PathBuf,
    pub appimagedir: PathBuf,
    pub keyboard_layoutdir: PathBuf,
    pub colorpickerdir: PathBuf,
    pub prefpanedir: PathBuf,
    pub qlplugindir: PathBuf,
    pub mdimporterdir: PathBuf,
    pub dictionarydir: PathBuf,
    pub fontdir: PathBuf,
    pub servicedir: PathBuf,
    pub input_methoddir: PathBuf,
    pub internet_plugindir: PathBuf,
    pub audio_unit_plugindir: PathBuf,
    pub vst_plugindir: PathBuf,
    pub vst3_plugindir: PathBuf,
    pub screen_saverdir: PathBuf,
    pub binarydir: PathBuf,
    pub manpagedir: PathBuf,
    pub languages: Vec<String>,
}

impl CaskDirs {
    /// Defaults merged with `HOMEBREW_CASK_OPTS` and explicit `--appdir=` style flags.
    pub fn resolve(_cfg: &Config, _explicit_flags: &[String]) -> CaskDirs {
        todo!("cask::config::CaskDirs::resolve")
    }

    /// JSON for `.metadata/config.json` (`{"default": {..}, "env": {..}, "explicit": {..}}`).
    pub fn to_config_json(&self, _cfg: &Config, _explicit_flags: &[String]) -> serde_json::Value {
        todo!("cask::config::CaskDirs::to_config_json")
    }
}
