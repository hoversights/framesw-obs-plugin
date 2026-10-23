//! A view of another source: its picture, at its own size, as a source of
//! its own, so it can carry filters of its own (FrameSW, 2026-10-23).
//!
//! A guest's page, or a camera, is one OBS input that every box showing
//! them shares: one connection to the guest, one opening of the camera.
//! But OBS filters belong to a source, not to a box, so the rounded mask a
//! face box needs was on the shared input, and every box of that guest
//! showed it: put full screen, they came out as a rounded 4:3 with the
//! sides empty, and seating them in a full-screen cue redrew the mask for
//! the whole frame and squared off their face on air (FrameSW
//! BOX_SIZED_GUESTS_WIN_VERIFICATION.md, round 4). A view per box carries
//! that box's mask; the input stays one.
//!
//! The view draws its source (`obs_source_video_render`) at the source's
//! own size (`get_width`/`get_height`), so a box's crop and placement mean
//! the same pixels as before. It makes its source active while it is
//! (`enum_active_sources`), so the source renders and its sound plays, once
//! however many views show it. It has no sound of its own. Its one setting,
//! `source`, names the source it shows.
//!
//! The same idea as exeldro's Source Clone plugin, written here so FrameSW
//! doesn't depend on another plugin being installed.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::Mutex;

use studio_mode_meters_core::metering::{ffi_guard, obs_get_source_by_name, obs_source_release, ObsSourceT};
use studio_mode_meters_core::obs_data::{obs_data_get_string, ObsDataT};
use studio_mode_meters_core::log_line;

/// The source type FrameSW asks OBS for (`CreateInput`'s `inputKind`).
pub const VIEW_SOURCE_ID: &CStr = c"framesw_view";

#[repr(C)]
pub struct ObsWeakSourceT {
    _private: [u8; 0],
}

type EnumProc = extern "C" fn(*mut ObsSourceT, *mut ObsSourceT, *mut c_void);

// Signatures verified against obsproject/obs-studio@32.0.4 (libobs/obs.h,
// libobs/obs-source.h), 2026-10-23.
studio_mode_meters_core::resolved_fn!(obs_register_source_s: extern "C" fn(*const ObsSourceInfoPrefix, usize));
studio_mode_meters_core::resolved_fn!(obs_source_get_weak_source: extern "C" fn(*mut ObsSourceT) -> *mut ObsWeakSourceT);
studio_mode_meters_core::resolved_fn!(obs_weak_source_get_source: extern "C" fn(*mut ObsWeakSourceT) -> *mut ObsSourceT);
studio_mode_meters_core::resolved_fn!(obs_weak_source_release: extern "C" fn(*mut ObsWeakSourceT));
studio_mode_meters_core::resolved_fn!(obs_source_get_width: extern "C" fn(*mut ObsSourceT) -> u32);
studio_mode_meters_core::resolved_fn!(obs_source_get_height: extern "C" fn(*mut ObsSourceT) -> u32);
studio_mode_meters_core::resolved_fn!(obs_source_video_render: extern "C" fn(*mut ObsSourceT));

/// `struct obs_source_info` up to `enum_active_sources`, in its order:
/// libobs copies only the size it is given (`obs_register_source_s`), the
/// rest zero.
#[repr(C)]
pub struct ObsSourceInfoPrefix {
    id: *const c_char,
    /// `enum obs_source_type`: `OBS_SOURCE_TYPE_INPUT` is 0.
    source_type: c_int,
    output_flags: u32,
    get_name: Option<extern "C" fn(*mut c_void) -> *const c_char>,
    create: Option<extern "C" fn(*mut ObsDataT, *mut ObsSourceT) -> *mut c_void>,
    destroy: Option<extern "C" fn(*mut c_void)>,
    get_width: Option<extern "C" fn(*mut c_void) -> u32>,
    get_height: Option<extern "C" fn(*mut c_void) -> u32>,
    get_defaults: Option<extern "C" fn(*mut ObsDataT)>,
    get_properties: Option<extern "C" fn(*mut c_void) -> *mut c_void>,
    update: Option<extern "C" fn(*mut c_void, *mut ObsDataT)>,
    activate: Option<extern "C" fn(*mut c_void)>,
    deactivate: Option<extern "C" fn(*mut c_void)>,
    show: Option<extern "C" fn(*mut c_void)>,
    hide: Option<extern "C" fn(*mut c_void)>,
    video_tick: Option<extern "C" fn(*mut c_void, f32)>,
    video_render: Option<extern "C" fn(*mut c_void, *mut c_void)>,
    filter_video: Option<extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void>,
    filter_audio: Option<extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void>,
    enum_active_sources: Option<extern "C" fn(*mut c_void, EnumProc, *mut c_void)>,
}

// OBS_SOURCE_VIDEO | OBS_SOURCE_CUSTOM_DRAW | OBS_SOURCE_DO_NOT_DUPLICATE
const OUTPUT_FLAGS: u32 = (1 << 0) | (1 << 3) | (1 << 7);

/// One view: the source it shows, held weakly so a source FrameSW removes
/// goes (its views are removed first; a view left showing nothing draws
/// nothing).
struct View {
    me: *mut ObsSourceT,
    target: Mutex<Option<(String, usize)>>,
}

// The raw pointers are OBS's, used only through libobs's own thread-safe calls.
unsafe impl Send for View {}
unsafe impl Sync for View {}

impl View {
    /// The source shown, as a strong reference the caller releases.
    fn source(&self) -> Option<*mut ObsSourceT> {
        let weak = self.target.lock().ok()?.as_ref().map(|(_, w)| *w).filter(|w| *w != 0)? as *mut ObsWeakSourceT;
        let s = obs_weak_source_get_source()?(weak);
        (!s.is_null()).then_some(s)
    }

    fn set_target(&self, name: &str) {
        let Ok(mut target) = self.target.lock() else { return };
        if target.as_ref().is_some_and(|(n, _)| n == name) {
            return;
        }
        if let (Some((_, old)), Some(release)) = (target.take(), obs_weak_source_release()) {
            release(old as *mut ObsWeakSourceT);
        }
        if name.is_empty() {
            return;
        }
        let (Some(by_name), Some(weak_of), Some(release)) = (obs_get_source_by_name(), obs_source_get_weak_source(), obs_source_release()) else {
            return;
        };
        let Ok(c) = CString::new(name) else { return };
        let s = by_name(c.as_ptr());
        if s.is_null() {
            log_line(&format!("view: no source named \"{name}\" (yet)"));
            *target = Some((name.to_string(), 0));
            return;
        }
        let weak = weak_of(s);
        release(s);
        *target = Some((name.to_string(), weak as usize));
    }

    /// A view made before its source: found by name once it exists.
    fn retry(&self) {
        let name = match self.target.lock() {
            Ok(t) => match t.as_ref() {
                Some((n, 0)) => n.clone(),
                _ => return,
            },
            Err(_) => return,
        };
        if let Ok(mut t) = self.target.lock() {
            *t = None;
        }
        self.set_target(&name);
    }
}

fn read_target(settings: *mut ObsDataT) -> String {
    let Some(get) = obs_data_get_string() else { return String::new() };
    let p = get(settings, c"source".as_ptr());
    if p.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

extern "C" fn view_get_name(_type_data: *mut c_void) -> *const c_char {
    c"FrameSW view".as_ptr()
}

extern "C" fn view_create(settings: *mut ObsDataT, source: *mut ObsSourceT) -> *mut c_void {
    ffi_guard("view_create", std::ptr::null_mut(), || {
        let view = Box::new(View { me: source, target: Mutex::new(None) });
        view.set_target(&read_target(settings));
        Box::into_raw(view) as *mut c_void
    })
}

extern "C" fn view_destroy(data: *mut c_void) {
    ffi_guard("view_destroy", (), || {
        if data.is_null() {
            return;
        }
        let view = unsafe { Box::from_raw(data as *mut View) };
        view.set_target("");
    })
}

extern "C" fn view_update(data: *mut c_void, settings: *mut ObsDataT) {
    ffi_guard("view_update", (), || {
        if let Some(view) = unsafe { (data as *const View).as_ref() } {
            view.set_target(&read_target(settings));
        }
    })
}

fn with_source<T>(data: *mut c_void, none: T, f: impl FnOnce(*mut ObsSourceT) -> T) -> T {
    let Some(view) = (unsafe { (data as *const View).as_ref() }) else { return none };
    let Some(s) = view.source() else { return none };
    let out = f(s);
    if let Some(release) = obs_source_release() {
        release(s);
    }
    out
}

extern "C" fn view_get_width(data: *mut c_void) -> u32 {
    ffi_guard("view_get_width", 0, || with_source(data, 0, |s| obs_source_get_width().map_or(0, |w| w(s))))
}

extern "C" fn view_get_height(data: *mut c_void) -> u32 {
    ffi_guard("view_get_height", 0, || with_source(data, 0, |s| obs_source_get_height().map_or(0, |h| h(s))))
}

extern "C" fn view_video_tick(data: *mut c_void, _seconds: f32) {
    ffi_guard("view_video_tick", (), || {
        if let Some(view) = unsafe { (data as *const View).as_ref() } {
            view.retry();
        }
    })
}

extern "C" fn view_video_render(data: *mut c_void, _effect: *mut c_void) {
    ffi_guard("view_video_render", (), || {
        with_source(data, (), |s| {
            if let Some(render) = obs_source_video_render() {
                render(s);
            }
        })
    })
}

/// What a view shows is active while the view is: it renders, and its
/// sound plays (once, however many views show it).
extern "C" fn view_enum_active_sources(data: *mut c_void, callback: EnumProc, param: *mut c_void) {
    ffi_guard("view_enum_active_sources", (), || {
        let Some(view) = (unsafe { (data as *const View).as_ref() }) else { return };
        let me = view.me;
        with_source(data, (), |s| callback(me, s, param))
    })
}

/// Called from `obs_module_load`.
pub fn register_view_source() {
    let Some(register) = obs_register_source_s() else {
        log_line("obs_register_source_s unavailable: no FrameSW view source");
        return;
    };
    let info = ObsSourceInfoPrefix {
        id: VIEW_SOURCE_ID.as_ptr(),
        source_type: 0,
        output_flags: OUTPUT_FLAGS,
        get_name: Some(view_get_name),
        create: Some(view_create),
        destroy: Some(view_destroy),
        get_width: Some(view_get_width),
        get_height: Some(view_get_height),
        get_defaults: None,
        get_properties: None,
        update: Some(view_update),
        activate: None,
        deactivate: None,
        show: None,
        hide: None,
        video_tick: Some(view_video_tick),
        video_render: Some(view_video_render),
        filter_video: None,
        filter_audio: None,
        enum_active_sources: Some(view_enum_active_sources),
    };
    register(&info, std::mem::size_of::<ObsSourceInfoPrefix>());
    log_line("registered the FrameSW view source (framesw_view)");
}
