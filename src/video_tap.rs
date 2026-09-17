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

//! SPIKE (FrameSW MONITOR_LAYOUTS_PLAN.md, Phase 0): sends OBS's Preview
//! and Program over NDI as small NV12 confidence feeds, so FrameSW's
//! monitors stop depending on DistroAV. Built to be measured, not shipped.
//!
//! Every declaration below was checked against obs-studio **32.2.2** (the
//! installed OBS), fetched with curl on 2026-09-15: `libobs/obs.h`,
//! `obs.c`, `obs-view.c`, `obs-video.c`, `obs-source.c`, `obs-output.{h,c}`,
//! `obs-module.c`, `media-io/video-io.{h,c}`,
//! `frontend/api/obs-frontend-api.{h,cpp}`, `frontend/OBSStudioAPI.cpp`.
//!
//! Facts from that source that the code depends on:
//! - **A mix only downloads raw frames while its `raw_active` counter is
//!   above zero** (`obs-video.c`), and only `start_raw_video` raises it —
//!   not exported. Reachable two ways: `obs_add_raw_video_callback2` (main
//!   mix only), or an `obs_output` whose `begin_data_capture` runs it on the
//!   output's `video_t`. A bare `video_output_connect2` registers a callback
//!   that is never called; the first version of this spike did exactly that
//!   and measured 0 frames. `start_raw_video` carries a TODO to revert the
//!   counter once outputs "use views/canvasses" (obs-studio #12366) — the
//!   output route is the one expected to survive that.
//! - `obs_view_set_source` activates with the view's type; an
//!   `obs_view_create` view is `AUX_VIEW`, which only bumps `show_refs`,
//!   never `activate_refs`. Preview-only sources stay inactive.
//! - `obs_view_destroy` does NOT take the view out of the render loop;
//!   `obs_view_remove` must come first (both it and `output_frames` hold
//!   `mixes_mutex`).
//! - Output lifecycle: `obs_output_stop` calls `info.stop` synchronously,
//!   and `stop` must call `obs_output_end_data_capture` — that spawns the
//!   thread which disconnects the raw callback and signals
//!   `stopping_event`. `obs_output_destroy` (last `obs_output_release`)
//!   waits on that event and joins the thread before calling
//!   `info.destroy`, so after release returns no `raw_video` is running.
//!   A `stop` that skipped `end_data_capture` would hang OBS's UI thread.
//! - `obs_register_output_s` copies `size` bytes and checks required fields
//!   by offset, so registering only the prefix up to `raw_video` works on
//!   any libobs whose struct is at least that long.
//! - `obs_remove_raw_video_callback` → `video_output_disconnect2`, which
//!   takes the `input_mutex` the video-io thread holds across callbacks:
//!   synchronous.
//! - `obs_shutdown` calls `stop_video()` before unloading modules and frees
//!   video/data after, so `stop_all` from `obs_module_unload` is sound.
//! - `OBSStudioAPI::on_event` copies each callback entry before calling it,
//!   so removing our callback from inside it is safe. EXIT is emitted
//!   before `obs_frontend_set_callbacks_internal(nullptr)`.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::log_line;
use crate::ndi_ffi::NdiVideoSender;
use studio_mode_meters_core::metering::{
    ffi_guard, obs_frontend_get_current_preview_scene, obs_queue_task, obs_source_get_name,
    obs_source_release, ObsOutputT, ObsSourceT, MAX_AV_PLANES, OBS_OUTPUT_VIDEO, OBS_TASK_UI,
};
use studio_mode_meters_core::obs_data::{self, ObsDataT};

pub enum ObsViewT {}
pub enum VideoT {}

/// `libobs/obs.h` `struct obs_video_info` (the plugin is C, so the
/// `#ifndef SWIG` `graphics_module` field is present).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ObsVideoInfo {
    graphics_module: *const c_char,
    fps_num: u32,
    fps_den: u32,
    base_width: u32,
    base_height: u32,
    output_width: u32,
    output_height: u32,
    output_format: c_int, // enum video_format
    adapter: u32,
    gpu_conversion: bool,
    colorspace: c_int, // enum video_colorspace
    range: c_int,      // enum video_range_type
    scale_type: c_int, // enum obs_scale_type
}

/// `libobs/media-io/video-io.h` `struct video_scale_info`.
#[repr(C)]
struct VideoScaleInfo {
    format: c_int,
    width: u32,
    height: u32,
    range: c_int,
    colorspace: c_int,
}

/// `libobs/media-io/video-io.h` `struct video_data`.
#[repr(C)]
pub struct VideoData {
    data: [*mut u8; MAX_AV_PLANES],
    linesize: [u32; MAX_AV_PLANES],
    timestamp: u64,
}

/// `libobs/obs-output.h` `struct obs_output_info`, **prefix only**, up to
/// and including `raw_video` — every field `obs_register_output_s` requires
/// for a raw (non-encoded, non-service) video output. Registered with this
/// struct's size; libobs zero-fills the rest (`REGISTER_OBS_DEF`).
#[repr(C)]
struct ObsOutputInfoPrefix {
    id: *const c_char,
    flags: u32,
    get_name: extern "C" fn(*mut c_void) -> *const c_char,
    create: extern "C" fn(*mut ObsDataT, *mut ObsOutputT) -> *mut c_void,
    destroy: extern "C" fn(*mut c_void),
    start: extern "C" fn(*mut c_void) -> bool,
    stop: extern "C" fn(*mut c_void, u64),
    raw_video: extern "C" fn(*mut c_void, *mut VideoData),
}

// enum video_format: NONE, I420, NV12 ...
const VIDEO_FORMAT_NV12: c_int = 2;
// enum video_colorspace: DEFAULT, 601, 709, SRGB, 2100_PQ, 2100_HLG
const VIDEO_CS_DEFAULT: c_int = 0;
const VIDEO_CS_709: c_int = 2;
const VIDEO_CS_2100_PQ: c_int = 4;
const VIDEO_CS_2100_HLG: c_int = 5;
// enum video_range_type: DEFAULT, PARTIAL, FULL
const VIDEO_RANGE_DEFAULT: c_int = 0;
// enum obs_scale_type: DISABLE, POINT, BICUBIC, BILINEAR, LANCZOS, AREA
const OBS_SCALE_BICUBIC: c_int = 2;

// enum obs_frontend_event (frontend/api/obs-frontend-api.h), by position.
const EVENT_SCENE_CHANGED: c_int = 8;
const EVENT_TRANSITION_CHANGED: c_int = 10;
const EVENT_SCENE_COLLECTION_CHANGED: c_int = 13;
const EVENT_EXIT: c_int = 17;
const EVENT_STUDIO_MODE_ENABLED: c_int = 22;
const EVENT_STUDIO_MODE_DISABLED: c_int = 23;
const EVENT_PREVIEW_SCENE_CHANGED: c_int = 24;
const EVENT_SCENE_COLLECTION_CLEANUP: c_int = 25;

const OUTPUT_TYPE_ID: &CStr = c"framesw_video_feed";

type VideoOutputCb = extern "C" fn(*mut c_void, *mut VideoData);
type FrontendEventCb = extern "C" fn(c_int, *mut c_void);

studio_mode_meters_core::resolved_fn!(obs_get_video_info: extern "C" fn(*mut ObsVideoInfo) -> bool);
studio_mode_meters_core::resolved_fn!(obs_get_output_source: extern "C" fn(u32) -> *mut ObsSourceT);
studio_mode_meters_core::resolved_fn!(obs_view_create: extern "C" fn() -> *mut ObsViewT);
studio_mode_meters_core::resolved_fn!(obs_view_destroy: extern "C" fn(*mut ObsViewT));
studio_mode_meters_core::resolved_fn!(obs_view_set_source: extern "C" fn(*mut ObsViewT, u32, *mut ObsSourceT));
studio_mode_meters_core::resolved_fn!(obs_view_get_source: extern "C" fn(*mut ObsViewT, u32) -> *mut ObsSourceT);
studio_mode_meters_core::resolved_fn!(obs_view_add2: extern "C" fn(*mut ObsViewT, *mut ObsVideoInfo) -> *mut VideoT);
studio_mode_meters_core::resolved_fn!(obs_view_remove: extern "C" fn(*mut ObsViewT));
studio_mode_meters_core::resolved_fn!(
    obs_add_raw_video_callback2: extern "C" fn(*const VideoScaleInfo, u32, VideoOutputCb, *mut c_void)
);
studio_mode_meters_core::resolved_fn!(obs_remove_raw_video_callback: extern "C" fn(VideoOutputCb, *mut c_void));
studio_mode_meters_core::resolved_fn!(video_output_get_width: extern "C" fn(*const VideoT) -> u32);
studio_mode_meters_core::resolved_fn!(video_output_get_height: extern "C" fn(*const VideoT) -> u32);
studio_mode_meters_core::resolved_fn!(video_output_get_format: extern "C" fn(*const VideoT) -> c_int);
studio_mode_meters_core::resolved_fn!(obs_register_output_s: extern "C" fn(*const ObsOutputInfoPrefix, usize));
studio_mode_meters_core::resolved_fn!(
    obs_output_create: extern "C" fn(*const c_char, *const c_char, *mut ObsDataT, *mut ObsDataT) -> *mut ObsOutputT
);
studio_mode_meters_core::resolved_fn!(obs_output_release: extern "C" fn(*mut ObsOutputT));
studio_mode_meters_core::resolved_fn!(obs_output_start: extern "C" fn(*mut ObsOutputT) -> bool);
studio_mode_meters_core::resolved_fn!(obs_output_stop: extern "C" fn(*mut ObsOutputT));
// `audio_t *` — always null here; the output type has no OBS_OUTPUT_AUDIO.
studio_mode_meters_core::resolved_fn!(obs_output_set_media: extern "C" fn(*mut ObsOutputT, *mut VideoT, *mut c_void));
studio_mode_meters_core::resolved_fn!(obs_output_can_begin_data_capture: extern "C" fn(*const ObsOutputT, u32) -> bool);
studio_mode_meters_core::resolved_fn!(obs_output_begin_data_capture: extern "C" fn(*mut ObsOutputT, u32) -> bool);
studio_mode_meters_core::resolved_fn!(obs_output_end_data_capture: extern "C" fn(*mut ObsOutputT));
studio_mode_meters_core::resolved_fn!(obs_output_get_last_error: extern "C" fn(*mut ObsOutputT) -> *const c_char);
studio_mode_meters_core::resolved_fn!(obs_frontend_add_event_callback: extern "C" fn(FrontendEventCb, *mut c_void));
studio_mode_meters_core::resolved_fn!(obs_frontend_remove_event_callback: extern "C" fn(FrontendEventCb, *mut c_void));

const STATS_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Which {
    Preview,
    Program,
}

/// How frames leave this plugin. NDI stays only until the shared-memory
/// path has been measured against it on both platforms (Phase 0's numbers
/// are the baseline), then it goes - two transports is not a feature.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Transport {
    /// A memory-mapped ring FrameSW reads directly (`shm_ring.rs`).
    Shm,
    /// The Phase 0 spike's NDI sender, kept for comparison.
    Ndi,
}

impl Transport {
    fn label(self) -> &'static str {
        match self {
            Transport::Shm => "shm",
            Transport::Ndi => "ndi",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Path {
    /// Own `obs_view` + `obs_view_add2` mix + our output: GPU render/scale.
    View,
    /// `obs_add_raw_video_callback2` on the main mix: swscale on the CPU,
    /// on the video-io thread that also feeds OBS's raw encoders.
    Raw,
}

impl Path {
    fn label(self) -> &'static str {
        match self {
            Path::View => "view",
            Path::Raw => "raw",
        }
    }
}

struct Frame {
    data: Vec<u8>,
    timecode: i64,
}

#[derive(Default)]
struct Slot {
    frame: Option<Frame>,
    spare: Vec<Vec<u8>>,
}

#[derive(Default)]
struct Stats {
    frames_in: AtomicU64,
    frames_sent: AtomicU64,
    frames_dropped: AtomicU64,
    copy_ns: AtomicU64,
    send_ns: AtomicU64,
    send_max_ns: AtomicU64,
}

/// Everything the OBS video callback and the send worker share.
struct Shared {
    name: String,
    transport: Transport,
    /// Written directly from OBS's video callback when the transport is
    /// `Shm`: the copy into the ring IS the delivery, so there is no worker
    /// thread and no second copy. Only teardown ever contends this lock, and
    /// only after the callback has been disconnected.
    ring: Mutex<Option<crate::shm_ring::RingWriter>>,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    running: AtomicBool,
    slot: Mutex<Slot>,
    ready: Condvar,
    stats: Stats,
    source_name: Mutex<String>,
}

struct Feed {
    which: Which,
    path: Path,
    view: *mut ObsViewT,
    output: *mut ObsOutputT,
    raw_connected: bool,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

// SAFETY: the raw pointers are libobs handles only ever used on OBS's UI
// thread (every start/stop/refresh runs there) or under `FEEDS`.
unsafe impl Send for Feed {}

struct Feeds {
    preview: Option<Feed>,
    program: Option<Feed>,
}

static FEEDS: Mutex<Feeds> = Mutex::new(Feeds { preview: None, program: None });
static EVENT_CALLBACK_REGISTERED: AtomicBool = AtomicBool::new(false);
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);
static OUTPUT_TYPE_REGISTERED: AtomicBool = AtomicBool::new(false);
/// Handed from `start_feed` to `output_create`, which `obs_output_create`
/// calls synchronously on the same thread.
static PENDING_OUTPUT_SHARED: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_100ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_nanos() / 100) as i64)
        .unwrap_or(0)
}

fn cstr_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
}

fn source_name(source: *mut ObsSourceT) -> String {
    if source.is_null() {
        return String::new();
    }
    obs_source_get_name().map_or(String::new(), |f| cstr_to_string(f(source)))
}

fn release(source: *mut ObsSourceT) {
    if !source.is_null() {
        if let Some(release) = obs_source_release() {
            release(source);
        }
    }
}

// ---------------------------------------------------------------------
// OBS video thread → latest-frame slot → worker → NDI
// ---------------------------------------------------------------------

/// Runs on OBS's video-io thread, which must never wait on NDI: copy the
/// two NV12 planes out (the frame is only valid inside this call), park
/// them in the slot, wake the worker. An unsent frame already in the slot
/// is overwritten and counted as dropped.
fn copy_frame(shared: &Shared, frame: *mut VideoData) {
    if frame.is_null() || !shared.running.load(Ordering::Relaxed) {
        return;
    }
    let frame = unsafe { &*frame };
    let (w, h) = (shared.width as usize, shared.height as usize);
    let (y_stride, uv_stride) = (frame.linesize[0] as usize, frame.linesize[1] as usize);
    if frame.data[0].is_null() || frame.data[1].is_null() || y_stride < w || uv_stride < w {
        return;
    }
    let started = Instant::now();
    if shared.transport == Transport::Shm {
        // Straight into the ring, on this thread: a memcpy of 345 KB at
        // 640x360, no encode, nothing to wake.
        if let Some(ring) = lock(&shared.ring).as_mut() {
            // SAFETY: libobs's own planes, valid for this call, and the
            // strides are the ones it just reported.
            unsafe {
                ring.write_frame(frame.data[0], y_stride, frame.data[1], uv_stride, now_100ns() as u64);
            }
            shared.stats.frames_in.fetch_add(1, Ordering::Relaxed);
            shared.stats.frames_sent.fetch_add(1, Ordering::Relaxed);
            shared.stats.copy_ns.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
        return;
    }
    let y_len = w * h;
    let mut buf = lock(&shared.slot).spare.pop().unwrap_or_default();
    buf.resize(y_len + w * (h / 2), 0);
    unsafe {
        for row in 0..h {
            std::ptr::copy_nonoverlapping(frame.data[0].add(row * y_stride), buf.as_mut_ptr().add(row * w), w);
        }
        for row in 0..h / 2 {
            std::ptr::copy_nonoverlapping(frame.data[1].add(row * uv_stride), buf.as_mut_ptr().add(y_len + row * w), w);
        }
    }
    let new = Frame { data: buf, timecode: now_100ns() };
    {
        let mut slot = lock(&shared.slot);
        if let Some(old) = slot.frame.replace(new) {
            shared.stats.frames_dropped.fetch_add(1, Ordering::Relaxed);
            slot.spare.push(old.data);
        }
    }
    shared.ready.notify_one();
    shared.stats.frames_in.fetch_add(1, Ordering::Relaxed);
    shared.stats.copy_ns.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

/// Raw-path callback; `param` is `Arc::as_ptr` of the feed's `Shared`, kept
/// alive by the `Feed` until `obs_remove_raw_video_callback` has returned.
extern "C" fn on_raw_video_frame(param: *mut c_void, frame: *mut VideoData) {
    ffi_guard(
        "video_tap::on_raw_video_frame",
        (),
        std::panic::AssertUnwindSafe(|| {
            if !param.is_null() {
                copy_frame(unsafe { &*param.cast::<Shared>() }, frame);
            }
        }),
    );
}

fn worker_loop(shared: Arc<Shared>, sender: Option<NdiVideoSender>) {
    let mut last_log = Instant::now();
    let mut logged = [0u64; 5];
    loop {
        let frame = {
            let mut slot = lock(&shared.slot);
            if slot.frame.is_none() && shared.running.load(Ordering::Relaxed) {
                slot = shared
                    .ready
                    .wait_timeout(slot, Duration::from_millis(100))
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            slot.frame.take()
        };
        if !shared.running.load(Ordering::Relaxed) {
            break;
        }
        if let (Some(mut frame), Some(sender)) = (frame, sender.as_ref()) {
            let started = Instant::now();
            sender.send_nv12(shared.width, shared.height, shared.fps_num, shared.fps_den, &mut frame.data, frame.timecode);
            let ns = started.elapsed().as_nanos() as u64;
            shared.stats.frames_sent.fetch_add(1, Ordering::Relaxed);
            shared.stats.send_ns.fetch_add(ns, Ordering::Relaxed);
            shared.stats.send_max_ns.fetch_max(ns, Ordering::Relaxed);
            let mut slot = lock(&shared.slot);
            if slot.spare.len() < 2 {
                slot.spare.push(frame.data);
            }
        }
        if last_log.elapsed() >= STATS_INTERVAL {
            let now = snapshot(&shared.stats);
            let d: Vec<u64> = now.iter().zip(logged).map(|(n, l)| n - l).collect();
            let secs = last_log.elapsed().as_secs_f64();
            log_line(&format!(
                "video feed '{}' {}x{} ({}): in={} ({:.1} fps) sent={} dropped={} copy_avg={}us send_avg={}us send_max={}us",
                shared.name,
                shared.width,
                shared.height,
                lock(&shared.source_name),
                d[0],
                d[0] as f64 / secs,
                d[1],
                d[2],
                d[3] / d[0].max(1) / 1000,
                d[4] / d[1].max(1) / 1000,
                shared.stats.send_max_ns.swap(0, Ordering::Relaxed) / 1000,
            ));
            logged = now;
            last_log = Instant::now();
        }
    }
    // Destroyed here, before `teardown`'s join returns — so a restart can
    // reuse the NDI name immediately.
    drop(sender);
}

/// frames_in, frames_sent, frames_dropped, copy_ns, send_ns.
fn snapshot(s: &Stats) -> [u64; 5] {
    [
        s.frames_in.load(Ordering::Relaxed),
        s.frames_sent.load(Ordering::Relaxed),
        s.frames_dropped.load(Ordering::Relaxed),
        s.copy_ns.load(Ordering::Relaxed),
        s.send_ns.load(Ordering::Relaxed),
    ]
}

// ---------------------------------------------------------------------
// The output type that raises a view mix's raw_active (module doc)
// ---------------------------------------------------------------------

struct OutputCtx {
    output: *mut ObsOutputT,
    shared: Arc<Shared>,
}

extern "C" fn output_get_name(_type_data: *mut c_void) -> *const c_char {
    c"FrameSW video feed".as_ptr()
}

extern "C" fn output_create(_settings: *mut ObsDataT, output: *mut ObsOutputT) -> *mut c_void {
    ffi_guard(
        "video_tap::output_create",
        std::ptr::null_mut(),
        std::panic::AssertUnwindSafe(|| {
            // Null (-> "Failed to create output") if not created by start_feed.
            let Some(shared) = lock(&PENDING_OUTPUT_SHARED).take() else {
                return std::ptr::null_mut();
            };
            Box::into_raw(Box::new(OutputCtx { output, shared })).cast()
        }),
    )
}

extern "C" fn output_destroy(data: *mut c_void) {
    ffi_guard(
        "video_tap::output_destroy",
        (),
        std::panic::AssertUnwindSafe(|| {
            if !data.is_null() {
                drop(unsafe { Box::from_raw(data.cast::<OutputCtx>()) });
            }
        }),
    );
}

extern "C" fn output_start(data: *mut c_void) -> bool {
    ffi_guard(
        "video_tap::output_start",
        false,
        std::panic::AssertUnwindSafe(|| {
            if data.is_null() {
                return false;
            }
            let ctx = unsafe { &*data.cast::<OutputCtx>() };
            let (Some(can_begin), Some(begin)) = (obs_output_can_begin_data_capture(), obs_output_begin_data_capture())
            else {
                return false;
            };
            can_begin(ctx.output, 0) && begin(ctx.output, 0)
        }),
    )
}

/// Must end data capture: that is what signals `stopping_event`, which
/// `obs_output_destroy` waits on (module doc).
extern "C" fn output_stop(data: *mut c_void, _ts: u64) {
    ffi_guard(
        "video_tap::output_stop",
        (),
        std::panic::AssertUnwindSafe(|| {
            if data.is_null() {
                return;
            }
            let ctx = unsafe { &*data.cast::<OutputCtx>() };
            if let Some(end) = obs_output_end_data_capture() {
                end(ctx.output);
            }
        }),
    );
}

extern "C" fn output_raw_video(data: *mut c_void, frame: *mut VideoData) {
    ffi_guard(
        "video_tap::output_raw_video",
        (),
        std::panic::AssertUnwindSafe(|| {
            if !data.is_null() {
                copy_frame(&unsafe { &*data.cast::<OutputCtx>() }.shared, frame);
            }
        }),
    );
}

/// Called from `obs_module_load` — libobs only accepts output types there.
pub fn register_output_type() {
    let Some(register) = obs_register_output_s() else {
        log_line("obs_register_output_s unavailable — Preview video feed disabled");
        return;
    };
    let info = ObsOutputInfoPrefix {
        id: OUTPUT_TYPE_ID.as_ptr(),
        flags: OBS_OUTPUT_VIDEO,
        get_name: output_get_name,
        create: output_create,
        destroy: output_destroy,
        start: output_start,
        stop: output_stop,
        raw_video: output_raw_video,
    };
    register(&info, std::mem::size_of::<ObsOutputInfoPrefix>());
    OUTPUT_TYPE_REGISTERED.store(true, Ordering::Relaxed);
}

// ---------------------------------------------------------------------
// Start / stop — UI thread only
// ---------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Request {
    preview: bool,
    program: bool,
    transport: Transport,
    width: u32,
    height: u32,
    program_path: Path,
}

/// Filename half of a feed's ring (`shm_ring::ring_path`). Short and
/// stable: FrameSW builds the same path from the same key.
fn feed_key(which: Which) -> &'static str {
    match which {
        Which::Preview => "preview",
        Which::Program => "program",
    }
}

fn ndi_name(which: Which) -> &'static str {
    match which {
        Which::Preview => "FrameSW Preview",
        Which::Program => "FrameSW Program",
    }
}

fn main_ovi() -> Result<ObsVideoInfo, String> {
    let get = obs_get_video_info().ok_or("obs_get_video_info unavailable")?;
    // SAFETY: all-zero is a valid `obs_video_info` (null pointer, zeros, false).
    let mut ovi: ObsVideoInfo = unsafe { std::mem::zeroed() };
    if !get(&mut ovi) {
        return Err("obs_get_video_info returned false (video not initialised)".into());
    }
    Ok(ovi)
}

/// The main mix's settings with a small NV12 output: base size stays the
/// canvas (scenes render in canvas coordinates), the GPU scales down.
fn small_ovi(width: u32, height: u32) -> Result<ObsVideoInfo, String> {
    let mut ovi = main_ovi()?;
    ovi.output_width = width;
    ovi.output_height = height;
    ovi.output_format = VIDEO_FORMAT_NV12;
    ovi.gpu_conversion = true;
    if ovi.colorspace == VIDEO_CS_2100_PQ || ovi.colorspace == VIDEO_CS_2100_HLG {
        ovi.colorspace = VIDEO_CS_709;
    }
    ovi.scale_type = OBS_SCALE_BICUBIC;
    Ok(ovi)
}

/// The source a feed's view should show right now (a new reference, or null).
fn wanted_source(which: Which) -> *mut ObsSourceT {
    match which {
        // Null outside Studio Mode (OBSStudioAPI::obs_frontend_get_current_preview_scene).
        Which::Preview => obs_frontend_get_current_preview_scene().map_or(std::ptr::null_mut(), |f| f()),
        // Channel 0 is the frontend's transition, i.e. what Program shows.
        Which::Program => obs_get_output_source().map_or(std::ptr::null_mut(), |f| f(0)),
    }
}

fn start_feed(
    which: Which,
    path: Path,
    transport: Transport,
    width: u32,
    height: u32,
) -> Result<Feed, String> {
    let ovi = main_ovi()?;
    let fps_num = ovi.fps_num.max(1);
    let fps_den = ovi.fps_den.max(1);
    // Whichever transport it is, fail on it BEFORE touching OBS's render
    // loop: a half-attached feed is the state worth never reaching.
    let (sender, ring) = match transport {
        Transport::Shm => (
            None,
            Some(crate::shm_ring::RingWriter::create(feed_key(which), width, height, fps_num, fps_den)?),
        ),
        Transport::Ndi => (Some(NdiVideoSender::new(ndi_name(which))?), None),
    };
    let ring_path = ring
        .as_ref()
        .map(|r| r.path().display().to_string())
        .unwrap_or_default();
    let shared = Arc::new(Shared {
        name: ndi_name(which).to_string(),
        transport,
        ring: Mutex::new(ring),
        width,
        height,
        fps_num,
        fps_den,
        running: AtomicBool::new(true),
        slot: Mutex::new(Slot::default()),
        ready: Condvar::new(),
        stats: Stats::default(),
        source_name: Mutex::new(String::new()),
    });
    let worker_shared = Arc::clone(&shared);
    // The worker sends NDI frames; with the ring there is nothing to send,
    // so it only reports the 10-second counters.
    let worker = std::thread::Builder::new()
        .name(format!("framesw-video-{}", path.label()))
        .spawn(move || worker_loop(worker_shared, sender))
        .map_err(|e| format!("spawning send worker failed: {e}"))?;
    let mut feed = Feed {
        which,
        path,
        view: std::ptr::null_mut(),
        output: std::ptr::null_mut(),
        raw_connected: false,
        shared,
        worker: Some(worker),
    };
    match attach(&mut feed) {
        Ok(()) => {
            log_line(&format!(
                "video feed '{}' started ({} path, {} transport{}, source '{}')",
                feed.shared.name,
                path.label(),
                transport.label(),
                if ring_path.is_empty() { String::new() } else { format!(" at {ring_path}") },
                lock(&feed.shared.source_name)
            ));
            Ok(feed)
        }
        Err(e) => {
            teardown(feed);
            Err(e)
        }
    }
}

/// Hooks `feed` into OBS. Leaves whatever it did set in `feed`, so the
/// caller's `teardown` undoes exactly that on failure.
fn attach(feed: &mut Feed) -> Result<(), String> {
    let (width, height) = (feed.shared.width, feed.shared.height);
    match feed.path {
        Path::Raw => {
            let add = obs_add_raw_video_callback2().ok_or("obs_add_raw_video_callback2 unavailable")?;
            let conversion = VideoScaleInfo {
                format: VIDEO_FORMAT_NV12,
                width,
                height,
                range: VIDEO_RANGE_DEFAULT,
                colorspace: VIDEO_CS_DEFAULT,
            };
            *lock(&feed.shared.source_name) = "main mix".into();
            let ovi = main_ovi()?;
            log_line(&format!(
                "video feed '{}' (raw): main mix {}x{} format={} -> swscale to {width}x{height} NV12",
                feed.shared.name, ovi.output_width, ovi.output_height, ovi.output_format
            ));
            // void: no failure signal; frames_in tells.
            add(&conversion, 1, on_raw_video_frame, Arc::as_ptr(&feed.shared).cast_mut().cast());
            feed.raw_connected = true;
            Ok(())
        }
        Path::View => {
            if !OUTPUT_TYPE_REGISTERED.load(Ordering::Relaxed) {
                return Err("output type not registered".into());
            }
            let (Some(create), Some(set_source), Some(add2)) = (obs_view_create(), obs_view_set_source(), obs_view_add2())
            else {
                return Err("obs_view_* unavailable".into());
            };
            let (Some(output_create_fn), Some(set_media), Some(start)) =
                (obs_output_create(), obs_output_set_media(), obs_output_start())
            else {
                return Err("obs_output_* unavailable".into());
            };
            let mut ovi = small_ovi(width, height)?;
            feed.view = create();
            if feed.view.is_null() {
                return Err("obs_view_create returned null".into());
            }
            let source = wanted_source(feed.which);
            set_source(feed.view, 0, source);
            *lock(&feed.shared.source_name) = source_name(source);
            release(source);

            let video = add2(feed.view, &mut ovi);
            if video.is_null() {
                // Never added to the mixes list; teardown's remove is a no-op.
                return Err("obs_view_add2 returned null".into());
            }
            if let (Some(w), Some(h), Some(f)) = (video_output_get_width(), video_output_get_height(), video_output_get_format()) {
                log_line(&format!(
                    "video feed '{}' (view): own mix video_t {}x{} format={}",
                    feed.shared.name,
                    w(video),
                    h(video),
                    f(video)
                ));
            }

            let name = CString::new(format!("{} feed", feed.shared.name)).map_err(|e| e.to_string())?;
            *lock(&PENDING_OUTPUT_SHARED) = Some(Arc::clone(&feed.shared));
            feed.output = output_create_fn(OUTPUT_TYPE_ID.as_ptr(), name.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut());
            let not_consumed = lock(&PENDING_OUTPUT_SHARED).take().is_some();
            if feed.output.is_null() {
                return Err("obs_output_create returned null".into());
            }
            if not_consumed {
                return Err("output create callback never ran".into());
            }
            set_media(feed.output, video, std::ptr::null_mut());
            if !start(feed.output) {
                let err = obs_output_get_last_error().map_or(String::new(), |f| cstr_to_string(f(feed.output)));
                return Err(format!("obs_output_start failed: {err}"));
            }
            Ok(())
        }
    }
}

/// Order matters (module doc): stop frames at the source, take the view
/// out of the render loop, destroy it, then join the worker.
fn teardown(mut feed: Feed) {
    feed.shared.running.store(false, Ordering::Relaxed);
    if feed.raw_connected {
        if let Some(remove) = obs_remove_raw_video_callback() {
            remove(on_raw_video_frame, Arc::as_ptr(&feed.shared).cast_mut().cast());
        }
        feed.raw_connected = false;
    }
    if !feed.output.is_null() {
        if let Some(stop) = obs_output_stop() {
            stop(feed.output);
        }
        // Last reference: waits for end_data_capture, then output_destroy.
        if let Some(release) = obs_output_release() {
            release(feed.output);
        }
        feed.output = std::ptr::null_mut();
    }
    if !feed.view.is_null() {
        if let Some(remove) = obs_view_remove() {
            remove(feed.view);
        }
        if let Some(destroy) = obs_view_destroy() {
            destroy(feed.view);
        }
        feed.view = std::ptr::null_mut();
    }
    feed.shared.ready.notify_all();
    if let Some(handle) = feed.worker.take() {
        let _ = handle.join();
    }
    let s = snapshot(&feed.shared.stats);
    log_line(&format!(
        "video feed '{}' stopped ({:?}, {} path): in={} sent={} dropped={}",
        feed.shared.name,
        feed.which,
        feed.path.label(),
        s[0],
        s[1],
        s[2]
    ));
}

fn reconcile(
    slot: &mut Option<Feed>,
    which: Which,
    want: bool,
    path: Path,
    transport: Transport,
    width: u32,
    height: u32,
) -> Result<(), String> {
    let matches = slot.as_ref().is_some_and(|f| {
        f.path == path
            && f.shared.transport == transport
            && f.shared.width == width
            && f.shared.height == height
    });
    if want && matches {
        return Ok(());
    }
    if let Some(old) = slot.take() {
        teardown(old);
    }
    if want {
        *slot = Some(start_feed(which, path, transport, width, height)?);
    }
    Ok(())
}

fn apply(req: Request) -> Result<(), String> {
    if SHUTTING_DOWN.load(Ordering::Relaxed) {
        return Err("OBS is exiting".into());
    }
    let mut feeds = lock(&FEEDS);
    let preview = reconcile(
        &mut feeds.preview,
        Which::Preview,
        req.preview,
        Path::View,
        req.transport,
        req.width,
        req.height,
    );
    let program = reconcile(
        &mut feeds.program,
        Which::Program,
        req.program,
        req.program_path,
        req.transport,
        req.width,
        req.height,
    );
    let any = feeds.preview.is_some() || feeds.program.is_some();
    drop(feeds);
    if any {
        register_event_callback();
    } else {
        unregister_event_callback();
    }
    match (preview, program) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), Ok(())) => Err(format!("preview: {e}")),
        (Ok(()), Err(e)) => Err(format!("program: {e}")),
        (Err(a), Err(b)) => Err(format!("preview: {a}; program: {b}")),
    }
}

/// Stops every feed. Safe from `obs_module_unload` (see module doc).
pub fn stop_all() {
    let mut feeds = lock(&FEEDS);
    for feed in [feeds.preview.take(), feeds.program.take()].into_iter().flatten() {
        teardown(feed);
    }
}

// ---------------------------------------------------------------------
// Following scene changes — frontend events arrive on the UI thread
// ---------------------------------------------------------------------

fn refresh_view(feed: &Feed, clear: bool) {
    if feed.view.is_null() {
        return;
    }
    let (Some(get_source), Some(set_source)) = (obs_view_get_source(), obs_view_set_source()) else {
        return;
    };
    let wanted = if clear { std::ptr::null_mut() } else { wanted_source(feed.which) };
    // obs_view_get_source returns a new reference; compare, then drop it.
    let current = get_source(feed.view, 0);
    let same = current == wanted;
    release(current);
    if !same {
        set_source(feed.view, 0, wanted);
        let name = source_name(wanted);
        log_line(&format!("video feed '{}' now shows '{}'", feed.shared.name, name));
        *lock(&feed.shared.source_name) = name;
    }
    release(wanted);
}

extern "C" fn on_frontend_event(event: c_int, _private_data: *mut c_void) {
    ffi_guard(
        "video_tap::on_frontend_event",
        (),
        std::panic::AssertUnwindSafe(|| match event {
            EVENT_EXIT => {
                SHUTTING_DOWN.store(true, Ordering::Relaxed);
                stop_all();
                unregister_event_callback();
            }
            EVENT_SCENE_COLLECTION_CLEANUP => {
                let feeds = lock(&FEEDS);
                for feed in [&feeds.preview, &feeds.program].into_iter().flatten() {
                    refresh_view(feed, true);
                }
            }
            EVENT_PREVIEW_SCENE_CHANGED
            | EVENT_SCENE_CHANGED
            | EVENT_TRANSITION_CHANGED
            | EVENT_STUDIO_MODE_ENABLED
            | EVENT_STUDIO_MODE_DISABLED
            | EVENT_SCENE_COLLECTION_CHANGED => {
                let feeds = lock(&FEEDS);
                for feed in [&feeds.preview, &feeds.program].into_iter().flatten() {
                    refresh_view(feed, false);
                }
            }
            _ => {}
        }),
    );
}

fn register_event_callback() {
    if EVENT_CALLBACK_REGISTERED.swap(true, Ordering::Relaxed) {
        return;
    }
    match obs_frontend_add_event_callback() {
        Some(add) => add(on_frontend_event, std::ptr::null_mut()),
        None => log_line("obs_frontend_add_event_callback unavailable — video feeds won't follow scene changes"),
    }
}

fn unregister_event_callback() {
    if !EVENT_CALLBACK_REGISTERED.swap(false, Ordering::Relaxed) {
        return;
    }
    if let Some(remove) = obs_frontend_remove_event_callback() {
        remove(on_frontend_event, std::ptr::null_mut());
    }
}

// ---------------------------------------------------------------------
// Vendor requests
// ---------------------------------------------------------------------

struct UiCall {
    ran: bool,
    request: Option<Request>, // None = stop
    result: Result<(), String>,
}

extern "C" fn run_on_ui_thread(param: *mut c_void) {
    ffi_guard(
        "video_tap::run_on_ui_thread",
        (),
        std::panic::AssertUnwindSafe(|| {
            if param.is_null() {
                return;
            }
            let call = unsafe { &mut *param.cast::<UiCall>() };
            call.ran = true;
            call.result = match call.request {
                Some(req) => apply(req),
                None => {
                    stop_all();
                    unregister_event_callback();
                    Ok(())
                }
            };
        }),
    );
}

fn run_on_ui(request: Option<Request>) -> Result<(), String> {
    let queue = obs_queue_task().ok_or("obs_queue_task unavailable")?;
    let mut call = UiCall { ran: false, request, result: Ok(()) };
    queue(OBS_TASK_UI, run_on_ui_thread, (&mut call as *mut UiCall).cast(), true);
    if !call.ran {
        return Err("UI task handler unavailable".into());
    }
    call.result
}

fn write_status(response: *mut ObsDataT) {
    let feeds = lock(&FEEDS);
    for (key, feed) in [("preview", &feeds.preview), ("program", &feeds.program)] {
        let Some(feed) = feed else {
            obs_data::set_bool(response, &format!("{key}_active"), false);
            continue;
        };
        let s = snapshot(&feed.shared.stats);
        obs_data::set_bool(response, &format!("{key}_active"), true);
        obs_data::set_string(response, &format!("{key}_ndi_name"), &feed.shared.name);
        obs_data::set_string(response, &format!("{key}_path"), feed.path.label());
        obs_data::set_string(response, &format!("{key}_transport"), feed.shared.transport.label());
        obs_data::set_string(
            response,
            &format!("{key}_ring"),
            &lock(&feed.shared.ring)
                .as_ref()
                .map(|r| r.path().display().to_string())
                .unwrap_or_default(),
        );
        obs_data::set_string(response, &format!("{key}_source"), &lock(&feed.shared.source_name));
        obs_data::set_int(response, &format!("{key}_width"), feed.shared.width as i64);
        obs_data::set_int(response, &format!("{key}_height"), feed.shared.height as i64);
        obs_data::set_int(response, &format!("{key}_fps_num"), feed.shared.fps_num as i64);
        obs_data::set_int(response, &format!("{key}_fps_den"), feed.shared.fps_den as i64);
        obs_data::set_int(response, &format!("{key}_frames_in"), s[0] as i64);
        obs_data::set_int(response, &format!("{key}_frames_sent"), s[1] as i64);
        obs_data::set_int(response, &format!("{key}_frames_dropped"), s[2] as i64);
        obs_data::set_int(response, &format!("{key}_copy_ns_total"), s[3] as i64);
        obs_data::set_int(response, &format!("{key}_send_ns_total"), s[4] as i64);
    }
}

/// Request: `{"preview": bool, "program": bool, "width": int, "height": int,
/// "program_path": "raw"|"view"}` — all optional (both feeds, 640x360,
/// raw). Sets the desired state: a feed not asked for is stopped, a running
/// feed with the same settings is left alone. Response: `{"ok": bool,
/// "error"?: string}` plus the `video_feed_status` fields.
pub extern "C" fn handle_start_video_feed(request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard(
        "handle_start_video_feed",
        (),
        std::panic::AssertUnwindSafe(|| {
            let request = obs_data::from_void(request_data);
            let response = obs_data::from_void(response_data);
            // Even sizes: NV12 chroma is subsampled 2x2.
            let even = |v: i64| (v.clamp(16, 3840) as u32) & !1;
            let req = Request {
                preview: obs_data::get_optional_bool(request, "preview").unwrap_or(true),
                program: obs_data::get_optional_bool(request, "program").unwrap_or(true),
                width: even(obs_data::get_optional_int(request, "width").unwrap_or(640)),
                height: even(obs_data::get_optional_int(request, "height").unwrap_or(360)),
                program_path: match obs_data::get_string(request, "program_path").as_deref() {
                    Some("view") => Path::View,
                    _ => Path::Raw,
                },
                // Shared memory unless something explicitly asks for the old
                // NDI path, which exists only for comparison now.
                transport: match obs_data::get_string(request, "transport").as_deref() {
                    Some("ndi") => Transport::Ndi,
                    _ => Transport::Shm,
                },
            };
            match run_on_ui(Some(req)) {
                Ok(()) => obs_data::set_bool(response, "ok", true),
                Err(e) => {
                    log_line(&format!("start_video_feed failed: {e}"));
                    obs_data::set_bool(response, "ok", false);
                    obs_data::set_string(response, "error", &e);
                }
            }
            write_status(response);
        }),
    );
}

/// Request: `{}`. Response: `{"ok": bool, "error"?: string}`.
pub extern "C" fn handle_stop_video_feed(_request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard(
        "handle_stop_video_feed",
        (),
        std::panic::AssertUnwindSafe(|| {
            let response = obs_data::from_void(response_data);
            match run_on_ui(None) {
                Ok(()) => obs_data::set_bool(response, "ok", true),
                Err(e) => {
                    obs_data::set_bool(response, "ok", false);
                    obs_data::set_string(response, "error", &e);
                }
            }
        }),
    );
}

/// Request: `{}`. Response: `{"ok": true, "<preview|program>_active": bool,
/// ...counters}` — cumulative counters; the caller diffs two samples.
pub extern "C" fn handle_video_feed_status(_request_data: *mut c_void, response_data: *mut c_void, _priv: *mut c_void) {
    ffi_guard(
        "handle_video_feed_status",
        (),
        std::panic::AssertUnwindSafe(|| {
            let response = obs_data::from_void(response_data);
            obs_data::set_bool(response, "ok", true);
            write_status(response);
        }),
    );
}
