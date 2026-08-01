//! FrameSW Companion Plugin for OBS.
//!
//! Phase 1 (validated live, 2026-07-14): proved `obs_source_add_audio_capture_callback`
//! genuinely receives real audio for Preview-only (not-yet-live) content,
//! unlike obs-websocket's own `InputVolumeMeters` (confirmed gated on
//! `obs_source_active()`, which is false for Preview-only sources under
//! Studio Mode) — confirmed via real staged FrameSW shots showing
//! `obs_source_active=false` alongside real, varying (non -100dB) peak
//! levels in OBS's own log.
//!
//! Phase 2 (this version): actually gets that data to FrameSW, via
//! obs-websocket's sanctioned third-party "vendor" event mechanism
//! (`calldata.rs`/`obs_data.rs`) — registers as vendor `"framesw"` and
//! emits a batched `audio_levels` event ~10 times/second.
//!
//! Every FFI declaration in this crate was checked against real,
//! verbatim-fetched source (`obsproject/obs-studio@master`,
//! `obsproject/obs-websocket@master`) — see each item's comment for which
//! header it came from, and `calldata.rs`'s module doc for why this ended
//! up as pure Rust FFI rather than a vendored C shim.
//!
//! Cross-platform (2026-07-15): every libobs/obs-websocket function is
//! resolved at *runtime* (`resolved_fn!`, see `platform.rs`) rather than
//! declared as a link-time `extern "C"` import against an import library
//! — the one mechanism that works identically on macOS and Windows, so
//! this crate needs zero platform-specific linker configuration. Proven
//! live on macOS (Preview-only audio levels genuinely reaching FrameSW,
//! see PROJECT_OVERVIEW.md); Windows is built and structurally correct
//! but not yet load-tested on a real Windows machine — see
//! `WINDOWS_HANDOFF.md` for exactly what still needs verifying there.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Mutex;

mod audio_tap;
mod calldata;
mod group;
mod ndi_ffi;
mod obs_data;
mod platform;

use obs_data::SourceLevel;

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
const MAX_AV_PLANES: usize = 8;

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
type ObsSourceAudioCaptureT =
    extern "C" fn(param: *mut c_void, source: *mut ObsSourceT, audio_data: *const AudioData, muted: bool);

/// `libobs/obs.h`: `void obs_enum_sources(bool (*enum_proc)(void *, obs_source_t *), void *param);`
type ObsEnumSourcesProc = extern "C" fn(param: *mut c_void, source: *mut ObsSourceT) -> bool;

/// `libobs/util/base.h`.
const LOG_INFO: c_int = 300;

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
crate::resolved_fn!(obs_source_active: extern "C" fn(*const ObsSourceT) -> bool);
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
struct ObsAudioInfo {
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
fn speaker_layout_to_channels(speakers: u32) -> u32 {
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
crate::resolved_fn!(obs_frontend_get_current_scene: extern "C" fn() -> *mut ObsSourceT);
crate::resolved_fn!(obs_frontend_get_current_preview_scene: extern "C" fn() -> *mut ObsSourceT);
crate::resolved_fn!(obs_frontend_preview_program_mode_active: extern "C" fn() -> bool);

/// `enum obs_task_type`'s first variant in libobs/obs.h — run on OBS's
/// Qt UI thread.
const OBS_TASK_UI: c_int = 0;

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
const OBS_OUTPUT_VIDEO: u32 = 1 << 0;

// `frontend/api/obs-frontend-api.h`: `config_t *obs_frontend_get_user_config(void)`
// — OBS's *live* user.ini config object, the same one OBS itself writes out
// at exit. Going through it (rather than editing user.ini on disk) is what
// makes a change stick: OBS holds these values in memory and rewrites the
// file on close, so any external edit made while OBS runs is silently
// clobbered. `obs_frontend_get_global_config` is the deprecated alias for
// the same thing and is deliberately not used here.
crate::resolved_fn!(obs_frontend_get_user_config: extern "C" fn() -> *mut ConfigT);
// `libobs/util/config-file.h`.
crate::resolved_fn!(config_get_bool: extern "C" fn(*mut ConfigT, *const c_char, *const c_char) -> bool);
crate::resolved_fn!(config_set_bool: extern "C" fn(*mut ConfigT, *const c_char, *const c_char, bool));
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
const PROJECTOR_ON_TOP_SECTION: &str = "BasicWindow";
const PROJECTOR_ON_TOP_KEY: &str = "ProjectorAlwaysOnTop";

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
crate::resolved_fn!(blog: extern "C" fn(c_int, *const c_char, ...));

fn log_line(msg: &str) {
    let Some(blog) = blog() else {
        return;
    };
    let Ok(fmt) = CString::new("[framesw] %s") else {
        return;
    };
    let msg = CString::new(msg).unwrap_or_else(|_| CString::new("[unprintable log line]").unwrap());
    blog(LOG_INFO, fmt.as_ptr(), msg.as_ptr());
}

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
fn ffi_guard<R>(entry_point: &str, fallback: R, f: impl FnOnce() -> R + std::panic::UnwindSafe) -> R {
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
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

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
static RESCAN_PAUSED: AtomicBool = AtomicBool::new(false);

/// Join handles for both background threads, so `obs_module_unload` can
/// block until they've actually exited rather than merely requesting a
/// stop and hoping — the flag alone leaves a window where a thread is
/// mid-call into libobs at the exact moment unload fires; joining closes
/// it, at the cost of unload blocking for at most one loop iteration
/// (~100ms).
static THREADS: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());

/// name -> (peak_db, obs_source_active). Updated on every audio callback
/// (cheap, in-memory only); drained by `spawn_emit_loop` at a much slower,
/// human/UI-appropriate cadence. `active` is the whole point of this
/// plugin existing — it's exactly what `InputVolumeMeters` can't report
/// for Preview-only content.
static LEVELS: Mutex<Option<HashMap<String, (f32, bool)>>> = Mutex::new(None);

extern "C" fn audio_capture_callback(
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

fn audio_capture_callback_impl(
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
    let Some(obs_source_active) = obs_source_active() else {
        return;
    };
    let name = unsafe {
        let ptr = obs_source_get_name(source);
        if ptr.is_null() {
            return;
        }
        CStr::from_ptr(ptr).to_string_lossy().into_owned()
    };
    let active = obs_source_active(source);

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
                audio_tap::forward_if_tapped(
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

    if let Ok(mut guard) = LEVELS.lock() {
        guard.get_or_insert_with(HashMap::new).insert(name, (peak_db, active));
    }
}

extern "C" fn attach_callback_enum_proc(param: *mut c_void, source: *mut ObsSourceT) -> bool {
    // Fallback `false` on a caught panic: stop this enumeration pass early
    // rather than risk repeating whatever triggered it against the rest
    // of the sources — the next 5s rescan tries again from scratch.
    ffi_guard(
        "attach_callback_enum_proc",
        false,
        std::panic::AssertUnwindSafe(|| attach_callback_enum_proc_impl(param, source)),
    )
}

fn attach_callback_enum_proc_impl(_param: *mut c_void, source: *mut ObsSourceT) -> bool {
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
const PROGRAM_PREVIEW_SCENE_NAMES: [&str; 2] = ["PGM-A", "PGM-B"];

/// Logged once per scene name the first time it's found, not every 5s
/// rescan — a direct, checkable confirmation (matching this plugin's
/// existing "check OBS's log" verification method) that the scene tap is
/// actually attached, independent of whether FrameSW's own meters are
/// showing anything (e.g. nothing audible is on Program/Preview yet).
static PGM_A_FOUND_LOGGED: AtomicBool = AtomicBool::new(false);
static PGM_B_FOUND_LOGGED: AtomicBool = AtomicBool::new(false);

/// Looks up FrameSW's two fixed scene names directly (`obs_enum_sources`
/// can't reach them — confirmed it filters to `OBS_SOURCE_TYPE_INPUT`
/// only, excluding scenes entirely) and attaches the same audio capture
/// callback used for regular sources. The looked-up reference is released
/// immediately after attaching — the callback registration itself doesn't
/// need the reference held past this call, only the scene's own existence
/// for as long as OBS keeps it in the scene collection. Harmless to call
/// repeatedly (same re-attach-is-idempotent-enough reasoning as
/// `attach_callback_enum_proc`'s own periodic re-invocation); a no-op
/// until FrameSW has actually connected and created these scenes.
fn attach_scene_audio_taps() {
    let (Some(obs_get_source_by_name), Some(obs_source_add_audio_capture_callback), Some(obs_source_release)) = (
        obs_get_source_by_name(),
        obs_source_add_audio_capture_callback(),
        obs_source_release(),
    ) else {
        return;
    };
    for (name, logged) in PROGRAM_PREVIEW_SCENE_NAMES
        .iter()
        .zip([&PGM_A_FOUND_LOGGED, &PGM_B_FOUND_LOGGED])
    {
        let Ok(cname) = CString::new(*name) else {
            continue;
        };
        let source = obs_get_source_by_name(cname.as_ptr());
        if source.is_null() {
            continue; // not created yet (or this OBS session isn't a FrameSW show)
        }
        // Same remove-then-add idempotency as attach_callback_enum_proc.
        if let Some(remove) = obs_source_remove_audio_capture_callback() {
            remove(source, audio_capture_callback, std::ptr::null_mut());
        }
        obs_source_add_audio_capture_callback(source, audio_capture_callback, std::ptr::null_mut());
        obs_source_release(source);
        if !logged.swap(true, Ordering::AcqRel) {
            log_line(&format!("attached real audio tap to scene '{name}'"));
        }
    }
}

/// Periodically re-enumerates and (re-)attaches the callback, rather than
/// hooking libobs's `source_create` signal — deliberately the simplest
/// thing that could prove the hypothesis, not the final design. Remaining
/// rough edge: sources created between scans (this fires every 5s) aren't
/// instrumented until the next scan. (The former rough edge — duplicate
/// attachment growing libobs's callback list unboundedly, confirmed real
/// 2026-07-19: libobs's add is a bare `da_push_back` — is closed by the
/// remove-then-add pattern in both attach paths below.)
fn spawn_periodic_rescan() {
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

// ---------------------------------------------------------------------
// SPIKE (throwaway): a single vendor *request* proving the plugin can
// create/manage real OBS groups (`group.rs`) — commanded from FrameSW
// via obs-websocket's `CallVendorRequest`, since group manipulation has
// no obs-websocket request of its own. Not wired into shot-creation.
// ---------------------------------------------------------------------

extern "C" fn handle_manage_group(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_manage_group",
        (),
        std::panic::AssertUnwindSafe(|| {
            handle_manage_group_impl(request_data, response_data, priv_data)
        }),
    );
}

/// Request shape: `{"action": "create"|"add_item"|"remove_item"|"lock"|
/// "unlock", "scene": "PGM-A", "group": "Layer 1", "source": "..."}`
/// (`source` only meaningful for `add_item`/`remove_item`). Response:
/// `{"ok": true}` or `{"ok": false, "error": "..."}` — deliberately the
/// simplest shape that lets a demo caller tell success from failure and
/// read *why*, not a designed-for-production API.
fn handle_manage_group_impl(
    request_data: *mut c_void,
    response_data: *mut c_void,
    _priv_data: *mut c_void,
) {
    let request_data = obs_data::from_void(request_data);
    let response_data = obs_data::from_void(response_data);

    let action = obs_data::get_string(request_data, "action").unwrap_or_default();
    let scene = obs_data::get_string(request_data, "scene").unwrap_or_default();
    let group = obs_data::get_string(request_data, "group").unwrap_or_default();
    let source = obs_data::get_string(request_data, "source").unwrap_or_default();

    let result = match action.as_str() {
        "create" => group::create_group(&scene, &group),
        "add_item" => group::add_item_to_group(&scene, &group, &source),
        "remove_item" => group::remove_item_from_group(&scene, &group, &source),
        "lock" => group::set_group_locked(&scene, &group, true),
        "unlock" => group::set_group_locked(&scene, &group, false),
        other => Err(format!("unknown action '{other}'")),
    };

    match result {
        Ok(()) => obs_data::set_bool(response_data, "ok", true),
        Err(e) => {
            log_line(&format!("manage_group action='{action}' failed: {e}"));
            obs_data::set_bool(response_data, "ok", false);
            obs_data::set_string(response_data, "error", &e);
        }
    }
}

// ---------------------------------------------------------------------
// PROMPT 17: monitor-speaker audio taps (`audio_tap.rs`/`ndi_ffi.rs`) —
// three vendor requests mirroring `manage_group`'s own shape/pattern
// (FrameSW's request, this plugin's response, `{"ok": bool, "error":
// string}` on failure). All three are idempotent per `audio_tap.rs`'s
// own doc comments — safe for FrameSW to call repeatedly, e.g. on every
// OBS reconnect's reconciliation pass, without needing to track "did I
// already ask for this" on the app side.
// ---------------------------------------------------------------------

extern "C" fn handle_start_audio_tap(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_start_audio_tap",
        (),
        std::panic::AssertUnwindSafe(|| {
            handle_start_audio_tap_impl(request_data, response_data, priv_data)
        }),
    );
}

/// Request: `{"source_name": "shot-abc123", "bus_id": "1"}`. Response:
/// `{"ok": true}` or `{"ok": false, "error": "..."}`.
fn handle_start_audio_tap_impl(
    request_data: *mut c_void,
    response_data: *mut c_void,
    _priv_data: *mut c_void,
) {
    let request_data = obs_data::from_void(request_data);
    let response_data = obs_data::from_void(response_data);
    let source_name = obs_data::get_string(request_data, "source_name").unwrap_or_default();
    let bus_id = obs_data::get_string(request_data, "bus_id").unwrap_or_default();

    match audio_tap::start_audio_tap(&source_name, &bus_id) {
        Ok(()) => obs_data::set_bool(response_data, "ok", true),
        Err(e) => {
            log_line(&format!("start_audio_tap bus_id='{bus_id}' failed: {e}"));
            obs_data::set_bool(response_data, "ok", false);
            obs_data::set_string(response_data, "error", &e);
        }
    }
}

extern "C" fn handle_stop_audio_tap(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_stop_audio_tap",
        (),
        std::panic::AssertUnwindSafe(|| {
            handle_stop_audio_tap_impl(request_data, response_data, priv_data)
        }),
    );
}

/// Request: `{"bus_id": "1"}`. Response: always `{"ok": true}` — stopping
/// an already-inactive bus is a defined no-op, per `audio_tap::
/// stop_audio_tap`'s own doc comment, not an error condition.
fn handle_stop_audio_tap_impl(
    request_data: *mut c_void,
    response_data: *mut c_void,
    _priv_data: *mut c_void,
) {
    let request_data = obs_data::from_void(request_data);
    let response_data = obs_data::from_void(response_data);
    let bus_id = obs_data::get_string(request_data, "bus_id").unwrap_or_default();
    audio_tap::stop_audio_tap(&bus_id);
    obs_data::set_bool(response_data, "ok", true);
}

extern "C" fn handle_create_scene(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_create_scene",
        (),
        std::panic::AssertUnwindSafe(|| handle_create_scene_impl(request_data, response_data, priv_data)),
    );
}

/// Request: `{"name": "PGM-A"}`. Response: `{"ok": true}` (whether the
/// scene was just created or already existed — idempotent, matching this
/// plugin's other creation-adjacent handlers) or `{"ok": false, "error":
/// "..."}` if the required libobs symbols aren't resolvable (shouldn't
/// happen in practice; this plugin only loads inside a real OBS process).
/// See `obs_scene_create`'s own comment for why this exists at all.
fn handle_create_scene_impl(
    request_data: *mut c_void,
    response_data: *mut c_void,
    _priv_data: *mut c_void,
) {
    let request_data = obs_data::from_void(request_data);
    let response_data = obs_data::from_void(response_data);
    let name = obs_data::get_string(request_data, "name").unwrap_or_default();

    let (Some(obs_get_source_by_name), Some(obs_source_release)) =
        (obs_get_source_by_name(), obs_source_release())
    else {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "required libobs symbols unavailable");
        return;
    };
    let Ok(cname) = CString::new(name.as_str()) else {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "invalid scene name");
        return;
    };

    // Idempotent: if it's already there (a previous connect already made
    // it, or a prior call to this same request), do nothing further.
    let existing = obs_get_source_by_name(cname.as_ptr());
    if !existing.is_null() {
        obs_source_release(existing);
        obs_data::set_bool(response_data, "ok", true);
        return;
    }

    let (Some(obs_scene_create), Some(obs_scene_release)) = (obs_scene_create(), obs_scene_release())
    else {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "required libobs symbols unavailable");
        return;
    };
    let scene = obs_scene_create(cname.as_ptr());
    if scene.is_null() {
        log_line(&format!("create_scene '{name}' failed: obs_scene_create returned null"));
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "obs_scene_create returned null");
        return;
    }
    // Releases only this handler's own strong reference — the scene
    // itself stays registered in the current scene collection, same
    // "released immediately after attaching" reasoning already used for
    // audio taps elsewhere in this file.
    obs_scene_release(scene);
    log_line(&format!("created scene '{name}' natively (obs_scene_create)"));
    obs_data::set_bool(response_data, "ok", true);
}

/// Filled in on OBS's own UI thread by `read_current_scenes_on_ui_thread`,
/// read back by `handle_get_current_scenes_impl` once `obs_queue_task`'s
/// blocking wait returns.
#[derive(Default)]
struct CurrentScenes {
    /// False if the task never ran at all — `obs_queue_task` logs and
    /// returns without running anything when libobs has no UI task handler
    /// registered. Keeps "OBS genuinely has no program scene" (a real,
    /// reportable answer) distinguishable from "we never got to look"
    /// (an error the caller must not mistake for the former).
    ran: bool,
    studio_mode: bool,
    program: Option<String>,
    preview: Option<String>,
}

/// Resolves one of the two nullable frontend scene getters to a name,
/// releasing the strong reference it hands back. `None` covers both "OBS
/// has no such scene right now" and "the symbol didn't resolve"; the
/// caller separates those via `CurrentScenes::ran`.
fn frontend_scene_name(getter: Option<extern "C" fn() -> *mut ObsSourceT>) -> Option<String> {
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

/// Runs on OBS's UI thread, via `obs_queue_task(OBS_TASK_UI, ...)`.
/// `param` is a `*mut CurrentScenes` owned by the (blocked) caller for the
/// whole duration of the task, so writing through it here is sound.
extern "C" fn read_current_scenes_on_ui_thread(param: *mut c_void) {
    ffi_guard(
        "read_current_scenes_on_ui_thread",
        (),
        std::panic::AssertUnwindSafe(|| {
            if param.is_null() {
                return;
            }
            let out = unsafe { &mut *param.cast::<CurrentScenes>() };
            out.ran = true;
            out.studio_mode =
                obs_frontend_preview_program_mode_active().is_some_and(|active| active());
            out.program = frontend_scene_name(obs_frontend_get_current_scene());
            out.preview = frontend_scene_name(obs_frontend_get_current_preview_scene());
        }),
    );
}

extern "C" fn handle_get_current_scenes(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_get_current_scenes",
        (),
        std::panic::AssertUnwindSafe(|| {
            handle_get_current_scenes_impl(request_data, response_data, priv_data)
        }),
    );
}

/// Request: `{}`. Response: `{"ok": true, "studio_mode": bool}` plus
/// `"program"`/`"preview"` *only when OBS actually has one* — an absent
/// key means "no current scene", which is a normal state, not an error.
///
/// Exists because obs-websocket's own `GetCurrentProgramScene` and
/// `GetCurrentPreviewScene` handlers pass `obs_frontend_get_current_scene()`
/// / `..._preview_scene()` straight into `obs_source_get_name()` with no
/// null check, then assign the result to a `json` value — which calls
/// `strlen(nullptr)` and takes the whole OBS process down with SIGSEGV.
/// Confirmed by symbolicating 23 identical crash reports against the
/// shipped OBS 32.1.2 `obs-websocket` binary: the faulting frame is
/// `_platform_strlen` inside the `GetCurrentProgramScene` handler, and the
/// same missing check is present in the preview handler. Reproduces 100%
/// of the time (not intermittently — this is a null deref, not a race)
/// whenever OBS has no current program scene, e.g. right after the scene
/// that *was* program is deleted.
///
/// So this handler does the two things obs-websocket's don't: null-check,
/// and marshal onto the UI thread rather than reading the frontend's
/// Qt-owned state from an obs-websocket worker thread.
fn handle_get_current_scenes_impl(
    _request_data: *mut c_void,
    response_data: *mut c_void,
    _priv_data: *mut c_void,
) {
    let response_data = obs_data::from_void(response_data);
    let Some(obs_queue_task) = obs_queue_task() else {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "obs_queue_task unavailable");
        return;
    };

    let mut scenes = CurrentScenes::default();
    obs_queue_task(
        OBS_TASK_UI,
        read_current_scenes_on_ui_thread,
        (&mut scenes as *mut CurrentScenes).cast(),
        true,
    );

    if !scenes.ran {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "UI task handler unavailable");
        return;
    }
    obs_data::set_bool(response_data, "ok", true);
    obs_data::set_bool(response_data, "studio_mode", scenes.studio_mode);
    if let Some(program) = &scenes.program {
        obs_data::set_string(response_data, "program", program);
    }
    if let Some(preview) = &scenes.preview {
        obs_data::set_string(response_data, "preview", preview);
    }
}

/// Collects `{name, kind}` for every currently-active video output.
/// `*mut c_void` param is a `&mut Vec<obs_data::NamedKind>` owned by
/// `handle_list_video_outputs_impl` for the whole enumeration.
extern "C" fn collect_video_output(param: *mut c_void, output: *mut ObsOutputT) -> bool {
    ffi_guard(
        "collect_video_output",
        true,
        std::panic::AssertUnwindSafe(|| {
            if param.is_null() || output.is_null() {
                return true;
            }
            let out = unsafe { &mut *param.cast::<Vec<obs_data::NamedKind>>() };
            let active = obs_output_active().is_some_and(|active| active(output));
            let flags = obs_output_get_flags().map_or(0, |get_flags| get_flags(output));
            if !active || flags & OBS_OUTPUT_VIDEO == 0 {
                return true;
            }
            let read = |getter: Option<extern "C" fn(*const ObsOutputT) -> *const c_char>| {
                getter.and_then(|getter| unsafe {
                    let ptr = getter(output);
                    if ptr.is_null() {
                        None
                    } else {
                        Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
                    }
                })
            };
            if let Some(name) = read(obs_output_get_name()) {
                out.push(obs_data::NamedKind {
                    name,
                    kind: read(obs_output_get_id()).unwrap_or_default(),
                });
            }
            true
        }),
    )
}

extern "C" fn handle_list_video_outputs(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_list_video_outputs",
        (),
        std::panic::AssertUnwindSafe(|| {
            handle_list_video_outputs_impl(request_data, response_data, priv_data)
        }),
    );
}

/// Request: `{}`. Response: `{"ok": true, "outputs": [{"name", "kind"}]}`
/// listing every *active video* output (the generic "is OBS's video
/// pipeline in use" signal, covering plugin-added outputs like NDI's
/// Main/Preview Output that the well-known streaming/recording checks
/// miss).
///
/// Exists because obs-websocket's `GetOutputList` puts `outputWidth`/
/// `outputHeight` on every entry, and `obs_output_get_width` resolves to
/// `obs_encoder_get_width(output->video_encoders[i])` or
/// `video_output_get_width(output->video)` — objects owned by the video
/// subsystem, which `obs_reset_video` destroys. `obs_enum_outputs` holds
/// libobs's outputs mutex, so the `obs_output_t` itself can't go away
/// mid-enumeration, but that mutex says nothing about the encoder/video_t
/// hanging off it. Changing video settings (in OBS's own Settings dialog
/// *or* FrameSW's Preflight) while a client polls `GetOutputList` therefore
/// dereferences freed memory and takes OBS down — confirmed from a real
/// crash report: SIGSEGV in `obs_output_get_width`, called from
/// obs-websocket's output-list handler on a worker thread.
///
/// So this reads only what the enumeration mutex actually protects:
/// `obs_output_get_name`/`_get_id`/`_active`/`_get_flags` all touch just
/// the `obs_output` struct. Width/height are never read at all — FrameSW
/// only ever needed name and kind.
fn handle_list_video_outputs_impl(
    _request_data: *mut c_void,
    response_data: *mut c_void,
    _priv_data: *mut c_void,
) {
    let response_data = obs_data::from_void(response_data);
    let Some(obs_enum_outputs) = obs_enum_outputs() else {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "obs_enum_outputs unavailable");
        return;
    };
    let mut outputs: Vec<obs_data::NamedKind> = Vec::new();
    obs_enum_outputs(
        collect_video_output,
        (&mut outputs as *mut Vec<obs_data::NamedKind>).cast(),
    );
    if !obs_data::set_pair_array(response_data, "outputs", &outputs) {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "obs_data array symbols unavailable");
        return;
    }
    obs_data::set_bool(response_data, "ok", true);
}

/// Carries the `ensure_profile` request across the UI-thread hop.
struct EnsureProfile {
    ran: bool,
    /// Profile to end up on.
    wanted: String,
    /// Whatever was current before — `None` if we were already on
    /// `wanted`, so the caller can never record its own profile as the
    /// user's.
    previous: Option<String>,
    /// True when the profile had to be created (by duplicating the user's).
    created: bool,
    /// Whether we genuinely ended up on `wanted`.
    switched: bool,
}

/// Reads `obs_frontend_get_current_profile` into an owned `String`,
/// freeing libobs's `bstrdup`'d buffer.
fn current_profile_name() -> Option<String> {
    let get = obs_frontend_get_current_profile()?;
    let bfree = bfree()?;
    let ptr = get();
    if ptr.is_null() {
        return None;
    }
    let name = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
    bfree(ptr.cast());
    Some(name)
}

/// Runs on OBS's UI thread. Every step here is a Qt-touching frontend call
/// that has no business running anywhere else — see the profile FFI block
/// above.
extern "C" fn ensure_profile_on_ui_thread(param: *mut c_void) {
    ffi_guard(
        "ensure_profile_on_ui_thread",
        (),
        std::panic::AssertUnwindSafe(|| {
            if param.is_null() {
                return;
            }
            let state = unsafe { &mut *param.cast::<EnsureProfile>() };
            let Ok(wanted) = CString::new(state.wanted.as_str()) else {
                return;
            };
            let Some(current) = current_profile_name() else {
                return;
            };
            state.ran = true;

            if current == state.wanted {
                state.switched = true;
                return;
            }
            state.previous = Some(current);

            // Switching to a name that doesn't exist is a defined no-op
            // (the menu walk matches nothing), so this doubles as the
            // existence check.
            if let Some(set_current) = obs_frontend_set_current_profile() {
                set_current(wanted.as_ptr());
            }
            if current_profile_name().as_deref() == Some(state.wanted.as_str()) {
                state.switched = true;
                return;
            }

            // Didn't exist. Duplicating the *current* profile carries the
            // user's whole configuration across in one atomic step —
            // encoders, audio, recording path, stream key — rather than the
            // handful of fields a caller could think to copy by hand, and
            // with no second thread writing config behind OBS's back. It
            // switches to the duplicate as part of the same operation.
            let Some(duplicate) = obs_frontend_duplicate_profile() else {
                return;
            };
            duplicate(wanted.as_ptr());
            state.created = true;
            state.switched = current_profile_name().as_deref() == Some(state.wanted.as_str());
        }),
    );
}

extern "C" fn handle_ensure_profile(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_ensure_profile",
        (),
        std::panic::AssertUnwindSafe(|| {
            handle_ensure_profile_impl(request_data, response_data, priv_data)
        }),
    );
}

/// Request: `{"name": "FrameSW"}`. Response:
/// `{"ok": true, "switched": bool, "created": bool}` plus `"previous"`
/// when a switch genuinely happened.
///
/// Exists because doing this over obs-websocket crashes OBS. Reproduced
/// live 2026-08-01: `CreateProfile` followed by seeding the new profile's
/// settings killed OBS 1.5s into launch — SIGSEGV in `strcmp(NULL, ...)`
/// on obs-websocket's pooled thread while OBS's main thread was inside
/// `config_save_safe`, leaving a half-written 23-byte basic.ini. libobs's
/// config layer does not tolerate a second thread writing config while OBS
/// saves it, and obs-websocket's profile requests run on a worker thread.
///
/// Doing the whole thing on the UI thread removes that race rather than
/// narrowing it: no delay, no retry, and the duplicate-then-switch happens
/// synchronously in the same task.
fn handle_ensure_profile_impl(
    request_data: *mut c_void,
    response_data: *mut c_void,
    _priv_data: *mut c_void,
) {
    let request_data = obs_data::from_void(request_data);
    let response_data = obs_data::from_void(response_data);
    let name = obs_data::get_string(request_data, "name").unwrap_or_default();
    if name.is_empty() {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "missing profile name");
        return;
    }
    let Some(obs_queue_task) = obs_queue_task() else {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "obs_queue_task unavailable");
        return;
    };

    let mut state = EnsureProfile {
        ran: false,
        wanted: name,
        previous: None,
        created: false,
        switched: false,
    };
    obs_queue_task(
        OBS_TASK_UI,
        ensure_profile_on_ui_thread,
        (&mut state as *mut EnsureProfile).cast(),
        true,
    );

    if !state.ran {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "profile frontend API unavailable");
        return;
    }
    if state.created {
        log_line(&format!(
            "created profile '{}' by duplicating the active one",
            state.wanted
        ));
    }
    obs_data::set_bool(response_data, "ok", true);
    obs_data::set_bool(response_data, "switched", state.switched);
    obs_data::set_bool(response_data, "created", state.created);
    if let Some(previous) = &state.previous {
        obs_data::set_string(response_data, "previous", previous);
    }
}

/// Carries the projector-on-top request across the UI-thread hop. `set`
/// is `None` for a pure read.
struct ProjectorOnTop {
    ran: bool,
    set: Option<bool>,
    value: bool,
}

/// Runs on OBS's UI thread via `obs_queue_task(OBS_TASK_UI, ...)` — this
/// touches the frontend's own config object, so it belongs on the thread
/// that owns it, same reasoning as `read_current_scenes_on_ui_thread`.
extern "C" fn projector_on_top_on_ui_thread(param: *mut c_void) {
    ffi_guard(
        "projector_on_top_on_ui_thread",
        (),
        std::panic::AssertUnwindSafe(|| {
            if param.is_null() {
                return;
            }
            let state = unsafe { &mut *param.cast::<ProjectorOnTop>() };
            let (Some(get_user_config), Some(config_get_bool)) =
                (obs_frontend_get_user_config(), config_get_bool())
            else {
                return;
            };
            let config = get_user_config();
            if config.is_null() {
                return;
            }
            let (Ok(section), Ok(key)) = (
                CString::new(PROJECTOR_ON_TOP_SECTION),
                CString::new(PROJECTOR_ON_TOP_KEY),
            ) else {
                return;
            };

            if let Some(desired) = state.set {
                let Some(config_set_bool) = config_set_bool() else {
                    return;
                };
                config_set_bool(config, section.as_ptr(), key.as_ptr(), desired);
                // Persist now rather than relying on OBS's write-at-exit:
                // a crash between here and shutdown would otherwise lose
                // the change silently. `config_save_safe`'s temp/backup
                // extensions match how OBS saves this file itself.
                if let Some(config_save_safe) = config_save_safe() {
                    let (Ok(tmp), Ok(bak)) = (CString::new("tmp"), CString::new("bak")) else {
                        return;
                    };
                    config_save_safe(config, tmp.as_ptr(), bak.as_ptr());
                }
            }

            state.value = config_get_bool(config, section.as_ptr(), key.as_ptr());
            state.ran = true;
        }),
    );
}

extern "C" fn handle_projector_on_top(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_projector_on_top",
        (),
        std::panic::AssertUnwindSafe(|| {
            handle_projector_on_top_impl(request_data, response_data, priv_data)
        }),
    );
}

/// Request: `{}` to read, or `{"enabled": true|false}` to set.
/// Response: `{"ok": true, "enabled": bool}` — always the value as it
/// stands *after* any set, so the caller never has to assume its write
/// landed.
///
/// Reads/writes OBS Settings -> General -> Projectors -> "Make projectors
/// always on top". See `PROJECTOR_ON_TOP_SECTION` for why this lives in
/// the plugin at all: it's in user.ini, which obs-websocket exposes no
/// request for, and which cannot be edited on disk while OBS is running
/// without being clobbered at exit.
///
/// Note the change applies to projectors opened *after* it — OBS reads
/// this when it creates a projector window, so any already-open projector
/// keeps its current always-on-top state until reopened.
fn handle_projector_on_top_impl(
    request_data: *mut c_void,
    response_data: *mut c_void,
    _priv_data: *mut c_void,
) {
    let request_data = obs_data::from_void(request_data);
    let response_data = obs_data::from_void(response_data);
    let Some(obs_queue_task) = obs_queue_task() else {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "obs_queue_task unavailable");
        return;
    };

    let mut state = ProjectorOnTop {
        ran: false,
        set: obs_data::get_optional_bool(request_data, "enabled"),
        value: false,
    };
    obs_queue_task(
        OBS_TASK_UI,
        projector_on_top_on_ui_thread,
        (&mut state as *mut ProjectorOnTop).cast(),
        true,
    );

    if !state.ran {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "user config unavailable");
        return;
    }
    obs_data::set_bool(response_data, "ok", true);
    obs_data::set_bool(response_data, "enabled", state.value);
}

extern "C" fn handle_pause_rescan(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_pause_rescan",
        (),
        std::panic::AssertUnwindSafe(|| handle_pause_rescan_impl(request_data, response_data, priv_data)),
    );
}

/// Request: `{}`. Response: always `{"ok": true}`. See `RESCAN_PAUSED`'s
/// doc comment for why this exists — FrameSW sends this before recreating
/// any of its own scenes, and `resume_rescan` after.
fn handle_pause_rescan_impl(
    _request_data: *mut c_void,
    response_data: *mut c_void,
    _priv_data: *mut c_void,
) {
    let response_data = obs_data::from_void(response_data);
    RESCAN_PAUSED.store(true, Ordering::Release);
    obs_data::set_bool(response_data, "ok", true);
}

extern "C" fn handle_resume_rescan(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_resume_rescan",
        (),
        std::panic::AssertUnwindSafe(|| handle_resume_rescan_impl(request_data, response_data, priv_data)),
    );
}

/// Request: `{}`. Response: always `{"ok": true}`. Counterpart to
/// `handle_pause_rescan_impl` — see `RESCAN_PAUSED`'s doc comment.
fn handle_resume_rescan_impl(
    _request_data: *mut c_void,
    response_data: *mut c_void,
    _priv_data: *mut c_void,
) {
    let response_data = obs_data::from_void(response_data);
    RESCAN_PAUSED.store(false, Ordering::Release);
    obs_data::set_bool(response_data, "ok", true);
}

extern "C" fn handle_tap_status(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_tap_status",
        (),
        std::panic::AssertUnwindSafe(|| handle_tap_status_impl(request_data, response_data, priv_data)),
    );
}

/// Request: `{"bus_id": "1"}`. Response: `{"ok": true, "active": bool,
/// "source_name": string}` — `source_name` is `""` when `active` is
/// `false`, mirroring `obs_data_get_string`'s own "missing means empty
/// string" convention used throughout this plugin.
fn handle_tap_status_impl(
    request_data: *mut c_void,
    response_data: *mut c_void,
    _priv_data: *mut c_void,
) {
    let request_data = obs_data::from_void(request_data);
    let response_data = obs_data::from_void(response_data);
    let bus_id = obs_data::get_string(request_data, "bus_id").unwrap_or_default();
    let status = audio_tap::tap_status(&bus_id);
    obs_data::set_bool(response_data, "ok", true);
    obs_data::set_bool(response_data, "active", status.is_some());
    obs_data::set_string(response_data, "source_name", &status.unwrap_or_default());
}

extern "C" fn handle_set_mix_sources(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_set_mix_sources",
        (),
        std::panic::AssertUnwindSafe(|| {
            handle_set_mix_sources_impl(request_data, response_data, priv_data)
        }),
    );
}

/// Request: `{"bus_id": "preview", "source_names": "shot-abc,shot-def"}`
/// — comma-joined rather than a real `obs_data_array_t` of objects
/// (which is what OBS's own array type actually holds; there's no plain
/// "array of strings" primitive in this API without wrapping each name
/// in its own object). A single delimited string keeps this at zero new
/// dependencies and no JSON parsing — safe because FrameSW's own
/// `input_name`s are internally generated (`shot-<uuid>`) and a bare
/// OBS scene/source name never contains a comma in practice. Response:
/// `{"ok": true}` or `{"ok": false, "error": "..."}`.
fn handle_set_mix_sources_impl(
    request_data: *mut c_void,
    response_data: *mut c_void,
    _priv_data: *mut c_void,
) {
    let request_data = obs_data::from_void(request_data);
    let response_data = obs_data::from_void(response_data);
    let bus_id = obs_data::get_string(request_data, "bus_id").unwrap_or_default();
    let source_names_raw = obs_data::get_string(request_data, "source_names").unwrap_or_default();
    let source_names: Vec<String> = source_names_raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();

    match audio_tap::set_mix_sources(&bus_id, &source_names) {
        Ok(()) => obs_data::set_bool(response_data, "ok", true),
        Err(e) => {
            log_line(&format!("set_mix_sources bus_id='{bus_id}' failed: {e}"));
            obs_data::set_bool(response_data, "ok", false);
            obs_data::set_string(response_data, "error", &e);
        }
    }
}

extern "C" fn handle_stop_mix_bus(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_stop_mix_bus",
        (),
        std::panic::AssertUnwindSafe(|| {
            handle_stop_mix_bus_impl(request_data, response_data, priv_data)
        }),
    );
}

/// Request: `{"bus_id": "preview"}`. Response: always `{"ok": true}` —
/// same "stopping an inactive bus is a defined no-op" shape as
/// `stop_audio_tap`.
fn handle_stop_mix_bus_impl(
    request_data: *mut c_void,
    response_data: *mut c_void,
    _priv_data: *mut c_void,
) {
    let request_data = obs_data::from_void(request_data);
    let response_data = obs_data::from_void(response_data);
    let bus_id = obs_data::get_string(request_data, "bus_id").unwrap_or_default();
    audio_tap::stop_mix_bus(&bus_id);
    obs_data::set_bool(response_data, "ok", true);
}

extern "C" fn handle_mix_status(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_mix_status",
        (),
        std::panic::AssertUnwindSafe(|| handle_mix_status_impl(request_data, response_data, priv_data)),
    );
}

/// Request: `{"bus_id": "preview"}`. Response: `{"ok": true, "active":
/// bool, "source_names": "shot-a,shot-b"}` — same comma-joined shape
/// `set_mix_sources` accepts, empty string when `active` is `false` or
/// nothing is currently contributing.
fn handle_mix_status_impl(
    request_data: *mut c_void,
    response_data: *mut c_void,
    _priv_data: *mut c_void,
) {
    let request_data = obs_data::from_void(request_data);
    let response_data = obs_data::from_void(response_data);
    let bus_id = obs_data::get_string(request_data, "bus_id").unwrap_or_default();
    let sources = audio_tap::mix_bus_sources(&bus_id);
    obs_data::set_bool(response_data, "ok", true);
    obs_data::set_bool(response_data, "active", sources.is_some());
    let joined = sources.map(|s| s.into_iter().collect::<Vec<_>>().join(",")).unwrap_or_default();
    obs_data::set_string(response_data, "source_names", &joined);
}

// ---------------------------------------------------------------------
// obs-websocket vendor wiring — registers as vendor "framesw" and
// forwards whatever `audio_capture_callback` has accumulated, at a
// steady ~10Hz, as a batched `audio_levels` event.
// ---------------------------------------------------------------------

/// `*mut c_void` rather than a typed handle — `obs_websocket_vendor` is
/// itself just `typedef void *obs_websocket_vendor;` in the real header,
/// an already-opaque handle libobs-websocket hands back, not something
/// this plugin interprets.
static VENDOR: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

const EMIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

fn spawn_emit_loop() {
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

static mut MODULE_POINTER: *mut ObsModuleT = std::ptr::null_mut();

#[no_mangle]
pub extern "C" fn obs_module_set_pointer(module: *mut ObsModuleT) {
    ffi_guard(
        "obs_module_set_pointer",
        (),
        std::panic::AssertUnwindSafe(|| unsafe {
            MODULE_POINTER = module;
        }),
    );
}

#[no_mangle]
pub extern "C" fn obs_current_module() -> *mut ObsModuleT {
    ffi_guard("obs_current_module", std::ptr::null_mut(), || unsafe { MODULE_POINTER })
}

#[no_mangle]
pub extern "C" fn obs_module_ver() -> u32 {
    ffi_guard("obs_module_ver", 30u32 << 24, obs_module_ver_impl)
}

fn obs_module_ver_impl() -> u32 {
    // MAKE_SEMANTIC_VERSION(30, 0, 0) — deliberately conservative, *not*
    // whatever obs-studio@master currently reports. Live-tested: claiming
    // 32.2.0 (master's current LIBOBS_API_VER, at the time this was first
    // written) against a real OBS 32.1.2 install produced a **hard
    // rejection**, not just a logged warning as originally assumed —
    // OBS's log was explicit: "compiled with newer libobs 32.2". OBS only
    // refuses a module claiming an API *newer* than its own; claiming
    // something safely older is fine and doesn't gate anything, since
    // every function this plugin calls (`obs_enum_sources`,
    // `obs_source_add_audio_capture_callback`, etc.) has been stable
    // libobs API for years, well before version 30. Only raise this if a
    // future phase starts depending on something genuinely
    // version-gated — don't just chase whatever `master` reports.
    30u32 << 24
}

#[no_mangle]
pub extern "C" fn obs_module_load() -> bool {
    ffi_guard("obs_module_load", false, || {
        log_line("loaded — watching for audio on Preview-only sources");
        spawn_periodic_rescan();
        true
    })
}

/// Called once, after every module (including obs-websocket, if
/// installed) has finished `obs_module_load` — the obs-websocket header's
/// own documented requirement for vendor registration, guaranteeing no
/// load-order race regardless of which order OBS happens to load modules
/// in.
#[no_mangle]
pub extern "C" fn obs_module_post_load() {
    ffi_guard("obs_module_post_load", (), || {
        let vendor = calldata::register_vendor("framesw");
        if vendor.is_null() {
            log_line("obs-websocket not installed/loaded — audio levels will only reach OBS's own log, not FrameSW");
            return;
        }
        VENDOR.store(vendor, Ordering::Release);
        log_line("registered as obs-websocket vendor \"framesw\" — forwarding audio levels");
        spawn_emit_loop();
        // SPIKE: see `handle_manage_group`'s doc comment.
        if calldata::register_request(vendor, "manage_group", handle_manage_group) {
            log_line("registered vendor request \"manage_group\" (spike — group management)");
        } else {
            log_line("failed to register vendor request \"manage_group\"");
        }
        for (request_type, callback) in [
            ("start_audio_tap", handle_start_audio_tap as calldata::RequestCallbackFn),
            ("stop_audio_tap", handle_stop_audio_tap as calldata::RequestCallbackFn),
            ("create_scene", handle_create_scene as calldata::RequestCallbackFn),
            ("get_current_scenes", handle_get_current_scenes as calldata::RequestCallbackFn),
            ("list_video_outputs", handle_list_video_outputs as calldata::RequestCallbackFn),
            ("projector_on_top", handle_projector_on_top as calldata::RequestCallbackFn),
            ("ensure_profile", handle_ensure_profile as calldata::RequestCallbackFn),
            ("pause_rescan", handle_pause_rescan as calldata::RequestCallbackFn),
            ("resume_rescan", handle_resume_rescan as calldata::RequestCallbackFn),
            ("tap_status", handle_tap_status as calldata::RequestCallbackFn),
            ("set_mix_sources", handle_set_mix_sources as calldata::RequestCallbackFn),
            ("stop_mix_bus", handle_stop_mix_bus as calldata::RequestCallbackFn),
            ("mix_status", handle_mix_status as calldata::RequestCallbackFn),
        ] {
            if calldata::register_request(vendor, request_type, callback) {
                log_line(&format!("registered vendor request \"{request_type}\""));
            } else {
                log_line(&format!("failed to register vendor request \"{request_type}\""));
            }
        }
    })
}

/// `libobs/obs-module.h`'s counterpart to `obs_module_load` — OBS calls
/// this during shutdown, before it starts tearing down core state
/// (sources list, obs-websocket's vendor registry, etc.), and waits for it
/// to return before proceeding. Without this export at all, OBS has no way
/// to tell this plugin's detached background threads to stop, and they
/// keep calling into libobs indefinitely — confirmed live, 2026-07-15:
/// segfault inside `obs_enum_sources`'s internal mutex lock at the moment
/// OBS was closed. Blocks (briefly — at most one loop iteration, ~100ms)
/// until both threads have actually exited, not just been asked to.
#[no_mangle]
pub extern "C" fn obs_module_unload() {
    ffi_guard("obs_module_unload", (), || {
        SHUTTING_DOWN.store(true, Ordering::Release);
        let handles: Vec<std::thread::JoinHandle<()>> = match THREADS.lock() {
            Ok(mut threads) => threads.drain(..).collect(),
            Err(_) => Vec::new(),
        };
        for handle in handles {
            let _ = handle.join();
        }
        // No active monitor tap's NDI sender should outlive the plugin.
        audio_tap::stop_all();
        log_line("unloaded — background threads stopped cleanly");
    })
}
