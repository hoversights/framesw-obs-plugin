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

//! Minimal, runtime-loaded NDI send-audio FFI — only what a passive audio
//! monitor tap needs (create a sender, push one audio frame, tear it
//! down). Every struct/function signature here is copied verbatim from
//! the real NDI 6 SDK headers — `Processing.NDI.Send.h`,
//! `Processing.NDI.structs.h`, `Processing.NDI.Lib.h` (checked directly
//! against a local NDI 6.3.2.0 SDK install) — and confirmed present as
//! flat exported C symbols in `libndi.dylib` via `nm -gU` (not just the
//! aggregated `NDIlib_v6` function-pointer-table loader), so the same
//! "resolve by name" approach `platform.rs` already uses for libobs
//! applies here too — just against an explicitly-`dlopen`ed library
//! handle instead of the current process's own already-loaded modules,
//! since libndi is never loaded into OBS itself.
//!
//! Deliberately **not** the `grafton-ndi` crate (what the main FrameSW
//! app uses for its own NDI receive/monitor windows): that needs the
//! full NDI SDK — headers plus `bindgen` — installed on the *build*
//! machine, which would force an NDI SDK install step onto this plugin's
//! hosted CI runners, a real new dependency this plugin's "builds with
//! stable Rust alone, nothing platform-specific to link" design
//! deliberately avoids (see `platform.rs`'s module doc). Raw FFI against
//! the already-installed *runtime* library needs nothing at build time;
//! at plugin-load time it needs the NDI Runtime already on the machine —
//! the same redistributable `grafton-ndi` needs present at the app's own
//! *run*time, bundled by FrameSW's installer either way. A missing
//! runtime degrades to "no monitor audio," never a crash — same rule as
//! every other optional symbol in this crate.

use std::ffi::{c_char, CString};
use std::sync::OnceLock;

/// `Processing.NDI.Send.h`: `typedef struct NDIlib_send_instance_type* NDIlib_send_instance_t;`
/// — opaque, we only ever hold the pointer.
pub enum NdiSendInstanceT {}

/// `Processing.NDI.Send.h`'s `NDIlib_send_create_t` — field order/types
/// verbatim. The two C++-only default-constructor overloads (gated by
/// `NDILIB_CPP_DEFAULT_CONSTRUCTORS`) don't exist in the C ABI this
/// struct actually has; every field must be supplied explicitly.
#[repr(C)]
struct NdiSendCreate {
    p_ndi_name: *const c_char,
    p_groups: *const c_char,
    clock_video: bool,
    clock_audio: bool,
}

/// `Processing.NDI.structs.h`'s `NDIlib_audio_frame_v2_t` — field
/// order/types verbatim, including the two `int64_t` fields NDI's own
/// header documents as receive-only (`timecode`/`timestamp`); left at 0
/// on send, matching every other NDI sender that doesn't need custom
/// timecoding (NDI synthesizes one from the local clock when 0/undefined
/// is supplied, per the header's own doc comment on `clock_audio`).
#[repr(C)]
pub struct NdiAudioFrameV2 {
    pub sample_rate: i32,
    pub no_channels: i32,
    pub no_samples: i32,
    pub timecode: i64,
    pub p_data: *mut f32,
    pub channel_stride_in_bytes: i32,
    pub p_metadata: *const c_char,
    pub timestamp: i64,
}

type NdiInitializeFn = extern "C" fn() -> bool;
type NdiSendCreateFn = extern "C" fn(*const NdiSendCreate) -> *mut NdiSendInstanceT;
type NdiSendDestroyFn = extern "C" fn(*mut NdiSendInstanceT);
type NdiSendSendAudioV2Fn = extern "C" fn(*mut NdiSendInstanceT, *const NdiAudioFrameV2);

/// Only `send_create`/`send_destroy`/`send_send_audio_v2` are kept past
/// load time — `NDIlib_initialize` is called exactly once, during
/// `load_ndi` itself, and NDI's own docs don't require calling it again.
struct NdiLib {
    send_create: NdiSendCreateFn,
    send_destroy: NdiSendDestroyFn,
    send_send_audio_v2: NdiSendSendAudioV2Fn,
}

// SAFETY: every field is a plain `extern "C" fn` pointer into a shared
// library that, once `dlopen`/`LoadLibraryW`ed, stays loaded and
// immutable for the rest of the process's life — no interior mutability,
// safe to call from any thread (NDI's own docs describe `NDIlib_send_*`
// as thread-safe when used on distinct instances, and this crate never
// shares one `NdiSendInstanceT` across concurrent callers without its own
// synchronization — see `audio_tap.rs`).
unsafe impl Sync for NdiLib {}

static NDI: OnceLock<Option<NdiLib>> = OnceLock::new();

/// NDI's own documented redistributable-discovery convention
/// (`Processing.NDI.Lib.h`: `NDILIB_REDIST_FOLDER "NDI_RUNTIME_DIR_V6"`)
/// — checked first; the hardcoded fallbacks below are the well-known
/// default install locations for when that variable isn't set. The
/// macOS fallback (`/usr/local/lib/libndi.dylib`) is confirmed against a
/// real local install (the NDI Runtime installer creates exactly this
/// symlink); the Windows fallback matches the NDI Runtime installer's
/// documented default but is unverified against a real Windows machine —
/// same honesty standard as the rest of this crate's Windows story (see
/// `WINDOWS_HANDOFF.md`).
fn candidate_paths() -> Vec<String> {
    let mut paths = Vec::new();
    if let Ok(dir) = std::env::var("NDI_RUNTIME_DIR_V6") {
        #[cfg(target_os = "macos")]
        paths.push(format!("{dir}/libndi.dylib"));
        #[cfg(target_os = "windows")]
        paths.push(format!("{dir}\\Processing.NDI.Lib.x64.dll"));
    }
    #[cfg(target_os = "macos")]
    {
        paths.push("/usr/local/lib/libndi.dylib".to_string());
        paths.push("libndi.dylib".to_string());
    }
    #[cfg(target_os = "windows")]
    {
        paths.push(
            "C:\\Program Files\\NDI\\NDI 6 Runtime\\v6\\Processing.NDI.Lib.x64.dll".to_string(),
        );
        paths.push("Processing.NDI.Lib.x64.dll".to_string());
    }
    paths
}

fn load_ndi() -> Option<NdiLib> {
    let mut handle = std::ptr::null_mut();
    for path in candidate_paths() {
        handle = crate::platform::load_library(&path);
        if !handle.is_null() {
            break;
        }
    }
    if handle.is_null() {
        return None;
    }
    unsafe {
        let initialize: NdiInitializeFn =
            crate::platform::resolve_in_as(handle, "NDIlib_initialize")?;
        let send_create: NdiSendCreateFn =
            crate::platform::resolve_in_as(handle, "NDIlib_send_create")?;
        let send_destroy: NdiSendDestroyFn =
            crate::platform::resolve_in_as(handle, "NDIlib_send_destroy")?;
        let send_send_audio_v2: NdiSendSendAudioV2Fn =
            crate::platform::resolve_in_as(handle, "NDIlib_send_send_audio_v2")?;
        if !initialize() {
            // Real NDI failure mode (documented): the CPU doesn't meet
            // NDI's minimum instruction-set requirement (SSE4.2). Treat
            // exactly like "runtime not found" — no monitor audio, host
            // untouched.
            return None;
        }
        Some(NdiLib { send_create, send_destroy, send_send_audio_v2 })
    }
}

fn ndi() -> Option<&'static NdiLib> {
    NDI.get_or_init(load_ndi).as_ref()
}

/// One audio-only NDI sender — created for exactly one monitor bus
/// (`audio_tap.rs`'s `bus_id`), torn down when that bus's tap stops.
pub struct NdiSender {
    instance: *mut NdiSendInstanceT,
}

// SAFETY: `NDIlib_send_send_audio_v2` is documented thread-safe for a
// single sender instance called from one thread at a time, which is
// exactly how `audio_tap.rs` uses it — an exclusive `Tap`'s sender is
// only ever driven from whichever OBS audio-capture callback thread
// currently owns that bus, and a `MixBus`'s sender is only ever driven
// by its own single dedicated flush thread (other callback threads only
// ever touch that bus's `latest`/`meta` mutexes, never `sender`
// directly). `Sync` is needed so `Arc<MixBus>` can be shared with that
// flush thread at all — it does not imply concurrent calls actually
// happen, only that the type is safe to reference from more than one
// thread, which holds given the access pattern above.
unsafe impl Send for NdiSender {}
unsafe impl Sync for NdiSender {}

impl NdiSender {
    /// `name` becomes the NDI source name other machines/apps see (e.g.
    /// `"FrameSW-Monitor-1"`). `Err` degrades to "no monitor audio for
    /// this bus," never a panic — but distinguishes *why*, unlike an
    /// earlier version that collapsed every failure into one generic
    /// "NDI runtime unavailable" message: that made a real, specific
    /// `NDIlib_send_create` failure (e.g. a same-named sender not yet
    /// fully torn down — live-reported, 2026-07-24: switching from the
    /// combined-mix bus to an exclusive Solo tap on the same `bus_id`
    /// repeatedly failed this way) indistinguishable from the runtime
    /// genuinely never having loaded at all.
    pub fn new(name: &str) -> Result<Self, String> {
        let lib = ndi().ok_or_else(|| "NDI runtime not loaded/initialized".to_string())?;
        let cname = CString::new(name)
            .map_err(|_| format!("sender name '{name}' contains an embedded NUL"))?;
        let create = NdiSendCreate {
            p_ndi_name: cname.as_ptr(),
            p_groups: std::ptr::null(),
            clock_video: false,
            // Audio-only sender clocked to real-time by NDI itself —
            // this plugin has no video to submit that could otherwise
            // pace it, and monitor audio must keep flowing at a steady
            // rate regardless of how often the OBS audio-capture
            // callback happens to fire.
            clock_audio: true,
        };
        let instance = (lib.send_create)(&create);
        // `cname` must outlive the `send_create` call (NDI copies the
        // name internally per its own docs, so it's safe to drop after
        // this point, but not before).
        drop(cname);
        if instance.is_null() {
            return Err(format!(
                "NDIlib_send_create('{name}') returned null (runtime loaded fine — this is a \
                 real creation failure, e.g. a same-named sender not fully torn down yet)"
            ));
        }
        Ok(NdiSender { instance })
    }

    /// Pushes one planar `f32` audio frame. `channels` must match
    /// `samples.len()`'s outer grouping — see `audio_tap.rs`'s caller for
    /// how OBS's per-plane capture buffers get rearranged into this
    /// single contiguous, evenly-strided layout `NDIlib_audio_frame_v2_t`
    /// expects (`channel_stride_in_bytes` — NDI does *not* accept OBS's
    /// native "independent allocation per plane" shape directly).
    pub fn send_audio(&self, sample_rate: i32, channels: i32, frames: i32, planar_samples: &mut [f32]) {
        let Some(lib) = ndi() else {
            return;
        };
        let stride = frames as usize * std::mem::size_of::<f32>();
        let frame = NdiAudioFrameV2 {
            sample_rate,
            no_channels: channels,
            no_samples: frames,
            timecode: 0, // NDI synthesizes one from the local clock (`clock_audio: true` above).
            p_data: planar_samples.as_mut_ptr(),
            channel_stride_in_bytes: stride as i32,
            p_metadata: std::ptr::null(),
            timestamp: 0,
        };
        (lib.send_send_audio_v2)(self.instance, &frame);
    }
}

impl Drop for NdiSender {
    fn drop(&mut self) {
        if let Some(lib) = ndi() {
            (lib.send_destroy)(self.instance);
        }
    }
}
