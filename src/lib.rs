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

mod calldata;
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
// `libobs/obs.h`: "Gets a source by its name. Increments the source
// reference counter, use obs_source_release to release it when complete."
// Needed because `obs_enum_sources` (confirmed against the real
// `obs.c` — `if (s->info.type == OBS_SOURCE_TYPE_INPUT ...)`)
// deliberately excludes scenes (`OBS_SOURCE_TYPE_SCENE`) entirely — the
// only way to reach FrameSW's fixed-name Program/Preview scenes
// ("PGM-A"/"PGM-B") is a direct name lookup, not the general rescan.
crate::resolved_fn!(obs_get_source_by_name: extern "C" fn(*const c_char) -> *mut ObsSourceT);
crate::resolved_fn!(obs_source_release: extern "C" fn(*mut ObsSourceT));
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

    if let Ok(mut guard) = LEVELS.lock() {
        guard.get_or_insert_with(HashMap::new).insert(name, (peak_db, active));
    }
}

extern "C" fn attach_callback_enum_proc(_param: *mut c_void, source: *mut ObsSourceT) -> bool {
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
        if let Some(obs_enum_sources) = obs_enum_sources() {
            if !SHUTTING_DOWN.load(Ordering::Acquire) {
                obs_enum_sources(attach_callback_enum_proc, std::ptr::null_mut());
            }
        }
        if !SHUTTING_DOWN.load(Ordering::Acquire) {
            attach_scene_audio_taps();
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
    unsafe {
        MODULE_POINTER = module;
    }
}

#[no_mangle]
pub extern "C" fn obs_current_module() -> *mut ObsModuleT {
    unsafe { MODULE_POINTER }
}

#[no_mangle]
pub extern "C" fn obs_module_ver() -> u32 {
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
    log_line("loaded — watching for audio on Preview-only sources");
    spawn_periodic_rescan();
    true
}

/// Called once, after every module (including obs-websocket, if
/// installed) has finished `obs_module_load` — the obs-websocket header's
/// own documented requirement for vendor registration, guaranteeing no
/// load-order race regardless of which order OBS happens to load modules
/// in.
#[no_mangle]
pub extern "C" fn obs_module_post_load() {
    let vendor = calldata::register_vendor("framesw");
    if vendor.is_null() {
        log_line("obs-websocket not installed/loaded — audio levels will only reach OBS's own log, not FrameSW");
        return;
    }
    VENDOR.store(vendor, Ordering::Release);
    log_line("registered as obs-websocket vendor \"framesw\" — forwarding audio levels");
    spawn_emit_loop();
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
    SHUTTING_DOWN.store(true, Ordering::Release);
    let handles: Vec<std::thread::JoinHandle<()>> = match THREADS.lock() {
        Ok(mut threads) => threads.drain(..).collect(),
        Err(_) => Vec::new(),
    };
    for handle in handles {
        let _ = handle.join();
    }
    log_line("unloaded — background threads stopped cleanly");
}
