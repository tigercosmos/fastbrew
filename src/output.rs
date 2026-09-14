//! Homebrew-style terminal output (`Library/Homebrew/utils/formatter.rb`,
//! `utils/tty.rb`, `utils/output.rb`).

use std::io::{IsTerminal, Write};
use std::sync::OnceLock;

/// Whether color escapes should be emitted on stdout/stderr.
///
/// Rules follow `Tty.color?`: `HOMEBREW_COLOR` forces on, `HOMEBREW_NO_COLOR`
/// or `NO_COLOR` force off, otherwise color only when stdout is a TTY.
pub fn color_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        if set("HOMEBREW_COLOR") {
            return true;
        }
        if set("HOMEBREW_NO_COLOR") || set("NO_COLOR") {
            return false;
        }
        std::io::stdout().is_terminal()
    })
}

pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const UNDERLINE: &str = "\x1b[4m";
const BLUE: &str = "\x1b[34m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";

fn paint(s: &str, codes: &[&str]) -> String {
    if color_enabled() {
        format!("{}{s}{RESET}", codes.concat())
    } else {
        s.to_string()
    }
}

pub fn bold(s: &str) -> String {
    paint(s, &[BOLD])
}

pub fn underline(s: &str) -> String {
    paint(s, &[UNDERLINE])
}

pub fn green(s: &str) -> String {
    paint(s, &[GREEN])
}

pub fn red(s: &str) -> String {
    paint(s, &[RED])
}

/// `==> title`: arrows bold blue, title bold (`Formatter.headline`).
pub fn format_ohai(title: &str) -> String {
    format!("{} {}", paint("==>", &[BOLD, BLUE]), bold(title))
}

pub fn ohai(title: &str) {
    println!("{}", format_ohai(title));
}

/// `ohai` with a body printed below the title.
pub fn ohai_with(title: &str, body: &str) {
    ohai(title);
    println!("{body}");
}

/// `Formatter.warning`: `Warning: msg` with a bold yellow label.
pub fn format_warning(msg: &str) -> String {
    format_label("Warning", msg, YELLOW)
}

pub fn opoo(msg: &str) {
    eprintln!("{}", format_warning(msg));
}

/// `Formatter.error`: `Error: msg` with a bold red label.
pub fn format_error(msg: &str) -> String {
    format_label("Error", msg, RED)
}

pub fn onoe(msg: &str) {
    eprintln!("{}", format_error(msg));
}

fn format_label(label: &str, msg: &str, color: &str) -> String {
    format!("{} {msg}", paint(&format!("{label}:"), &[BOLD, color]))
}

/// `Formatter.url`: underlined on a TTY.
pub fn format_url(url: &str) -> String {
    underline(url)
}

/// `Formatter.success`: bold green with a check mark on a TTY (`name ✔`), plain otherwise.
pub fn pretty_installed(name: &str) -> String {
    if color_enabled() {
        format!("{} {}", bold(name), paint("✔", &[BOLD, GREEN]))
    } else {
        format!("{name} (installed)")
    }
}

/// Uninstalled marker used by `search`/`info`: name with ✘ on a TTY, plain otherwise.
pub fn pretty_uninstalled(name: &str) -> String {
    if color_enabled() {
        format!("{} {}", bold(name), paint("✘", &[BOLD, RED]))
    } else {
        name.to_string()
    }
}

/// Terminal width in columns (`COLUMNS`, then ioctl, then 80).
pub fn terminal_width() -> usize {
    if let Some(c) = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .filter(|c| *c > 0)
    {
        return c;
    }
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes a winsize struct; stdout is a valid fd.
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 {
        ws.ws_col as usize
    } else {
        80
    }
}

/// Lay out `items` like `ls -C`: columns filled top-to-bottom, sized to the terminal width.
pub fn columns(items: &[String], width: usize) -> String {
    if items.is_empty() {
        return String::new();
    }
    let widths: Vec<usize> = items
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.as_str()))
        .collect();
    let longest = widths.iter().copied().max().unwrap_or(0);
    let gap = 2;
    let per_row = ((width + gap) / (longest + gap)).max(1);
    let rows = items.len().div_ceil(per_row);
    let cols = items.len().div_ceil(rows);
    let col_widths: Vec<usize> = (0..cols)
        .map(|c| {
            widths
                .iter()
                .skip(c * rows)
                .take(rows)
                .copied()
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for r in 0..rows {
        let mut line = String::new();
        for (c, col_width) in col_widths.iter().enumerate() {
            let i = c * rows + r;
            if i >= items.len() {
                break;
            }
            line.push_str(&items[i]);
            if c + 1 < cols && (c + 1) * rows + r < items.len() {
                for _ in widths[i]..col_width + gap {
                    line.push(' ');
                }
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Print items in columns on a TTY, one per line otherwise.
pub fn print_columns(items: &[String]) {
    if stdout_is_tty() {
        print!("{}", columns(items, terminal_width()));
    } else {
        for i in items {
            println!("{i}");
        }
    }
}

/// `pretty_duration`: `2 seconds`, `1 minute 3 seconds`, `1 hour 2 minutes`.
pub fn pretty_duration(secs: f64) -> String {
    let total = secs.round() as u64;
    if total < 60 {
        return plural(total, "second");
    }
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    let mut parts = vec![];
    if h > 0 {
        parts.push(plural(h, "hour"));
    }
    if m > 0 {
        parts.push(plural(m, "minute"));
    }
    if s > 0 && h == 0 {
        parts.push(plural(s, "second"));
    }
    parts.join(" ")
}

pub fn plural(n: u64, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {word}s")
    }
}

pub fn flush() {
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_layout() {
        let items: Vec<String> = ["aa", "b", "ccc", "d", "e"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(columns(&items, 12), "aa   d\nb    e\nccc\n");
        assert_eq!(columns(&items, 20), "aa  ccc  e\nb   d\n");
        assert_eq!(columns(&items, 3), "aa\nb\nccc\nd\ne\n");
    }

    #[test]
    fn durations() {
        assert_eq!(pretty_duration(1.2), "1 second");
        assert_eq!(pretty_duration(63.0), "1 minute 3 seconds");
        assert_eq!(pretty_duration(3720.0), "1 hour 2 minutes");
    }
}
