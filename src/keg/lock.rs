//! Formula and cask locks: `var/homebrew/locks/<name>.formula.lock` with
//! `flock(LOCK_EX | LOCK_NB)`. On contention fail with
//! `Error: <name> is already locked by another process`.

use std::fs::File;

use crate::config::Config;
use crate::error::Result;

pub struct Lock {
    _file: File,
}

pub fn lock_formula(_cfg: &Config, _name: &str) -> Result<Lock> {
    todo!("keg::lock::lock_formula")
}

pub fn lock_cask(_cfg: &Config, _token: &str) -> Result<Lock> {
    todo!("keg::lock::lock_cask")
}
