//! End-to-end rubylite tests over embedded formula and cask texts.

use serde_json::json;

use super::*;
use crate::platform::BottleTag;

const BUN: &str = include_str!("testdata/bun.rb");
const PYTHON: &str = include_str!("testdata/python@3.14.rb");
const HOARD: &str = include_str!("testdata/hoard.rb");
const PEEKABOO: &str = include_str!("testdata/peekaboo.rb");
const RECTANGLE: &str = include_str!("testdata/rectangle.rb");
const CODEXBAR: &str = include_str!("testdata/codexbar.rb");

fn tag(s: &str) -> BottleTag {
    BottleTag::parse(s).unwrap()
}

fn formula(source: &str, name: &str, t: &str) -> TapFormula {
    parse_formula_source(source, name, "user/repo", &tag(t))
        .unwrap_or_else(|e| panic!("{name} on {t}: {e}"))
}

/// A Ruby `post_install` only the formula DSL can run has to be visible on
/// the entry, or the installer would skip it without a word.
#[test]
fn records_a_ruby_post_install_method() {
    let source = r#"class Withpost < Formula
  desc "Formula with a Ruby post_install"
  homepage "https://example.invalid/withpost"
  url "https://example.invalid/withpost-1.0.tar.gz"
  sha256 "1111111111111111111111111111111111111111111111111111111111111111"

  def install
    bin.install "withpost"
  end

  def post_install
    (var/"marker").write("x")
  end
end
"#;
    let f = formula(source, "withpost", "arm64_tahoe");
    assert!(f.has_install_method);
    assert!(f.entry.post_install_defined);
    // Nothing declarative is generated for it: the Ruby is the only source.
    assert!(f.entry.post_install_steps.is_empty());

    // A formula without one keeps the flag clear.
    let plain = formula(BUN, "bun", "arm64_tahoe");
    assert!(!plain.entry.post_install_defined);
}

#[test]
fn parses_bun_from_the_host_tap() {
    // `bun.rb` branches with `if OS.mac?` / `Hardware::CPU.arm?`, not `on_macos`.
    let mac = formula(BUN, "bun", "arm64_tahoe");
    assert_eq!(mac.entry.name, "bun");
    assert_eq!(mac.entry.tap, "user/repo");
    assert_eq!(mac.entry.stable_version.as_deref(), Some("1.4.2"));
    assert_eq!(
        mac.entry.desc.as_deref(),
        Some(
            "Incredibly fast JavaScript runtime, bundler, transpiler and package manager - all in one."
        )
    );
    assert_eq!(mac.entry.homepage.as_deref(), Some("https://bun.sh/"));
    assert_eq!(mac.entry.license_string().as_deref(), Some("MIT"));
    assert_eq!(
        mac.entry.stable_url(),
        Some("https://github.com/oven-sh/bun/releases/download/bun-v1.4.2/bun-darwin-aarch64.zip")
    );
    assert_eq!(
        mac.entry.stable_checksum.as_deref(),
        Some("90987a3a16d7db556d886ac3d551e7b6d3edf0a1cf43acaed622e8676be1d12f")
    );
    assert!(mac.has_install_method);
    // The livecheck regex and `def test` must not leak into the metadata.
    assert!(mac.entry.stable_dependencies.is_empty());

    let intel = formula(BUN, "bun", "sonoma");
    assert_eq!(
        intel.entry.stable_url(),
        Some("https://github.com/oven-sh/bun/releases/download/bun-v1.4.2/bun-darwin-x64.zip")
    );

    let linux = formula(BUN, "bun", "arm64_linux");
    assert_eq!(
        linux.entry.stable_url(),
        Some("https://github.com/oven-sh/bun/releases/download/bun-v1.4.2/bun-linux-aarch64.zip")
    );
}

#[test]
fn parses_versioned_formula() {
    let f = formula(PYTHON, "python@3.14", "arm64_tahoe");
    assert_eq!(f.entry.name, "python@3.14");
    assert_eq!(f.entry.stable_version.as_deref(), Some("3.14.2"));
    assert_eq!(f.entry.revision, 1);
    assert_eq!(f.entry.version_scheme, 2);
    assert_eq!(f.entry.pkg_version(), "3.14.2_1");
    assert_eq!(f.entry.license_string().as_deref(), Some("Python-2.0"));

    // bottle do
    assert_eq!(
        f.bottle_root_url.as_deref(),
        Some("https://ghcr.io/v2/homebrew/core")
    );
    assert_eq!(f.entry.bottle_rebuild, 3);
    assert_eq!(
        f.entry.bottle_checksum.as_deref(),
        Some("aaaa000000000000000000000000000000000000000000000000000000000001")
    );
    assert_eq!(f.entry.bottle_cellar.as_deref(), Some(":any"));

    // depends_on / uses_from_macos
    let deps = f.entry.dependencies();
    assert_eq!(deps.len(), 3, "{:?}", f.entry.stable_dependencies);
    assert!(deps[0].is_build() && deps[0].name == "pkgconf");
    assert_eq!(deps[1].name, "openssl@3");
    assert!(deps[1].tags.is_empty());
    assert!(deps[2].is_build() && deps[2].is_test());
    let ufm = f.entry.uses_from_macos();
    assert_eq!(ufm.len(), 3);
    assert_eq!(ufm[0].dep.name, "bzip2");
    assert_eq!(ufm[1].since.as_deref(), Some("sequoia"));
    assert!(ufm[2].dep.is_build());

    assert_eq!(
        f.entry.keg_only().unwrap().0,
        crate::model::KegOnly::VersionedFormula
    );
    assert_eq!(f.entry.conflicts_with()[0].0, "python-build");
    assert_eq!(
        f.entry.conflicts_with()[0].1.as_deref(),
        Some("both install `python-build` binaries")
    );
    assert_eq!(
        f.entry.link_overwrite_paths,
        vec!["bin/python3", "lib/python3.14/*"]
    );
    assert!(f.has_install_method);

    let caveats = f.entry.caveats.clone().unwrap();
    assert!(
        caveats.contains("$HOMEBREW_PREFIX/opt/python@3.14/bin/python3.14"),
        "{caveats}"
    );
    assert!(
        caveats.contains("$HOMEBREW_PREFIX/opt/python@3.14/libexec/bin"),
        "{caveats}"
    );

    // The bottle for a different platform is not picked up.
    let linux = formula(PYTHON, "python@3.14", "x86_64_linux");
    assert_eq!(
        linux.entry.bottle_checksum.as_deref(),
        Some("aaaa000000000000000000000000000000000000000000000000000000000003")
    );
    // `cellar: :any_skip_relocation` is stored as an absent cellar.
    assert_eq!(linux.entry.bottle_cellar, None);
}

#[test]
fn parses_on_os_blocks_and_service() {
    let mac = formula(HOARD, "hoard", "arm64_tahoe");
    assert_eq!(
        mac.entry.stable_url(),
        Some("https://example.com/hoard/v2.7.0/hoard-2.7.0-darwin-arm64.tar.gz")
    );
    assert_eq!(
        mac.entry.stable_checksum.as_deref(),
        Some("1111111111111111111111111111111111111111111111111111111111111111")
    );
    assert_eq!(
        mac.entry
            .dependencies()
            .iter()
            .map(|d| d.name.clone())
            .collect::<Vec<_>>(),
        vec!["openssl@3"]
    );
    assert_eq!(
        mac.entry.license_string().as_deref(),
        Some("(MIT or Apache-2.0)")
    );
    assert_eq!(
        mac.entry.keg_only().unwrap().0,
        crate::model::KegOnly::Reason("it shadows the system hoard".into())
    );
    assert_eq!(mac.entry.bottle_rebuild, 2);
    assert_eq!(
        mac.entry.bottle_checksum.as_deref(),
        Some("4444444444444444444444444444444444444444444444444444444444444444")
    );

    let intel = formula(HOARD, "hoard", "sonoma");
    assert_eq!(
        intel.entry.stable_checksum.as_deref(),
        Some("2222222222222222222222222222222222222222222222222222222222222222")
    );

    let linux = formula(HOARD, "hoard", "x86_64_linux");
    assert_eq!(
        linux.entry.stable_checksum.as_deref(),
        Some("3333333333333333333333333333333333333333333333333333333333333333")
    );
    assert_eq!(
        linux
            .entry
            .dependencies()
            .iter()
            .map(|d| d.name.clone())
            .collect::<Vec<_>>(),
        vec!["glibc"]
    );
    // The legacy `sha256 "..." => :tag` form.
    assert_eq!(
        linux.entry.bottle_checksum.as_deref(),
        Some("5555555555555555555555555555555555555555555555555555555555555555")
    );

    // `service do` maps into the API shape.
    assert_eq!(
        mac.entry.service_run_args,
        vec![json!([
            "$HOMEBREW_PREFIX/opt/hoard/bin/hoard",
            "--config",
            "$HOMEBREW_PREFIX/etc/hoard.conf"
        ])]
    );
    assert_eq!(
        mac.entry.service_args,
        vec![
            json!([":run_type", ":immediate"]),
            json!([":keep_alive", {":successful_exit": false}]),
            json!([":require_root", true]),
            json!([":environment_variables", {
                ":PATH": "$HOMEBREW_PREFIX/bin:$HOMEBREW_PREFIX/sbin:/usr/bin:/bin:/usr/sbin:/sbin",
                ":HOARD_HOME": "$HOMEBREW_PREFIX/var/hoard"
            }]),
            json!([":working_dir", "$HOMEBREW_PREFIX/var"]),
            json!([":log_path", "$HOMEBREW_PREFIX/var/log/hoard.log"]),
            json!([":error_log_path", "$HOMEBREW_PREFIX/var/log/hoard.err.log"]),
            json!([":stop_timeout", 30]),
            json!([":process_type", ":background"]),
        ]
    );
}

#[test]
fn service_block_drives_plist_generation() {
    let f = formula(HOARD, "hoard", "arm64_tahoe");
    let cfg = crate::services::test_support::config_with_prefix("/opt/homebrew", "/Users/fastbrew");
    let plist = crate::services::plist::to_plist(&cfg, &f.entry)
        .unwrap()
        .expect("hoard has a service");
    assert!(
        plist.contains("<string>homebrew.mxcl.hoard</string>"),
        "{plist}"
    );
    assert!(plist.contains("<string>/opt/homebrew/opt/hoard/bin/hoard</string>"));
    assert!(plist.contains("<string>/opt/homebrew/etc/hoard.conf</string>"));
    assert!(plist.contains("<key>ProcessType</key>\n\t<string>Background</string>"));
    assert!(plist.contains("<key>SuccessfulExit</key>\n\t\t<false/>"));
    assert!(plist.contains("<key>ExitTimeOut</key>\n\t<integer>30</integer>"));
    assert!(plist.contains("/usr/bin:/bin:/usr/sbin:/sbin"));
}

#[test]
fn parses_formula_without_explicit_version() {
    let f = formula(PEEKABOO, "peekaboo", "arm64_tahoe");
    // The version comes from the GitHub release tag in the URL.
    assert_eq!(f.entry.stable_version.as_deref(), Some("4.3.0"));
    assert_eq!(
        f.entry.stable_checksum.as_deref(),
        Some("fec965e4bd6371b8fb017fb582e8d31c6a59628f77e266878f45cf1d4844836f")
    );
    assert_eq!(f.entry.license_string().as_deref(), Some("MIT"));
    // `depends_on macos: :sequoia` is a requirement, not a dependency.
    assert!(f.entry.stable_dependencies.is_empty());
    assert!(f.has_install_method);
    let caveats = f.entry.caveats.clone().unwrap();
    assert!(
        caveats.starts_with("Peekaboo requires Screen Recording permission"),
        "{caveats}"
    );
    assert!(caveats.contains("peekaboo config init"));
}

/// `rubylite` has no URL heuristics of its own: it detects a missing version
/// with `version::Version::detect_from_url`, the port of `Version.parse`.
#[test]
fn detects_versions_from_urls() {
    let cases = [
        (
            "https://github.com/openclaw/Peekaboo/releases/download/v4.3.0/peekaboo-macos-universal.tar.gz",
            "4.3.0",
        ),
        (
            "https://github.com/x/y/archive/refs/tags/v1.2.3.tar.gz",
            "1.2.3",
        ),
        (
            "https://www.python.org/ftp/python/3.14.2/Python-3.14.2.tar.xz",
            "3.14.2",
        ),
        ("https://example.com/hello-2.12.3.tar.gz", "2.12.3"),
        ("https://example.com/foo/tree-2.2.1.tgz", "2.2.1"),
        ("https://example.com/jq-1.8.2.zip", "1.8.2"),
    ];
    for (url, want) in cases {
        let got = crate::version::Version::detect_from_url(url).map(|v| v.to_string());
        assert_eq!(got.as_deref(), Some(want), "{url}");
    }
}

#[test]
fn parses_casks() {
    let c = parse_cask_source(RECTANGLE, "rectangle", "homebrew/cask", &tag("arm64_tahoe"))
        .expect("rectangle parses");
    assert_eq!(c.token, "rectangle");
    assert_eq!(c.version.as_deref(), Some("1.100"));
    assert_eq!(
        c.sha256.as_deref(),
        Some("5cfbe9b68a558458302c5305cb7060491f68029338e2a4561c54a2981eb8622f")
    );
    assert_eq!(
        c.url(),
        Some("https://github.com/rxhanson/Rectangle/releases/download/v1.100/Rectangle1.100.dmg")
    );
    assert_eq!(c.names, vec!["Rectangle"]);
    assert_eq!(
        c.desc.as_deref(),
        Some("Move and resize windows using keyboard shortcuts or snap areas")
    );
    assert_eq!(c.homepage.as_deref(), Some("https://rectangleapp.com/"));
    assert!(c.auto_updates);
    assert_eq!(c.cask_dependencies(), Vec::<String>::new());
    assert_eq!(
        c.depends_on_args,
        Some(json!({":macos": {">=": [":ventura"]}}))
    );
    assert_eq!(
        c.conflicts_with_args,
        Some(json!({":cask": "rectangle-pro"}))
    );

    let artifacts = c.artifacts();
    assert_eq!(artifacts[0].kind, "app");
    assert_eq!(artifacts[0].args, vec![json!(["Rectangle.app"])]);
    assert_eq!(artifacts[1].kind, "binary");
    assert_eq!(
        artifacts[1].args,
        vec![json!([
            "$APPDIR/Rectangle.app/Contents/MacOS/Rectangle",
            {":target": "rectangle"}
        ])]
    );
    assert_eq!(artifacts[2].kind, "uninstall");
    assert_eq!(
        artifacts[2].args,
        vec![json!({":quit": "com.knollsoft.Rectangle", ":login_item": "Rectangle"})]
    );
    assert_eq!(artifacts[3].kind, "zap");
    assert_eq!(
        artifacts[3].args[0][":trash"][0],
        json!("~/Library/Application Scripts/com.knollsoft.RectangleLauncher")
    );
    assert!(
        c.caveats_text()
            .unwrap()
            .contains("Accessibility permission")
    );
}

#[test]
fn parses_cask_from_the_host_tap() {
    let c = parse_cask_source(CODEXBAR, "codexbar", "steipete/tap", &tag("arm64_tahoe"))
        .expect("codexbar parses");
    assert_eq!(c.version.as_deref(), Some("0.60.1"));
    assert_eq!(
        c.url(),
        Some(
            "https://github.com/steipete/CodexBar/releases/download/v0.60.1/CodexBar-macos-universal-0.60.1.zip"
        )
    );
    assert_eq!(c.display_name(), "CodexBar");
    assert!(c.auto_updates);
    assert_eq!(c.depends_on_args, Some(json!({":macos": ":sonoma"})));
    let artifacts = c.artifacts();
    assert_eq!(artifacts[0].kind, "app");
    assert_eq!(artifacts[1].kind, "binary");
    assert_eq!(
        artifacts[1].args,
        vec![json!([
            "$APPDIR/CodexBar.app/Contents/Helpers/CodexBarCLI",
            {":target": "codexbar"}
        ])]
    );
    assert_eq!(artifacts[2].kind, "zap");
    assert_eq!(c.tap(), "steipete/tap");
}

#[test]
fn unparseable_files_need_delegation() {
    assert!(parse_formula_source("puts 'hi'\n", "x", "user/repo", &tag("arm64_tahoe")).is_err());
    assert!(
        parse_formula_source(
            "class X < Formula\n  desc \"no url\"\nend\n",
            "x",
            "user/repo",
            &tag("arm64_tahoe")
        )
        .is_err()
    );
    assert!(parse_cask_source("puts 'hi'\n", "x", "user/repo", &tag("arm64_tahoe")).is_err());
    // A cask with arbitrary Ruby in a flight block cannot be run natively.
    let ruby = "cask \"x\" do\n  version \"1\"\n  url \"https://e/x-1.zip\"\n  \
                postflight do\n    system_command \"/bin/ls\"\n  end\n  app \"X.app\"\nend\n";
    assert!(parse_cask_source(ruby, "x", "user/repo", &tag("arm64_tahoe")).is_err());
}

#[test]
#[ignore = "reads the host taps; run with --ignored for a smoke check"]
fn host_taps_smoke() {
    use std::path::Path;
    let root = Path::new("/opt/homebrew/Library/Taps");
    if !root.is_dir() {
        return;
    }
    let host = tag("arm64_tahoe");
    let mut ok = 0;
    let mut failed: Vec<String> = vec![];
    for entry in walkdir::WalkDir::new(root).into_iter().flatten() {
        let p = entry.path();
        if p.extension().is_none_or(|e| e != "rb") {
            continue;
        }
        let s = p.to_string_lossy().into_owned();
        let is_cask = s.contains("/Casks/");
        let name = name_from_path(p);
        let source = std::fs::read_to_string(p).unwrap_or_default();
        let result = if is_cask {
            parse_cask_source(&source, &name, "t/t", &host).map(|_| ())
        } else {
            parse_formula_source(&source, &name, "t/t", &host).map(|_| ())
        };
        match result {
            Ok(()) => ok += 1,
            Err(e) => failed.push(format!("{s}: {e}")),
        }
    }
    println!("parsed {ok} files, {} failed", failed.len());
    for f in &failed {
        println!("  {f}");
    }
}

#[test]
fn derives_names_from_paths() {
    use std::path::Path;
    assert_eq!(
        name_from_path(Path::new("/t/Formula/p/python@3.14.rb")),
        "python@3.14"
    );
    assert_eq!(
        name_from_path(Path::new("/t/Casks/Rectangle.rb")),
        "rectangle"
    );
}
