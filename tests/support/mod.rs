//! Shared helpers for the integration tests.
//!
//! Two flavours of isolation live here:
//!
//! - Library tests (`services`, `tap`) run against the ambient sandbox
//!   `scripts/sandbox.sh test` creates; outside it they report a skip and pass.
//! - CLI tests ([`Sandbox`]) run the built binary against a private prefix in
//!   a fresh temp directory, sharing one cache seeded with a copy of the
//!   internal packages file. Nothing ever touches `/opt/homebrew` or the
//!   user's Homebrew cache.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use assert_cmd::prelude::*;
use tempfile::TempDir;

use fastbrew::config::Config;

/// True when the test should be skipped because there is no sandbox.
pub fn skip_unless_sandbox() -> bool {
    if std::env::var_os("FASTBREW_REQUIRE_SANDBOX").is_some()
        && std::env::var_os("HOMEBREW_PREFIX").is_some()
    {
        return false;
    }
    eprintln!("skipping: run these tests with `scripts/sandbox.sh test`");
    true
}

pub fn sandbox_config() -> Config {
    Config::from_env().expect("sandbox config")
}

/// Drop SGR escapes so expected output does not depend on TTY detection.
pub fn strip_ansi(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] == '\u{1b}' && chars.get(i + 1) == Some(&'[') {
            i += 2;
            while i < chars.len() && !chars[i].is_ascii_alphabetic() {
                i += 1;
            }
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn bottle_tag() -> String {
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
pub fn seed_packages_file() -> Option<&'static PathBuf> {
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
pub fn shared_cache() -> Option<&'static PathBuf> {
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

pub struct Sandbox {
    _dir: TempDir,
    pub prefix: PathBuf,
    pub cache: PathBuf,
    pub home: PathBuf,
}

impl Sandbox {
    /// Build a sandbox, or `None` when no cached API file is available.
    pub fn new() -> Option<Sandbox> {
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

    pub fn cmd(&self) -> Command {
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

    pub fn run(&self, args: &[&str]) -> std::process::Output {
        self.cmd().args(args).output().expect("run fastbrew")
    }

    pub fn stdout(&self, args: &[&str]) -> String {
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
    pub fn add_keg(&self, name: &str, version: &str, on_request: bool) {
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

pub fn symlink(target: &str, link: &Path) {
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(target, link).unwrap();
}

pub fn network_tests_enabled() -> bool {
    std::env::var("FASTBREW_TEST_NETWORK").is_ok_and(|v| !v.is_empty())
}
