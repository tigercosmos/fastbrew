//! Ad-hoc re-signing of modified Mach-O files (`extend/os/mac/keg.rb`).
//!
//! `codesign --sign - --force --preserve-metadata=entitlements,requirements,flags,runtime <file>`;
//! on failure copy the file to a new inode and retry once. Run in parallel
//! across files with `rayon`, making each file writable first if needed.

use std::path::Path;

use crate::error::Result;

pub fn codesign_files(_files: &[&Path]) -> Result<()> {
    todo!("bottle::codesign::codesign_files")
}
