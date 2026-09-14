//! Error type shared across the crate.
//!
//! User-facing failures carry Homebrew's wording so scripts and users see
//! familiar messages. Use [`Error::user`] for those and `anyhow` context for
//! unexpected internal failures.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// A message meant for the user, printed as `Error: <msg>` and exiting 1.
    User(String),
    /// A formula or cask name that does not exist; carries suggestions.
    Unavailable {
        name: String,
        kind: PackageKind,
        suggestions: Vec<String>,
        /// `FormulaUnavailableError#dependent`: the formula that pulled this
        /// name in, when it was reached as a dependency.
        dependent: Option<String>,
    },
    /// The command must be handled by the Ruby `brew` (see `delegate`).
    NeedsDelegation { reason: String },
    /// Any other failure.
    Other(anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    Formula,
    Cask,
}

impl Error {
    pub fn user(msg: impl Into<String>) -> Self {
        Error::User(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::User(m) => write!(f, "{m}"),
            Error::Unavailable {
                name,
                kind,
                suggestions,
                dependent,
            } => {
                let what = match kind {
                    PackageKind::Formula => "formula",
                    PackageKind::Cask => "cask",
                };
                // `FormulaUnavailableError#dependent_s`.
                let of = match dependent {
                    Some(d) if d != name => format!(" (dependency of {d})"),
                    _ => String::new(),
                };
                write!(f, "No available {what} with the name \"{name}\"{of}.")?;
                if !suggestions.is_empty() {
                    // `Utils::Text.to_sentence(..., conjunction: "or")`.
                    write!(
                        f,
                        " Did you mean {}?",
                        crate::resolve::to_sentence(suggestions, "or")
                    )?;
                }
                Ok(())
            }
            Error::NeedsDelegation { reason } => write!(f, "{reason}"),
            Error::Other(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Other(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Other(e.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
