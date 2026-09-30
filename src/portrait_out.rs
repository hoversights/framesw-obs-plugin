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

//! Portrait streams (FrameSW MULTISTREAM_PLAN.md): a portrait picture of one
//! scene, encoded on its own and sent over RTMP beside OBS's own stream.
//! Several at once, one per id: FrameSW's show stream ("portrait") and its
//! per-site test streams. Started as the Phase 0 spike, and measured before
//! it was kept: live to YouTube's dual-format event on 2026-09-29, following
//! every TAKE, 0 frames dropped.
//!
//! Every new declaration was checked against obs-studio **32.2.2** (the
//! installed OBS), fetched with curl on 2026-09-29: `libobs/obs.h`
//! (`obs_enum_encoder_types` 697, `obs_get_audio` 709, `obs_output_active`
//! 1948, `obs_output_set_video_encoder` 2017, `obs_output_set_audio_encoder`
//! 2037, `obs_output_set_service` 2061, `obs_output_get_total_bytes` 2071,
//! `obs_output_get_frames_dropped` 2072, `obs_output_get_total_frames` 2073,
//! `obs_video_encoder_create` 2237, `obs_audio_encoder_create` 2249,
//! `obs_encoder_release` 2256, `obs_encoder_set_video` 2413,
//! `obs_encoder_set_audio` 2416, `obs_service_create_private` 2487,
//! `obs_service_release` 2493) and `libobs/obs-encoder.c`.
//!
//! Facts from that source the code depends on:
//! - An encoder may take any mix's `video_t`, a view's included:
//!   `get_mix_for_video` finds the view's mix. The GPU path is used when the
//!   encoder takes textures and the mix makes them; otherwise
//!   `start_raw_video` raises the mix's `raw_active`.
//! - `obs_encoder_set_video` refuses once the encoder is active or
//!   initialised, so it is set before the output starts.
//! - Unlike the monitor feeds (`video_tap.rs`), the view's BASE size is the
//!   portrait frame, so a scene laid out in portrait coordinates fills it,
//!   unclipped past the canvas's own size (measured, Phase 0).
//! - Teardown order: the output (its release stops it and joins), then the
//!   encoders and the service, then the view, with `obs_view_remove`
//!   before `obs_view_destroy` (`video_tap.rs`'s header).
//!
//! **The stream key is a secret.** It goes from the request into the
//! service's settings and nowhere else: never logged, never in a status.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::{Mutex, PoisonError};

use crate::log_line;
use crate::video_tap::{self, ObsViewT, VideoT, EVENT_EXIT, EVENT_SCENE_COLLECTION_CLEANUP};
use studio_mode_meters_core::metering::{
    ffi_guard, frontend_scene_name, obs_frontend_get_current_scene, obs_get_source_by_name, obs_queue_task,
    obs_source_create_private, obs_source_release, ObsOutputT, ObsSourceT, OBS_TASK_UI,
};
use studio_mode_meters_core::obs_data::{self, ObsDataT};

pub enum ObsEncoderT {}
pub enum ObsServiceT {}
pub enum AudioT {}

studio_mode_meters_core::resolved_fn!(
    obs_video_encoder_create: extern "C" fn(*const c_char, *const c_char, *mut ObsDataT, *mut ObsDataT) -> *mut ObsEncoderT
);
studio_mode_meters_core::resolved_fn!(
    obs_audio_encoder_create:
        extern "C" fn(*const c_char, *const c_char, *mut ObsDataT, usize, *mut ObsDataT) -> *mut ObsEncoderT
);
studio_mode_meters_core::resolved_fn!(obs_encoder_release: extern "C" fn(*mut ObsEncoderT));
studio_mode_meters_core::resolved_fn!(obs_encoder_set_video: extern "C" fn(*mut ObsEncoderT, *mut VideoT));
studio_mode_meters_core::resolved_fn!(obs_encoder_set_audio: extern "C" fn(*mut ObsEncoderT, *mut AudioT));
studio_mode_meters_core::resolved_fn!(obs_get_audio: extern "C" fn() -> *mut AudioT);
studio_mode_meters_core::resolved_fn!(
    obs_service_create_private: extern "C" fn(*const c_char, *const c_char, *mut ObsDataT) -> *mut ObsServiceT
);
studio_mode_meters_core::resolved_fn!(obs_service_release: extern "C" fn(*mut ObsServiceT));
studio_mode_meters_core::resolved_fn!(obs_output_set_video_encoder: extern "C" fn(*mut ObsOutputT, *mut ObsEncoderT));
studio_mode_meters_core::resolved_fn!(
    obs_output_set_audio_encoder: extern "C" fn(*mut ObsOutputT, *mut ObsEncoderT, usize)
);
studio_mode_meters_core::resolved_fn!(obs_output_set_service: extern "C" fn(*mut ObsOutputT, *mut ObsServiceT));
studio_mode_meters_core::resolved_fn!(obs_output_active: extern "C" fn(*const ObsOutputT) -> bool);
studio_mode_meters_core::resolved_fn!(obs_output_get_total_bytes: extern "C" fn(*const ObsOutputT) -> u64);
studio_mode_meters_core::resolved_fn!(obs_output_get_frames_dropped: extern "C" fn(*const ObsOutputT) -> c_int);
studio_mode_meters_core::resolved_fn!(obs_output_get_total_frames: extern "C" fn(*const ObsOutputT) -> c_int);
studio_mode_meters_core::resolved_fn!(obs_enum_encoder_types: extern "C" fn(usize, *mut *const c_char) -> bool);

// Following OBS's Program (MULTISTREAM_PLAN.md §5). Checked against
// obs-studio 32.2.2, fetched with curl on 2026-09-29: `libobs/obs.h`
// (`obs_source_get_id` 1179, `enum obs_transition_mode` 1586,
// `obs_transition_start` 1591, `obs_transition_set` 1594) and
// `frontend/api/obs-frontend-api.h` (`enum obs_frontend_event` 16,
// `obs_frontend_get_current_transition` 128,
// `obs_frontend_get_transition_duration` 130).
studio_mode_meters_core::resolved_fn!(obs_source_get_id: extern "C" fn(*const ObsSourceT) -> *const c_char);
studio_mode_meters_core::resolved_fn!(obs_transition_set: extern "C" fn(*mut ObsSourceT, *mut ObsSourceT));
// `enum obs_transition_mode mode`: an int.
studio_mode_meters_core::resolved_fn!(
    obs_transition_start: extern "C" fn(*mut ObsSourceT, c_int, u32, *mut ObsSourceT) -> bool
);
studio_mode_meters_core::resolved_fn!(obs_frontend_get_current_transition: extern "C" fn() -> *mut ObsSourceT);
studio_mode_meters_core::resolved_fn!(obs_frontend_get_transition_duration: extern "C" fn() -> c_int);

// enum obs_transition_mode: AUTO, MANUAL.
const OBS_TRANSITION_MODE_AUTO: c_int = 0;
// enum obs_frontend_event, by position (`video_tap.rs` keeps the others).
const EVENT_SCENE_CHANGED: c_int = 8;
const EVENT_TRANSITION_CHANGED: c_int = 10;
const EVENT_TRANSITION_STOPPED: c_int = 11;

/// The portrait twin of one of FrameSW's Program scenes. The names are
/// FrameSW's (`ProgramSlot::portrait_scene_name` in the app): the two must
/// stay the same.
fn portrait_twin(program: &str) -> Option<&'static str> {
    match program {
        "FrameSW A" => Some("FrameSW A · Portrait"),
        "FrameSW B" => Some("FrameSW B · Portrait"),
        _ => None,
    }
}

/// The running portrait output. Its pointers are libobs objects this
/// plugin holds a reference to, touched only on OBS's UI thread (start,
/// stop) or through libobs's thread-safe getters (status).
struct Out {
    /// Which output: "portrait" for the show's portrait stream, others for
    /// FrameSW's test streams (one landscape, one portrait at once).
    id: String,
    view: *mut ObsViewT,
    /// Following OBS's Program: the view's source is this transition, a
    /// private one of the same kind as OBS's, which cuts to each Program
    /// scene's portrait twin as OBS cuts to the scene. Null otherwise.
    transition: *mut ObsSourceT,
    follow: bool,
    video_enc: *mut ObsEncoderT,
    audio_enc: *mut ObsEncoderT,
    service: *mut ObsServiceT,
    output: *mut ObsOutputT,
    scene: String,
    video_encoder: String,
    size: (u32, u32),
}

// SAFETY: libobs objects are reference-counted and safe to hand between
// threads; every mutation here runs on OBS's UI thread (`run_on_ui`).
unsafe impl Send for Out {}

/// The outputs running now, each by its id.
static OUTS: Mutex<Vec<Out>> = Mutex::new(Vec::new());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn cstr(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
}

fn release_source(source: *mut ObsSourceT) {
    if !source.is_null() {
        if let Some(release) = obs_source_release() {
            release(source);
        }
    }
}

struct Start {
    id: String,
    /// Ignored when following: the Program's portrait twin is shown.
    follow: bool,
    scene: String,
    server: String,
    key: String,
    video_encoder: String,
    audio_encoder: String,
    bitrate: i64,
    width: u32,
    height: u32,
}

fn default_video_encoder() -> &'static str {
    if cfg!(target_os = "macos") {
        "com.apple.videotoolbox.videoencoder.ave.avc"
    } else {
        "obs_x264"
    }
}

fn default_audio_encoder() -> &'static str {
    if cfg!(target_os = "macos") {
        "CoreAudio_AAC"
    } else {
        "ffmpeg_aac"
    }
}

fn new_data() -> Result<*mut ObsDataT, String> {
    let create = obs_data::obs_data_create().ok_or("obs_data_create unavailable")?;
    let data = create();
    if data.is_null() {
        return Err("obs_data_create returned null".into());
    }
    Ok(data)
}

fn start(req: &Start) -> Result<(), String> {
    if lock(&OUTS).iter().any(|o| o.id == req.id) {
        return Err(format!("the output '{}' is already running", req.id));
    }
    let mut out = Out {
        id: req.id.clone(),
        view: std::ptr::null_mut(),
        transition: std::ptr::null_mut(),
        follow: req.follow,
        video_enc: std::ptr::null_mut(),
        audio_enc: std::ptr::null_mut(),
        service: std::ptr::null_mut(),
        output: std::ptr::null_mut(),
        scene: req.scene.clone(),
        video_encoder: req.video_encoder.clone(),
        size: (req.width, req.height),
    };
    match build(&mut out, req) {
        Ok(()) => {
            // The server only: the key is never logged.
            log_line(&format!(
                "output '{}' started: scene '{}'{}, {}x{}, {} at {} kbps, to {}",
                out.id,
                out.scene,
                if req.follow { ", following Program" } else { "" },
                req.width,
                req.height,
                req.video_encoder,
                req.bitrate,
                req.server
            ));
            lock(&OUTS).push(out);
            register_event_callback();
            Ok(())
        }
        Err(e) => {
            log_line(&format!("output '{}' failed to start: {e}", req.id));
            teardown(out);
            Err(e)
        }
    }
}

/// Builds `out` step by step. Leaves whatever it made in `out`, so the
/// caller's `teardown` undoes exactly that on failure.
fn build(out: &mut Out, req: &Start) -> Result<(), String> {
    // The picture: a view of the scene, at the portrait size.
    let get_source = obs_get_source_by_name().ok_or("obs_get_source_by_name unavailable")?;
    let (Some(view_create), Some(set_source), Some(add2)) =
        (video_tap::obs_view_create(), video_tap::obs_view_set_source(), video_tap::obs_view_add2())
    else {
        return Err("obs_view_* unavailable".into());
    };
    if out.follow {
        let program = frontend_scene_name(obs_frontend_get_current_scene()).unwrap_or_default();
        let twin = portrait_twin(&program).ok_or(format!("OBS's Program ('{program}') isn't one of FrameSW's scenes"))?;
        out.scene = twin.to_string();
    }
    let cscene = CString::new(out.scene.as_str()).map_err(|e| e.to_string())?;
    let scene = get_source(cscene.as_ptr());
    if scene.is_null() {
        return Err(format!("no scene named '{}'", out.scene));
    }
    out.view = view_create();
    if out.view.is_null() {
        release_source(scene);
        return Err("obs_view_create returned null".into());
    }
    if out.follow {
        // Through a transition, so a TAKE is followed the way OBS makes it.
        let Some(set) = obs_transition_set() else {
            release_source(scene);
            return Err("obs_transition_set unavailable".into());
        };
        match new_transition() {
            Ok(t) => {
                out.transition = t;
                set(t, scene);
                set_source(out.view, 0, t);
            }
            Err(e) => {
                release_source(scene);
                return Err(e);
            }
        }
    } else {
        set_source(out.view, 0, scene);
    }
    release_source(scene);
    let mut ovi = video_tap::portrait_ovi(req.width, req.height)?;
    let video = add2(out.view, &mut ovi);
    if video.is_null() {
        return Err("obs_view_add2 returned null".into());
    }

    // Its encoders.
    let venc_create = obs_video_encoder_create().ok_or("obs_video_encoder_create unavailable")?;
    let aenc_create = obs_audio_encoder_create().ok_or("obs_audio_encoder_create unavailable")?;
    let set_video = obs_encoder_set_video().ok_or("obs_encoder_set_video unavailable")?;
    let set_audio = obs_encoder_set_audio().ok_or("obs_encoder_set_audio unavailable")?;
    let get_audio = obs_get_audio().ok_or("obs_get_audio unavailable")?;

    let settings = new_data()?;
    obs_data::set_string(settings, "rate_control", "CBR");
    obs_data::set_int(settings, "bitrate", req.bitrate);
    // Two seconds: what YouTube asks for.
    obs_data::set_int(settings, "keyint_sec", 2);
    if req.video_encoder == "obs_x264" {
        obs_data::set_string(settings, "preset", "veryfast");
    }
    let vid = CString::new(req.video_encoder.as_str()).map_err(|e| e.to_string())?;
    out.video_enc = venc_create(vid.as_ptr(), c"FrameSW portrait video".as_ptr(), settings, std::ptr::null_mut());
    obs_data::release(settings);
    if out.video_enc.is_null() {
        return Err(format!("video encoder '{}' unavailable", req.video_encoder));
    }
    set_video(out.video_enc, video);

    let settings = new_data()?;
    obs_data::set_int(settings, "bitrate", 160);
    let aid = CString::new(req.audio_encoder.as_str()).map_err(|e| e.to_string())?;
    out.audio_enc =
        aenc_create(aid.as_ptr(), c"FrameSW portrait audio".as_ptr(), settings, 0, std::ptr::null_mut());
    obs_data::release(settings);
    if out.audio_enc.is_null() {
        return Err(format!("audio encoder '{}' unavailable", req.audio_encoder));
    }
    set_audio(out.audio_enc, get_audio());

    // Where it goes. The key lives in these settings and nowhere else.
    let service_create = obs_service_create_private().ok_or("obs_service_create_private unavailable")?;
    let settings = new_data()?;
    obs_data::set_string(settings, "server", &req.server);
    obs_data::set_string(settings, "key", &req.key);
    out.service = service_create(c"rtmp_custom".as_ptr(), c"FrameSW portrait service".as_ptr(), settings);
    obs_data::release(settings);
    if out.service.is_null() {
        return Err("the rtmp_custom service couldn't be created".into());
    }

    // The output that sends it.
    let output_create = video_tap::obs_output_create().ok_or("obs_output_create unavailable")?;
    let set_venc = obs_output_set_video_encoder().ok_or("obs_output_set_video_encoder unavailable")?;
    let set_aenc = obs_output_set_audio_encoder().ok_or("obs_output_set_audio_encoder unavailable")?;
    let set_service = obs_output_set_service().ok_or("obs_output_set_service unavailable")?;
    let output_start = video_tap::obs_output_start().ok_or("obs_output_start unavailable")?;
    out.output = output_create(
        c"rtmp_output".as_ptr(),
        c"FrameSW portrait".as_ptr(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    if out.output.is_null() {
        return Err("the rtmp_output couldn't be created".into());
    }
    set_venc(out.output, out.video_enc);
    set_aenc(out.output, out.audio_enc, 0);
    set_service(out.output, out.service);
    if !output_start(out.output) {
        let err = video_tap::obs_output_get_last_error().map_or(String::new(), |f| cstr(f(out.output)));
        return Err(format!("obs_output_start failed: {err}"));
    }
    Ok(())
}

fn teardown(out: Out) {
    if !out.output.is_null() {
        if obs_output_active().is_some_and(|active| active(out.output)) {
            if let Some(stop) = video_tap::obs_output_stop() {
                stop(out.output);
            }
        }
        // The last reference: destroy waits for the stop and joins its
        // threads, so nothing of the output runs after this returns.
        if let Some(release) = video_tap::obs_output_release() {
            release(out.output);
        }
    }
    if let Some(release) = obs_encoder_release() {
        for enc in [out.video_enc, out.audio_enc] {
            if !enc.is_null() {
                release(enc);
            }
        }
    }
    if !out.service.is_null() {
        if let Some(release) = obs_service_release() {
            release(out.service);
        }
    }
    if !out.view.is_null() {
        if let Some(set_source) = video_tap::obs_view_set_source() {
            set_source(out.view, 0, std::ptr::null_mut());
        }
        // `obs_view_remove` before `obs_view_destroy`: destroy alone
        // leaves the view in the render loop.
        if let Some(remove) = video_tap::obs_view_remove() {
            remove(out.view);
        }
        if let Some(destroy) = video_tap::obs_view_destroy() {
            destroy(out.view);
        }
    }
    // After the view has let go of it.
    release_source(out.transition);
}

/// A private transition of the kind OBS is using now (a fade when OBS
/// can't say), for the portrait view.
fn new_transition() -> Result<*mut ObsSourceT, String> {
    let id = obs_frontend_get_current_transition()
        .map(|get| get())
        .filter(|t| !t.is_null())
        .map(|t| {
            let id = obs_source_get_id().map_or(String::new(), |f| cstr(f(t)));
            release_source(t);
            id
        })
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "fade_transition".to_string());
    let create = obs_source_create_private().ok_or("obs_source_create_private unavailable")?;
    let cid = CString::new(id.as_str()).map_err(|e| e.to_string())?;
    let t = create(cid.as_ptr(), c"FrameSW portrait transition".as_ptr(), std::ptr::null_mut());
    if t.is_null() {
        return Err(format!("the '{id}' transition couldn't be created"));
    }
    Ok(t)
}

/// OBS's Program changed: the portrait view goes to its twin, with the
/// transition and duration OBS is using. From OBS's scene-changed event,
/// which it sends as it starts the main transition (MULTISTREAM_PLAN.md
/// §5: FrameSW sends no second TAKE, so a TAKE from anywhere is followed).
/// A scene that isn't FrameSW's leaves the portrait where it is.
fn follow_program() {
    let Some(program) = frontend_scene_name(obs_frontend_get_current_scene()) else { return };
    let Some(twin) = portrait_twin(&program) else { return };
    for out in lock(&OUTS).iter_mut().filter(|o| o.follow && !o.transition.is_null()) {
        follow_to(out, twin);
    }
}

fn follow_to(out: &mut Out, twin: &'static str) {
    if twin == out.scene {
        return;
    }
    let (Some(get_source), Some(start), Some(set)) = (obs_get_source_by_name(), obs_transition_start(), obs_transition_set())
    else {
        return;
    };
    let Ok(ctwin) = CString::new(twin) else { return };
    let dest = get_source(ctwin.as_ptr());
    if dest.is_null() {
        log_line(&format!("output '{}': no scene named '{twin}' to follow Program to", out.id));
        return;
    }
    let duration = obs_frontend_get_transition_duration().map_or(300, |f| f().max(0) as u32);
    // One already running refuses a second: cut instead.
    let eased = start(out.transition, OBS_TRANSITION_MODE_AUTO, duration, dest);
    if !eased {
        set(out.transition, dest);
    }
    release_source(dest);
    log_line(&format!(
        "output '{}' follows Program to '{twin}' ({})",
        out.id,
        if eased { format!("{duration} ms") } else { "cut: a transition was running".to_string() }
    ));
    out.scene = twin.to_string();
}

/// OBS's transition was changed (Fade to Cut, say): the portrait's becomes
/// the same kind, showing what it showed.
fn match_transition() {
    for out in lock(&OUTS).iter_mut().filter(|o| o.follow && !o.transition.is_null()) {
        match_transition_of(out);
    }
}

fn match_transition_of(out: &mut Out) {
    let have = obs_source_get_id().map_or(String::new(), |f| cstr(f(out.transition)));
    let Ok(next) = new_transition() else { return };
    let want = obs_source_get_id().map_or(String::new(), |f| cstr(f(next)));
    let (Some(get_source), Some(set), Some(set_source)) =
        (obs_get_source_by_name(), obs_transition_set(), video_tap::obs_view_set_source())
    else {
        release_source(next);
        return;
    };
    if want == have {
        release_source(next);
        return;
    }
    let Ok(cscene) = CString::new(out.scene.as_str()) else {
        release_source(next);
        return;
    };
    let scene = get_source(cscene.as_ptr());
    set(next, scene);
    release_source(scene);
    set_source(out.view, 0, next);
    release_source(std::mem::replace(&mut out.transition, next));
    log_line(&format!("output '{}' transition is now '{want}', as OBS's", out.id));
}

/// Stops every output and stops listening for OBS's events. On OBS's UI
/// thread.
pub fn stop_all() {
    stop_output();
    unregister_event_callback();
}

/// Stops one output; the events go when the last does. On OBS's UI thread.
fn stop_id(id: &str) {
    let found = {
        let mut outs = lock(&OUTS);
        outs.iter().position(|o| o.id == id).map(|i| outs.remove(i))
    };
    if let Some(out) = found {
        stop_one(out);
    }
    if lock(&OUTS).is_empty() {
        unregister_event_callback();
    }
}

fn stop_one(out: Out) {
    let (id, scene) = (out.id.clone(), out.scene.clone());
    teardown(out);
    log_line(&format!("output '{id}' stopped (scene '{scene}')"));
}

/// Every output only: for module unload, when the frontend's callbacks are
/// already gone and must not be touched (lib.rs, `obs_module_unload`).
pub fn stop_output() {
    let outs = std::mem::take(&mut *lock(&OUTS));
    for out in outs {
        stop_one(out);
    }
}

// ---------------------------------------------------------------------
// OBS quitting, or the scene collection going away
// ---------------------------------------------------------------------

static EVENTS_REGISTERED: Mutex<bool> = Mutex::new(false);

fn register_event_callback() {
    let mut registered = lock(&EVENTS_REGISTERED);
    if *registered {
        return;
    }
    match video_tap::obs_frontend_add_event_callback() {
        Some(add) => {
            add(on_frontend_event, std::ptr::null_mut());
            *registered = true;
        }
        None => log_line("obs_frontend_add_event_callback unavailable: stop the portrait output before quitting OBS"),
    }
}

fn unregister_event_callback() {
    let mut registered = lock(&EVENTS_REGISTERED);
    if !*registered {
        return;
    }
    if let Some(remove) = video_tap::obs_frontend_remove_event_callback() {
        remove(on_frontend_event, std::ptr::null_mut());
    }
    *registered = false;
}

extern "C" fn on_frontend_event(event: c_int, _private_data: *mut c_void) {
    ffi_guard(
        "portrait_out::on_frontend_event",
        (),
        std::panic::AssertUnwindSafe(|| {
            // The view holds the scene: it lets go before the collection
            // is torn down, and the output stops before OBS does.
            match event {
                EVENT_EXIT | EVENT_SCENE_COLLECTION_CLEANUP => stop_all(),
                EVENT_SCENE_CHANGED => follow_program(),
                EVENT_TRANSITION_CHANGED => match_transition(),
                // With the "follows Program" line above, how far apart the
                // two transitions ran (MULTISTREAM_PLAN.md §5: measure).
                EVENT_TRANSITION_STOPPED => {
                    if lock(&OUTS).iter().any(|o| o.follow) {
                        log_line("portrait output: OBS's transition finished");
                    }
                }
                _ => {}
            }
        }),
    );
}

// ---------------------------------------------------------------------
// Vendor requests
// ---------------------------------------------------------------------

struct UiCall {
    ran: bool,
    request: UiRequest,
    result: Result<(), String>,
}

enum UiRequest {
    Start(Start),
    Stop(String),
}

extern "C" fn run_on_ui_thread(param: *mut c_void) {
    ffi_guard(
        "portrait_out::run_on_ui_thread",
        (),
        std::panic::AssertUnwindSafe(|| {
            if param.is_null() {
                return;
            }
            let call = unsafe { &mut *param.cast::<UiCall>() };
            call.ran = true;
            call.result = match &call.request {
                UiRequest::Start(req) => start(req),
                UiRequest::Stop(id) => {
                    stop_id(id);
                    Ok(())
                }
            };
        }),
    );
}

fn run_on_ui(request: UiRequest) -> Result<(), String> {
    let queue = obs_queue_task().ok_or("obs_queue_task unavailable")?;
    let mut call = UiCall { ran: false, request, result: Ok(()) };
    queue(OBS_TASK_UI, run_on_ui_thread, (&mut call as *mut UiCall).cast(), true);
    if !call.ran {
        return Err("UI task handler unavailable".into());
    }
    call.result
}

/// Request: `{"server", "key"}` required, and `"scene"` unless
/// `"follow_program": true`, which shows OBS's Program's portrait twin and
/// follows every TAKE; `"id"` ("portrait": the show's portrait stream;
/// FrameSW's test streams use their own), `"video_encoder"`,
/// `"audio_encoder"`, `"bitrate"` (kbps, 6000), `"width"` (1080) and
/// `"height"` (1920) optional. Several may run at once, one per id.
/// Response: `{"ok": true}` or `{"ok": false, "error": "..."}`.
pub extern "C" fn handle_start_portrait_out(request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard(
        "handle_start_portrait_out",
        (),
        std::panic::AssertUnwindSafe(|| {
            let request = obs_data::from_void(request_data);
            let response = obs_data::from_void(response_data);
            let text = |key: &str| obs_data::get_string(request, key).filter(|s| !s.is_empty());
            let follow = obs_data::get_optional_bool(request, "follow_program").unwrap_or(false);
            let scene = text("scene").or_else(|| follow.then(String::new));
            let (Some(scene), Some(server), Some(key)) = (scene, text("server"), text("key")) else {
                obs_data::set_bool(response, "ok", false);
                obs_data::set_string(response, "error", "server and key are required, and scene unless following Program");
                return;
            };
            let req = Start {
                id: text("id").unwrap_or_else(|| "portrait".to_string()),
                follow,
                scene,
                server,
                key,
                video_encoder: text("video_encoder").unwrap_or_else(|| default_video_encoder().into()),
                audio_encoder: text("audio_encoder").unwrap_or_else(|| default_audio_encoder().into()),
                bitrate: obs_data::get_optional_int(request, "bitrate").filter(|b| *b > 0).unwrap_or(6000),
                width: obs_data::get_optional_int(request, "width").filter(|w| *w > 0).unwrap_or(1080) as u32,
                height: obs_data::get_optional_int(request, "height").filter(|h| *h > 0).unwrap_or(1920) as u32,
            };
            match run_on_ui(UiRequest::Start(req)) {
                Ok(()) => obs_data::set_bool(response, "ok", true),
                Err(e) => {
                    obs_data::set_bool(response, "ok", false);
                    obs_data::set_string(response, "error", &e);
                }
            }
        }),
    );
}

/// Request: `{"id"}` ("portrait" when left out). Response: `{"ok": true}`.
pub extern "C" fn handle_stop_portrait_out(request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard(
        "handle_stop_portrait_out",
        (),
        std::panic::AssertUnwindSafe(|| {
            let request = obs_data::from_void(request_data);
            let response = obs_data::from_void(response_data);
            let id = obs_data::get_string(request, "id").filter(|s| !s.is_empty()).unwrap_or_else(|| "portrait".into());
            match run_on_ui(UiRequest::Stop(id)) {
                Ok(()) => obs_data::set_bool(response, "ok", true),
                Err(e) => {
                    obs_data::set_bool(response, "ok", false);
                    obs_data::set_string(response, "error", &e);
                }
            }
        }),
    );
}

/// Request: `{"id"}` ("portrait" when left out). Response: `{"active",
/// "frames", "dropped", "bytes", "last_error", "scene", "video_encoder",
/// "width", "height", "encoders"}`; `"encoders"` lists every encoder id this
/// OBS has, comma separated. Never the key.
pub extern "C" fn handle_portrait_out_status(request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard(
        "handle_portrait_out_status",
        (),
        std::panic::AssertUnwindSafe(|| {
            let request = obs_data::from_void(request_data);
            let response = obs_data::from_void(response_data);
            let id = obs_data::get_string(request, "id").filter(|s| !s.is_empty()).unwrap_or_else(|| "portrait".into());
            {
                let guard = lock(&OUTS);
                match guard.iter().find(|o| o.id == id) {
                    None => obs_data::set_bool(response, "active", false),
                    Some(out) => {
                        let o = out.output;
                        obs_data::set_bool(response, "active", obs_output_active().is_some_and(|f| f(o)));
                        obs_data::set_int(response, "frames", obs_output_get_total_frames().map_or(-1, |f| f(o) as i64));
                        obs_data::set_int(response, "dropped", obs_output_get_frames_dropped().map_or(-1, |f| f(o) as i64));
                        obs_data::set_int(response, "bytes", obs_output_get_total_bytes().map_or(0, |f| f(o) as i64));
                        let err = video_tap::obs_output_get_last_error().map_or(String::new(), |f| cstr(f(o)));
                        obs_data::set_string(response, "last_error", &err);
                        obs_data::set_string(response, "scene", &out.scene);
                        obs_data::set_bool(response, "follow_program", out.follow);
                        obs_data::set_string(response, "video_encoder", &out.video_encoder);
                        obs_data::set_int(response, "width", out.size.0 as i64);
                        obs_data::set_int(response, "height", out.size.1 as i64);
                    }
                }
            }
            let mut ids = Vec::new();
            if let Some(next) = obs_enum_encoder_types() {
                let mut i = 0;
                loop {
                    let mut id: *const c_char = std::ptr::null();
                    if !next(i, &mut id) {
                        break;
                    }
                    ids.push(cstr(id));
                    i += 1;
                }
            }
            obs_data::set_string(response, "encoders", &ids.join(","));
        }),
    );
}
