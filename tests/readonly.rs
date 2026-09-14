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

mod support;
use support::{Sandbox, bottle_tag, network_tests_enabled, symlink};

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
fn deps_uses_recorded_runtime_dependencies_for_installed_formulae() {
    let sandbox = sandbox_or_skip!();
    sandbox.add_keg("jq", "1.8.2", true);

    // The fake receipt records no runtime dependencies, so Homebrew (and
    // fastbrew) report none for the installed formula rather than walking the
    // declared graph.
    assert_eq!(sandbox.stdout(&["deps", "jq"]), "");
    // A flag that asks for the declared graph switches back to the API data.
    assert_eq!(
        sandbox.stdout(&["deps", "--tree", "jq"]),
        "jq\n└── oniguruma\n\n"
    );

    // Without `HOMEBREW_NO_ENV_HINTS` the mismatch is announced on stderr.
    let out = sandbox
        .cmd()
        .env_remove("HOMEBREW_NO_ENV_HINTS")
        .args(["deps", "--tree", "jq"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with(
            "Warning: `fastbrew deps` is not the actual runtime dependencies because --tree was passed!\n"
        ),
        "{stderr}"
    );
}

#[test]
fn info_cask_prints_artifacts_with_resolved_paths() {
    let sandbox = sandbox_or_skip!();
    let out = sandbox
        .cmd()
        .env("HOMEBREW_CASK_OPTS", "--appdir=/Applications")
        .args(["info", "--cask", "ghostty"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("==> ghostty (Ghostty): "), "{text}");
    assert!(text.contains(" (auto_updates)\n"), "{text}");
    assert!(text.contains("\nNot installed\n"), "{text}");
    assert!(
        text.contains(
            "From: https://github.com/Homebrew/homebrew-cask/blob/HEAD/Casks/g/ghostty.rb\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("\n==> Artifacts\nGhostty.app (App)\n"),
        "{text}"
    );
    assert!(
        text.contains("/Applications/Ghostty.app/Contents/Resources/man/man1/ghostty.1 (Manpage)"),
        "{text}"
    );

    let out = sandbox.run(&["info", "--cask", "nonexistentxyz"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "Error: Cask 'nonexistentxyz' is unavailable: No Cask with this name exists."
    );
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
    // All three `output_update_report` outcomes are legitimate here: the
    // cached file may be current (304), or the fetch may bring a new
    // generation with or without package changes.
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("Already up-to-date.")
            || text.contains("No changes to formulae or casks.")
            || text.contains("Updated ")
            || text.contains("==>"),
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
