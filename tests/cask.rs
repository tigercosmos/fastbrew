//! End-to-end cask tests.
//!
//! Every test runs entirely inside the sandbox `scripts/sandbox.sh` creates
//! (`$FASTBREW_SANDBOX`), a private tree under `target/` when the suite is run
//! outside it, or the temporary prefix of a [`support::Sandbox`]. Nothing here
//! touches `/opt/homebrew`, `/Applications` or the real `~/Library`.
//!
//! - The fake-cask tests need no network: the container is written straight
//!   into the download cache at the path `download_cask` would use, so the
//!   download step is a cache hit. A cask whose container is *not* cached and
//!   whose url points at a dead local port stands in for a failed download.
//! - `installs_and_uninstalls_rectangle` is gated on `FASTBREW_TEST_NETWORK=1`
//!   and exercises the real dmg pipeline.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use fastbrew::api::index::Index;
use fastbrew::cask::artifacts;
use fastbrew::cask::config::CaskDirs;
use fastbrew::cask::install::{CaskInstallOptions, install_cask_entry, upgrade_cask_entry};
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

// ------------------------------------------------ replace/upgrade fixtures

/// Build `app` carrying `version` in its executable and zip it with `ditto`.
fn build_versioned_zip(root: &Path, token: &str, app: &str, version: &str) -> PathBuf {
    let staging = root.join(format!("tmp/{token}-{version}-src"));
    let _ = std::fs::remove_dir_all(&staging);
    let bundle = staging.join(app);
    std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
    std::fs::write(
        bundle.join("Contents/Info.plist"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>org.fastbrew.{token}</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
<key>CFBundleVersion</key><string>{version}</string>
</dict></plist>
"#
        ),
    )
    .unwrap();
    std::fs::write(
        bundle.join("Contents/MacOS/demo"),
        format!("#!/bin/sh\necho {version}\n"),
    )
    .unwrap();

    let zip = root.join(format!("tmp/{token}-{version}.zip"));
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

/// The url a fake cask version is served from. It is never fetched: the
/// container is seeded into the cache under exactly this url's cache path.
fn fake_url(token: &str, version: &str) -> String {
    format!("https://example.invalid/fastbrew/{token}-{version}.zip")
}

/// A url nothing listens on, for an upgrade whose download must fail.
fn dead_url(token: &str, version: &str) -> String {
    format!("http://127.0.0.1:9/{token}-{version}.zip")
}

/// A cask entry with a single `app` artifact.
fn app_entry(token: &str, app: &str, version: &str, url: &str, sha256: &str) -> CaskEntry {
    entry(
        token,
        json!({
            "homepage": "https://example.invalid/",
            "names": [app.trim_end_matches(".app")],
            "tap_string": "homebrew/cask",
            "version": version,
            "url_args": [url],
            "sha256": sha256,
            "raw_artifacts": [[":app", [app]]]
        }),
    )
}

/// Build `version` of `token`, seed the download cache with it, and return the
/// entry that installs it straight from the cache.
fn cached_app_entry(cfg: &Config, root: &Path, token: &str, app: &str, version: &str) -> CaskEntry {
    let zip = build_versioned_zip(root, token, app, version);
    let url = fake_url(token, version);
    let cached = download::cached_location(cfg, &url, &format!("{token}-{version}.zip"));
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
    std::fs::copy(&zip, &cached).unwrap();
    app_entry(
        token,
        app,
        version,
        &url,
        &download::file_sha256(&zip).unwrap(),
    )
}

/// Sorted names of the entries directly inside `dir`.
fn entries(dir: &Path) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut names: Vec<String> = read
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Sorted relative paths of everything under `root`.
fn tree(root: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read.flatten() {
            let path = entry.path();
            out.push(
                path.strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned(),
            );
            if path.is_dir() && !path.is_symlink() {
                walk(base, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

/// The version `build_versioned_zip` wrote into the installed app.
fn app_version(app: &Path) -> String {
    let script = std::fs::read_to_string(app.join("Contents/MacOS/demo"))
        .unwrap_or_else(|e| panic!("read {}: {e}", app.display()));
    script
        .trim()
        .rsplit(' ')
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Caskroom entries a backed-up predecessor would leave behind.
fn backup_leftovers(caskroom: &Path) -> Vec<String> {
    tree(caskroom)
        .into_iter()
        .filter(|name| name.contains(".upgrading") || name.contains(".backup"))
        .collect()
}

/// Install `version` of a fake cask from the seeded cache.
fn install_fake(cfg: &Config, root: &Path, dirs: &CaskDirs, token: &str, app: &str, version: &str) {
    let cask = cached_app_entry(cfg, root, token, app, version);
    install_cask_entry(cfg, None, dirs, &cask, &CaskInstallOptions::default())
        .unwrap_or_else(|e| panic!("install {token} {version}: {e}"));
}

// --------------------------------------------------- dry run and rollback

#[test]
fn dry_run_install_of_an_outdated_cask_changes_nothing() {
    let root = sandbox_root();
    let cfg = sandbox_config(&root);
    let dirs = CaskDirs::resolve(&cfg, &[]);
    let token = "fastbrew-dry-upgrade";
    let app = "FastbrewDryUpgrade.app";
    reset(&cfg, &dirs, token, app);

    install_fake(&cfg, &root, &dirs, token, app, "1.0");
    let caskroom = cask::caskroom_path(&cfg, token);
    let app_path = dirs.appdir.join(app);
    let before = tree(&caskroom);

    // 2.0 is neither cached nor reachable: a dry run must not fetch it, and
    // must not start the upgrade that would remove 1.0.
    let outdated = app_entry(token, app, "2.0", &dead_url(token, "2.0"), &"0".repeat(64));
    let opts = CaskInstallOptions {
        dry_run: true,
        ..CaskInstallOptions::default()
    };
    install_cask_entry(&cfg, None, &dirs, &outdated, &opts).expect("the dry run succeeds");

    assert_eq!(tree(&caskroom), before, "the dry run changed the Caskroom");
    assert_eq!(cask::installed_cask(&cfg, token).unwrap().version, "1.0");
    assert_eq!(app_version(&app_path), "1.0");
    reset(&cfg, &dirs, token, app);
}

#[test]
fn a_failed_upgrade_download_keeps_the_installed_version() {
    let root = sandbox_root();
    let cfg = sandbox_config(&root);
    let dirs = CaskDirs::resolve(&cfg, &[]);
    let token = "fastbrew-failed-download";
    let app = "FastbrewFailedDownload.app";
    reset(&cfg, &dirs, token, app);

    install_fake(&cfg, &root, &dirs, token, app, "1.0");
    let caskroom = cask::caskroom_path(&cfg, token);
    let app_path = dirs.appdir.join(app);
    let before = tree(&caskroom);

    // Nothing is cached for 2.0 and nothing answers on the url.
    let outdated = app_entry(token, app, "2.0", &dead_url(token, "2.0"), &"0".repeat(64));
    let error = install_cask_entry(&cfg, None, &dirs, &outdated, &CaskInstallOptions::default())
        .expect_err("the upgrade must fail");
    assert!(
        error.to_string().contains("Failed to download"),
        "unexpected error: {error}"
    );

    // The working 1.0 install survived the failure untouched.
    assert!(app_path.is_dir(), "{} was removed", app_path.display());
    assert_eq!(app_version(&app_path), "1.0");
    assert_eq!(cask::installed_cask(&cfg, token).unwrap().version, "1.0");
    assert_eq!(tree(&caskroom), before);
    assert!(
        backup_leftovers(&caskroom).is_empty(),
        "backup directories left behind: {:?}",
        backup_leftovers(&caskroom)
    );
    reset(&cfg, &dirs, token, app);
}

#[test]
fn a_failed_upgrade_restores_the_previous_version() {
    let root = sandbox_root();
    let cfg = sandbox_config(&root);
    let dirs = CaskDirs::resolve(&cfg, &[]);
    let token = "fastbrew-failed-stage";
    let app = "FastbrewFailedStage.app";
    reset(&cfg, &dirs, token, app);

    install_fake(&cfg, &root, &dirs, token, app, "1.0");
    let caskroom = cask::caskroom_path(&cfg, token);
    let app_path = dirs.appdir.join(app);

    // A cached "container" that is not an archive: the download and its
    // checksum pass, and the install fails once the predecessor has been moved
    // aside, which is exactly when the rollback has to run.
    let url = fake_url(token, "2.0");
    let cached = download::cached_location(&cfg, &url, &format!("{token}-2.0.zip"));
    std::fs::create_dir_all(cached.parent().unwrap()).unwrap();
    std::fs::write(&cached, b"not an archive\n").unwrap();
    let broken = app_entry(
        token,
        app,
        "2.0",
        &url,
        &download::file_sha256(&cached).unwrap(),
    );

    install_cask_entry(&cfg, None, &dirs, &broken, &CaskInstallOptions::default())
        .expect_err("the upgrade must fail");

    // The predecessor is back in place and still installed.
    assert!(app_path.is_dir(), "{} was not restored", app_path.display());
    assert_eq!(app_version(&app_path), "1.0");
    let installed = cask::installed_cask(&cfg, token).expect("1.0 is still installed");
    assert_eq!(installed.version, "1.0");
    assert!(installed.staged_path().join(app).is_symlink());
    assert!(installed.metadata_versioned_path().is_dir());
    assert!(
        backup_leftovers(&caskroom).is_empty(),
        "backup directories left behind: {:?}",
        backup_leftovers(&caskroom)
    );
    assert!(!caskroom.join("2.0").exists(), "2.0 was left staged");
    reset(&cfg, &dirs, token, app);
    let _ = std::fs::remove_file(&cached);
}

#[test]
fn a_successful_upgrade_leaves_one_version() {
    let root = sandbox_root();
    let cfg = sandbox_config(&root);
    let dirs = CaskDirs::resolve(&cfg, &[]);
    let token = "fastbrew-upgrade";
    let app = "FastbrewUpgrade.app";
    reset(&cfg, &dirs, token, app);

    install_fake(&cfg, &root, &dirs, token, app, "1.0");
    install_fake(&cfg, &root, &dirs, token, app, "2.0");

    let caskroom = cask::caskroom_path(&cfg, token);
    let app_path = dirs.appdir.join(app);
    assert_eq!(app_version(&app_path), "2.0");
    let installed = cask::installed_cask(&cfg, token).expect("2.0 is installed");
    assert_eq!(installed.version, "2.0");
    assert!(installed.staged_path().join(app).is_symlink());

    // Exactly one staged version and one versioned metadata directory.
    assert_eq!(entries(&caskroom), [".metadata", "2.0"]);
    assert_eq!(
        entries(&caskroom.join(".metadata")),
        ["2.0", "INSTALL_RECEIPT.json", "config.json"]
    );
    assert!(backup_leftovers(&caskroom).is_empty());
    reset(&cfg, &dirs, token, app);
}

#[test]
fn reinstalling_the_same_version_keeps_one_version() {
    let root = sandbox_root();
    let cfg = sandbox_config(&root);
    let dirs = CaskDirs::resolve(&cfg, &[]);
    let token = "fastbrew-reinstall";
    let app = "FastbrewReinstall.app";
    reset(&cfg, &dirs, token, app);

    install_fake(&cfg, &root, &dirs, token, app, "1.0");
    let caskroom = cask::caskroom_path(&cfg, token);
    let app_path = dirs.appdir.join(app);

    // `reinstall` replaces the installed version through the same backup and
    // purge as an upgrade, so the Caskroom ends up exactly as it started.
    let cask = cached_app_entry(&cfg, &root, token, app, "1.0");
    let opts = CaskInstallOptions {
        reinstall: true,
        ..CaskInstallOptions::default()
    };
    install_cask_entry(&cfg, None, &dirs, &cask, &opts).expect("reinstall 1.0");

    assert!(app_path.is_dir(), "{} was removed", app_path.display());
    assert_eq!(app_version(&app_path), "1.0");
    let installed = cask::installed_cask(&cfg, token).expect("1.0 is installed");
    assert_eq!(installed.version, "1.0");
    assert!(installed.staged_path().join(app).is_symlink());
    assert_eq!(entries(&caskroom), [".metadata", "1.0"]);
    assert_eq!(
        entries(&caskroom.join(".metadata")),
        ["1.0", "INSTALL_RECEIPT.json", "config.json"]
    );
    // The reinstall left exactly one timestamped metadata directory.
    assert_eq!(entries(&caskroom.join(".metadata/1.0")).len(), 1);
    assert!(backup_leftovers(&caskroom).is_empty());
    reset(&cfg, &dirs, token, app);
}

#[test]
fn the_upgrade_entry_point_replaces_the_installed_version() {
    let root = sandbox_root();
    let cfg = sandbox_config(&root);
    let dirs = CaskDirs::resolve(&cfg, &[]);
    let token = "fastbrew-upgrade-entry";
    let app = "FastbrewUpgradeEntry.app";
    reset(&cfg, &dirs, token, app);

    install_fake(&cfg, &root, &dirs, token, app, "1.0");
    let installed = cask::installed_cask(&cfg, token).expect("1.0 is installed");

    // `upgrade --cask` drives this entry point directly.
    let next = cached_app_entry(&cfg, &root, token, app, "2.0");
    upgrade_cask_entry(
        &cfg,
        None,
        &dirs,
        &next,
        &installed,
        &CaskInstallOptions::default(),
    )
    .expect("upgrade to 2.0");

    let caskroom = cask::caskroom_path(&cfg, token);
    assert_eq!(app_version(&dirs.appdir.join(app)), "2.0");
    assert_eq!(cask::installed_cask(&cfg, token).unwrap().version, "2.0");
    assert_eq!(entries(&caskroom), [".metadata", "2.0"]);
    assert!(backup_leftovers(&caskroom).is_empty());
    reset(&cfg, &dirs, token, app);
}

// -------------------------------------------------------------- the CLI

/// Write the Caskroom records and the app of a `version` install of `token`,
/// the way `uninstall` and `install` read them back.
fn fake_cli_install(sandbox: &support::Sandbox, token: &str, version: &str, app: &str) -> PathBuf {
    let apps = sandbox.home.join("Applications");
    let installed_app = apps.join(app);
    std::fs::create_dir_all(installed_app.join("Contents/MacOS")).unwrap();
    std::fs::write(installed_app.join("Contents/MacOS/demo"), "#!/bin/sh\n").unwrap();

    let caskroom = sandbox.prefix.join("Caskroom").join(token);
    let staged = caskroom.join(version);
    std::fs::create_dir_all(&staged).unwrap();
    let link = staged.join(app);
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&installed_app, &link).unwrap();

    let metadata = caskroom.join(fastbrew::cask::METADATA_SUBDIR);
    let timestamped = metadata.join(version).join("20250101000000.000");
    std::fs::create_dir_all(timestamped.join("Casks")).unwrap();
    std::fs::write(
        timestamped.join("Casks").join(format!("{token}.json")),
        "{}",
    )
    .unwrap();
    std::fs::write(
        metadata.join("config.json"),
        serde_json::to_string(&json!({
            "default": {"appdir": "/Applications"},
            "env": {"appdir": apps.to_string_lossy()},
            "explicit": {}
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        metadata.join("INSTALL_RECEIPT.json"),
        serde_json::to_string(&json!({
            "homebrew_version": fastbrew::HOMEBREW_COMPAT_VERSION,
            "loaded_from_api": true,
            "installed_on_request": true,
            "uninstall_artifacts": [{"app": [app]}],
            "source": {"tap": "homebrew/cask", "version": version},
        }))
        .unwrap(),
    )
    .unwrap();
    installed_app
}

/// A CLI sandbox plus the version the seeded API index serves for `token`, or
/// `None` when there is no cached API file to resolve names against.
fn cli_sandbox(token: &str) -> Option<(support::Sandbox, String)> {
    let sandbox = support::Sandbox::new()?;
    let version = support::api_index()?.cask(token)?.version?;
    Some((sandbox, version))
}

#[test]
fn cli_install_cask_dry_run_reports_an_upgrade() {
    let Some((sandbox, version)) = cli_sandbox("rectangle") else {
        eprintln!("no cached Homebrew API file available; skipping");
        return;
    };
    if version == "1.0" {
        eprintln!("skipping: the API index serves rectangle 1.0");
        return;
    }
    let app = fake_cli_install(&sandbox, "rectangle", "1.0", "Rectangle.app");
    let caskroom = sandbox.prefix.join("Caskroom/rectangle");
    let before = tree(&caskroom);

    let out = sandbox
        .cmd()
        .env(
            "HOMEBREW_CASK_OPTS",
            format!("--appdir={}", sandbox.home.join("Applications").display()),
        )
        .args(["install", "--cask", "--dry-run", "rectangle"])
        .output()
        .expect("run fastbrew");
    let stdout = support::strip_ansi(&String::from_utf8_lossy(&out.stdout));
    assert!(
        out.status.success(),
        "install --dry-run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(&format!("Would upgrade rectangle 1.0 -> {version}")),
        "{stdout}"
    );

    assert!(app.is_dir(), "the dry run removed {}", app.display());
    assert_eq!(tree(&caskroom), before, "the dry run changed the Caskroom");
}
