//! Mic check: read a device input raw, before the operator's filters and
//! fader, without touching it. See FrameSW's `MIC_CHECK_PLAN.md`.
//!
//! The question is "is this mic actually delivering?", asked when FrameSW
//! stages the shot and usually before anyone speaks. A live input is never
//! silent: it carries a noise floor. A dead one (unplugged, no permission,
//! a board channel sending nothing) delivers exact zeros. The level tap in
//! `metering.rs` cannot answer this, because it runs after the input's
//! filters (libobs's `obs_source_output_audio`: `process_audio`, then
//! `filter_async_audio`, then the capture callbacks), and a closed noise
//! gate turns a live floor into zeros. Measured 2026-09-25: with a gate at
//! −32 dB, 39 of 39 reports of a live, quiet mic read −100.
//!
//! So this creates a *private* copy of the input: same kind, a copy of the
//! same settings, no filters, centred balance, no mono downmix. It is never
//! saved into the scene collection and never appears in OBS's UI. It is
//! tapped for about a second and released. The operator's own input and its
//! filters are never touched.
//!
//! Device inputs only (`coreaudio_input_capture`, `wasapi_input_capture`).
//! A copy of a browser source (a guest link) would be a second guest
//! connection, and a copy of a media source would play a second time; FrameSW
//! checks those another way.
//!
//! The request answers at once with a probe id and the result arrives as a
//! vendor event, `input_probe_result`: waiting a second inside the request
//! would hold FrameSW's obs-websocket connection, and a TAKE clicked right
//! after staging would wait behind it.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use studio_mode_meters_core::metering::{
    ffi_guard, obs_get_source_by_name, obs_queue_task, obs_source_add_audio_capture_callback,
    obs_source_create_private, obs_source_release, obs_source_remove_audio_capture_callback,
    output_channels, AudioData, ObsSourceT, MAX_AV_PLANES, OBS_TASK_UI, SHUTTING_DOWN, THREADS, VENDOR,
};
use studio_mode_meters_core::obs_data::{self, ObsDataT};
use studio_mode_meters_core::{calldata, log_line};

// Signatures verified against obsproject/obs-studio@master, 2026-09-25:
// libobs/obs.h
//   EXPORT const char *obs_source_get_id(const obs_source_t *source);
//   EXPORT obs_data_t *obs_source_get_settings(const obs_source_t *source);
//       (returns an added reference: obs-source.c calls obs_data_addref)
//   #define OBS_SOURCE_FLAG_FORCE_MONO (1 << 1)
//   EXPORT void obs_source_set_flags(obs_source_t *source, uint32_t flags);
//   EXPORT uint32_t obs_source_get_flags(const obs_source_t *source);
// libobs/obs-data.h
//   EXPORT const char *obs_data_get_json(obs_data_t *data);
studio_mode_meters_core::resolved_fn!(obs_source_get_id: extern "C" fn(*const ObsSourceT) -> *const c_char);
studio_mode_meters_core::resolved_fn!(obs_source_get_settings: extern "C" fn(*const ObsSourceT) -> *mut ObsDataT);
studio_mode_meters_core::resolved_fn!(obs_source_set_flags: extern "C" fn(*mut ObsSourceT, u32));
studio_mode_meters_core::resolved_fn!(obs_source_get_flags: extern "C" fn(*const ObsSourceT) -> u32);
studio_mode_meters_core::resolved_fn!(obs_data_get_json: extern "C" fn(*mut ObsDataT) -> *const c_char);

pub const OBS_SOURCE_FLAG_FORCE_MONO: u32 = 1 << 1;

/// The kinds a private copy is safe for: opening the same device twice.
/// Measured on macOS 2026-09-25 (two `coreaudio_input_capture` inputs on
/// one device read the same floor). Windows' shared-mode WASAPI is expected
/// to allow the same and is still to be measured.
const DEVICE_KINDS: &[&str] = &["coreaudio_input_capture", "wasapi_input_capture"];

/// Packets arriving in the copy's first moments are not counted: a device
/// that has just been opened can hand over a short run of zeros while it
/// starts, which would read as "nothing arriving".
const WARMUP: Duration = Duration::from_millis(200);
const DEFAULT_WINDOW_MS: u64 = 1000;

#[derive(Default, Clone)]
struct ChannelStats {
    peak: f32,
    sum_sq: f64,
    zeros: u64,
}

struct Stats {
    created: Instant,
    first_packet: Option<Duration>,
    packets: u64,
    warmup_packets: u64,
    /// Frames counted after the warm-up, per channel.
    frames: u64,
    channels: Vec<ChannelStats>,
}

/// One probe in flight. The source pointer is only ever touched to remove
/// the callback and to release it, both done once, by `finish`.
struct Probe {
    input_name: String,
    kind: String,
    source: *mut ObsSourceT,
    stats: Box<Mutex<Stats>>,
}
// Safety: the raw source pointer is owned by this probe alone (a private
// source nothing else holds), and is used from exactly one place at a time:
// created on the UI thread, then handed to the finisher thread.
unsafe impl Send for Probe {}

static PROBES: Mutex<Option<HashMap<u64, Probe>>> = Mutex::new(None);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

extern "C" fn probe_callback(param: *mut c_void, _source: *mut ObsSourceT, audio_data: *const AudioData, _muted: bool) {
    ffi_guard(
        "input_probe_callback",
        (),
        std::panic::AssertUnwindSafe(|| {
            if param.is_null() || audio_data.is_null() {
                return;
            }
            // Safety: `param` is the `Box<Mutex<Stats>>` registered with this
            // callback, and `finish` removes the callback before freeing it.
            let stats = unsafe { &*(param as *const Mutex<Stats>) };
            let Ok(mut s) = stats.lock() else { return };
            let now = s.created.elapsed();
            let first = *s.first_packet.get_or_insert(now);
            s.packets += 1;
            if now - first < WARMUP {
                s.warmup_packets += 1;
                return;
            }
            // Safety: valid for the duration of this callback (libobs).
            let audio_data = unsafe { &*audio_data };
            let frames = audio_data.frames as usize;
            if frames == 0 {
                return;
            }
            for (c, channel) in s.channels.iter_mut().enumerate() {
                let plane = audio_data.data[c];
                if plane.is_null() {
                    // A missing plane is silence, as in `packet_levels`.
                    channel.zeros += frames as u64;
                    continue;
                }
                // OBS's pipeline is 32-bit float planar by the time capture
                // callbacks fire (see `metering::audio_capture_callback_impl`).
                let samples = unsafe { std::slice::from_raw_parts(plane.cast::<f32>(), frames) };
                for &x in samples {
                    channel.peak = channel.peak.max(x.abs());
                    channel.sum_sq += f64::from(x) * f64::from(x);
                    if x == 0.0 {
                        channel.zeros += 1;
                    }
                }
            }
            s.frames += frames as u64;
        }),
    );
}

fn to_db(linear: f64) -> f64 {
    if linear <= 0.0 { -100.0 } else { (20.0 * linear.log10()).max(-100.0) }
}

// ------------------------------------------------------------------ start

/// Filled in on the UI thread by `start_on_ui_thread`.
struct Start {
    ran: bool,
    input_name: String,
    result: Result<(u64, String), String>,
}

extern "C" fn start_on_ui_thread(param: *mut c_void) {
    ffi_guard(
        "input_probe_start_on_ui_thread",
        (),
        std::panic::AssertUnwindSafe(|| {
            let state = unsafe { &mut *(param as *mut Start) };
            state.ran = true;
            state.result = create_probe(&state.input_name);
        }),
    );
}

/// Creates the private copy and attaches the tap. UI thread: creating a
/// capture source opens an OS device, the same work `list_devices` keeps
/// on this thread.
fn create_probe(input_name: &str) -> Result<(u64, String), String> {
    let (Some(by_name), Some(release), Some(get_id), Some(get_settings), Some(get_json), Some(from_json), Some(create), Some(add_cb)) = (
        obs_get_source_by_name(),
        obs_source_release(),
        obs_source_get_id(),
        obs_source_get_settings(),
        obs_data_get_json(),
        obs_data::obs_data_create_from_json(),
        obs_source_create_private(),
        obs_source_add_audio_capture_callback(),
    ) else {
        return Err("required libobs symbols unavailable".into());
    };
    let name_c = CString::new(input_name).map_err(|_| "input name contained a NUL".to_string())?;
    let original = by_name(name_c.as_ptr());
    if original.is_null() {
        return Err(format!("no input named \"{input_name}\""));
    }

    let kind = {
        let id = get_id(original);
        if id.is_null() { String::new() } else { unsafe { CStr::from_ptr(id) }.to_string_lossy().into_owned() }
    };
    if !DEVICE_KINDS.contains(&kind.as_str()) {
        release(original);
        return Err(format!("not a device input (kind \"{kind}\")"));
    }

    // A COPY of the settings, through JSON. libobs keeps a reference to the
    // settings object a new source is given (obs.c: `context->settings =
    // obs_data_newref(settings)`), so passing the original's own object
    // would tie the probe to the operator's input.
    let settings = get_settings(original);
    let copy = if settings.is_null() {
        std::ptr::null_mut()
    } else {
        let json = get_json(settings);
        let copy = if json.is_null() { std::ptr::null_mut() } else { from_json(json) };
        obs_data::release(settings);
        copy
    };
    release(original);

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let kind_c = CString::new(kind.clone()).map_err(|_| "kind contained a NUL".to_string())?;
    // Named to explain itself in OBS's log, where a capture kind reports its
    // own complaints against this name.
    let probe_name = CString::new(format!("FrameSW mic check (temporary, not a shot) - {input_name}"))
        .map_err(|_| "input name contained a NUL".to_string())?;
    let source = create(kind_c.as_ptr(), probe_name.as_ptr(), copy);
    if !copy.is_null() {
        obs_data::release(copy);
    }
    if source.is_null() {
        return Err("OBS could not create a private copy of the input".into());
    }

    let channels = output_channels().max(1).min(MAX_AV_PLANES);
    let stats = Box::new(Mutex::new(Stats {
        created: Instant::now(),
        first_packet: None,
        packets: 0,
        warmup_packets: 0,
        frames: 0,
        channels: vec![ChannelStats::default(); channels],
    }));
    let param = (&*stats as *const Mutex<Stats>).cast_mut().cast::<c_void>();
    add_cb(source, probe_callback, param);

    if let Ok(mut probes) = PROBES.lock() {
        probes
            .get_or_insert_with(HashMap::new)
            .insert(id, Probe { input_name: input_name.to_string(), kind: kind.clone(), source, stats });
    } else {
        // Can't track it, so don't leave it running.
        if let Some(remove_cb) = obs_source_remove_audio_capture_callback() {
            remove_cb(source, probe_callback, param);
        }
        release(source);
        return Err("probe table unavailable".into());
    }
    Ok((id, kind))
}

// ----------------------------------------------------------------- finish

extern "C" fn release_on_ui_thread(param: *mut c_void) {
    ffi_guard(
        "input_probe_release_on_ui_thread",
        (),
        std::panic::AssertUnwindSafe(|| {
            if let Some(release) = obs_source_release() {
                release(param.cast::<ObsSourceT>());
            }
        }),
    );
}

/// Detaches the tap, releases the copy, and reports what it heard.
fn finish(id: u64) {
    let Some(probe) = PROBES.lock().ok().and_then(|mut p| p.as_mut()?.remove(&id)) else {
        return;
    };
    // After the remove returns, libobs will not call the tap again (the
    // callback list is walked under its own lock), so `stats` can be read
    // and freed.
    if let Some(remove_cb) = obs_source_remove_audio_capture_callback() {
        let param = (&*probe.stats as *const Mutex<Stats>).cast_mut().cast::<c_void>();
        remove_cb(probe.source, probe_callback, param);
    }
    if SHUTTING_DOWN.load(Ordering::Acquire) {
        // OBS is exiting: its own teardown frees what is left. Releasing
        // here could race it, and a UI task queued now may never run.
        log_line(&format!("mic check {id}: OBS is exiting, copy left to OBS's teardown"));
    } else if let Some(queue) = obs_queue_task() {
        // Released on the UI thread, where it was created. Not waited on:
        // nothing here depends on it.
        queue(OBS_TASK_UI, release_on_ui_thread, probe.source.cast(), false);
    }

    let stats = probe.stats.lock().map(|s| Stats {
        created: s.created,
        first_packet: s.first_packet,
        packets: s.packets,
        warmup_packets: s.warmup_packets,
        frames: s.frames,
        channels: s.channels.clone(),
    });
    let Ok(stats) = stats else { return };
    emit_result(id, &probe.input_name, &probe.kind, &stats);
}

fn emit_result(id: u64, input_name: &str, kind: &str, s: &Stats) {
    let vendor = VENDOR.load(Ordering::Acquire);
    let (Some(create), Some(set_double), Some(array_create), Some(push), Some(array_release), Some(set_array)) = (
        obs_data::obs_data_create(),
        obs_data::obs_data_set_double(),
        obs_data::obs_data_array_create(),
        obs_data::obs_data_array_push_back(),
        obs_data::obs_data_array_release(),
        obs_data::obs_data_set_array(),
    ) else {
        return;
    };
    let root = create();
    obs_data::set_int(root, "probe_id", id as i64);
    obs_data::set_string(root, "input_name", input_name);
    obs_data::set_string(root, "kind", kind);
    obs_data::set_int(root, "packets", s.packets as i64);
    obs_data::set_int(root, "warmup_packets", s.warmup_packets as i64);
    obs_data::set_int(root, "frames", s.frames as i64);
    obs_data::set_int(root, "first_packet_ms", s.first_packet.map_or(-1, |d| d.as_millis() as i64));
    let channels = array_create();
    let key = |k: &str| CString::new(k).unwrap_or_default();
    let (peak_k, rms_k, zero_k) = (key("peak_db"), key("rms_db"), key("zero_fraction"));
    for c in &s.channels {
        let entry = create();
        let frames = s.frames.max(1) as f64;
        set_double(entry, peak_k.as_ptr(), to_db(f64::from(c.peak)));
        set_double(entry, rms_k.as_ptr(), to_db((c.sum_sq / frames).sqrt()));
        set_double(entry, zero_k.as_ptr(), if s.frames == 0 { 1.0 } else { c.zeros as f64 / frames });
        push(channels, entry);
        obs_data::release(entry);
    }
    let channels_k = key("channels");
    set_array(root, channels_k.as_ptr(), channels);
    array_release(channels);
    obs_data::set_bool(root, "ok", true);
    if !vendor.is_null() {
        calldata::vendor_emit_event(vendor, "input_probe_result", obs_data::as_void(root));
    }
    obs_data::release(root);
    log_line(&format!(
        "mic check {id} on \"{input_name}\": {} packets ({} warm-up), {} frames",
        s.packets, s.warmup_packets, s.frames
    ));
}

// --------------------------------------------------------------- requests

extern "C" fn handle_probe_input(request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard(
        "handle_probe_input",
        (),
        std::panic::AssertUnwindSafe(|| handle_probe_input_impl(request_data, response_data)),
    );
}

/// Request: `{"input_name": "...", "window_ms": 1000}` (`window_ms`
/// optional, 300–5000). Response at once: `{"ok": true, "probe_id": n,
/// "kind": "..."}`, or `{"ok": false, "error": "..."}` — including "not a
/// device input" for a guest link, NDI or media, which FrameSW checks
/// another way. The measurement follows as the `input_probe_result` event:
/// `{"probe_id", "input_name", "kind", "packets", "warmup_packets",
/// "frames", "first_packet_ms", "channels": [{"peak_db", "rms_db",
/// "zero_fraction"}]}`. `frames == 0` means nothing arrived at all.
fn handle_probe_input_impl(request_data: *mut c_void, response_data: *mut c_void) {
    let request_data = obs_data::from_void(request_data);
    let response_data = obs_data::from_void(response_data);
    let Some(input_name) = obs_data::get_string(request_data, "input_name") else {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "input_name is required");
        return;
    };
    let window_ms = obs_data::get_optional_int(request_data, "window_ms")
        .map_or(DEFAULT_WINDOW_MS, |ms| ms.clamp(300, 5000) as u64);
    let Some(queue) = obs_queue_task() else {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "obs_queue_task unavailable");
        return;
    };

    let mut state = Start { ran: false, input_name, result: Err(String::new()) };
    queue(OBS_TASK_UI, start_on_ui_thread, (&mut state as *mut Start).cast(), true);
    if !state.ran {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "UI-thread task never ran");
        return;
    }
    let (id, kind) = match state.result {
        Ok(v) => v,
        Err(e) => {
            obs_data::set_bool(response_data, "ok", false);
            obs_data::set_string(response_data, "error", &e);
            return;
        }
    };

    // Finish after the warm-up plus the window, on a thread of its own,
    // joined at unload like the plugin's other threads.
    let deadline = Instant::now() + WARMUP + Duration::from_millis(window_ms) + Duration::from_millis(150);
    let handle = std::thread::spawn(move || {
        while Instant::now() < deadline && !SHUTTING_DOWN.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(50));
        }
        finish(id);
    });
    if let Ok(mut threads) = THREADS.lock() {
        threads.push(handle);
    }
    obs_data::set_bool(response_data, "ok", true);
    obs_data::set_int(response_data, "probe_id", id as i64);
    obs_data::set_string(response_data, "kind", &kind);
}

extern "C" fn handle_mono(request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard("handle_mono", (), std::panic::AssertUnwindSafe(|| handle_mono_impl(request_data, response_data)));
}

struct Mono {
    ran: bool,
    input_name: String,
    set: Option<bool>,
    result: Result<bool, String>,
}

extern "C" fn mono_on_ui_thread(param: *mut c_void) {
    ffi_guard(
        "mono_on_ui_thread",
        (),
        std::panic::AssertUnwindSafe(|| {
            let state = unsafe { &mut *(param as *mut Mono) };
            state.ran = true;
            state.result = (|| {
                let (Some(by_name), Some(release), Some(get_flags), Some(set_flags)) =
                    (obs_get_source_by_name(), obs_source_release(), obs_source_get_flags(), obs_source_set_flags())
                else {
                    return Err("required libobs symbols unavailable".to_string());
                };
                let name = CString::new(state.input_name.as_str()).map_err(|_| "input name contained a NUL".to_string())?;
                let source = by_name(name.as_ptr());
                if source.is_null() {
                    return Err(format!("no input named \"{}\"", state.input_name));
                }
                if let Some(mono) = state.set {
                    let flags = get_flags(source);
                    let flags = if mono { flags | OBS_SOURCE_FLAG_FORCE_MONO } else { flags & !OBS_SOURCE_FLAG_FORCE_MONO };
                    set_flags(source, flags);
                }
                let mono = get_flags(source) & OBS_SOURCE_FLAG_FORCE_MONO != 0;
                release(source);
                Ok(mono)
            })();
        }),
    );
}

/// Request: `{"input_name": "...", "mono": true}` — `mono` optional; without
/// it this only reads. Response: `{"ok": true, "mono": bool}`, read back
/// after any change. This is OBS's own "Downmix to Mono" (Advanced Audio
/// Properties), which obs-websocket has no request for. On the UI thread
/// because setting flags signals OBS's UI to update that checkbox.
fn handle_mono_impl(request_data: *mut c_void, response_data: *mut c_void) {
    let request_data = obs_data::from_void(request_data);
    let response_data = obs_data::from_void(response_data);
    let Some(input_name) = obs_data::get_string(request_data, "input_name") else {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "input_name is required");
        return;
    };
    let Some(queue) = obs_queue_task() else {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "obs_queue_task unavailable");
        return;
    };
    let mut state = Mono { ran: false, input_name, set: obs_data::get_optional_bool(request_data, "mono"), result: Err(String::new()) };
    queue(OBS_TASK_UI, mono_on_ui_thread, (&mut state as *mut Mono).cast(), true);
    match (state.ran, state.result) {
        (true, Ok(mono)) => {
            obs_data::set_bool(response_data, "ok", true);
            obs_data::set_bool(response_data, "mono", mono);
        }
        (true, Err(e)) => {
            obs_data::set_bool(response_data, "ok", false);
            obs_data::set_string(response_data, "error", &e);
        }
        (false, _) => {
            obs_data::set_bool(response_data, "ok", false);
            obs_data::set_string(response_data, "error", "UI-thread task never ran");
        }
    }
}

pub fn requests() -> [(&'static str, calldata::RequestCallbackFn); 2] {
    [
        ("probe_input", handle_probe_input as calldata::RequestCallbackFn),
        ("mono", handle_mono as calldata::RequestCallbackFn),
    ]
}

#[cfg(test)]
mod tests {
    use super::to_db;

    #[test]
    fn silence_is_minus_100_and_full_scale_is_zero() {
        assert_eq!(to_db(0.0), -100.0);
        assert!((to_db(1.0) - 0.0).abs() < 1e-9);
        assert!((to_db(0.5) + 6.0206).abs() < 1e-3);
        assert_eq!(to_db(1e-9), -100.0);
    }
}
