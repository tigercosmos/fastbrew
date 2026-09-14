//! End-to-end tests for the mutating formula operations.
//!
//! Every test runs the built binary against an isolated prefix, sharing one
//! cache directory (seeded with the internal packages file, and reused for
//! bottle downloads) so repeated runs do not re-download. Nothing here touches
//! `/opt/homebrew` or the user's Homebrew cache.
//!
//! Run them with `FASTBREW_TEST_NETWORK=1 scripts/sandbox.sh test ops`. Without
//! `FASTBREW_TEST_NETWORK=1` the network tests skip; the receipt and
//! `install_etc_var` tests run everywhere.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use assert_cmd::prelude::*;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Sandbox plumbing (mirrors `tests/readonly.rs` and `scripts/sandbox.sh`)
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn bottle_tag() -> String {
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64_"
    } else {
        ""
    };
    let product = Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let major: u32 = product
        .split('.')
        .next()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    let name = match major {
        27 => "golden_gate",
        26 => "tahoe",
        15 => "sequoia",
        14 => "sonoma",
        13 => "ventura",
        12 => "monterey",
        11 => "big_sur",
        _ => "unknown",
    };
    format!("{arch}{name}")
}

fn seed_packages_file() -> Option<&'static PathBuf> {
    static SEED: OnceLock<Option<PathBuf>> = OnceLock::new();
    SEED.get_or_init(|| {
        let rel = format!("api/internal/packages.{}.jws.json", bottle_tag());
        if let Some(cache) = std::env::var_os("HOMEBREW_CACHE") {
            let path = PathBuf::from(cache).join(&rel);
            if path.is_file() {
                return Some(path);
            }
        }
        let out = Command::new(repo_root().join("scripts/sandbox.sh"))
            .arg("env")
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let cache = text.lines().find_map(|line| {
            line.strip_prefix("export HOMEBREW_CACHE=")
                .map(|v| v.trim_matches('"').to_string())
        })?;
        let path = PathBuf::from(cache).join(&rel);
        path.is_file().then_some(path)
    })
    .as_ref()
}

/// One cache shared by every test: the 15 MB API file, the fast index and the
/// bottle downloads are all expensive to recreate.
fn shared_cache() -> Option<&'static PathBuf> {
    static CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let seed = seed_packages_file()?;
            let cache = repo_root().join("target/sandbox-tests/cache");
            let target = cache.join(format!("api/internal/packages.{}.jws.json", bottle_tag()));
            std::fs::create_dir_all(target.parent()?).ok()?;
            if !target.is_file() && std::fs::hard_link(seed, &target).is_err() {
                std::fs::copy(seed, &target).ok()?;
            }
            Some(cache)
        })
        .as_ref()
}

struct Sandbox {
    _dir: TempDir,
    prefix: PathBuf,
    cache: PathBuf,
    home: PathBuf,
}

impl Sandbox {
    fn new() -> Option<Sandbox> {
        let cache = shared_cache()?.clone();
        let dir = TempDir::new().ok()?;
        let prefix = dir.path().join("prefix");
        let home = dir.path().join("home");
        for sub in [
            "bin",
            "sbin",
            "etc",
            "include",
            "lib",
            "share",
            "Frameworks",
            "Cellar",
            "opt",
            "Caskroom",
            "Library/Taps",
            "var/homebrew/linked",
            "var/homebrew/pinned",
            "var/homebrew/pinned_casks",
            "var/homebrew/locks",
        ] {
            std::fs::create_dir_all(prefix.join(sub)).ok()?;
        }
        std::fs::create_dir_all(home.join("Library/LaunchAgents")).ok()?;
        Some(Sandbox {
            _dir: dir,
            prefix,
            cache,
            home,
        })
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("fastbrew").expect("fastbrew binary");
        cmd.env("HOMEBREW_PREFIX", &self.prefix)
            .env("HOMEBREW_CELLAR", self.prefix.join("Cellar"))
            .env("HOMEBREW_REPOSITORY", &self.prefix)
            .env("HOMEBREW_LIBRARY", self.prefix.join("Library"))
            .env("HOMEBREW_CACHE", &self.cache)
            .env("HOMEBREW_LOGS", self.home.join("logs"))
            .env("HOMEBREW_TEMP", self.home.join("tmp"))
            .env("HOME", &self.home)
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .env("HOMEBREW_NO_ANALYTICS", "1")
            .env("HOMEBREW_NO_COLOR", "1")
            .env("FASTBREW_REQUIRE_SANDBOX", "1")
            .env("FASTBREW_NO_DELEGATE", "1")
            .env_remove("HOMEBREW_NO_INSTALL_CLEANUP")
            .env_remove("HOMEBREW_NO_ENV_HINTS")
            .env_remove("HOMEBREW_API_DOMAIN")
            .env_remove("HOMEBREW_COLOR");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().expect("run fastbrew")
    }

    /// Run and require success, returning stdout and stderr joined.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        let text = combined(&out);
        assert!(
            out.status.success(),
            "`fastbrew {}` failed:\n{text}",
            args.join(" ")
        );
        text
    }

    /// Run and require failure, returning stdout and stderr joined.
    fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        let text = combined(&out);
        assert!(
            !out.status.success(),
            "`fastbrew {}` unexpectedly succeeded:\n{text}",
            args.join(" ")
        );
        text
    }

    fn keg(&self, name: &str, version: &str) -> PathBuf {
        self.prefix.join("Cellar").join(name).join(version)
    }

    fn receipt(&self, name: &str, version: &str) -> String {
        std::fs::read_to_string(self.keg(name, version).join("INSTALL_RECEIPT.json"))
            .unwrap_or_else(|e| panic!("no receipt for {name} {version}: {e}"))
    }

    fn receipt_json(&self, name: &str, version: &str) -> serde_json::Value {
        serde_json::from_str(&self.receipt(name, version)).expect("receipt is valid JSON")
    }
}

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Top-level keys of a pretty-printed receipt, in file order.
fn receipt_keys(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.strip_prefix("  \""))
        .filter_map(|l| l.split('"').next())
        .map(str::to_string)
        .collect()
}

/// The keys `docs/COMPAT.md` 3 says are omitted when null, so a receipt can be
/// compared against the mandatory sequence whatever the bottle's tab carried.
const OPTIONAL_RECEIPT_KEYS: [&str; 7] = [
    "built_prefix",
    "padded_prefix",
    "linkage_files",
    "binary_relocation_files",
    "relocated_build_prefix",
    "relocated_files",
    "stdlib",
];

/// The mandatory keys, in `Tab#to_json` order.
const RECEIPT_KEYS: [&str; 17] = [
    "homebrew_version",
    "used_options",
    "unused_options",
    "built_as_bottle",
    "poured_from_bottle",
    "loaded_from_api",
    "loaded_from_internal_api",
    "installed_on_request",
    "changed_files",
    "time",
    "source_modified_time",
    "compiler",
    "aliases",
    "runtime_dependencies",
    "source",
    "arch",
    "built_on",
];

fn mandatory_receipt_keys(text: &str) -> Vec<String> {
    receipt_keys(text)
        .into_iter()
        .filter(|k| !OPTIONAL_RECEIPT_KEYS.contains(&k.as_str()))
        .collect()
}

fn network() -> bool {
    let on = std::env::var("FASTBREW_TEST_NETWORK").as_deref() == Ok("1");
    if !on {
        eprintln!("skipping: set FASTBREW_TEST_NETWORK=1 to run network tests");
    }
    on
}

fn sandbox() -> Option<Sandbox> {
    match Sandbox::new() {
        Some(s) => Some(s),
        None => {
            eprintln!("skipping: no cached packages file; run through scripts/sandbox.sh");
            None
        }
    }
}

/// Version of `name` in the API the sandbox is seeded with.
fn api_version(sandbox: &Sandbox, name: &str) -> String {
    let out = sandbox.ok(&["list", "--versions", name]);
    out.split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string()
}

fn run_binary(path: &Path, args: &[&str]) -> String {
    let out = Command::new(path).args(args).output().expect("run binary");
    assert!(
        out.status.success(),
        "{} {args:?} failed: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

#[test]
fn installs_hello_without_relocation() {
    let Some(sb) = sandbox() else { return };
    if !network() {
        return;
    }

    let out = sb.ok(&["install", "hello"]);
    assert!(out.contains("==> Fetching hello"), "{out}");
    assert!(out.contains("==> Installing hello"), "{out}");
    assert!(out.contains("==> Pouring hello--"), "{out}");
    assert!(
        out.contains("🍺  ") && out.contains("files,"),
        "missing the summary line:\n{out}"
    );

    let version = sb
        .prefix
        .join("Cellar/hello")
        .read_dir()
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .to_string_lossy()
        .into_owned();
    let linked = sb.prefix.join("bin/hello");
    assert!(linked.is_symlink(), "bin/hello was not linked");
    assert_eq!(
        std::fs::read_link(&linked).unwrap(),
        PathBuf::from(format!("../Cellar/hello/{version}/bin/hello"))
    );
    assert_eq!(run_binary(&linked, &["--greeting=fastbrew"]), "fastbrew");
}

#[test]
fn installs_jq_with_its_dependency_and_writes_homebrew_receipts() {
    let Some(sb) = sandbox() else { return };
    if !network() {
        return;
    }

    let out = sb.ok(&["install", "jq"]);
    assert!(
        out.contains("==> Fetching dependencies for jq: oniguruma"),
        "{out}"
    );
    assert!(
        out.contains("==> Installing dependencies for jq: oniguruma"),
        "{out}"
    );
    assert!(
        out.contains("==> Installing jq dependency: oniguruma"),
        "{out}"
    );
    assert!(out.contains("==> Installing jq"), "{out}");
    assert!(out.contains("==> Pouring oniguruma--"), "{out}");
    assert!(out.contains("==> Pouring jq--"), "{out}");

    let jq_version = api_version(&sb, "jq");
    let onig_version = api_version(&sb, "oniguruma");

    // --- Receipts: key order and values per `docs/COMPAT.md` 3.
    let text = sb.receipt("jq", &jq_version);
    assert_eq!(
        mandatory_receipt_keys(&text),
        RECEIPT_KEYS,
        "unexpected receipt keys:\n{text}"
    );
    let jq = sb.receipt_json("jq", &jq_version);
    assert_eq!(jq["homebrew_version"], "6.0.22");
    assert_eq!(jq["installed_on_request"], true);
    assert_eq!(jq["poured_from_bottle"], true);
    assert_eq!(jq["loaded_from_internal_api"], true);
    assert_eq!(jq["arch"], "arm64");
    assert_eq!(jq["source"]["tap"], "homebrew/core");
    assert_eq!(jq["source"]["spec"], "stable");
    assert_eq!(jq["source"]["versions"]["stable"], jq_version.as_str());
    assert!(
        jq["source"]["path"]
            .as_str()
            .unwrap()
            .ends_with(&format!("api/internal/packages.{}.jws.json", bottle_tag())),
        "{}",
        jq["source"]["path"]
    );
    let runtime = jq["runtime_dependencies"].as_array().unwrap();
    assert_eq!(runtime.len(), 1, "{runtime:?}");
    assert_eq!(runtime[0]["full_name"], "oniguruma");
    assert_eq!(runtime[0]["declared_directly"], true);
    assert_eq!(runtime[0]["pkg_version"], onig_version.as_str());
    assert!(runtime[0].get("bottle_rebuild").is_some(), "{runtime:?}");
    // `built_on` comes from the bottle, not from this machine.
    assert_eq!(jq["built_on"]["os"], "Macintosh");

    let onig = sb.receipt_json("oniguruma", &onig_version);
    assert_eq!(
        onig["installed_on_request"], false,
        "a dependency is not installed on request"
    );
    assert!(
        sb.receipt("oniguruma", &onig_version)
            .find("installed_as_dependency")
            .is_none(),
        "Homebrew 6 does not write installed_as_dependency"
    );

    // --- Records and links.
    for (name, version) in [("jq", &jq_version), ("oniguruma", &onig_version)] {
        let opt = sb.prefix.join("opt").join(name);
        assert!(opt.is_symlink(), "opt/{name} missing");
        assert_eq!(
            std::fs::read_link(&opt).unwrap(),
            PathBuf::from(format!("../Cellar/{name}/{version}"))
        );
        let linked = sb.prefix.join("var/homebrew/linked").join(name);
        assert!(linked.is_symlink(), "linked record for {name} missing");
        assert_eq!(
            std::fs::read_link(&linked).unwrap(),
            PathBuf::from(format!("../../../Cellar/{name}/{version}"))
        );
    }
    let bin_jq = sb.prefix.join("bin/jq");
    assert!(bin_jq.is_symlink());
    assert_eq!(
        std::fs::read_link(&bin_jq).unwrap(),
        PathBuf::from(format!("../Cellar/jq/{jq_version}/bin/jq")),
        "bin/jq must be a relative symlink"
    );

    // --- The relocated binary runs against the relocated dependency.
    assert_eq!(
        run_binary(&bin_jq, &["--version"]),
        format!("jq-{jq_version}")
    );
    assert_eq!(run_binary(&bin_jq, &["-n", "1+1"]), "2");

    // --- Installing again warns instead of reinstalling.
    let again = sb.ok(&["install", "jq"]);
    assert!(
        again.contains(&format!(
            "Warning: jq {jq_version} is already installed and up-to-date."
        )),
        "{again}"
    );
    assert!(
        again.contains(&format!("To reinstall {jq_version}, run:")),
        "{again}"
    );
    assert!(again.contains("  brew reinstall jq"), "{again}");

    // --- `--dry-run` lists without touching anything.
    let dry = sb.ok(&["install", "-n", "wget"]);
    assert!(dry.contains("Would install"), "{dry}");
    assert!(!sb.prefix.join("Cellar/wget").exists());
}

#[test]
fn refuses_to_uninstall_a_dependency_unless_told_to() {
    let Some(sb) = sandbox() else { return };
    if !network() {
        return;
    }
    sb.ok(&["install", "jq"]);
    let onig_version = api_version(&sb, "oniguruma");

    let refusal = sb.fails(&["uninstall", "oniguruma"]);
    let onig_keg = sb.keg("oniguruma", &onig_version);
    assert!(
        refusal.contains(&format!("Refusing to uninstall {}", onig_keg.display())),
        "{refusal}"
    );
    assert!(
        refusal.contains("because it is required by jq, which is currently installed."),
        "{refusal}"
    );
    assert!(
        refusal.contains("You can override this and force removal with:\n  brew uninstall --ignore-dependencies oniguruma"),
        "{refusal}"
    );
    assert!(sb.keg("oniguruma", &onig_version).is_dir());

    let forced = sb.ok(&["uninstall", "--ignore-dependencies", "oniguruma"]);
    assert!(
        forced.contains(&format!("Uninstalling {}...", onig_keg.display())),
        "{forced}"
    );
    assert!(forced.contains("files,"), "{forced}");
    assert!(!sb.prefix.join("Cellar/oniguruma").exists());
    assert!(!sb.prefix.join("opt/oniguruma").is_symlink());
    assert!(!sb.prefix.join("var/homebrew/linked/oniguruma").is_symlink());
}

#[test]
fn reinstalls_and_pins_jq() {
    let Some(sb) = sandbox() else { return };
    if !network() {
        return;
    }
    sb.ok(&["install", "jq"]);
    let version = api_version(&sb, "jq");

    let out = sb.ok(&["reinstall", "jq"]);
    assert!(out.contains("==> Reinstalling jq"), "{out}");
    assert!(out.contains("==> Pouring jq--"), "{out}");
    assert!(sb.keg("jq", &version).join("bin/jq").is_file());
    assert!(sb.prefix.join("bin/jq").is_symlink());
    assert_eq!(
        sb.receipt_json("jq", &version)["installed_on_request"],
        true,
        "reinstall keeps installed_on_request"
    );

    // --- pin / unpin
    sb.ok(&["pin", "jq"]);
    let pin = sb.prefix.join("var/homebrew/pinned/jq");
    assert!(pin.is_symlink(), "pin record missing");
    assert_eq!(
        std::fs::read_link(&pin).unwrap(),
        PathBuf::from(format!("../../../Cellar/jq/{version}"))
    );
    let again = sb.ok(&["pin", "jq"]);
    assert!(again.contains("Warning: jq already pinned"), "{again}");
    assert!(sb.ok(&["list", "--pinned"]).contains("jq"));

    sb.ok(&["unpin", "jq"]);
    assert!(!pin.is_symlink());
    let not_pinned = sb.ok(&["unpin", "jq"]);
    assert!(
        not_pinned.contains("Warning: jq not pinned"),
        "{not_pinned}"
    );
}

#[test]
fn upgrades_jq_from_a_simulated_older_keg() {
    let Some(sb) = sandbox() else { return };
    if !network() {
        return;
    }
    sb.ok(&["install", "jq"]);
    let new_version = api_version(&sb, "jq");
    let old_version = "1.8.1";
    assert_ne!(new_version, old_version, "the fixture assumes jq moved on");

    // Pretend the older release is what is installed: unlink, rename the keg,
    // correct the receipt's version fields, link it again.
    sb.ok(&["unlink", "jq"]);
    std::fs::rename(sb.keg("jq", &new_version), sb.keg("jq", old_version)).unwrap();
    let receipt_path = sb.keg("jq", old_version).join("INSTALL_RECEIPT.json");
    let mut receipt: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&receipt_path).unwrap()).unwrap();
    receipt["source"]["versions"]["stable"] = serde_json::json!(old_version);
    std::fs::write(
        &receipt_path,
        serde_json::to_string_pretty(&receipt).unwrap(),
    )
    .unwrap();
    sb.ok(&["link", "jq"]);

    let outdated = sb.ok(&["outdated", "--verbose"]);
    assert!(outdated.contains("jq"), "{outdated}");
    assert!(outdated.contains(old_version), "{outdated}");

    let dry = sb.ok(&["upgrade", "-n", "jq"]);
    assert!(dry.contains("Would upgrade 1 outdated package:"), "{dry}");
    assert!(
        dry.contains(&format!("jq {old_version} -> {new_version}")),
        "{dry}"
    );
    assert!(
        sb.keg("jq", old_version).is_dir(),
        "--dry-run changes nothing"
    );

    // A pinned formula named explicitly is refused and fails the run.
    sb.ok(&["pin", "jq"]);
    let refused = sb.fails(&["upgrade", "jq"]);
    assert!(
        refused.contains("Not upgrading 1 pinned package:"),
        "{refused}"
    );
    assert!(refused.contains(&format!("jq {new_version}")), "{refused}");
    assert!(sb.keg("jq", old_version).is_dir(), "{refused}");
    // A bare `upgrade` only warns about it.
    let warned = sb.ok(&["upgrade"]);
    assert!(
        warned.contains("Warning: Not upgrading 1 pinned package:"),
        "{warned}"
    );
    sb.ok(&["unpin", "jq"]);

    let out = sb.ok(&["upgrade", "jq"]);
    assert!(out.contains("==> Upgrading 1 outdated package:"), "{out}");
    assert!(out.contains("==> Upgrading jq"), "{out}");
    assert!(
        out.contains(&format!("{old_version} -> {new_version}")),
        "{out}"
    );
    assert!(sb.keg("jq", &new_version).is_dir());
    assert!(
        !sb.keg("jq", old_version).exists(),
        "the old keg is cleaned up:\n{out}"
    );
    assert!(out.contains("==> Running `brew cleanup jq`..."), "{out}");
    assert!(
        out.contains("Disable this behaviour by setting `HOMEBREW_NO_INSTALL_CLEANUP=1`."),
        "{out}"
    );
    assert_eq!(
        std::fs::read_link(sb.prefix.join("var/homebrew/linked/jq")).unwrap(),
        PathBuf::from(format!("../../../Cellar/jq/{new_version}"))
    );
    assert_eq!(
        run_binary(&sb.prefix.join("bin/jq"), &["--version"]),
        format!("jq-{new_version}")
    );
}

#[test]
fn autoremoves_orphaned_dependencies_and_cleans_the_cache() {
    let Some(sb) = sandbox() else { return };
    if !network() {
        return;
    }
    sb.ok(&["install", "jq"]);
    let jq_version = api_version(&sb, "jq");
    sb.ok(&["uninstall", "jq"]);

    let dry = sb.ok(&["autoremove", "-n"]);
    assert!(
        dry.contains("Would autoremove 1 unneeded formula:"),
        "{dry}"
    );
    assert!(dry.contains("oniguruma"), "{dry}");
    assert!(sb.prefix.join("Cellar/oniguruma").is_dir());

    let out = sb.ok(&["autoremove"]);
    assert!(
        out.contains("==> Autoremoving 1 unneeded formula:"),
        "{out}"
    );
    assert!(out.contains("Uninstalling "), "{out}");
    assert!(!sb.prefix.join("Cellar/oniguruma").exists());

    // --- `cleanup` on a stale cached bottle of a formula that is gone.
    let stale = sb.cache.join("jq--0.0.1");
    std::fs::write(&stale, b"not really a bottle").unwrap();
    let preview = sb.ok(&["cleanup", "-n"]);
    assert!(
        preview.contains(&format!("Would remove: {}", stale.display())),
        "{preview}"
    );
    assert!(
        preview.contains("This operation would free approximately"),
        "{preview}"
    );
    assert!(stale.is_file(), "--dry-run removes nothing");

    let cleaned = sb.ok(&["cleanup"]);
    assert!(
        cleaned.contains(&format!("Removing: {}...", stale.display())),
        "{cleaned}"
    );
    assert!(
        cleaned.contains("This operation has freed approximately"),
        "{cleaned}"
    );
    assert!(!stale.exists());
    // The real jq bottle stays: it is still the current version.
    let keep = sb.cache.join(format!("jq--{jq_version}"));
    assert!(keep.exists(), "the current bottle must survive cleanup");
}

// ---------------------------------------------------------------------------
// Unit-level tests that need no network
// ---------------------------------------------------------------------------

#[test]
fn builds_a_receipt_from_a_bottle_tab() {
    use fastbrew::bottle::BottleTab;
    use fastbrew::config::Config;
    use fastbrew::model::{FormulaEntry, RuntimeDependency};
    use fastbrew::ops::receipt::{self, ReceiptArgs};

    let tmp = TempDir::new().unwrap();
    let cfg = Config::for_test(tmp.path());
    let tab = BottleTab {
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
        built_on: Some(serde_json::json!({
            "os": "Macintosh", "os_version": "macOS 26", "cpu_family": "dunno",
            "xcode": "26.4", "clt": "26.4.0.0.1774242506", "preferred_perl": "5.34"
        })),
    };
    let formula = FormulaEntry {
        name: "jq".into(),
        tap: "homebrew/core".into(),
        stable_version: Some("1.8.2".into()),
        ..Default::default()
    };
    let built = receipt::build(
        &cfg,
        ReceiptArgs {
            formula: &formula,
            tab: &tab,
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
    let text = built.to_json_string();
    // The tab carries `linkage_files`, so it shows up in its documented slot.
    assert_eq!(
        receipt_keys(&text)
            .iter()
            .position(|k| k == "linkage_files"),
        Some(9),
        "{text}"
    );
    assert_eq!(mandatory_receipt_keys(&text), RECEIPT_KEYS, "{text}");
    // Build-time fields are the tab's, install-time fields ours.
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["homebrew_version"], "6.0.22");
    assert_eq!(value["source_modified_time"], 1_773_700_688u64);
    assert_eq!(value["built_on"]["xcode"], "26.4");
    assert_eq!(value["time"], 1_778_031_361u64);
    assert_eq!(value["changed_files"][0], "lib/pkgconfig/libjq.pc");
}

#[test]
fn install_etc_var_writes_defaults_beside_modified_configs() {
    use fastbrew::config::Config;
    use fastbrew::keg::Keg;
    use fastbrew::ops::postinstall::install_etc_var;

    let tmp = TempDir::new().unwrap();
    let cfg = Config::for_test(tmp.path());
    let seed = |version: &str, body: &str| {
        let keg = Keg::new(&cfg, "demo", version);
        let etc = keg.path.join(".bottle/etc/demo");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(etc.join("demo.conf"), body).unwrap();
        keg
    };

    // A first install seeds the file verbatim.
    let one = seed("1.0", "a = 1\n");
    install_etc_var(&cfg, &one).unwrap();
    let conf = cfg.prefix.join("etc/demo/demo.conf");
    assert_eq!(std::fs::read_to_string(&conf).unwrap(), "a = 1\n");

    // A second install with the same default is a no-op.
    install_etc_var(&cfg, &one).unwrap();
    assert!(!cfg.prefix.join("etc/demo/demo.conf.default").exists());

    // A new default with the live file still untouched advances it in place.
    let two = seed("2.0", "a = 2\n");
    install_etc_var(&cfg, &two).unwrap();
    assert_eq!(std::fs::read_to_string(&conf).unwrap(), "a = 2\n");
    assert!(!cfg.prefix.join("etc/demo/demo.conf.default").exists());

    // Once the user edits the file, the new default lands as `.default`.
    std::fs::write(&conf, "a = 99\n").unwrap();
    let three = seed("3.0", "a = 3\n");
    install_etc_var(&cfg, &three).unwrap();
    assert_eq!(std::fs::read_to_string(&conf).unwrap(), "a = 99\n");
    assert_eq!(
        std::fs::read_to_string(cfg.prefix.join("etc/demo/demo.conf.default")).unwrap(),
        "a = 3\n"
    );
}
