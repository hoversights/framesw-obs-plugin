//! `calldata_t`/`proc_handler_t` FFI, and the obs-websocket vendor API
//! built on top of them — ported line-for-line from real, verbatim source
//! (fetched via `curl`, not routed through any summarizing/paraphrasing
//! step, given how easily a subtly-wrong reimplementation of a binary RPC
//! format could corrupt memory inside OBS's own process):
//! - `obsproject/obs-studio@master`: `libobs/callback/calldata.h`,
//!   `libobs/callback/proc.h`.
//! - `obsproject/obs-websocket@master`: `lib/obs-websocket-api.h`.
//!
//! Key finding that made this pure-Rust FFI viable instead of needing a
//! vendored C shim: `calldata_get_data`/`calldata_set_data` (the actual
//! binary stack-encoding logic) are real `EXPORT`ed libobs symbols, not
//! `static inline` — only the trivial one-line typed wrappers around them
//! (`calldata_set_ptr`, `calldata_ptr`, `calldata_bool`, ...) are inline,
//! and those are simple enough to port by hand with confidence. Nothing
//! in this file guesses at calldata's internal layout beyond what's
//! quoted directly from the real header below.
//!
//! Every real libobs function here is resolved at *runtime*
//! (`crate::resolved_fn!`, see `platform.rs`) rather than declared as a
//! link-time `extern "C"` import — the one mechanism that works
//! identically on macOS and Windows, see `platform.rs`'s module doc.

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicPtr, Ordering};

/// Opaque — `struct proc_handler;` is forward-declared only in
/// `callback/proc.h`; libobs never exposes its layout.
pub enum ProcHandlerT {}

/// `libobs/callback/calldata.h`:
/// ```c
/// struct calldata {
///     uint8_t *stack;
///     size_t size;     /* size of the stack, in bytes */
///     size_t capacity; /* capacity of the stack, in bytes */
///     bool fixed;      /* fixed size (using call stack) */
/// };
/// ```
/// Field order/types verbatim — this is a real (non-opaque) struct we
/// construct and pass by pointer, so getting the ABI exactly right
/// matters.
#[repr(C)]
pub struct CallData {
    pub stack: *mut u8,
    pub size: usize,
    pub capacity: usize,
    pub fixed: bool,
}

impl CallData {
    /// `calldata_init`: `memset(data, 0, sizeof(struct calldata))`.
    pub fn zeroed() -> Self {
        CallData {
            stack: std::ptr::null_mut(),
            size: 0,
            capacity: 0,
            fixed: false,
        }
    }
}

impl Drop for CallData {
    /// `calldata_free`:
    /// ```c
    /// static inline void calldata_free(struct calldata *data) {
    ///     if (!data->fixed) bfree(data->stack);
    /// }
    /// ```
    fn drop(&mut self) {
        if !self.fixed && !self.stack.is_null() {
            if let Some(bfree) = bfree() {
                bfree(self.stack.cast());
            }
        }
    }
}

// Resolved at runtime (`platform::resolve_as`), not linked at build time —
// see `platform.rs`'s module doc for why. Each of these is real EXPORTs
// from libobs (`libobs/callback/calldata.h`, `libobs/callback/proc.h`,
// `libobs/util/bmem.h`, `libobs/obs.h`), not `static inline`.
crate::resolved_fn!(calldata_get_data: extern "C" fn(*const CallData, *const c_char, *mut c_void, usize) -> bool);
crate::resolved_fn!(calldata_set_data: extern "C" fn(*mut CallData, *const c_char, *const c_void, usize));
crate::resolved_fn!(proc_handler_call: extern "C" fn(*mut ProcHandlerT, *const c_char, *mut CallData) -> bool);
crate::resolved_fn!(bfree: extern "C" fn(*mut c_void));
crate::resolved_fn!(obs_get_proc_handler: extern "C" fn() -> *mut ProcHandlerT);

fn cstr(s: &str) -> CString {
    // Truncates at the first embedded NUL rather than failing outright —
    // every caller here passes a short, controlled field name or a
    // plugin-generated string, never arbitrary external input, so this
    // is a non-issue in practice; still handled rather than `.unwrap()`ed
    // since this runs inside OBS's process, where a panic-driven abort is
    // far worse than a silently-truncated log line.
    CString::new(s).unwrap_or_else(|e| {
        let valid_len = e.nul_position();
        CString::new(&e.into_vec()[..valid_len]).unwrap_or_default()
    })
}

/// `calldata_set_ptr`: `calldata_set_data(data, name, &ptr, sizeof(ptr))`.
fn calldata_set_ptr(data: &mut CallData, name: &str, ptr: *mut c_void) {
    let Some(calldata_set_data) = calldata_set_data() else {
        return;
    };
    let name = cstr(name);
    calldata_set_data(
        data,
        name.as_ptr(),
        (&ptr as *const *mut c_void).cast(),
        std::mem::size_of::<*mut c_void>(),
    );
}

/// `calldata_set_string`: `calldata_set_data(data, name, str, strlen(str)+1)`.
fn calldata_set_string(data: &mut CallData, name: &str, value: &str) {
    let Some(calldata_set_data) = calldata_set_data() else {
        return;
    };
    let name = cstr(name);
    let value = cstr(value);
    let bytes = value.as_bytes_with_nul();
    calldata_set_data(data, name.as_ptr(), bytes.as_ptr().cast(), bytes.len());
}

/// `calldata_ptr`: reads via `calldata_get_ptr`/`calldata_get_data`,
/// defaulting to NULL if absent.
fn calldata_ptr(data: &CallData, name: &str) -> *mut c_void {
    let mut out: *mut c_void = std::ptr::null_mut();
    let Some(calldata_get_data) = calldata_get_data() else {
        return out;
    };
    let name = cstr(name);
    calldata_get_data(
        data,
        name.as_ptr(),
        (&mut out as *mut *mut c_void).cast(),
        std::mem::size_of::<*mut c_void>(),
    );
    out
}

/// `calldata_bool`: same shape, defaulting to `false` if absent.
fn calldata_bool(data: &CallData, name: &str) -> bool {
    let mut out = false;
    let Some(calldata_get_data) = calldata_get_data() else {
        return out;
    };
    let name = cstr(name);
    calldata_get_data(
        data,
        name.as_ptr(),
        (&mut out as *mut bool).cast(),
        std::mem::size_of::<bool>(),
    );
    out
}

// ---------------------------------------------------------------------
// obs-websocket-api.h's vendor API, ported line-for-line.
// ---------------------------------------------------------------------

/// Mirrors `obs-websocket-api.h`'s `static proc_handler_t *_ph;` module-
/// local cache — fetched once, reused for every subsequent vendor call.
static PH: AtomicPtr<ProcHandlerT> = AtomicPtr::new(std::ptr::null_mut());

/// `obs_websocket_get_ph`:
/// ```c
/// static inline proc_handler_t *obs_websocket_get_ph(void) {
///     proc_handler_t *global_ph = obs_get_proc_handler();
///     assert(global_ph != NULL);
///     calldata_t cd = {0, 0, 0, 0};
///     if (!proc_handler_call(global_ph, "obs_websocket_api_get_ph", &cd))
///         blog(LOG_DEBUG, "Unable to fetch obs-websocket proc handler object. obs-websocket not installed?");
///     proc_handler_t *ret = (proc_handler_t *)calldata_ptr(&cd, "ph");
///     calldata_free(&cd);
///     return ret;
/// }
/// ```
fn obs_websocket_get_ph() -> *mut ProcHandlerT {
    let Some(obs_get_proc_handler) = obs_get_proc_handler() else {
        return std::ptr::null_mut();
    };
    let Some(proc_handler_call) = proc_handler_call() else {
        return std::ptr::null_mut();
    };
    let global_ph = obs_get_proc_handler();
    if global_ph.is_null() {
        // The real header `assert()`s here — asserting inside a plugin
        // loaded into someone else's long-running process is worse than
        // just failing gracefully, so this treats it as "obs-websocket
        // API unavailable" instead of aborting OBS.
        return std::ptr::null_mut();
    }
    let mut cd = CallData::zeroed();
    let name = cstr("obs_websocket_api_get_ph");
    proc_handler_call(global_ph, name.as_ptr(), &mut cd);
    calldata_ptr(&cd, "ph").cast()
}

/// `obs_websocket_ensure_ph`.
fn obs_websocket_ensure_ph() -> *mut ProcHandlerT {
    let cached = PH.load(Ordering::Acquire);
    if !cached.is_null() {
        return cached;
    }
    let fetched = obs_websocket_get_ph();
    if !fetched.is_null() {
        PH.store(fetched, Ordering::Release);
    }
    fetched
}

/// `obs_websocket_vendor_run_simple_proc`.
fn vendor_run_simple_proc(vendor: *mut c_void, proc_name: &str, cd: &mut CallData) -> bool {
    let ph = obs_websocket_ensure_ph();
    let Some(proc_handler_call) = proc_handler_call() else {
        return false;
    };
    if ph.is_null() || vendor.is_null() || proc_name.is_empty() {
        return false;
    }
    calldata_set_ptr(cd, "vendor", vendor);
    let name = cstr(proc_name);
    proc_handler_call(ph, name.as_ptr(), cd);
    calldata_bool(cd, "success")
}

/// `obs_websocket_register_vendor` — **only call from `obs_module_post_load`**,
/// same requirement the real header documents: this is the point OBS
/// guarantees every module (including obs-websocket itself, if installed)
/// has finished `obs_module_load`, so there's no load-order race. Returns
/// null if obs-websocket isn't installed/loaded — always handled as a
/// normal, expected outcome throughout this plugin, never an error.
pub fn register_vendor(name: &str) -> *mut c_void {
    let ph = obs_websocket_ensure_ph();
    let Some(proc_handler_call) = proc_handler_call() else {
        return std::ptr::null_mut();
    };
    if ph.is_null() {
        return std::ptr::null_mut();
    }
    let mut cd = CallData::zeroed();
    calldata_set_string(&mut cd, "name", name);
    let proc_name = cstr("vendor_register");
    proc_handler_call(ph, proc_name.as_ptr(), &mut cd);
    calldata_ptr(&cd, "vendor")
}

/// `obs_websocket_vendor_emit_event`. `event_data` is an `obs_data_t*`
/// (opaque to this module — see `obs_data.rs`); ownership/refcount is
/// unaffected by this call, matching the real API's own documented
/// behavior ("Does not affect event_data refcount").
pub fn vendor_emit_event(vendor: *mut c_void, event_name: &str, event_data: *mut c_void) -> bool {
    let mut cd = CallData::zeroed();
    calldata_set_string(&mut cd, "type", event_name);
    calldata_set_ptr(&mut cd, "data", event_data);
    vendor_run_simple_proc(vendor, "vendor_event_emit", &mut cd)
}

#[allow(dead_code)]
pub fn cstr_lossy(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
}
