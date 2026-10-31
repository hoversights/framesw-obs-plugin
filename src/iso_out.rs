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
//! Separate recordings (FrameSW TASKS.md, "Separate recordings"): one source
//! to a file of its own, beside OBS's own recording. The game, and the
//! host's camera, each at its own size, each with its own sound, to be put
//! together in an editor later.
//!
//! The 9:16 stream's pattern (`portrait_out.rs`): a view at its own size,
//! encoders of its own, an output. Here the view shows one source, not a
//! scene, at the source's own size, and the output writes a file
//! (`ffmpeg_muxer`, the output OBS's own recording uses). The sound is one
//! of OBS's audio tracks (the audio encoder's mixer): FrameSW puts that
//! source's sound alone on it (obs-websocket's `SetInputAudioTracks`), so
//! the file holds that sound and nothing else. OBS mixes only what is on
//! Program, so a source off air gives its file no sound, while its picture
//! is recorded all the time.
//!
//! Facts from libobs (checked for `portrait_out.rs` against 32.2.2) this
//! depends on: an encoder may take a view's `video_t`;
//! `obs_encoder_set_video` refuses once the encoder is active, so it is set
//! before the output starts; teardown is the output (its release stops it
//! and joins), then the encoders, then the view, `obs_view_remove` before
//! `obs_view_destroy`. `obs_audio_encoder_create`'s fourth argument is the
//! mixer (track) index, 0 for track 1.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::{Mutex, PoisonError};

use crate::log_line;
use crate::portrait_out::{
    obs_audio_encoder_create, obs_encoder_release, obs_encoder_set_audio, obs_encoder_set_video, obs_get_audio,
    obs_output_get_frames_dropped, obs_output_get_total_frames, obs_output_set_audio_encoder,
    obs_output_set_video_encoder, obs_video_encoder_create, ObsEncoderT,
};
use crate::video_tap::{self, ObsViewT, EVENT_EXIT, EVENT_SCENE_COLLECTION_CLEANUP};
use studio_mode_meters_core::metering::{
    ffi_guard, obs_get_source_by_name, obs_queue_task, obs_source_release, ObsOutputT, ObsSourceT, OBS_TASK_UI,
};
use studio_mode_meters_core::obs_data::{self, ObsDataT};

// A source's own size. libobs/obs.h: `uint32_t obs_source_get_width(obs_source_t *source)`.
studio_mode_meters_core::resolved_fn!(obs_source_get_width: extern "C" fn(*mut ObsSourceT) -> u32);
studio_mode_meters_core::resolved_fn!(obs_source_get_height: extern "C" fn(*mut ObsSourceT) -> u32);

/// One file being recorded. Its pointers are libobs objects this plugin
/// holds a reference to, touched only on OBS's UI thread.
struct Iso {
    id: String,
    path: String,
    view: *mut ObsViewT,
    video_enc: *mut ObsEncoderT,
    audio_enc: *mut ObsEncoderT,
    output: *mut ObsOutputT,
}

// SAFETY: libobs objects are reference-counted and safe to hand between
// threads; every mutation here runs on OBS's UI thread (`run_on_ui`).
unsafe impl Send for Iso {}

static ISOS: Mutex<Vec<Iso>> = Mutex::new(Vec::new());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn cstr(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
}

fn new_data() -> Result<*mut ObsDataT, String> {
    let create = obs_data::obs_data_create().ok_or("obs_data_create unavailable")?;
    let data = create();
    if data.is_null() {
        return Err("obs_data_create returned null".into());
    }
    Ok(data)
}

struct Start {
    id: String,
    source: String,
    /// OBS's audio track, 1 to 6.
    track: i64,
    path: String,
    video_encoder: String,
    audio_encoder: String,
}

/// The settings that make `encoder` record at OBS's own "Indistinguishable
/// quality" (Simple output's HQ): a quality, not a bitrate, so a still menu
/// costs little and a busy game gets what it needs (FrameSW
/// `recording_settings.rs`). OBS's `UpdateRecordingSettings_*`: Apple's
/// hardware CRF at quality 70, NVIDIA's CQP 16, x264's CRF 16.
fn quality_settings(encoder: &str, settings: *mut ObsDataT) {
    if encoder.contains("videotoolbox") {
        obs_data::set_string(settings, "rate_control", "CRF");
        obs_data::set_string(settings, "profile", "high");
        obs_data::set_int(settings, "quality", 70);
    } else if encoder.contains("nvenc") {
        obs_data::set_string(settings, "rate_control", "CQP");
        obs_data::set_int(settings, "cqp", 16);
    } else if encoder == "obs_x264" {
        obs_data::set_string(settings, "rate_control", "CRF");
        obs_data::set_int(settings, "crf", 16);
        obs_data::set_string(settings, "preset", "veryfast");
    } else {
        // An encoder without a known quality mode: a generous bitrate.
        obs_data::set_string(settings, "rate_control", "CBR");
        obs_data::set_int(settings, "bitrate", 20000);
    }
    obs_data::set_int(settings, "keyint_sec", 2);
}

fn start(req: &Start) -> Result<(), String> {
    if lock(&ISOS).iter().any(|i| i.id == req.id) {
        return Err(format!("the separate recording '{}' is already running", req.id));
    }
    let mut iso = Iso {
        id: req.id.clone(),
        path: req.path.clone(),
        view: std::ptr::null_mut(),
        video_enc: std::ptr::null_mut(),
        audio_enc: std::ptr::null_mut(),
        output: std::ptr::null_mut(),
    };
    match build(&mut iso, req) {
        Ok((w, h)) => {
            log_line(&format!(
                "separate recording '{}' started: '{}' at {w}x{h}, track {}, {} to {}",
                iso.id, req.source, req.track, req.video_encoder, iso.path
            ));
            lock(&ISOS).push(iso);
            register_event_callback();
            Ok(())
        }
        Err(e) => {
            log_line(&format!("separate recording '{}' failed to start: {e}", req.id));
            teardown(iso);
            Err(e)
        }
    }
}

/// Builds `iso` step by step; leaves whatever it made in `iso`, so the
/// caller's `teardown` undoes exactly that on failure. The picture's size.
fn build(iso: &mut Iso, req: &Start) -> Result<(u32, u32), String> {
    if !(1..=6).contains(&req.track) {
        return Err(format!("track {} isn't one of OBS's six", req.track));
    }
    // The picture: a view of the source, at the source's own size.
    let get_source = obs_get_source_by_name().ok_or("obs_get_source_by_name unavailable")?;
    let (Some(view_create), Some(set_source), Some(add2)) =
        (video_tap::obs_view_create(), video_tap::obs_view_set_source(), video_tap::obs_view_add2())
    else {
        return Err("obs_view_* unavailable".into());
    };
    let (Some(width), Some(height), Some(release)) =
        (obs_source_get_width(), obs_source_get_height(), obs_source_release())
    else {
        return Err("obs_source_get_width/height unavailable".into());
    };
    let cname = CString::new(req.source.as_str()).map_err(|e| e.to_string())?;
    let source = get_source(cname.as_ptr());
    if source.is_null() {
        return Err(format!("no source named '{}'", req.source));
    }
    // Even sizes: NV12's chroma is halved both ways.
    let (w, h) = (width(source) & !1, height(source) & !1);
    if w < 16 || h < 16 {
        release(source);
        return Err(format!("'{}' has no picture yet ({w}x{h})", req.source));
    }
    iso.view = view_create();
    if iso.view.is_null() {
        release(source);
        return Err("obs_view_create returned null".into());
    }
    set_source(iso.view, 0, source);
    release(source);
    let mut ovi = video_tap::portrait_ovi(w, h)?;
    let video = add2(iso.view, &mut ovi);
    if video.is_null() {
        return Err("obs_view_add2 returned null".into());
    }

    // Its encoders.
    let venc_create = obs_video_encoder_create().ok_or("obs_video_encoder_create unavailable")?;
    let aenc_create = obs_audio_encoder_create().ok_or("obs_audio_encoder_create unavailable")?;
    let set_video = obs_encoder_set_video().ok_or("obs_encoder_set_video unavailable")?;
    let set_audio = obs_encoder_set_audio().ok_or("obs_encoder_set_audio unavailable")?;
    let get_audio = obs_get_audio().ok_or("obs_get_audio unavailable")?;
    let name = CString::new(format!("FrameSW separate {}", req.id)).map_err(|e| e.to_string())?;

    let settings = new_data()?;
    quality_settings(&req.video_encoder, settings);
    let vid = CString::new(req.video_encoder.as_str()).map_err(|e| e.to_string())?;
    iso.video_enc = venc_create(vid.as_ptr(), name.as_ptr(), settings, std::ptr::null_mut());
    obs_data::release(settings);
    if iso.video_enc.is_null() {
        return Err(format!("video encoder '{}' unavailable", req.video_encoder));
    }
    set_video(iso.video_enc, video);

    let settings = new_data()?;
    obs_data::set_int(settings, "bitrate", 192);
    let aid = CString::new(req.audio_encoder.as_str()).map_err(|e| e.to_string())?;
    iso.audio_enc = aenc_create(aid.as_ptr(), name.as_ptr(), settings, (req.track - 1) as usize, std::ptr::null_mut());
    obs_data::release(settings);
    if iso.audio_enc.is_null() {
        return Err(format!("audio encoder '{}' unavailable", req.audio_encoder));
    }
    set_audio(iso.audio_enc, get_audio());

    // The file.
    let output_create = video_tap::obs_output_create().ok_or("obs_output_create unavailable")?;
    let set_venc = obs_output_set_video_encoder().ok_or("obs_output_set_video_encoder unavailable")?;
    let set_aenc = obs_output_set_audio_encoder().ok_or("obs_output_set_audio_encoder unavailable")?;
    let output_start = video_tap::obs_output_start().ok_or("obs_output_start unavailable")?;
    let settings = new_data()?;
    obs_data::set_string(settings, "path", &req.path);
    iso.output = output_create(c"ffmpeg_muxer".as_ptr(), name.as_ptr(), settings, std::ptr::null_mut());
    obs_data::release(settings);
    if iso.output.is_null() {
        return Err("the ffmpeg_muxer output couldn't be created".into());
    }
    set_venc(iso.output, iso.video_enc);
    set_aenc(iso.output, iso.audio_enc, 0);
    if !output_start(iso.output) {
        let err = video_tap::obs_output_get_last_error().map_or(String::new(), |f| cstr(f(iso.output)));
        return Err(format!("obs_output_start failed: {err}"));
    }
    Ok((w, h))
}

fn teardown(iso: Iso) {
    if !iso.output.is_null() {
        if let Some(stop) = video_tap::obs_output_stop() {
            stop(iso.output);
        }
        // The last reference: destroy waits for the stop and joins its
        // threads, and the file is finished, before this returns.
        if let Some(release) = video_tap::obs_output_release() {
            release(iso.output);
        }
    }
    if let Some(release) = obs_encoder_release() {
        for enc in [iso.video_enc, iso.audio_enc] {
            if !enc.is_null() {
                release(enc);
            }
        }
    }
    if !iso.view.is_null() {
        if let Some(set_source) = video_tap::obs_view_set_source() {
            set_source(iso.view, 0, std::ptr::null_mut());
        }
        if let Some(remove) = video_tap::obs_view_remove() {
            remove(iso.view);
        }
        if let Some(destroy) = video_tap::obs_view_destroy() {
            destroy(iso.view);
        }
    }
}

/// Stops every separate recording: on FrameSW's ask, OBS quitting, or the
/// module unloading. On OBS's UI thread, or after the frontend is gone.
pub fn stop_all() {
    let isos = std::mem::take(&mut *lock(&ISOS));
    for iso in isos {
        let (frames, dropped) = (
            obs_output_get_total_frames().map_or(0, |f| f(iso.output)),
            obs_output_get_frames_dropped().map_or(0, |f| f(iso.output)),
        );
        let (id, path) = (iso.id.clone(), iso.path.clone());
        teardown(iso);
        log_line(&format!("separate recording '{id}' stopped: {frames} frames, {dropped} dropped, {path}"));
    }
}

// ---------------------------------------------------------------------
// OBS's recording stopping, or OBS quitting
// ---------------------------------------------------------------------

// enum obs_frontend_event, by position (obs-frontend-api.h).
const EVENT_RECORDING_STOPPING: c_int = 6;

static EVENTS_REGISTERED: Mutex<bool> = Mutex::new(false);

fn register_event_callback() {
    let mut registered = lock(&EVENTS_REGISTERED);
    if *registered {
        return;
    }
    if let Some(add) = video_tap::obs_frontend_add_event_callback() {
        add(on_frontend_event, std::ptr::null_mut());
        *registered = true;
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
        "iso_out::on_frontend_event",
        (),
        std::panic::AssertUnwindSafe(|| {
            // The separate files end with OBS's own recording, whoever
            // stopped it, and before OBS quits.
            if matches!(event, EVENT_RECORDING_STOPPING | EVENT_EXIT | EVENT_SCENE_COLLECTION_CLEANUP) {
                stop_all();
                unregister_event_callback();
            }
        }),
    );
}

// ---------------------------------------------------------------------
// Vendor requests
// ---------------------------------------------------------------------

enum UiRequest {
    Start(Start),
    Stop,
}

struct UiCall {
    ran: bool,
    request: UiRequest,
    result: Result<(), String>,
}

extern "C" fn run_on_ui_thread(param: *mut c_void) {
    ffi_guard(
        "iso_out::run_on_ui_thread",
        (),
        std::panic::AssertUnwindSafe(|| {
            if param.is_null() {
                return;
            }
            let call = unsafe { &mut *param.cast::<UiCall>() };
            call.ran = true;
            call.result = match &call.request {
                UiRequest::Start(req) => start(req),
                UiRequest::Stop => {
                    stop_all();
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

/// Request: `{"id", "source", "path"}` required; `"track"` (OBS's audio
/// track, 1 to 6: 2), `"video_encoder"` and `"audio_encoder"` optional
/// (Apple's hardware H.264 and AAC on the Mac, x264 and FFmpeg AAC
/// elsewhere). Response: `{"ok": true}` or `{"ok": false, "error": "..."}`.
pub extern "C" fn handle_start_iso(request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard(
        "handle_start_iso",
        (),
        std::panic::AssertUnwindSafe(|| {
            let request = obs_data::from_void(request_data);
            let response = obs_data::from_void(response_data);
            let text = |key: &str| obs_data::get_string(request, key).filter(|s| !s.is_empty());
            let (Some(id), Some(source), Some(path)) = (text("id"), text("source"), text("path")) else {
                obs_data::set_bool(response, "ok", false);
                obs_data::set_string(response, "error", "id, source and path are required");
                return;
            };
            let req = Start {
                id,
                source,
                track: obs_data::get_optional_int(request, "track").unwrap_or(2),
                path,
                video_encoder: text("video_encoder").unwrap_or_else(|| {
                    if cfg!(target_os = "macos") { "com.apple.videotoolbox.videoencoder.ave.avc" } else { "obs_x264" }.to_string()
                }),
                audio_encoder: text("audio_encoder")
                    .unwrap_or_else(|| if cfg!(target_os = "macos") { "CoreAudio_AAC" } else { "ffmpeg_aac" }.to_string()),
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

/// Request: `{}`. Stops every separate recording. Response: `{"ok": true}`.
pub extern "C" fn handle_stop_iso(_request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard(
        "handle_stop_iso",
        (),
        std::panic::AssertUnwindSafe(|| {
            let response = obs_data::from_void(response_data);
            match run_on_ui(UiRequest::Stop) {
                Ok(()) => obs_data::set_bool(response, "ok", true),
                Err(e) => {
                    obs_data::set_bool(response, "ok", false);
                    obs_data::set_string(response, "error", &e);
                }
            }
        }),
    );
}
