//! `tap` integration tests. No network unless `FASTBREW_TEST_NETWORK=1`: the
//! remote is a local git repository cloned over `file://`.

use std::path::Path;
use std::process::Command;

use fastbrew::config::Config;
use fastbrew::platform::Host;
use fastbrew::rubylite;
use fastbrew::tap::{self, Tap};

mod support;
use support::{Sandbox, network_tests_enabled, sandbox_config, skip_unless_sandbox};

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

// ---------------------------------------------------------------------------
// End-to-end: a tap formula seen through the CLI
// ---------------------------------------------------------------------------

/// A formula with a `bottle do` block for the host tag whose `root_url` points
/// at a host that does not resolve, so the install stops at the download.
fn bottled_formula(tag: &str) -> String {
    format!(
        r#"class Bottled < Formula
  desc "Tap formula that ships a bottle"
  homepage "https://example.com/bottled"
  url "https://example.com/bottled/bottled-2.0.0.tar.gz"
  sha256 "5555555555555555555555555555555555555555555555555555555555555555"
  license "MIT"

  bottle do
    root_url "https://bottles.invalid/v2/tiger/bottled"
    sha256 cellar: :any_skip_relocation, {tag}: "6666666666666666666666666666666666666666666666666666666666666666"
  end
end
"#
    )
}

/// Build a git repository shaped like a tap around `Formula/bottled.rb`.
fn make_bottled_remote(dir: &Path, tag: &str) {
    std::fs::create_dir_all(dir.join("Formula")).expect("mkdir Formula");
    std::fs::write(dir.join("Formula/bottled.rb"), bottled_formula(tag)).expect("write");
    git(dir, &["init", "--initial-branch=main", "--quiet"]);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--quiet", "-m", "Add bottled"]);
}

#[test]
fn cli_reads_a_tap_formula_end_to_end() {
    let Some(sandbox) = Sandbox::new() else {
        eprintln!("no cached Homebrew API file available; skipping");
        return;
    };
    let tag = support::bottle_tag();
    let remote = tempfile::tempdir().expect("tempdir");
    make_bottled_remote(remote.path(), &tag);
    let url = format!("file://{}", remote.path().display());

    let out = sandbox.run(&["tap", "tiger/test", &url]);
    assert!(
        out.status.success(),
        "tap failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("==> Tapping tiger/test"), "{stderr}");
    assert!(stderr.contains("Tapped 1 formula ("), "{stderr}");

    // --- info, by full name and by bare name ----------------------------
    for reference in ["tiger/test/bottled", "bottled"] {
        let info = sandbox.stdout(&["info", reference]);
        assert!(
            info.starts_with("==> tiger/test/bottled: stable 2.0.0 (bottled)\n"),
            "{info}"
        );
        assert!(info.contains("\nTap: tiger/test\n"), "{info}");
        assert!(
            info.contains(&format!("From: {url}/Formula/bottled.rb\n")),
            "the From: line points at the tap's own file:\n{info}"
        );
        assert!(info.contains("\nLicense: MIT\n"), "{info}");
    }

    // --- search and tap-info --------------------------------------------
    let search = sandbox.stdout(&["search", "bottled"]);
    assert!(
        search.lines().any(|l| l == "tiger/test/bottled"),
        "{search}"
    );
    let tap_info = sandbox.stdout(&["tap-info", "tiger/test"]);
    assert!(
        tap_info.starts_with("tiger/test: Installed\n"),
        "{tap_info}"
    );
    assert!(tap_info.contains("\n1 formula\n"), "{tap_info}");
    assert!(tap_info.contains("\n==> Formulae\nbottled\n"), "{tap_info}");

    // --- deps ------------------------------------------------------------
    // The formula declares none, so both forms print nothing and succeed.
    assert_eq!(sandbox.stdout(&["deps", "tiger/test/bottled"]), "");
    assert_eq!(
        sandbox.stdout(&["deps", "--tree", "tiger/test/bottled"]),
        "tiger/test/bottled\n\n"
    );

    // --- the parsed bottle is what `ops::install` needs -------------------
    let cfg = Config::for_test(sandbox.prefix.parent().expect("sandbox root"));
    let t = Tap::parse("tiger/test").expect("parse");
    let host_tag = Host::detect().bottle_tag();
    let meta = fastbrew::api::taps::load_tap(&cfg, &t, &host_tag);
    let entry = meta
        .formula("bottled")
        .expect("the tap carries it")
        .expect("it parses")
        .entry;
    assert!(entry.has_bottle(), "a bottle for the host tag was found");
    assert_eq!(
        entry.bottle_checksum.as_deref(),
        Some("6666666666666666666666666666666666666666666666666666666666666666")
    );
    assert_eq!(
        entry.bottle_root_url.as_deref(),
        Some("https://bottles.invalid/v2/tiger/bottled"),
        "the `root_url` reaches `ops::install` on the entry"
    );

    // --- install: resolution succeeds, the download cannot ---------------
    let out = sandbox.run(&["install", "tiger/test/bottled"]);
    assert!(
        !out.status.success(),
        "an unreachable root_url cannot install"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("No available formula") && !stderr.contains("requires the tap"),
        "the name resolved, so the failure is about the download:\n{stderr}"
    );

    // --- untap ------------------------------------------------------------
    let out = sandbox.run(&["untap", "tiger/test"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = sandbox.run(&["info", "tiger/test/bottled"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("This command requires the tap tiger/test."),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn ambiguous_bare_names_list_every_tap() {
    let Some(sandbox) = Sandbox::new() else {
        eprintln!("no cached Homebrew API file available; skipping");
        return;
    };
    let tag = support::bottle_tag();
    let first = tempfile::tempdir().expect("tempdir");
    let second = tempfile::tempdir().expect("tempdir");
    make_bottled_remote(first.path(), &tag);
    make_bottled_remote(second.path(), &tag);

    for (name, dir) in [("aaa/one", first.path()), ("bbb/two", second.path())] {
        let url = format!("file://{}", dir.display());
        let out = sandbox.run(&["tap", name, &url]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // `TapFormulaAmbiguityError`.
    let out = sandbox.run(&["info", "bottled"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Formulae found in multiple taps:"),
        "{stderr}"
    );
    assert!(stderr.contains("* aaa/one/bottled"), "{stderr}");
    assert!(stderr.contains("* bbb/two/bottled"), "{stderr}");
    assert!(
        stderr.contains("Please use the fully-qualified name (e.g. aaa/one/bottled)"),
        "{stderr}"
    );

    // The qualified name still works.
    let info = sandbox.stdout(&["info", "bbb/two/bottled"]);
    assert!(info.contains("\nTap: bbb/two\n"), "{info}");
}

#[test]
fn taps_oven_sh_bun_and_reads_it() {
    if !network_tests_enabled() {
        eprintln!("set FASTBREW_TEST_NETWORK=1 to run network tests; skipping");
        return;
    }
    let Some(sandbox) = Sandbox::new() else {
        eprintln!("no cached Homebrew API file available; skipping");
        return;
    };
    let out = sandbox.run(&["tap", "oven-sh/bun"]);
    assert!(
        out.status.success(),
        "tap oven-sh/bun failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let info = sandbox.stdout(&["info", "oven-sh/bun/bun"]);
    assert!(info.starts_with("==> oven-sh/bun/bun: stable "), "{info}");
    assert!(info.contains("\nTap: oven-sh/bun\n"), "{info}");
    assert!(
        info.contains("From: https://github.com/oven-sh/homebrew-bun/blob/HEAD/Formula/bun.rb"),
        "{info}"
    );

    // `FromNameLoader` checks the core tap first, so the bare name is
    // homebrew/core's `bun`, exactly as `brew info bun` reports it.
    let core = sandbox.stdout(&["info", "bun"]);
    assert!(core.starts_with("==> bun: stable "), "{core}");
    assert!(!core.contains("Tap: oven-sh/bun"), "{core}");

    // Every tap formula is searchable by its full name.
    let search = sandbox.stdout(&["search", "bun"]);
    assert!(search.lines().any(|l| l == "oven-sh/bun/bun"), "{search}");

    let tap_info = sandbox.stdout(&["tap-info", "oven-sh/bun"]);
    assert!(
        tap_info.starts_with("oven-sh/bun: Installed\n"),
        "{tap_info}"
    );
    // The formula count moves with the tap, so only its shape is asserted.
    assert!(tap_info.contains(" formulae\n"), "{tap_info}");

    let out = sandbox.run(&["untap", "oven-sh/bun"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn update_reports_the_taps_it_moved() {
    let Some(sandbox) = Sandbox::new() else {
        eprintln!("no cached Homebrew API file available; skipping");
        return;
    };
    let tag = support::bottle_tag();
    let remote = tempfile::tempdir().expect("tempdir");
    make_bottled_remote(remote.path(), &tag);
    let url = format!("file://{}", remote.path().display());
    let out = sandbox.run(&["tap", "tiger/moving", &url]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Nothing moved upstream yet, and the API is unreachable, so the only
    // thing `update` can say is that everything is current.
    let offline = |args: &[&str]| {
        sandbox
            .cmd()
            .args(args)
            .env("HOMEBREW_API_DOMAIN", "http://127.0.0.1:1/api")
            .env("HOMEBREW_CURL_RETRIES", "0")
            .output()
            .expect("run fastbrew")
    };
    let out = offline(&["update"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "Already up-to-date."
    );

    // Add a formula upstream: the pull moves the tap and the report names it.
    std::fs::write(
        remote.path().join("Formula/second.rb"),
        bottled_formula(&tag).replace("class Bottled", "class Second"),
    )
    .expect("write");
    git(remote.path(), &["add", "-A"]);
    git(remote.path(), &["commit", "--quiet", "-m", "Add second"]);

    let out = offline(&["update"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("Updated 1 tap (tiger/moving).\n"),
        "{text}"
    );
    assert!(text.contains("==> New Formulae\n"), "{text}");
    assert!(text.contains("tiger/moving/second"), "{text}");

    // The new formula is immediately resolvable.
    let info = sandbox.stdout(&["info", "tiger/moving/second"]);
    assert!(info.contains("\nTap: tiger/moving\n"), "{info}");
}

#[test]
fn commands_lists_a_taps_external_command() {
    let Some(sandbox) = Sandbox::new() else {
        eprintln!("no cached Homebrew API file available; skipping");
        return;
    };
    let remote = tempfile::tempdir().expect("tempdir");
    let cmd_dir = remote.path().join("cmd");
    std::fs::create_dir_all(&cmd_dir).expect("mkdir cmd");
    let script = cmd_dir.join("brew-greet");
    std::fs::write(&script, "#!/bin/sh\necho hi\n").expect("write");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    git(remote.path(), &["init", "--initial-branch=main", "--quiet"]);
    git(remote.path(), &["add", "-A"]);
    git(
        remote.path(),
        &["commit", "--quiet", "-m", "Add brew-greet"],
    );

    let url = format!("file://{}", remote.path().display());
    let out = sandbox.run(&["tap", "tiger/cmds", &url]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Tapped 1 command ("),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let commands = sandbox.stdout(&["commands"]);
    assert!(commands.contains("\n==> External commands\n"), "{commands}");
    assert!(
        commands.lines().any(|l| l == "greet"),
        "the tap's command is listed:\n{commands}"
    );

    let tap_info = sandbox.stdout(&["tap-info", "tiger/cmds"]);
    assert!(tap_info.contains("\n==> Commands\ngreet\n"), "{tap_info}");
}
