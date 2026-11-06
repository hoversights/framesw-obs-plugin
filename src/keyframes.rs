//! A keyframe every 2 s on OBS's own stream and recording encoders, asked by
//! FrameSW before each start (`stream_keyframes`).
//!
//! OBS's Simple output mode sets no keyframe interval: obs-studio 33.0.0,
//! `frontend/utility/SimpleOutput.cpp`, has no `keyint` anywhere. `Update`
//! sets the rate control and bitrate, then `obs_service_apply_encoder_settings`,
//! which adds a site's recommended interval only for a listed service, not
//! for a custom server, and FrameSW gives OBS every site as a custom server.
//! Each encoder then keeps its own default: Apple's is 2 s, obs-nvenc's and
//! x264's are 250 frames, 8.33 s at 30 fps (measured on Windows with NVENC
//! and on the Mac with x264, 2026-11-06). Every site joining OBS's encoder
//! (`start_site_out`) waits for a keyframe, and one was timed out by its
//! server at 10 s; Twitch asks for 2 s; and a replay starts on a keyframe, so
//! a 20 s replay buffer saved 12.7 s. The plugin's own encoders already set
//! `keyint_sec` 2 (`portrait_out`, `iso_out`); this gives OBS's the same.
//!
//! `obs_encoder_update` merges into the encoder's settings (`obs_data_apply`,
//! `libobs/obs-encoder.c`), and Simple mode's own updates never set
//! `keyint_sec`, so the value holds until OBS builds its outputs again (at its
//! start, or after a change in its Settings), and FrameSW asks before every
//! start. Only an encoder that isn't running, and only one left on its
//! encoder's own default (`keyint_sec` 0 or not set): a value chosen in OBS
//! stays.

use std::ffi::{c_char, c_void, CString};

use crate::log_line;
use crate::portrait_out::ObsEncoderT;
use studio_mode_meters_core::calldata;
use studio_mode_meters_core::metering::{ffi_guard, obs_queue_task, OBS_TASK_UI};
use studio_mode_meters_core::obs_data::{self, ObsDataT};

// libobs/obs.h, obs-studio 33.0.0 (fetched with curl, 2026-11-06):
// `obs_get_encoder_by_name` 810, `obs_encoder_release` 2290,
// `obs_encoder_update` 2438, `obs_encoder_get_settings` 2444,
// `obs_encoder_active` 2475.
studio_mode_meters_core::resolved_fn!(obs_get_encoder_by_name: extern "C" fn(*const c_char) -> *mut ObsEncoderT);
studio_mode_meters_core::resolved_fn!(obs_encoder_release: extern "C" fn(*mut ObsEncoderT));
studio_mode_meters_core::resolved_fn!(obs_encoder_update: extern "C" fn(*mut ObsEncoderT, *mut ObsDataT));
studio_mode_meters_core::resolved_fn!(obs_encoder_get_settings: extern "C" fn(*const ObsEncoderT) -> *mut ObsDataT);
studio_mode_meters_core::resolved_fn!(obs_encoder_active: extern "C" fn(*const ObsEncoderT) -> bool);

/// Simple mode's video encoders, by the names OBS gives them
/// (`SimpleOutput.cpp`: `LoadStreamingPreset_Lossy`, `LoadRecordingPreset_Lossy`).
/// The recording one exists only when the recording isn't "Same as stream";
/// the replay buffer records with it.
const ENCODERS: [&str; 2] = ["simple_video_stream", "simple_video_recording"];

/// Seconds between keyframes, as the plugin's own encoders have.
const KEYINT_SEC: i64 = 2;

/// What happened to one encoder.
fn apply_one(name: &str) -> Result<&'static str, String> {
    let (Some(by_name), Some(release), Some(update), Some(get_settings), Some(active)) = (
        obs_get_encoder_by_name(),
        obs_encoder_release(),
        obs_encoder_update(),
        obs_encoder_get_settings(),
        obs_encoder_active(),
    ) else {
        return Err("libobs's encoder functions aren't reachable".into());
    };
    let c_name = CString::new(name).map_err(|e| e.to_string())?;
    let encoder = by_name(c_name.as_ptr());
    if encoder.is_null() {
        return Ok("absent");
    }
    let outcome = if active(encoder) {
        // Its settings are fixed while it runs; it gets them at its next start.
        "running"
    } else {
        let settings = get_settings(encoder);
        let current = obs_data::get_optional_int(settings, "keyint_sec");
        obs_data::release(settings);
        if current.is_some_and(|k| k != 0) {
            "kept"
        } else {
            let Some(create) = obs_data::obs_data_create() else {
                release(encoder);
                return Err("obs_data_create unavailable".into());
            };
            let change = create();
            obs_data::set_int(change, "keyint_sec", KEYINT_SEC);
            update(encoder, change);
            obs_data::release(change);
            "set"
        }
    };
    release(encoder);
    Ok(outcome)
}

struct Work {
    ran: bool,
    results: Vec<(&'static str, Result<&'static str, String>)>,
}

extern "C" fn apply_on_ui_thread(param: *mut c_void) {
    ffi_guard(
        "keyframes::apply_on_ui_thread",
        (),
        std::panic::AssertUnwindSafe(|| {
            let work = unsafe { &mut *(param as *mut Work) };
            work.ran = true;
            work.results = ENCODERS.iter().map(|name| (*name, apply_one(name))).collect();
        }),
    );
}

extern "C" fn handle_stream_keyframes(request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard(
        "handle_stream_keyframes",
        (),
        std::panic::AssertUnwindSafe(|| handle_stream_keyframes_impl(request_data, response_data)),
    );
}

/// Request: `{}`. Response: `{"ok": true, "simple_video_stream": "set",
/// "simple_video_recording": "absent"}`, each encoder one of `set`, `kept`
/// (an interval already chosen), `running` (left as it is until its next
/// start) or `absent`, and `"<name>_error"` where one failed.
fn handle_stream_keyframes_impl(_request_data: *mut c_void, response_data: *mut c_void) {
    let response_data = obs_data::from_void(response_data);
    let Some(queue) = obs_queue_task() else {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "obs_queue_task unavailable");
        return;
    };
    let mut work = Work { ran: false, results: Vec::new() };
    queue(OBS_TASK_UI, apply_on_ui_thread, (&mut work as *mut Work).cast(), true);
    if !work.ran {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "UI-thread task never ran");
        return;
    }
    let mut ok = true;
    for (name, result) in &work.results {
        match result {
            Ok(outcome) => {
                obs_data::set_string(response_data, name, outcome);
                if *outcome == "set" {
                    log_line(&format!("keyframes every {KEYINT_SEC} s on '{name}'"));
                }
            }
            Err(e) => {
                ok = false;
                obs_data::set_string(response_data, &format!("{name}_error"), e);
            }
        }
    }
    obs_data::set_bool(response_data, "ok", ok);
}

pub fn requests() -> [(&'static str, calldata::RequestCallbackFn); 1] {
    [("stream_keyframes", handle_stream_keyframes as calldata::RequestCallbackFn)]
}
