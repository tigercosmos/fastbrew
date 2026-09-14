//! Shared helpers for the `services` and `tap` integration tests.
//!
//! Every integration test runs inside the sandbox created by
//! `scripts/sandbox.sh test`; outside it they report a skip and pass.

#![allow(dead_code)]

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
