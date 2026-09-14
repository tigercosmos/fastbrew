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

    /// Write an executable `cmd/brew-<name>` into a tap that needs no clone:
    /// every tap lookup fastbrew does is a directory walk.
    fn write_tap_command(&self, tap: &str, name: &str, script: &str) -> PathBuf {
        let (user, repo) = tap.split_once('/').expect("user/repo");
        let dir = self
            .sandbox
            .prefix
            .join("Library/Taps")
            .join(user)
            .join(format!("homebrew-{repo}"))
            .join("cmd");
        std::fs::create_dir_all(&dir).expect("mkdir cmd");
        let path = dir.join(format!("brew-{name}"));
        std::fs::write(&path, script).expect("write the command");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
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
    assert_eq!(
        plan, "==> Would install 1 cask:\nfixture/casks/rectangle\n",
        "the plan follows the qualified token"
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

// ------------------------------------------------------- cask edge cases

/// `--appdir` on the command line beats `HOMEBREW_CASK_OPTS`, and the layers
/// are recorded separately in the cask's `config.json` (`Cask::Config`).
#[test]
fn appdir_on_the_command_line_wins_over_the_environment() {
    let env = env_or_skip!();
    let token = "fastbrew-appdir";
    let app = "FastbrewAppdir.app";
    let elsewhere = env.sandbox.home.join("Elsewhere");
    std::fs::create_dir_all(&elsewhere).expect("mkdir");

    let url = fixture_url(token, "1.0");
    let sha = env.seed_app_zip(&url, app, "1.0");
    env.write_cask(token, &app_cask_rb(token, "1.0", &url, &sha, app, ""));

    let out = env
        .cmd()
        .args([
            "install",
            "--cask",
            &format!("--appdir={}", elsewhere.display()),
            token,
        ])
        .output()
        .expect("run fastbrew");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        elsewhere.join(app).is_dir(),
        "the app went to {} instead",
        env.appdir().display()
    );
    assert!(!env.appdir().join(app).exists());

    // `config.json` keeps the default, the environment and the flag apart.
    let config: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(env.caskroom(token).join(".metadata/config.json"))
            .expect("config.json"),
    )
    .expect("json");
    assert_eq!(
        config["default"]["appdir"],
        serde_json::json!("/Applications")
    );
    assert_eq!(
        config["env"]["appdir"],
        serde_json::json!(env.appdir().to_string_lossy().as_ref())
    );
    assert_eq!(
        config["explicit"]["appdir"],
        serde_json::json!(elsewhere.to_string_lossy().as_ref())
    );

    // The uninstall follows the recorded directory, not the current one.
    let out = env.run(&["uninstall", "--cask", token]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!elsewhere.join(app).exists());
}

/// `--require-sha` (also via `HOMEBREW_CASK_OPTS`) refuses a cask that has no
/// checksum to verify, before anything is downloaded.
#[test]
fn require_sha_refuses_a_cask_without_a_checksum() {
    let env = env_or_skip!();
    let token = "fastbrew-no-check";
    let app = "FastbrewNoCheck.app";
    let url = fixture_url(token, "1.0");
    env.seed_app_zip(&url, app, "1.0");
    // `sha256 :no_check` is the stanza Homebrew refuses under `--require-sha`.
    env.write_cask(
        token,
        &app_cask_rb(token, "1.0", &url, ":no_check", app, ""),
    );

    // Without the switch it installs.
    let out = env.run(&["install", "--cask", token]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = env.run(&["uninstall", "--cask", token]);

    // `cmd/install.rb` reports a cask failure as `<full name>: <error>`.
    let expected = format!(
        "Error: fixture/casks/{token}: Cask '{token}' does not have a sha256 checksum defined.\n\
         This means you have the --require-sha option set, perhaps in your `$HOMEBREW_CASK_OPTS`."
    );
    // As a flag ...
    let out = env.run(&["install", "--cask", "--require-sha", token]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        support::strip_ansi(&String::from_utf8_lossy(&out.stderr)).contains(&expected),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // ... and out of the environment, where Homebrew documents it.
    let out = env
        .cmd()
        .env(
            "HOMEBREW_CASK_OPTS",
            format!("--appdir={} --require-sha", env.appdir().display()),
        )
        .args(["install", "--cask", token])
        .output()
        .expect("run fastbrew");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        support::strip_ansi(&String::from_utf8_lossy(&out.stderr)).contains(&expected),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!env.caskroom(token).exists(), "nothing may be staged");

    // `--force` is Homebrew's escape hatch (`require_sha? && !force?`).
    let out = env.run(&["install", "--cask", "--require-sha", "--force", token]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = env.run(&["uninstall", "--cask", token]);
}

/// `Cask::Installer#prelude` runs before anything is fetched: a macOS
/// requirement the host cannot meet, and a conflicting cask, both stop there.
#[test]
fn requirements_and_conflicts_stop_the_install_before_staging() {
    let env = env_or_skip!();

    // --- depends_on macos ------------------------------------------------
    let token = "fastbrew-too-new";
    let app = "FastbrewTooNew.app";
    let url = fixture_url(token, "1.0");
    let sha = env.seed_app_zip(&url, app, "1.0");
    // A macOS that does not exist yet, so the host can never satisfy it.
    env.write_cask(
        token,
        &app_cask_rb(
            token,
            "1.0",
            &url,
            &sha,
            app,
            "  depends_on macos: \">= :golden_gate\"\n",
        ),
    );
    let out = env.run(&["install", "--cask", token]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = support::strip_ansi(&String::from_utf8_lossy(&out.stderr));
    assert!(
        stderr.contains(&format!(
            "{token}: This cask does not run on macOS versions older than Golden Gate."
        )),
        "{stderr}"
    );
    assert!(!env.caskroom(token).exists(), "nothing may be staged");

    // --- conflicts_with cask ---------------------------------------------
    let first = "fastbrew-conflict-a";
    let second = "fastbrew-conflict-b";
    env.install_app_cask(first, "FastbrewConflictA.app", "1.0", "");

    let url = fixture_url(second, "1.0");
    let sha = env.seed_app_zip(&url, "FastbrewConflictB.app", "1.0");
    env.write_cask(
        second,
        &app_cask_rb(
            second,
            "1.0",
            &url,
            &sha,
            "FastbrewConflictB.app",
            &format!("  conflicts_with cask: \"{first}\"\n"),
        ),
    );
    let out = env.run(&["install", "--cask", second]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = support::strip_ansi(&String::from_utf8_lossy(&out.stderr));
    assert!(
        stderr.contains(&format!("Cask '{second}' conflicts with '{first}'.")),
        "{stderr}"
    );
    assert!(!env.caskroom(second).exists());
    let _ = env.run(&["uninstall", "--cask", first]);
}

/// A `binary` artifact never silently replaces something already in `bin`
/// (`Symlinked#link`'s `:conflict`), and the failed install purges itself.
#[test]
fn a_binary_artifact_will_not_overwrite_an_existing_file() {
    let env = env_or_skip!();
    let token = "fastbrew-binary-clash";
    let app = "FastbrewBinaryClash.app";
    let bin = env.sandbox.prefix.join("bin/clash");
    std::fs::create_dir_all(bin.parent().expect("bin")).expect("mkdir bin");
    std::fs::write(&bin, "#!/bin/sh\necho mine\n").expect("write the existing file");

    let url = fixture_url(token, "1.0");
    let sha = env.seed_app_zip(&url, app, "1.0");
    env.write_cask(
        token,
        &app_cask_rb(
            token,
            "1.0",
            &url,
            &sha,
            app,
            "  binary \"#{appdir}/FastbrewBinaryClash.app/Contents/MacOS/demo\", target: \"clash\"\n",
        ),
    );

    let out = env.run(&["install", "--cask", token]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = support::strip_ansi(&String::from_utf8_lossy(&out.stderr));
    assert!(
        stderr.contains(&format!(
            "It seems there is already a Binary at '{}'.",
            bin.display()
        )),
        "{stderr}"
    );
    // The file that was there is untouched, and the half-install is gone.
    assert_eq!(
        std::fs::read_to_string(&bin).expect("still there"),
        "#!/bin/sh\necho mine\n"
    );
    assert!(!bin.is_symlink());
    assert!(
        !env.caskroom(token).exists(),
        "the failed version was purged"
    );
    assert!(!env.appdir().join(app).exists());
}

/// An app the user deleted by hand still leaves Caskroom records, and
/// `uninstall` has to clean them up rather than refuse.
#[test]
fn uninstall_cleans_up_after_a_manually_deleted_app() {
    let env = env_or_skip!();
    let token = "fastbrew-deleted-app";
    let app = "FastbrewDeletedApp.app";
    env.install_app_cask(token, app, "1.0", "");

    std::fs::remove_dir_all(env.appdir().join(app)).expect("delete the app by hand");
    assert_eq!(env.stdout(&["list", "--cask"]), format!("{token}\n"));

    let out = env.run(&["uninstall", "--cask", token]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!env.caskroom(token).exists());
    assert_eq!(env.stdout(&["list", "--cask"]), "");
}

/// `--adopt` takes over an app that is already in the app directory when it
/// is the same bundle, and refuses when it is not.
#[test]
fn adopt_takes_over_an_identical_app() {
    let env = env_or_skip!();
    let token = "fastbrew-adopt";
    let app = "FastbrewAdopt.app";
    let url = fixture_url(token, "1.0");
    let sha = env.seed_app_zip(&url, app, "1.0");
    env.write_cask(token, &app_cask_rb(token, "1.0", &url, &sha, app, ""));

    // Put a *different* app there first: adoption has to refuse it.
    let target = env.appdir().join(app);
    std::fs::create_dir_all(target.join("Contents/MacOS")).expect("mkdir");
    std::fs::write(
        target.join("Contents/MacOS/demo"),
        "#!/bin/sh\necho other\n",
    )
    .expect("write");
    let out = env.run(&["install", "--cask", "--adopt", token]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = support::strip_ansi(&String::from_utf8_lossy(&out.stderr));
    assert!(
        stderr.contains("It seems the existing App is different from the one being installed."),
        "{stderr}"
    );

    // Without `--adopt` or `--force` an occupied target is a hard error.
    let out = env.run(&["install", "--cask", token]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        support::strip_ansi(&String::from_utf8_lossy(&out.stderr)).contains(&format!(
            "It seems there is already an App at '{}'.",
            target.display()
        )),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The identical bundle is adopted: the app stays where it is and the
    // Caskroom records it.
    std::fs::remove_dir_all(&target).expect("remove");
    let staged = build_app_zip(&env.sandbox.home.join("fixtures"), app, "1.0");
    let unzip = env.sandbox.home.join("adopt-src");
    let _ = std::fs::remove_dir_all(&unzip);
    std::fs::create_dir_all(&unzip).expect("mkdir");
    assert!(
        Command::new("ditto")
            .args(["-x", "-k"])
            .arg(&staged)
            .arg(&unzip)
            .status()
            .expect("run ditto")
            .success()
    );
    std::fs::rename(unzip.join(app), &target).expect("put the app in place");

    let out = env.run(&["install", "--cask", "--adopt", token]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        support::strip_ansi(&String::from_utf8_lossy(&out.stdout))
            .contains(&format!("Adopting existing App at '{}'", target.display())),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(target.join("Contents/MacOS/demo").is_file());
    assert_eq!(
        env.stdout(&["list", "--cask", "--versions"]),
        format!("{token} 1.0\n")
    );
    let _ = env.run(&["uninstall", "--cask", token]);
}

/// A `pkg` cask is planned without running `/usr/sbin/installer`: the dry run
/// touches nothing, and the command it would build is checked directly
/// because the real one needs `sudo`.
#[test]
fn a_pkg_cask_is_planned_without_running_the_installer() {
    let env = env_or_skip!();
    let token = "fastbrew-pkg";
    let url = fixture_url(token, "1.0");
    // The container is never unpacked in a dry run, so any bytes will do.
    env.seed_app_zip(&url, "FastbrewPkg.app", "1.0");
    env.write_cask(
        token,
        &format!(
            r#"cask "{token}" do
  version "1.0"
  sha256 :no_check

  url "{url}"
  name "pkg fixture"
  homepage "https://example.invalid/{token}"

  pkg "Fastbrew.pkg"

  uninstall pkgutil: "org.fastbrew.pkg"
end
"#
        ),
    );

    let plan = env.combined(&["install", "--cask", "--dry-run", token]);
    assert_eq!(
        plan,
        format!("==> Would install 1 cask:\nfixture/casks/{token}\n")
    );
    assert!(!env.caskroom(token).exists(), "the dry run staged nothing");

    // `Artifact::Pkg#install_phase` runs exactly this, with `sudo`.
    let (program, args) = fastbrew::cask::artifacts::pkg_command(
        Path::new("/Caskroom/fastbrew-pkg/1.0/Fastbrew.pkg"),
        None,
        false,
        false,
    );
    assert_eq!(program, "/usr/sbin/installer");
    assert_eq!(
        args,
        [
            "-pkg",
            "/Caskroom/fastbrew-pkg/1.0/Fastbrew.pkg",
            "-target",
            "/"
        ]
    );
}

// ------------------------------------------------------------------ taps

/// A tap's `cmd/brew-<name>` is an external command: `commands` lists it and
/// `fastbrew <name>` runs it, the way `brew.rb` `exec`s `external_cmd_path`.
#[test]
fn a_taps_external_command_is_listed_and_executed() {
    let env = env_or_skip!();
    env.write_tap_command(
        "tiger/cmds",
        "greet",
        "#!/bin/sh\necho \"greetings from $1 in $HOMEBREW_PREFIX\"\n",
    );

    let listed = env.stdout(&["commands"]);
    assert!(
        listed.contains("\n==> External commands\n"),
        "the section is there:\n{listed}"
    );
    assert!(
        listed.lines().any(|l| l == "greet"),
        "the tap's command is listed:\n{listed}"
    );

    // `--quiet` drops the headers but keeps the command.
    let quiet = env.stdout(&["commands", "--quiet"]);
    assert!(quiet.lines().any(|l| l == "greet"), "{quiet}");

    // Running it hands over the arguments and the resolved prefix.
    let out = env.run(&["greet", "world"]);
    assert!(
        out.status.success(),
        "the external command failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("greetings from world in {}", env.sandbox.prefix.display())
    );

    // Its exit status is the command's own.
    env.write_tap_command("tiger/cmds", "boom", "#!/bin/sh\nexit 3\n");
    assert_eq!(env.run(&["boom"]).status.code(), Some(3));

    // A name no tap provides is still a candidate for delegation.
    let out = env.run(&["definitely-not-a-command"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("refusing to delegate to brew"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `untap` of a tap that is not installed is `TapUnavailableError`, which
/// names the command that would create it.
#[test]
fn untap_of_a_missing_tap_names_the_tap_new_command() {
    let env = env_or_skip!();
    let out = env.run(&["untap", "no/such"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        support::strip_ansi(&String::from_utf8_lossy(&out.stderr)),
        "Error: No available tap no/such.\nRun brew tap-new no/such to create a new no/such tap!\n"
    );
}

/// `git` with the identity and hooks a test needs, never the user's.
fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=fastbrew tests",
            "-c",
            "user.email=tests@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("run git")
        .status
        .success()
}

/// A git repository shaped like a tap, for `tap` to clone over `file://`.
fn make_tap_remote(dir: &Path) {
    std::fs::create_dir_all(dir.join("Formula")).expect("mkdir Formula");
    std::fs::write(
        dir.join("Formula/scenario.rb"),
        "class Scenario < Formula\n  desc \"Scenario fixture\"\n  \
         homepage \"https://example.invalid/scenario\"\n  \
         url \"https://example.invalid/scenario-1.0.tar.gz\"\n  \
         sha256 \"1111111111111111111111111111111111111111111111111111111111111111\"\nend\n",
    )
    .expect("write formula");
    assert!(git(dir, &["init", "--initial-branch=main", "--quiet"]));
    assert!(git(dir, &["add", "-A"]));
    assert!(git(dir, &["commit", "--quiet", "-m", "Add scenario"]));
}

/// A tap whose remote has gone away is `Fetching <dir> failed!`, and it makes
/// the whole `update` fail instead of reporting "Already up-to-date."
/// (`cmd/update.sh`'s `HOMEBREW_UPDATE_FAILED`).
#[test]
fn update_fails_when_a_taps_remote_cannot_be_fetched() {
    let env = env_or_skip!();
    let remote = tempfile::tempdir().expect("tempdir");
    make_tap_remote(remote.path());
    let url = format!("file://{}", remote.path().display());

    let out = env.run(&["tap", "tiger/scenario", &url]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The API is unreachable, so only the taps can move.
    let update = |env: &Env| {
        env.cmd()
            .args(["update"])
            .env("HOMEBREW_API_DOMAIN", "http://127.0.0.1:1/api")
            .env("HOMEBREW_CURL_RETRIES", "0")
            .output()
            .expect("run fastbrew")
    };

    let out = update(&env);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "Already up-to-date."
    );

    // Take the remote away: the fetch fails, and so does `update`.
    let moved = remote.path().with_extension("gone");
    std::fs::rename(remote.path(), &moved).expect("move the remote aside");
    let out = update(&env);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a failed tap fetch fails update"
    );
    let stderr = support::strip_ansi(&String::from_utf8_lossy(&out.stderr));
    assert!(
        stderr.contains("Error: Fetching ") && stderr.contains("homebrew-scenario failed!"),
        "{stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("Already up-to-date."),
        "a failed update is not up to date:\n{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // Putting it back makes `update` succeed again.
    std::fs::rename(&moved, remote.path()).expect("restore the remote");
    let out = update(&env);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "Already up-to-date."
    );
}

// -------------------------------------------------------------- services

/// A keg that ships a service: the generated plist and systemd unit plus the
/// receipt `services list` reads. Nothing here ever talks to launchd.
fn install_service_keg(env: &Env, name: &str, version: &str) -> PathBuf {
    let entry: fastbrew::model::FormulaEntry = serde_json::from_value(serde_json::json!({
        "desc": "Scenario service",
        "homepage": "https://example.invalid/svc",
        "stable_version": version,
        "service_run_args": [[
            "$HOMEBREW_PREFIX/opt/scenario-svc/bin/scenario-svc",
            "--config",
            "$HOMEBREW_PREFIX/etc/scenario-svc.conf"
        ]],
        "service_args": [
            [":run_type", ":immediate"],
            [":working_dir", "$HOMEBREW_PREFIX"],
            [":log_path", "$HOMEBREW_PREFIX/var/log/scenario-svc.log"],
            [":keep_alive", {":always": true}]
        ]
    }))
    .expect("entry");
    let mut entry = entry;
    entry.name = name.to_string();
    entry.tap = "homebrew/core".to_string();

    let keg = env.sandbox.prefix.join("Cellar").join(name).join(version);
    std::fs::create_dir_all(keg.join("bin")).expect("mkdir keg");
    std::fs::write(
        keg.join("INSTALL_RECEIPT.json"),
        serde_json::json!({
            "homebrew_version": fastbrew::HOMEBREW_COMPAT_VERSION,
            "installed_on_request": true,
            "runtime_dependencies": [],
            "source": {"tap": "homebrew/core", "versions": {"stable": version}},
        })
        .to_string(),
    )
    .expect("write receipt");
    fastbrew::services::plist::install_service_files(&env.config(), &entry, &keg)
        .expect("install service files");
    keg
}

/// A formula with a service is discoverable, reported as `none`, and gone
/// again once the keg is: the whole read-only half of `brew services`.
#[test]
fn a_services_keg_is_listed_until_it_is_uninstalled() {
    let env = env_or_skip!();
    let name = "scenario-svc";
    let keg = install_service_keg(&env, name, "1.0");

    // The keg carries both generated files, under the legacy plist label.
    let plist = keg.join(format!("homebrew.mxcl.{name}.plist"));
    assert!(plist.is_file(), "{} missing", plist.display());
    assert!(keg.join(format!("homebrew.{name}.service")).is_file());

    // `print_table`: the status column is 15 wide, the header 15 - 9.
    let table = env.stdout(&["services", "list"]);
    assert_eq!(
        table,
        "Name         Status User File\nscenario-svc none                 \n"
    );
    assert_eq!(env.stdout(&["services", "ls"]), table);

    let json: serde_json::Value =
        serde_json::from_str(&env.stdout(&["services", "list", "--json"])).expect("json");
    assert_eq!(json[0]["name"], serde_json::json!(name));
    assert_eq!(json[0]["status"], serde_json::json!("none"));
    assert_eq!(json[0]["user"], serde_json::Value::Null);
    assert_eq!(json[0]["exit_code"], serde_json::Value::Null);
    assert_eq!(
        json[0]["file"],
        serde_json::json!(plist.to_string_lossy().as_ref())
    );

    let info = env.stdout(&["services", "info", name]);
    assert_eq!(
        info,
        format!(
            "{name} (homebrew.mxcl.{name})\nRunning: false\nLoaded: false\nSchedulable: false\n"
        )
    );
    let info_json: serde_json::Value =
        serde_json::from_str(&env.stdout(&["services", "info", "--json", name])).expect("json");
    assert_eq!(
        info_json[0]["service_name"],
        serde_json::json!(format!("homebrew.mxcl.{name}"))
    );
    assert_eq!(info_json[0]["loaded"], serde_json::json!(false));

    // A formula without a service still answers `info`, with every flag false
    // (`FormulaWrapper#to_hash`), but cannot be started.
    env.sandbox
        .add_keg("jq", &support::api_pkg_version("jq"), true);
    assert_eq!(
        env.stdout(&["services", "info", "jq"]),
        "jq (sh.brew.jq)\nRunning: false\nLoaded: false\nSchedulable: false\n"
    );
    let out = env.run(&["services", "start", "jq"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        support::strip_ansi(&String::from_utf8_lossy(&out.stderr)).trim(),
        "Error: Formula `jq` has not implemented #plist, #service or provided a locatable service file."
    );

    // A formula that is not installed at all fails in name resolution.
    let out = env.run(&["services", "start", "definitely-not-a-formula"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("No available formula with the name"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Uninstalling the keg takes the service with it.
    std::fs::remove_dir_all(env.sandbox.prefix.join("Cellar").join(name)).expect("remove the keg");
    assert_eq!(env.stdout(&["services", "list"]), "");
    assert_eq!(env.stdout(&["services", "list", "--json"]).trim(), "[]");
    assert_eq!(
        env.stdout(&["services", "cleanup"]).trim(),
        "All user-space services OK, nothing cleaned..."
    );
}

/// `--sudo-service-user` names the user the service runs as, so it needs a
/// value and root (`Subcommand.dispatch`). Nothing about it may be guessed:
/// a missing username used to leave the plist untouched and the service
/// running as root.
#[test]
fn sudo_service_user_needs_a_username_and_root() {
    let env = env_or_skip!();
    install_service_keg(&env, "scenario-svc", "1.0");

    // `flag "--sudo-service-user="`: the value is not optional.
    let out = env.run(&["services", "start", "--sudo-service-user", "scenario-svc"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("equal sign is needed"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // An empty value is a usage error rather than "run as nobody in
    // particular".
    let out = env.run(&["services", "start", "--sudo-service-user=", "scenario-svc"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("requires a username"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The tests never run as root, which is exactly the case Homebrew
    // refuses: nothing is copied and nothing is bootstrapped.
    let out = env.run(&[
        "services",
        "start",
        "--sudo-service-user=nobody",
        "scenario-svc",
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        support::strip_ansi(&String::from_utf8_lossy(&out.stderr)).trim(),
        "Error: `fastbrew services --sudo-service-user` is supported only when running as root!"
    );
    assert_eq!(
        env.stdout(&["services", "list"]),
        "Name         Status User File\nscenario-svc none                 \n",
        "the service must still be unloaded"
    );
    assert!(
        !env.sandbox
            .home
            .join("Library/LaunchAgents/homebrew.mxcl.scenario-svc.plist")
            .exists(),
        "nothing may be installed into LaunchAgents"
    );
}

/// The tap commands a user runs around a `file://` tap: tapping twice,
/// `tap-info` in both shapes, the alias that resolves, and the untap that is
/// refused while something from the tap is installed.
#[test]
fn tapping_reading_and_untapping_a_local_tap() {
    let env = env_or_skip!();
    let remote = tempfile::tempdir().expect("tempdir");
    make_tap_remote(remote.path());
    // An `Aliases/` entry is a symlink to the formula it renames.
    std::fs::create_dir_all(remote.path().join("Aliases")).expect("mkdir Aliases");
    std::os::unix::fs::symlink(
        "../Formula/scenario.rb",
        remote.path().join("Aliases/scenario-alias"),
    )
    .expect("symlink");
    assert!(git(remote.path(), &["add", "-A"]));
    assert!(git(
        remote.path(),
        &["commit", "--quiet", "-m", "Add the alias"]
    ));
    let url = format!("file://{}", remote.path().display());

    let out = env.run(&["tap", "tiger/local", &url]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = support::strip_ansi(&String::from_utf8_lossy(&out.stderr));
    assert!(stderr.contains("==> Tapping tiger/local"), "{stderr}");
    assert!(stderr.contains("Tapped 1 formula ("), "{stderr}");

    // `cmd/tap.rb` rescues `TapAlreadyTappedError`: tapping again is a no-op.
    let again = env.run(&["tap", "tiger/local", &url]);
    assert!(again.status.success());
    assert_eq!(String::from_utf8_lossy(&again.stdout), "");

    // A bare `tap` lists what is installed.
    assert!(
        env.stdout(&["tap"]).lines().any(|l| l == "tiger/local"),
        "{}",
        env.stdout(&["tap"])
    );

    // --- tap-info ---------------------------------------------------------
    let info = env.stdout(&["tap-info", "tiger/local"]);
    assert!(info.starts_with("tiger/local: Installed\n"), "{info}");
    assert!(info.contains("\n1 formula\n"), "{info}");
    assert!(info.contains(&format!("\nFrom: {url}\n")), "{info}");
    assert!(info.contains("\n==> Formulae\nscenario\n"), "{info}");

    let json: serde_json::Value =
        serde_json::from_str(&env.stdout(&["tap-info", "--installed", "--json"])).expect("json");
    let row = json
        .as_array()
        .expect("an array")
        .iter()
        .find(|t| t["name"] == serde_json::json!("tiger/local"))
        .expect("the tap is listed");
    assert_eq!(row["user"], serde_json::json!("tiger"));
    assert_eq!(row["repo"], serde_json::json!("local"));
    assert_eq!(row["installed"], serde_json::json!(true));
    assert_eq!(row["official"], serde_json::json!(false));
    assert_eq!(row["remote"], serde_json::json!(url));
    assert_eq!(row["custom_remote"], serde_json::json!(true));
    assert_eq!(
        row["formula_names"],
        serde_json::json!(["tiger/local/scenario"])
    );

    // A tap that is not installed is reported and fails the command.
    let out = env.run(&["tap-info", "nobody/nothing"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("nobody/nothing: Not installed"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // --- the tap's formula is resolvable, by name and by alias ------------
    let formula = env.stdout(&["info", "tiger/local/scenario"]);
    assert!(
        formula.starts_with("==> tiger/local/scenario: stable 1.0\n"),
        "{formula}"
    );
    assert!(formula.contains("\nTap: tiger/local\n"), "{formula}");
    let aliased = env.stdout(&["info", "tiger/local/scenario-alias"]);
    assert!(
        aliased.starts_with("==> tiger/local/scenario: stable 1.0\n"),
        "the alias resolves to the formula it points at:\n{aliased}"
    );
    let search = env.stdout(&["search", "scenario"]);
    assert!(
        search.lines().any(|l| l == "tiger/local/scenario"),
        "search lists the tap-qualified name:\n{search}"
    );

    // --- a tap formula with no bottle is the Ruby brew's job --------------
    let out = env.run(&["install", "--dry-run", "tiger/local/scenario"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = support::strip_ansi(&String::from_utf8_lossy(&out.stderr));
    assert!(
        stderr.contains("refusing to delegate to brew (scenario: no bottle available!)"),
        "the delegation names the reason:\n{stderr}"
    );

    // --- untap ------------------------------------------------------------
    let keg = env.sandbox.prefix.join("Cellar/scenario/1.0");
    std::fs::create_dir_all(&keg).expect("mkdir keg");
    std::fs::write(
        keg.join("INSTALL_RECEIPT.json"),
        serde_json::json!({"source": {"tap": "tiger/local"}}).to_string(),
    )
    .expect("write receipt");

    let out = env.run(&["untap", "tiger/local"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        support::strip_ansi(&String::from_utf8_lossy(&out.stderr)),
        "Error: Refusing to untap tiger/local because it contains the following installed formulae:\n\
         tiger/local/scenario\n"
    );
    assert!(env.stdout(&["tap"]).contains("tiger/local"));

    // `--force` untaps anyway.
    let out = env.run(&["untap", "--force", "tiger/local"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!env.stdout(&["tap"]).contains("tiger/local"));
    let out = env.run(&["info", "tiger/local/scenario"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("This command requires the tap tiger/local."),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ------------------------------------------------------- ruby post_install

/// Write a tap formula plus an installed keg for it, so the commands that act
/// on an installed formula can reach it.
fn tap_formula_keg(env: &Env, tap: &str, name: &str, body: &str) -> PathBuf {
    let (user, repo) = tap.split_once('/').expect("user/repo");
    let dir = env
        .sandbox
        .prefix
        .join("Library/Taps")
        .join(user)
        .join(format!("homebrew-{repo}"))
        .join("Formula");
    std::fs::create_dir_all(&dir).expect("mkdir Formula");
    std::fs::write(dir.join(format!("{name}.rb")), body).expect("write formula");

    let keg = env.sandbox.prefix.join("Cellar").join(name).join("1.0");
    std::fs::create_dir_all(keg.join("bin")).expect("mkdir keg");
    std::fs::write(
        keg.join("INSTALL_RECEIPT.json"),
        serde_json::json!({
            "installed_on_request": true,
            "runtime_dependencies": [],
            "source": {
                "spec": "stable",
                "versions": {"stable": "1.0", "version_scheme": 0},
                "tap": tap,
            },
        })
        .to_string(),
    )
    .expect("write receipt");
    keg
}

/// A `brew` that records its arguments instead of doing anything.
fn fake_brew(env: &Env) -> PathBuf {
    let path = env.sandbox.home.join("fake-brew");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
            env.sandbox.home.join("brew-argv").display()
        ),
    )
    .expect("write the fake brew");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// A tap formula's Ruby `post_install` needs the formula DSL. Skipping it
/// silently would leave the keg unconfigured, so `postinstall` hands the run
/// to the Ruby `brew` — and says so plainly when it cannot.
#[test]
fn a_ruby_post_install_is_delegated_rather_than_skipped() {
    let env = env_or_skip!();
    let tag = support::bottle_tag();
    let tap = "tiger/postinstall";
    let body = format!(
        r#"class Postfoo < Formula
  desc "Tap formula with a Ruby post_install"
  homepage "https://example.invalid/postfoo"
  url "https://example.invalid/postfoo-1.0.tar.gz"
  sha256 "7777777777777777777777777777777777777777777777777777777777777777"
  license "MIT"

  bottle do
    root_url "https://bottles.invalid/v2/tiger/postfoo"
    sha256 cellar: :any_skip_relocation, {tag}: "8888888888888888888888888888888888888888888888888888888888888888"
  end

  def post_install
    (var/"postfoo-marker").write("x")
  end
end
"#
    );
    tap_formula_keg(&env, tap, "postfoo", &body);

    // Planning still works; nothing about the plan needs Ruby.
    let plan = env.combined(&["install", "--dry-run", "tiger/postinstall/postfoo"]);
    assert!(
        plan.contains("postfoo"),
        "the formula still resolves and plans:\n{plan}"
    );

    // Without a `brew` there is nothing to delegate to, and the command says
    // exactly which step it cannot run.
    let out = env.run(&["postinstall", "tiger/postinstall/postfoo"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = support::strip_ansi(&String::from_utf8_lossy(&out.stderr));
    assert!(
        stderr.contains("post_install needs the Ruby formula DSL"),
        "the reason names post_install:\n{stderr}"
    );

    // With one, the invocation is handed over unchanged.
    let brew = fake_brew(&env);
    let argv = env.sandbox.home.join("brew-argv");
    let _ = std::fs::remove_file(&argv);
    let out = env
        .cmd()
        .args(["postinstall", "tiger/postinstall/postfoo"])
        .env_remove("FASTBREW_NO_DELEGATE")
        .env("FASTBREW_BREW", &brew)
        .output()
        .expect("run fastbrew");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&argv).expect("the fake brew ran"),
        "postinstall\ntiger/postinstall/postfoo\n"
    );
}

// ---------------------------------------------------------- general CLI

/// `brew` with no arguments prints `HOMEBREW_HELP_MESSAGE` on stderr and
/// exits 1; `help` prints the same text on stdout and exits 0.
#[test]
fn a_bare_invocation_prints_the_usage_summary_on_stderr() {
    let env = env_or_skip!();
    let out = env.run(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with("Example usage:\n"), "{stderr}");
    assert!(stderr.contains("\nFurther help:\n"), "{stderr}");

    let help = env.stdout(&["help"]);
    assert_eq!(help, stderr);
}

// -------------------------------------------------------- cask lifecycle

/// The whole install -> list -> upgrade -> uninstall round trip, with the
/// greedy switches and the pin that holds a version back.
#[test]
fn the_cask_lifecycle_from_install_to_uninstall() {
    let env = env_or_skip!();
    let token = "fastbrew-lifecycle";
    let app = "FastbrewLifecycle.app";
    env.install_app_cask(token, app, "1.0", "");

    // --- what the read-only commands report -------------------------------
    assert_eq!(env.stdout(&["list", "--cask"]), format!("{token}\n"));
    assert_eq!(
        env.stdout(&["list", "--cask", "--versions"]),
        format!("{token} 1.0\n")
    );
    let info = env.stdout(&["info", "--cask", token]);
    assert!(
        info.starts_with(&format!(
            "==> fixture/casks/{token} ({token} fixture): 1.0\n"
        )),
        "{info}"
    );
    assert!(info.contains("\nInstalled\n"), "{info}");
    assert!(info.contains(&format!("Caskroom/{token}/1.0")), "{info}");
    assert!(info.contains(&format!("{app} (App)")), "{info}");

    // Nothing is outdated while the tap still serves 1.0.
    let plan = env.combined(&["upgrade", "--cask", "--dry-run"]);
    assert_eq!(plan, "", "an up-to-date cask produces no output:\n{plan}");

    // --- 2.0 upstream -----------------------------------------------------
    let url = fixture_url(token, "2.0");
    let sha = env.seed_app_zip(&url, app, "2.0");
    env.write_cask(token, &app_cask_rb(token, "2.0", &url, &sha, app, ""));

    let plan = env.combined(&["upgrade", "--cask", "--dry-run"]);
    assert!(
        plan.contains("==> Would upgrade 1 outdated package:"),
        "{plan}"
    );
    assert!(
        plan.contains(&format!("fixture/casks/{token} 1.0 -> 2.0")),
        "{plan}"
    );
    assert_eq!(
        app_version(&env.appdir().join(app)),
        "1.0",
        "--dry-run changed the installed app"
    );

    // A pin holds it back, and says so.
    assert!(env.run(&["pin", "--cask", token]).status.success());
    let held = env.combined(&["upgrade", "--cask"]);
    assert!(held.contains("Not upgrading 1 pinned package:"), "{held}");
    assert!(
        held.contains(&format!("fixture/casks/{token} 1.0")),
        "{held}"
    );
    assert_eq!(app_version(&env.appdir().join(app)), "1.0");
    assert!(env.run(&["unpin", "--cask", token]).status.success());

    // --- the real upgrade -------------------------------------------------
    let done = env.combined(&["upgrade", "--cask"]);
    assert!(done.contains("==> Upgrading 1 outdated package:"), "{done}");
    assert_eq!(app_version(&env.appdir().join(app)), "2.0");
    assert_eq!(
        env.stdout(&["list", "--cask", "--versions"]),
        format!("{token} 2.0\n")
    );

    // Exactly one staged version, and no backup left behind.
    let caskroom = env.caskroom(token);
    assert_eq!(entries(&caskroom), [".metadata", "2.0"]);
    assert!(backup_leftovers(&caskroom).is_empty());

    // A second `upgrade` (formulae and casks) has nothing to do.
    let again = env.combined(&["upgrade"]);
    assert_eq!(again, "", "nothing left to upgrade:\n{again}");

    // Naming it reports why it is being skipped.
    let named = env.combined(&["upgrade", "--cask", token]);
    assert!(
        named.contains(&format!(
            "Not upgrading {token}, the latest version is already installed"
        )),
        "{named}"
    );

    // --- reinstall and uninstall -----------------------------------------
    let out = env.run(&["reinstall", "--cask", token]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(app_version(&env.appdir().join(app)), "2.0");
    assert_eq!(entries(&caskroom), [".metadata", "2.0"]);

    let out = env.run(&["uninstall", "--cask", token]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!env.appdir().join(app).exists());
    assert!(!caskroom.exists());
    assert_eq!(env.stdout(&["list", "--cask"]), "");

    // Uninstalling it again is Homebrew's `CaskNotInstalledError`.
    let out = env.run(&["uninstall", "--cask", token]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        support::strip_ansi(&String::from_utf8_lossy(&out.stderr)).trim(),
        format!("Error: Cask '{token}' is not installed.")
    );
}

/// An `auto_updates` cask keeps itself current, so the sweep leaves it alone
/// until the run is greedy; naming it is greedy by itself.
#[test]
fn auto_updates_casks_are_only_swept_up_when_greedy() {
    let env = env_or_skip!();
    let token = "fastbrew-auto-updates";
    let app = "FastbrewAutoUpdates.app";
    env.install_app_cask(token, app, "1.0", "  auto_updates true\n");

    let url = fixture_url(token, "2.0");
    let sha = env.seed_app_zip(&url, app, "2.0");
    env.write_cask(
        token,
        &app_cask_rb(token, "2.0", &url, &sha, app, "  auto_updates true\n"),
    );

    // The sweep says nothing at all.
    assert_eq!(env.combined(&["upgrade", "--cask", "--dry-run"]), "");
    // `--greedy-latest` is for `version :latest` casks, not this one.
    assert_eq!(
        env.combined(&["upgrade", "--cask", "--dry-run", "--greedy-latest"]),
        ""
    );

    for flag in ["--greedy", "--greedy-auto-updates"] {
        let plan = env.combined(&["upgrade", "--cask", "--dry-run", flag]);
        assert!(
            plan.contains(&format!("fixture/casks/{token} 1.0 -> 2.0")),
            "{flag} has to pick it up:\n{plan}"
        );
    }

    // Naming the cask is greedy on its own (`outdated?(greedy: true)`).
    let plan = env.combined(&["upgrade", "--cask", "--dry-run", token]);
    assert!(
        plan.contains(&format!("fixture/casks/{token} 1.0 -> 2.0")),
        "{plan}"
    );

    // ... and so is `install`, which upgrades an outdated installed cask.
    let out = env.run(&["install", "--cask", token]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(app_version(&env.appdir().join(app)), "2.0");
    let _ = env.run(&["uninstall", "--cask", token]);
}

/// A `version :latest` cask has no version to compare, so it is outdated only
/// when the container's checksum has moved and the run asked for it.
#[test]
fn latest_casks_need_greedy_latest_and_a_changed_download() {
    let env = env_or_skip!();
    let token = "fastbrew-latest";
    let app = "FastbrewLatest.app";

    // `version :latest` with a url that does not carry the version.
    let url = "https://example.invalid/fastbrew/fastbrew-latest.zip";
    let sha = env.seed_app_zip(url, app, "1.0");
    env.write_cask(token, &app_cask_rb(token, "latest", url, &sha, app, ""));
    let out = env.run(&["install", "--cask", token]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        env.stdout(&["list", "--cask", "--versions"]),
        format!("{token} latest\n")
    );

    // The install recorded the container's checksum.
    let recorded =
        std::fs::read_to_string(env.caskroom(token).join(".metadata/LATEST_DOWNLOAD_SHA256"))
            .expect("LATEST_DOWNLOAD_SHA256");
    assert_eq!(recorded.trim(), sha);

    // Nothing has changed, so even a greedy run leaves it alone.
    for flags in [
        vec!["upgrade", "--cask", "--dry-run"],
        vec!["upgrade", "--cask", "--dry-run", "--greedy-auto-updates"],
        vec!["upgrade", "--cask", "--dry-run", "--greedy"],
        vec!["upgrade", "--cask", "--dry-run", "--greedy-latest"],
    ] {
        assert_eq!(
            env.combined(&flags),
            "",
            "the download has not changed, so `{}` finds nothing",
            flags.join(" ")
        );
    }
    // Naming it reports Homebrew's `:latest` wording.
    let named = env.combined(&["upgrade", "--cask", token]);
    assert!(
        named.contains(&format!(
            "Not upgrading {token}, the downloaded artifact has not changed"
        )),
        "{named}"
    );

    // A new container under the same url is a new "version".
    let changed = env.seed_app_zip(url, app, "2.0");
    assert_ne!(changed, sha);
    env.write_cask(token, &app_cask_rb(token, "latest", url, &changed, app, ""));
    assert_eq!(env.combined(&["upgrade", "--cask", "--dry-run"]), "");
    let plan = env.combined(&["upgrade", "--cask", "--dry-run", "--greedy-latest"]);
    assert!(
        plan.contains(&format!("fixture/casks/{token} latest -> latest")),
        "{plan}"
    );
    let _ = env.run(&["uninstall", "--cask", token]);
}

/// `pin`/`unpin` work on casks in Homebrew 6 (`Cask#pin`): a relative symlink
/// under `var/homebrew/pinned_casks`.
#[test]
fn pinning_a_cask_records_a_relative_symlink() {
    let env = env_or_skip!();
    let token = "fastbrew-pinned";
    let app = "FastbrewPinned.app";
    env.install_app_cask(token, app, "1.0", "");

    let pin = env
        .sandbox
        .prefix
        .join("var/homebrew/pinned_casks")
        .join(token);
    assert!(env.run(&["pin", "--cask", token]).status.success());
    assert!(pin.is_symlink(), "{} is not a symlink", pin.display());
    assert_eq!(
        std::fs::read_link(&pin).unwrap(),
        PathBuf::from(format!("../../../Caskroom/{token}/1.0")),
        "Homebrew records a relative symlink"
    );

    // `info --json` of a *tap* cask needs the Ruby cask DSL, so it delegates
    // rather than guessing (`docs/DESIGN.md` 5).
    let out = env.run(&["info", "--json=v2", "--cask", token]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("needs the Ruby cask DSL"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Pinning twice warns rather than failing.
    let again = env.combined(&["pin", "--cask", token]);
    assert!(
        again.contains(&format!("{token} already pinned")),
        "{again}"
    );

    assert!(env.run(&["unpin", "--cask", token]).status.success());
    assert!(!pin.exists() && !pin.is_symlink());
    let repeated = env.combined(&["unpin", "--cask", token]);
    assert!(
        repeated.contains(&format!("{token} not pinned")),
        "{repeated}"
    );

    // An uninstalled cask cannot be pinned: `ofail`, so the exit status is 1.
    let out = env.run(&["pin", "--cask", "fastbrew-never-installed"]);
    assert_eq!(out.status.code(), Some(1));
    let _ = env.run(&["uninstall", "--cask", token]);
}

/// `help`, `--help`, `commands` and the wording of a bad flag.
#[test]
fn help_and_usage_errors() {
    let env = env_or_skip!();

    // `help <command>` is the command's usage banner; an alias says so.
    let install = env.stdout(&["help", "install"]);
    assert!(install.starts_with("Usage: fastbrew install "), "{install}");
    assert!(install.contains("--only-dependencies"), "{install}");
    let alias = env.stdout(&["help", "ls"]);
    assert!(alias.starts_with("Usage: fastbrew list "), "{alias}");
    assert!(alias.contains("`ls` is an alias for `list`."), "{alias}");

    // `--help` on a subcommand is clap's own, on stdout, exiting 0.
    let out = env.run(&["install", "--help"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Usage: fastbrew install"), "{text}");
    assert!(text.contains("--cask"), "{text}");

    // An unknown flag is a usage error on stderr, exiting 1.
    let out = env.run(&["list", "--definitely-not-a-flag"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert!(
        support::strip_ansi(&String::from_utf8_lossy(&out.stderr))
            .contains("Error: unexpected argument '--definitely-not-a-flag' found"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `commands` groups the built-ins; `--quiet` drops the headers.
    let commands = env.stdout(&["commands"]);
    assert!(
        commands.starts_with("==> Built-in commands\n"),
        "{commands}"
    );
    for expected in ["install", "services", "tap-info", "doctor", "completions"] {
        assert!(
            commands.lines().any(|l| l == expected),
            "`{expected}` missing from:\n{commands}"
        );
    }
    let quiet = env.stdout(&["commands", "--quiet"]);
    assert!(!quiet.contains("==>"), "{quiet}");
}

/// Every path command and `config` answer with the sandbox, never the host
/// prefix, and `--version`/`-v` agree.
#[test]
fn the_path_commands_answer_with_the_sandbox() {
    let env = env_or_skip!();
    let prefix = env.sandbox.prefix.display().to_string();

    assert_eq!(env.stdout(&["--prefix"]).trim(), prefix);
    assert_eq!(
        env.stdout(&["--cellar"]).trim(),
        env.sandbox.prefix.join("Cellar").display().to_string()
    );
    assert_eq!(
        env.stdout(&["--caskroom"]).trim(),
        env.sandbox.prefix.join("Caskroom").display().to_string()
    );
    assert_eq!(
        env.stdout(&["--cache"]).trim(),
        env.sandbox.cache.display().to_string()
    );
    assert_eq!(env.stdout(&["--repository"]).trim(), prefix);
    assert_eq!(
        env.stdout(&["--taps"]).trim(),
        env.sandbox
            .prefix
            .join("Library/Taps")
            .display()
            .to_string()
    );

    // `--version` and its `-v` alias print the compatibility version first.
    let version = env.stdout(&["--version"]);
    assert!(
        version.starts_with(&format!("Homebrew {}\n", fastbrew::HOMEBREW_COMPAT_VERSION)),
        "{version}"
    );
    assert!(
        version.contains(&format!("fastbrew {}", fastbrew::FASTBREW_VERSION)),
        "{version}"
    );
    assert_eq!(env.stdout(&["-v"]), version);

    // `config` reports the sandbox, the bottle tag and the variables that are
    // set, never `/opt/homebrew`.
    let config = env.stdout(&["config"]);
    assert!(
        config.contains(&format!("HOMEBREW_PREFIX: {prefix}\n")),
        "{config}"
    );
    assert!(
        config.contains(&format!(
            "HOMEBREW_CACHE: {}\n",
            env.sandbox.cache.display()
        )),
        "{config}"
    );
    assert!(
        config.contains("HOMEBREW_NO_AUTO_UPDATE: set\n"),
        "{config}"
    );
    assert!(
        config.contains(&format!("Bottle tag: {}", support::bottle_tag())),
        "{config}"
    );
    assert!(!config.contains("/opt/homebrew"), "{config}");
}

/// `shellenv` speaks each shell's own syntax, from `$SHELL` or the argument.
#[test]
fn shellenv_follows_the_shell() {
    let env = env_or_skip!();
    let prefix = env.sandbox.prefix.display().to_string();
    let shellenv = |shell: &str, arg: Option<&str>| -> String {
        let mut cmd = env.cmd();
        cmd.env("SHELL", format!("/bin/{shell}")).arg("shellenv");
        if let Some(arg) = arg {
            cmd.arg(arg);
        }
        let out = cmd.output().expect("run fastbrew");
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let zsh = shellenv("zsh", None);
    assert!(
        zsh.starts_with(&format!("export HOMEBREW_PREFIX=\"{prefix}\";\n")),
        "{zsh}"
    );
    assert!(
        zsh.contains(&format!(
            "fpath[1,0]=\"{prefix}/share/zsh/site-functions\";"
        )),
        "the zsh form adds the completions directory:\n{zsh}"
    );

    let bash = shellenv("bash", None);
    assert!(bash.starts_with("export HOMEBREW_PREFIX="), "{bash}");
    assert!(!bash.contains("fpath"), "{bash}");
    assert!(
        bash.contains(&format!("export PATH=\"{prefix}/bin:")),
        "{bash}"
    );

    // The argument wins over `$SHELL`.
    let fish = shellenv("bash", Some("fish"));
    assert!(
        fish.starts_with(&format!(
            "set --global --export HOMEBREW_PREFIX \"{prefix}\";\n"
        )),
        "{fish}"
    );
    assert!(
        fish.contains("fish_add_path --global --move --path"),
        "{fish}"
    );
}

/// `-q` and `-v` are global flags Homebrew accepts anywhere, so no command
/// may reject them.
#[test]
fn quiet_and_verbose_are_accepted_everywhere() {
    let env = env_or_skip!();
    for args in [
        vec!["list"],
        vec!["search", "jq"],
        vec!["info", "jq"],
        vec!["deps", "jq"],
        vec!["outdated"],
        vec!["leaves"],
        vec!["commands"],
        vec!["config"],
        vec!["doctor"],
        vec!["tap"],
        vec!["services", "list"],
        vec!["completions", "state"],
    ] {
        for flag in ["-q", "--quiet", "-v", "--verbose"] {
            let mut with_flag = args.clone();
            with_flag.push(flag);
            let out = env.run(&with_flag);
            assert!(
                out.status.success(),
                "`fastbrew {}` failed: {}",
                with_flag.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

/// `--cask` applies to every named argument (`cmd/install.rb` conflicts the
/// two switches), and a name that is both resolves to the formula with
/// Homebrew's warning.
#[test]
fn mixed_formula_and_cask_arguments() {
    let env = env_or_skip!();

    // `install <formula> --cask <cask>` treats *both* as casks, so the
    // formula name fails as an unknown cask.
    let out = env.run(&["install", "jq", "--cask", "rectangle"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        support::strip_ansi(&String::from_utf8_lossy(&out.stderr)).trim(),
        "Error: Cask 'jq' is unavailable: No Cask with this name exists."
    );
    assert!(!env.caskroom("rectangle").exists(), "nothing was installed");

    // `--formula` and `--cask` cannot both be given.
    let out = env.run(&["install", "--formula", "--cask", "jq"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot be used with"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A tap cask that shadows a core formula name: the formula wins, with
    // `NamedArgs#package_conflicts_message`.
    env.write_cask(
        "jq",
        &app_cask_rb(
            "jq",
            "9.9",
            "https://example.invalid/jq.zip",
            ":no_check",
            "Jq.app",
            "",
        ),
    );
    let out = env.run(&["info", "jq"]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("==> jq: stable "),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        support::strip_ansi(&String::from_utf8_lossy(&out.stderr)).trim(),
        "Warning: Treating jq as a formula. For the cask, use fixture/casks/jq or specify the \
         `--cask` flag. To silence this message, use the `--formula` flag."
    );

    // `-q` silences it, `--cask` picks the cask.
    let out = env.run(&["info", "-q", "jq"]);
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    let cask = env.stdout(&["info", "--cask", "jq"]);
    assert!(
        cask.starts_with("==> fixture/casks/jq (jq fixture): 9.9"),
        "{cask}"
    );
}
