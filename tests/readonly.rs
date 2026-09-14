//! Integration tests for fastbrew's read-only commands.
//!
//! Every test runs the built binary inside an isolated sandbox: a private
//! prefix (a fresh temp directory per test) plus a shared cache seeded with a
//! copy of the internal packages file, exactly as `scripts/sandbox.sh` does.
//! Nothing here touches `/opt/homebrew` or the user's Homebrew cache; the
//! sandbox script copies the API file once and the tests hardlink it.
//!
//! Run them with `scripts/sandbox.sh test`. Tests needing the network are
//! gated on `FASTBREW_TEST_NETWORK=1`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use assert_cmd::prelude::*;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Sandbox plumbing
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

/// The cached packages file to seed sandboxes from: the ambient
/// `HOMEBREW_CACHE` when running under `scripts/sandbox.sh`, otherwise the
/// sandbox the script creates on demand.
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
        // Ask the sandbox script for (and if needed create) its environment.
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

/// A cache directory shared by every test (the API file and the fast index are
/// expensive to rebuild), seeded once.
fn shared_cache() -> Option<&'static PathBuf> {
    static CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let seed = seed_packages_file()?;
            let cache = repo_root().join("target/sandbox-tests/cache");
            let target = cache.join(format!("api/internal/packages.{}.jws.json", bottle_tag()));
            std::fs::create_dir_all(target.parent()?).ok()?;
            if !target.is_file() {
                // Hardlink to avoid copying 15 MB per run; fall back to a copy.
                if std::fs::hard_link(seed, &target).is_err() {
                    std::fs::copy(seed, &target).ok()?;
                }
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
    /// Build a sandbox, or `None` when no cached API file is available.
    fn new() -> Option<Sandbox> {
        let cache = shared_cache()?.clone();
        let dir = TempDir::new().ok()?;
        let prefix = dir.path().join("prefix");
        let home = dir.path().join("home");
        for sub in [
            "bin",
            "sbin",
            "etc",
            "lib",
            "share",
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
        std::fs::create_dir_all(&home).ok()?;
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
            .env("HOMEBREW_NO_ENV_HINTS", "1")
            .env("HOMEBREW_NO_COLOR", "1")
            .env("FASTBREW_REQUIRE_SANDBOX", "1")
            // Never hand a test invocation to the host's Ruby `brew`.
            .env("FASTBREW_NO_DELEGATE", "1")
            .env_remove("HOMEBREW_API_DOMAIN")
            .env_remove("HOMEBREW_COLOR");
        cmd
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.cmd().args(args).output().expect("run fastbrew")
    }

    fn stdout(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "`fastbrew {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Write a minimal keg plus its `opt` and `linked` records.
    fn add_keg(&self, name: &str, version: &str, on_request: bool) {
        let keg = self.prefix.join("Cellar").join(name).join(version);
        std::fs::create_dir_all(keg.join("bin")).unwrap();
        std::fs::write(keg.join("bin").join(name), "#!/bin/sh\n").unwrap();
        let receipt = serde_json::json!({
            "homebrew_version": "6.0.22",
            "used_options": [],
            "unused_options": [],
            "built_as_bottle": true,
            "poured_from_bottle": true,
            "loaded_from_api": true,
            "loaded_from_internal_api": true,
            "installed_as_dependency": !on_request,
            "installed_on_request": on_request,
            "changed_files": [],
            "time": 1778031361u64,
            "source_modified_time": 1773700688u64,
            "compiler": "clang",
            "aliases": [],
            "runtime_dependencies": [],
            "source": {
                "spec": "stable",
                "versions": {"stable": version, "head": null, "version_scheme": 0,
                             "compatibility_version": null},
                "path": self.cache.join("api/internal").to_string_lossy(),
                "tap_git_head": null,
                "tap": "homebrew/core"
            },
            "arch": "arm64",
            "built_on": {"os": "Macintosh", "os_version": "macOS 26", "cpu_family": "dunno",
                         "xcode": "26.3", "clt": "26.3", "preferred_perl": "5.34"}
        });
        std::fs::write(
            keg.join("INSTALL_RECEIPT.json"),
            serde_json::to_string_pretty(&receipt).unwrap(),
        )
        .unwrap();
        symlink(
            &format!("../Cellar/{name}/{version}"),
            &self.prefix.join("opt").join(name),
        );
        symlink(
            &format!("../../../Cellar/{name}/{version}"),
            &self.prefix.join("var/homebrew/linked").join(name),
        );
    }
}

fn symlink(target: &str, link: &Path) {
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// Skip a test (with a note) when no cached API data is available.
macro_rules! sandbox_or_skip {
    () => {
        match Sandbox::new() {
            Some(s) => s,
            None => {
                eprintln!("no cached Homebrew API file available; skipping");
                return;
            }
        }
    };
}

fn network_tests_enabled() -> bool {
    std::env::var("FASTBREW_TEST_NETWORK").is_ok_and(|v| !v.is_empty())
}

// ---------------------------------------------------------------------------
// Tests that need no API data
// ---------------------------------------------------------------------------

#[test]
fn version_prints_homebrew_then_fastbrew() {
    let sandbox = sandbox_or_skip!();
    let out = sandbox.stdout(&["--version"]);
    let mut lines = out.lines();
    assert_eq!(lines.next(), Some("Homebrew 6.0.22"));
    assert_eq!(lines.next(), Some("fastbrew 0.1.0"));
    assert_eq!(lines.next(), None);

    // `-v` in command position is Homebrew's alias for `--version`.
    assert_eq!(sandbox.stdout(&["-v"]), out);
}

#[test]
fn prefix_and_friends_print_paths() {
    let sandbox = sandbox_or_skip!();
    assert_eq!(
        sandbox.stdout(&["--prefix"]).trim(),
        sandbox.prefix.to_string_lossy()
    );
    assert_eq!(
        sandbox.stdout(&["--cellar"]).trim(),
        sandbox.prefix.join("Cellar").to_string_lossy()
    );
    assert_eq!(
        sandbox.stdout(&["--cache"]).trim(),
        sandbox.cache.to_string_lossy()
    );
    assert_eq!(
        sandbox.stdout(&["--repository"]).trim(),
        sandbox.prefix.to_string_lossy()
    );
    assert_eq!(
        sandbox.stdout(&["--caskroom"]).trim(),
        sandbox.prefix.join("Caskroom").to_string_lossy()
    );
    assert_eq!(
        sandbox.stdout(&["--prefix", "jq"]).trim(),
        sandbox.prefix.join("opt/jq").to_string_lossy()
    );
}

#[test]
fn shellenv_matches_homebrew() {
    let sandbox = sandbox_or_skip!();
    let prefix = sandbox.prefix.to_string_lossy().into_owned();
    let zsh = sandbox.stdout(&["shellenv", "zsh"]);
    assert!(zsh.contains(&format!("export HOMEBREW_PREFIX=\"{prefix}\";\n")));
    assert!(zsh.contains(&format!(
        "fpath[1,0]=\"{prefix}/share/zsh/site-functions\";\nexport FPATH;\n"
    )));
    assert!(zsh.contains("${PATH+:$PATH}\";"));

    let bash = sandbox.stdout(&["shellenv", "bash"]);
    assert!(!bash.contains("fpath"));
    assert!(bash.contains(&format!("export HOMEBREW_CELLAR=\"{prefix}/Cellar\";")));

    let fish = sandbox.stdout(&["shellenv", "fish"]);
    assert!(fish.contains("set --global --export HOMEBREW_PREFIX"));
    assert!(fish.contains("fish_add_path --global --move --path"));

    let csh = sandbox.stdout(&["shellenv", "csh"]);
    assert!(csh.starts_with(&format!("setenv HOMEBREW_PREFIX {prefix};\n")));
    assert!(csh.contains("setenv PATH \""));
}

// ---------------------------------------------------------------------------
// Query commands against the cached API
// ---------------------------------------------------------------------------

#[test]
fn info_prints_homebrews_layout() {
    let sandbox = sandbox_or_skip!();
    let out = sandbox.stdout(&["info", "jq"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "==> jq: stable 1.8.2 (bottled), HEAD");
    assert_eq!(
        lines[1],
        "Lightweight and flexible command-line JSON processor"
    );
    assert_eq!(lines[2], "https://jqlang.github.io/jq/");
    assert_eq!(lines[3], "Not installed");
    assert_eq!(
        lines[4],
        "From: https://github.com/Homebrew/homebrew-core/blob/HEAD/Formula/j/jq.rb"
    );
    assert_eq!(lines[5], "License: MIT");
    assert_eq!(lines[6], "==> Dependencies");
    assert_eq!(lines[7], "Required: oniguruma");

    // `abv` is Homebrew's alias for `info`.
    assert_eq!(sandbox.stdout(&["abv", "jq"]), out);
}

#[test]
fn info_on_an_unknown_formula_matches_homebrews_error() {
    let sandbox = sandbox_or_skip!();

    let out = sandbox.run(&["info", "nonexistentxyz"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "Error: No available formula with the name \"nonexistentxyz\"."
    );
    assert!(out.stdout.is_empty());

    // A near miss gets Homebrew's DidYouMean suggestions, joined its way.
    let out = sandbox.run(&["info", "jqq"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "Error: No available formula with the name \"jqq\". Did you mean jq, jqp, jql or jaq?"
    );
}

#[test]
fn info_shows_installed_kegs_and_the_upgrade_arrow() {
    let sandbox = sandbox_or_skip!();
    sandbox.add_keg("jq", "1.8.1", true);
    let out = sandbox.stdout(&["info", "jq"]);
    assert!(
        out.starts_with("==> jq: 1.8.1 → stable 1.8.2 (bottled), HEAD\n"),
        "{out}"
    );
    assert!(out.contains("\nInstalled\n"), "{out}");
    let keg = sandbox.prefix.join("Cellar/jq/1.8.1");
    assert!(out.contains(&format!("{} (", keg.display())), "{out}");
    // Linked kegs are marked with a trailing asterisk.
    assert!(out.contains(") *\n"), "{out}");
    assert!(
        out.contains("  Poured from bottle using the internal formulae.brew.sh API on "),
        "{out}"
    );
}

#[test]
fn search_finds_names_aliases_and_casks() {
    let sandbox = sandbox_or_skip!();

    let out = sandbox.stdout(&["search", "oniguruma"]);
    assert!(out.lines().any(|l| l == "oniguruma"), "{out}");

    // Without a TTY there are no `==> Formulae` headers, like Homebrew.
    assert!(!out.contains("==>"), "{out}");

    // Regex queries.
    let out = sandbox.stdout(&["search", "/^json-c$/"]);
    assert_eq!(out.trim(), "json-c");

    // `--formula` suppresses casks and vice versa.
    let formulae = sandbox.stdout(&["search", "--formula", "firefox"]);
    assert!(!formulae.contains("firefox\n") || !formulae.is_empty());
    let casks = sandbox.stdout(&["search", "--cask", "firefox"]);
    assert!(casks.lines().any(|l| l == "firefox"), "{casks}");

    // `-S` is Homebrew's alias for `search`.
    assert_eq!(sandbox.stdout(&["-S", "/^json-c$/"]).trim(), "json-c");

    // Descriptions.
    let out = sandbox.stdout(&["search", "--desc", "json"]);
    assert!(out.starts_with("==> Formulae\n"), "{out}");
    assert!(out.contains("\n==> Casks\n"), "{out}");
    assert!(
        out.contains("jq: Lightweight and flexible command-line JSON processor\n"),
        "{out}"
    );

    let out = sandbox.run(&["search", "zzzznotexistzz"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "Error: No formulae or casks found for \"zzzznotexistzz\"."
    );
}

#[test]
fn desc_prints_name_and_description() {
    let sandbox = sandbox_or_skip!();
    assert_eq!(
        sandbox.stdout(&["desc", "jq"]).trim(),
        "jq: Lightweight and flexible command-line JSON processor"
    );
    let out = sandbox.stdout(&["desc", "-n", "jq"]);
    assert!(out.starts_with("==> Formulae\n"), "{out}");
    assert!(out.contains("\njq: "), "{out}");
}

#[test]
fn deps_tree_uses_homebrews_box_drawing() {
    let sandbox = sandbox_or_skip!();
    let out = sandbox.stdout(&["deps", "--tree", "jq"]);
    assert_eq!(out, "jq\n└── oniguruma\n\n");

    assert_eq!(sandbox.stdout(&["deps", "jq"]).trim(), "oniguruma");
    assert_eq!(
        sandbox.stdout(&["deps", "--for-each", "jq"]).trim(),
        "jq: oniguruma"
    );

    // A deeper tree exercises both branch characters and the indent.
    let out = sandbox.stdout(&["deps", "--tree", "wget"]);
    assert!(out.starts_with("wget\n"), "{out}");
    assert!(out.contains("├── "), "{out}");
    assert!(out.contains("└── "), "{out}");
    assert!(out.ends_with("\n\n"), "{out}");
}

#[test]
fn deps_include_flags_widen_the_graph() {
    let sandbox = sandbox_or_skip!();
    let runtime = sandbox.stdout(&["deps", "jq"]);
    let with_build = sandbox.stdout(&["deps", "--include-build", "jq"]);
    assert!(with_build.lines().count() > runtime.lines().count());
    assert!(with_build.lines().any(|l| l == "oniguruma"));
}

#[test]
fn list_is_empty_in_a_fresh_sandbox() {
    let sandbox = sandbox_or_skip!();
    assert_eq!(sandbox.stdout(&["list"]), "");
    assert_eq!(sandbox.stdout(&["list", "--versions"]), "");
    assert_eq!(sandbox.stdout(&["list", "--pinned"]), "");
    assert_eq!(sandbox.stdout(&["leaves"]), "");
    // `ls` is Homebrew's alias for `list`.
    assert_eq!(sandbox.stdout(&["ls"]), "");
}

#[test]
fn list_reports_fake_kegs() {
    let sandbox = sandbox_or_skip!();
    sandbox.add_keg("jq", "1.8.2", true);
    sandbox.add_keg("oniguruma", "6.9.10_1", false);

    assert_eq!(sandbox.stdout(&["list"]), "jq\noniguruma\n");
    assert_eq!(
        sandbox.stdout(&["list", "--versions"]),
        "jq 1.8.2\noniguruma 6.9.10_1\n"
    );
    assert_eq!(sandbox.stdout(&["list", "--installed-on-request"]), "jq\n");
    assert_eq!(
        sandbox.stdout(&["list", "--installed-as-dependency"]),
        "oniguruma\n"
    );
    assert_eq!(sandbox.stdout(&["leaves"]), "jq\n");

    // `list <formula>` prints the keg's files.
    let files = sandbox.stdout(&["list", "jq"]);
    let keg = sandbox.prefix.join("Cellar/jq/1.8.2");
    assert!(
        files.contains(&format!("{}\n", keg.join("bin/jq").display())),
        "{files}"
    );
    assert!(
        files.contains(&format!("{}\n", keg.join("INSTALL_RECEIPT.json").display())),
        "{files}"
    );

    let json = sandbox.stdout(&["list", "--versions", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
    assert_eq!(doc["formulae"][0]["name"], "jq");
    assert_eq!(doc["formulae"][0]["linked_version"], "1.8.2");
    assert_eq!(doc["casks"].as_array().unwrap().len(), 0);

    let out = sandbox.run(&["list", "nosuchformula"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Error: No such keg:"),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn outdated_is_empty_in_a_fresh_sandbox() {
    let sandbox = sandbox_or_skip!();
    assert_eq!(sandbox.stdout(&["outdated"]), "");
    assert_eq!(sandbox.stdout(&["outdated", "-v"]), "");
    let json = sandbox.stdout(&["outdated", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(doc["formulae"].as_array().unwrap().len(), 0);
    assert_eq!(doc["casks"].as_array().unwrap().len(), 0);
}

#[test]
fn outdated_reports_an_old_keg() {
    let sandbox = sandbox_or_skip!();
    sandbox.add_keg("jq", "1.8.1", true);

    assert_eq!(sandbox.stdout(&["outdated"]), "jq\n");
    assert_eq!(sandbox.stdout(&["outdated", "-v"]), "jq (1.8.1) < 1.8.2\n");

    let json = sandbox.stdout(&["outdated", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(doc["formulae"][0]["name"], "jq");
    assert_eq!(doc["formulae"][0]["installed_versions"][0], "1.8.1");
    assert_eq!(doc["formulae"][0]["current_version"], "1.8.2");
    assert_eq!(doc["formulae"][0]["pinned"], false);

    // A pinned formula still shows up, annotated.
    symlink(
        "../../../Cellar/jq/1.8.1",
        &sandbox.prefix.join("var/homebrew/pinned/jq"),
    );
    assert_eq!(
        sandbox.stdout(&["outdated", "-v"]),
        "jq (1.8.1) < 1.8.2 [pinned at 1.8.1]\n"
    );
    assert_eq!(sandbox.stdout(&["list", "--pinned"]), "jq\n");
    assert_eq!(
        sandbox.stdout(&["list", "--pinned", "--versions"]),
        "jq 1.8.1\n"
    );
}

#[test]
fn a_current_but_unlinked_keg_counts_as_outdated() {
    let sandbox = sandbox_or_skip!();
    sandbox.add_keg("jq", "1.8.2", true);
    assert_eq!(sandbox.stdout(&["outdated"]), "");

    // Drop both the linked and the opt record: Homebrew then treats the keg as
    // outdated even though its version is current (`docs/COMPAT.md` 8).
    std::fs::remove_file(sandbox.prefix.join("var/homebrew/linked/jq")).unwrap();
    std::fs::remove_file(sandbox.prefix.join("opt/jq")).unwrap();
    assert_eq!(sandbox.stdout(&["outdated"]), "jq\n");
}

#[test]
fn uses_and_missing_walk_the_graph() {
    let sandbox = sandbox_or_skip!();
    sandbox.add_keg("jq", "1.8.2", true);

    assert_eq!(
        sandbox.stdout(&["uses", "--installed", "oniguruma"]),
        "jq\n"
    );
    assert_eq!(sandbox.stdout(&["missing"]).trim(), "oniguruma");

    sandbox.add_keg("oniguruma", "6.9.10_1", false);
    assert_eq!(sandbox.stdout(&["missing"]), "");
}

#[test]
fn which_formula_finds_executables() {
    let sandbox = sandbox_or_skip!();
    let out = sandbox.stdout(&["which-formula", "jq"]);
    assert!(out.lines().any(|l| l == "jq"), "{out}");

    let out = sandbox.run(&["which-formula", "definitely-not-a-command"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn config_reports_the_sandbox() {
    let sandbox = sandbox_or_skip!();
    let out = sandbox.stdout(&["config"]);
    assert!(out.contains("HOMEBREW_VERSION: 6.0.22\n"), "{out}");
    assert!(
        out.contains(&format!("HOMEBREW_PREFIX: {}\n", sandbox.prefix.display())),
        "{out}"
    );
    assert!(
        out.contains(&format!("Bottle tag: {}\n", bottle_tag())),
        "{out}"
    );
}

#[test]
fn unknown_commands_would_delegate() {
    let sandbox = sandbox_or_skip!();
    // `FASTBREW_NO_DELEGATE=1` turns delegation into an explanatory error
    // instead of running the host's Ruby `brew`.
    let out = sandbox.run(&["frobnicate"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("FASTBREW_NO_DELEGATE"), "{stderr}");
    assert!(stderr.contains("frobnicate"), "{stderr}");

    // Source builds and HEAD installs delegate too.
    let out = sandbox.run(&["install", "--build-from-source", "jq"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--build-from-source"),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn the_sandbox_guard_refuses_real_prefixes() {
    let sandbox = sandbox_or_skip!();
    let out = sandbox
        .cmd()
        .env("HOMEBREW_PREFIX", "/opt/homebrew")
        .env("HOMEBREW_CELLAR", "/opt/homebrew/Cellar")
        .arg("--prefix")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("FASTBREW_REQUIRE_SANDBOX"),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// Network tests (opt-in)
// ---------------------------------------------------------------------------

#[test]
fn update_refreshes_the_api_file() {
    if !network_tests_enabled() {
        eprintln!("set FASTBREW_TEST_NETWORK=1 to run network tests; skipping");
        return;
    }
    let sandbox = sandbox_or_skip!();
    let out = sandbox.run(&["update"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("Already up-to-date.") || text.contains("==>"),
        "{text}"
    );
}

#[test]
fn info_json_merges_local_state() {
    if !network_tests_enabled() {
        eprintln!("set FASTBREW_TEST_NETWORK=1 to run network tests; skipping");
        return;
    }
    let sandbox = sandbox_or_skip!();
    sandbox.add_keg("jq", "1.8.1", true);
    let out = sandbox.stdout(&["info", "--json=v2", "jq"]);
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    let formula = &doc["formulae"][0];
    assert_eq!(formula["name"], "jq");
    assert_eq!(formula["installed"][0]["version"], "1.8.1");
    assert_eq!(formula["installed"][0]["installed_on_request"], true);
    assert_eq!(formula["linked_keg"], "1.8.1");
    assert_eq!(formula["pinned"], false);
    assert_eq!(formula["outdated"], true);
}
