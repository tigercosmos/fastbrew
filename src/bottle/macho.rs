//! Minimal Mach-O load-command editor.
//!
//! `rewrite_install_names(path, |old| -> Option<new>)` rewrites the names in
//! `LC_ID_DYLIB`, `LC_LOAD_DYLIB`, `LC_LOAD_WEAK_DYLIB`, `LC_REEXPORT_DYLIB`,
//! `LC_LOAD_UPWARD_DYLIB` and the paths in `LC_RPATH` of every architecture in
//! a thin or fat file. Command sizes stay 8-byte aligned. When a longer name
//! does not fit in the header pad (space between the end of the load commands
//! and the first section's file offset), return [`MachOError::NoHeaderPad`] so
//! the caller can fall back to `install_name_tool`. Returns whether the file
//! was modified. Also exposes [`dylib_id`], [`linked_libraries`], [`rpaths`]
//! and [`is_macho`] for `fix_dynamic_linkage`-style checks.
//!
//! Ported from `ruby-macho`'s `MachOFile#replace_command` / `#low_fileoff`
//! (`vendor/bundle/.../ruby-macho-6.0.0/lib/macho/macho_file.rb`), which
//! Homebrew drives from `extend/os/mac/keg_relocate.rb`. Like ruby-macho, an
//! edit never changes the file's size: the load-command region is rebuilt in
//! place and the slack between its end and the first section's data is NUL
//! padding, so every file offset recorded elsewhere stays valid.

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

// Mach-O magics.
const MH_MAGIC: u32 = 0xfeed_face;
const MH_CIGAM: u32 = 0xcefa_edfe;
const MH_MAGIC_64: u32 = 0xfeed_facf;
const MH_CIGAM_64: u32 = 0xcffa_edfe;
const FAT_MAGIC: u32 = 0xcafe_babe;
const FAT_MAGIC_64: u32 = 0xcafe_babf;

// Mach-O file types we relocate (`Keg#mach_o_files`).
const MH_EXECUTE: u32 = 0x2;
const MH_DYLIB: u32 = 0x6;
const MH_BUNDLE: u32 = 0x8;

const LC_REQ_DYLD: u32 = 0x8000_0000;
const LC_SEGMENT: u32 = 0x1;
const LC_SEGMENT_64: u32 = 0x19;
const LC_LOAD_DYLIB: u32 = 0xc;
const LC_ID_DYLIB: u32 = 0xd;
const LC_LOAD_WEAK_DYLIB: u32 = 0x18 | LC_REQ_DYLD;
const LC_RPATH: u32 = 0x1c | LC_REQ_DYLD;
const LC_REEXPORT_DYLIB: u32 = 0x1f | LC_REQ_DYLD;
const LC_LOAD_UPWARD_DYLIB: u32 = 0x23 | LC_REQ_DYLD;

const S_ZEROFILL: u32 = 0x1;
const S_THREAD_LOCAL_ZEROFILL: u32 = 0x12;

const MACH_HEADER_64_SIZE: usize = 32;
const MACH_HEADER_SIZE: usize = 28;
/// Load commands of a 64-bit Mach-O are padded to this many bytes.
const LC_ALIGNMENT: usize = 8;

#[derive(Debug)]
pub enum MachOError {
    /// The rewritten load commands no longer fit before the first section:
    /// fall back to `install_name_tool`, which relays the whole file out.
    NoHeaderPad { path: PathBuf },
    /// The file is not a Mach-O we can edit, or its headers are inconsistent.
    Parse { path: PathBuf, reason: String },
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for MachOError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MachOError::NoHeaderPad { path } => write!(
                f,
                "the header pad of {} is too small for the new load commands",
                path.display()
            ),
            MachOError::Parse { path, reason } => {
                write!(f, "could not parse {}: {reason}", path.display())
            }
            MachOError::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for MachOError {}

impl From<MachOError> for crate::error::Error {
    fn from(e: MachOError) -> Self {
        crate::error::Error::Other(anyhow::Error::new(e))
    }
}

pub type Result<T> = std::result::Result<T, MachOError>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MachOInfo {
    pub dylib_id: Option<String>,
    pub linked_libraries: Vec<String>,
    pub rpaths: Vec<String>,
}

/// Whether `path` is a Mach-O executable, dylib or bundle, thin or fat
/// (`Keg#mach_o_files`: `dylib?`, `mach_o_bundle?` or `mach_o_executable?`).
pub fn is_macho(path: &Path) -> bool {
    let Ok(len) = std::fs::metadata(path).map(|m| m.len() as usize) else {
        return false;
    };
    let Ok(head) = read_head(path, 4096.min(len)) else {
        return false;
    };
    match slices(&head, len) {
        Ok(slices) => slices.iter().any(|s| {
            // A fat header only carries offsets; read the slice's own header.
            match slice_filetype(path, &head, s) {
                Some(ft) => matches!(ft, MH_EXECUTE | MH_DYLIB | MH_BUNDLE),
                None => false,
            }
        }),
        Err(_) => false,
    }
}

/// Read the `LC_ID_DYLIB` name, the linked dylib names and the rpaths of every
/// slice, in file order and without duplicates.
pub fn read_info(path: &Path) -> Result<MachOInfo> {
    let data = read_file(path)?;
    let mut info = MachOInfo::default();
    let len = data.len();
    for slice in slices(&data, len).map_err(|reason| MachOError::Parse {
        path: path.to_path_buf(),
        reason,
    })? {
        let Some(buf) = slice.bytes(&data) else {
            continue;
        };
        let Ok(hdr) = MachHeader::parse(buf) else {
            continue;
        };
        for cmd in hdr.commands(buf) {
            let Some(s) = cmd.lc_string(buf) else {
                continue;
            };
            match cmd.cmd {
                LC_ID_DYLIB if info.dylib_id.is_none() => info.dylib_id = Some(s),
                LC_LOAD_DYLIB | LC_LOAD_WEAK_DYLIB | LC_REEXPORT_DYLIB | LC_LOAD_UPWARD_DYLIB
                    if !info.linked_libraries.contains(&s) =>
                {
                    info.linked_libraries.push(s)
                }
                LC_RPATH if !info.rpaths.contains(&s) => info.rpaths.push(s),
                _ => {}
            }
        }
    }
    Ok(info)
}

pub fn dylib_id(path: &Path) -> Result<Option<String>> {
    Ok(read_info(path)?.dylib_id)
}

pub fn linked_libraries(path: &Path) -> Result<Vec<String>> {
    Ok(read_info(path)?.linked_libraries)
}

pub fn rpaths(path: &Path) -> Result<Vec<String>> {
    Ok(read_info(path)?.rpaths)
}

/// Rewrite dylib IDs, install names and rpaths through `map`, which returns the
/// replacement for a name or `None` to leave it alone. Returns whether anything
/// changed. 32-bit slices of a fat file are left untouched rather than
/// corrupted.
pub fn rewrite_install_names(path: &Path, map: &dyn Fn(&str) -> Option<String>) -> Result<bool> {
    let mut data = read_file(path)?;
    let slices = slices(&data, data.len()).map_err(|reason| MachOError::Parse {
        path: path.to_path_buf(),
        reason,
    })?;

    // Plan every slice before touching the file, so a slice that needs
    // `install_name_tool` leaves the whole file untouched for the fallback.
    let mut edits: Vec<(usize, Vec<u8>)> = Vec::new();
    for slice in &slices {
        let Some(buf) = slice.bytes(&data) else {
            continue;
        };
        let Ok(hdr) = MachHeader::parse(buf) else {
            continue;
        };
        if !hdr.is64 {
            continue;
        }
        if let Some(region) = plan_slice(path, buf, &hdr, map)? {
            edits.push((slice.offset, region));
        }
    }
    if edits.is_empty() {
        return Ok(false);
    }
    for (offset, region) in edits {
        let end = offset + region.len();
        data[offset..end].copy_from_slice(&region);
    }
    write_file(path, &data)?;
    Ok(true)
}

/// The rewritten header-plus-load-command region of one slice, or `None` when
/// nothing in it changed. The returned bytes replace the slice's first
/// `header + sizeofcmds(old)` bytes, so the slice's size never changes.
fn plan_slice(
    path: &Path,
    buf: &[u8],
    hdr: &MachHeader,
    map: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Vec<u8>>> {
    let header_size = hdr.header_size();
    let commands = hdr.commands(buf);
    // Never rewrite a file whose load commands we could not walk in full: a
    // dropped command would be silently deleted from the rebuilt region.
    if header_size + hdr.sizeofcmds > buf.len() || commands.len() != hdr.ncmds {
        return Err(MachOError::Parse {
            path: path.to_path_buf(),
            reason: format!(
                "truncated load commands ({} of {} readable)",
                commands.len(),
                hdr.ncmds
            ),
        });
    }
    let mut region: Vec<u8> = Vec::with_capacity(hdr.sizeofcmds);
    let mut modified = false;

    for cmd in commands {
        let is_name_command = matches!(
            cmd.cmd,
            LC_ID_DYLIB
                | LC_LOAD_DYLIB
                | LC_LOAD_WEAK_DYLIB
                | LC_REEXPORT_DYLIB
                | LC_LOAD_UPWARD_DYLIB
                | LC_RPATH
        );
        let replacement = if is_name_command {
            cmd.lc_string(buf)
                .and_then(|old| map(&old).filter(|new| *new != old).map(|new| (old, new)))
        } else {
            None
        };
        match replacement {
            None => region.extend_from_slice(&buf[cmd.offset..cmd.offset + cmd.cmdsize]),
            Some((_old, new)) => {
                modified = true;
                // Keep every fixed field (including the newer `LC_LOAD_DYLIB`
                // variant's trailing `flags`) by copying the bytes before the
                // string and only re-packing the string payload.
                let str_off = cmd.str_off.expect("name command has a string offset");
                let mut out = buf[cmd.offset..cmd.offset + str_off].to_vec();
                out.extend_from_slice(new.as_bytes());
                out.push(0);
                while !out.len().is_multiple_of(LC_ALIGNMENT) {
                    out.push(0);
                }
                let size = u32::try_from(out.len()).map_err(|_| MachOError::Parse {
                    path: path.to_path_buf(),
                    reason: "load command too large".into(),
                })?;
                hdr.write_u32(&mut out[4..8], size);
                region.extend_from_slice(&out);
            }
        }
    }

    if !modified {
        return Ok(None);
    }

    let low_fileoff = hdr.low_fileoff(buf);
    if header_size + region.len() > low_fileoff {
        return Err(MachOError::NoHeaderPad {
            path: path.to_path_buf(),
        });
    }

    // Header, then the new commands, then NUL padding up to where the old
    // commands ended so nothing after them moves.
    let old_end = header_size + hdr.sizeofcmds;
    let new_end = header_size + region.len();
    let mut out = buf[..header_size].to_vec();
    out.extend_from_slice(&region);
    if new_end < old_end {
        out.resize(old_end, 0);
    }
    let sizeofcmds = u32::try_from(region.len()).map_err(|_| MachOError::Parse {
        path: path.to_path_buf(),
        reason: "load commands too large".into(),
    })?;
    hdr.write_u32(&mut out[20..24], sizeofcmds);
    Ok(Some(out))
}

// ---------------------------------------------------------------- parsing

#[derive(Debug, Clone, Copy)]
struct Slice {
    offset: usize,
    size: usize,
}

impl Slice {
    fn bytes<'a>(&self, data: &'a [u8]) -> Option<&'a [u8]> {
        data.get(self.offset..self.offset.checked_add(self.size)?)
    }
}

/// Architecture slices of a thin or fat file, in file order. `data` may hold
/// only the head of the file; `total_len` is the file's real length, against
/// which the fat architecture table is validated.
fn slices(data: &[u8], total_len: usize) -> std::result::Result<Vec<Slice>, String> {
    if data.len() < 8 {
        return Err("file is too short for a Mach-O header".into());
    }
    let be = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    if be == FAT_MAGIC || be == FAT_MAGIC_64 {
        let is64 = be == FAT_MAGIC_64;
        let nfat = u32::from_be_bytes([data[4], data[5], data[6], data[7]]) as usize;
        // Java class files share `0xcafebabe`; their "nfat_arch" is a minor
        // version number, so an implausible count means this is not a fat file.
        if nfat == 0 || nfat > 64 {
            return Err("not a fat Mach-O (implausible architecture count)".into());
        }
        let entry = if is64 { 32 } else { 20 };
        let mut out = Vec::with_capacity(nfat);
        for i in 0..nfat {
            let base = 8 + i * entry;
            let end = base + entry;
            if end > data.len() {
                return Err("truncated fat architecture table".into());
            }
            let (offset, size) = if is64 {
                (
                    u64::from_be_bytes(data[base + 8..base + 16].try_into().unwrap()) as usize,
                    u64::from_be_bytes(data[base + 16..base + 24].try_into().unwrap()) as usize,
                )
            } else {
                (
                    u32::from_be_bytes(data[base + 8..base + 12].try_into().unwrap()) as usize,
                    u32::from_be_bytes(data[base + 12..base + 16].try_into().unwrap()) as usize,
                )
            };
            if offset.checked_add(size).is_none_or(|e| e > total_len) {
                return Err("fat architecture outside the file".into());
            }
            out.push(Slice { offset, size });
        }
        return Ok(out);
    }
    let le = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    if matches!(le, MH_MAGIC | MH_CIGAM | MH_MAGIC_64 | MH_CIGAM_64) {
        return Ok(vec![Slice {
            offset: 0,
            size: total_len,
        }]);
    }
    Err("not a Mach-O file".into())
}

/// `filetype` of one slice, reading only its header.
fn slice_filetype(path: &Path, head: &[u8], slice: &Slice) -> Option<u32> {
    if let Some(buf) = slice.bytes(head)
        && let Ok(h) = MachHeader::parse(buf)
    {
        return Some(h.filetype);
    }
    // The slice starts beyond the bytes we read: pull in just its header.
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(slice.offset as u64)).ok()?;
    let mut hdr = [0u8; MACH_HEADER_64_SIZE];
    std::io::Read::read_exact(&mut file, &mut hdr).ok()?;
    MachHeader::parse(&hdr).ok().map(|h| h.filetype)
}

#[derive(Debug)]
struct MachHeader {
    is64: bool,
    big_endian: bool,
    filetype: u32,
    ncmds: usize,
    sizeofcmds: usize,
}

#[derive(Debug, Clone, Copy)]
struct LoadCommand {
    cmd: u32,
    offset: usize,
    cmdsize: usize,
    /// Offset of the `lc_str` payload inside the command, for name commands.
    str_off: Option<usize>,
}

impl MachHeader {
    fn parse(buf: &[u8]) -> std::result::Result<MachHeader, ()> {
        if buf.len() < MACH_HEADER_SIZE {
            return Err(());
        }
        let raw = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let (is64, big_endian) = match raw {
            MH_MAGIC_64 => (true, false),
            MH_CIGAM_64 => (true, true),
            MH_MAGIC => (false, false),
            MH_CIGAM => (false, true),
            _ => return Err(()),
        };
        let read = |o: usize| -> u32 {
            let b: [u8; 4] = buf[o..o + 4].try_into().unwrap();
            if big_endian {
                u32::from_be_bytes(b)
            } else {
                u32::from_le_bytes(b)
            }
        };
        let hdr = MachHeader {
            is64,
            big_endian,
            filetype: read(12),
            ncmds: read(16) as usize,
            sizeofcmds: read(20) as usize,
        };
        Ok(hdr)
    }

    fn header_size(&self) -> usize {
        if self.is64 {
            MACH_HEADER_64_SIZE
        } else {
            MACH_HEADER_SIZE
        }
    }

    fn u32_at(&self, buf: &[u8], o: usize) -> Option<u32> {
        let b: [u8; 4] = buf.get(o..o + 4)?.try_into().ok()?;
        Some(if self.big_endian {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    }

    fn u64_at(&self, buf: &[u8], o: usize) -> Option<u64> {
        let b: [u8; 8] = buf.get(o..o + 8)?.try_into().ok()?;
        Some(if self.big_endian {
            u64::from_be_bytes(b)
        } else {
            u64::from_le_bytes(b)
        })
    }

    fn write_u32(&self, out: &mut [u8], v: u32) {
        let b = if self.big_endian {
            v.to_be_bytes()
        } else {
            v.to_le_bytes()
        };
        out.copy_from_slice(&b);
    }

    fn commands(&self, buf: &[u8]) -> Vec<LoadCommand> {
        let mut out = Vec::with_capacity(self.ncmds);
        let mut offset = self.header_size();
        // A caller may hold only the head of the file (`is_macho`), so clamp.
        let end = (offset + self.sizeofcmds).min(buf.len());
        for _ in 0..self.ncmds {
            if offset + 8 > end {
                break;
            }
            let Some(cmd) = self.u32_at(buf, offset) else {
                break;
            };
            let Some(cmdsize) = self.u32_at(buf, offset + 4).map(|s| s as usize) else {
                break;
            };
            if cmdsize < 8 || offset + cmdsize > end {
                break;
            }
            let str_off = match cmd {
                LC_ID_DYLIB | LC_LOAD_DYLIB | LC_LOAD_WEAK_DYLIB | LC_REEXPORT_DYLIB
                | LC_LOAD_UPWARD_DYLIB | LC_RPATH => self
                    .u32_at(buf, offset + 8)
                    .map(|v| v as usize)
                    .filter(|v| *v >= 12 && *v < cmdsize),
                _ => None,
            };
            out.push(LoadCommand {
                cmd,
                offset,
                cmdsize,
                str_off,
            });
            offset += cmdsize;
        }
        out
    }

    /// Offset of the first section's data, i.e. the end of the header pad
    /// (`MachOFile#low_fileoff`).
    fn low_fileoff(&self, buf: &[u8]) -> usize {
        let mut offset = buf.len();
        for cmd in self.commands(buf) {
            let wide = match cmd.cmd {
                LC_SEGMENT_64 => true,
                LC_SEGMENT => false,
                _ => continue,
            };
            let (fileoff, filesize, nsects_off, sect_size) = if wide {
                (
                    self.u64_at(buf, cmd.offset + 40).unwrap_or(0) as usize,
                    self.u64_at(buf, cmd.offset + 48).unwrap_or(0) as usize,
                    cmd.offset + 64,
                    80usize,
                )
            } else {
                (
                    self.u32_at(buf, cmd.offset + 24).unwrap_or(0) as usize,
                    self.u32_at(buf, cmd.offset + 28).unwrap_or(0) as usize,
                    cmd.offset + 48,
                    68usize,
                )
            };
            let nsects = self.u32_at(buf, nsects_off).unwrap_or(0) as usize;
            if nsects == 0 && fileoff > 0 && filesize > 0 && fileoff < offset {
                offset = fileoff;
            }
            let first_sect = nsects_off + 8;
            for i in 0..nsects {
                let s = first_sect + i * sect_size;
                if s + sect_size > cmd.offset + cmd.cmdsize {
                    break;
                }
                let (size, sect_off, flags) = if wide {
                    (
                        self.u64_at(buf, s + 40).unwrap_or(0),
                        self.u32_at(buf, s + 48).unwrap_or(0) as usize,
                        self.u32_at(buf, s + 64).unwrap_or(0),
                    )
                } else {
                    (
                        self.u32_at(buf, s + 36).unwrap_or(0) as u64,
                        self.u32_at(buf, s + 40).unwrap_or(0) as usize,
                        self.u32_at(buf, s + 56).unwrap_or(0),
                    )
                };
                if size == 0 {
                    continue;
                }
                let ty = flags & 0xff;
                if ty == S_ZEROFILL || ty == S_THREAD_LOCAL_ZEROFILL {
                    continue;
                }
                if sect_off < offset {
                    offset = sect_off;
                }
            }
        }
        offset
    }
}

impl LoadCommand {
    /// The NUL-terminated `lc_str` payload of a name command.
    fn lc_string(&self, buf: &[u8]) -> Option<String> {
        let start = self.offset + self.str_off?;
        let end = self.offset + self.cmdsize;
        let bytes = buf.get(start..end)?;
        let len = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        std::str::from_utf8(&bytes[..len]).ok().map(str::to_string)
    }
}

// ---------------------------------------------------------------- file I/O

fn read_file(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|source| MachOError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn read_head(path: &Path, max: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|source| MachOError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut buf = vec![0u8; max];
    let mut filled = 0;
    loop {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => {
                filled += n;
                if filled == max {
                    break;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(source) => {
                return Err(MachOError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// Rewrite the file in place, keeping its inode (and so its hard links),
/// ownership and mode, as `ruby-macho`'s `MachOFile#write!` does.
fn write_file(path: &Path, data: &[u8]) -> Result<()> {
    let io = |source| MachOError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(io)?;
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    file.write_all(data).map_err(io)?;
    file.set_len(data.len() as u64).map_err(io)?;
    file.flush().map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_macho() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("text.txt");
        std::fs::write(&p, b"#!/bin/sh\necho hi\n").unwrap();
        assert!(!is_macho(&p));
        // A Java class file also starts with 0xcafebabe.
        let j = tmp.path().join("A.class");
        std::fs::write(&j, [0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 65, 0, 0]).unwrap();
        assert!(!is_macho(&j));
    }

    #[test]
    fn parses_host_binary() {
        let p = Path::new("/bin/ls");
        if !p.exists() {
            return;
        }
        assert!(is_macho(p));
        let info = read_info(p).unwrap();
        assert!(
            info.linked_libraries
                .iter()
                .any(|l| l.contains("libSystem")),
            "{:?}",
            info.linked_libraries
        );
        assert_eq!(info.dylib_id, None);
    }

    /// Build `libgreet.dylib` and `greeter` in `dir` with a large header pad,
    /// returning their paths, or `None` when no toolchain is available.
    fn build_fixtures(dir: &Path) -> Option<(PathBuf, PathBuf)> {
        let clang = Path::new("/usr/bin/clang");
        if !clang.exists() {
            return None;
        }
        std::fs::write(dir.join("greet.c"), "int greet(void) { return 42; }\n").unwrap();
        std::fs::write(
            dir.join("main.c"),
            "int greet(void);\n#include <stdio.h>\nint main(void){printf(\"%d\\n\", greet());return 0;}\n",
        )
        .unwrap();
        let dylib = dir.join("libgreet.dylib");
        let ok = std::process::Command::new(clang)
            .args(["-dynamiclib", "-o"])
            .arg(&dylib)
            .arg(dir.join("greet.c"))
            .args([
                "-install_name",
                "@@HOMEBREW_PREFIX@@/opt/greet/lib/libgreet.dylib",
                "-Wl,-headerpad_max_install_names",
            ])
            // `ld` warns about `@@...` looking like a response file.
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?
            .success();
        if !ok {
            return None;
        }
        let exe = dir.join("greeter");
        let ok = std::process::Command::new(clang)
            .arg("-o")
            .arg(&exe)
            .arg(dir.join("main.c"))
            .arg(&dylib)
            .args([
                "-Wl,-headerpad_max_install_names",
                "-Wl,-rpath,@@HOMEBREW_PREFIX@@/lib",
            ])
            // `ld` warns about `@@...` looking like a response file.
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?
            .success();
        if !ok {
            return None;
        }
        Some((dylib, exe))
    }

    fn otool_l(path: &Path) -> String {
        let out = std::process::Command::new("/usr/bin/otool")
            .arg("-l")
            .arg(path)
            .output()
            .expect("otool");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn otool_big_l(path: &Path) -> String {
        let out = std::process::Command::new("/usr/bin/otool")
            .arg("-L")
            .arg(path)
            .output()
            .expect("otool -L");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn codesign_ok(path: &Path) -> bool {
        std::process::Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn rewrites_longer_and_shorter_names() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((dylib, exe)) = build_fixtures(tmp.path()) else {
            eprintln!("skipping: no /usr/bin/clang");
            return;
        };
        assert!(is_macho(&dylib) && is_macho(&exe));

        let info = read_info(&dylib).unwrap();
        assert_eq!(
            info.dylib_id.as_deref(),
            Some("@@HOMEBREW_PREFIX@@/opt/greet/lib/libgreet.dylib")
        );
        let exe_info = read_info(&exe).unwrap();
        assert!(
            exe_info
                .linked_libraries
                .iter()
                .any(|l| l == "@@HOMEBREW_PREFIX@@/opt/greet/lib/libgreet.dylib"),
            "{:?}",
            exe_info.linked_libraries
        );
        assert!(
            exe_info
                .rpaths
                .contains(&"@@HOMEBREW_PREFIX@@/lib".to_string()),
            "{:?}",
            exe_info.rpaths
        );

        // A much longer replacement than the placeholder: needs the header pad.
        let long_prefix = "/Users/someone/very/long/sandbox/prefix/for/fastbrew/tests";
        let expand = |s: &str| -> Option<String> {
            s.strip_prefix("@@HOMEBREW_PREFIX@@")
                .map(|rest| format!("{long_prefix}{rest}"))
        };
        assert!(rewrite_install_names(&dylib, &expand).unwrap());
        assert!(rewrite_install_names(&exe, &expand).unwrap());

        let want_id = format!("{long_prefix}/opt/greet/lib/libgreet.dylib");
        assert_eq!(
            read_info(&dylib).unwrap().dylib_id.as_deref(),
            Some(&*want_id)
        );
        assert!(otool_l(&dylib).contains(&want_id), "{}", otool_l(&dylib));
        assert!(otool_big_l(&exe).contains(&want_id));
        assert!(
            read_info(&exe)
                .unwrap()
                .rpaths
                .contains(&format!("{long_prefix}/lib")),
            "rpath not rewritten"
        );
        // `otool` must still be able to walk the file, i.e. ncmds/sizeofcmds
        // and every command size stayed consistent.
        assert!(otool_l(&exe).contains("LC_RPATH"), "{}", otool_l(&exe));

        // Now shrink the names again and check the file stays valid.
        let shrink = move |s: &str| -> Option<String> {
            s.strip_prefix(long_prefix).map(|rest| format!("/o{rest}"))
        };
        assert!(rewrite_install_names(&dylib, &shrink).unwrap());
        assert!(rewrite_install_names(&exe, &shrink).unwrap());
        assert_eq!(
            read_info(&dylib).unwrap().dylib_id.as_deref(),
            Some("/o/opt/greet/lib/libgreet.dylib")
        );
        assert!(otool_l(&exe).contains("/o/opt/greet/lib/libgreet.dylib"));
        // Nothing to do the third time.
        assert!(!rewrite_install_names(&dylib, &shrink).unwrap());
    }

    #[test]
    fn rewritten_binary_signs_and_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((dylib, exe)) = build_fixtures(tmp.path()) else {
            eprintln!("skipping: no /usr/bin/clang");
            return;
        };
        let dir = tmp.path().to_path_buf();
        let map = move |s: &str| -> Option<String> {
            s.strip_prefix("@@HOMEBREW_PREFIX@@/opt/greet/lib")
                .map(|rest| format!("{}{rest}", dir.display()))
        };
        assert!(rewrite_install_names(&dylib, &map).unwrap());
        assert!(rewrite_install_names(&exe, &map).unwrap());
        crate::bottle::codesign::codesign_files(&[&dylib, &exe]).unwrap();
        assert!(codesign_ok(&dylib), "dylib signature invalid");
        assert!(codesign_ok(&exe), "executable signature invalid");

        let out = std::process::Command::new(&exe).output().expect("run");
        assert!(out.status.success(), "{:?}", out);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "42");
    }

    #[test]
    fn reports_no_header_pad() {
        let tmp = tempfile::tempdir().unwrap();
        let clang = Path::new("/usr/bin/clang");
        if !clang.exists() {
            eprintln!("skipping: no /usr/bin/clang");
            return;
        }
        std::fs::write(tmp.path().join("g.c"), "int g(void){return 1;}\n").unwrap();
        let dylib = tmp.path().join("libg.dylib");
        // `-headerpad 0` leaves no room to grow the load commands.
        let ok = std::process::Command::new(clang)
            .args(["-dynamiclib", "-o"])
            .arg(&dylib)
            .arg(tmp.path().join("g.c"))
            .args(["-install_name", "/a/libg.dylib", "-Wl,-headerpad,0"])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let huge = format!("/{}/libg.dylib", "x".repeat(4096));
        let err = rewrite_install_names(&dylib, &|s: &str| {
            (s == "/a/libg.dylib").then(|| huge.clone())
        })
        .unwrap_err();
        assert!(
            matches!(err, MachOError::NoHeaderPad { .. }),
            "expected NoHeaderPad, got {err:?}"
        );
        // The file must be untouched so the caller can fall back.
        assert_eq!(
            read_info(&dylib).unwrap().dylib_id.as_deref(),
            Some("/a/libg.dylib")
        );
    }

    #[test]
    fn padding_rounds_to_eight() {
        // The command layout invariant the editor relies on: a dylib command is
        // 24 fixed bytes plus the NUL-terminated name padded to 8 bytes.
        for (len, want) in [(1usize, 32usize), (7, 32), (8, 40), (15, 40), (16, 48)] {
            let unpadded = 24 + len + 1;
            let padded = unpadded.div_ceil(LC_ALIGNMENT) * LC_ALIGNMENT;
            assert_eq!(padded, want, "name length {len}");
        }
    }
}
