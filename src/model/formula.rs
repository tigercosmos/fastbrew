//! Formula entry of the internal packages API (`docs/COMPAT.md` 1.3).
//!
//! Deserializes directly from the payload's `formulae[name]` object. Fields
//! are kept close to the wire format; interpretation helpers live below.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::sym;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FormulaEntry {
    /// Filled in by the loader, not present in the wire object.
    #[serde(skip)]
    pub name: String,
    /// `homebrew/core` for API formulae; `user/repo` for tap formulae.
    #[serde(skip)]
    pub tap: String,

    pub desc: Option<String>,
    pub homepage: Option<String>,
    /// String or nested object (`{":any_of": [...]}`); render with `license_string`.
    pub license: Option<Value>,
    pub ruby_source_checksum: Option<String>,
    pub stable_version: Option<String>,
    /// `[url]` or `[url, {":tag": .., ":revision": .., ":using": ..}]`.
    pub stable_url_args: Vec<Value>,
    pub stable_checksum: Option<String>,
    pub head_url_args: Vec<Value>,
    pub revision: u32,
    pub version_scheme: u32,
    pub bottle_checksum: Option<String>,
    /// `":any"`, `"/opt/homebrew/Cellar"`, or `None` meaning `:any_skip_relocation`.
    pub bottle_cellar: Option<String>,
    /// `":all"` or another tag when the bottle is served under a different tag.
    pub bottle_tag: Option<String>,
    pub bottle_rebuild: u32,
    pub stable_dependencies: Vec<Value>,
    pub head_dependencies: Vec<Value>,
    pub stable_uses_from_macos: Vec<Value>,
    pub head_uses_from_macos: Vec<Value>,
    pub executables: Vec<String>,
    pub stable_patches: Vec<Value>,
    pub caveats: Option<String>,
    /// `[[name, {":because": reason}]]`.
    pub conflicts: Vec<Value>,
    pub deprecate_args: Option<Value>,
    pub disable_args: Option<Value>,
    pub service_args: Vec<Value>,
    pub service_run_args: Vec<Value>,
    pub service_run_kwargs: Option<Value>,
    /// The payload stores this as a bare hash (`{":macos": "label"}`), not an
    /// array; `one_or_many` accepts both and always yields a list.
    #[serde(deserialize_with = "one_or_many")]
    pub service_name_args: Vec<Value>,
    pub keg_only_args: Vec<Value>,
    pub aliases: Vec<String>,
    pub versioned_formulae: Vec<String>,
    pub oldnames: Vec<String>,
    pub post_install_steps: Vec<Value>,
    pub no_autobump_args: Option<Value>,
    pub link_overwrite_paths: Vec<String>,
    pub pour_bottle_args: Option<Value>,
    /// Registry root for the bottle (`bottle do root_url`), set for third-party
    /// tap formulae; `None` means `HOMEBREW_BOTTLE_DOMAIN`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bottle_root_url: Option<String>,
    /// Path of the formula file inside its tap (`Formula/f/foo.rb`), set for
    /// third-party tap formulae. Core formulae compute it from the name with
    /// [`FormulaEntry::core_ruby_path`]; the payload does not carry it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ruby_source_path: Option<String>,
}

/// How a dependency is tagged in `depends_on`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DependencyTag {
    Build,
    Test,
    Optional,
    Recommended,
    Implicit,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Dependency {
    pub name: String,
    pub tags: Vec<DependencyTag>,
}

impl Dependency {
    pub fn is_build(&self) -> bool {
        self.tags.contains(&DependencyTag::Build)
    }
    pub fn is_test(&self) -> bool {
        self.tags.contains(&DependencyTag::Test)
    }
    pub fn is_optional(&self) -> bool {
        self.tags.contains(&DependencyTag::Optional)
    }
    pub fn is_recommended(&self) -> bool {
        self.tags.contains(&DependencyTag::Recommended)
    }
    /// Needed at runtime: neither build-only nor test-only.
    pub fn is_runtime(&self) -> bool {
        let build_or_test = self
            .tags
            .iter()
            .all(|t| matches!(t, DependencyTag::Build | DependencyTag::Test));
        self.tags.is_empty() || !build_or_test
    }
}

/// `uses_from_macos "name", since: :sequoia` and friends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsesFromMacos {
    pub dep: Dependency,
    /// macOS release symbol (`sequoia`) from which the OS provides the software.
    pub since: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KegOnly {
    VersionedFormula,
    ProvidedByMacos,
    ShadowedByMacos,
    Reason(String),
}

impl KegOnly {
    /// Wording from `keg_only_reason.rb`.
    pub fn explanation(&self, extra: Option<&str>) -> String {
        if let Some(e) = extra.filter(|e| !e.is_empty()) {
            return e.to_string();
        }
        match self {
            KegOnly::VersionedFormula => "this is an alternate version of another formula".into(),
            KegOnly::ProvidedByMacos => "macOS already provides this software and installing another version in\nparallel can cause all kinds of trouble".into(),
            KegOnly::ShadowedByMacos => "macOS provides similar software and installing this software in\nparallel can cause all kinds of trouble".into(),
            KegOnly::Reason(r) => r.clone(),
        }
    }
}

impl FormulaEntry {
    pub fn full_name(&self) -> String {
        if self.tap.is_empty() || self.tap == "homebrew/core" {
            self.name.clone()
        } else {
            format!("{}/{}", self.tap, self.name)
        }
    }

    /// `<version>` or `<version>_<revision>`.
    pub fn pkg_version(&self) -> String {
        let v = self.stable_version.clone().unwrap_or_default();
        if self.revision > 0 {
            format!("{v}_{}", self.revision)
        } else {
            v
        }
    }

    pub fn has_bottle(&self) -> bool {
        self.bottle_checksum.is_some()
    }

    /// Cellar kind for relocation decisions.
    pub fn bottle_cellar_kind(&self) -> BottleCellar {
        match self.bottle_cellar.as_deref() {
            None => BottleCellar::AnySkipRelocation,
            Some(":any") | Some("any") => BottleCellar::Any,
            Some(p) => BottleCellar::Fixed(p.to_string()),
        }
    }

    pub fn stable_url(&self) -> Option<&str> {
        self.stable_url_args.first().and_then(Value::as_str)
    }

    pub fn dependencies(&self) -> Vec<Dependency> {
        self.stable_dependencies
            .iter()
            .filter_map(parse_dependency)
            .collect()
    }

    pub fn head_dependencies(&self) -> Vec<Dependency> {
        self.head_dependencies
            .iter()
            .filter_map(parse_dependency)
            .collect()
    }

    pub fn uses_from_macos(&self) -> Vec<UsesFromMacos> {
        self.stable_uses_from_macos
            .iter()
            .filter_map(parse_uses_from_macos)
            .collect()
    }

    pub fn keg_only(&self) -> Option<(KegOnly, Option<String>)> {
        let first = self.keg_only_args.first()?.as_str()?;
        let extra = self
            .keg_only_args
            .get(1)
            .and_then(Value::as_str)
            .map(str::to_string);
        let reason = match first {
            ":versioned_formula" => KegOnly::VersionedFormula,
            ":provided_by_macos" => KegOnly::ProvidedByMacos,
            ":shadowed_by_macos" => KegOnly::ShadowedByMacos,
            other => KegOnly::Reason(other.to_string()),
        };
        Some((reason, extra))
    }

    pub fn is_keg_only(&self) -> bool {
        !self.keg_only_args.is_empty()
    }

    /// `(name, reason)` pairs from `conflicts_with`.
    pub fn conflicts_with(&self) -> Vec<(String, Option<String>)> {
        self.conflicts
            .iter()
            .filter_map(|c| {
                let arr = c.as_array()?;
                let name = arr.first()?.as_str()?.to_string();
                let reason = arr
                    .get(1)
                    .and_then(|v| v.get(":because"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Some((name, reason))
            })
            .collect()
    }

    pub fn is_deprecated(&self) -> bool {
        self.deprecate_args.is_some()
    }
    pub fn is_disabled(&self) -> bool {
        self.disable_args.is_some()
    }

    /// `deprecate!`/`disable!` message body: `deprecated because it <reason>!` style text is built by callers.
    pub fn deprecation(&self) -> Option<DeprecateDisable> {
        parse_deprecate(self.deprecate_args.as_ref()?)
    }

    pub fn disablement(&self) -> Option<DeprecateDisable> {
        parse_deprecate(self.disable_args.as_ref()?)
    }

    pub fn has_service(&self) -> bool {
        !self.service_run_args.is_empty() || !self.service_args.is_empty()
    }

    /// `pour_bottle only_if:` symbol (`default_prefix` / `clt_installed`).
    pub fn pour_bottle_only_if(&self) -> Option<String> {
        self.pour_bottle_args
            .as_ref()?
            .get(":only_if")
            .and_then(Value::as_str)
            .map(|s| sym(s).to_string())
    }

    /// Human-readable license (SPDX expression), approximating `SPDX.license_expression_to_string`.
    pub fn license_string(&self) -> Option<String> {
        self.license.as_ref().map(license_to_string)
    }

    /// Path of the formula file inside its tap: the parsed path for tap
    /// formulae, the sharded homebrew-core path otherwise.
    pub fn ruby_path(&self) -> String {
        match &self.ruby_source_path {
            Some(p) => p.clone(),
            None => self.core_ruby_path(),
        }
    }

    /// Path of the formula file inside homebrew-core (`Formula/h/hello.rb`, `Formula/lib/libfoo.rb`).
    pub fn core_ruby_path(&self) -> String {
        let shard = if self.name.starts_with("lib") {
            "lib".to_string()
        } else {
            self.name
                .chars()
                .next()
                .map(|c| c.to_string())
                .unwrap_or_default()
        };
        format!("Formula/{shard}/{}.rb", self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BottleCellar {
    AnySkipRelocation,
    Any,
    Fixed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeprecateDisable {
    pub date: Option<String>,
    /// `deprecated_upstream`, `unmaintained`, ... or free text.
    pub because: Option<String>,
    pub replacement_formula: Option<String>,
    pub replacement_cask: Option<String>,
}

fn parse_deprecate(v: &Value) -> Option<DeprecateDisable> {
    let get = |k: &str| v.get(k).and_then(Value::as_str).map(|s| sym(s).to_string());
    Some(DeprecateDisable {
        date: get(":date"),
        because: get(":because"),
        replacement_formula: get(":replacement_formula"),
        replacement_cask: get(":replacement_cask"),
    })
}

/// Accept either a single value or a list of them, always producing a list.
/// Used for payload fields whose Ruby DSL takes keyword arguments only.
fn one_or_many<'de, D>(deserializer: D) -> std::result::Result<Vec<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Value::deserialize(deserializer)? {
        Value::Null => vec![],
        Value::Array(a) => a,
        other => vec![other],
    })
}

fn parse_tag(s: &str) -> Option<DependencyTag> {
    match sym(s) {
        "build" => Some(DependencyTag::Build),
        "test" => Some(DependencyTag::Test),
        "optional" => Some(DependencyTag::Optional),
        "recommended" => Some(DependencyTag::Recommended),
        "implicit" => Some(DependencyTag::Implicit),
        _ => None,
    }
}

/// `"name"` | `{"name": ":build"}` | `{"name": [":build", ":test"]}`.
pub fn parse_dependency(v: &Value) -> Option<Dependency> {
    match v {
        Value::String(s) => Some(Dependency {
            name: s.clone(),
            tags: vec![],
        }),
        Value::Object(map) => {
            let (name, tags) = map.iter().next()?;
            let tags = match tags {
                Value::String(s) => parse_tag(s).into_iter().collect(),
                Value::Array(a) => a
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(parse_tag)
                    .collect(),
                _ => vec![],
            };
            Some(Dependency {
                name: name.clone(),
                tags,
            })
        }
        _ => None,
    }
}

/// `["name"]` | `["name", {":since": ":sequoia"}]` | `[{"name": ":build"}]` | `[{"name": ":build"}, {":since": ..}]`.
pub fn parse_uses_from_macos(v: &Value) -> Option<UsesFromMacos> {
    let arr = v.as_array()?;
    let dep = parse_dependency(arr.first()?)?;
    let since = arr
        .get(1)
        .and_then(|b| b.get(":since"))
        .and_then(Value::as_str)
        .map(|s| sym(s).to_string());
    Some(UsesFromMacos { dep, since })
}

fn license_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(license_to_string)
            .collect::<Vec<_>>()
            .join(" and "),
        Value::Object(m) => {
            let mut parts = vec![];
            for (k, val) in m {
                match sym(k) {
                    "any_of" => parts.push(format!(
                        "({})",
                        val.as_array()
                            .map(|a| a
                                .iter()
                                .map(license_to_string)
                                .collect::<Vec<_>>()
                                .join(" or "))
                            .unwrap_or_default()
                    )),
                    "all_of" => parts.push(
                        val.as_array()
                            .map(|a| {
                                a.iter()
                                    .map(license_to_string)
                                    .collect::<Vec<_>>()
                                    .join(" and ")
                            })
                            .unwrap_or_default(),
                    ),
                    "with" => parts.push(format!("with {}", val.as_str().unwrap_or(""))),
                    other => parts.push(format!("{other} {}", license_to_string(val))),
                }
            }
            parts.join(" ")
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_dependencies() {
        let e: FormulaEntry = serde_json::from_value(json!({
            "stable_dependencies": ["pcre2", {"pkgconf": ":build"}, {"foo": [":build", ":test"]}],
            "stable_uses_from_macos": [["bzip2"], ["expat", {":since": ":sequoia"}], [{"bison": ":build"}]],
            "keg_only_args": [":provided_by_macos", "Apple's CLT provides apr"],
            "conflicts": [["cgrep", {":because": "both install `cgrep` binaries"}]],
            "bottle_cellar": ":any",
            "stable_version": "1.2", "revision": 3, "bottle_rebuild": 1
        }))
        .unwrap();
        let deps = e.dependencies();
        assert_eq!(deps.len(), 3);
        assert!(deps[0].is_runtime());
        assert!(deps[1].is_build() && !deps[1].is_runtime());
        assert!(deps[2].is_build() && deps[2].is_test() && !deps[2].is_runtime());
        let ufm = e.uses_from_macos();
        assert_eq!(ufm[1].since.as_deref(), Some("sequoia"));
        assert!(ufm[2].dep.is_build());
        assert_eq!(e.keg_only().unwrap().0, KegOnly::ProvidedByMacos);
        assert_eq!(e.conflicts_with()[0].0, "cgrep");
        assert_eq!(e.bottle_cellar_kind(), BottleCellar::Any);
        assert_eq!(e.pkg_version(), "1.2_3");
    }

    #[test]
    fn absent_cellar_means_skip_relocation() {
        let e: FormulaEntry = serde_json::from_value(json!({"stable_version": "1"})).unwrap();
        assert_eq!(e.bottle_cellar_kind(), BottleCellar::AnySkipRelocation);
        assert!(!e.has_bottle());
    }
}
