//! Cask entry of the internal packages API (`docs/COMPAT.md` 1.3).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::sym;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CaskEntry {
    #[serde(skip)]
    pub token: String,
    pub homepage: Option<String>,
    pub names: Vec<String>,
    pub desc: Option<String>,
    /// `{":sha256": ".."}`.
    pub ruby_source_checksum: Option<Value>,
    pub ruby_source_path: Option<String>,
    pub tap_string: Option<String>,
    pub version: Option<String>,
    /// `[url]` or `[url, {..}]`.
    pub url_args: Vec<Value>,
    pub url_kwargs: Option<Value>,
    /// Hex digest or `":no_check"`.
    pub sha256: Option<String>,
    /// `[[":app", ["X.app"]], ...]` in declaration order.
    pub raw_artifacts: Vec<Value>,
    pub depends_on_args: Option<Value>,
    pub conflicts_with_args: Option<Value>,
    pub container_args: Option<Value>,
    pub auto_updates: bool,
    pub deprecate_args: Option<Value>,
    pub disable_args: Option<Value>,
    pub caveats_rosetta: bool,
    pub raw_caveats: Option<Value>,
    pub renames: Option<Value>,
    pub languages: Vec<String>,
    pub language_variations: Option<Value>,
}

/// `CaskName` is the display name (`names[0]`).
pub type CaskName = String;

/// One artifact stanza: kind (`app`, `binary`, `zap`, ...) plus its raw arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct Artifact {
    pub kind: String,
    /// Positional arguments (strings, or a hash for `uninstall`/`zap`/`*_steps`).
    pub args: Vec<Value>,
}

impl CaskEntry {
    pub fn tap(&self) -> &str {
        self.tap_string.as_deref().unwrap_or("homebrew/cask")
    }

    pub fn full_token(&self) -> String {
        if self.tap() == "homebrew/cask" {
            self.token.clone()
        } else {
            format!("{}/{}", self.tap(), self.token)
        }
    }

    pub fn display_name(&self) -> &str {
        self.names
            .first()
            .map(String::as_str)
            .unwrap_or(&self.token)
    }

    pub fn url(&self) -> Option<&str> {
        self.url_args.first().and_then(Value::as_str)
    }

    /// `version` is `"latest"` for unversioned casks.
    pub fn is_latest(&self) -> bool {
        self.version.as_deref() == Some("latest")
    }

    pub fn sha256_no_check(&self) -> bool {
        self.sha256.as_deref().is_some_and(|s| sym(s) == "no_check")
    }

    pub fn artifacts(&self) -> Vec<Artifact> {
        self.raw_artifacts
            .iter()
            .filter_map(|a| {
                let arr = a.as_array()?;
                let kind = sym(arr.first()?.as_str()?).to_string();
                Some(Artifact {
                    kind,
                    args: arr[1..].to_vec(),
                })
            })
            .collect()
    }

    pub fn is_deprecated(&self) -> bool {
        self.deprecate_args.is_some()
    }
    pub fn is_disabled(&self) -> bool {
        self.disable_args.is_some()
    }

    /// Formula dependencies from `depends_on formula:`.
    pub fn formula_dependencies(&self) -> Vec<String> {
        self.depends_on_list(":formula")
    }

    /// Cask dependencies from `depends_on cask:`.
    pub fn cask_dependencies(&self) -> Vec<String> {
        self.depends_on_list(":cask")
    }

    fn depends_on_list(&self, key: &str) -> Vec<String> {
        match self.depends_on_args.as_ref().and_then(|d| d.get(key)) {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => vec![],
        }
    }

    /// Raw `depends_on macos:` value (symbol, comparator hash, or array).
    pub fn macos_requirement(&self) -> Option<&Value> {
        self.depends_on_args.as_ref()?.get(":macos")
    }

    pub fn arch_requirement(&self) -> Option<&Value> {
        self.depends_on_args.as_ref()?.get(":arch")
    }

    pub fn caveats_text(&self) -> Option<String> {
        match &self.raw_caveats {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Array(a)) => Some(
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_artifacts_and_deps() {
        let c: CaskEntry = serde_json::from_value(json!({
            "raw_artifacts": [[":app", ["Ghostty.app"]], [":zap", {":trash": ["~/.config/ghostty"]}]],
            "depends_on_args": {":macos": ":ventura", ":formula": ["git"]},
            "sha256": ":no_check", "version": "latest"
        }))
        .unwrap();
        let arts = c.artifacts();
        assert_eq!(arts[0].kind, "app");
        assert_eq!(arts[1].kind, "zap");
        assert_eq!(c.formula_dependencies(), vec!["git"]);
        assert!(c.sha256_no_check());
        assert!(c.is_latest());
    }
}
