// SPDX-License-Identifier: GPL-2.0-or-later
//! Per-source audio metering that distinguishes Program from Preview.
//!
//! The whole reason this plugin exists: obs-websocket can report a level
//! for a source that is live, but not for one only staged in Preview.
//! Everything here is read-only with respect to OBS state — it attaches
//! audio capture callbacks and reads levels, and changes nothing.
//!
//! Shared so the community plugin and FrameSW's companion plugin meter
//! identically from one copy of this FFI-critical code. Nothing here names
//! either product; the consumer supplies its identity via
//! `crate::set_identity`.

#![allow(dead_code)]

use crate::obs_data::{self, SourceLevel};
use crate::{calldata, log_line};
use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Mutex;

/// Raw audio for a source, handed on to whoever asked for it.
///
/// `(source_name, samples_per_sec, channels, frames, planes, volume)`.
pub type AudioSink = fn(&str, i32, i32, i32, &[*const f32], f32);

static AUDIO_SINK: Mutex<Option<AudioSink>> = Mutex::new(None);

/// Installs a sink that receives the same audio this module meters.
///
/// Exists because FrameSW forwards that audio to a monitor-speaker tap,
/// and the metering callback used to call FrameSW's own module directly —
/// one line that would have dragged an NDI tap into a community plugin
/// with no business shipping it. As a hook, core stays honest about being
/// read-only: install no sink and nothing leaves this module.
pub fn set_audio_sink(sink: AudioSink) {
    if let Ok(mut slot) = AUDIO_SINK.lock() {
        *slot = Some(sink);
    }
}

// ---------------------------------------------------------------------
// libobs FFI surface — only what this phase needs.
// ---------------------------------------------------------------------

/// Opaque — libobs never exposes `obs_module_t`'s layout to plugins, only
/// pointers to it (`libobs/obs.h`).
pub enum ObsModuleT {}
/// Opaque — same story for `obs_source_t` (`libobs/obs.h`).
pub enum ObsSourceT {}
/// Opaque — same story for `obs_scene_t` (`libobs/obs-scene.h`). A distinct
/// handle type from `ObsSourceT` in libobs's own public API, even though a
/// scene is a source under the hood.
pub enum ObsSceneT {}
/// Opaque — same story for `obs_output_t` (`libobs/obs.h`).
pub enum ObsOutputT {}
/// Opaque — same story for `config_t` (`libobs/util/config-file.h`).
pub enum ConfigT {}

/// `libobs/media-io/media-io-defs.h`: `#define MAX_AV_PLANES 8`.
pub const MAX_AV_PLANES: usize = 8;

/// `libobs/media-io/audio-io.h`'s `struct audio_data` — verbatim field
/// order/types, required for correct ABI since this is a real (not
/// opaque) struct passed by pointer into our callback.
#[repr(C)]
pub struct AudioData {
    pub data: [*mut u8; MAX_AV_PLANES],
    pub frames: u32,
    pub timestamp: u64,
}

/// `libobs/obs.h`:
/// `typedef void (*obs_source_audio_capture_t)(void *param, obs_source_t *source, const struct audio_data *audio_data, bool muted);`
pub type ObsSourceAudioCaptureT =
    extern "C" fn(param: *mut c_void, source: *mut ObsSourceT, audio_data: *const AudioData, muted: bool);

/// `libobs/obs.h`: `void obs_enum_sources(bool (*enum_proc)(void *, obs_source_t *), void *param);`
pub type ObsEnumSourcesProc = extern "C" fn(param: *mut c_void, source: *mut ObsSourceT) -> bool;

// Resolved at runtime (`platform::resolve_as` via `resolved_fn!`), not
// linked at build time — see `platform.rs`'s module doc for why. Exact
// signatures confirmed against obs-studio@master's `libobs/obs.h`.
crate::resolved_fn!(obs_enum_sources: extern "C" fn(ObsEnumSourcesProc, *mut c_void));
crate::resolved_fn!(obs_source_add_audio_capture_callback: extern "C" fn(*mut ObsSourceT, ObsSourceAudioCaptureT, *mut c_void));
// Removing before every add keeps the callback list at exactly one entry
// per source: libobs's add is a bare `da_push_back` with NO dedup
// (obs-source.c, confirmed 2026-07-19), so the 5s re-attach loops would
// otherwise grow the list unboundedly (~720 duplicates/hour/source).
// Remove of a not-present entry is a safe no-op, which is what makes
// remove-then-add idempotent without tracking attach state ourselves
// (any name/pointer-based "already attached" set would go stale when
// FrameSW destroys and recreates a same-named input).
crate::resolved_fn!(obs_source_remove_audio_capture_callback: extern "C" fn(*mut ObsSourceT, ObsSourceAudioCaptureT, *mut c_void));
// Capture callbacks receive PRE-fader audio by design in libobs (volume
// is applied later, at mix time). OBS's own mixer meter gets these same
// raw samples and multiplies by the source's current volume itself
// (obs-audio-controls.c, volmeter_source_data_received) — any meter that
// should track the slider must do the same, hence this lookup.
crate::resolved_fn!(obs_source_get_volume: extern "C" fn(*const ObsSourceT) -> f32);
crate::resolved_fn!(obs_source_get_name: extern "C" fn(*const ObsSourceT) -> *const c_char);
// `libobs/obs.h`'s `struct obs_audio_info { uint32_t samples_per_sec; enum
// speaker_layout speakers; };` and `EXPORT bool obs_get_audio_info(struct
// obs_audio_info *oai);` — the one global source of "how many channels/
// what sample rate is this OBS instance actually running," needed to
// rebuild a tapped source's per-plane audio into the single contiguous,
// evenly-strided buffer NDI's send API expects (`ndi_ffi.rs`). Verified
// against `obsproject/obs-studio@master`'s real header, not guessed.
#[repr(C)]
pub struct ObsAudioInfo {
    samples_per_sec: u32,
    speakers: u32,
}
crate::resolved_fn!(obs_get_audio_info: extern "C" fn(*mut ObsAudioInfo) -> bool);

/// `libobs/media-io/audio-io.h`'s `get_audio_channels` — a `static
/// inline` C function, so it has no linkable symbol to resolve; this is
/// the same lookup table ported by hand, values verified against the
/// real header's `enum speaker_layout`
/// (`SPEAKERS_UNKNOWN=0, MONO=1, STEREO=2, 2POINT1=3, 4POINT0=4,
/// 4POINT1=5, 5POINT1=6, 7POINT1=8`).
pub fn speaker_layout_to_channels(speakers: u32) -> u32 {
    match speakers {
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 5,
        6 => 6,
        8 => 8,
        _ => 0, // SPEAKERS_UNKNOWN (0) or anything unrecognized.
    }
}
// `libobs/obs.h`: "Gets a source by its name. Increments the source
// reference counter, use obs_source_release to release it when complete."
// Needed because `obs_enum_sources` (confirmed against the real
// `obs.c` — `if (s->info.type == OBS_SOURCE_TYPE_INPUT ...)`)
// deliberately excludes scenes (`OBS_SOURCE_TYPE_SCENE`) entirely — the
// only way to reach FrameSW's fixed-name Program/Preview scenes
// ("PGM-A"/"PGM-B") is a direct name lookup, not the general rescan.
crate::resolved_fn!(obs_get_source_by_name: extern "C" fn(*const c_char) -> *mut ObsSourceT);
crate::resolved_fn!(obs_source_release: extern "C" fn(*mut ObsSourceT));

// libobs/obs.h: `obs_source_t *obs_source_create_private(const char *id,
// const char *name, obs_data_t *settings)`.
//
// Private, not `obs_source_create`: a private source is never written into
// the user's scene collection. The CEF warm-up must not leave anything
// behind in their OBS config.
crate::resolved_fn!(obs_source_create_private: extern "C" fn(*const c_char, *const c_char, *mut crate::obs_data::ObsDataT) -> *mut ObsSourceT);

// `libobs/obs.h`: the pair a projector window uses to say "render this
// source even though no scene is showing it". They move a reference count,
// so every `inc` needs exactly one matching `dec`.
//
// MEASURED 2026-08-20, and the reason `set_source_showing` exists at all:
// obs-browser produces no frames whatsoever while its source is not
// showing. A browser source parked in a scene nobody is rendering stays
// black forever — on a healthy Metal renderer just as much as on a broken
// one. FrameSW's browser-render preflight check screenshotted exactly such
// a source and reported "browser sources are not rendering" on a machine
// where a guest's screen share was live on screen. Forcing the probe
// showing turns it magenta in under half a second.
crate::resolved_fn!(obs_source_inc_showing: extern "C" fn(*mut ObsSourceT));
crate::resolved_fn!(obs_source_dec_showing: extern "C" fn(*mut ObsSourceT));
// CORRECTION (2026-07-31, second pass): an earlier comment here claimed
// obs-websocket's `CreateScene` request was itself crashing OBS 32.1.2,
// and blamed OBS 32.1's "partial"/"unstable" Canvas support. Both claims
// were wrong. Symbolicating the crash reports against the shipped
// `obs-websocket` binary put the fault in its `GetCurrentProgramScene`
// handler instead — see `get_current_scenes` below for the real bug.
// `obs_scene_create` is also not canvas-blind: libobs implements it as
// `create_id(obs->data.main_canvas, "scene", name)`, i.e. attached to the
// main canvas exactly like OBS's own "+" button.
//
// `create_scene` (below) is kept anyway on its own merits: creating
// scenes from inside OBS's own process is one fewer obs-websocket round
// trip, and it degrades gracefully when this plugin isn't installed.
crate::resolved_fn!(obs_scene_create: extern "C" fn(*const c_char) -> *mut ObsSceneT);
crate::resolved_fn!(obs_scene_release: extern "C" fn(*mut ObsSceneT));

// libobs/obs.h: `void obs_queue_task(enum obs_task_type type, obs_task_t
// task, void *param, bool wait)`, where `obs_task_t` is
// `void (*)(void *param)` and `enum obs_task_type`'s first variant is
// `OBS_TASK_UI` (= 0, see `OBS_TASK_UI` below). With `wait = true` OBS
// runs the task on its own UI thread and blocks the caller until it
// returns — the sanctioned way for a plugin on a background thread to
// touch frontend state. obs-websocket itself uses exactly this for
// `SetStudioModeEnabled`; it just fails to for the getters below.
crate::resolved_fn!(obs_queue_task: extern "C" fn(c_int, extern "C" fn(*mut c_void), *mut c_void, bool));
// frontend/api/obs-frontend-api.h. Both getters return a *new strong
// reference*, and both are genuinely nullable: `obs_frontend_get_current_
// scene` resolves `main->programScene` (a weak ref that goes dead when the
// program scene is deleted) in Studio Mode, or reads the scene-list
// widget's current item otherwise, and the preview getter returns null
// whenever Studio Mode is off. Resolved process-wide rather than against
// libobs specifically — these live in `obs-frontend-api`, which
// `platform::resolve` already searches on both platforms.
/// `libobs/obs.h`: `void obs_source_enum_active_sources(obs_source_t *source,
/// obs_source_enum_proc_t enum_callback, void *param);` — for a scene, the
/// items it is currently rendering.
type ObsSourceEnumProc = extern "C" fn(parent: *mut ObsSourceT, child: *mut ObsSourceT, param: *mut c_void);
crate::resolved_fn!(obs_source_enum_active_sources: extern "C" fn(*mut ObsSourceT, ObsSourceEnumProc, *mut c_void));

crate::resolved_fn!(obs_frontend_get_current_scene: extern "C" fn() -> *mut ObsSourceT);
crate::resolved_fn!(obs_frontend_get_current_preview_scene: extern "C" fn() -> *mut ObsSourceT);
crate::resolved_fn!(obs_frontend_preview_program_mode_active: extern "C" fn() -> bool);

// `libobs/obs.h`: `bool obs_set_audio_monitoring_device(const char *name,
// const char *id)` — the call OBS's own Settings dialog makes when the
// Monitoring Device is changed. It re-opens the monitoring output there and
// then, which is the whole reason this is here.
//
// Writing `Audio/MonitoringDeviceName` through obs-websocket's
// `SetProfileParameter` does NOT do this. Measured 2026-08-24: the request
// returned success, the value read back changed, and OBS logged no
// monitoring-device change at all — the config moved and the audio did not.
// The same "silently succeeds while doing nothing" shape that makes
// wrong-thread frontend calls so dangerous.
crate::resolved_fn!(obs_set_audio_monitoring_device: extern "C" fn(*const c_char, *const c_char) -> bool);

// `libobs/obs.h`: `bool obs_get_audio_monitoring_device(const char **name,
// const char **id)` — the device OBS is monitoring through *right now*.
//
// Deliberately paired with the setter. The profile config and the running
// device are two different things: `obs_set_audio_monitoring_device` changes
// the live one and does not write config, while `SetProfileParameter` writes
// config and does not touch the live one. Reading the runtime value back is
// the only way a caller can tell what is actually in effect rather than what
// somebody wrote down.
crate::resolved_fn!(obs_get_audio_monitoring_device: extern "C" fn(*mut *const c_char, *mut *const c_char));

// `libobs/obs.h`: `void obs_enum_audio_monitoring_devices(
// obs_enum_audio_device_cb cb, void *data)`, where the callback is
// `bool (*)(void *data, const char *name, const char *id)` and returning
// false stops the walk.
//
// This is the authoritative list: OBS needs a device *name and id* pair,
// and enumerating any other way (cpal, CoreAudio directly) yields names
// that then have to be matched back to ids by string — which is exactly how
// you end up setting the wrong device.
pub type ObsEnumAudioDeviceCb =
    extern "C" fn(data: *mut c_void, name: *const c_char, id: *const c_char) -> bool;
crate::resolved_fn!(obs_enum_audio_monitoring_devices: extern "C" fn(ObsEnumAudioDeviceCb, *mut c_void));


// --- audio monitoring device -------------------------------------------

/// Read/write state for `monitoring_device_on_ui_thread`.
#[derive(Default)]
pub struct MonitoringDevice {
    /// Set this to change the device; leave `None` to only read.
    pub wanted: Option<(String, String)>,
    /// True once the UI-thread task actually ran.
    pub ran: bool,
    /// What `obs_set_audio_monitoring_device` returned, if a write was asked.
    pub applied: bool,
    /// Every device OBS will accept, as (name, id).
    pub devices: Vec<(String, String)>,
    /// What OBS is actually monitoring through after this task ran — the
    /// live device, not what the profile says.
    pub current: Option<(String, String)>,
}

extern "C" fn collect_monitoring_device(
    data: *mut c_void,
    name: *const c_char,
    id: *const c_char,
) -> bool {
    if data.is_null() {
        return false;
    }
    let out = unsafe { &mut *data.cast::<Vec<(String, String)>>() };
    let name = if name.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned()
    };
    let id = if id.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(id) }.to_string_lossy().into_owned()
    };
    out.push((name, id));
    true
}

/// Sets and/or enumerates OBS's audio monitoring device, on OBS's UI thread.
///
/// On the UI thread deliberately. `obs_set_audio_monitoring_device` tears
/// down and re-creates the monitoring output, which is frontend-owned state;
/// the plugin's other frontend calls are queued the same way and for the same
/// reason.
pub extern "C" fn monitoring_device_on_ui_thread(param: *mut c_void) {
    if param.is_null() {
        return;
    }
    let state = unsafe { &mut *param.cast::<MonitoringDevice>() };
    state.ran = true;

    if let Some((name, id)) = state.wanted.clone() {
        if let (Ok(name), Ok(id), Some(set)) = (
            CString::new(name),
            CString::new(id),
            obs_set_audio_monitoring_device(),
        ) {
            state.applied = set(name.as_ptr(), id.as_ptr());
        }
    }

    if let Some(get) = obs_get_audio_monitoring_device() {
        let mut name: *const c_char = std::ptr::null();
        let mut id: *const c_char = std::ptr::null();
        get(&mut name, &mut id);
        if !name.is_null() || !id.is_null() {
            let to_s = |p: *const c_char| {
                if p.is_null() {
                    String::new()
                } else {
                    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
                }
            };
            state.current = Some((to_s(name), to_s(id)));
        }
    }

    if let Some(enum_devices) = obs_enum_audio_monitoring_devices() {
        let mut found: Vec<(String, String)> = Vec::new();
        enum_devices(
            collect_monitoring_device,
            (&mut found as *mut Vec<(String, String)>).cast(),
        );
        state.devices = found;
    }
}

/// `enum obs_task_type`'s first variant in libobs/obs.h — run on OBS's
/// Qt UI thread.
pub const OBS_TASK_UI: c_int = 0;

// libobs/obs.h: `void obs_enum_outputs(bool (*enum_proc)(void *,
// obs_output_t *), void *param)`. libobs holds its outputs mutex for the
// whole enumeration, so an `obs_output_t*` handed to the callback stays
// alive for that call — but only the output struct itself. See
// `list_video_outputs` for what that does and doesn't make safe to read.
crate::resolved_fn!(obs_enum_outputs: extern "C" fn(extern "C" fn(*mut c_void, *mut ObsOutputT) -> bool, *mut c_void));
crate::resolved_fn!(obs_output_get_name: extern "C" fn(*const ObsOutputT) -> *const c_char);
crate::resolved_fn!(obs_output_get_id: extern "C" fn(*const ObsOutputT) -> *const c_char);
crate::resolved_fn!(obs_output_active: extern "C" fn(*const ObsOutputT) -> bool);
crate::resolved_fn!(obs_output_get_flags: extern "C" fn(*const ObsOutputT) -> u32);

/// `libobs/obs-output.h`: `#define OBS_OUTPUT_VIDEO (1 << 0)`.
pub const OBS_OUTPUT_VIDEO: u32 = 1 << 0;

// `frontend/api/obs-frontend-api.h`: `config_t *obs_frontend_get_user_config(void)`
// — OBS's *live* user.ini config object, the same one OBS itself writes out
// at exit. Going through it (rather than editing user.ini on disk) is what
// makes a change stick: OBS holds these values in memory and rewrites the
// file on close, so any external edit made while OBS runs is silently
// clobbered. `obs_frontend_get_global_config` is the deprecated alias for
// the same thing and is deliberately not used here.
crate::resolved_fn!(obs_frontend_get_user_config: extern "C" fn() -> *mut ConfigT);

// `frontend/api/obs-frontend-api.h`: `config_t *obs_frontend_get_app_config(void)`
// — a DIFFERENT file from the user config above. OBS 30.2 split its settings
// in two: user.ini (what `obs_frontend_get_user_config` returns) and
// global.ini (this one). The graphics renderer lives in global.ini, so the
// projector/NDI precedent does not reach it. Verified against a real
// global.ini on 2026-08-20: `[Video] Renderer=Metal`.
//
// Older OBS has no such symbol — the resolver returns `None` and the caller
// reports that rather than writing to the wrong file.
crate::resolved_fn!(obs_frontend_get_app_config: extern "C" fn() -> *mut ConfigT);

// `libobs/util/config-file.h`.
crate::resolved_fn!(config_get_bool: extern "C" fn(*mut ConfigT, *const c_char, *const c_char) -> bool);
crate::resolved_fn!(config_set_bool: extern "C" fn(*mut ConfigT, *const c_char, *const c_char, bool));
// Returns a pointer into the config object's own storage — valid only until
// the config changes, so copy it before doing anything else with `config`.
crate::resolved_fn!(config_get_string: extern "C" fn(*mut ConfigT, *const c_char, *const c_char) -> *const c_char);
crate::resolved_fn!(config_set_string: extern "C" fn(*mut ConfigT, *const c_char, *const c_char, *const c_char));
crate::resolved_fn!(config_save_safe: extern "C" fn(*mut ConfigT, *const c_char, *const c_char) -> c_int);

/// user.ini's `[BasicWindow] ProjectorAlwaysOnTop` — OBS Settings ->
/// General -> Projectors -> "Make projectors always on top". Verified
/// against a real user.ini on 2026-08-01.
///
/// Deliberately app-global, NOT per-profile: OBS keeps it in user.ini, so
/// it cannot be scoped to FrameSW's own profile the way video settings and
/// the monitoring device can. Changing it follows the user into every
/// other profile and scene collection they have — which is exactly why
/// FrameSW exposes it as an explicit, clearly-labelled opt-in rather than
/// setting it silently on connect.
pub const PROJECTOR_ON_TOP_SECTION: &str = "BasicWindow";
pub const PROJECTOR_ON_TOP_KEY: &str = "ProjectorAlwaysOnTop";

/// DistroAV (the NDI plugin) keeps its output switches in OBS's own
/// user.ini under `[NDIPlugin]`, so the same live config object used for
/// `projector_on_top` reaches them — verified against a real user.ini
/// 2026-08-01.
///
/// These are the "Main Output"/"Preview Output" checkboxes in
/// Tools -> NDI Output Settings. FrameSW's Audio Monitor listens to the NDI
/// sources they produce ("... (OBS Program)" / "... (OBS Preview)"), so
/// with both off that feature has nothing to connect to.
///
/// The keys exist whether or not DistroAV is installed — they're just inert
/// without it, which is why the caller checks for the plugin separately
/// rather than inferring it from a successful write here.
/// global.ini's `[Video] Renderer` — OBS Settings -> Advanced -> Video ->
/// Renderer. Verified against a real global.ini on 2026-08-20.
///
/// In the **app** config (global.ini), not the user config the two constants
/// above use — see `obs_frontend_get_app_config`.
///
/// App-global and profile-independent, like `PROJECTOR_ON_TOP_SECTION`:
/// changing it follows the operator into every other profile and scene
/// collection, and OBS only reads it at startup, so a write here does
/// nothing until OBS restarts. Both facts have to reach the operator before
/// FrameSW writes it.
pub const RENDERER_SECTION: &str = "Video";
pub const RENDERER_KEY: &str = "Renderer";

pub const NDI_SECTION: &str = "NDIPlugin";
pub const NDI_MAIN_OUTPUT_KEY: &str = "MainOutputEnabled";
pub const NDI_PREVIEW_OUTPUT_KEY: &str = "PreviewOutputEnabled";

// `frontend/api/obs-frontend-api.h` profile API. All three are verified
// against OBS 32.1.2's own `OBSStudioAPI.cpp` implementation, and the
// implementation detail is the entire reason `ensure_profile` exists:
//
// * `obs_frontend_set_current_profile` walks `main->ui->profileMenu`'s
//   QActions and calls `action->trigger()` — raw Qt widget access with no
//   marshalling whatsoever. Calling it off the UI thread (as obs-websocket's
//   own `SetCurrentProfile` request does, from a pooled worker) touches Qt
//   widgets from the wrong thread. It also simply does nothing when no
//   action matches, which makes "try to switch, then read back" a safe
//   existence check — no profile-list array to enumerate or free.
// * `obs_frontend_duplicate_profile` posts `CreateDuplicateProfile` with
//   Qt::AutoConnection, so it is *asynchronous* from a worker thread but
//   *synchronous* from the UI thread. Running it on the UI thread is what
//   makes it complete before returning instead of landing whenever.
// * `obs_frontend_get_current_profile` returns `bstrdup(...)` — caller
//   frees with `bfree`.
crate::resolved_fn!(obs_frontend_get_current_profile: extern "C" fn() -> *mut c_char);
crate::resolved_fn!(obs_frontend_set_current_profile: extern "C" fn(*const c_char));
crate::resolved_fn!(obs_frontend_duplicate_profile: extern "C" fn(*const c_char));
// `libobs/util/bmem.h` — frees what libobs allocated.
crate::resolved_fn!(bfree: extern "C" fn(*mut c_void));
// libobs/util/base.h — real signature is variadic
// (`void blog(int log_level, const char *format, ...)`). Always called
// here with a fixed "%s" format and exactly one string arg (`log_line`
// below) — deliberately never passing anything OBS-/source-controlled as
// the format string itself.


// ---------------------------------------------------------------------
// FFI panic safety: every function OBS calls into this plugin — either
// directly (the module entry points below) or via function pointer (the
// two callbacks it hands to `obs_enum_sources`/
// `obs_source_add_audio_capture_callback`) — must never let a Rust panic
// unwind across that boundary. Unwinding into the C frames on the other
// side is undefined behavior, and in this plugin's case that's inside a
// user's live-streaming process, not a sandbox. `catch_unwind` turns any
// panic into an `Err` here instead, logged via the existing `log_line`
// path, with the entry point returning its safe "did nothing" value to
// OBS exactly as if this call had never been made — never propagated,
// never aborted.
pub fn ffi_guard<R>(entry_point: &str, fallback: R, f: impl FnOnce() -> R + std::panic::UnwindSafe) -> R {
    match std::panic::catch_unwind(f) {
        Ok(value) => value,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            log_line(&format!("PANIC caught at FFI boundary in {entry_point} — {msg}"));
            fallback
        }
    }
}

// ---------------------------------------------------------------------
// Attach an audio capture callback to every source we can find; each
// callback updates a shared map (not a direct log/emit — that's far too
// often to usefully log or send over the wire) that a separate,
// slower-cadence thread drains and forwards to FrameSW.
// ---------------------------------------------------------------------

/// Set from `obs_module_unload`, checked at the top of every iteration
/// (and right before each libobs call) in both background loops below.
/// Without this, a crash is guaranteed sooner or later: these threads are
/// detached and loop forever with no other way to learn that OBS is
/// shutting down, so they keep calling into libobs (`obs_enum_sources`,
/// etc.) even after OBS has started tearing down the very state those
/// calls read/lock — confirmed live, 2026-07-15: OBS segfaulted inside
/// `obs_enum_sources`'s internal mutex lock, called from
/// `spawn_periodic_rescan`, at the moment the user closed OBS.
pub static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Live-confirmed real crash source (2026-07-31): `spawn_periodic_rescan`'s
/// `attach_scene_audio_taps` calls native `obs_get_source_by_name`/
/// `obs_source_add_audio_capture_callback` directly on PGM-A/PGM-B, on its
/// own independent 5s timer — completely decoupled from FrameSW's app-side
/// websocket timing. Racing that against FrameSW's own `CreateScene`/
/// `SetCurrentProgramScene` calls for the same scene (right after a
/// reconnect that has to recreate one) crashed OBS reliably. FrameSW's app
/// now sends `pause_rescan` before doing that scene setup and
/// `resume_rescan` right after — this flag is what the rescan loop checks
/// each cycle to skip its work (but keep looping, ready to resume) while
/// paused. Defaults to *not* paused so the plugin behaves exactly as
/// before if the app never sends either request (e.g. an older FrameSW
/// version, or another obs-websocket client entirely).
pub static RESCAN_PAUSED: AtomicBool = AtomicBool::new(false);

/// Join handles for both background threads, so `obs_module_unload` can
/// block until they've actually exited rather than merely requesting a
/// stop and hoping — the flag alone leaves a window where a thread is
/// mid-call into libobs at the exact moment unload fires; joining closes
/// it, at the cost of unload blocking for at most one loop iteration
/// (~100ms).
pub static THREADS: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());

/// name -> (peak_db, obs_source_active). Updated on every audio callback
/// (cheap, in-memory only); drained by `spawn_emit_loop` at a much slower,
/// human/UI-appropriate cadence. `active` is the whole point of this
/// plugin existing — it's exactly what `InputVolumeMeters` can't report
/// for Preview-only content.
pub static LEVELS: Mutex<Option<HashMap<String, (f32, bool)>>> = Mutex::new(None);

pub extern "C" fn audio_capture_callback(
    param: *mut c_void,
    source: *mut ObsSourceT,
    audio_data: *const AudioData,
    muted: bool,
) {
    ffi_guard(
        "audio_capture_callback",
        (),
        std::panic::AssertUnwindSafe(|| audio_capture_callback_impl(param, source, audio_data, muted)),
    );
}

pub fn audio_capture_callback_impl(
    _param: *mut c_void,
    source: *mut ObsSourceT,
    audio_data: *const AudioData,
    muted: bool,
) {
    if audio_data.is_null() {
        return;
    }
    // Safety: libobs guarantees `audio_data` is valid for the duration of
    // this callback (it's a stack-allocated struct on the audio thread's
    // side, not something we're expected to retain past this call).
    let audio_data = unsafe { &*audio_data };
    if audio_data.frames == 0 || audio_data.data[0].is_null() {
        return;
    }

    // Verified live in Phase 1 (real, sane dB values from real FrameSW
    // shots): OBS's internal audio pipeline is 32-bit float, planar
    // (AUDIO_FORMAT_FLOAT_PLANAR) by the time a source's own audio
    // capture callback fires.
    let samples = unsafe {
        std::slice::from_raw_parts(audio_data.data[0].cast::<f32>(), audio_data.frames as usize)
    };
    let peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
    // Post-fader, matching OBS's mixer meter: these samples are pre-fader
    // (libobs applies volume at mix time, after this callback), so scale
    // by the source's current volume and honor the mute flag here —
    // otherwise FrameSW's meters keep showing full signal with the slider
    // pulled to silence (TASKS.md item 26). Missing symbol degrades to
    // 1.0 (the old pre-fader behavior), never to silence.
    let volume = obs_source_get_volume().map_or(1.0, |get_volume| get_volume(source));
    let peak = peak * volume;
    let peak_db = if muted || peak <= 0.0 { -100.0 } else { 20.0 * peak.log10() };

    let Some(obs_source_get_name) = obs_source_get_name() else {
        return;
    };
    let name = unsafe {
        let ptr = obs_source_get_name(source);
        if ptr.is_null() {
            return;
        }
        CStr::from_ptr(ptr).to_string_lossy().into_owned()
    };
    // Bus by scene membership, not `obs_source_active(source)` — see
    // `SOURCE_BUS` for the measurement that retired that call.
    let active = source_is_on_program(&name);

    // Preview-layer monitor taps (`audio_tap.rs`) — reuses this exact
    // callback (already attached to every source, unconditionally) rather
    // than a second `obs_source_add_audio_capture_callback` registration,
    // so a tap adds no new attachment for libobs's callback list to
    // dedupe (see the `resolved_fn!` comment on
    // `obs_source_remove_audio_capture_callback` above for why that
    // matters). Channel count/sample rate come from OBS's one global
    // audio setting (`obs_get_audio_info`), since `audio_data` itself
    // carries neither — missing symbol or a not-yet-tapped source both
    // degrade to `forward_if_tapped` doing nothing, same as every other
    // best-effort path in this crate.
    if let Some(obs_get_audio_info) = obs_get_audio_info() {
        let mut info = ObsAudioInfo { samples_per_sec: 0, speakers: 0 };
        if obs_get_audio_info(&mut info) {
            let channels = speaker_layout_to_channels(info.speakers);
            if channels > 0 {
                let planes: Vec<*const f32> = audio_data.data[..channels as usize]
                    .iter()
                    .map(|p| p.cast::<f32>().cast_const())
                    .collect();
                // Same `muted` the peak-dB calc above already honors
                // (this callback's own `muted` parameter, direct from
                // libobs) — without this, a muted layer's raw audio
                // still reached the monitor tap/mix, since `muted` only
                // ever affected the *meter* reading here, never what got
                // forwarded. Live-reported: the Layer Audio Out strip's
                // own mute button had no effect on the monitor speaker.
                let effective_volume = if muted { 0.0 } else { volume };
                // Nothing here unless a consumer installed a sink; the
                // community plugin installs none and this is a no-op.
                let sink = AUDIO_SINK.lock().ok().and_then(|s| *s);
                if let Some(sink) = sink {
                    sink(
                        &name,
                        info.samples_per_sec as i32,
                        channels as i32,
                        audio_data.frames as i32,
                        &planes,
                        effective_volume,
                    );
                }
            }
        }
    }

    if let Ok(mut guard) = LEVELS.lock() {
        guard.get_or_insert_with(HashMap::new).insert(name, (peak_db, active));
    }
}

pub extern "C" fn attach_callback_enum_proc(param: *mut c_void, source: *mut ObsSourceT) -> bool {
    // Fallback `false` on a caught panic: stop this enumeration pass early
    // rather than risk repeating whatever triggered it against the rest
    // of the sources — the next 5s rescan tries again from scratch.
    ffi_guard(
        "attach_callback_enum_proc",
        false,
        std::panic::AssertUnwindSafe(|| attach_callback_enum_proc_impl(param, source)),
    )
}

pub fn attach_callback_enum_proc_impl(_param: *mut c_void, source: *mut ObsSourceT) -> bool {
    // Remove-then-add: net exactly one list entry per source per cycle
    // (see the resolved_fn comment on remove — libobs's add never dedups).
    if let Some(remove) = obs_source_remove_audio_capture_callback() {
        remove(source, audio_capture_callback, std::ptr::null_mut());
    }
    if let Some(obs_source_add_audio_capture_callback) = obs_source_add_audio_capture_callback() {
        obs_source_add_audio_capture_callback(source, audio_capture_callback, std::ptr::null_mut());
    }
    true // keep enumerating
}

/// FrameSW's fixed Program/Preview scene names (`shot.rs`'s
/// `ProgramSlot::scene_name()` on the FrameSW side) — identity never
/// changes, only which one currently holds the "Program" vs "Preview"
/// role, which FrameSW itself already tracks. Attaching here gives real
/// composited-mix audio for whichever is live, the same way
/// `attach_callback_enum_proc` does for individual shot inputs — this is
/// the only reason Main Audio Out's real metering used to depend on OBS's
/// NDI Main/Preview Output at all.
/// The scene each role is currently tapped on, so a role that moves to a
/// different scene detaches from the old one.
///
/// Under the previous fixed-name scheme this could not happen: the taps sat
/// on `PGM-A`/`PGM-B` forever. Resolving by role means the underlying scene
/// changes whenever the operator switches, and attaching without detaching
/// would leave a live audio callback on every scene that was ever Program —
/// growing for the life of the OBS session and reporting levels for scenes
/// that are no longer on air.
pub static ATTACHED_PROGRAM_SCENE: Mutex<Option<String>> = Mutex::new(None);
pub static ATTACHED_PREVIEW_SCENE: Mutex<Option<String>> = Mutex::new(None);

/// Last Program/Preview scene names read on the UI thread, as
/// `(program, preview)`. `None` until the first refresh lands.
///
/// **This cache exists to break a shutdown deadlock, not for speed.**
/// `obs_module_unload` runs on OBS's UI thread — measured, not assumed:
/// instrumenting both paths showed unload and the UI tasks on the same
/// `ThreadId` — and it joins the rescan thread. If the rescan thread were
/// blocked in `obs_queue_task(OBS_TASK_UI, .., wait: true)`, it would be
/// waiting for a queue only the UI thread can drain, while the UI thread
/// waits in `join()`. OBS hangs on exit, roughly once in a hundred quits:
/// the worst kind of hang.
///
/// The `SHUTTING_DOWN` check before the call cannot fix that — the check
/// and the blocking wait are not atomic, so a thread already past the check
/// enters the wait regardless.
///
/// So the rescan thread never waits. It queues a refresh that writes here
/// whenever the UI thread gets to it, and attaches using whatever the last
/// refresh produced. One cycle of staleness (~5s) is harmless: taps are
/// re-attached every cycle anyway, and this loop was already eventual by
/// design.
pub static CACHED_SCENE_ROLES: Mutex<Option<(Option<String>, Option<String>)>> = Mutex::new(None);

/// Attaches the composited-mix audio callback to whichever scenes OBS
/// currently reports as **Program and Preview**, by role.
///
/// Previously this looked up two hardcoded names, `PGM-A` and `PGM-B` —
/// FrameSW's own scene naming baked into the metering path. In any OBS that
/// is not running a FrameSW show those scenes do not exist, so scene-level
/// metering silently did nothing. Resolving by role makes it correct
/// everywhere, FrameSW included: FrameSW's Program and Preview really are
/// PGM-A/PGM-B, so the same two scenes are found, but because they hold the
/// role rather than because of their names.
///
/// **The frontend read is marshalled onto OBS's UI thread**, reusing the
/// same `read_current_scenes_on_ui_thread` task the `get_current_scenes`
/// vendor request uses. That is not incidental: this function runs on the
/// periodic rescan thread and on obs-websocket request threads, and reading
/// the frontend's Qt-owned scene state from a worker thread is exactly the
/// pattern this plugin exists to avoid — see
/// `handle_get_current_scenes_impl`'s own comment. The task is tiny (two
/// getters and two name copies) and only runs every ~5s.
///
/// `obs_enum_sources` cannot reach scenes at all (confirmed: it filters to
/// `OBS_SOURCE_TYPE_INPUT`), which is why they need this separate path.
/// Refreshes `CACHED_SCENE_ROLES`. Queued onto the UI thread **without
/// waiting**, so it takes no pointer to caller stack — it writes into a
/// `'static` and the caller may have moved on by the time this runs.
pub extern "C" fn cache_scene_roles_on_ui_thread(_param: *mut c_void) {
    ffi_guard(
        "cache_scene_roles_on_ui_thread",
        (),
        std::panic::AssertUnwindSafe(|| {
            let program = frontend_scene_name(obs_frontend_get_current_scene());
            let preview = frontend_scene_name(obs_frontend_get_current_preview_scene());
            if let Ok(mut cached) = CACHED_SCENE_ROLES.lock() {
                *cached = Some((program, preview));
            }
        }),
    );
}

/// Source name -> true when that source is on the Program bus.
///
/// Replaces `obs_source_active()`, which was measured on 2026-08-14
/// returning false for sources demonstrably in the Program scene — even
/// ones with video. Whatever that call tracks, it is not "is this source
/// on the Program bus", and no other per-source query answers that
/// question either: `obs_source_showing` is true for both buses, which is
/// the distinction being asked about.
///
/// The authoritative answer is membership. The plugin already resolves
/// which scene is Program and which is Preview (`attach_scene_audio_taps`),
/// so walking each of those two scenes and recording what is inside it
/// gives the bus by construction, with nothing left to disagree.
///
/// Rebuilt on every rescan, so a source that moves between scenes is
/// re-classified within one cycle rather than keeping a stale bus forever.
static SOURCE_BUS: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);

/// Collects one scene's active children into `SOURCE_BUS`.
extern "C" fn collect_bus_member(
    _parent: *mut ObsSourceT,
    child: *mut ObsSourceT,
    param: *mut c_void,
) {
    let is_program = !param.is_null();
    let Some(get_name) = obs_source_get_name() else {
        return;
    };
    let raw = get_name(child);
    if raw.is_null() {
        return;
    }
    let name = unsafe { CStr::from_ptr(raw) }.to_string_lossy().to_string();
    if let Ok(mut guard) = SOURCE_BUS.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        // Program wins if a source is somehow in both: what is on air is
        // the more consequential fact to report.
        let entry = map.entry(name).or_insert(is_program);
        *entry = *entry || is_program;
    }
}

/// Rebuilds `SOURCE_BUS` from the current Program and Preview scenes.
pub fn refresh_source_bus(program: Option<&str>, preview: Option<&str>) {
    let (Some(by_name), Some(release), Some(enum_active)) = (
        obs_get_source_by_name(),
        obs_source_release(),
        obs_source_enum_active_sources(),
    ) else {
        return;
    };
    if let Ok(mut guard) = SOURCE_BUS.lock() {
        *guard = Some(HashMap::new());
    }
    for (scene, is_program) in [(program, true), (preview, false)] {
        let Some(scene) = scene else { continue };
        let Ok(cname) = CString::new(scene) else { continue };
        let src = by_name(cname.as_ptr());
        if src.is_null() {
            continue;
        }
        // A non-null param marks the Program walk; null marks Preview.
        let marker = if is_program { 1usize as *mut c_void } else { std::ptr::null_mut() };
        enum_active(src, collect_bus_member, marker);
        release(src);
    }
}

/// Which bus a source is on, or false when it is on neither/unknown.
pub fn source_is_on_program(name: &str) -> bool {
    SOURCE_BUS
        .lock()
        .ok()
        .and_then(|g| g.as_ref().and_then(|m| m.get(name).copied()))
        .unwrap_or(false)
}

pub fn attach_scene_audio_taps() {
    // Ask the UI thread for fresh names, but never wait for the answer —
    // see `CACHED_SCENE_ROLES`. This call is what unload's `join()` would
    // otherwise deadlock against.
    if let Some(obs_queue_task) = obs_queue_task() {
        obs_queue_task(
            OBS_TASK_UI,
            cache_scene_roles_on_ui_thread,
            std::ptr::null_mut(),
            false,
        );
    }

    let snapshot = match CACHED_SCENE_ROLES.lock() {
        Ok(cached) => cached.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let Some((program, preview)) = snapshot else {
        return; // first cycle — nothing read yet, attach on the next one
    };
    attach_role_tap("program", program.as_deref(), &ATTACHED_PROGRAM_SCENE);
    attach_role_tap("preview", preview.as_deref(), &ATTACHED_PREVIEW_SCENE);
    // Rebuilt here, where the two roles have just been resolved, so the
    // bus map and the taps can never describe different scenes.
    refresh_source_bus(program.as_deref(), preview.as_deref());
}

/// Moves one role's audio tap to `scene`, detaching from whatever that role
/// was tapped on before.
///
/// Re-attaching to the same scene is deliberately still done every cycle:
/// it is the same remove-then-add idempotency `attach_callback_enum_proc`
/// relies on, and it re-establishes the tap if OBS destroyed and recreated
/// the scene under the same name. Only the *logging* is suppressed for an
/// unchanged scene, so the log records role changes rather than repeating
/// every 5 seconds.
pub fn attach_role_tap(role: &str, scene: Option<&str>, attached: &Mutex<Option<String>>) {
    let (Some(get_by_name), Some(add_cb), Some(release)) = (
        obs_get_source_by_name(),
        obs_source_add_audio_capture_callback(),
        obs_source_release(),
    ) else {
        return;
    };
    let remove_cb = obs_source_remove_audio_capture_callback();

    let mut attached = match attached.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let changed = attached.as_deref() != scene;

    // Detach from the scene this role has moved off, or from the scene it
    // held when the role went away entirely (no Program scene at all is a
    // real OBS state — see handle_get_current_scenes_impl).
    if changed {
        if let (Some(old), Some(remove)) = (attached.as_deref(), remove_cb) {
            if let Ok(cold) = CString::new(old) {
                let src = get_by_name(cold.as_ptr());
                if !src.is_null() {
                    remove(src, audio_capture_callback, std::ptr::null_mut());
                    release(src);
                }
            }
        }
        *attached = None;
    }

    let Some(scene) = scene else {
        return;
    };
    let Ok(cname) = CString::new(scene) else {
        return;
    };
    let source = get_by_name(cname.as_ptr());
    if source.is_null() {
        return; // named scene not present yet
    }
    if let Some(remove) = remove_cb {
        remove(source, audio_capture_callback, std::ptr::null_mut());
    }
    add_cb(source, audio_capture_callback, std::ptr::null_mut());
    release(source);

    if changed {
        log_line(&format!("attached real audio tap to {role} scene '{scene}'"));
    }
    *attached = Some(scene.to_string());
}

/// Periodically re-enumerates and (re-)attaches the callback, rather than
/// hooking libobs's `source_create` signal — deliberately the simplest
/// thing that could prove the hypothesis, not the final design. Remaining
/// rough edge: sources created between scans (this fires every 5s) aren't
/// instrumented until the next scan. (The former rough edge — duplicate
/// attachment growing libobs's callback list unboundedly, confirmed real
/// 2026-07-19: libobs's add is a bare `da_push_back` — is closed by the
/// remove-then-add pattern in both attach paths below.)
pub fn spawn_periodic_rescan() {
    let handle = std::thread::spawn(|| loop {
        if SHUTTING_DOWN.load(Ordering::Acquire) {
            return;
        }
        // See `RESCAN_PAUSED`'s doc comment — skip this cycle's work
        // entirely while paused, but keep the loop (and its shutdown
        // responsiveness) alive. Checked only here, once per ~5s cycle —
        // a `resume_rescan` sent while this thread is mid-sleep takes
        // effect at the next cycle, not instantly. That's fine for this
        // flag's actual use (FrameSW pauses before, and resumes some time
        // after, its own scene setup) — instant pickup isn't needed the
        // way it is for shutdown.
        if !RESCAN_PAUSED.load(Ordering::Acquire) {
            if let Some(obs_enum_sources) = obs_enum_sources() {
                if !SHUTTING_DOWN.load(Ordering::Acquire) {
                    obs_enum_sources(attach_callback_enum_proc, std::ptr::null_mut());
                }
            }
            if !SHUTTING_DOWN.load(Ordering::Acquire) {
                attach_scene_audio_taps();
            }
        }
        // Slept in short increments rather than one 5s call so a shutdown
        // request is noticed within ~100ms instead of up to 5s later.
        for _ in 0..50 {
            if SHUTTING_DOWN.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
    if let Ok(mut threads) = THREADS.lock() {
        threads.push(handle);
    }
}

pub static VENDOR: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

pub const EMIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

pub fn spawn_emit_loop() {
    let handle = std::thread::spawn(|| loop {
        std::thread::sleep(EMIT_INTERVAL);
        if SHUTTING_DOWN.load(Ordering::Acquire) {
            return;
        }
        let vendor = VENDOR.load(Ordering::Acquire);
        if vendor.is_null() {
            continue;
        }
        let drained: Vec<SourceLevel> = {
            let Ok(mut guard) = LEVELS.lock() else {
                continue;
            };
            guard
                .get_or_insert_with(HashMap::new)
                .drain()
                .map(|(name, (peak_db, active))| SourceLevel { name, peak_db, active })
                .collect()
        };
        if drained.is_empty() {
            continue;
        }
        if SHUTTING_DOWN.load(Ordering::Acquire) {
            return;
        }
        let payload = obs_data::build_levels_payload(&drained);
        calldata::vendor_emit_event(vendor, "audio_levels", obs_data::as_void(payload));
        obs_data::release(payload);
    });
    if let Ok(mut threads) = THREADS.lock() {
        threads.push(handle);
    }
}

// ---------------------------------------------------------------------
// Required OBS module entry points — see `OBS_DECLARE_MODULE()` in
// `libobs/obs-module.h`; hand-expanded here since we're not using the C
// macro (no C compilation step in this crate).
// ---------------------------------------------------------------------



/// Resolves one of the two nullable frontend scene getters to a name,
/// releasing the strong reference it hands back. `None` covers both "OBS
/// has no such scene right now" and "the symbol didn't resolve"; the
/// caller separates those via `CurrentScenes::ran`.
pub fn frontend_scene_name(getter: Option<extern "C" fn() -> *mut ObsSourceT>) -> Option<String> {
    let getter = getter?;
    let obs_source_get_name = obs_source_get_name()?;
    let obs_source_release = obs_source_release()?;
    let source = getter();
    if source.is_null() {
        return None;
    }
    let name = unsafe {
        let ptr = obs_source_get_name(source);
        if ptr.is_null() {
            None
        } else {
            Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
        }
    };
    obs_source_release(source);
    name
}

/// Stops the background threads and waits for them to actually exit.
///
/// Not "asks them to stop" — waits. OBS unloads a module without telling
/// its detached threads, and they keep calling into libobs afterwards:
/// confirmed live 2026-07-15 as a segfault inside `obs_enum_sources`'s
/// internal mutex at the moment OBS closed. Blocks briefly, at most one
/// loop iteration (~100ms).
///
/// A consumer's `obs_module_unload` must call this before doing anything
/// else, and must do its own teardown *after* it returns.
pub fn shutdown() {
    SHUTTING_DOWN.store(true, Ordering::Release);
    let handles: Vec<std::thread::JoinHandle<()>> = match THREADS.lock() {
        Ok(mut threads) => threads.drain(..).collect(),
        Err(_) => Vec::new(),
    };
    for handle in handles {
        let _ = handle.join();
    }
}
