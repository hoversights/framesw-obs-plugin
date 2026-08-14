// SPDX-License-Identifier: GPL-2.0-or-later
//
// FrameSW Companion Plugin for OBS Studio
// Copyright (C) 2026 Hoversights
//
// This program is free software; you can redistribute it and/or modify it
// under the terms of the GNU General Public License as published by the
// Free Software Foundation; either version 2 of the License, or (at your
// option) any later version.
//
// This program is distributed in the hope that it will be useful, but
// WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU
// General Public License for more details.
//
// You should have received a copy of the GNU General Public License along
// with this program; if not, see <https://www.gnu.org/licenses/>.

//! Cross-platform "find this libobs/obs-websocket symbol somewhere
//! already loaded in the current process." This is the *only* file with
//! `#[cfg(target_os = ...)]` branches in the whole plugin — every other
//! file (`calldata.rs`, `obs_data.rs`, `lib.rs`) is identical C-ABI logic
//! on both platforms, since libobs's own API doesn't vary by OS.
//!
//! Deliberately **runtime** symbol resolution (`dlsym`/`GetProcAddress`)
//! rather than link-time `extern "C"` declarations against an import
//! library: macOS and Windows have genuinely different models for how a
//! plugin's undefined symbols get satisfied by its host process at link
//! time (macOS: `-Wl,-undefined,dynamic_lookup` defers it to load time
//! for free; Windows: normally requires linking against a `.lib` import
//! library generated from OBS's own SDK, which this project doesn't
//! have/want to depend on). Resolving everything at runtime instead means
//! both platforms share **one** mechanism and this crate needs zero
//! platform-specific linker configuration at all.

use std::ffi::{c_void, CString};

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const std::os::raw::c_char) -> *mut c_void;
        fn dlopen(path: *const std::os::raw::c_char, mode: std::os::raw::c_int) -> *mut c_void;
    }

    /// `RTLD_NOW` — verified the same way as `RTLD_DEFAULT` below: `2` on
    /// macOS (`dlfcn.h`). Resolves every undefined symbol at load time
    /// rather than lazily, so a load that "succeeds" really means the
    /// library is fully usable, not deferred-and-hoping.
    const RTLD_NOW: std::os::raw::c_int = 2;

    /// `RTLD_DEFAULT` — verified directly on this machine (a small C
    /// program printing `dlfcn.h`'s actual macro expansion), not assumed
    /// from memory: `(void*)-2` on macOS. Searches every image currently
    /// loaded in the process, not one specific library — exactly "find
    /// this symbol wherever OBS itself put it" without this plugin
    /// needing to know libobs's exact bundle path, which has genuinely
    /// changed across OBS versions (bare `libobs.dylib` vs.
    /// `libobs.framework/Versions/A/libobs`).
    const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

    pub fn resolve(name: &str) -> *mut c_void {
        let Ok(cname) = CString::new(name) else {
            return std::ptr::null_mut();
        };
        unsafe { dlsym(RTLD_DEFAULT, cname.as_ptr()) }
    }

    /// Loads (or returns the already-loaded handle for) a library that is
    /// *not* already part of the host process — unlike `resolve` above,
    /// which only ever searches what's already loaded. Used for the NDI
    /// runtime, which OBS itself never links.
    pub fn load_library(path: &str) -> *mut c_void {
        let Ok(cpath) = CString::new(path) else {
            return std::ptr::null_mut();
        };
        unsafe { dlopen(cpath.as_ptr(), RTLD_NOW) }
    }

    /// `dlsym` against a specific handle (from `load_library`), not the
    /// process-wide `RTLD_DEFAULT` search `resolve` does.
    pub fn resolve_in(handle: *mut c_void, name: &str) -> *mut c_void {
        if handle.is_null() {
            return std::ptr::null_mut();
        }
        let Ok(cname) = CString::new(name) else {
            return std::ptr::null_mut();
        };
        unsafe { dlsym(handle, cname.as_ptr()) }
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows_sys::Win32::System::ProcessStatus::K32EnumProcessModules;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    /// Loads a library by path (`LoadLibraryW`) — unlike `resolve` below,
    /// which only searches modules already loaded into this process. Used
    /// for the NDI runtime, which OBS itself never links. Takes a UTF-8
    /// `&str` and converts to UTF-16 itself since `LoadLibraryW` is the
    /// wide-string entry point (the narrow `LoadLibraryA` is documented by
    /// Microsoft as not supporting paths above `MAX_PATH` without an
    /// explicit long-path prefix — `W` avoids that entirely).
    pub fn load_library(path: &str) -> *mut c_void {
        let mut wide: Vec<u16> = path.encode_utf16().collect();
        wide.push(0);
        unsafe { LoadLibraryW(wide.as_ptr()) as *mut c_void }
    }

    /// Reads FrameSW's install directory from the key its NSIS installer
    /// writes (`InstallLocation` under the standard Uninstall key). This is
    /// how the plugin finds the NDI runtime FrameSW bundles: FrameSW ships
    /// `Processing.NDI.Lib.x64.dll` next to its own exe because Windows
    /// resolves an exe's static imports before any of its code runs, but
    /// this plugin lives inside `obs64.exe`, whose DLL search path never
    /// includes FrameSW's directory. Without this lookup the bundled copy is
    /// unreachable and Preview monitoring is silent on every machine that
    /// hasn't separately installed the NDI Runtime.
    ///
    /// Returns `None` rather than guessing a path if the key is absent —
    /// a machine with no FrameSW install has nothing useful to point at.
    pub fn framesw_install_dir() -> Option<String> {
        // Must match `WriteRegStr HKLM "${UNINST_KEY}" "InstallLocation"` in
        // the app repo's packaging/windows/installer.nsi.
        const SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\FrameSW";
        let raw = read_hklm_sz(SUBKEY, "InstallLocation")?;
        let trimmed = raw.trim_end_matches('\\');
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// Reads one `REG_SZ` value from under `HKEY_LOCAL_MACHINE`. Split out
    /// from `framesw_install_dir` so the fiddly part - sizing the buffer,
    /// decoding UTF-16, handling a value that is not NUL-terminated on disk
    /// - can be tested against a key that exists on every Windows machine,
    /// rather than only on one that happens to have FrameSW installed.
    pub fn read_hklm_sz(subkey: &str, value: &str) -> Option<String> {
        use windows_sys::Win32::System::Registry::{
            RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
            REG_SZ,
        };

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }

        unsafe {
            let mut key: HKEY = std::ptr::null_mut();
            if RegOpenKeyExW(HKEY_LOCAL_MACHINE, wide(subkey).as_ptr(), 0, KEY_READ, &mut key) != 0
            {
                return None;
            }
            // Ask for the size first: the value is written by the installer
            // and there is no fixed upper bound worth assuming.
            let name = wide(value);
            let mut kind: u32 = 0;
            let mut bytes: u32 = 0;
            let sized = RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null_mut(),
                &mut kind,
                std::ptr::null_mut(),
                &mut bytes,
            );
            if sized != 0 || kind != REG_SZ || bytes == 0 {
                RegCloseKey(key);
                return None;
            }
            let mut buf: Vec<u16> = vec![0; (bytes as usize).div_ceil(2)];
            let read = RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null_mut(),
                &mut kind,
                buf.as_mut_ptr().cast(),
                &mut bytes,
            );
            RegCloseKey(key);
            if read != 0 {
                return None;
            }
            // REG_SZ is not required to be null-terminated on disk; trim at
            // the first NUL if one is present, otherwise take the whole run.
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let s = String::from_utf16_lossy(&buf[..end]);
            let s = s.trim_end_matches('\\');
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        }
    }

    /// `GetProcAddress` against a specific module handle (from
    /// `load_library`), not the process-wide enumeration `resolve` does.
    pub fn resolve_in(handle: *mut c_void, name: &str) -> *mut c_void {
        if handle.is_null() {
            return std::ptr::null_mut();
        }
        let Ok(cname) = CString::new(name) else {
            return std::ptr::null_mut();
        };
        unsafe { GetProcAddress(handle as HMODULE, cname.as_ptr().cast()).map_or(std::ptr::null_mut(), |f| f as *mut c_void) }
    }

    /// No Windows equivalent of `RTLD_DEFAULT` — enumerates every module
    /// currently loaded in this process instead (`K32EnumProcessModules`,
    /// confirmed present directly in `kernel32.dll` on Windows 7+ /
    /// PSAPI_VERSION 2, so this needs no separate `psapi.lib` link) and
    /// tries `GetProcAddress` against each until one has the symbol.
    /// Same effective result as the macOS side: works regardless of
    /// which exact DLL OBS's core library is named in a given version
    /// (expected to be `obs.dll`, but this doesn't hardcode that
    /// assumption).
    pub fn resolve(name: &str) -> *mut c_void {
        let Ok(cname) = CString::new(name) else {
            return std::ptr::null_mut();
        };
        unsafe {
            let process = GetCurrentProcess();
            const MAX_MODULES: usize = 1024;
            let mut modules: [HMODULE; MAX_MODULES] = [std::ptr::null_mut(); MAX_MODULES];
            let mut needed: u32 = 0;
            let ok = K32EnumProcessModules(
                process,
                modules.as_mut_ptr(),
                std::mem::size_of_val(&modules) as u32,
                &mut needed,
            );
            if ok == 0 {
                return std::ptr::null_mut();
            }
            let count =
                (needed as usize / std::mem::size_of::<HMODULE>()).min(MAX_MODULES);
            for &module in &modules[..count] {
                if module.is_null() {
                    continue;
                }
                if let Some(addr) = GetProcAddress(module, cname.as_ptr().cast()) {
                    return addr as *mut c_void;
                }
            }
            std::ptr::null_mut()
        }
    }
}

/// Raw symbol lookup — prefer `resolved_fn!` below over calling this
/// directly, which also handles caching and casting to a concrete
/// function pointer type.
pub fn resolve(name: &str) -> *mut c_void {
    imp::resolve(name)
}

/// Resolves `name` and casts it to the given function pointer type — the
/// one unsafe transmute this whole plugin needs, isolated to a single
/// call site rather than repeated everywhere. `None` if the symbol isn't
/// found anywhere in the current process (wrong/ancient libobs version,
/// or a genuinely optional symbol).
///
/// # Safety
/// Caller must get `F` exactly right — this has no way to verify the
/// resolved address actually matches `F`'s real signature.
pub unsafe fn resolve_as<F: Copy>(name: &str) -> Option<F> {
    let ptr = resolve(name);
    if ptr.is_null() {
        return None;
    }
    debug_assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*mut c_void>());
    Some(std::mem::transmute_copy(&ptr))
}

/// Loads a library that is *not* already part of the host process — used
/// only by `ndi_ffi.rs` for the NDI runtime, which OBS itself never
/// links (unlike libobs/obs-websocket, which `resolve`/`resolve_as`
/// above assume are already loaded). Null if the path doesn't exist or
/// isn't a loadable library on this platform.
pub fn load_library(path: &str) -> *mut c_void {
    imp::load_library(path)
}

/// FrameSW's install directory, if this machine has one — the directory
/// holding the NDI runtime FrameSW bundles. Windows reads it from the
/// installer's registry key; macOS has no equivalent registry, so
/// `ndi_ffi.rs` probes the standard `.app` locations directly instead.
#[cfg(target_os = "windows")]
pub fn framesw_install_dir() -> Option<String> {
    imp::framesw_install_dir()
}

/// Resolves `name` against a specific library handle (from
/// `load_library`) and casts it to the given function pointer type — the
/// `ndi_ffi.rs` counterpart to `resolve_as`, which only ever searches
/// symbols already loaded somewhere in the process.
///
/// # Safety
/// Caller must get `F` exactly right — this has no way to verify the
/// resolved address actually matches `F`'s real signature.
pub unsafe fn resolve_in_as<F: Copy>(handle: *mut c_void, name: &str) -> Option<F> {
    let ptr = imp::resolve_in(handle, name);
    if ptr.is_null() {
        return None;
    }
    debug_assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*mut c_void>());
    Some(std::mem::transmute_copy(&ptr))
}

/// Declares a lazily-resolved, cached-after-first-lookup libobs/
/// obs-websocket function. Expands to a `fn NAME() -> Option<TYPE>` —
/// every call site handles `None` explicitly (symbol genuinely missing)
/// rather than panicking across the FFI boundary into OBS's own process,
/// matching this plugin's "degrade gracefully, never crash the host"
/// rule throughout.
#[macro_export]
macro_rules! resolved_fn {
    ($name:ident : $ty:ty) => {
        // `pub` because these resolvers now live in the core crate while
        // some call sites are in the consuming cdylib. Visibility is not a
        // safety boundary here: every resolver returns `Option` and each
        // call site must handle `None` regardless of who calls it.
        #[allow(non_snake_case)]
        pub fn $name() -> Option<$ty> {
            static CELL: std::sync::OnceLock<Option<$ty>> = std::sync::OnceLock::new();
            *CELL.get_or_init(|| unsafe { $crate::platform::resolve_as(stringify!($name)) })
        }
    };
}
