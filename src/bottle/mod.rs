//! Bottles: registry access, downloads, extraction and relocation
//! (`docs/COMPAT.md` 2 and 4).

pub mod codesign;
pub mod extract;
pub mod fetch;
pub mod macho;
pub mod relocate;

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Run `f` with `path` temporarily user-writable, restoring its mode
/// afterwards (`Utils::Path.ensure_writable`). Bottled files are commonly
/// `r-xr-xr-x`, and relocation, signing and text replacement all rewrite them
/// in place.
pub fn with_writable<T>(
    path: &Path,
    f: impl FnOnce() -> crate::error::Result<T>,
) -> crate::error::Result<T> {
    use std::os::unix::fs::PermissionsExt;
    let original = std::fs::symlink_metadata(path)
        .ok()
        .map(|m| m.permissions());
    let restore = match &original {
        Some(p) if p.mode() & 0o200 == 0 => {
            let mut relaxed = p.clone();
            relaxed.set_mode(p.mode() | 0o200);
            std::fs::set_permissions(path, relaxed)
                .ok()
                .map(|()| p.clone())
        }
        _ => None,
    };
    let result = f();
    if let Some(mode) = restore {
        let _ = std::fs::set_permissions(path, mode);
    }
    result
}

/// The `sh.brew.tab` annotation of a bottle manifest.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BottleTab {
    pub homebrew_version: Option<String>,
    pub changed_files: Vec<String>,
    pub linkage_files: Option<Vec<String>>,
    pub binary_relocation_files: Option<Vec<String>>,
    pub padded_prefix: Option<bool>,
    pub built_prefix: Option<String>,
    pub source_modified_time: u64,
    pub compiler: Option<String>,
    pub stdlib: Option<String>,
    pub runtime_dependencies: Vec<crate::model::RuntimeDependency>,
    pub arch: Option<String>,
    pub built_on: Option<serde_json::Value>,
}

/// Resolved location of one bottle for the host platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BottleRef {
    pub name: String,
    pub pkg_version: String,
    pub rebuild: u32,
    /// Tag the bottle is served under (may differ from the host tag or be `all`).
    pub tag: crate::platform::BottleTag,
    /// Registry root, e.g. `https://ghcr.io/v2/homebrew/core`.
    pub root_url: String,
    pub sha256: String,
}

impl BottleRef {
    /// `python@3.14` -> `python/3.14`, `+` -> `x`.
    pub fn image_name(&self) -> String {
        self.name.replace('@', "/").replace('+', "x")
    }

    /// `<version>` or `<version>-<rebuild>`.
    pub fn manifest_tag(&self) -> String {
        if self.rebuild > 0 {
            format!("{}-{}", self.pkg_version, self.rebuild)
        } else {
            self.pkg_version.clone()
        }
    }

    pub fn manifest_url(&self) -> String {
        format!(
            "{}/{}/manifests/{}",
            self.root_url,
            self.image_name(),
            self.manifest_tag()
        )
    }

    pub fn blob_url(&self) -> String {
        format!(
            "{}/{}/blobs/sha256:{}",
            self.root_url,
            self.image_name(),
            self.sha256
        )
    }

    /// `org.opencontainers.image.ref.name` of the platform manifest to select.
    pub fn ref_name(&self) -> String {
        if self.rebuild > 0 {
            format!("{}.{}.{}", self.pkg_version, self.tag, self.rebuild)
        } else {
            format!("{}.{}", self.pkg_version, self.tag)
        }
    }

    /// `<name>--<pkg_version>.<tag>.bottle[.<rebuild>].tar.gz`.
    pub fn filename(&self) -> String {
        let rebuild = if self.rebuild > 0 {
            format!(".{}", self.rebuild)
        } else {
            String::new()
        };
        format!(
            "{}--{}.{}.bottle{}.tar.gz",
            self.name, self.pkg_version, self.tag, rebuild
        )
    }

    /// `<name>-<version[-rebuild]>.bottle_manifest.json`.
    pub fn manifest_filename(&self) -> String {
        format!("{}-{}.bottle_manifest.json", self.name, self.manifest_tag())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::BottleTag;

    fn b(name: &str, v: &str, rebuild: u32) -> BottleRef {
        BottleRef {
            name: name.into(),
            pkg_version: v.into(),
            rebuild,
            tag: BottleTag::parse("arm64_tahoe").unwrap(),
            root_url: "https://ghcr.io/v2/homebrew/core".into(),
            sha256: "ab".into(),
        }
    }

    #[test]
    fn naming() {
        let h = b("hello", "2.12.3", 1);
        assert_eq!(
            h.manifest_url(),
            "https://ghcr.io/v2/homebrew/core/hello/manifests/2.12.3-1"
        );
        assert_eq!(h.ref_name(), "2.12.3.arm64_tahoe.1");
        assert_eq!(h.filename(), "hello--2.12.3.arm64_tahoe.bottle.1.tar.gz");
        assert_eq!(h.manifest_filename(), "hello-2.12.3-1.bottle_manifest.json");
        let p = b("python@3.14", "3.14.7", 0);
        assert_eq!(p.image_name(), "python/3.14");
        assert_eq!(p.ref_name(), "3.14.7.arm64_tahoe");
        assert_eq!(
            p.filename(),
            "python@3.14--3.14.7.arm64_tahoe.bottle.tar.gz"
        );
    }
}
