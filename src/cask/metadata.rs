//! Caskroom metadata: the installed caskfile, `config.json` and the cask
//! `INSTALL_RECEIPT.json` (`docs/COMPAT.md` 6).
//!
//! The receipt is written by hand rather than through `serde` so the key order
//! matches `Cask::Tab#to_json` byte-for-byte; Homebrew must be able to read
//! back what fastbrew wrote.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::config::Config;
use crate::error::Result;
use crate::model::CaskEntry;

use super::config::CaskDirs;

/// Everything the metadata writers need about the cask being installed.
pub struct ReceiptInput<'a> {
    pub cask: &'a CaskEntry,
    /// `.metadata/<version>/<timestamp>/Casks/<token>.json` timestamp directory.
    pub metadata_subdir: PathBuf,
    pub receipt_path: PathBuf,
    pub config_path: PathBuf,
    pub uninstall_artifacts: Vec<Value>,
    pub installed_on_request: bool,
    pub tap_git_head: Option<String>,
    pub api_path: Option<String>,
    pub runtime_dependencies: Value,
}

/// `Cask#to_installed_json_hash` plus the `artifacts` key Homebrew adds when a
/// cask declares nothing that needs uninstalling.
pub fn installed_caskfile_json(cask: &CaskEntry, uninstall_artifacts: &[Value]) -> Value {
    let mut map = Map::new();
    let only_path = super::download::UrlKwargs::from_value(cask.url_kwargs.as_ref()).only_path;
    if let Some(only_path) = only_path.filter(|p| !p.is_empty()) {
        let mut specs = Map::new();
        specs.insert("only_path".into(), Value::String(only_path));
        map.insert("url_specs".into(), Value::Object(specs));
    }
    if uninstall_artifacts.is_empty() {
        map.insert("artifacts".into(), Value::Array(vec![]));
    }
    Value::Object(map)
}

/// `Cask::Tab#to_json`: key order is load-bearing.
pub fn receipt_json(input: &ReceiptInput<'_>) -> Value {
    let mut source = Map::new();
    source.insert("tap".into(), Value::String(input.cask.tap().to_string()));
    source.insert(
        "tap_git_head".into(),
        match &input.tap_git_head {
            Some(head) => Value::String(head.clone()),
            None => Value::Null,
        },
    );
    source.insert(
        "version".into(),
        match &input.cask.version {
            Some(v) => Value::String(v.clone()),
            None => Value::Null,
        },
    );
    source.insert(
        "path".into(),
        match &input.api_path {
            Some(p) => Value::String(p.clone()),
            None => Value::Null,
        },
    );

    let mut map = Map::new();
    map.insert(
        "homebrew_version".into(),
        Value::String(crate::HOMEBREW_COMPAT_VERSION.to_string()),
    );
    map.insert("loaded_from_api".into(), Value::Bool(true));
    map.insert("loaded_from_internal_api".into(), Value::Bool(true));
    // fastbrew never evaluates Ruby flight blocks, so this is always false.
    map.insert("uninstall_flight_blocks".into(), Value::Bool(false));
    map.insert(
        "installed_on_request".into(),
        Value::Bool(input.installed_on_request),
    );
    map.insert("time".into(), Value::Number(now_seconds().into()));
    map.insert(
        "runtime_dependencies".into(),
        input.runtime_dependencies.clone(),
    );
    map.insert("source".into(), Value::Object(source));
    map.insert(
        "arch".into(),
        Value::String(crate::platform::Host::detect().arch.as_str().to_string()),
    );
    map.insert(
        "uninstall_artifacts".into(),
        Value::Array(input.uninstall_artifacts.clone()),
    );
    map.insert(
        "built_on".into(),
        crate::model::receipt::built_on_for_host(),
    );
    Value::Object(map)
}

pub fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `Cask::Installer#save_caskfile`: write
/// `.metadata/<version>/<timestamp>/Casks/<token>.json` and drop the previous
/// timestamp directory.
pub fn write_caskfile(input: &ReceiptInput<'_>, previous_metadata: Option<&Path>) -> Result<()> {
    let casks = input.metadata_subdir.join("Casks");
    std::fs::create_dir_all(&casks)?;
    let caskfile = casks.join(format!("{}.json", input.cask.token));
    let contents = installed_caskfile_json(input.cask, &input.uninstall_artifacts);
    crate::keg::atomic_write(&caskfile, pretty(&contents).as_bytes())?;

    if let Some(previous) = previous_metadata
        && previous != input.metadata_subdir
    {
        let _ = std::fs::remove_dir_all(previous);
    }
    Ok(())
}

/// `Cask::Installer#save_config_file`: the compact `config.json`.
pub fn write_config(
    cfg: &Config,
    dirs: &CaskDirs,
    path: &Path,
    explicit_flags: &[String],
) -> Result<()> {
    crate::keg::atomic_write(
        path,
        serde_json::to_string(&dirs.to_config_json(cfg, explicit_flags))
            .unwrap_or_default()
            .as_bytes(),
    )?;
    Ok(())
}

/// `Cask::Tab#write`.
pub fn write_receipt(input: &ReceiptInput<'_>) -> Result<()> {
    crate::keg::atomic_write(&input.receipt_path, pretty(&receipt_json(input)).as_bytes())?;
    Ok(())
}

/// `JSON.pretty_generate`: two-space indent, no trailing newline.
pub fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// Read `uninstall_artifacts` out of an installed cask's receipt.
pub fn receipt_uninstall_artifacts(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| {
            v.get("uninstall_artifacts")
                .and_then(Value::as_array)
                .cloned()
        })
        .unwrap_or_default()
}

/// Read the `artifacts` key of an installed caskfile, when it carries one.
pub fn caskfile_artifacts(path: &Path) -> Option<Vec<Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.get("artifacts").and_then(Value::as_array).cloned()
}

/// Read `url_specs.only_path` from an installed caskfile.
pub fn caskfile_only_path(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("url_specs")?
        .get("only_path")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cask() -> CaskEntry {
        let mut cask: CaskEntry = serde_json::from_value(serde_json::json!({
            "tap_string": "homebrew/cask",
            "version": "1.1.3",
            "url_args": ["https://example.com/Demo.dmg"],
            "sha256": "abc"
        }))
        .unwrap();
        cask.token = "demo".into();
        cask
    }

    #[test]
    fn receipt_key_order_matches_homebrew() {
        let cask = cask();
        let input = ReceiptInput {
            cask: &cask,
            metadata_subdir: PathBuf::from("/tmp/x"),
            receipt_path: PathBuf::from("/tmp/r"),
            config_path: PathBuf::from("/tmp/c"),
            uninstall_artifacts: vec![serde_json::json!({"app": ["Demo.app"]})],
            installed_on_request: true,
            tap_git_head: Some("deadbeef".into()),
            api_path: Some("/cache/api/internal/packages.arm64_tahoe.jws.json".into()),
            runtime_dependencies: serde_json::json!({}),
        };
        let json = receipt_json(&input);
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "homebrew_version",
                "loaded_from_api",
                "loaded_from_internal_api",
                "uninstall_flight_blocks",
                "installed_on_request",
                "time",
                "runtime_dependencies",
                "source",
                "arch",
                "uninstall_artifacts",
                "built_on",
            ]
        );
        let source_keys: Vec<&str> = json["source"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(source_keys, ["tap", "tap_git_head", "version", "path"]);
        assert_eq!(json["homebrew_version"], crate::HOMEBREW_COMPAT_VERSION);
        assert_eq!(json["source"]["version"], "1.1.3");
    }

    #[test]
    fn installed_caskfile_shapes() {
        let mut cask = cask();
        // Uninstallable artifacts and no `only_path`: an empty object.
        let json = installed_caskfile_json(&cask, &[serde_json::json!({"app": ["Demo.app"]})]);
        assert_eq!(json, serde_json::json!({}));

        // No uninstallable artifacts: Homebrew records the empty list.
        let json = installed_caskfile_json(&cask, &[]);
        assert_eq!(json, serde_json::json!({"artifacts": []}));

        cask.url_kwargs = Some(serde_json::json!({":only_path": "inner/dir"}));
        let json = installed_caskfile_json(&cask, &[serde_json::json!({"app": ["Demo.app"]})]);
        assert_eq!(
            json,
            serde_json::json!({"url_specs": {"only_path": "inner/dir"}})
        );
    }
}
