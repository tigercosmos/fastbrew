//! Integration tests for the command tree itself: the commands that answer
//! without an installed package, and the wiring of the mutating ones.
//!
//! Every test drives the built binary inside a private sandbox (see
//! `tests/support`), so nothing touches `/opt/homebrew` or the user's cache.
//! Delegation is disabled (`FASTBREW_NO_DELEGATE=1`), which is also what
//! `scripts/sandbox.sh` exports.

mod support;
use support::Sandbox;

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

// ---------------------------------------------------------------------------
// commands / help / doctor
// ---------------------------------------------------------------------------

#[test]
fn commands_lists_the_built_in_section() {
    let sandbox = sandbox_or_skip!();
    let out = sandbox.stdout(&["commands"]);
    assert!(out.starts_with("==> Built-in commands\n"), "{out}");
    for expected in ["install", "services", "tap-info", "doctor", "completions"] {
        assert!(
            out.lines().any(|l| l == expected),
            "`{expected}` missing from:\n{out}"
        );
    }
    // `Commands.internal_developer_commands` is empty for fastbrew and
    // `next if commands.blank?` skips empty sections entirely.
    assert!(!out.contains("Built-in developer commands"), "{out}");

    // `--quiet` drops the headers.
    let quiet = sandbox.stdout(&["commands", "--quiet"]);
    assert!(!quiet.contains("==>"), "{quiet}");
    assert!(quiet.lines().any(|l| l == "install"), "{quiet}");
}

#[test]
fn help_prints_usage_for_one_command() {
    let sandbox = sandbox_or_skip!();
    let out = sandbox.stdout(&["help", "install"]);
    assert!(out.starts_with("Usage: fastbrew install "), "{out}");
    assert!(out.contains("--only-dependencies"), "{out}");

    // An alias resolves and says so.
    let alias = sandbox.stdout(&["help", "ls"]);
    assert!(alias.starts_with("Usage: fastbrew list "), "{alias}");
    assert!(alias.contains("`ls` is an alias for `list`."), "{alias}");

    // Bare `help` is the summary.
    let summary = sandbox.stdout(&["help"]);
    assert!(summary.contains("Example usage:"), "{summary}");
}

#[test]
fn doctor_lists_checks_and_passes_on_a_clean_prefix() {
    let sandbox = sandbox_or_skip!();
    let checks = sandbox.stdout(&["doctor", "--list-checks"]);
    assert!(
        checks.lines().all(|l| l.starts_with("check_")),
        "every line is a check name:\n{checks}"
    );
    assert!(checks.contains("check_for_broken_symlinks"), "{checks}");

    let out = sandbox.run(&["doctor"]);
    assert!(out.status.success(), "a fresh prefix is healthy");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "Your system is ready to brew."
    );

    // A broken symlink in the prefix is a finding, and `doctor` then exits 1.
    support::symlink("../nowhere/at/all", &sandbox.prefix.join("bin/dangling"));
    let out = sandbox.run(&["doctor"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Broken symlinks were found:"), "{stderr}");
    assert!(stderr.contains("dangling"), "{stderr}");
}

// ---------------------------------------------------------------------------
// services
// ---------------------------------------------------------------------------

#[test]
fn services_list_is_quiet_on_an_empty_sandbox() {
    let sandbox = sandbox_or_skip!();
    let out = sandbox.run(&["services", "list"]);
    assert!(out.status.success());
    // `ListSubcommand#run` prints the "No services available to control with
    // `brew services`" hint only when stderr is a terminal, so a piped run
    // produces nothing at all.
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");

    assert_eq!(sandbox.stdout(&["services", "list", "--json"]).trim(), "[]");
    // `ls` is Homebrew's alias for `list`, and `list` is the default.
    assert_eq!(sandbox.stdout(&["services", "ls"]), "");
    assert_eq!(sandbox.stdout(&["services"]), "");
}

#[test]
fn services_reject_unknown_subcommands_and_missing_names() {
    let sandbox = sandbox_or_skip!();
    let out = sandbox.run(&["services", "bogus"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Error: Unknown subcommand: bogus"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `Cli.check!` wording.
    let out = sandbox.run(&["services", "start"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("Formula(e) missing, please provide a formula name or use `--all`."),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // An unknown formula fails in name resolution, like `Formulary.factory`.
    let out = sandbox.run(&["services", "info", "definitelynotaformula"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("No available formula with the name"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// link / unlink / pin
// ---------------------------------------------------------------------------

/// Create `Cellar/<name>/<version>/bin/<name>` with a minimal receipt.
fn synthetic_keg(sandbox: &Sandbox, name: &str, version: &str) -> std::path::PathBuf {
    let keg = sandbox.prefix.join("Cellar").join(name).join(version);
    std::fs::create_dir_all(keg.join("bin")).unwrap();
    std::fs::write(keg.join("bin").join(name), "#!/bin/sh\n").unwrap();
    std::fs::write(
        keg.join("INSTALL_RECEIPT.json"),
        serde_json::json!({"source": {"tap": "homebrew/core"}, "aliases": []}).to_string(),
    )
    .unwrap();
    keg
}

#[test]
fn link_and_unlink_a_synthetic_keg() {
    let sandbox = sandbox_or_skip!();
    let keg = synthetic_keg(&sandbox, "foo", "1.0");

    let out = sandbox.stdout(&["link", "foo"]);
    assert!(
        out.starts_with(&format!("Linking {}... ", keg.display())),
        "{out}"
    );
    assert!(out.trim_end().ends_with("symlinks created."), "{out}");
    let linked = sandbox.prefix.join("bin/foo");
    assert!(linked.is_symlink(), "bin/foo is a symlink");
    assert!(sandbox.prefix.join("opt/foo").exists(), "opt record");
    assert!(
        sandbox.prefix.join("var/homebrew/linked/foo").exists(),
        "linked record"
    );

    // Linking again is a warning, not an error.
    let out = sandbox.run(&["link", "foo"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("Warning: Already linked: {}", keg.display())),
        "{stderr}"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("To relink, run:\n  brew unlink foo"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    let out = sandbox.stdout(&["unlink", "foo"]);
    assert!(
        out.starts_with(&format!("Unlinking {}... ", keg.display())),
        "{out}"
    );
    assert!(out.trim_end().ends_with("symlinks removed."), "{out}");
    assert!(!linked.exists(), "bin/foo is gone");

    // `--dry-run` lists rather than links.
    let out = sandbox.stdout(&["link", "--dry-run", "foo"]);
    assert!(out.starts_with("Would link:\n"), "{out}");
    assert!(out.contains("bin/foo"), "{out}");
    assert!(!linked.exists(), "still not linked after --dry-run");
}

#[test]
fn link_reports_a_missing_keg_like_homebrew() {
    let sandbox = sandbox_or_skip!();
    let out = sandbox.run(&["link", "jq"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        format!(
            "Error: No such keg: {}",
            sandbox.prefix.join("Cellar/jq").display()
        )
    );
}

#[test]
fn pin_reports_a_formula_that_is_not_installed() {
    let sandbox = sandbox_or_skip!();
    // `cmd/pin.rb` uses `ofail`, so the message is an error and the exit is 1.
    let out = sandbox.run(&["pin", "jq"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "Error: jq not installed"
    );

    // `cmd/unpin.rb` uses `onoe`, which leaves the exit status alone.
    let out = sandbox.run(&["unpin", "jq"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "Error: jq not installed"
    );
}

// ---------------------------------------------------------------------------
// delegation
// ---------------------------------------------------------------------------

#[test]
fn delegated_commands_fail_when_delegation_is_off() {
    let sandbox = sandbox_or_skip!();
    for (args, reason) in [
        (vec!["bundle"], "`bundle` is not implemented by fastbrew"),
        (vec!["test", "jq"], "`test` is not implemented by fastbrew"),
        (vec!["--env"], "`--env` is not implemented by fastbrew"),
        (
            vec!["install", "--build-from-source", "jq"],
            "`--build-from-source` needs the Ruby formula DSL",
        ),
        (
            vec!["install", "--HEAD", "jq"],
            "`--HEAD` installs need the Ruby formula DSL",
        ),
    ] {
        let out = sandbox.run(&args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr).trim(),
            format!("Error: FASTBREW_NO_DELEGATE is set, refusing to delegate to brew ({reason}).")
        );
    }
}

#[test]
fn an_unknown_command_says_what_it_would_delegate() {
    let sandbox = sandbox_or_skip!();
    let out = sandbox.run(&["definitely-not-a-command"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "Error: FASTBREW_NO_DELEGATE is set, refusing to delegate to brew \
         (`definitely-not-a-command` is not implemented by fastbrew)."
    );
}

// ---------------------------------------------------------------------------
// completions / update
// ---------------------------------------------------------------------------

#[test]
fn completions_link_state_round_trips() {
    let sandbox = sandbox_or_skip!();
    assert_eq!(
        sandbox.stdout(&["completions"]).trim(),
        "Completions are not linked."
    );
    assert_eq!(
        sandbox.stdout(&["completions", "link"]).trim(),
        "Completions are now linked."
    );
    assert_eq!(
        sandbox.stdout(&["completions", "state"]).trim(),
        "Completions are linked."
    );
    assert_eq!(
        sandbox.stdout(&["completions", "unlink"]).trim(),
        "Completions are no longer linked."
    );
    assert_eq!(
        sandbox.stdout(&["completions"]).trim(),
        "Completions are not linked."
    );
}

#[test]
fn update_falls_back_to_the_cached_api_file_when_offline() {
    let sandbox = sandbox_or_skip!();
    // Point the API at a port nothing listens on: the cached file has to be
    // enough, with the warning `fetch_api_file` prints.
    let out = sandbox
        .cmd()
        .args(["update"])
        .env("HOMEBREW_API_DOMAIN", "http://127.0.0.1:1/api")
        .env("HOMEBREW_CURL_RETRIES", "0")
        .output()
        .expect("run fastbrew");
    assert!(
        out.status.success(),
        "a cached API file makes `update` succeed offline: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("update failed, falling back to cached version."),
        "{stderr}"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "Already up-to-date."
    );
}
