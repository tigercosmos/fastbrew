//! `services` integration tests. They only run inside `scripts/sandbox.sh`.
//!
//! Nothing here mutates launchd: `services::list` reads `launchctl print` and
//! `launchctl list`, which are read-only probes.

use std::path::{Path, PathBuf};

use fastbrew::config::Config;
use fastbrew::model::FormulaEntry;
use fastbrew::services::{self, ServiceStatus};

mod support;
use support::{sandbox_config, skip_unless_sandbox, strip_ansi};

/// A formula whose service writes to `<prefix>/var/log`, like `black` does.
fn foo_entry() -> FormulaEntry {
    let mut entry: FormulaEntry = serde_json::from_value(serde_json::json!({
        "desc": "Test service",
        "homepage": "https://example.com/foo",
        "stable_version": "1.0",
        "service_run_args": [[
            "$HOMEBREW_PREFIX/opt/foo/bin/foo",
            "--config",
            "$HOMEBREW_PREFIX/etc/foo.conf"
        ]],
        "service_args": [
            [":run_type", ":immediate"],
            [":working_dir", "$HOMEBREW_PREFIX"],
            [":log_path", "$HOMEBREW_PREFIX/var/log/foo.log"],
            [":error_log_path", "$HOMEBREW_PREFIX/var/log/foo.log"],
            [":keep_alive", {":always": true}]
        ]
    }))
    .expect("entry");
    entry.name = "foo".to_string();
    entry.tap = "homebrew/core".to_string();
    entry
}

/// Create `Cellar/foo/1.0` with a receipt and the generated service files.
fn install_fake_keg(cfg: &Config, entry: &FormulaEntry) -> PathBuf {
    let keg = cfg.cellar.join(&entry.name).join("1.0");
    std::fs::create_dir_all(keg.join("bin")).expect("mkdir keg");
    std::fs::write(
        keg.join("INSTALL_RECEIPT.json"),
        serde_json::json!({
            "homebrew_version": fastbrew::HOMEBREW_COMPAT_VERSION,
            "used_options": [],
            "unused_options": [],
            "built_as_bottle": true,
            "poured_from_bottle": true,
            "loaded_from_api": true,
            "loaded_from_internal_api": true,
            "installed_as_dependency": false,
            "installed_on_request": true,
            "changed_files": [],
            "time": 1_700_000_000,
            "source_modified_time": 1_700_000_000,
            "compiler": "clang",
            "aliases": [],
            "runtime_dependencies": [],
            "source": {"tap": "homebrew/core", "path": null, "tap_git_head": null},
            "arch": "arm64",
            "built_on": null
        })
        .to_string(),
    )
    .expect("write receipt");
    services::plist::install_service_files(cfg, entry, &keg).expect("install service files");
    keg
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
}

#[test]
fn generates_service_files_and_lists_them() {
    if skip_unless_sandbox() {
        return;
    }
    let cfg = sandbox_config();
    let entry = foo_entry();
    let keg = install_fake_keg(&cfg, &entry);

    // The plist and the systemd unit land in the keg with mode 0644.
    let plist_path = keg.join("homebrew.mxcl.foo.plist");
    let unit_path = keg.join("homebrew.foo.service");
    assert!(plist_path.is_file(), "{} missing", plist_path.display());
    assert!(unit_path.is_file(), "{} missing", unit_path.display());
    assert_eq!(mode(&plist_path), 0o644);
    assert_eq!(mode(&unit_path), 0o644);

    let plist = std::fs::read_to_string(&plist_path).expect("read plist");
    assert!(plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
    assert!(plist.ends_with("</plist>\n"));
    assert!(plist.contains("<key>Label</key>\n\t<string>homebrew.mxcl.foo</string>"));
    assert!(plist.contains(&format!(
        "<string>{}/opt/foo/bin/foo</string>",
        cfg.prefix.display()
    )));
    assert!(plist.contains(&format!(
        "<key>StandardOutPath</key>\n\t<string>{}/var/log/foo.log</string>",
        cfg.prefix.display()
    )));
    assert!(plist.contains("<key>KeepAlive</key>\n\t<true/>"));
    assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));

    // `<prefix>/var/log` is created because the plist mentions it.
    assert!(cfg.prefix.join("var/log").is_dir());

    let unit = std::fs::read_to_string(&unit_path).expect("read unit");
    assert!(unit.starts_with("[Unit]\nDescription=Homebrew generated unit for foo\n"));
    assert!(unit.contains("Restart=on-failure"));

    // `services list` finds the keg's service and reports it as not loaded.
    let listed = services::list(&cfg).expect("list");
    let foo = listed
        .iter()
        .find(|s| s.name == "foo")
        .expect("foo is listed");
    assert_eq!(foo.status, ServiceStatus::None);
    assert_eq!(foo.label, "homebrew.mxcl.foo");
    assert!(!foo.loaded);
    assert!(!foo.running);
    assert!(!foo.registered);
    assert_eq!(foo.user, None);
    assert_eq!(foo.pid, None);
    assert_eq!(foo.file.as_deref(), Some(plist_path.as_path()));
    assert!(!foo.schedulable);

    // The table matches Homebrew's layout: the status column is 15 wide while
    // its header is 15 - 9 (the width of the colour escapes) wide.
    let only_foo: Vec<_> = listed.into_iter().filter(|s| s.name == "foo").collect();
    let table = strip_ansi(&services::format_list_table(&only_foo));
    assert_eq!(table, "Name Status User File\nfoo  none                 \n");

    let json: serde_json::Value =
        serde_json::from_str(&services::list_json(&only_foo)).expect("json");
    assert_eq!(json[0]["name"], "foo");
    assert_eq!(json[0]["status"], "none");
    assert_eq!(json[0]["user"], serde_json::Value::Null);
    assert_eq!(json[0]["exit_code"], serde_json::Value::Null);
    assert_eq!(json[0]["file"], plist_path.to_string_lossy().as_ref());

    // `services info` reports the same state.
    let info = services::info(&cfg, "foo").expect("info");
    assert_eq!(info.status, ServiceStatus::None);
    let text = strip_ansi(&services::format_info(&info, false));
    assert!(text.starts_with("foo (homebrew.mxcl.foo)\n"), "{text}");
    assert!(text.contains("Running: false"), "{text}");

    // An uninstalled formula is a user error, not a panic.
    let err = services::info(&cfg, "nope").expect_err("nope is not installed");
    assert_eq!(err.to_string(), "Formula `nope` is not installed.");

    std::fs::remove_dir_all(cfg.cellar.join("foo")).expect("cleanup");
}

#[test]
fn cron_service_is_schedulable() {
    if skip_unless_sandbox() {
        return;
    }
    let cfg = sandbox_config();
    let mut entry: FormulaEntry = serde_json::from_value(serde_json::json!({
        "stable_version": "2.0",
        "service_run_args": [["$HOMEBREW_PREFIX/opt/cronny/bin/cronny"]],
        "service_args": [[":run_type", ":cron"], [":cron", "25 6 * * *"]]
    }))
    .expect("entry");
    entry.name = "cronny".to_string();

    let keg = install_fake_keg(&cfg, &entry);
    // Timed services also get a systemd timer.
    assert!(keg.join("homebrew.cronny.timer").is_file());

    let plist = std::fs::read_to_string(keg.join("homebrew.mxcl.cronny.plist")).expect("plist");
    assert!(
        plist.contains("<key>StartCalendarInterval</key>"),
        "{plist}"
    );
    assert!(
        plist.contains("<key>Hour</key>\n\t\t<integer>6</integer>"),
        "{plist}"
    );
    assert!(
        plist.contains("<key>Minute</key>\n\t\t<integer>25</integer>"),
        "{plist}"
    );

    let listed = services::list(&cfg).expect("list");
    let cronny = listed
        .iter()
        .find(|s| s.name == "cronny")
        .expect("cronny is listed");
    assert!(cronny.schedulable);
    assert_eq!(cronny.cron.as_deref(), Some("25 6 * * *"));
    assert_eq!(cronny.status, ServiceStatus::None);

    std::fs::remove_dir_all(cfg.cellar.join("cronny")).expect("cleanup");
}

// ---------------------------------------------------------------------------
// The `services` command itself
// ---------------------------------------------------------------------------

#[test]
fn services_command_prints_the_table_and_json() {
    let Some(sandbox) = support::Sandbox::new() else {
        eprintln!("no cached Homebrew API file available; skipping");
        return;
    };
    // A private prefix with one installed formula that ships a service.
    let cfg = Config::for_test(sandbox.prefix.parent().expect("sandbox root"));
    let entry = foo_entry();
    let keg = install_fake_keg(&cfg, &entry);

    // `print_table`: the status column is 15 wide, its header 15 - 9, so
    // without colour the rows trail nine spaces past the header.
    let table = sandbox.stdout(&["services", "list"]);
    assert_eq!(table, "Name Status User File\nfoo  none                 \n");
    // `--verbose`/`-q` do not change the table.
    assert_eq!(sandbox.stdout(&["services", "ls"]), table);

    let json: serde_json::Value =
        serde_json::from_str(&sandbox.stdout(&["services", "list", "--json"])).expect("json");
    assert_eq!(json[0]["name"], "foo");
    assert_eq!(json[0]["status"], "none");
    assert_eq!(
        json[0]["file"],
        keg.join("homebrew.mxcl.foo.plist")
            .to_string_lossy()
            .as_ref()
    );

    let info = strip_ansi(&sandbox.stdout(&["services", "info", "foo"]));
    assert_eq!(
        info,
        "foo (homebrew.mxcl.foo)\nRunning: false\nLoaded: false\nSchedulable: false\n"
    );
    let verbose = strip_ansi(&sandbox.stdout(&["services", "info", "-v", "foo"]));
    assert!(verbose.contains("Registered at login: false"), "{verbose}");
    assert!(verbose.contains("Command: "), "{verbose}");

    let all = sandbox.stdout(&["services", "info", "--all", "--json"]);
    let all: serde_json::Value = serde_json::from_str(&all).expect("json");
    assert_eq!(all[0]["name"], "foo");
    assert_eq!(all[0]["service_name"], "homebrew.mxcl.foo");

    // `services info` for a formula without a service reports every flag
    // false rather than failing, like `FormulaWrapper#to_hash`.
    sandbox.add_keg("jq", &support::api_pkg_version("jq"), true);
    let info = strip_ansi(&sandbox.stdout(&["services", "info", "jq"]));
    assert_eq!(
        info,
        "jq (sh.brew.jq)\nRunning: false\nLoaded: false\nSchedulable: false\n"
    );

    // `start` on a formula without a service is Homebrew's wording.
    let out = sandbox.run(&["services", "start", "jq"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "Error: Formula `jq` has not implemented #plist, #service or provided a locatable service file."
    );

    // `cleanup` has nothing to do in a fresh prefix.
    let out = sandbox.stdout(&["services", "cleanup"]);
    assert_eq!(out.trim(), "All user-space services OK, nothing cleaned...");
}
