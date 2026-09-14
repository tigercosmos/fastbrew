//! Scenario tests: the cask, tap, services and general-CLI paths a user
//! actually drives, run through the built binary.
//!
//! Every test owns a private prefix, cache-sharing `support::Sandbox` whose
//! `HOME`, app directory and font directory live inside it, so nothing here
//! can reach `/opt/homebrew`, `/Applications` or the real `~/Library`.
//!
//! Casks come from a `file://`-style fixture tap written straight into
//! `Library/Taps`: the CLI resolves them by token exactly as it resolves a
//! core cask, and their containers are seeded into the shared download cache
//! at the path `download_cask` computes, so the install pipeline runs end to
//! end without a network. Tests that do need the network check
//! `FASTBREW_TEST_NETWORK=1` and skip otherwise.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use fastbrew::cask::download;
use fastbrew::config::Config;

// ---------------------------------------------------------------- harness

struct Env {
    sandbox: support::Sandbox,
}

impl Env {
    /// `None` when there is no cached API file to resolve core names against.
    fn new() -> Option<Env> {
        let sandbox = support::Sandbox::new()?;
        let env = Env { sandbox };
        std::fs::create_dir_all(env.appdir()).ok()?;
        std::fs::create_dir_all(env.fontdir()).ok()?;
        std::fs::create_dir_all(env.tap_path().join("Casks")).ok()?;
        Some(env)
    }

    fn appdir(&self) -> PathBuf {
        self.sandbox.home.join("Applications")
    }

    fn fontdir(&self) -> PathBuf {
        self.sandbox.home.join("Library/Fonts")
    }

    fn tap_path(&self) -> PathBuf {
        self.sandbox
            .prefix
            .join("Library/Taps/fixture/homebrew-casks")
    }

    fn caskroom(&self, token: &str) -> PathBuf {
        self.sandbox.prefix.join("Caskroom").join(token)
    }

    /// A `Config` describing this sandbox, for the cache-path helpers the
    /// fixtures need. Only the paths matter here.
    fn config(&self) -> Config {
        let mut cfg = Config::for_test(self.sandbox.prefix.parent().expect("sandbox root"));
        cfg.cache = self.sandbox.cache.clone();
        cfg.home = self.sandbox.home.clone();
        cfg
    }

    fn cmd(&self) -> Command {
        let mut cmd = self.sandbox.cmd();
        cmd.env(
            "HOMEBREW_CASK_OPTS",
            format!(
                "--appdir={} --fontdir={}",
                self.appdir().display(),
                self.fontdir().display()
            ),
        )
        // `uninstall quit`/`login_item` would drive AppleScript; a test must
        // never ask System Events for Automation access.
        .env("FASTBREW_NO_CASK_AUTOMATION", "1");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().expect("run fastbrew")
    }

    /// stdout and stderr joined and de-coloured: `ohai` prints to stdout and
    /// the warnings and errors to stderr.
    fn combined(&self, args: &[&str]) -> String {
        let out = self.run(args);
        support::strip_ansi(&format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ))
    }

    fn stdout(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "`fastbrew {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        support::strip_ansi(&String::from_utf8_lossy(&out.stdout))
    }

    /// Write `Casks/<token>.rb` into the fixture tap.
    fn write_cask(&self, token: &str, body: &str) {
        let path = self.tap_path().join("Casks").join(format!("{token}.rb"));
        std::fs::create_dir_all(path.parent().expect("Casks")).expect("mkdir Casks");
        std::fs::write(&path, body).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }

    /// Build a zipped `<app>` carrying `version`, seed it into the download
    /// cache at the path `download_cask` would use for `url`, and return its
    /// sha256.
    fn seed_app_zip(&self, url: &str, app: &str, version: &str) -> String {
        let zip = build_app_zip(&self.sandbox.home.join("fixtures"), app, version);
        let sha = download::file_sha256(&zip).expect("sha256 of the fixture zip");
        let basename = url.rsplit('/').next().unwrap_or(url);
        let cached = download::cached_location(&self.config(), url, basename);
        std::fs::create_dir_all(cached.parent().expect("downloads")).expect("mkdir downloads");
        std::fs::copy(&zip, &cached).expect("seed the download cache");
        sha
    }

    /// Install `version` of a fixture cask with a single `app` artifact.
    fn install_app_cask(&self, token: &str, app: &str, version: &str, extra: &str) {
        let url = fixture_url(token, version);
        let sha = self.seed_app_zip(&url, app, version);
        self.write_cask(token, &app_cask_rb(token, version, &url, &sha, app, extra));
        let out = self.run(&["install", "--cask", token]);
        assert!(
            out.status.success(),
            "install {token} {version} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The url a fixture version is served from. Nothing ever fetches it: the
/// container is seeded into the cache under exactly this url's cache path.
fn fixture_url(token: &str, version: &str) -> String {
    format!("https://example.invalid/fastbrew/{token}-{version}.zip")
}

/// A cask file with one `app` artifact plus whatever `extra` stanzas the
/// scenario needs.
fn app_cask_rb(
    token: &str,
    version: &str,
    url: &str,
    sha256: &str,
    app: &str,
    extra: &str,
) -> String {
    format!(
        r#"cask "{token}" do
  version "{version}"
  sha256 "{sha256}"

  url "{url}"
  name "{token} fixture"
  desc "Fixture cask for the scenario tests"
  homepage "https://example.invalid/{token}"

  app "{app}"
{extra}
end
"#
    )
}

/// Build `<app>` with `version` in its Info.plist and its executable, zipped
/// the way a cask ships one.
fn build_app_zip(root: &Path, app: &str, version: &str) -> PathBuf {
    let staging = root.join(format!("src-{app}-{version}"));
    let _ = std::fs::remove_dir_all(&staging);
    let bundle = staging.join(app);
    std::fs::create_dir_all(bundle.join("Contents/MacOS")).expect("mkdir bundle");
    std::fs::write(
        bundle.join("Contents/Info.plist"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>org.fastbrew.fixture</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
<key>CFBundleVersion</key><string>{version}</string>
</dict></plist>
"#
        ),
    )
    .expect("write Info.plist");
    std::fs::write(
        bundle.join("Contents/MacOS/demo"),
        format!("#!/bin/sh\necho {version}\n"),
    )
    .expect("write executable");

    let zip = root.join(format!("{app}-{version}.zip"));
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

/// The version `build_app_zip` wrote into the installed app.
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

/// Caskroom entries a backed-up predecessor would leave behind.
fn backup_leftovers(caskroom: &Path) -> Vec<String> {
    tree(caskroom)
        .into_iter()
        .filter(|name| name.contains(".upgrading") || name.contains(".backup"))
        .collect()
}

macro_rules! env_or_skip {
    () => {
        match Env::new() {
            Some(e) => e,
            None => {
                eprintln!("no cached Homebrew API file available; skipping");
                return;
            }
        }
    };
}

// ------------------------------------------------------------- rollback

/// A failure while reversing the predecessor's artifacts is as destructive as
/// a failure during staging: the app has already left the app directory. The
/// revert has to cover it, so an upgrade that dies in the `uninstall` stanza
/// leaves the working version installed.
#[test]
fn a_failed_predecessor_uninstall_restores_the_installed_version() {
    let env = env_or_skip!();
    let token = "fastbrew-uninstall-fails";
    let app = "FastbrewUninstallFails.app";

    // 1.0 installs cleanly; its `uninstall` stanza only runs on the way out.
    env.install_app_cask(
        token,
        app,
        "1.0",
        "  uninstall script: { executable: \"/usr/bin/false\" }\n",
    );
    let app_path = env.appdir().join(app);
    assert_eq!(app_version(&app_path), "1.0");
    let caskroom = env.caskroom(token);
    let before = tree(&caskroom);

    // 2.0 is fetchable, so the upgrade gets as far as reversing 1.0's
    // artifacts, where the uninstall script fails.
    let url = fixture_url(token, "2.0");
    let sha = env.seed_app_zip(&url, app, "2.0");
    env.write_cask(
        token,
        &app_cask_rb(
            token,
            "2.0",
            &url,
            &sha,
            app,
            "  uninstall script: { executable: \"/usr/bin/false\" }\n",
        ),
    );

    let out = env.run(&["upgrade", "--cask", token]);
    assert!(
        !out.status.success(),
        "the upgrade must fail: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let report = support::strip_ansi(&format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ));
    assert!(
        report.contains(&format!("Reverting upgrade for Cask {token}")),
        "the failure has to announce the revert:\n{report}"
    );

    // The working 1.0 install is back, exactly as it was.
    assert!(app_path.is_dir(), "{} was not restored", app_path.display());
    assert_eq!(app_version(&app_path), "1.0");
    assert_eq!(tree(&caskroom), before, "the Caskroom changed");
    assert!(
        backup_leftovers(&caskroom).is_empty(),
        "backup directories left behind: {:?}",
        backup_leftovers(&caskroom)
    );
    assert!(!caskroom.join("2.0").exists(), "2.0 was left staged");

    // ... and it is still the installed version as far as every command is
    // concerned.
    assert_eq!(
        env.stdout(&["list", "--cask", "--versions"]),
        format!("{token} 1.0\n")
    );
    let plan = env.combined(&["upgrade", "--cask", "--dry-run", token]);
    assert!(
        plan.contains(&format!("{token} 1.0 -> 2.0")),
        "1.0 is still what an upgrade would replace:\n{plan}"
    );
}

// ----------------------------------------------------- tap qualification

/// `user/repo/token` has to reach the cask engine as the tap's entry: naming
/// the fixture tap's `rectangle` must plan version 99.0, never core's.
#[test]
fn a_tap_qualified_cask_is_the_one_that_is_installed() {
    let env = env_or_skip!();
    let core_version = match support::api_index().and_then(|i| i.cask("rectangle")) {
        Some(cask) => cask.version.expect("rectangle is versioned"),
        None => {
            eprintln!("the API index has no rectangle cask; skipping");
            return;
        }
    };
    assert_ne!(core_version, "99.0", "the fixture has to shadow core");

    let app = "FastbrewTapRectangle.app";
    let url = fixture_url("rectangle", "99.0");
    let sha = env.seed_app_zip(&url, app, "99.0");
    env.write_cask(
        "rectangle",
        &app_cask_rb("rectangle", "99.0", &url, &sha, app, ""),
    );

    // The bare token is still core's cask; `FromNameLoader` checks it first.
    let core = env.stdout(&["info", "--cask", "rectangle"]);
    assert!(
        core.starts_with(&format!("==> rectangle (Rectangle): {core_version}")),
        "{core}"
    );

    // The qualified token is the tap's.
    let tapped = env.stdout(&["info", "--cask", "fixture/casks/rectangle"]);
    assert!(
        tapped.starts_with("==> fixture/casks/rectangle (rectangle fixture): 99.0"),
        "{tapped}"
    );

    let plan = env.combined(&["install", "--cask", "--dry-run", "fixture/casks/rectangle"]);
    assert!(
        plan.contains("Would install cask fixture/casks/rectangle 99.0"),
        "the plan follows the qualified token:\n{plan}"
    );

    // ... and the install itself uses the tap's entry.
    let out = env.run(&["install", "--cask", "fixture/casks/rectangle"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        env.stdout(&["list", "--cask", "--versions"]),
        "rectangle 99.0\n"
    );
    assert!(env.appdir().join(app).is_dir());

    // The qualified token also reaches `uninstall`.
    let out = env.run(&["uninstall", "--cask", "fixture/casks/rectangle"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!env.appdir().join(app).exists());
    assert_eq!(env.stdout(&["list", "--cask"]), "");
}
