//! `INSTALL_RECEIPT.json` construction (`docs/COMPAT.md` 3).
//!
//! Port of `FormulaInstaller#pour`'s tab rewrite plus the `finish` step that
//! replaces `runtime_dependencies` with the resolved graph
//! (`Tab.runtime_deps_hash` / `Tab.formula_to_dep_hash`). Build-time fields
//! come from the manifest's `sh.brew.tab` annotation, falling back to the
//! receipt the bottle itself ships (`Utils::Bottles.load_tab`); install-time
//! fields are filled in here.

use std::path::Path;

use crate::api::index::Index;
use crate::bottle::BottleTab;
use crate::config::Config;
use crate::deps::{self, DepOptions};
use crate::model::receipt::{ReceiptSource, ReceiptVersions};
use crate::model::{FormulaEntry, FormulaReceipt, RuntimeDependency};
use crate::platform::Host;

/// Everything about one install the receipt records beyond the tab.
pub struct ReceiptArgs<'a> {
    pub formula: &'a FormulaEntry,
    pub tab: &'a BottleTab,
    pub installed_on_request: bool,
    /// Unix seconds (`Time.now.to_i`).
    pub time: u64,
    /// Final `runtime_dependencies`, in dependency order.
    pub runtime_dependencies: Vec<RuntimeDependency>,
}

/// Path of the internal packages file, recorded as `source.path`.
pub fn api_file_path(cfg: &Config) -> String {
    let tag = Host::detect().bottle_tag();
    cfg.cache_api()
        .join(format!("internal/packages.{tag}.jws.json"))
        .to_string_lossy()
        .into_owned()
}

/// Merge the receipt the bottle ships into the manifest tab, so fields the
/// annotation omits (`built_on`, `compiler`, `source_modified_time`) still come
/// from the build machine. Mirrors `Utils::Bottles.load_tab`, which only trusts
/// the annotation when its `built_on.os` matches this system.
pub fn tab_with_keg_fallback(tab: &BottleTab, keg_path: &Path) -> BottleTab {
    let mut merged = tab.clone();
    let host_os = Host::detect().system_name();
    let annotation_matches = merged
        .built_on
        .as_ref()
        .and_then(|b| b.get("os"))
        .and_then(|v| v.as_str())
        == Some(host_os);
    if annotation_matches && merged.compiler.is_some() {
        return merged;
    }
    let Ok(bottled) = FormulaReceipt::read(&keg_path.join("INSTALL_RECEIPT.json")) else {
        return merged;
    };
    if merged.built_on.is_none() {
        merged.built_on = bottled.built_on.clone();
    }
    if merged.compiler.is_none() && !bottled.compiler.is_empty() {
        merged.compiler = Some(bottled.compiler.clone());
    }
    if merged.stdlib.is_none() {
        merged.stdlib = bottled.stdlib.clone();
    }
    if merged.source_modified_time == 0 {
        merged.source_modified_time = bottled.source_modified_time;
    }
    if merged.changed_files.is_none() {
        merged.changed_files = bottled.changed_files.clone();
    }
    if merged.linkage_files.is_none() {
        merged.linkage_files = bottled.linkage_files.clone();
    }
    if merged.binary_relocation_files.is_none() {
        merged.binary_relocation_files = bottled.binary_relocation_files.clone();
    }
    if merged.arch.is_none() {
        merged.arch = bottled.arch.clone();
    }
    merged
}

/// Build the receipt Homebrew writes after pouring a bottle.
pub fn build(cfg: &Config, args: ReceiptArgs<'_>) -> FormulaReceipt {
    let formula = args.formula;
    let tab = args.tab;
    FormulaReceipt {
        homebrew_version: Some(crate::HOMEBREW_COMPAT_VERSION.to_string()),
        used_options: vec![],
        unused_options: vec![],
        built_as_bottle: true,
        built_prefix: tab.built_prefix.clone(),
        padded_prefix: tab.padded_prefix,
        poured_from_bottle: true,
        loaded_from_api: true,
        loaded_from_internal_api: true,
        // Homebrew 6 no longer writes this key.
        installed_as_dependency: None,
        installed_on_request: args.installed_on_request,
        changed_files: tab.changed_files.clone(),
        linkage_files: tab.linkage_files.clone(),
        binary_relocation_files: tab.binary_relocation_files.clone(),
        relocated_build_prefix: None,
        relocated_files: None,
        time: Some(args.time),
        source_modified_time: tab.source_modified_time,
        stdlib: tab.stdlib.clone(),
        compiler: tab.compiler.clone().unwrap_or_else(|| "clang".to_string()),
        aliases: formula.aliases.clone(),
        runtime_dependencies: Some(args.runtime_dependencies),
        source: ReceiptSource {
            spec: Some("stable".to_string()),
            versions: Some(ReceiptVersions {
                stable: formula.stable_version.clone(),
                head: None,
                version_scheme: formula.version_scheme,
                compatibility_version: None,
            }),
            path: Some(api_file_path(cfg)),
            tap_git_head: None,
            tap: Some(if formula.tap.is_empty() {
                "homebrew/core".to_string()
            } else {
                formula.tap.clone()
            }),
        },
        arch: tab
            .arch
            .clone()
            .or_else(|| Some(Host::detect().arch.as_str().to_string())),
        built_on: tab
            .built_on
            .clone()
            .or_else(|| Some(crate::model::receipt::built_on_for_host())),
    }
}

/// `Tab.runtime_deps_hash`: the resolved runtime closure in dependency order,
/// `declared_directly` for the formula's own `depends_on` entries, each with
/// the pkg_version of the keg that ends up installed. `planned` supplies the
/// versions this run is about to install, which are not on disk yet.
pub fn runtime_dependencies(
    cfg: &Config,
    index: &Index,
    formula: &FormulaEntry,
    planned: &dyn Fn(&str) -> Option<String>,
) -> Vec<RuntimeDependency> {
    let declared: Vec<String> =
        deps::entry_dependency_names(index, formula, DepOptions::default(), true)
            .into_iter()
            .map(|d| deps::short_name(&d).to_string())
            .collect();
    let closure = deps::recursive_dependencies(cfg, index, formula, DepOptions::default())
        .unwrap_or_default();
    closure
        .into_iter()
        .filter_map(|entry| {
            let name = entry.name.clone();
            let pkg_version = planned(&name)
                .or_else(|| crate::keg::latest_keg(cfg, &name).map(|k| k.version.to_string()))
                .or_else(|| Some(entry.pkg_version()).filter(|v| !v.is_empty()))?;
            let (version, revision) = split_pkg_version(&pkg_version);
            Some(RuntimeDependency {
                full_name: entry.full_name(),
                version,
                revision,
                bottle_rebuild: Some(entry.bottle_rebuild),
                pkg_version,
                declared_directly: declared.contains(&name),
                compatibility_version: None,
            })
        })
        .collect()
}

/// `<version>_<revision>` -> `(version, revision)`.
fn split_pkg_version(pkg_version: &str) -> (String, u32) {
    match pkg_version.rsplit_once('_') {
        Some((v, r)) if !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()) => {
            (v.to_string(), r.parse().unwrap_or(0))
        }
        _ => (pkg_version.to_string(), 0),
    }
}

/// Seconds since the epoch, for the receipt's `time`.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jq_tab() -> BottleTab {
        BottleTab {
            homebrew_version: Some("6.0.9".into()),
            changed_files: Some(vec!["lib/pkgconfig/libjq.pc".into()]),
            linkage_files: Some(vec!["bin/jq".into()]),
            binary_relocation_files: None,
            padded_prefix: None,
            built_prefix: None,
            source_modified_time: 1_773_700_688,
            compiler: Some("clang".into()),
            stdlib: None,
            runtime_dependencies: vec![],
            arch: Some("arm64".into()),
            built_on: Some(serde_json::json!({"os": "Macintosh", "os_version": "macOS 26"})),
        }
    }

    #[test]
    fn splits_pkg_versions() {
        assert_eq!(split_pkg_version("1.8.2"), ("1.8.2".into(), 0));
        assert_eq!(split_pkg_version("1.8.2_3"), ("1.8.2".into(), 3));
        assert_eq!(split_pkg_version("2026-07-16"), ("2026-07-16".into(), 0));
    }

    #[test]
    fn builds_a_receipt_from_a_bottle_tab() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::for_test(tmp.path());
        let formula = FormulaEntry {
            name: "jq".into(),
            tap: "homebrew/core".into(),
            stable_version: Some("1.8.2".into()),
            ..Default::default()
        };
        let receipt = build(
            &cfg,
            ReceiptArgs {
                formula: &formula,
                tab: &jq_tab(),
                installed_on_request: true,
                time: 1_778_031_361,
                runtime_dependencies: vec![RuntimeDependency {
                    full_name: "oniguruma".into(),
                    version: "6.9.10".into(),
                    revision: 0,
                    bottle_rebuild: Some(0),
                    pkg_version: "6.9.10".into(),
                    declared_directly: true,
                    compatibility_version: None,
                }],
            },
        );
        let json = receipt.to_json_string();
        // Homebrew 6 dropped `installed_as_dependency`.
        assert!(!json.contains("installed_as_dependency"), "{json}");
        assert!(json.contains("\"homebrew_version\": \"6.0.22\""), "{json}");
        assert!(json.contains("\"declared_directly\": true"), "{json}");
        assert!(
            json.contains("internal/packages."),
            "source.path must name the internal API file:\n{json}"
        );

        // Key order matches `Tab#to_json`, with the null-valued keys omitted.
        let keys: Vec<&str> = json
            .lines()
            .filter_map(|l| l.strip_prefix("  \""))
            .filter_map(|l| l.split('"').next())
            .collect();
        assert_eq!(
            keys,
            [
                "homebrew_version",
                "used_options",
                "unused_options",
                "built_as_bottle",
                "poured_from_bottle",
                "loaded_from_api",
                "loaded_from_internal_api",
                "installed_on_request",
                "changed_files",
                "linkage_files",
                "time",
                "source_modified_time",
                "compiler",
                "aliases",
                "runtime_dependencies",
                "source",
                "arch",
                "built_on",
            ]
        );
    }

    #[test]
    fn falls_back_to_the_receipt_shipped_in_the_bottle() {
        let tmp = tempfile::tempdir().unwrap();
        let keg = tmp.path().join("keg");
        std::fs::create_dir_all(&keg).unwrap();
        let bottled = FormulaReceipt {
            compiler: "gcc-13".into(),
            source_modified_time: 42,
            built_on: Some(serde_json::json!({"os": "Macintosh", "xcode": "26.4"})),
            ..Default::default()
        };
        bottled.write(&keg.join("INSTALL_RECEIPT.json")).unwrap();

        let mut tab = jq_tab();
        tab.compiler = None;
        tab.built_on = None;
        tab.source_modified_time = 0;
        let merged = tab_with_keg_fallback(&tab, &keg);
        assert_eq!(merged.compiler.as_deref(), Some("gcc-13"));
        assert_eq!(merged.source_modified_time, 42);
        assert_eq!(
            merged.built_on.as_ref().and_then(|b| b.get("xcode")),
            Some(&serde_json::json!("26.4"))
        );

        // A complete annotation for this system is used as is.
        let complete = tab_with_keg_fallback(&jq_tab(), &keg);
        assert_eq!(complete.compiler.as_deref(), Some("clang"));
        assert_eq!(complete.source_modified_time, 1_773_700_688);
    }
}
