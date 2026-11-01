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
//! FrameSW arms them (`arm_iso`) and the plugin starts them the moment OBS's
//! own recording starts, whoever started it, each named after OBS's file
//! (`2026-10-31 17-30-00 PiP 1.mov` beside `2026-10-31 17-30-00.mov`), and
//! stops them as it stops. `start_iso` starts one now, for tests.
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
// OBS's recording and the file it writes (obs-frontend-api.h, obs.h; both
// new references).
studio_mode_meters_core::resolved_fn!(obs_frontend_get_recording_output: extern "C" fn() -> *mut ObsOutputT);
studio_mode_meters_core::resolved_fn!(obs_frontend_recording_active: extern "C" fn() -> bool);
studio_mode_meters_core::resolved_fn!(obs_output_get_settings: extern "C" fn(*mut ObsOutputT) -> *mut ObsDataT);
// The encoder an output has (borrowed) and its id, e.g. "obs_nvenc_h264_tex".
studio_mode_meters_core::resolved_fn!(obs_output_get_video_encoder: extern "C" fn(*mut ObsOutputT) -> *mut ObsEncoderT);
studio_mode_meters_core::resolved_fn!(obs_encoder_get_id: extern "C" fn(*mut ObsEncoderT) -> *const c_char);

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

/// A separate recording to make with each of OBS's recordings (`arm_iso`):
/// `start` without its path, and the name its file gets after OBS's.
struct Armed {
    start: Start,
    name: String,
}

static ARMED: Mutex<Vec<Armed>> = Mutex::new(Vec::new());

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

#[derive(Clone)]
struct Start {
    id: String,
    source: String,
    /// OBS's audio track, 1 to 6.
    track: i64,
    path: String,
    /// None: the encoder OBS's own recording uses (`video_encoder_for`).
    video_encoder: Option<String>,
    audio_encoder: String,
    /// The file's picture is no taller than this: a 4K camera recorded at
    /// its own size wrote 49 GB an hour (2026-10-31). None: the source's size.
    max_height: Option<u32>,
    /// The encoder's own quality number in place of OBS's (`quality_settings`).
    quality: Option<i64>,
    /// A smaller file at a quality a little under OBS's (`quality_settings`).
    smaller: bool,
}

/// What `req` is encoded with: the encoder asked for, else the one OBS's own
/// recording uses (Apple's on the Mac, NVIDIA's on a card that has one, or
/// x264, as FrameSW set it or the user chose), else the platform's own.
fn video_encoder_for(req: &Start) -> String {
    req.video_encoder.clone().or_else(recording_encoder).unwrap_or_else(|| {
        if cfg!(target_os = "macos") { "com.apple.videotoolbox.videoencoder.ave.avc" } else { "obs_x264" }.to_string()
    })
}

/// The id of the video encoder OBS's recording output has.
fn recording_encoder() -> Option<String> {
    let output = obs_frontend_get_recording_output()?();
    if output.is_null() {
        return None;
    }
    let encoder = obs_output_get_video_encoder().map_or(std::ptr::null_mut(), |f| f(output));
    let id = (!encoder.is_null())
        .then(|| obs_encoder_get_id().map(|f| cstr(f(encoder))))
        .flatten()
        .filter(|id| !id.is_empty());
    if let Some(release) = video_tap::obs_output_release() {
        release(output);
    }
    id
}

/// The settings that make `encoder` record at OBS's own "Indistinguishable
/// quality" (Simple output's HQ): a quality, not a bitrate, so a still menu
/// costs little and a busy game gets what it needs (FrameSW
/// `recording_settings.rs`). OBS's `UpdateRecordingSettings_*`: Apple's
/// hardware CRF at quality 70, NVIDIA's CQP 16, x264's CRF 16.
///
/// `smaller`: Apple's quality 60, which on a 4K camera scaled to 1080p
/// looked the same as 70 at 2× and wrote 4.8 GB an hour, not 12.7
/// (2026-10-31). NVIDIA's CQP 20, on two 4K webcams scaled to 1080p and
/// recorded at 16 and 20 at once: 39 and 44 % less than CQP 16 (SSIM 0.994
/// and 0.983), no difference visible at 2× (RTX 4070 Ti SUPER, 2026-10-31).
/// The busier camera still wrote 20 GB an hour: a quality, not a ceiling.
/// x264's CRF 20 stands in until measured. `quality`, the encoder's own
/// number, overrides both.
fn quality_settings(encoder: &str, quality: Option<i64>, smaller: bool, settings: *mut ObsDataT) {
    let pick = |hq: i64, small: i64| quality.unwrap_or(if smaller { small } else { hq });
    if encoder.contains("videotoolbox") {
        obs_data::set_string(settings, "rate_control", "CRF");
        obs_data::set_string(settings, "profile", "high");
        obs_data::set_int(settings, "quality", pick(70, 60));
    } else if encoder.contains("nvenc") {
        obs_data::set_string(settings, "rate_control", "CQP");
        obs_data::set_int(settings, "cqp", pick(16, 20));
    } else if encoder == "obs_x264" {
        obs_data::set_string(settings, "rate_control", "CRF");
        obs_data::set_int(settings, "crf", pick(16, 20));
        obs_data::set_string(settings, "preset", "veryfast");
    } else {
        // An encoder without a known quality mode: a generous bitrate.
        obs_data::set_string(settings, "rate_control", "CBR");
        obs_data::set_int(settings, "bitrate", if smaller { 10000 } else { 20000 });
    }
    obs_data::set_int(settings, "keyint_sec", 2);
}

fn start(req: &Start) -> Result<(), String> {
    if lock(&ISOS).iter().any(|i| i.id == req.id) {
        return Err(format!("the separate recording '{}' is already running", req.id));
    }
    let req = &Start { video_encoder: Some(video_encoder_for(req)), ..req.clone() };
    let encoder = req.video_encoder.as_deref().unwrap_or_default();
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
                iso.id, req.source, req.track, encoder, iso.path
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
    // The picture: a view of the source, drawn at the source's own size and
    // scaled down by the GPU to `max_height`.
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
    let (base, (w, h)) = match req.max_height.filter(|&m| m >= 16 && m < h) {
        Some(max) => ((w, h), (((w as u64 * max as u64 / h as u64) as u32) & !1, max & !1)),
        None => ((w, h), (w, h)),
    };
    let mut ovi = video_tap::scaled_ovi(base, w, h)?;
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

    let encoder = req.video_encoder.clone().unwrap_or_else(|| video_encoder_for(req));
    let settings = new_data()?;
    quality_settings(&encoder, req.quality, req.smaller, settings);
    let vid = CString::new(encoder.as_str()).map_err(|e| e.to_string())?;
    iso.video_enc = venc_create(vid.as_ptr(), name.as_ptr(), settings, std::ptr::null_mut());
    obs_data::release(settings);
    if iso.video_enc.is_null() {
        return Err(format!("video encoder '{encoder}' unavailable"));
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

/// The file OBS's recording is writing, while it records.
fn recording_path() -> Option<String> {
    let output = obs_frontend_get_recording_output()?();
    if output.is_null() {
        return None;
    }
    let settings = obs_output_get_settings().map_or(std::ptr::null_mut(), |f| f(output));
    let path = obs_data::get_string(settings, "path").filter(|p| !p.is_empty());
    obs_data::release(settings);
    if let Some(release) = video_tap::obs_output_release() {
        release(output);
    }
    path
}

/// `2026-10-31 17-30-00.mov` and "PiP 1": `2026-10-31 17-30-00 PiP 1.mov`.
fn beside(recording: &str, name: &str) -> String {
    let path = std::path::Path::new(recording);
    let stem = path.file_stem().map_or_else(String::new, |s| s.to_string_lossy().into_owned());
    let file = match path.extension() {
        Some(ext) => format!("{stem} {name}.{}", ext.to_string_lossy()),
        None => format!("{stem} {name}"),
    };
    path.with_file_name(file).to_string_lossy().into_owned()
}

/// Starts each armed recording that isn't running, beside OBS's file. On
/// OBS's UI thread.
fn start_armed() {
    let Some(recording) = recording_path() else {
        log_line("separate recordings: OBS's recording has no file path, none started");
        return;
    };
    let wanted: Vec<Start> = lock(&ARMED)
        .iter()
        .map(|a| Start { path: beside(&recording, &a.name), ..a.start.clone() })
        .collect();
    for req in wanted {
        if !lock(&ISOS).iter().any(|i| i.id == req.id) {
            let _ = start(&req);
        }
    }
}

// ---------------------------------------------------------------------
// OBS's recording starting and stopping, or OBS quitting
// ---------------------------------------------------------------------

// enum obs_frontend_event, by position (obs-frontend-api.h).
const EVENT_RECORDING_STARTED: c_int = 5;
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
            // The separate files start and end with OBS's own recording,
            // whoever started or stopped it, and end before OBS quits. What
            // is armed names a scene collection's sources, so it goes with it.
            match event {
                EVENT_RECORDING_STARTED => start_armed(),
                EVENT_RECORDING_STOPPING => stop_all(),
                EVENT_EXIT | EVENT_SCENE_COLLECTION_CLEANUP => {
                    stop_all();
                    lock(&ARMED).clear();
                    unregister_event_callback();
                }
                _ => {}
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
    Arm(Armed),
    Disarm,
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
                UiRequest::Arm(_) => {
                    let UiRequest::Arm(armed) = std::mem::replace(&mut call.request, UiRequest::Disarm) else {
                        unreachable!()
                    };
                    let mut list = lock(&ARMED);
                    list.retain(|a| a.start.id != armed.start.id);
                    list.push(armed);
                    drop(list);
                    register_event_callback();
                    // Armed while OBS records: it starts now, beside the file.
                    if obs_frontend_recording_active().is_some_and(|f| f()) {
                        start_armed();
                    }
                    Ok(())
                }
                UiRequest::Disarm => {
                    lock(&ARMED).clear();
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

/// Request: `{"id", "source", "path"}` required; optional: `"track"` (OBS's
/// audio track, 1 to 6: 2), `"max_height"`, `"smaller"`, `"quality"`,
/// `"video_encoder"` (else the one OBS's recording uses, else Apple's H.264
/// on the Mac and x264 elsewhere) and `"audio_encoder"` (AAC). See `Start`.
/// Response: `{"ok": true}` or `{"ok": false, "error": "..."}`.
pub extern "C" fn handle_start_iso(request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard(
        "handle_start_iso",
        (),
        std::panic::AssertUnwindSafe(|| {
            let request = obs_data::from_void(request_data);
            let response = obs_data::from_void(response_data);
            let result = match (parse_start(request), obs_data::get_string(request, "path").filter(|s| !s.is_empty())) {
                (Some(req), Some(path)) => run_on_ui(UiRequest::Start(Start { path, ..req })),
                _ => Err("id, source and path are required".into()),
            };
            answer(response, result);
        }),
    );
}

/// `{"id", "source"}` and the optional settings (`handle_start_iso`), the
/// path left empty. None without an id and a source.
fn parse_start(request: *mut ObsDataT) -> Option<Start> {
    let text = |key: &str| obs_data::get_string(request, key).filter(|s| !s.is_empty());
    Some(Start {
        id: text("id")?,
        source: text("source")?,
        track: obs_data::get_optional_int(request, "track").unwrap_or(2),
        path: String::new(),
        video_encoder: text("video_encoder"),
        audio_encoder: text("audio_encoder")
            .unwrap_or_else(|| if cfg!(target_os = "macos") { "CoreAudio_AAC" } else { "ffmpeg_aac" }.to_string()),
        max_height: obs_data::get_optional_int(request, "max_height").and_then(|m| u32::try_from(m).ok()),
        quality: obs_data::get_optional_int(request, "quality"),
        smaller: obs_data::get_optional_bool(request, "smaller").unwrap_or(false),
    })
}

fn answer(response: *mut ObsDataT, result: Result<(), String>) {
    match result {
        Ok(()) => obs_data::set_bool(response, "ok", true),
        Err(e) => {
            obs_data::set_bool(response, "ok", false);
            obs_data::set_string(response, "error", &e);
        }
    }
}

/// Request: `{"id", "source", "name"}` required, and `start_iso`'s optional
/// settings. From OBS's next recording on, or now if OBS is recording, the
/// source is recorded beside OBS's file, named after it with `name` added.
/// An id armed again is replaced. Response: `{"ok": true}` or `{"ok": false,
/// "error": "..."}`.
pub extern "C" fn handle_arm_iso(request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard(
        "handle_arm_iso",
        (),
        std::panic::AssertUnwindSafe(|| {
            let request = obs_data::from_void(request_data);
            let response = obs_data::from_void(response_data);
            let name = obs_data::get_string(request, "name").filter(|s| !s.is_empty());
            let result = match (parse_start(request), name) {
                (Some(start), Some(name)) => run_on_ui(UiRequest::Arm(Armed { start, name })),
                _ => Err("id, source and name are required".into()),
            };
            answer(response, result);
        }),
    );
}

/// Request: `{}`. Nothing is armed any more; files already recording go on
/// to the end of OBS's recording. Response: `{"ok": true}`.
pub extern "C" fn handle_disarm_isos(_request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard(
        "handle_disarm_isos",
        (),
        std::panic::AssertUnwindSafe(|| {
            answer(obs_data::from_void(response_data), run_on_ui(UiRequest::Disarm));
        }),
    );
}

/// Request: `{}`. Stops every separate recording. Response: `{"ok": true}`.
pub extern "C" fn handle_stop_iso(_request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard(
        "handle_stop_iso",
        (),
        std::panic::AssertUnwindSafe(|| {
            answer(obs_data::from_void(response_data), run_on_ui(UiRequest::Stop));
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A separate file sits beside OBS's, its name OBS's with the camera's
    /// added, its kind OBS's.
    #[test]
    fn a_separate_file_is_named_after_obss() {
        assert_eq!(beside("/rec/2026-10-31 17-30-00.mov", "PiP 1"), "/rec/2026-10-31 17-30-00 PiP 1.mov");
        assert_eq!(beside("/rec/show.mkv", "Main"), "/rec/show Main.mkv");
        assert_eq!(beside("/rec/show", "Main"), "/rec/show Main");
    }
}
