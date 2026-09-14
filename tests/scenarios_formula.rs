//! Scenario tests: the things people actually do to formulae with `brew`.
//!
//! Every test drives the built binary through the CLI, the way a user would,
//! against a private prefix in a fresh temp directory plus the shared,
//! API-seeded cache from `tests/support`. Nothing here touches `/opt/homebrew`
//! or the user's Homebrew cache.
//!
//! Expectations were checked against a Ruby Homebrew running in a second
//! sandbox (`scripts/sandbox.sh brew`, and `scripts/compat-check.sh` for the
//! installed state), never against the host prefix.
//!
//! Run them with `FASTBREW_TEST_NETWORK=1 scripts/sandbox.sh test
//! scenarios_formula`. Without the network the tests that need bottles skip.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod support;
use support::{Sandbox, api_formula, api_index, api_pkg_version, older_version};

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

/// Skip a test that needs to download bottles.
macro_rules! network_or_skip {
    () => {
        if !support::network_tests_enabled() {
            eprintln!("skipping: set FASTBREW_TEST_NETWORK=1 to run network tests");
            return;
        }
    };
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Run and require success, returning stdout and stderr joined.
fn ok(sb: &Sandbox, args: &[&str]) -> String {
    let out = sb.run(args);
    let text = combined(&out);
    assert!(
        out.status.success(),
        "`fastbrew {}` failed:\n{text}",
        args.join(" ")
    );
    text
}

/// Run and require failure, returning stdout and stderr joined.
fn fails(sb: &Sandbox, args: &[&str]) -> String {
    let out = sb.run(args);
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "`fastbrew {}` unexpectedly succeeded:\n{text}",
        args.join(" ")
    );
    text
}

fn keg(sb: &Sandbox, name: &str, version: &str) -> PathBuf {
    sb.prefix.join("Cellar").join(name).join(version)
}

fn receipt(sb: &Sandbox, name: &str, version: &str) -> serde_json::Value {
    let path = keg(sb, name, version).join("INSTALL_RECEIPT.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("no receipt at {}: {e}", path.display()));
    serde_json::from_str(&text).expect("the receipt is valid JSON")
}

/// Every symlink under the prefix that resolves into `name`'s rack.
fn links_into(sb: &Sandbox, name: &str) -> Vec<String> {
    // Canonicalized: the temp directory lives under a symlinked `/var` on
    // macOS, so a resolved link never matches the path we built by hand.
    let rack = std::fs::canonicalize(sb.prefix.join("Cellar").join(name))
        .unwrap_or_else(|_| sb.prefix.join("Cellar").join(name));
    let mut found = Vec::new();
    let mut stack = vec![sb.prefix.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_symlink() {
                if let Ok(target) = std::fs::canonicalize(&path)
                    && target.starts_with(&rack)
                    && let Ok(relative) = path.strip_prefix(&sb.prefix)
                {
                    found.push(relative.to_string_lossy().into_owned());
                }
            } else if path.is_dir() && !path.starts_with(sb.prefix.join("Cellar")) {
                stack.push(path);
            }
        }
    }
    found.sort();
    found
}

/// Turn the installed keg of `name` into one of an older release: unlink it,
/// rename the directory, correct the receipt and link it again. That is the
/// state `outdated` and `upgrade` need, without pinning the test to a version
/// the API no longer carries.
fn fake_older_keg(sb: &Sandbox, name: &str, current: &str, older: &str) {
    ok(sb, &["unlink", name]);
    std::fs::rename(keg(sb, name, current), keg(sb, name, older)).expect("rename the keg");
    let path = keg(sb, name, older).join("INSTALL_RECEIPT.json");
    let mut json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    json["source"]["versions"]["stable"] = serde_json::json!(older);
    std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    ok(sb, &["link", name]);
}

/// Hold a rack's formula lock from another process, the way a concurrent
/// `brew` would.
struct LockHolder {
    child: std::process::Child,
}

impl LockHolder {
    fn take(sb: &Sandbox, name: &str) -> LockHolder {
        const SCRIPT: &str = concat!(
            "import fcntl, os, sys, time\n",
            "path = sys.argv[1]\n",
            "os.makedirs(os.path.dirname(path), exist_ok=True)\n",
            "handle = open(path, 'a+')\n",
            "fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)\n",
            "sys.stdout.write('ready\\n')\n",
            "sys.stdout.flush()\n",
            "time.sleep(300)\n",
        );
        let path = sb
            .prefix
            .join("var/homebrew/locks")
            .join(format!("{name}.formula.lock"));
        let mut child = Command::new("python3")
            .arg("-c")
            .arg(SCRIPT)
            .arg(&path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn the lock holder");
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.as_mut().expect("holder stdout")),
            &mut line,
        )
        .expect("the holder reports the lock it took");
        assert_eq!(line.trim(), "ready", "the holder could not take the lock");
        LockHolder { child }
    }
}

impl Drop for LockHolder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(["-c", "user.name=fastbrew tests"])
        .args(["-c", "user.email=tests@example.invalid"])
        .args(["-c", "commit.gpgsign=false"])
        .args(["-c", "protocol.file.allow=always"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

// ---------------------------------------------------------------------------
// 1. Fresh machine: install a few formulae and ask every question about them
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_machine_installs_and_answers_every_query() {
    let sb = sandbox_or_skip!();
    network_or_skip!();

    // `hello` has no dependencies, `jq` pulls `oniguruma`, `ripgrep` pulls
    // `pcre2`, and `bfs` shares `oniguruma` with `jq`. All four are
    // relocatable, so they pour into a sandbox prefix of any length.
    ok(&sb, &["install", "hello"]);
    let jq_out = ok(&sb, &["install", "jq"]);
    assert!(
        jq_out.contains("==> Installing dependencies for jq: oniguruma"),
        "{jq_out}"
    );
    assert!(
        jq_out.contains("==> Installing jq dependency: oniguruma"),
        "{jq_out}"
    );
    assert!(jq_out.contains("\n==> Installing jq\n"), "{jq_out}");
    ok(&sb, &["install", "ripgrep"]);
    let bfs_out = ok(&sb, &["install", "bfs"]);
    // `oniguruma` is already there, so `bfs` gets no dependency heading.
    assert!(
        !bfs_out.contains("Installing dependencies for bfs"),
        "{bfs_out}"
    );

    let jq_version = api_pkg_version("jq");
    let oniguruma_version = api_pkg_version("oniguruma");

    // `list` is one name per line off a TTY, alphabetically.
    let list = ok(&sb, &["list"]);
    assert_eq!(
        list.lines().collect::<Vec<_>>(),
        vec!["bfs", "hello", "jq", "oniguruma", "pcre2", "ripgrep"],
        "{list}"
    );
    let versions = ok(&sb, &["list", "--versions"]);
    assert!(
        versions.lines().any(|l| l == format!("jq {jq_version}")),
        "{versions}"
    );

    // Dependencies are not leaves.
    let leaves = ok(&sb, &["leaves"]);
    assert_eq!(
        leaves.lines().collect::<Vec<_>>(),
        vec!["bfs", "hello", "jq", "ripgrep"],
        "{leaves}"
    );

    let tree = ok(&sb, &["deps", "--installed", "--tree"]);
    assert!(tree.contains("jq\n└── oniguruma\n"), "{tree}");
    assert!(tree.contains("ripgrep\n└── pcre2\n"), "{tree}");
    assert!(tree.contains("hello\n\n"), "a leaf prints alone:\n{tree}");

    let uses = ok(&sb, &["uses", "--installed", "oniguruma"]);
    assert_eq!(
        uses.lines().collect::<Vec<_>>(),
        vec!["bfs", "jq"],
        "{uses}"
    );

    // `info` on an installed formula: the keg line with the linked asterisk
    // and the receipt's tab line underneath.
    let info = ok(&sb, &["info", "jq"]);
    assert!(
        info.starts_with(&format!("==> jq: stable {jq_version} (bottled), HEAD\n")),
        "{info}"
    );
    assert!(info.contains("\nInstalled\n"), "{info}");
    assert!(
        info.contains(&format!(
            "{} (20 files, ",
            keg(&sb, "jq", &jq_version).display()
        )),
        "{info}"
    );
    assert!(info.contains(") *\n"), "a linked keg is starred:\n{info}");
    assert!(
        info.contains("  Poured from bottle using the internal formulae.brew.sh API on "),
        "{info}"
    );
    assert!(
        info.contains("\n==> Dependencies\nRequired: oniguruma"),
        "{info}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&sb.stdout(&["info", "--json=v2", "jq"])).expect("valid JSON");
    let entry = &json["formulae"][0];
    assert_eq!(entry["name"], "jq");
    assert_eq!(entry["linked_keg"], serde_json::json!(jq_version));
    assert_eq!(
        entry["installed"][0]["version"],
        serde_json::json!(jq_version)
    );
    assert_eq!(entry["installed"][0]["installed_on_request"], true);
    assert_eq!(entry["installed"][0]["poured_from_bottle"], true);
    assert_eq!(entry["outdated"], false);
    // Homebrew 6 dropped `installed_as_dependency` from this hash.
    assert!(
        entry["installed"][0]
            .get("installed_as_dependency")
            .is_none(),
        "{entry}"
    );
    assert_eq!(
        entry["installed"][0]["runtime_dependencies"][0]["full_name"],
        "oniguruma"
    );
    assert_eq!(
        entry["installed"][0]["runtime_dependencies"][0]["pkg_version"],
        serde_json::json!(oniguruma_version)
    );

    // The path queries answer with `opt` and the rack, not with the keg.
    assert_eq!(
        ok(&sb, &["--prefix", "jq"]).trim(),
        sb.prefix.join("opt/jq").to_string_lossy()
    );
    assert_eq!(
        ok(&sb, &["--cellar", "jq"]).trim(),
        sb.prefix.join("Cellar/jq").to_string_lossy()
    );

    // `list <formula>` off a TTY is `find <keg> -not -type d -not -name .DS_Store`.
    let files = ok(&sb, &["list", "jq"]);
    let keg_path = keg(&sb, "jq", &jq_version);
    assert!(
        files
            .lines()
            .any(|l| l == keg_path.join("bin/jq").to_string_lossy()),
        "{files}"
    );
    assert!(
        files
            .lines()
            .any(|l| l == keg_path.join("INSTALL_RECEIPT.json").to_string_lossy()),
        "{files}"
    );
    assert!(
        !files
            .lines()
            .any(|l| l == keg_path.join("bin").to_string_lossy()),
        "directories are not listed:\n{files}"
    );

    // `which-formula` maps an executable back to the formula that ships it.
    assert_eq!(ok(&sb, &["which-formula", "rg"]).trim(), "ripgrep");

    // The receipt of a dependency records that it was not requested.
    let dep = receipt(&sb, "oniguruma", &oniguruma_version);
    assert_eq!(dep["installed_on_request"], false);
    assert_eq!(
        receipt(&sb, "jq", &jq_version)["installed_on_request"],
        true
    );
}

// ---------------------------------------------------------------------------
// 2. Installing something that is already there
// ---------------------------------------------------------------------------

#[test]
fn installing_an_installed_formula_warns_and_reinstall_redoes_it() {
    let sb = sandbox_or_skip!();
    network_or_skip!();

    ok(&sb, &["install", "jq"]);
    let version = api_pkg_version("jq");
    let oniguruma_version = api_pkg_version("oniguruma");

    // `Homebrew::Install.install_formula?`: a warning, and the run still
    // succeeds.
    let again = ok(&sb, &["install", "jq"]);
    assert!(
        again.contains(&format!(
            "Warning: jq {version} is already installed and up-to-date.\n\
             To reinstall {version}, run:\n  brew reinstall jq"
        )),
        "{again}"
    );

    // `--force` does not change that for a formula that is not keg-only:
    // `force` only unblocks the keg-only opt-link check.
    let forced = ok(&sb, &["install", "jq", "--force"]);
    assert!(
        forced.contains("is already installed and up-to-date."),
        "{forced}"
    );

    // Naming an installed dependency promotes it to a requested formula.
    assert_eq!(
        receipt(&sb, "oniguruma", &oniguruma_version)["installed_on_request"],
        false
    );
    ok(&sb, &["install", "oniguruma"]);
    assert_eq!(
        receipt(&sb, "oniguruma", &oniguruma_version)["installed_on_request"],
        true,
        "`install <dependency>` records the explicit request"
    );

    // `reinstall` replaces the keg in place and keeps it linked.
    let before = links_into(&sb, "jq");
    let out = ok(&sb, &["reinstall", "jq"]);
    assert!(out.contains("==> Reinstalling jq"), "{out}");
    assert!(keg(&sb, "jq", &version).is_dir());
    assert_eq!(links_into(&sb, "jq"), before, "the link set is unchanged");

    // A dependency that went missing comes back with `reinstall`, which is
    // what Homebrew does too; a plain `install` only prints the warning.
    ok(&sb, &["uninstall", "--ignore-dependencies", "oniguruma"]);
    let install = ok(&sb, &["install", "jq"]);
    assert!(
        install.contains("is already installed and up-to-date."),
        "{install}"
    );
    assert!(
        !sb.prefix.join("Cellar/oniguruma").exists(),
        "`install` of an installed formula does nothing:\n{install}"
    );
    let repaired = ok(&sb, &["reinstall", "jq"]);
    assert!(
        repaired.contains("==> Installing jq dependency: oniguruma"),
        "{repaired}"
    );
    assert!(keg(&sb, "oniguruma", &oniguruma_version).is_dir());
}

// ---------------------------------------------------------------------------
// 3. The upgrade lifecycle
// ---------------------------------------------------------------------------

#[test]
fn the_upgrade_lifecycle_from_outdated_to_pinned_and_back() {
    let sb = sandbox_or_skip!();
    network_or_skip!();

    ok(&sb, &["install", "jq"]);
    let current = api_pkg_version("jq");
    let old = older_version(&current);
    assert_ne!(current, old);
    fake_older_keg(&sb, "jq", &current, &old);

    assert_eq!(ok(&sb, &["outdated"]).trim(), "jq");
    assert_eq!(
        ok(&sb, &["outdated", "-v"]).trim(),
        format!("jq ({old}) < {current}")
    );
    let json: serde_json::Value =
        serde_json::from_str(&sb.stdout(&["outdated", "--json"])).expect("valid JSON");
    assert_eq!(json["formulae"][0]["name"], "jq");
    assert_eq!(
        json["formulae"][0]["installed_versions"][0],
        serde_json::json!(old)
    );
    assert_eq!(
        json["formulae"][0]["current_version"],
        serde_json::json!(current)
    );
    assert_eq!(json["formulae"][0]["pinned"], false);
    assert_eq!(json["casks"], serde_json::json!([]));

    let dry = ok(&sb, &["upgrade", "--dry-run"]);
    assert!(
        dry.contains("==> Would upgrade 1 outdated package:"),
        "{dry}"
    );
    assert!(dry.contains(&format!("jq {old} -> {current}")), "{dry}");
    assert!(keg(&sb, "jq", &old).is_dir(), "a dry run writes nothing");

    // A pinned formula is skipped. Named explicitly it `ofail`s (exit 1);
    // in a bare `upgrade` it is only a warning.
    ok(&sb, &["pin", "jq"]);
    assert!(sb.prefix.join("var/homebrew/pinned/jq").is_symlink());
    // The heading and the name list go to stdout, the `ofail`/`opoo` line to
    // stderr, so they are asserted separately.
    let refused = fails(&sb, &["upgrade", "jq"]);
    assert!(refused.contains("==> No packages to upgrade"), "{refused}");
    assert!(
        refused.contains("Error: Not upgrading 1 pinned package:"),
        "{refused}"
    );
    assert!(refused.contains(&format!("jq {current}")), "{refused}");
    let warned = ok(&sb, &["upgrade"]);
    assert!(
        warned.contains("Warning: Not upgrading 1 pinned package:"),
        "a bare upgrade only warns:\n{warned}"
    );
    assert!(warned.contains(&format!("jq {current}")), "{warned}");
    assert!(keg(&sb, "jq", &old).is_dir(), "the pinned keg is untouched");
    ok(&sb, &["unpin", "jq"]);

    let out = ok(&sb, &["upgrade", "jq"]);
    assert!(out.contains("==> Upgrading 1 outdated package:"), "{out}");
    assert!(out.contains(&format!("jq {old} -> {current}")), "{out}");
    assert!(out.contains("==> Upgrading jq\n"), "{out}");
    // `Upgrade.print_upgrade_message` joins the empty option list with a
    // space, so the version line ends with one.
    assert!(out.contains(&format!("  {old} -> {current} \n")), "{out}");
    assert!(keg(&sb, "jq", &current).is_dir());
    assert_eq!(
        std::fs::read_link(sb.prefix.join("var/homebrew/linked/jq")).unwrap(),
        PathBuf::from(format!("../../../Cellar/jq/{current}"))
    );

    // Nothing left to do: `upgrade_outdated_formulae!` returns before
    // printing anything.
    let quiet = ok(&sb, &["upgrade"]);
    assert_eq!(quiet, "", "a no-op upgrade says nothing: {quiet:?}");
    assert_eq!(ok(&sb, &["outdated"]), "");
}

#[test]
fn a_current_keg_that_is_neither_linked_nor_opt_linked_is_upgraded_in_place() {
    let sb = sandbox_or_skip!();
    network_or_skip!();

    ok(&sb, &["install", "jq"]);
    let current = api_pkg_version("jq");

    // `Formula#outdated_kegs` only counts a keg as current when it is linked,
    // opt-linked or pinned; without those records the rack is outdated even
    // at the newest version.
    ok(&sb, &["unlink", "jq"]);
    std::fs::remove_file(sb.prefix.join("opt/jq")).expect("drop the opt record");
    assert_eq!(
        ok(&sb, &["outdated", "-v"]).trim(),
        format!("jq ({current}) < {current}")
    );

    // The upgrade pours the version that is already in the rack, so the
    // existing keg has to be replaced rather than treated as a collision.
    let out = ok(&sb, &["upgrade", "jq"]);
    assert!(out.contains("==> Upgrading jq"), "{out}");
    assert!(!out.contains("already exists"), "{out}");
    assert!(
        keg(&sb, "jq", &current)
            .join("INSTALL_RECEIPT.json")
            .is_file()
    );
    assert!(
        sb.prefix.join("opt/jq").is_symlink(),
        "the opt record is back"
    );
    assert!(sb.prefix.join("var/homebrew/linked/jq").is_symlink());
    assert_eq!(ok(&sb, &["outdated"]), "");
}

// ---------------------------------------------------------------------------
// 4. Removal
// ---------------------------------------------------------------------------

#[test]
fn removing_a_dependency_then_autoremoving_the_orphan() {
    let sb = sandbox_or_skip!();
    network_or_skip!();

    ok(&sb, &["install", "jq"]);
    ok(&sb, &["install", "bfs"]);
    let oniguruma_version = api_pkg_version("oniguruma");

    // Two dependents, listed as a sentence.
    let refused = fails(&sb, &["uninstall", "oniguruma"]);
    assert!(
        refused.contains(&format!(
            "Error: Refusing to uninstall {}\n\
             because it is required by bfs and jq, which are currently installed.\n\
             You can override this and force removal with:\n  \
             brew uninstall --ignore-dependencies oniguruma",
            keg(&sb, "oniguruma", &oniguruma_version).display()
        )),
        "{refused}"
    );
    assert!(keg(&sb, "oniguruma", &oniguruma_version).is_dir());

    let gone = ok(&sb, &["uninstall", "--ignore-dependencies", "oniguruma"]);
    assert!(
        gone.contains(&format!(
            "Uninstalling {}... (",
            keg(&sb, "oniguruma", &oniguruma_version).display()
        )),
        "{gone}"
    );
    assert!(!sb.prefix.join("Cellar/oniguruma").exists());

    // `missing` reports both dependents.
    let missing = ok(&sb, &["missing"]);
    assert_eq!(
        missing.lines().collect::<Vec<_>>(),
        vec!["bfs: oniguruma", "jq: oniguruma"],
        "{missing}"
    );

    ok(&sb, &["reinstall", "jq"]);
    assert!(keg(&sb, "oniguruma", &oniguruma_version).is_dir());
    assert_eq!(ok(&sb, &["missing"]), "", "the dependency is back");

    // With both dependents gone the dependency is unneeded.
    ok(&sb, &["uninstall", "jq"]);
    ok(&sb, &["uninstall", "bfs"]);
    let dry = ok(&sb, &["autoremove", "--dry-run"]);
    assert!(
        dry.contains("==> Would autoremove 1 unneeded formula:"),
        "{dry}"
    );
    assert!(dry.lines().any(|l| l == "oniguruma"), "{dry}");
    assert!(
        keg(&sb, "oniguruma", &oniguruma_version).is_dir(),
        "a dry run removes nothing"
    );

    let done = ok(&sb, &["autoremove"]);
    assert!(
        done.contains("==> Autoremoving 1 unneeded formula:"),
        "{done}"
    );
    assert!(!sb.prefix.join("Cellar/oniguruma").exists());
    assert_eq!(ok(&sb, &["list"]), "");
}

/// Record an installed cask from a third-party tap that needs `formula`.
///
/// Only the Caskroom metadata matters here: the index carries `homebrew/cask`
/// alone, so the receipt is the only record of what a tap cask depends on.
fn add_tap_cask(sb: &Sandbox, token: &str, version: &str, tap: &str, formula: &str) {
    let caskroom = sb.prefix.join("Caskroom").join(token);
    std::fs::create_dir_all(caskroom.join(version)).unwrap();
    std::fs::create_dir_all(caskroom.join(".metadata")).unwrap();
    let receipt = serde_json::json!({
        "homebrew_version": "6.0.22",
        "loaded_from_api": true,
        "loaded_from_internal_api": true,
        "uninstall_flight_blocks": false,
        "installed_on_request": true,
        "time": 1778031361u64,
        "runtime_dependencies": {
            "formula": [{"full_name": formula, "declared_directly": true}]
        },
        "source": {"tap": tap, "tap_git_head": null, "version": version, "path": null},
        "arch": "arm64",
        "uninstall_artifacts": [],
    });
    std::fs::write(
        caskroom.join(".metadata/INSTALL_RECEIPT.json"),
        serde_json::to_string_pretty(&receipt).unwrap(),
    )
    .unwrap();
}

#[test]
fn a_tap_casks_dependency_is_neither_autoremoved_nor_uninstalled() {
    let sb = sandbox_or_skip!();

    let version = api_pkg_version("hello");
    // Installed as a dependency, so nothing but a dependent keeps it.
    sb.add_keg("hello", &version, false);

    let orphan = ok(&sb, &["autoremove", "--dry-run"]);
    assert!(
        orphan.lines().any(|l| l == "hello"),
        "without a dependent it is unneeded:\n{orphan}"
    );

    add_tap_cask(&sb, "fixture-app", "1.0", "review/fixture", "hello");

    let kept = ok(&sb, &["autoremove", "--dry-run"]);
    assert!(
        !kept.lines().any(|l| l == "hello"),
        "the tap cask's dependency stays:\n{kept}"
    );
    assert!(keg(&sb, "hello", &version).is_dir());

    let refused = fails(&sb, &["uninstall", "hello"]);
    assert!(
        refused.contains(&format!(
            "Error: Refusing to uninstall {}",
            keg(&sb, "hello", &version).display()
        )),
        "{refused}"
    );
    assert!(
        refused.contains("because it is required by fixture-app, which is currently installed."),
        "{refused}"
    );
    assert!(keg(&sb, "hello", &version).is_dir());

    // With the cask gone it is unneeded again.
    std::fs::remove_dir_all(sb.prefix.join("Caskroom/fixture-app")).unwrap();
    let again = ok(&sb, &["autoremove", "--dry-run"]);
    assert!(again.lines().any(|l| l == "hello"), "{again}");
}

#[test]
fn uninstall_reports_unknown_names_and_leftover_versions() {
    let sb = sandbox_or_skip!();

    // A known formula that is not installed is a `NoSuchKegError`.
    let no_keg = fails(&sb, &["uninstall", "jq"]);
    assert!(
        no_keg.contains(&format!(
            "Error: No such keg: {}",
            sb.prefix.join("Cellar/jq").display()
        )),
        "{no_keg}"
    );

    // A name the API does not know at all is a `FormulaUnavailableError`.
    let unknown = fails(&sb, &["uninstall", "nope-not-a-formula"]);
    assert!(
        unknown.contains("Error: No available formula with the name \"nope-not-a-formula\""),
        "{unknown}"
    );

    // Two versions in the rack: a plain `uninstall` takes the linked one and
    // says what is left; `--force` takes the whole rack and prints its name.
    let current = api_pkg_version("hello");
    let old = older_version(&current);
    sb.add_keg("hello", &old, true);
    sb.add_keg("hello", &current, true);
    let out = ok(&sb, &["uninstall", "hello"]);
    assert!(
        out.contains(&format!(
            "hello {old} is still installed.\nTo remove all versions, run:\n  brew uninstall --force hello"
        )),
        "{out}"
    );
    assert!(keg(&sb, "hello", &old).is_dir());

    let forced = ok(&sb, &["uninstall", "--force", "hello"]);
    assert!(forced.contains("Uninstalling hello... ("), "{forced}");
    assert!(!sb.prefix.join("Cellar/hello").exists());
    assert!(!sb.prefix.join("opt/hello").exists());
    assert!(!sb.prefix.join("var/homebrew/linked/hello").exists());
}

#[test]
fn cleanup_prunes_kegs_downloads_and_interrupted_staging_directories() {
    let sb = sandbox_or_skip!();
    network_or_skip!();

    ok(&sb, &["install", "hello"]);
    let current = api_pkg_version("hello");
    let old = older_version(&current);
    sb.add_keg("hello", &old, true);
    // `add_keg` points the records at the older keg; put them back, so the
    // current keg is the linked one and only the old keg is eligible.
    support::symlink(
        &format!("../Cellar/hello/{current}"),
        &sb.prefix.join("opt/hello"),
    );
    support::symlink(
        &format!("../../../Cellar/hello/{current}"),
        &sb.prefix.join("var/homebrew/linked/hello"),
    );

    let dry = ok(&sb, &["cleanup", "-n", "hello"]);
    assert!(
        dry.contains(&format!(
            "Would remove: {}",
            keg(&sb, "hello", &old).display()
        )),
        "{dry}"
    );
    assert!(
        keg(&sb, "hello", &old).is_dir(),
        "a dry run removes nothing"
    );

    // A killed install leaves its extraction staging directory in the rack;
    // `cleanup` is what removes it.
    let staging = sb.prefix.join("Cellar/hello/.fastbrew-0123456789abcdef");
    std::fs::create_dir_all(staging.join("hello").join(&current)).unwrap();

    let done = ok(&sb, &["cleanup", "hello"]);
    assert!(
        done.contains(&format!("Removing: {}", keg(&sb, "hello", &old).display())),
        "{done}"
    );
    assert!(!keg(&sb, "hello", &old).exists());
    assert!(
        done.contains(&format!("Removing: {}", staging.display())),
        "the staging leftovers go too:\n{done}"
    );
    assert!(!staging.exists());
    assert!(
        keg(&sb, "hello", &current).is_dir(),
        "the current keg stays"
    );

    // `-s` prunes a cached download only when the formula's latest version is
    // not installed (`Cleanup.stale_formula?`), so hello's bottle stays.
    let scrub = ok(&sb, &["cleanup", "-s", "-n"]);
    assert!(
        !scrub.lines().any(|l| l.contains("--hello--")),
        "the installed version's download is kept:\n{scrub}"
    );
    // `--prune=<days>` skips the staleness check and goes by age alone, so
    // zero days reaches everything.
    let prune = ok(&sb, &["cleanup", "--prune=0", "-n"]);
    assert!(
        prune
            .lines()
            .any(|l| l.starts_with("Would remove: ") && l.contains("--hello--")),
        "{prune}"
    );
}

// ---------------------------------------------------------------------------
// 5. Links
// ---------------------------------------------------------------------------

#[test]
fn unlink_link_conflicts_and_overwrite() {
    let sb = sandbox_or_skip!();
    network_or_skip!();

    ok(&sb, &["install", "jq"]);
    let version = api_pkg_version("jq");
    let keg_path = keg(&sb, "jq", &version);
    let linked = links_into(&sb, "jq");

    let out = ok(&sb, &["unlink", "jq"]);
    // `Keg#unlink` counts only the symlinks inside the prefix tree, not the
    // `opt` record, which it leaves alone.
    let removed = count_in(&out, "symlinks removed.");
    assert!(
        out.starts_with(&format!("Unlinking {}... ", keg_path.display())),
        "{out}"
    );
    assert!(!sb.prefix.join("var/homebrew/linked/jq").exists());
    assert!(
        sb.prefix.join("opt/jq").is_symlink(),
        "opt survives an unlink"
    );
    assert_eq!(links_into(&sb, "jq"), vec!["opt/jq".to_string()]);

    let dry = ok(&sb, &["link", "--dry-run", "jq"]);
    assert!(dry.starts_with("Would link:\n"), "{dry}");
    assert!(
        dry.lines()
            .any(|l| l == sb.prefix.join("bin/jq").to_string_lossy()),
        "{dry}"
    );
    assert!(
        !sb.prefix.join("bin/jq").exists(),
        "a dry run links nothing"
    );

    let out = ok(&sb, &["link", "jq"]);
    let created = count_in(&out, "symlinks created.");
    // `Keg#link` counts exactly what `link_dir` made: neither the `opt` record
    // nor `var/homebrew/linked/<name>` is counted.
    assert_eq!(created, removed, "link and unlink count the same links");
    assert_eq!(
        links_into(&sb, "jq"),
        linked,
        "the same link set comes back"
    );

    // A real file in the way is `Keg::ConflictError`.
    ok(&sb, &["unlink", "jq"]);
    std::fs::write(sb.prefix.join("bin/jq"), b"not jq\n").unwrap();
    let conflict = fails(&sb, &["link", "jq"]);
    assert!(
        conflict.contains("Error: Could not symlink bin/jq"),
        "{conflict}"
    );
    assert!(
        conflict.contains(&format!(
            "Target {}\nalready exists. You may want to remove it:\n  rm '{}'",
            sb.prefix.join("bin/jq").display(),
            sb.prefix.join("bin/jq").display()
        )),
        "{conflict}"
    );
    assert!(
        conflict.contains(
            "To force the link and overwrite all conflicting files:\n  brew link --overwrite jq"
        ),
        "{conflict}"
    );
    assert!(
        conflict.contains(
            "To list all files that would be deleted:\n  brew link --overwrite jq --dry-run"
        ),
        "{conflict}"
    );
    // The failed link rolled back: nothing else of jq is linked.
    assert_eq!(
        links_into(&sb, "jq"),
        vec!["opt/jq".to_string()],
        "{conflict}"
    );
    assert!(!sb.prefix.join("bin/jq").is_symlink());

    let would = ok(&sb, &["link", "--overwrite", "--dry-run", "jq"]);
    assert!(would.starts_with("Would remove:\n"), "{would}");
    assert!(
        would
            .lines()
            .any(|l| l == sb.prefix.join("bin/jq").to_string_lossy()),
        "{would}"
    );
    assert!(
        sb.prefix.join("bin/jq").exists(),
        "a dry run removes nothing"
    );

    ok(&sb, &["link", "--overwrite", "jq"]);
    assert!(sb.prefix.join("bin/jq").is_symlink());
    assert_eq!(links_into(&sb, "jq"), linked);
}

#[test]
fn unlink_removes_the_links_that_are_there_not_the_newest_kegs() {
    let sb = sandbox_or_skip!();

    // 1.0 is linked, 2.0 is installed but not: `unlink` has to take the keg
    // the prefix records point at (`NamedArgs#resolve_default_keg`), not the
    // one that sorts highest.
    sb.add_keg("demo", "2.0", true);
    sb.add_keg("demo", "1.0", true);
    support::symlink("../Cellar/demo/1.0", &sb.prefix.join("opt/demo"));
    support::symlink(
        "../../../Cellar/demo/1.0",
        &sb.prefix.join("var/homebrew/linked/demo"),
    );
    support::symlink("../Cellar/demo/1.0/bin/demo", &sb.prefix.join("bin/demo"));

    let out = ok(&sb, &["unlink", "demo"]);
    assert!(
        out.contains(&format!(
            "Unlinking {}... ",
            sb.prefix.join("Cellar/demo/1.0").display()
        )),
        "{out}"
    );
    assert_eq!(count_in(&out, "symlinks removed."), 1, "{out}");
    assert!(!sb.prefix.join("bin/demo").exists(), "{out}");
    assert!(
        !sb.prefix.join("var/homebrew/linked/demo").exists(),
        "{out}"
    );
}

/// The trailing number of a `... N symlinks <verb>` line.
fn count_in(text: &str, suffix: &str) -> usize {
    let line = text
        .lines()
        .find(|l| l.ends_with(suffix))
        .unwrap_or_else(|| panic!("no line ending in {suffix:?}:\n{text}"));
    line.split_whitespace()
        .rev()
        .nth(2)
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no count in {line:?}"))
}

#[test]
fn a_keg_only_formula_is_installed_but_not_linked() {
    let sb = sandbox_or_skip!();
    network_or_skip!();

    // `libxml2` is keg-only `:provided_by_macos` and relocatable, and its only
    // runtime dependency (`readline`) is relocatable too.
    let entry = api_formula("libxml2").expect("libxml2 is in the index");
    assert!(
        !entry.keg_only_args.is_empty(),
        "the scenario needs a keg-only formula"
    );

    let out = ok(&sb, &["install", "libxml2"]);
    let version = api_pkg_version("libxml2");
    assert!(keg(&sb, "libxml2", &version).is_dir());
    assert!(
        sb.prefix.join("opt/libxml2").is_symlink(),
        "keg-only still gets opt"
    );
    assert!(
        !sb.prefix.join("var/homebrew/linked/libxml2").exists(),
        "a keg-only formula is not linked"
    );
    assert_eq!(links_into(&sb, "libxml2"), vec!["opt/libxml2".to_string()]);

    // The caveat block is printed under the package's own heading.
    assert!(out.contains("==> Caveats"), "{out}");
    assert!(out.contains("==> libxml2\n"), "{out}");
    assert!(
        out.contains(&format!(
            "libxml2 is keg-only, which means it was not symlinked into {},\n\
             because macOS already provides this software and installing another version in\n\
             parallel can cause all kinds of trouble.",
            sb.prefix.display()
        )),
        "{out}"
    );

    let info = ok(&sb, &["info", "libxml2"]);
    assert!(info.starts_with("==> libxml2: stable "), "{info}");
    assert!(
        info.lines().next().unwrap().ends_with(" [keg-only]"),
        "{info}"
    );
    // The keg is installed but unlinked, so it gets no asterisk.
    assert!(info.contains("\nInstalled\n"), "{info}");
    assert!(
        !info.contains(") *\n"),
        "an unlinked keg is not starred:\n{info}"
    );

    // `brew link` refuses without `--force` and still exits 0 (`opoo`).
    let refused = ok(&sb, &["link", "libxml2"]);
    assert!(
        refused.contains("Warning: libxml2 is keg-only and must be linked with `--force`."),
        "{refused}"
    );
    assert!(!sb.prefix.join("var/homebrew/linked/libxml2").exists());

    let forced = ok(&sb, &["link", "--force", "libxml2"]);
    assert!(forced.contains("symlinks created."), "{forced}");
    assert!(sb.prefix.join("var/homebrew/linked/libxml2").is_symlink());
    assert!(
        links_into(&sb, "libxml2").len() > 1,
        "the keg is linked now"
    );
}

// ---------------------------------------------------------------------------
// 6. Queries that need no bottles
// ---------------------------------------------------------------------------

#[test]
fn search_desc_deps_and_uses_on_a_mixed_state() {
    let sb = sandbox_or_skip!();
    let jq_version = api_pkg_version("jq");
    sb.add_keg("jq", &jq_version, true);

    // Off a TTY `search` prints names only, formulae first, then a blank line
    // and the casks (`Cmd::Search#print_results`).
    let search = ok(&sb, &["search", "jq"]);
    assert!(search.lines().any(|l| l == "jq"), "{search}");
    assert!(
        !search.contains("==> Formulae"),
        "headings are TTY-only:\n{search}"
    );

    let desc_search = ok(&sb, &["search", "--desc", "JSON processor"]);
    assert!(desc_search.contains("==> Formulae"), "{desc_search}");
    assert!(
        desc_search.contains("jq: Lightweight and flexible command-line JSON processor"),
        "{desc_search}"
    );

    assert_eq!(
        ok(&sb, &["desc", "jq"]).trim(),
        "jq: Lightweight and flexible command-line JSON processor"
    );

    // `--annotate` with `--tree` appends a space to every dependency name,
    // even when there is no tag to show (`Cmd::Deps#dep_display_name`).
    let tree = ok(&sb, &["deps", "--tree", "--annotate", "jq"]);
    assert!(tree.contains("jq\n└── oniguruma \n"), "{tree:?}");

    // `uses --recursive` walks the whole graph.
    let uses = ok(&sb, &["uses", "--recursive", "pcre2"]);
    assert!(uses.lines().count() > 10, "{uses}");
    assert!(uses.lines().any(|l| l == "ripgrep"), "{uses}");

    // Nothing is outdated: the synthetic keg is the current version.
    assert_eq!(ok(&sb, &["outdated"]), "");
}

// ---------------------------------------------------------------------------
// 7. Update and auto-update
// ---------------------------------------------------------------------------

/// A cache of this test's own, seeded with the API file, so that running
/// `update` never replaces the packages file every other test reads.
fn private_cache() -> Option<tempfile::TempDir> {
    let dir = tempfile::tempdir().ok()?;
    let seed = support::seed_packages_file()?;
    let api = dir.path().join(format!(
        "api/internal/packages.{}.jws.json",
        support::bottle_tag()
    ));
    std::fs::create_dir_all(api.parent()?).ok()?;
    std::fs::copy(seed, &api).ok()?;
    // Keep the source mtime: `update` sends it as `If-Modified-Since`.
    let mtime = filetime::FileTime::from_last_modification_time(&std::fs::metadata(seed).ok()?);
    filetime::set_file_mtime(&api, mtime).ok()?;
    Some(dir)
}

#[test]
fn update_reports_once_and_is_then_up_to_date() {
    let sb = sandbox_or_skip!();
    network_or_skip!();
    let Some(cache) = private_cache() else {
        return;
    };

    let update = |sb: &Sandbox| {
        let out = sb
            .cmd()
            .env("HOMEBREW_CACHE", cache.path())
            .arg("update")
            .output()
            .expect("run fastbrew");
        let text = combined(&out);
        assert!(out.status.success(), "{text}");
        text
    };

    // Either the API file moved (a tap report) or it was already current.
    let first = update(&sb);
    assert!(
        first.contains("Already up-to-date.") || first.starts_with("Updated "),
        "{first}"
    );
    assert_eq!(
        update(&sb).trim(),
        "Already up-to-date.",
        "a second run revalidates and finds nothing new"
    );
}

#[test]
fn update_without_a_cached_api_file_and_no_network_fails() {
    let sb = sandbox_or_skip!();

    // A cache of its own, so the shared one keeps its API file.
    let empty = tempfile::tempdir().expect("tempdir");
    let out = sb
        .cmd()
        .env("HOMEBREW_CACHE", empty.path())
        .env("HOMEBREW_API_DOMAIN", "http://127.0.0.1:9")
        .arg("update")
        .output()
        .expect("run fastbrew");
    let text = combined(&out);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("Error: Failed to download"), "{text}");
    assert!(
        text.contains("packages."),
        "the failing URL names the packages file:\n{text}"
    );
}

#[test]
fn auto_update_runs_before_install_unless_it_is_turned_off() {
    let sb = sandbox_or_skip!();
    network_or_skip!();

    // A private cache: the marker's mtime is what the policy reads, and the
    // shared cache is used by every other test.
    let Some(cache) = private_cache() else {
        return;
    };
    let marker = cache.path().join("fastbrew/.last_auto_update");

    let run = |no_auto_update: bool| {
        let mut cmd = sb.cmd();
        cmd.env("HOMEBREW_CACHE", cache.path());
        if no_auto_update {
            cmd.env("HOMEBREW_NO_AUTO_UPDATE", "1");
        } else {
            cmd.env_remove("HOMEBREW_NO_AUTO_UPDATE");
        }
        let out = cmd
            .args(["install", "--dry-run", "hello"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", combined(&out));
    };

    // First run with the policy on: the check happens and leaves its marker.
    run(false);
    assert!(
        marker.is_file(),
        "the auto-update check records when it ran"
    );
    let stale = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    let set_stale = || {
        filetime::set_file_mtime(&marker, filetime::FileTime::from_system_time(stale)).unwrap();
    };
    let mtime = || std::fs::metadata(&marker).unwrap().modified().unwrap();

    set_stale();
    run(true);
    assert_eq!(
        mtime(),
        stale,
        "HOMEBREW_NO_AUTO_UPDATE keeps the check from running at all"
    );

    run(false);
    assert!(mtime() > stale, "a stale marker triggers the check again");
}

// ---------------------------------------------------------------------------
// 8. Failure paths
// ---------------------------------------------------------------------------

#[test]
fn an_unknown_formula_suggests_similar_names() {
    let sb = sandbox_or_skip!();
    let out = fails(&sb, &["install", "jqq"]);
    assert!(
        out.starts_with("Error: No available formula with the name \"jqq\". Did you mean "),
        "{out}"
    );
    assert!(out.contains("jq"), "{out}");
    assert!(out.trim_end().ends_with('?'), "{out}");
}

#[test]
fn disabled_formulae_are_refused_and_deprecated_ones_only_warn() {
    let sb = sandbox_or_skip!();
    let index = api_index().expect("the index is loaded");

    // Find the two states in the index rather than naming formulae that get
    // promoted from one to the other over time.
    let mut disabled = None;
    let mut deprecated = None;
    for entry in index.all_formulae() {
        let Some(status) = entry.deprecate_disable() else {
            continue;
        };
        let usable = entry.bottle_checksum.is_some() && entry.pour_bottle_args.is_none();
        if status.disabled && !status.deprecated && disabled.is_none() {
            disabled = Some(entry.name.clone());
        } else if status.deprecated && deprecated.is_none() && usable {
            deprecated = Some(entry.name.clone());
        }
        if disabled.is_some() && deprecated.is_some() {
            break;
        }
    }

    // `--ignore-dependencies` keeps the plan to the named formula, so the
    // scenario does not depend on what its dependencies happen to be.
    // `DeprecateDisable.type` reports `:deprecated` first, and
    // `FormulaInstaller#prelude_fetch` only refuses a `:disabled` one.
    let name = disabled.expect("the index carries disabled formulae");
    let out = fails(
        &sb,
        &["install", "--dry-run", "--ignore-dependencies", &name],
    );
    assert!(
        out.contains(&format!("Error: {name} has been disabled because it ")),
        "{out}"
    );
    assert!(
        out.contains("disabled on 20"),
        "the date sentence is there:\n{out}"
    );
    assert!(
        !out.contains("Would install"),
        "planning never starts:\n{out}"
    );

    // `--force` turns the refusal into a warning.
    let forced = ok(
        &sb,
        &[
            "install",
            "--dry-run",
            "--force",
            "--ignore-dependencies",
            &name,
        ],
    );
    assert!(
        forced.contains(&format!("Warning: {name} has been disabled")),
        "{forced}"
    );
    assert!(forced.contains("==> Would install"), "{forced}");

    let name = deprecated.expect("the index carries deprecated formulae");
    let out = ok(
        &sb,
        &["install", "--dry-run", "--ignore-dependencies", &name],
    );
    assert!(
        out.contains(&format!("Warning: {name} has been deprecated")),
        "{out}"
    );
    assert!(
        out.contains("==> Would install"),
        "the plan still runs:\n{out}"
    );
}

#[test]
fn conflicting_formulae_are_refused() {
    let sb = sandbox_or_skip!();
    network_or_skip!();

    // `aklomp-base64` and `base64` both install a `base64` binary.
    let entry = api_formula("aklomp-base64").expect("aklomp-base64 is in the index");
    assert!(!entry.conflicts.is_empty(), "the scenario needs a conflict");

    ok(&sb, &["install", "aklomp-base64"]);
    let out = fails(&sb, &["install", "base64"]);
    assert!(
        out.contains("Error: Cannot install base64 because conflicting formulae are installed."),
        "{out}"
    );
    assert!(
        out.contains("  aklomp-base64: because both install `base64` binaries"),
        "{out}"
    );
    assert!(
        out.contains("Please `brew unlink aklomp-base64` before continuing."),
        "{out}"
    );
    assert!(!sb.prefix.join("Cellar/base64").exists());
}

#[test]
fn a_corrupt_cached_bottle_is_downloaded_again() {
    let sb = sandbox_or_skip!();
    network_or_skip!();

    ok(&sb, &["install", "hello"]);
    let version = api_pkg_version("hello");
    ok(&sb, &["uninstall", "hello"]);

    // Find the cached blob by name and truncate it.
    let downloads = sb.cache.join("downloads");
    let blob = std::fs::read_dir(&downloads)
        .expect("the cache has a downloads directory")
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("--hello--") && n.ends_with(".tar.gz"))
        })
        .expect("the hello bottle is cached");
    std::fs::write(&blob, b"not a bottle").unwrap();

    let out = ok(&sb, &["install", "hello"]);
    assert!(
        !out.contains(&format!("Already downloaded: {}", blob.display())),
        "a bad checksum is not reused:\n{out}"
    );
    assert!(keg(&sb, "hello", &version).is_dir());
    assert!(
        std::fs::metadata(&blob).unwrap().len() > 12,
        "the blob was refetched"
    );
}

#[test]
fn an_interrupted_install_leaves_no_half_keg_behind() {
    let sb = sandbox_or_skip!();
    network_or_skip!();

    // Warm the cache so the kill lands during extraction, not the download.
    ok(&sb, &["install", "pcre2"]);
    let version = api_pkg_version("pcre2");
    ok(&sb, &["uninstall", "pcre2"]);
    std::fs::remove_dir_all(sb.prefix.join("Cellar/pcre2")).ok();

    let mut child = sb
        .cmd()
        .args(["install", "pcre2"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the install");
    std::thread::sleep(std::time::Duration::from_millis(40));
    let _ = child.kill();
    let _ = child.wait();

    // Whatever the kill interrupted, the rack never holds a keg directory
    // that is not a real version: extraction happens in `.fastbrew-<uuid>`
    // and is renamed into place in one step.
    let rack = sb.prefix.join("Cellar/pcre2");
    let rack_entries = |rack: &Path| -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(rack) else {
            return vec![];
        };
        entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    };
    for name in rack_entries(&rack) {
        assert!(
            name == version || name.starts_with(".fastbrew-"),
            "unexpected {name:?} in the rack"
        );
        if name == version {
            assert!(
                rack.join(&name).join("INSTALL_RECEIPT.json").is_file(),
                "a keg that landed is complete"
            );
        }
    }

    // `cleanup` sweeps whatever staging directory the kill left behind.
    ok(&sb, &["cleanup", "pcre2"]);
    let leftovers: Vec<String> = rack_entries(&rack)
        .into_iter()
        .filter(|n| n.starts_with(".fastbrew-"))
        .collect();
    assert!(leftovers.is_empty(), "cleanup left {leftovers:?}");

    // And a fresh install puts the prefix right again.
    let _ = std::fs::remove_dir_all(&rack);
    ok(&sb, &["install", "pcre2"]);
    assert!(
        keg(&sb, "pcre2", &version)
            .join("INSTALL_RECEIPT.json")
            .is_file()
    );
    assert!(sb.prefix.join("var/homebrew/linked/pcre2").is_symlink());
    assert_eq!(
        ok(&sb, &["list", "--versions", "pcre2"]).trim(),
        format!("pcre2 {version}")
    );
}

#[test]
fn a_failed_dependency_leaves_no_keg_for_the_next_run_to_mistake() {
    let sb = sandbox_or_skip!();
    network_or_skip!();

    let jq_version = api_pkg_version("jq");
    let oniguruma_version = api_pkg_version("oniguruma");

    // Make oniguruma's pour fail by putting a plain file where its rack goes.
    // Everything is downloaded and extracted before anything is finished, so
    // jq's bottle is already unpacked when its dependency fails.
    std::fs::create_dir_all(sb.prefix.join("Cellar")).unwrap();
    std::fs::write(sb.prefix.join("Cellar/oniguruma"), b"in the way\n").unwrap();

    let out = fails(&sb, &["install", "jq"]);
    assert!(out.contains("Error: oniguruma: "), "{out}");
    assert!(
        out.contains("Error: jq: skipped because a dependency failed to install"),
        "{out}"
    );
    assert!(
        !keg(&sb, "jq", &jq_version).exists(),
        "the half-installed keg is removed:\n{out}"
    );
    assert!(!sb.prefix.join("var/homebrew/linked/jq").exists(), "{out}");

    // A keg that survived some other way still must not be mistaken for an
    // installation, because it carries no receipt.
    std::fs::create_dir_all(keg(&sb, "jq", &jq_version).join("bin")).unwrap();

    std::fs::remove_file(sb.prefix.join("Cellar/oniguruma")).unwrap();
    let retry = ok(&sb, &["install", "jq"]);
    assert!(
        !retry.contains("already installed"),
        "the retry does the work:\n{retry}"
    );
    assert!(
        keg(&sb, "jq", &jq_version)
            .join("INSTALL_RECEIPT.json")
            .is_file(),
        "{retry}"
    );
    assert!(
        keg(&sb, "oniguruma", &oniguruma_version).is_dir(),
        "{retry}"
    );
    assert!(
        sb.prefix.join("var/homebrew/linked/jq").is_symlink(),
        "{retry}"
    );
    assert_eq!(
        ok(&sb, &["list", "--versions", "jq"]).trim(),
        format!("jq {jq_version}")
    );
}

#[test]
fn a_second_install_of_the_same_formula_hits_the_lock() {
    let sb = sandbox_or_skip!();
    network_or_skip!();

    let held = LockHolder::take(&sb, "hello");
    let out = fails(&sb, &["install", "hello"]);
    assert!(
        out.contains(&format!(
            "Error: A `brew` process has already locked {}",
            sb.prefix.join("Cellar/hello").display()
        )),
        "{out}"
    );
    assert!(
        out.contains("Please wait for it to finish or terminate it to continue."),
        "{out}"
    );
    assert!(!sb.prefix.join("Cellar/hello").exists());

    drop(held);
    ok(&sb, &["install", "hello"]);
    assert!(keg(&sb, "hello", &api_pkg_version("hello")).is_dir());
}

#[test]
fn build_from_source_asks_for_the_ruby_brew() {
    let sb = sandbox_or_skip!();
    let out = fails(&sb, &["install", "--build-from-source", "hello"]);
    assert!(
        out.contains("Error: FASTBREW_NO_DELEGATE is set, refusing to delegate to brew"),
        "{out}"
    );
    assert!(
        out.contains("`--build-from-source` needs the Ruby formula DSL"),
        "{out}"
    );
    assert!(!sb.prefix.join("Cellar/hello").exists());
}

#[test]
fn a_fixed_cellar_bottle_is_refused_when_the_prefix_is_too_long() {
    let sb = sandbox_or_skip!();
    network_or_skip!();

    // `diction` is bottled for `/opt/homebrew/Cellar` with no padded prefix
    // and has no dependencies, so the pour is refused on prefix length alone.
    let entry = api_formula("diction").expect("diction is in the index");
    assert_eq!(
        entry.bottle_cellar.as_deref(),
        Some("/opt/homebrew/Cellar"),
        "the scenario needs a fixed-cellar bottle"
    );

    // A prefix that is certainly longer than both /opt/homebrew and the
    // padded-prefix constant, whatever the temp directory is called.
    let deep = sb.prefix.join("d".repeat(80)).join("prefix");
    for sub in [
        "bin",
        "sbin",
        "etc",
        "include",
        "lib",
        "share",
        "Cellar",
        "opt",
        "var/homebrew/linked",
        "var/homebrew/pinned",
        "var/homebrew/locks",
    ] {
        std::fs::create_dir_all(deep.join(sub)).unwrap();
    }
    let out = sb
        .cmd()
        .env("HOMEBREW_PREFIX", &deep)
        .env("HOMEBREW_CELLAR", deep.join("Cellar"))
        .env("HOMEBREW_REPOSITORY", &deep)
        .args(["install", "diction"])
        .output()
        .expect("run fastbrew");
    let text = combined(&out);
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains(
            "diction was built for /opt/homebrew and can only be relocated to a prefix \
             with a maximum length of 13 characters"
        ),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "(yours is {}, {} characters)",
            deep.display(),
            deep.to_string_lossy().len()
        )),
        "{text}"
    );
    assert!(!deep.join("Cellar/diction").exists(), "nothing was poured");
}

/// A throwaway HTTP server that records the requests it is sent and answers
/// every one with 404, so a download against it fails quickly.
struct RecordingServer {
    port: u16,
    requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl RecordingServer {
    fn start() -> RecordingServer {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        let port = listener.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => break,
                    }
                }
                sink.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&head).into_owned());
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            }
        });
        RecordingServer { port, requests }
    }

    fn recorded(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

#[test]
fn a_taps_own_bottle_host_gets_no_registry_credentials() {
    let sb = sandbox_or_skip!();

    let server = RecordingServer::start();
    let root_url = format!("http://127.0.0.1:{}/v2/review/fixture", server.port);
    let remote = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(remote.path().join("Formula")).unwrap();
    std::fs::write(
        remote.path().join("Formula/fbauth.rb"),
        format!(
            r#"class Fbauth < Formula
  desc "Tap formula with its own bottle host"
  homepage "https://example.com/fbauth"
  url "https://example.com/fbauth-1.0.tar.gz"
  sha256 "7777777777777777777777777777777777777777777777777777777777777777"
  license "MIT"

  bottle do
    root_url "{root_url}"
    sha256 cellar: :any_skip_relocation, {tag}: "8888888888888888888888888888888888888888888888888888888888888888"
  end
end
"#,
            tag = support::bottle_tag()
        ),
    )
    .unwrap();
    git(remote.path(), &["init", "--initial-branch=main", "--quiet"]);
    git(remote.path(), &["add", "-A"]);
    git(remote.path(), &["commit", "--quiet", "-m", "Add fbauth"]);
    ok(
        &sb,
        &[
            "tap",
            "review/fixture",
            &format!("file://{}", remote.path().display()),
        ],
    );

    // The download fails (the server answers 404); what matters is what was
    // sent before it did.
    let out = sb
        .cmd()
        .env("HOMEBREW_GITHUB_PACKAGES_TOKEN", "supersecret-token")
        .env("HOMEBREW_GITHUB_PACKAGES_USER", "someone")
        .env("HOMEBREW_CURL_RETRIES", "0")
        .args(["install", "review/fixture/fbauth"])
        .output()
        .expect("run fastbrew");
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "the fake host serves nothing:\n{text}"
    );

    let requests = server.recorded();
    assert!(
        !requests.is_empty(),
        "the tap's root_url was contacted:\n{text}"
    );
    for request in &requests {
        let lower = request.to_lowercase();
        assert!(
            !lower.contains("authorization:"),
            "a tap's own bottle host must get no credentials:\n{request}"
        );
        assert!(!request.contains("supersecret-token"), "{request}");
        assert!(!request.contains("someone"), "{request}");
    }
}

// ---------------------------------------------------------------------------
// 9. One rack, one package
// ---------------------------------------------------------------------------

#[test]
fn two_taps_cannot_claim_the_same_rack_in_one_run() {
    let sb = sandbox_or_skip!();

    // A tap carrying its own `jq`, which wants the same rack as core's.
    let remote = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(remote.path().join("Formula")).unwrap();
    std::fs::write(
        remote.path().join("Formula/jq.rb"),
        format!(
            r#"class Jq < Formula
  desc "Tap formula for the install-plan tests"
  homepage "https://example.com/jq"
  url "https://example.com/jq-99.0.tar.gz"
  sha256 "7777777777777777777777777777777777777777777777777777777777777777"
  license "MIT"

  bottle do
    root_url "https://bottles.invalid/v2/review/fixture"
    sha256 cellar: :any_skip_relocation, {tag}: "8888888888888888888888888888888888888888888888888888888888888888"
  end
end
"#,
            tag = support::bottle_tag()
        ),
    )
    .unwrap();
    git(remote.path(), &["init", "--initial-branch=main", "--quiet"]);
    git(remote.path(), &["add", "-A"]);
    git(remote.path(), &["commit", "--quiet", "-m", "Add jq"]);
    ok(
        &sb,
        &[
            "tap",
            "review/fixture",
            &format!("file://{}", remote.path().display()),
        ],
    );

    // Named alone, the tap's formula is what gets planned.
    let tapped = ok(&sb, &["install", "--dry-run", "review/fixture/jq"]);
    assert!(tapped.contains("==> Would install 1 formula:"), "{tapped}");
    assert!(tapped.lines().any(|l| l.trim() == "jq"), "{tapped}");
    assert!(
        !tapped.contains("oniguruma"),
        "core jq's dependency must not appear:\n{tapped}"
    );

    // Both at once cannot work: one rack cannot hold two packages, and the
    // run has to say so before it writes anything.
    let clash = fails(&sb, &["install", "--dry-run", "jq", "review/fixture/jq"]);
    assert!(
        clash.contains(
            "Error: Formulae with the same name from different taps cannot be installed \
             at the same time:"
        ),
        "{clash}"
    );
    assert!(clash.contains("       * jq\n"), "{clash}");
    assert!(clash.contains("       * review/fixture/jq\n"), "{clash}");
    assert!(clash.contains("  brew uninstall jq"), "{clash}");

    // Deduplication still folds a formula that is both named and a dependency
    // into a single requested item.
    let both = ok(&sb, &["install", "--dry-run", "jq", "oniguruma"]);
    assert!(both.contains("==> Would install 2 formulae:"), "{both}");
    assert_eq!(
        both.matches("oniguruma").count(),
        1,
        "each package is planned once:\n{both}"
    );
}
