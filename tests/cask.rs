//! End-to-end cask tests.
//!
//! Both tests run entirely inside the sandbox `scripts/sandbox.sh` creates
//! (`$FASTBREW_SANDBOX`), or a private tree under `target/` when the suite is
//! run outside it. Nothing here touches `/opt/homebrew`, `/Applications` or the
//! real `~/Library`.
//!
//! - `installs_and_uninstalls_a_local_cask` needs no network: the container is
//!   written straight into the download cache at the path `download_cask` would
//!   use, so the download step is a cache hit.
//! - `installs_and_uninstalls_rectangle` is gated on `FASTBREW_TEST_NETWORK=1`
//!   and exercises the real dmg pipeline.

use std::path::{Path, PathBuf};
use std::process::Command;

use fastbrew::api::index::Index;
use fastbrew::cask::artifacts;
use fastbrew::cask::config::CaskDirs;
use fastbrew::cask::install::{CaskInstallOptions, install_cask_entry};
use fastbrew::cask::uninstall::uninstall_installed_cask;
use fastbrew::cask::{self, download};
use fastbrew::config::Config;
use fastbrew::model::CaskEntry;
use fastbrew::platform::Host;
use serde_json::{Value, json};

// ------------------------------------------------------------ fixtures

/// Root of the sandbox: `$FASTBREW_SANDBOX` when running under
/// `scripts/sandbox.sh test`, otherwise a private tree under `target/`.
fn sandbox_root() -> PathBuf {
    let root = match std::env::var_os("FASTBREW_SANDBOX") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/cask-sandbox"),
    };
    for sub in [
        "prefix/bin",
        "prefix/Caskroom",
        "prefix/etc",
        "prefix/share",
        "prefix/var/homebrew/locks",
        "cache/downloads",
        "cache/Cask",
        "home/Applications",
        "home/Library/Fonts",
        "tmp",
        "logs",
    ] {
        std::fs::create_dir_all(root.join(sub)).expect("create sandbox tree");
    }
    assert!(
        !root.starts_with("/opt/homebrew") && !root.starts_with("/usr/local"),
        "refusing to run against a real Homebrew prefix: {}",
        root.display()
    );
    root
}

/// A `Config` for the sandbox, built without touching the process environment
/// so the tests stay independent of how they were launched.
fn sandbox_config(root: &Path) -> Config {
    let prefix = root.join("prefix");
    let home = root.join("home");
    Config {
        cellar: prefix.join("Cellar"),
        repository: prefix.clone(),
        library: prefix.join("Library"),
        prefix,
        cache: root.join("cache"),
        logs: root.join("logs"),
        temp: root.join("tmp"),
        api_domain: fastbrew::config::DEFAULT_API_DOMAIN.to_string(),
        bottle_domain: fastbrew::config::DEFAULT_BOTTLE_DOMAIN.to_string(),
        artifact_domain: None,
        github_packages_token: None,
        github_packages_user: None,
        no_auto_update: true,
        auto_update_secs: 86_400,
        api_auto_update_secs: 450,
        no_install_cleanup: true,
        no_install_upgrade: false,
        no_installed_dependents_check: false,
        no_emoji: false,
        install_badge: "🍺".to_string(),
        no_env_hints: true,
        verbose: false,
        debug: false,
        download_concurrency: 1,
        cleanup_max_age_days: 120,
        curl_retries: 3,
        cask_opts: vec![
            format!("--appdir={}/Applications", home.display()),
            format!("--fontdir={}/Library/Fonts", home.display()),
        ],
        home,
        brew_path: None,
        no_delegate: true,
        require_sandbox: true,
    }
}

fn entry(token: &str, json: Value) -> CaskEntry {
    let mut cask: CaskEntry = serde_json::from_value(json).expect("cask entry parses");
    cask.token = token.to_string();
    cask
}

fn read_json(path: &Path) -> Value {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// Remove any leftovers of `token` from a previous run.
fn reset(cfg: &Config, dirs: &CaskDirs, token: &str, app: &str) {
    let _ = std::fs::remove_dir_all(cask::caskroom_path(cfg, token));
    let _ = std::fs::remove_dir_all(dirs.appdir.join(app));
    let _ = std::fs::remove_file(dirs.appdir.join(app));
}

// ------------------------------------------------------ local cask test

/// Build `Demo.app` plus a `bin/demo-cli` helper and zip them with `ditto`.
fn build_demo_zip(root: &Path) -> PathBuf {
    let staging = root.join("tmp/demo-src");
    let _ = std::fs::remove_dir_all(&staging);
    let macos = staging.join("Demo.app/Contents/MacOS");
    std::fs::create_dir_all(&macos).unwrap();
    std::fs::create_dir_all(staging.join("bin")).unwrap();
    std::fs::write(
        staging.join("Demo.app/Contents/Info.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>org.fastbrew.demo</string>
<key>CFBundleShortVersionString</key><string>1.0</string>
<key>CFBundleVersion</key><string>1</string>
</dict></plist>
"#,
    )
    .unwrap();
    std::fs::write(macos.join("demo"), "#!/bin/sh\necho fastbrew-demo\n").unwrap();
    std::fs::write(staging.join("bin/demo-cli"), "#!/bin/sh\necho demo-cli\n").unwrap();

    let zip = root.join("tmp/fastbrew-demo-1.0.zip");
    let _ = std::fs::remove_file(&zip);
    let status = Command::new("ditto")
        .args(["-c", "-k", "--sequesterRsrc"])
        .arg(&staging)
        .arg(&zip)
        .status()
        .expect("run ditto");
    assert!(status.success(), "ditto failed to build the fixture zip");
    zip
}

#[test]
fn installs_and_uninstalls_a_local_cask() {
    let root = sandbox_root();
    let cfg = sandbox_config(&root);
    let dirs = CaskDirs::resolve(&cfg, &[]);
    let token = "fastbrew-demo";
    reset(&cfg, &dirs, token, "Demo.app");

    let url = "https://example.invalid/fastbrew/fastbrew-demo-1.0.zip";
    let zip = build_demo_zip(&root);
    let sha256 = download::file_sha256(&zip).unwrap();

    // Seed the shared download cache at exactly the path `download_cask` uses,
    // so the install is a cache hit and the test never reaches the network.
    let cached = download::cached_location(&cfg, url, "fastbrew-demo-1.0.zip");
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
    std::fs::copy(&zip, &cached).unwrap();

    let zap_dir = cfg.home.join("Library/Application Support/fastbrew-demo");
    std::fs::create_dir_all(&zap_dir).unwrap();
    std::fs::write(zap_dir.join("state.json"), b"{}").unwrap();

    let cask = entry(
        token,
        json!({
            "homepage": "https://example.invalid/",
            "names": ["Demo"],
            "tap_string": "homebrew/cask",
            "version": "1.0",
            "url_args": [url],
            "sha256": sha256,
            "raw_artifacts": [
                [":app", ["Demo.app"]],
                [":binary", ["$APPDIR/Demo.app/Contents/MacOS/demo", {":target": "demo"}]],
                [":zap", {":trash": ["~/Library/Application Support/fastbrew-demo"]}]
            ]
        }),
    );

    let opts = CaskInstallOptions::default();
    install_cask_entry(&cfg, None, &dirs, &cask, &opts).expect("install the demo cask");

    // The app moved into the sandbox app directory ...
    let app = dirs.appdir.join("Demo.app");
    assert!(
        app.join("Contents/MacOS/demo").is_file(),
        "app was not moved into {}",
        dirs.appdir.display()
    );
    // ... and left a symlink behind in the staged directory.
    let staged = cask::staged_path(&cfg, token, "1.0");
    assert!(staged.join("Demo.app").is_symlink());
    assert_eq!(std::fs::read_link(staged.join("Demo.app")).unwrap(), app);
    assert!(staged.join("bin/demo-cli").is_file());

    // The binary artifact is linked into the prefix.
    let binary = cfg.prefix.join("bin/demo");
    assert!(binary.is_symlink(), "{} is not a symlink", binary.display());
    assert_eq!(
        std::fs::read_link(&binary).unwrap(),
        app.join("Contents/MacOS/demo")
    );

    // Caskroom metadata.
    let installed = cask::installed_cask(&cfg, token).expect("cask is discoverable");
    assert_eq!(installed.version, "1.0");
    let caskfile = installed.caskfile_path().expect("caskfile written");
    assert!(
        caskfile.ends_with(format!("Casks/{token}.json")),
        "unexpected caskfile {}",
        caskfile.display()
    );
    assert_eq!(read_json(&caskfile), json!({}));

    let config_json = read_json(&installed.config_path());
    assert_eq!(
        config_json["default"]["appdir"].as_str(),
        Some("/Applications")
    );
    assert_eq!(
        config_json["env"]["appdir"].as_str(),
        Some(dirs.appdir.to_string_lossy().as_ref())
    );
    assert!(config_json["explicit"].is_object());

    let receipt = read_json(&installed.receipt_path());
    assert_eq!(
        receipt["homebrew_version"].as_str(),
        Some(fastbrew::HOMEBREW_COMPAT_VERSION)
    );
    assert_eq!(receipt["loaded_from_api"], json!(true));
    assert_eq!(receipt["installed_on_request"], json!(true));
    assert_eq!(receipt["source"]["tap"], json!("homebrew/cask"));
    assert_eq!(receipt["source"]["version"], json!("1.0"));
    assert_eq!(receipt["runtime_dependencies"], json!({}));
    assert_eq!(
        receipt["uninstall_artifacts"],
        json!([
            {"app": ["Demo.app"]},
            {"binary": [
                format!("{}/Demo.app/Contents/MacOS/demo", dirs.appdir.display()),
                {"target": "demo"}
            ]},
            {"zap": [{"trash": ["~/Library/Application Support/fastbrew-demo"]}]}
        ])
    );
    assert!(receipt["built_on"].is_object());

    // Uninstall with --zap removes everything, including the zapped state.
    uninstall_installed_cask(&cfg, &dirs, &installed, Some(&cask), true, false)
        .expect("uninstall the demo cask");

    assert!(!app.exists(), "{} survived the uninstall", app.display());
    assert!(!binary.exists() && !binary.is_symlink());
    assert!(!cask::caskroom_path(&cfg, token).exists());
    assert!(cask::installed_cask(&cfg, token).is_none());
    assert!(
        !zap_dir.exists(),
        "zap did not remove {}",
        zap_dir.display()
    );
    // The trashed directory landed in the sandbox home's Trash.
    assert!(cfg.home.join(".Trash/fastbrew-demo").exists());
    let _ = std::fs::remove_dir_all(cfg.home.join(".Trash/fastbrew-demo"));
}

#[test]
fn artifact_specs_match_the_receipt_shape() {
    let root = sandbox_root();
    let cfg = sandbox_config(&root);
    let dirs = CaskDirs::resolve(&cfg, &[]);
    let cask = entry(
        "fastbrew-shape",
        json!({
            "version": "2.0",
            "raw_artifacts": [
                [":pkg", ["Demo.pkg", {":allow_untrusted": true}]],
                [":app", ["Demo.app"]],
                [":uninstall", {":pkgutil": "org.fastbrew.demo"}]
            ]
        }),
    );
    let specs = artifacts::artifact_specs(&cfg, &dirs, &cask);
    // `uninstall` runs first, then `pkg`, then the moved artifacts.
    assert_eq!(
        specs.iter().map(|s| s.kind.as_str()).collect::<Vec<_>>(),
        ["uninstall", "pkg", "app"]
    );
    // `pkg` has no uninstall phase, so it is not recorded in the receipt.
    assert_eq!(
        Value::Array(artifacts::specs_to_json(&specs)),
        json!([
            {"uninstall": [{"pkgutil": "org.fastbrew.demo"}]},
            {"app": ["Demo.app"]}
        ])
    );
}

// ------------------------------------------------------- network test

/// The `rectangle` entry exactly as the sandbox's API file serves it, so the
/// test follows the cask's current version and checksum instead of a pinned
/// release. `None` when the sandbox has no cached API file.
fn rectangle_entry(cfg: &Config) -> Option<CaskEntry> {
    let index = Index::load(cfg, &Host::detect().bottle_tag()).ok()?;
    index.cask("rectangle")
}

/// The file name Homebrew resolves a cask download to: the last path segment
/// of its URL.
fn url_basename(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or(url)
}

#[test]
fn installs_and_uninstalls_rectangle() {
    if std::env::var("FASTBREW_TEST_NETWORK").as_deref() != Ok("1") {
        eprintln!("skipping: set FASTBREW_TEST_NETWORK=1 to run the rectangle cask test");
        return;
    }
    // Never drive AppleScript from a test: `uninstall login_item` would ask
    // System Events for Automation access.
    // SAFETY: integration tests run with `--test-threads=1` inside the sandbox.
    unsafe { std::env::set_var("FASTBREW_NO_CASK_AUTOMATION", "1") };

    let root = sandbox_root();
    let cfg = sandbox_config(&root);
    let dirs = CaskDirs::resolve(&cfg, &[]);
    let Some(cask) = rectangle_entry(&cfg) else {
        eprintln!("skipping: no cached API file; run through scripts/sandbox.sh");
        return;
    };
    let version = cask.version.clone().expect("rectangle is versioned");
    reset(&cfg, &dirs, &cask.token, "Rectangle.app");

    install_cask_entry(&cfg, None, &dirs, &cask, &CaskInstallOptions::default())
        .expect("install rectangle");

    let app = dirs.appdir.join("Rectangle.app");
    assert!(
        app.join("Contents/Info.plist").is_file(),
        "Rectangle.app is missing from {}",
        dirs.appdir.display()
    );
    let installed = cask::installed_cask(&cfg, &cask.token).expect("rectangle is installed");
    assert_eq!(installed.version, version);
    let receipt = read_json(&installed.receipt_path());
    assert_eq!(receipt["source"]["version"], json!(version));
    assert!(
        receipt["uninstall_artifacts"]
            .as_array()
            .expect("an array")
            .contains(&json!({"app": ["Rectangle.app"]})),
        "{}",
        receipt["uninstall_artifacts"]
    );

    // The download is cached under Homebrew's shared naming.
    let url = cask.url().unwrap();
    let cached = download::cached_location(&cfg, url, url_basename(url));
    assert!(cached.is_file(), "{} is missing", cached.display());
    assert!(
        cfg.cache
            .join(format!("Cask/rectangle--{version}.dmg"))
            .is_symlink()
    );

    uninstall_installed_cask(&cfg, &dirs, &installed, Some(&cask), true, false)
        .expect("uninstall rectangle");
    assert!(!app.exists(), "{} survived the uninstall", app.display());
    assert!(cask::installed_cask(&cfg, &cask.token).is_none());
}
