//! Homebrew-style terminal output.
//!
//! - `ohai(title)`: `==> title` with `==>` bold blue and the title bold on a TTY.
//! - `oh1(title)`: same as ohai but used for top-level headers (Homebrew prints
//!   `==> ` in bold green? No: `oh1` uses the same blue arrows; keep one style).
//! - `opoo(msg)`: `Warning: msg` (yellow label) to stderr.
//! - `onoe(msg)`/`ofail`: `Error: msg` (red label) to stderr.
//! - `pretty_installed(name)`: name in bold green with ✔ on TTY, `name (installed)` otherwise.
//! - `columns(items)`: `ls -C`-style column layout for a TTY using the terminal width.
//! - `Tty::color_enabled()` honoring `HOMEBREW_NO_COLOR`, `HOMEBREW_COLOR`, `NO_COLOR`, `isatty`.
//! - `pretty_duration(secs)`: `2 seconds`, `1 minute 3 seconds`.
//! - `format_url(url)`: underlined on TTY.

use std::io::Write;

pub fn ohai(title: &str) {
    println!("{}", format_ohai(title));
}

pub fn format_ohai(_title: &str) -> String {
    todo!("output::format_ohai")
}

pub fn opoo(msg: &str) {
    eprintln!("{}", format_warning(msg));
}

pub fn format_warning(_msg: &str) -> String {
    todo!("output::format_warning")
}

pub fn onoe(msg: &str) {
    eprintln!("{}", format_error(msg));
}

pub fn format_error(_msg: &str) -> String {
    todo!("output::format_error")
}

pub fn color_enabled() -> bool {
    todo!("output::color_enabled")
}

pub fn stdout_is_tty() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdout())
}

pub fn terminal_width() -> usize {
    todo!("output::terminal_width")
}

/// Print items in columns on a TTY, one per line otherwise.
pub fn print_columns(_items: &[String]) {
    todo!("output::print_columns")
}

pub fn pretty_duration(_secs: f64) -> String {
    todo!("output::pretty_duration")
}

pub fn flush() {
    let _ = std::io::stdout().flush();
}
