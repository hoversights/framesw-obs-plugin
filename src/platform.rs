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
    }

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
}

#[cfg(target_os = "windows")]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
    use windows_sys::Win32::System::ProcessStatus::K32EnumProcessModules;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

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

/// Declares a lazily-resolved, cached-after-first-lookup libobs/
/// obs-websocket function. Expands to a `fn NAME() -> Option<TYPE>` —
/// every call site handles `None` explicitly (symbol genuinely missing)
/// rather than panicking across the FFI boundary into OBS's own process,
/// matching this plugin's "degrade gracefully, never crash the host"
/// rule throughout.
#[macro_export]
macro_rules! resolved_fn {
    ($name:ident : $ty:ty) => {
        #[allow(non_snake_case)]
        fn $name() -> Option<$ty> {
            static CELL: std::sync::OnceLock<Option<$ty>> = std::sync::OnceLock::new();
            *CELL.get_or_init(|| unsafe { $crate::platform::resolve_as(stringify!($name)) })
        }
    };
}
