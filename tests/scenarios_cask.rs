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
