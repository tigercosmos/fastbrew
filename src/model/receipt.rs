//! `INSTALL_RECEIPT.json` for formulae (kegs) and casks (`docs/COMPAT.md` 3 and 6).
//!
//! Serialization must reproduce Homebrew's key order and omit the optional
//! keys when `None`, so that receipts round-trip byte-for-byte.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RuntimeDependency {
    pub full_name: String,
    pub version: String,
    pub revision: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bottle_rebuild: Option<u32>,
    pub pkg_version: String,
    pub declared_directly: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility_version: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ReceiptVersions {
    pub stable: Option<String>,
    pub head: Option<String>,
    pub version_scheme: u32,
    #[serde(default)]
    pub compatibility_version: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ReceiptSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub versions: Option<ReceiptVersions>,
    pub path: Option<String>,
    pub tap_git_head: Option<String>,
    pub tap: Option<String>,
}

/// Formula install receipt, key order as Homebrew writes it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct FormulaReceipt {
    pub homebrew_version: Option<String>,
    pub used_options: Vec<String>,
    pub unused_options: Vec<String>,
    pub built_as_bottle: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub built_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padded_prefix: Option<bool>,
    pub poured_from_bottle: bool,
    pub loaded_from_api: bool,
    pub loaded_from_internal_api: bool,
    pub installed_as_dependency: bool,
    pub installed_on_request: bool,
    pub changed_files: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linkage_files: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_relocation_files: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relocated_build_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relocated_files: Option<Vec<String>>,
    pub time: Option<u64>,
    pub source_modified_time: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdlib: Option<String>,
    pub compiler: String,
    pub aliases: Vec<String>,
    pub runtime_dependencies: Option<Vec<RuntimeDependency>>,
    pub source: ReceiptSource,
    pub arch: Option<String>,
    pub built_on: Option<Value>,
}

impl FormulaReceipt {
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Pretty JSON with two-space indentation, matching `JSON.pretty_generate`.
    pub fn to_json_string(&self) -> String {
        serde_json::to_string_pretty(self).expect("receipt serializes")
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        crate::keg::atomic_write(path, self.to_json_string().as_bytes())
    }

    pub fn tap(&self) -> Option<&str> {
        self.source.tap.as_deref()
    }

    /// Runtime dependency names (`full_name`) as recorded at install time.
    pub fn runtime_dependency_names(&self) -> Vec<&str> {
        self.runtime_dependencies
            .as_deref()
            .map(|d| d.iter().map(|r| r.full_name.as_str()).collect())
            .unwrap_or_default()
    }
}

/// Cask install receipt (`Caskroom/<token>/.metadata/INSTALL_RECEIPT.json`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct CaskReceipt {
    pub homebrew_version: Option<String>,
    pub loaded_from_api: bool,
    pub uninstall_flight_blocks: bool,
    pub installed_as_dependency: bool,
    pub installed_on_request: bool,
    pub time: Option<u64>,
    pub runtime_dependencies: Value,
    pub source: CaskReceiptSource,
    pub arch: Option<String>,
    pub uninstall_artifacts: Vec<Value>,
    pub built_on: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct CaskReceiptSource {
    pub tap: Option<String>,
    pub tap_git_head: Option<String>,
    pub version: Option<String>,
    pub path: Option<String>,
}

impl CaskReceipt {
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).expect("receipt serializes");
        crate::keg::atomic_write(path, text.as_bytes())
    }
}

/// `built_on` block for receipts written by fastbrew.
pub fn built_on_for_host() -> Value {
    let host = crate::platform::Host::detect();
    serde_json::json!({
        "os": host.system_name(),
        "os_version": host.os_version_string(),
        "cpu_family": "dunno",
        "xcode": "",
        "clt": "",
        "preferred_perl": "5.34",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_homebrew_receipt() {
        let text = r#"{
  "homebrew_version": "5.1.0-51-g2316400",
  "used_options": [],
  "unused_options": [],
  "built_as_bottle": true,
  "poured_from_bottle": true,
  "loaded_from_api": true,
  "loaded_from_internal_api": false,
  "installed_as_dependency": false,
  "installed_on_request": true,
  "changed_files": [],
  "time": 1778031361,
  "source_modified_time": 1773700688,
  "compiler": "clang",
  "aliases": [],
  "runtime_dependencies": [],
  "source": {
    "spec": "stable",
    "versions": {
      "stable": "2.3.2",
      "head": null,
      "version_scheme": 0,
      "compatibility_version": null
    },
    "path": "/Users/x/Library/Caches/Homebrew/api/formula.jws.json",
    "tap_git_head": null,
    "tap": "homebrew/core"
  },
  "arch": "arm64",
  "built_on": {
    "os": "Macintosh",
    "os_version": "macOS 26.3",
    "cpu_family": "dunno",
    "xcode": "26.3",
    "clt": "26.3.0.0.1.1771626560",
    "preferred_perl": "5.34"
  }
}"#;
        let r: FormulaReceipt = serde_json::from_str(text).unwrap();
        assert_eq!(r.to_json_string(), text);
    }
}
