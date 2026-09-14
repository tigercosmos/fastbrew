//! Minimal Mach-O load-command editor.
//!
//! `rewrite_install_names(path, |old| -> Option<new>)` rewrites the names in
//! `LC_ID_DYLIB`, `LC_LOAD_DYLIB`, `LC_LOAD_WEAK_DYLIB`, `LC_REEXPORT_DYLIB`,
//! `LC_LOAD_UPWARD_DYLIB` and the paths in `LC_RPATH` of every architecture in
//! a thin or fat file. Command sizes stay 8-byte aligned. When a longer name
//! does not fit in the header pad (space between the end of the load commands
//! and the first section's file offset), return `Err(NoHeaderPad)` so the
//! caller can fall back to `install_name_tool`. Returns whether the file was
//! modified. Also exposes `dylib_id`, `linked_libraries`, `rpaths` and
//! `is_macho` for `fix_dynamic_linkage`-style checks.

use std::path::Path;

use crate::error::Result;

#[derive(Debug, Clone, Default)]
pub struct MachOInfo {
    pub dylib_id: Option<String>,
    pub linked_libraries: Vec<String>,
    pub rpaths: Vec<String>,
}

pub fn is_macho(_path: &Path) -> bool {
    todo!("bottle::macho::is_macho")
}

pub fn read_info(_path: &Path) -> Result<MachOInfo> {
    todo!("bottle::macho::read_info")
}

pub fn rewrite_install_names(_path: &Path, _map: &dyn Fn(&str) -> Option<String>) -> Result<bool> {
    todo!("bottle::macho::rewrite_install_names")
}
