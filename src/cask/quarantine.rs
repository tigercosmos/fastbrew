//! `com.apple.quarantine` (port of `Library/Homebrew/cask/quarantine.rb` and
//! `extend/os/mac/cask/quarantine.rb`).
//!
//! A container fetched with `curl` carries no quarantine attribute, so
//! `Quarantine.cask!` registers the download with LaunchServices as a web
//! download before anything is staged; macOS composes the attribute value
//! (`<flags>;<hex time>;<agent>;<event uuid>`) and writes it. `propagate` then
//! copies that value, with the "no translocation" bit set, onto every staged
//! file so Gatekeeper still evaluates the app and it runs from the Caskroom
//! instead of a randomized read-only mount.

use std::path::Path;

use crate::error::{Error, Result};

/// `kLSQuarantineAgentNameKey` Homebrew registers downloads under.
pub const AGENT_NAME: &str = "Homebrew Cask";

/// `Quarantine::QUARANTINE_ATTRIBUTE`.
pub const QUARANTINE_ATTRIBUTE: &str = "com.apple.quarantine";

/// `Quarantine::USER_APPROVED_FLAG` (`QuarantineSPI.h`).
pub const USER_APPROVED_FLAG: u64 = 0x0040;

/// Bit 8 of the flags field disables app translocation.
pub const NO_TRANSLOCATION_FLAG: u64 = 0x0100;

/// `Quarantine.status`: the raw attribute value, or `None` when absent.
pub fn status(path: &Path) -> Option<String> {
    let raw = xattr::get(path, QUARANTINE_ATTRIBUTE).ok().flatten()?;
    let text = String::from_utf8_lossy(&raw).trim_end().to_string();
    (!text.is_empty()).then_some(text)
}

/// `Quarantine.detect`: whether the path carries a quarantine attribute.
pub fn detect(path: &Path) -> bool {
    status(path).is_some()
}

/// `Quarantine.toggle_no_translocation_bit`: OR bit 8 into the flags field.
///
/// The attribute is `<flags>;<hex time>;<agent>;<uuid>`; only the first field
/// changes, and it keeps at least four hex digits.
pub fn toggle_no_translocation_bit(attribute: &str) -> String {
    let mut fields: Vec<String> = attribute.split(';').map(str::to_string).collect();
    if fields.is_empty() {
        return attribute.to_string();
    }
    let flags = u64::from_str_radix(fields[0].trim(), 16).unwrap_or(0);
    fields[0] = format!("{:0>4x}", flags | NO_TRANSLOCATION_FLAG);
    fields.join(";")
}

/// `Quarantine.user_approved_status?`: whether the flags carry the approval bit.
pub fn user_approved_status(attribute: &str) -> bool {
    if attribute.is_empty() {
        return false;
    }
    let flags = attribute
        .split(';')
        .next()
        .and_then(|f| u64::from_str_radix(f.trim(), 16).ok())
        .unwrap_or(0);
    flags & USER_APPROVED_FLAG != 0
}

/// `Quarantine.cask!`: register `download_path` with LaunchServices as a web
/// download of `url` from `homepage`, which is what makes macOS write the
/// `com.apple.quarantine` attribute a browser download would carry.
///
/// A download that already has the attribute is left alone, exactly as
/// `return if detect(download_path)` does.
pub fn cask(download_path: &Path, url: &str, homepage: &str) -> Result<()> {
    if detect(download_path) {
        return Ok(());
    }
    // `CaskQuarantineError`.
    lsquarantine::mark_web_download(download_path, AGENT_NAME, url, homepage).map_err(|reason| {
        Error::user(format!(
            "Failed to quarantine {}. Here's the reason:\n{reason}",
            download_path.display()
        ))
    })
}

/// `Quarantine.propagate`: copy `from`'s quarantine attribute (with the
/// no-translocation bit set) onto everything under `to`, making each path
/// writable first (`chmod -h u+w`).
///
/// Homebrew raises `CaskQuarantinePropagationError` when `xattr` fails, so a
/// staged file that cannot be quarantined fails the install rather than
/// producing an app Gatekeeper will never evaluate.
pub fn propagate(from: &Path, to: &Path) -> Result<()> {
    let Some(attribute) = status(from) else {
        return Ok(());
    };
    let attribute = toggle_no_translocation_bit(&attribute);
    let mut failures: Vec<String> = Vec::new();
    for path in staged_paths(to) {
        make_user_writable(&path);
        if let Err(error) = xattr::set(&path, QUARANTINE_ATTRIBUTE, attribute.as_bytes()) {
            failures.push(format!("{}: {error}", path.display()));
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    Err(Error::user(format!(
        "Failed to quarantine one or more files within {}. Here's the reason:\n{}",
        to.display(),
        failures.join("\n")
    )))
}

/// The LaunchServices side of `OS::Mac::Cask::Quarantine.cask!`.
///
/// Homebrew sets `kCFURLQuarantinePropertiesKey` on the downloaded file with
/// the web-download quarantine type, the agent name and the two URLs; macOS
/// assigns the event UUID and timestamp and writes `com.apple.quarantine`
/// itself, so the value is identical to the one a browser download carries and
/// the download also appears in the LaunchServices quarantine event database.
mod lsquarantine {
    use std::ffi::c_void;
    use std::path::Path;

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFURLRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type CFIndex = isize;
    type CFStringEncoding = u32;
    type Boolean = u8;

    const UTF8: CFStringEncoding = 0x0800_0100;
    /// `kCFURLPOSIXPathStyle`.
    const POSIX_PATH_STYLE: CFIndex = 0;

    #[repr(C)]
    struct CFDictionaryKeyCallBacks {
        _version: CFIndex,
        _retain: *const c_void,
        _release: *const c_void,
        _copy_description: *const c_void,
        _equal: *const c_void,
        _hash: *const c_void,
    }

    #[repr(C)]
    struct CFDictionaryValueCallBacks {
        _version: CFIndex,
        _retain: *const c_void,
        _release: *const c_void,
        _copy_description: *const c_void,
        _equal: *const c_void,
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFTypeDictionaryKeyCallBacks: CFDictionaryKeyCallBacks;
        static kCFTypeDictionaryValueCallBacks: CFDictionaryValueCallBacks;
        static kCFURLQuarantinePropertiesKey: CFStringRef;

        fn CFStringCreateWithBytes(
            alloc: CFAllocatorRef,
            bytes: *const u8,
            num_bytes: CFIndex,
            encoding: CFStringEncoding,
            is_external_representation: Boolean,
        ) -> CFStringRef;
        fn CFURLCreateWithFileSystemPath(
            alloc: CFAllocatorRef,
            file_path: CFStringRef,
            path_style: CFIndex,
            is_directory: Boolean,
        ) -> CFURLRef;
        fn CFDictionaryCreate(
            alloc: CFAllocatorRef,
            keys: *const CFTypeRef,
            values: *const CFTypeRef,
            num_values: CFIndex,
            key_call_backs: *const CFDictionaryKeyCallBacks,
            value_call_backs: *const CFDictionaryValueCallBacks,
        ) -> CFDictionaryRef;
        fn CFURLSetResourcePropertyForKey(
            url: CFURLRef,
            key: CFStringRef,
            value: CFTypeRef,
            error: *mut CFTypeRef,
        ) -> Boolean;
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        static kLSQuarantineAgentNameKey: CFStringRef;
        static kLSQuarantineTypeKey: CFStringRef;
        static kLSQuarantineTypeWebDownload: CFStringRef;
        static kLSQuarantineDataURLKey: CFStringRef;
        static kLSQuarantineOriginURLKey: CFStringRef;
    }

    /// Every Core Foundation object this call owns, released together.
    #[derive(Default)]
    struct Pool(Vec<CFTypeRef>);

    impl Pool {
        fn track(&mut self, object: CFTypeRef) -> Result<CFTypeRef, String> {
            if object.is_null() {
                return Err("Core Foundation returned a null reference".to_string());
            }
            self.0.push(object);
            Ok(object)
        }

        /// A `CFString` for `value`, owned by the pool.
        fn string(&mut self, value: &str) -> Result<CFTypeRef, String> {
            // SAFETY: `value` is a valid slice for the length passed, and the
            // returned reference is owned by this pool.
            let string = unsafe {
                CFStringCreateWithBytes(
                    std::ptr::null(),
                    value.as_ptr(),
                    value.len() as CFIndex,
                    UTF8,
                    0,
                )
            };
            self.track(string)
        }
    }

    impl Drop for Pool {
        fn drop(&mut self) {
            for object in self.0.drain(..) {
                // SAFETY: every tracked reference was created by this module
                // with a create/copy function, so we own one retain each.
                unsafe { CFRelease(object) };
            }
        }
    }

    /// Register `path` as a web download of `data_url` from `origin_url`.
    pub fn mark_web_download(
        path: &Path,
        agent: &str,
        data_url: &str,
        origin_url: &str,
    ) -> Result<(), String> {
        let mut pool = Pool::default();
        let path_string = pool.string(&path.to_string_lossy())?;
        // SAFETY: `path_string` is a live CFString from the pool.
        let url = unsafe {
            CFURLCreateWithFileSystemPath(std::ptr::null(), path_string, POSIX_PATH_STYLE, 0)
        };
        let url = pool.track(url)?;

        let agent = pool.string(agent)?;
        let data_url = pool.string(data_url)?;
        let origin_url = pool.string(origin_url)?;
        // SAFETY: reading immutable statics the frameworks export.
        let (type_key, web_download, agent_key, data_key, origin_key, quarantine_key) = unsafe {
            (
                kLSQuarantineTypeKey,
                kLSQuarantineTypeWebDownload,
                kLSQuarantineAgentNameKey,
                kLSQuarantineDataURLKey,
                kLSQuarantineOriginURLKey,
                kCFURLQuarantinePropertiesKey,
            )
        };
        let keys: [CFTypeRef; 4] = [agent_key, type_key, data_key, origin_key];
        let values: [CFTypeRef; 4] = [agent, web_download, data_url, origin_url];
        // SAFETY: `keys` and `values` are four live CFStrings each, and the
        // callback structs are the frameworks' own.
        let properties = unsafe {
            CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                keys.len() as CFIndex,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            )
        };
        let properties = pool.track(properties)?;

        let mut error: CFTypeRef = std::ptr::null();
        // SAFETY: `url`, `quarantine_key` and `properties` are live.
        let ok = unsafe {
            CFURLSetResourcePropertyForKey(url, quarantine_key, properties, &raw mut error)
        };
        if !error.is_null() {
            // SAFETY: `CFURLSetResourcePropertyForKey` hands over a retain.
            unsafe { CFRelease(error) };
        }
        if ok == 0 {
            return Err("could not set the quarantine properties on the download".to_string());
        }
        Ok(())
    }
}

/// `Pathname.glob(to/"**/*", File::FNM_DOTMATCH).reject(&:symlink?)`: every
/// descendant of `to` (not `to` itself), symlinks excluded.
fn staged_paths(to: &Path) -> Vec<std::path::PathBuf> {
    walkdir::WalkDir::new(to)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter(|e| !e.path_is_symlink())
        .map(|e| e.into_path())
        .collect()
}

/// `chmod -h u+w`, ignoring failures on paths we do not own.
fn make_user_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_symlink() {
        return;
    }
    let mode = metadata.permissions().mode();
    if mode & 0o200 != 0 {
        return;
    }
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o200));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sets_no_translocation_bit() {
        assert_eq!(
            toggle_no_translocation_bit("0081;68b8c1a2;Safari;1B2C3D4E"),
            "0181;68b8c1a2;Safari;1B2C3D4E"
        );
        // Already set: unchanged, but normalized to four digits.
        assert_eq!(
            toggle_no_translocation_bit("181;68b8c1a2;Safari;X"),
            "0181;68b8c1a2;Safari;X"
        );
        // Wide flags keep every digit.
        assert_eq!(toggle_no_translocation_bit("00020081;a;b;c"), "20181;a;b;c");
        assert_eq!(toggle_no_translocation_bit("0083"), "0183");
    }

    #[test]
    fn quarantines_a_download_the_way_a_browser_does() {
        let tmp = tempfile::tempdir().unwrap();
        let download = tmp.path().join("Demo.zip");
        std::fs::write(&download, b"x").unwrap();
        if xattr::set(&download, "user.fastbrew-probe", b"1").is_err() {
            return; // The filesystem does not support extended attributes.
        }
        let _ = xattr::remove(&download, "user.fastbrew-probe");

        cask(
            &download,
            "https://example.invalid/Demo.zip",
            "https://example.invalid/",
        )
        .unwrap();
        let value = status(&download).expect("the download is quarantined");
        // `<flags>;<hex time>;<agent>;<event uuid>`, composed by macOS.
        let fields: Vec<&str> = value.split(';').collect();
        assert_eq!(fields.len(), 4, "{value}");
        let flags = u64::from_str_radix(fields[0], 16).unwrap_or_else(|e| panic!("{value}: {e}"));
        assert!(flags != 0, "{value}");
        assert!(!user_approved_status(&value), "{value}");
        assert!(u64::from_str_radix(fields[1], 16).is_ok(), "{value}");
        assert_eq!(fields[3].len(), 36, "the event id is a uuid: {value}");

        // `return if detect(download_path)`: a download macOS already recorded
        // keeps the attribute it has, so a retry never rewrites the event.
        cask(
            &download,
            "https://example.invalid/Other.zip",
            "https://example.invalid/",
        )
        .unwrap();
        assert_eq!(status(&download).as_deref(), Some(value.as_str()));

        // The staged copy inherits it with the no-translocation bit set.
        let staged = tmp.path().join("staged");
        std::fs::create_dir_all(staged.join("Demo.app")).unwrap();
        std::fs::write(staged.join("Demo.app/run"), b"y").unwrap();
        propagate(&download, &staged).unwrap();
        assert_eq!(
            status(&staged.join("Demo.app/run")).as_deref(),
            Some(toggle_no_translocation_bit(&value).as_str())
        );
    }

    #[test]
    fn reads_user_approved_flag() {
        assert!(user_approved_status("0043;68b8c1a2;Homebrew;X"));
        assert!(!user_approved_status("0083;68b8c1a2;Homebrew;X"));
        assert!(!user_approved_status(""));
    }

    #[test]
    fn propagates_between_files() {
        let tmp = tempfile::tempdir().unwrap();
        let download = tmp.path().join("Demo.zip");
        std::fs::write(&download, b"x").unwrap();
        let staged = tmp.path().join("staged");
        std::fs::create_dir_all(staged.join("Demo.app/Contents")).unwrap();
        std::fs::write(staged.join("Demo.app/Contents/Info.plist"), b"y").unwrap();

        // No attribute on the download: nothing happens and it is not an error.
        propagate(&download, &staged).unwrap();
        assert!(status(&staged.join("Demo.app")).is_none());

        if xattr::set(&download, QUARANTINE_ATTRIBUTE, b"0083;68b8c1a2;Safari;ABC").is_err() {
            return; // The filesystem does not support extended attributes.
        }
        propagate(&download, &staged).unwrap();
        assert_eq!(
            status(&staged.join("Demo.app/Contents/Info.plist")).as_deref(),
            Some("0183;68b8c1a2;Safari;ABC")
        );
        // The destination root itself is not touched, matching the Ruby glob.
        assert!(status(&staged).is_none());
    }
}
