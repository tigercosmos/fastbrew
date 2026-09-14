//! Formatting helpers shared by the query commands.
//!
//! These mirror `Library/Homebrew/utils/output.rb` and `utils/formatter.rb`
//! rather than `crate::output`, because Homebrew's install-status decorations
//! collapse to the bare name when stdout is not a TTY (whereas
//! `output::pretty_installed` renders `name (installed)`).

use crate::output;

/// `pretty_installed`: `name ✔` on a TTY, the bare name otherwise.
pub fn pretty_installed(name: &str) -> String {
    if output::stdout_is_tty() {
        format!("{} {}", output::bold(name), output::green("✔"))
    } else {
        name.to_string()
    }
}

/// `pretty_uninstalled`: `name ✘` on a TTY, the bare name otherwise.
pub fn pretty_uninstalled(name: &str) -> String {
    if output::stdout_is_tty() {
        format!("{} {}", output::bold(name), output::red("✘"))
    } else {
        name.to_string()
    }
}

/// `pretty_unmarked`: bold on a TTY.
pub fn pretty_unmarked(name: &str) -> String {
    if output::stdout_is_tty() {
        output::bold(name)
    } else {
        name.to_string()
    }
}

/// Decorate a dependency or search result by its install state.
pub fn install_status(name: &str, installed: bool, mark_uninstalled: bool) -> String {
    if installed {
        pretty_installed(name)
    } else if mark_uninstalled {
        pretty_uninstalled(name)
    } else {
        pretty_unmarked(name)
    }
}

/// `Formatter.number_readable`: group digits in threes.
pub fn number_readable(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `Pathname#abv`: `4,041 files, 64.9MB`, or just the size for a single file.
pub fn abv(files: u64, bytes: u64) -> String {
    let size = crate::keg::disk_usage_readable(bytes);
    if files > 1 {
        format!("{} files, {size}", number_readable(files))
    } else {
        size
    }
}

/// `Tab#to_s` for a poured bottle: the `info` provenance line.
pub fn tab_line(receipt: &crate::model::FormulaReceipt) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(
        if receipt.poured_from_bottle {
            "Poured from bottle"
        } else {
            "Built from source"
        }
        .to_string(),
    );
    if receipt.loaded_from_internal_api {
        parts.push("using the internal formulae.brew.sh API".to_string());
    } else if receipt.loaded_from_api {
        parts.push("using the formulae.brew.sh API".to_string());
    }
    if let Some(t) = receipt.time {
        parts.push(format!("on {}", local_time(t)));
    }
    if !receipt.used_options.is_empty() {
        parts.push("with:".to_string());
        parts.push(receipt.used_options.join(" "));
    }
    parts.join(" ")
}

/// `Time.at(t).strftime("%Y-%m-%d at %H:%M:%S")` in the local zone.
pub fn local_time(secs: u64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(secs as i64, 0).single() {
        Some(t) => t.format("%Y-%m-%d at %H:%M:%S").to_string(),
        None => secs.to_string(),
    }
}

/// Print names in columns on a TTY, one per line otherwise (`Formatter.columns`).
pub fn print_names(items: &[String]) {
    output::print_columns(items);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_numbers() {
        assert_eq!(number_readable(0), "0");
        assert_eq!(number_readable(999), "999");
        assert_eq!(number_readable(1_000), "1,000");
        assert_eq!(number_readable(4_041), "4,041");
        assert_eq!(number_readable(105_697), "105,697");
    }

    #[test]
    fn abv_strings() {
        assert_eq!(abv(1, 5_300_000), "5.3MB");
        assert_eq!(abv(20, 1_235_416), "20 files, 1.2MB");
        assert_eq!(abv(4_041, 64_900_000), "4,041 files, 64.9MB");
    }
}
