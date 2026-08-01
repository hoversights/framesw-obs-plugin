//! Minimal `obs_data_t`/`obs_data_array_t` FFI — just enough to build the
//! `audio_levels` event payload. Both types are fully opaque from this
//! plugin's side (we only ever hold pointers and call real `EXPORT`ed
//! libobs functions on them, never touch their layout), so there's no ABI
//! risk here the way there was for `calldata_t`. Signatures verified
//! against `obsproject/obs-studio@master`'s `libobs/obs-data.h` via
//! `curl`, same as `calldata.rs`. Resolved at runtime, not linked at
//! build time — see `platform.rs`'s module doc for why.

use std::ffi::{c_char, c_void, CString};

pub enum ObsDataT {}
pub enum ObsDataArrayT {}

crate::resolved_fn!(obs_data_create: extern "C" fn() -> *mut ObsDataT);
crate::resolved_fn!(obs_data_release: extern "C" fn(*mut ObsDataT));
crate::resolved_fn!(obs_data_set_string: extern "C" fn(*mut ObsDataT, *const c_char, *const c_char));
crate::resolved_fn!(obs_data_set_double: extern "C" fn(*mut ObsDataT, *const c_char, f64));
crate::resolved_fn!(obs_data_set_bool: extern "C" fn(*mut ObsDataT, *const c_char, bool));
crate::resolved_fn!(obs_data_set_array: extern "C" fn(*mut ObsDataT, *const c_char, *mut ObsDataArrayT));
crate::resolved_fn!(obs_data_array_create: extern "C" fn() -> *mut ObsDataArrayT);
crate::resolved_fn!(obs_data_array_release: extern "C" fn(*mut ObsDataArrayT));
crate::resolved_fn!(obs_data_array_push_back: extern "C" fn(*mut ObsDataArrayT, *mut ObsDataT) -> usize);
// `libobs/obs-data.h`: "Gets string, defaults to "" if it does not exist
// or is not a string."  Signature: `const char *obs_data_get_string(obs_data_t *data, const char *name);`
crate::resolved_fn!(obs_data_get_string: extern "C" fn(*mut ObsDataT, *const c_char) -> *const c_char);

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_else(|e| {
        let valid_len = e.nul_position();
        CString::new(&e.into_vec()[..valid_len]).unwrap_or_default()
    })
}

/// One source's level, as reported to FrameSW. `active` carries exactly
/// the information `InputVolumeMeters` can't: whether OBS considers this
/// source active — always `false` for genuinely Preview-only content,
/// which is the entire reason this plugin exists.
pub struct SourceLevel {
    pub name: String,
    pub peak_db: f32,
    pub active: bool,
}

/// Builds `{"levels": [{"name": ..., "peak_db": ..., "active": ...}, ...]}`
/// as a real `obs_data_t*`, ready to hand to
/// `calldata::vendor_emit_event`. Caller owns the returned pointer and
/// must release it via `release` once the emit call returns (per
/// `obs_websocket_vendor_emit_event`'s own doc comment: it does not touch
/// `event_data`'s refcount itself). Returns null if any required
/// `obs_data_*` symbol couldn't be resolved — these are extremely core,
/// universally-present libobs functions, so that would mean something is
/// fundamentally wrong (not the expected/normal degraded-gracefully case
/// the way a missing obs-websocket is), and building a silently-partial
/// payload would be worse than just not emitting one at all.
pub fn build_levels_payload(levels: &[SourceLevel]) -> *mut ObsDataT {
    let (
        Some(obs_data_create),
        Some(obs_data_release),
        Some(obs_data_set_string),
        Some(obs_data_set_double),
        Some(obs_data_set_bool),
        Some(obs_data_set_array),
        Some(obs_data_array_create),
        Some(obs_data_array_release),
        Some(obs_data_array_push_back),
    ) = (
        self::obs_data_create(),
        self::obs_data_release(),
        self::obs_data_set_string(),
        self::obs_data_set_double(),
        self::obs_data_set_bool(),
        self::obs_data_set_array(),
        self::obs_data_array_create(),
        self::obs_data_array_release(),
        self::obs_data_array_push_back(),
    )
    else {
        return std::ptr::null_mut();
    };

    let root = obs_data_create();
    let array = obs_data_array_create();
    for level in levels {
        let entry = obs_data_create();
        let name_key = cstr("name");
        let name_val = cstr(&level.name);
        obs_data_set_string(entry, name_key.as_ptr(), name_val.as_ptr());
        let peak_key = cstr("peak_db");
        obs_data_set_double(entry, peak_key.as_ptr(), level.peak_db as f64);
        let active_key = cstr("active");
        obs_data_set_bool(entry, active_key.as_ptr(), level.active);
        // `obs_data_array_push_back` addrefs its own copy internally
        // (standard OBS refcounting convention for every "add/set" call
        // in this API) — release our local ref immediately afterward
        // rather than leak it.
        obs_data_array_push_back(array, entry);
        obs_data_release(entry);
    }
    let levels_key = cstr("levels");
    obs_data_set_array(root, levels_key.as_ptr(), array);
    obs_data_array_release(array);
    root
}

// `libobs/obs-data.h`: `bool obs_data_get_bool(obs_data_t *data, const
// char *name)` and `bool obs_data_has_user_value(obs_data_t *data, const
// char *name)`. The presence check is required, not optional polish:
// `obs_data_get_bool` returns `false` for a key that isn't there at all,
// which would make "read the current value" indistinguishable from "set
// it to false".
crate::resolved_fn!(obs_data_get_bool: extern "C" fn(*mut ObsDataT, *const c_char) -> bool);
crate::resolved_fn!(obs_data_has_user_value: extern "C" fn(*mut ObsDataT, *const c_char) -> bool);

/// `Some(value)` only when the key is genuinely present; `None` for
/// absent, so callers can distinguish "not supplied" from `false`.
pub fn get_optional_bool(data: *mut ObsDataT, key: &str) -> Option<bool> {
    let (Some(obs_data_get_bool), Some(obs_data_has_user_value)) =
        (self::obs_data_get_bool(), self::obs_data_has_user_value())
    else {
        return None;
    };
    let key = cstr(key);
    if !obs_data_has_user_value(data, key.as_ptr()) {
        return None;
    }
    Some(obs_data_get_bool(data, key.as_ptr()))
}

/// One `{name, kind}` pair for `set_pair_array`.
pub struct NamedKind {
    pub name: String,
    pub kind: String,
}

/// Attaches `key: [{"name": ..., "kind": ...}, ...]` to an existing
/// `obs_data_t*` (a vendor request's `response_data`, unlike
/// `build_levels_payload`, which creates its own root). Returns false if
/// any required `obs_data_*` symbol is unresolvable, so the caller can
/// report a real error instead of silently answering with an empty list —
/// "no active outputs" and "couldn't look" must stay distinguishable.
pub fn set_pair_array(data: *mut ObsDataT, key: &str, items: &[NamedKind]) -> bool {
    let (
        Some(obs_data_create),
        Some(obs_data_release),
        Some(obs_data_set_string),
        Some(obs_data_set_array),
        Some(obs_data_array_create),
        Some(obs_data_array_release),
        Some(obs_data_array_push_back),
    ) = (
        self::obs_data_create(),
        self::obs_data_release(),
        self::obs_data_set_string(),
        self::obs_data_set_array(),
        self::obs_data_array_create(),
        self::obs_data_array_release(),
        self::obs_data_array_push_back(),
    )
    else {
        return false;
    };

    let array = obs_data_array_create();
    for item in items {
        let entry = obs_data_create();
        let name_key = cstr("name");
        let name_val = cstr(&item.name);
        obs_data_set_string(entry, name_key.as_ptr(), name_val.as_ptr());
        let kind_key = cstr("kind");
        let kind_val = cstr(&item.kind);
        obs_data_set_string(entry, kind_key.as_ptr(), kind_val.as_ptr());
        // Same refcount convention as `build_levels_payload`: push_back
        // addrefs internally, so drop our own ref right after.
        obs_data_array_push_back(array, entry);
        obs_data_release(entry);
    }
    let array_key = cstr(key);
    obs_data_set_array(data, array_key.as_ptr(), array);
    obs_data_array_release(array);
    true
}

pub fn release(data: *mut ObsDataT) {
    if data.is_null() {
        return;
    }
    if let Some(obs_data_release) = obs_data_release() {
        obs_data_release(data);
    }
}

// Re-exported for `lib.rs` to pass through `calldata::vendor_emit_event`,
// which takes an opaque `*mut c_void` so `calldata.rs` doesn't need to
// know about `obs_data.rs`'s types.
pub fn as_void(data: *mut ObsDataT) -> *mut c_void {
    data.cast()
}

/// Reverse of `as_void` — for `lib.rs`'s request-callback trampoline,
/// which receives the `request_data`/`response_data` pair from
/// `calldata.rs`'s `RequestCallbackFn` as opaque `*mut c_void` (that
/// module doesn't know about `ObsDataT`) and needs them back as typed
/// pointers to call anything in this module.
pub fn from_void(ptr: *mut c_void) -> *mut ObsDataT {
    ptr.cast()
}

/// `obs_data_get_string`, defaulting to `""` (never null on a successful
/// call — that's the real function's own documented default) if the key
/// is absent or `data` is null; `None` only if the `obs_data_get_string`
/// symbol itself couldn't be resolved.
pub fn get_string(data: *mut ObsDataT, key: &str) -> Option<String> {
    let obs_data_get_string = self::obs_data_get_string()?;
    if data.is_null() {
        return Some(String::new());
    }
    let key = cstr(key);
    let ptr = obs_data_get_string(data, key.as_ptr());
    if ptr.is_null() {
        return Some(String::new());
    }
    Some(unsafe { std::ffi::CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
}

/// `obs_data_set_string`, silently a no-op if `data` is null or the
/// symbol can't be resolved — used only for building this plugin's own
/// response payloads, where "couldn't set a field" degrades to "the
/// field is just absent," never a crash.
pub fn set_string(data: *mut ObsDataT, key: &str, value: &str) {
    let Some(obs_data_set_string) = self::obs_data_set_string() else {
        return;
    };
    if data.is_null() {
        return;
    }
    let key = cstr(key);
    let value = cstr(value);
    obs_data_set_string(data, key.as_ptr(), value.as_ptr());
}

/// `obs_data_set_bool`, same no-op-on-failure shape as `set_string`.
pub fn set_bool(data: *mut ObsDataT, key: &str, value: bool) {
    let Some(obs_data_set_bool) = self::obs_data_set_bool() else {
        return;
    };
    if data.is_null() {
        return;
    }
    let key = cstr(key);
    obs_data_set_bool(data, key.as_ptr(), value);
}
