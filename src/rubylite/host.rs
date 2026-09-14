//! Host predicates used to pick a branch of `on_macos`/`if OS.mac?` and friends.

use crate::platform::{Arch, BottleTag, MacOsVersion};

/// What `on_*` blocks and `if` conditions are evaluated against.
#[derive(Debug, Clone)]
pub struct HostCtx {
    pub linux: bool,
    pub arm: bool,
    /// macOS major version (`26` for Tahoe); `None` on Linux.
    pub macos_major: Option<u32>,
    pub tag: BottleTag,
}

impl HostCtx {
    pub fn from_tag(tag: &BottleTag) -> HostCtx {
        HostCtx {
            linux: tag.is_linux(),
            arm: tag.arch == Some(Arch::Arm64),
            macos_major: if tag.is_linux() {
                None
            } else {
                MacOsVersion::major_for_symbol(&tag.system)
            },
            tag: tag.clone(),
        }
    }

    pub fn macos(&self) -> bool {
        !self.linux
    }

    /// Evaluate a boolean expression; `None` when rubylite does not understand it.
    pub fn eval(&self, expr: &str) -> Option<bool> {
        self.or_expr(expr.trim())
    }

    fn or_expr(&self, s: &str) -> Option<bool> {
        if let Some(parts) = split_operator(s, "||") {
            let mut any_unknown = false;
            for part in &parts {
                match self.and_expr(part.trim()) {
                    Some(true) => return Some(true),
                    Some(false) => {}
                    None => any_unknown = true,
                }
            }
            return if any_unknown { None } else { Some(false) };
        }
        self.and_expr(s)
    }

    fn and_expr(&self, s: &str) -> Option<bool> {
        if let Some(parts) = split_operator(s, "&&") {
            let mut any_unknown = false;
            for part in &parts {
                match self.unary(part.trim()) {
                    Some(false) => return Some(false),
                    Some(true) => {}
                    None => any_unknown = true,
                }
            }
            return if any_unknown { None } else { Some(true) };
        }
        self.unary(s)
    }

    fn unary(&self, s: &str) -> Option<bool> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix('!') {
            return self.unary(rest).map(|b| !b);
        }
        if s.starts_with('(') && s.ends_with(')') && balanced(&s[1..s.len() - 1]) {
            return self.or_expr(&s[1..s.len() - 1]);
        }
        self.atom(s)
    }

    fn atom(&self, s: &str) -> Option<bool> {
        let s = s.trim();
        // Comparisons against a macOS release symbol.
        for op in ["<=", ">=", "==", "!=", "<", ">"] {
            if let Some((lhs, rhs)) = split_once_outside(s, op) {
                let lhs = lhs.trim();
                let rhs = rhs.trim();
                if is_macos_version(lhs) {
                    let want = release_major(rhs)?;
                    let have = self.macos_major?;
                    return Some(match op {
                        "<=" => have <= want,
                        ">=" => have >= want,
                        "==" => have == want,
                        "!=" => have != want,
                        "<" => have < want,
                        ">" => have > want,
                        _ => unreachable!(),
                    });
                }
                if matches!(lhs, "Hardware::CPU.type" | "Hardware::CPU.arch") {
                    let want = rhs.trim_start_matches(':');
                    let is = match want {
                        "arm" | "arm64" | "aarch64" => self.arm,
                        "intel" | "x86_64" => !self.arm,
                        _ => return None,
                    };
                    return Some(if op == "!=" { !is } else { is });
                }
                return None;
            }
        }

        match s {
            "OS.mac?" | "OS.mac" | "OS::Mac?" => Some(self.macos()),
            "OS.linux?" | "OS.linux" => Some(self.linux),
            "Hardware::CPU.arm?" | "Hardware::CPU.physical_cpu_arm64?" => Some(self.arm),
            "Hardware::CPU.intel?" => Some(!self.arm),
            "Hardware::CPU.is_64_bit?" => Some(true),
            // Rosetta is never used for the bottle fastbrew picks.
            "Hardware::CPU.in_rosetta2?" => Some(false),
            // x86 feature probes: false on Apple silicon, assumed on Intel.
            "Hardware::CPU.avx2?" | "Hardware::CPU.avx?" | "Hardware::CPU.sse4?" => Some(!self.arm),
            "build.head?" | "build.bottle?" => Some(false),
            "build.stable?" => Some(true),
            "true" => Some(true),
            "false" => Some(false),
            _ => {
                if s.starts_with("build.with?") || s.starts_with("build.without?") {
                    return Some(false);
                }
                None
            }
        }
    }

    /// `on_macos`, `on_linux`, `on_arm`, `on_intel`, `on_<release>`.
    pub fn on_block_active(&self, head: &str, args: &str) -> Option<bool> {
        match head {
            "on_macos" => Some(self.macos()),
            "on_linux" => Some(self.linux),
            "on_arm" => Some(self.arm),
            "on_intel" => Some(!self.arm),
            "on_system" => Some(self.on_system(args)),
            _ => {
                let release = head.strip_prefix("on_")?;
                let want = MacOsVersion::major_for_symbol(release)?;
                let have = self.macos_major?;
                let modifier = args.trim().trim_start_matches(':');
                Some(match modifier {
                    "or_newer" => have >= want,
                    "or_older" => have <= want,
                    _ => have == want,
                })
            }
        }
    }

    /// `on_system :linux, macos: :ventura_or_newer`.
    fn on_system(&self, args: &str) -> bool {
        let mut linux_listed = false;
        let mut macos_rule: Option<String> = None;
        for part in args.split(',') {
            let part = part.trim();
            if let Some(rest) = part.strip_prefix("macos:") {
                macos_rule = Some(rest.trim().trim_start_matches(':').to_string());
            } else if part.trim_start_matches(':') == "linux" {
                linux_listed = true;
            }
        }
        if self.linux {
            return linux_listed;
        }
        let Some(rule) = macos_rule else { return false };
        let Some(have) = self.macos_major else {
            return false;
        };
        if let Some(name) = rule.strip_suffix("_or_newer") {
            return MacOsVersion::major_for_symbol(name).is_some_and(|w| have >= w);
        }
        if let Some(name) = rule.strip_suffix("_or_older") {
            return MacOsVersion::major_for_symbol(name).is_some_and(|w| have <= w);
        }
        MacOsVersion::major_for_symbol(&rule).is_some_and(|w| have == w)
    }
}

fn is_macos_version(s: &str) -> bool {
    matches!(
        s,
        "MacOS.version" | "OS::Mac.version" | "MacOS::version" | "OS::Mac::version"
    )
}

fn release_major(s: &str) -> Option<u32> {
    let s = s.trim().trim_matches('"').trim_start_matches(':');
    MacOsVersion::major_for_symbol(s).or_else(|| MacOsVersion::parse(s).map(|v| v.major))
}

fn balanced(s: &str) -> bool {
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// Split on `op` at parenthesis depth zero; `None` when the operator is absent.
fn split_operator(s: &str, op: &str) -> Option<Vec<String>> {
    let bytes = s.as_bytes();
    let mut parts: Vec<String> = vec![];
    let mut depth = 0i32;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            _ => {}
        }
        if depth == 0 && s[i..].starts_with(op) {
            parts.push(s[start..i].to_string());
            i += op.len();
            start = i;
            continue;
        }
        i += 1;
    }
    if parts.is_empty() {
        return None;
    }
    parts.push(s[start..].to_string());
    Some(parts)
}

fn split_once_outside<'a>(s: &'a str, op: &str) -> Option<(&'a str, &'a str)> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            _ => {}
        }
        if depth == 0 && s[i..].starts_with(op) {
            // `>=` must not be seen as `>`; the caller tries the longer ops first.
            return Some((&s[..i], &s[i + op.len()..]));
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tahoe_arm() -> HostCtx {
        HostCtx::from_tag(&BottleTag::parse("arm64_tahoe").unwrap())
    }

    fn linux_x86() -> HostCtx {
        HostCtx::from_tag(&BottleTag::parse("x86_64_linux").unwrap())
    }

    #[test]
    fn evaluates_os_and_cpu() {
        let h = tahoe_arm();
        assert_eq!(h.eval("OS.mac?"), Some(true));
        assert_eq!(h.eval("OS.linux?"), Some(false));
        assert_eq!(h.eval("Hardware::CPU.arm?"), Some(true));
        assert_eq!(h.eval("Hardware::CPU.intel?"), Some(false));
        assert_eq!(h.eval("Hardware::CPU.avx2?"), Some(false));
        assert_eq!(
            h.eval("Hardware::CPU.arm? || Hardware::CPU.in_rosetta2?"),
            Some(true)
        );
        assert_eq!(h.eval("!OS.mac?"), Some(false));
        assert_eq!(h.eval("OS.mac? && Hardware::CPU.intel?"), Some(false));
        assert_eq!(h.eval("(OS.mac?)"), Some(true));
        assert_eq!(h.eval("something_unknown?"), None);

        let l = linux_x86();
        assert_eq!(l.eval("OS.linux?"), Some(true));
        assert_eq!(l.eval("Hardware::CPU.intel?"), Some(true));
    }

    #[test]
    fn compares_macos_versions() {
        let h = tahoe_arm();
        assert_eq!(h.eval("MacOS.version >= :ventura"), Some(true));
        assert_eq!(h.eval("MacOS.version < :ventura"), Some(false));
        assert_eq!(h.eval("MacOS.version == :tahoe"), Some(true));
        assert_eq!(h.eval("OS::Mac.version <= :sequoia"), Some(false));
        assert_eq!(h.eval("Hardware::CPU.type == :arm"), Some(true));
    }

    #[test]
    fn on_blocks() {
        let h = tahoe_arm();
        assert_eq!(h.on_block_active("on_macos", ""), Some(true));
        assert_eq!(h.on_block_active("on_linux", ""), Some(false));
        assert_eq!(h.on_block_active("on_arm", ""), Some(true));
        assert_eq!(h.on_block_active("on_intel", ""), Some(false));
        assert_eq!(h.on_block_active("on_tahoe", ""), Some(true));
        assert_eq!(h.on_block_active("on_ventura", ":or_newer"), Some(true));
        assert_eq!(h.on_block_active("on_ventura", ""), Some(false));
        assert_eq!(
            h.on_block_active("on_system", ":linux, macos: :ventura_or_newer"),
            Some(true)
        );
        assert_eq!(
            linux_x86().on_block_active("on_system", ":linux, macos: :ventura_or_newer"),
            Some(true)
        );
        assert_eq!(
            linux_x86().on_block_active("on_system", "macos: :ventura_or_newer"),
            Some(false)
        );
    }
}
