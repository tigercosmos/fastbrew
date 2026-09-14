//! `pin`/`unpin`: `var/homebrew/pinned/<name>` symlink to the latest keg.

use crate::config::Config;
use crate::error::Result;

pub fn pin(_cfg: &Config, _name: &str) -> Result<()> {
    todo!("ops::pin::pin")
}

pub fn unpin(_cfg: &Config, _name: &str) -> Result<()> {
    todo!("ops::pin::unpin")
}
