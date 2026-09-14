//! Cask target directories (`Cask::Config`): defaults, `HOMEBREW_CASK_OPTS`,
//! command-line overrides, and `.metadata/config.json` persistence.
//!
//! Port of `Library/Homebrew/cask/config.rb` plus
//! `extend/os/mac/cask/config.rb`. Precedence is explicit flags, then
//! `HOMEBREW_CASK_OPTS`, then the defaults; every path is `~`-expanded with
//! `cfg.home` exactly as `Pathname#expand_path` does.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use serde_json::{Map, Value};

use crate::config::Config;

/// `Cask::Config::DEFAULT_DIRS`, in Homebrew's declaration order. The order is
/// load-bearing: `config.json` is compared byte-for-byte with Homebrew's.
pub const DEFAULT_DIRS: &[(&str, &str)] = &[
    ("appdir", "/Applications"),
    ("appimagedir", "~/Applications"),
    ("keyboard_layoutdir", "/Library/Keyboard Layouts"),
    ("colorpickerdir", "~/Library/ColorPickers"),
    ("prefpanedir", "~/Library/PreferencePanes"),
    ("qlplugindir", "~/Library/QuickLook"),
    ("mdimporterdir", "~/Library/Spotlight"),
    ("dictionarydir", "~/Library/Dictionaries"),
    ("fontdir", "~/Library/Fonts"),
    ("servicedir", "~/Library/Services"),
    ("input_methoddir", "~/Library/Input Methods"),
    ("internet_plugindir", "~/Library/Internet Plug-Ins"),
    (
        "audio_unit_plugindir",
        "~/Library/Audio/Plug-Ins/Components",
    ),
    ("vst_plugindir", "~/Library/Audio/Plug-Ins/VST"),
    ("vst3_plugindir", "~/Library/Audio/Plug-Ins/VST3"),
    ("screen_saverdir", "~/Library/Screen Savers"),
];

/// A `--flag` / `--no-flag` switch as it appears in `HOMEBREW_CASK_OPTS` or
/// on the command line (`Cask::Config` and `cask_options`). The last
/// occurrence wins, and `None` means the layer said nothing.
pub fn bool_flag(opts: &[String], name: &str) -> Option<bool> {
    let on = format!("--{name}");
    let off = format!("--no-{name}");
    let mut value = None;
    for token in opts {
        let token = token.trim();
        if token == on {
            value = Some(true);
        } else if token == off {
            value = Some(false);
        }
    }
    value
}

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
    /// `$PREFIX/etc/bash_completion.d`.
    pub bash_completion: PathBuf,
    /// `$PREFIX/share/zsh/site-functions`.
    pub zsh_completion: PathBuf,
    /// `$PREFIX/share/fish/vendor_completions.d`.
    pub fish_completion: PathBuf,
}

impl CaskDirs {
    /// Defaults merged with `HOMEBREW_CASK_OPTS` and explicit `--appdir=` style flags.
    pub fn resolve(cfg: &Config, explicit_flags: &[String]) -> CaskDirs {
        let env = parse_dir_flags(&cfg.cask_opts);
        let explicit = parse_dir_flags(explicit_flags);
        Self::from_layers(cfg, &explicit, &env, None)
    }

    /// Build from already-parsed layers; `default_override` replaces the built-in
    /// defaults (used when reading back a `config.json`).
    fn from_layers(
        cfg: &Config,
        explicit: &[(String, Value)],
        env: &[(String, Value)],
        default_override: Option<&[(String, Value)]>,
    ) -> CaskDirs {
        let pick = |key: &str| -> PathBuf {
            for layer in [explicit, env] {
                if let Some(v) = lookup_str(layer, key) {
                    return expand_path(cfg, &v);
                }
            }
            if let Some(defaults) = default_override
                && let Some(v) = lookup_str(defaults, key)
            {
                return expand_path(cfg, &v);
            }
            let fallback = DEFAULT_DIRS
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| *v)
                .unwrap_or("/");
            expand_path(cfg, fallback)
        };

        let mut languages: Vec<String> = Vec::new();
        let mut push_languages = |list: Option<Vec<String>>| {
            for lang in list.unwrap_or_default() {
                if !languages.contains(&lang) {
                    languages.push(lang);
                }
            }
        };
        push_languages(lookup_languages(explicit));
        push_languages(lookup_languages(env));
        match default_override.and_then(lookup_languages) {
            Some(list) => push_languages(Some(list)),
            None => push_languages(Some(system_languages())),
        }

        CaskDirs {
            appdir: pick("appdir"),
            appimagedir: pick("appimagedir"),
            keyboard_layoutdir: pick("keyboard_layoutdir"),
            colorpickerdir: pick("colorpickerdir"),
            prefpanedir: pick("prefpanedir"),
            qlplugindir: pick("qlplugindir"),
            mdimporterdir: pick("mdimporterdir"),
            dictionarydir: pick("dictionarydir"),
            fontdir: pick("fontdir"),
            servicedir: pick("servicedir"),
            input_methoddir: pick("input_methoddir"),
            internet_plugindir: pick("internet_plugindir"),
            audio_unit_plugindir: pick("audio_unit_plugindir"),
            vst_plugindir: pick("vst_plugindir"),
            vst3_plugindir: pick("vst3_plugindir"),
            screen_saverdir: pick("screen_saverdir"),
            binarydir: cfg.prefix.join("bin"),
            manpagedir: cfg.prefix.join("share/man"),
            languages,
            bash_completion: cfg.prefix.join("etc/bash_completion.d"),
            zsh_completion: cfg.prefix.join("share/zsh/site-functions"),
            fish_completion: cfg.prefix.join("share/fish/vendor_completions.d"),
        }
    }

    /// JSON for `.metadata/config.json` (`{"default": {..}, "env": {..}, "explicit": {..}}`).
    pub fn to_config_json(&self, cfg: &Config, explicit_flags: &[String]) -> Value {
        let mut default = Map::new();
        default.insert(
            "languages".into(),
            Value::Array(
                system_languages()
                    .into_iter()
                    .map(Value::String)
                    .collect::<Vec<_>>(),
            ),
        );
        for (key, value) in DEFAULT_DIRS {
            default.insert(
                (*key).to_string(),
                Value::String(expand_path(cfg, value).to_string_lossy().into_owned()),
            );
        }

        let canonicalize = |layer: &[(String, Value)]| -> Value {
            let mut map = Map::new();
            for (key, value) in layer {
                let value = match value {
                    Value::String(s) => {
                        Value::String(expand_path(cfg, s).to_string_lossy().into_owned())
                    }
                    other => other.clone(),
                };
                map.insert(key.clone(), value);
            }
            Value::Object(map)
        };

        let mut root = Map::new();
        root.insert("default".into(), Value::Object(default));
        root.insert("env".into(), canonicalize(&parse_dir_flags(&cfg.cask_opts)));
        root.insert(
            "explicit".into(),
            canonicalize(&parse_dir_flags(explicit_flags)),
        );
        Value::Object(root)
    }

    /// Read back a `.metadata/config.json` written by Homebrew or fastbrew.
    pub fn from_config_json(cfg: &Config, value: &Value) -> CaskDirs {
        let layer = |key: &str| -> Vec<(String, Value)> {
            value
                .get(key)
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        // Legacy hyphenated keys were never honored; drop them
                        // like `Config.reject_legacy_keys` does.
                        .filter(|(k, _)| !k.contains('-'))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let explicit = layer("explicit");
        let env = layer("env");
        let default = layer("default");
        Self::from_layers(cfg, &explicit, &env, Some(&default))
    }

    /// Read `<caskroom>/<token>/.metadata/config.json`, falling back to the
    /// current environment when it is missing or unreadable.
    pub fn read_or_resolve(cfg: &Config, path: &Path, explicit_flags: &[String]) -> CaskDirs {
        match std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        {
            Some(v) => Self::from_config_json(cfg, &v),
            None => Self::resolve(cfg, explicit_flags),
        }
    }

    /// `Cask::Config#merge` as `installer.rb` uses it
    /// (`@cask.config = @cask.default_config.merge(old_config)`): the explicit
    /// flags an installed cask recorded, with this run's flags on top.
    ///
    /// Only the explicit layer carries over; the defaults and
    /// `HOMEBREW_CASK_OPTS` are re-read, exactly as `Config.new(explicit:)`
    /// does. The result is a flag list so the merged layer is what
    /// [`CaskDirs::resolve`] resolves and [`CaskDirs::to_config_json`] records,
    /// which keeps the directories stable across the next reinstall.
    pub fn merged_explicit_flags(config_path: &Path, explicit_flags: &[String]) -> Vec<String> {
        let saved = std::fs::read_to_string(config_path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok());
        let Some(explicit) = saved
            .as_ref()
            .and_then(|v| v.get("explicit"))
            .and_then(Value::as_object)
        else {
            return explicit_flags.to_vec();
        };
        // `Config.from_args` builds the explicit hash in `DEFAULT_DIRS` order.
        let mut flags: Vec<String> = DEFAULT_DIRS
            .iter()
            .filter_map(|(key, _)| {
                let value = explicit.get(*key)?.as_str()?;
                Some(format!("--{}={value}", key.replace('_', "-")))
            })
            .collect();
        if let Some(languages) = explicit.get("languages").and_then(Value::as_array) {
            let list: Vec<&str> = languages.iter().filter_map(Value::as_str).collect();
            if !list.is_empty() {
                flags.push(format!("--language={}", list.join(",")));
            }
        }
        // `parse_dir_flags` lets the last occurrence of a key win, so this run's
        // flags override what the predecessor recorded.
        flags.extend(explicit_flags.iter().cloned());
        flags
    }

    /// Target directory for a moved artifact kind (`Relocated.dirmethod`).
    pub fn dir_for_kind(&self, kind: &str) -> Option<&Path> {
        let path = match kind {
            "app" => &self.appdir,
            "suite" => &self.appdir,
            "app_image" | "appimage" => &self.appimagedir,
            "keyboard_layout" => &self.keyboard_layoutdir,
            "colorpicker" => &self.colorpickerdir,
            "prefpane" => &self.prefpanedir,
            "qlplugin" => &self.qlplugindir,
            "mdimporter" => &self.mdimporterdir,
            "dictionary" => &self.dictionarydir,
            "font" => &self.fontdir,
            "service" => &self.servicedir,
            "input_method" => &self.input_methoddir,
            "internet_plugin" => &self.internet_plugindir,
            "audio_unit_plugin" => &self.audio_unit_plugindir,
            "vst_plugin" => &self.vst_plugindir,
            "vst3_plugin" => &self.vst3_plugindir,
            "screen_saver" => &self.screen_saverdir,
            "binary" | "command_wrapper" => &self.binarydir,
            "manpage" => &self.manpagedir,
            // `artifact` has no default directory: it requires an explicit target.
            _ => return None,
        };
        Some(path.as_path())
    }

    /// Base directory for an install-step `base:` value, when it names a cask dir.
    pub fn base_for(&self, base: &str) -> Option<&Path> {
        let path = match base {
            "appdir" => &self.appdir,
            "appimagedir" => &self.appimagedir,
            "keyboard_layoutdir" => &self.keyboard_layoutdir,
            "colorpickerdir" => &self.colorpickerdir,
            "prefpanedir" => &self.prefpanedir,
            "qlplugindir" => &self.qlplugindir,
            "mdimporterdir" => &self.mdimporterdir,
            "dictionarydir" => &self.dictionarydir,
            "fontdir" => &self.fontdir,
            "servicedir" => &self.servicedir,
            "input_methoddir" => &self.input_methoddir,
            "internet_plugindir" => &self.internet_plugindir,
            "audio_unit_plugindir" => &self.audio_unit_plugindir,
            "vst_plugindir" => &self.vst_plugindir,
            "vst3_plugindir" => &self.vst3_plugindir,
            "screen_saverdir" => &self.screen_saverdir,
            "binarydir" => &self.binarydir,
            "manpagedir" => &self.manpagedir,
            "bash_completion" => &self.bash_completion,
            "zsh_completion" => &self.zsh_completion,
            "fish_completion" => &self.fish_completion,
            _ => return None,
        };
        Some(path.as_path())
    }
}

/// Parse `--appdir=/foo` style arguments into `(config key, value)` pairs.
///
/// Command-line flags are hyphenated (`--input-methoddir`) but config keys use
/// underscores, and `--language=a,b` becomes the `languages` array.
pub fn parse_dir_flags(args: &[String]) -> Vec<(String, Value)> {
    let mut out: Vec<(String, Value)> = Vec::new();
    for arg in args {
        let Some((flag, value)) = arg.split_once('=') else {
            continue;
        };
        let key = flag.trim_start_matches("--").replace('-', "_");
        let (key, value) = if key == "language" {
            (
                "languages".to_string(),
                Value::Array(
                    value
                        .split(',')
                        .map(|s| Value::String(s.to_string()))
                        .collect(),
                ),
            )
        } else {
            (key, Value::String(value.to_string()))
        };
        if let Some(slot) = out.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
        } else {
            out.push((key, value));
        }
    }
    out
}

fn lookup_str(layer: &[(String, Value)], key: &str) -> Option<String> {
    layer
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.as_str())
        .map(str::to_string)
}

fn lookup_languages(layer: &[(String, Value)]) -> Option<Vec<String>> {
    layer
        .iter()
        .find(|(k, _)| k == "languages")
        .and_then(|(_, v)| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
}

/// `Pathname#expand_path`: `~` (and `~/...`) resolve against `HOME`, relative
/// paths against the working directory.
pub fn expand_path(cfg: &Config, value: &str) -> PathBuf {
    if value == "~" {
        return cfg.home.clone();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return cfg.home.join(rest);
    }
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    }
}

/// `OS::Mac.languages`: `defaults read -g AppleLanguages`, falling back to the
/// system-wide global preferences. Cached for the lifetime of the process.
pub fn system_languages() -> Vec<String> {
    static LANGS: OnceLock<Vec<String>> = OnceLock::new();
    LANGS
        .get_or_init(|| {
            if let Ok(forced) = std::env::var("FASTBREW_CASK_LANGUAGES") {
                return forced
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            if cfg!(not(target_os = "macos")) {
                return vec![];
            }
            let read = |args: &[&str]| -> String {
                Command::new("defaults")
                    .args(args)
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                    .unwrap_or_default()
            };
            let mut out = read(&["read", "-g", "AppleLanguages"]);
            if out.trim().is_empty() {
                out = read(&[
                    "read",
                    "/Library/Preferences/.GlobalPreferences",
                    "AppleLanguages",
                ]);
            }
            scan_languages(&out)
        })
        .clone()
}

/// `os_langs.scan(/[^ \n"(),]+/)` on the `defaults read` output.
fn scan_languages(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if matches!(ch, ' ' | '\n' | '"' | '(' | ')' | ',') {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(home: &str, prefix: &str, cask_opts: &[&str]) -> Config {
        let mut cfg = crate::cask::tests_support::config(Path::new("/tmp/fastbrew-cask-test"));
        cfg.home = PathBuf::from(home);
        cfg.prefix = PathBuf::from(prefix);
        cfg.cellar = cfg.prefix.join("Cellar");
        cfg.cask_opts = cask_opts.iter().map(|s| s.to_string()).collect();
        cfg
    }

    #[test]
    fn defaults_expand_home() {
        let cfg = test_config("/Users/x", "/opt/fb", &[]);
        let dirs = CaskDirs::resolve(&cfg, &[]);
        assert_eq!(dirs.appdir, Path::new("/Applications"));
        assert_eq!(dirs.fontdir, Path::new("/Users/x/Library/Fonts"));
        assert_eq!(
            dirs.keyboard_layoutdir,
            Path::new("/Library/Keyboard Layouts")
        );
        assert_eq!(dirs.binarydir, Path::new("/opt/fb/bin"));
        assert_eq!(dirs.manpagedir, Path::new("/opt/fb/share/man"));
        assert_eq!(
            dirs.zsh_completion,
            Path::new("/opt/fb/share/zsh/site-functions")
        );
    }

    #[test]
    fn env_and_explicit_override() {
        let cfg = test_config(
            "/Users/x",
            "/opt/fb",
            &["--appdir=/sandbox/Applications", "--fontdir=~/Fonts"],
        );
        let dirs = CaskDirs::resolve(&cfg, &[]);
        assert_eq!(dirs.appdir, Path::new("/sandbox/Applications"));
        assert_eq!(dirs.fontdir, Path::new("/Users/x/Fonts"));

        let explicit = vec!["--appdir=/explicit".to_string()];
        let dirs = CaskDirs::resolve(&cfg, &explicit);
        assert_eq!(dirs.appdir, Path::new("/explicit"));
        assert_eq!(dirs.fontdir, Path::new("/Users/x/Fonts"));
    }

    #[test]
    fn hyphenated_flags_and_language() {
        let parsed = parse_dir_flags(&[
            "--input-methoddir=/im".to_string(),
            "--language=en,zh-Hant".to_string(),
        ]);
        assert_eq!(parsed[0].0, "input_methoddir");
        assert_eq!(parsed[1].0, "languages");
        assert_eq!(parsed[1].1, serde_json::json!(["en", "zh-Hant"]));
    }

    #[test]
    fn config_json_structure() {
        let cfg = test_config("/Users/x", "/opt/fb", &["--appdir=/sandbox/Applications"]);
        let dirs = CaskDirs::resolve(&cfg, &[]);
        let json = dirs.to_config_json(&cfg, &["--language=en".to_string()]);
        let text = serde_json::to_string(&json).unwrap();
        assert!(text.starts_with(r#"{"default":{"languages":["#));
        let default = json.get("default").unwrap();
        assert_eq!(default.get("appdir").unwrap(), "/Applications");
        assert_eq!(
            default.get("screen_saverdir").unwrap(),
            "/Users/x/Library/Screen Savers"
        );
        assert_eq!(
            json.get("env").unwrap().get("appdir").unwrap(),
            "/sandbox/Applications"
        );
        assert_eq!(
            json.get("explicit").unwrap().get("languages").unwrap(),
            &serde_json::json!(["en"])
        );
        // Key order matches Homebrew's `DEFAULT_DIRS`.
        let keys: Vec<&String> = default.as_object().unwrap().keys().collect();
        assert_eq!(keys[0], "languages");
        assert_eq!(keys[1], "appdir");
        assert_eq!(keys[2], "appimagedir");
        assert_eq!(keys.last().unwrap().as_str(), "screen_saverdir");
    }

    #[test]
    fn round_trips_config_json() {
        let cfg = test_config("/Users/x", "/opt/fb", &["--appdir=/sandbox/Applications"]);
        let dirs = CaskDirs::resolve(&cfg, &[]);
        let json = dirs.to_config_json(&cfg, &[]);
        let read_back = CaskDirs::from_config_json(&cfg, &json);
        assert_eq!(read_back.appdir, Path::new("/sandbox/Applications"));
        assert_eq!(read_back.fontdir, dirs.fontdir);
    }

    #[test]
    fn scans_defaults_output() {
        let text = "(\n    \"en-TW\",\n    \"zh-Hant-TW\"\n)\n";
        assert_eq!(scan_languages(text), vec!["en-TW", "zh-Hant-TW"]);
    }
}
