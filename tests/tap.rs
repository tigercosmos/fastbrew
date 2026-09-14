//! `tap` integration tests. No network: the remote is a local git repository
//! cloned over `file://`.

use std::path::Path;
use std::process::Command;

use fastbrew::platform::Host;
use fastbrew::rubylite;
use fastbrew::tap::{self, Tap};

mod support;
use support::{sandbox_config, skip_unless_sandbox};

const FOO_RB: &str = r#"class Foo < Formula
  desc "Test formula in a local tap"
  homepage "https://example.com/foo"
  url "https://example.com/foo/foo-1.2.3.tar.gz"
  sha256 "1111111111111111111111111111111111111111111111111111111111111111"
  license "MIT"

  depends_on "bar" => :build

  def install
    bin.install "foo"
  end
end
"#;

const BAR_RB: &str = r#"cask "bar" do
  version "3.2.1"
  sha256 "2222222222222222222222222222222222222222222222222222222222222222"

  url "https://example.com/bar/Bar-#{version}.dmg"
  name "Bar"
  desc "Test cask in a local tap"
  homepage "https://example.com/bar"

  app "Bar.app"

  zap trash: [
    "~/Library/Preferences/com.example.bar.plist",
  ]
end
"#;

const BAZ_RB: &str = r#"class Baz < Formula
  desc "Second formula, added after tapping"
  homepage "https://example.com/baz"
  url "https://example.com/baz/baz-0.1.0.tar.gz"
  sha256 "3333333333333333333333333333333333333333333333333333333333333333"
end
"#;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("user.name=fastbrew tests")
        .arg("-c")
        .arg("user.email=tests@example.invalid")
        .arg("-c")
        .arg("commit.gpgsign=false")
        .arg("-c")
        .arg("protocol.file.allow=always")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// A local git repository shaped like a tap, used as the clone source.
fn make_remote(dir: &Path) {
    std::fs::create_dir_all(dir.join("Formula")).expect("mkdir Formula");
    std::fs::create_dir_all(dir.join("Casks")).expect("mkdir Casks");
    std::fs::write(dir.join("Formula/foo.rb"), FOO_RB).expect("write foo.rb");
    std::fs::write(dir.join("Casks/bar.rb"), BAR_RB).expect("write bar.rb");
    git(dir, &["init", "--initial-branch=main", "--quiet"]);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--quiet", "-m", "Add foo and bar"]);
}

#[test]
fn tap_enumerate_parse_update_and_untap() {
    if skip_unless_sandbox() {
        return;
    }
    let cfg = sandbox_config();
    let host_tag = Host::detect().bottle_tag();
    let remote_dir = tempfile::tempdir().expect("tempdir");
    make_remote(remote_dir.path());
    let url = format!("file://{}", remote_dir.path().display());

    // --- tap ------------------------------------------------------------
    tap::tap(&cfg, "testuser/test", Some(&url), false, true).expect("tap");
    let t = Tap::parse("testuser/test").expect("parse");
    assert_eq!(t.name(), "testuser/test");
    let path = t.path(&cfg);
    assert_eq!(path, cfg.taps_dir().join("testuser/homebrew-test"));
    assert!(path.join("Formula/foo.rb").is_file());
    assert!(path.join(".git").exists());
    assert!(tap::installed_taps(&cfg).contains(&t));
    assert!(tap::tap_git_head(&cfg, &t).is_some());

    // Tapping again is refused with Homebrew's wording.
    let err = tap::tap_with_outcome(&cfg, "testuser/test", Some(&url), false, true)
        .expect_err("already tapped");
    assert_eq!(err.to_string(), "Tap testuser/test already tapped.\n");

    // The core taps need `--force`.
    let err = tap::tap_with_outcome(&cfg, "homebrew/core", None, false, true)
        .expect_err("core tap refused");
    assert!(
        err.to_string()
            .starts_with("Tapping homebrew/core is no longer typically necessary."),
        "{err}"
    );

    // --- enumerate ------------------------------------------------------
    let formulae = tap::formula_files(&cfg, &t);
    assert_eq!(formulae.len(), 1);
    assert_eq!(formulae[0].0, "foo");
    assert_eq!(formulae[0].1, path.join("Formula/foo.rb"));

    let casks = tap::cask_files(&cfg, &t);
    assert_eq!(casks.len(), 1);
    assert_eq!(casks[0].0, "bar");

    // --- parse ----------------------------------------------------------
    let parsed = rubylite::parse_formula_file(&formulae[0].1, &t.name(), &host_tag).expect("parse");
    assert_eq!(parsed.entry.name, "foo");
    assert_eq!(parsed.entry.tap, "testuser/test");
    assert_eq!(parsed.entry.full_name(), "testuser/test/foo");
    assert_eq!(parsed.entry.stable_version.as_deref(), Some("1.2.3"));
    assert_eq!(
        parsed.entry.stable_url(),
        Some("https://example.com/foo/foo-1.2.3.tar.gz")
    );
    assert_eq!(parsed.entry.license_string().as_deref(), Some("MIT"));
    assert!(parsed.entry.dependencies()[0].is_build());
    assert!(parsed.has_install_method);

    let cask =
        rubylite::parse_cask_file_for(&casks[0].1, &t.name(), &host_tag).expect("parse cask");
    assert_eq!(cask.token, "bar");
    assert_eq!(cask.version.as_deref(), Some("3.2.1"));
    assert_eq!(cask.url(), Some("https://example.com/bar/Bar-3.2.1.dmg"));
    assert_eq!(cask.display_name(), "Bar");
    assert_eq!(cask.artifacts()[0].kind, "app");
    assert_eq!(cask.ruby_source_path.as_deref(), Some("Casks/bar.rb"));

    // --- update ---------------------------------------------------------
    assert!(
        tap::update_all(&cfg, true).expect("update").is_empty(),
        "nothing changed upstream yet"
    );

    std::fs::write(remote_dir.path().join("Formula/baz.rb"), BAZ_RB).expect("write baz.rb");
    git(remote_dir.path(), &["add", "-A"]);
    git(remote_dir.path(), &["commit", "--quiet", "-m", "Add baz"]);

    let changed = tap::update_all(&cfg, true).expect("update");
    assert_eq!(changed, vec![t.clone()], "the tap moved");
    assert!(path.join("Formula/baz.rb").is_file());
    let names: Vec<String> = tap::formula_files(&cfg, &t)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert_eq!(names, vec!["baz", "foo"]);

    // --- untap ----------------------------------------------------------
    // An installed formula from the tap blocks untapping.
    let keg = cfg.cellar.join("foo/1.2.3");
    std::fs::create_dir_all(&keg).expect("mkdir keg");
    std::fs::write(
        keg.join("INSTALL_RECEIPT.json"),
        serde_json::json!({"source": {"tap": "testuser/test"}}).to_string(),
    )
    .expect("write receipt");
    let err = tap::untap(&cfg, "testuser/test", false).expect_err("refused");
    assert_eq!(
        err.to_string(),
        "Refusing to untap testuser/test because it contains the following installed formulae:\n\
         testuser/test/foo\n"
    );
    assert!(path.is_dir());

    std::fs::remove_dir_all(cfg.cellar.join("foo")).expect("remove keg");
    tap::untap(&cfg, "testuser/test", false).expect("untap");
    assert!(!path.exists());
    assert!(!tap::installed_taps(&cfg).contains(&t));

    let err = tap::untap(&cfg, "testuser/test", false).expect_err("not tapped");
    assert_eq!(err.to_string(), "No available tap testuser/test.\n");
}

#[test]
fn tap_with_homebrew_formula_directory_and_aliases() {
    if skip_unless_sandbox() {
        return;
    }
    let cfg = sandbox_config();
    let remote_dir = tempfile::tempdir().expect("tempdir");
    let root = remote_dir.path();
    std::fs::create_dir_all(root.join("HomebrewFormula")).expect("mkdir");
    std::fs::create_dir_all(root.join("Aliases")).expect("mkdir");
    std::fs::write(root.join("HomebrewFormula/foo.rb"), FOO_RB).expect("write");
    std::os::unix::fs::symlink("../HomebrewFormula/foo.rb", root.join("Aliases/foo-alias"))
        .expect("symlink");
    git(root, &["init", "--initial-branch=main", "--quiet"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "--quiet", "-m", "Add foo"]);

    let url = format!("file://{}", root.display());
    tap::tap(&cfg, "other/alt", Some(&url), false, true).expect("tap");
    let t = Tap::parse("other/alt").expect("parse");

    let formulae = tap::formula_files(&cfg, &t);
    assert_eq!(formulae.len(), 1);
    assert_eq!(formulae[0].0, "foo");
    assert!(formulae[0].1.ends_with("HomebrewFormula/foo.rb"));

    let aliases = tap::aliases(&cfg, &t);
    assert_eq!(aliases.get("foo-alias").map(String::as_str), Some("foo"));

    tap::untap(&cfg, "other/alt", false).expect("untap");
}
