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

use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::Ordering;

mod audio_tap;
mod ndi_ffi;

// `calldata`, `obs_data` and `platform` now live in the shared core crate
// so the community metering plugin builds on the same copy of this
// FFI-critical code rather than a fork of it. Re-exported under their old
// names so the ~1900 lines below need no edit — a refactor that has to be
// invisible to FrameSW is not the place to also churn call sites.
use studio_mode_meters_core::metering::*;
// Core stamps the log prefix this plugin registered via `set_identity`.
pub(crate) use studio_mode_meters_core::log_line;
use studio_mode_meters_core::{calldata, obs_data, platform};



// ---------------------------------------------------------------------
// Monitor-speaker audio taps (`audio_tap.rs`/`ndi_ffi.rs`) — three vendor
// requests following this plugin's standard shape (FrameSW's request,
// this plugin's response, `{"ok": bool, "error":
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

/// Carries an NDI-output request across the UI-thread hop. Either field
/// `None` means "read this one, don't change it".
struct NdiOutputs {
    ran: bool,
    set_main: Option<bool>,
    set_preview: Option<bool>,
    main: bool,
    preview: bool,
}

/// Runs on OBS's UI thread — same reasoning as the projector handler: this
/// touches the frontend's own live config object.
extern "C" fn ndi_outputs_on_ui_thread(param: *mut c_void) {
    ffi_guard(
        "ndi_outputs_on_ui_thread",
        (),
        std::panic::AssertUnwindSafe(|| {
            if param.is_null() {
                return;
            }
            let state = unsafe { &mut *param.cast::<NdiOutputs>() };
            let (Some(get_user_config), Some(config_get_bool)) =
                (obs_frontend_get_user_config(), config_get_bool())
            else {
                return;
            };
            let config = get_user_config();
            if config.is_null() {
                return;
            }
            let (Ok(section), Ok(main_key), Ok(preview_key)) = (
                CString::new(NDI_SECTION),
                CString::new(NDI_MAIN_OUTPUT_KEY),
                CString::new(NDI_PREVIEW_OUTPUT_KEY),
            ) else {
                return;
            };

            let mut wrote = false;
            if let Some(config_set_bool) = config_set_bool() {
                if let Some(v) = state.set_main {
                    config_set_bool(config, section.as_ptr(), main_key.as_ptr(), v);
                    wrote = true;
                }
                if let Some(v) = state.set_preview {
                    config_set_bool(config, section.as_ptr(), preview_key.as_ptr(), v);
                    wrote = true;
                }
            }
            if wrote {
                if let Some(config_save_safe) = config_save_safe() {
                    let (Ok(tmp), Ok(bak)) = (CString::new("tmp"), CString::new("bak")) else {
                        return;
                    };
                    config_save_safe(config, tmp.as_ptr(), bak.as_ptr());
                }
            }

            state.main = config_get_bool(config, section.as_ptr(), main_key.as_ptr());
            state.preview = config_get_bool(config, section.as_ptr(), preview_key.as_ptr());
            state.ran = true;
        }),
    );
}

extern "C" fn handle_ndi_outputs(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_ndi_outputs",
        (),
        std::panic::AssertUnwindSafe(|| {
            handle_ndi_outputs_impl(request_data, response_data, priv_data)
        }),
    );
}

/// Request: `{}` to read, or any of `{"main": bool, "preview": bool}` to
/// set. Response: `{"ok": true, "main": bool, "preview": bool}` — always
/// the values as they stand *after* any write.
///
/// See `NDI_SECTION` for why this lives in the plugin: DistroAV's switches
/// are in OBS's user.ini, which obs-websocket exposes no request for and
/// which can't be edited on disk while OBS runs without being clobbered.
///
/// Whether a change takes effect immediately or only for outputs started
/// afterwards is DistroAV's business, not ours — the caller should read
/// back rather than assume.
fn handle_ndi_outputs_impl(
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

    let mut state = NdiOutputs {
        ran: false,
        set_main: obs_data::get_optional_bool(request_data, "main"),
        set_preview: obs_data::get_optional_bool(request_data, "preview"),
        main: false,
        preview: false,
    };
    obs_queue_task(
        OBS_TASK_UI,
        ndi_outputs_on_ui_thread,
        (&mut state as *mut NdiOutputs).cast(),
        true,
    );

    if !state.ran {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "user config unavailable");
        return;
    }
    obs_data::set_bool(response_data, "ok", true);
    obs_data::set_bool(response_data, "main", state.main);
    obs_data::set_bool(response_data, "preview", state.preview);
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

extern "C" fn handle_rescan_now(
    request_data: *mut c_void,
    response_data: *mut c_void,
    priv_data: *mut c_void,
) {
    ffi_guard(
        "handle_rescan_now",
        (),
        std::panic::AssertUnwindSafe(|| {
            handle_rescan_now_impl(request_data, response_data, priv_data)
        }),
    );
}

/// Request: `{}`. Response: `{"ok": true}`.
///
/// Runs one attach pass immediately instead of waiting for the periodic
/// thread's next ~5s cycle. Exactly the same work that cycle already does,
/// on the same (non-UI) thread — no new mechanism, it just removes the
/// wait.
///
/// Why it matters: a source is only metered once an audio capture callback
/// is attached to it, so for up to 5 seconds after FrameSW creates a shot
/// that shot's meter reads silent even though audio is flowing. Usually a
/// brief cosmetic lag; it becomes a real failure for anything checking a
/// *newly created* input's level against a deadline, which is what made
/// Preflight's Preview audio test fail (reported live 2026-08-01: "no real
/// level arrived for this shot within 8s").
///
/// Honours `RESCAN_PAUSED`: if FrameSW has deliberately paused rescanning
/// for its own scene setup, an explicit request doesn't override that.
fn handle_rescan_now_impl(
    _request_data: *mut c_void,
    response_data: *mut c_void,
    _priv_data: *mut c_void,
) {
    let response_data = obs_data::from_void(response_data);
    if SHUTTING_DOWN.load(Ordering::Acquire) || RESCAN_PAUSED.load(Ordering::Acquire) {
        obs_data::set_bool(response_data, "ok", false);
        obs_data::set_string(response_data, "error", "rescan paused or shutting down");
        return;
    }
    if let Some(obs_enum_sources) = obs_enum_sources() {
        obs_enum_sources(attach_callback_enum_proc, std::ptr::null_mut());
    }
    attach_scene_audio_taps();
    obs_data::set_bool(response_data, "ok", true);
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

/// Human-readable name OBS shows in its UI and writes to its log. Without
/// these two exports OBS has no name for the module at all and falls back
/// to the bare filename, which is unhelpful in a crash log and looks
/// unfinished in a plugin listing.
///
/// Returned as a `'static` C string: OBS only reads it, never frees it.
#[no_mangle]
pub extern "C" fn obs_module_name() -> *const c_char {
    ffi_guard("obs_module_name", std::ptr::null(), || {
        c"FrameSW Companion".as_ptr()
    })
}

/// One-line description, shown alongside the name.
#[no_mangle]
pub extern "C" fn obs_module_description() -> *const c_char {
    ffi_guard("obs_module_description", std::ptr::null(), || {
        c"Reports real audio levels for Preview-only sources, and provides monitor-speaker audio taps over NDI.".as_ptr()
    })
}

#[no_mangle]
pub extern "C" fn obs_module_load() -> bool {
    ffi_guard("obs_module_load", false, || {
        // Before the first log line, so nothing is ever tagged with core's
        // neutral fallback prefix instead of FrameSW's.
        studio_mode_meters_core::set_identity(studio_mode_meters_core::Identity {
            vendor: "framesw",
            log_prefix: "[framesw]",
        });
        // The metering callback lives in core now and forwards audio only
        // to a sink a consumer installs. Without this line FrameSW still
        // meters correctly and the monitor speaker goes silent — a
        // regression with no error anywhere, caught only because the
        // compiler noticed `forward_if_tapped` had become unreachable.
        studio_mode_meters_core::metering::set_audio_sink(audio_tap::forward_if_tapped);
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
        let vendor = calldata::register_vendor(studio_mode_meters_core::identity().vendor);
        if vendor.is_null() {
            log_line("obs-websocket not installed/loaded — audio levels will only reach OBS's own log, not FrameSW");
            return;
        }
        VENDOR.store(vendor, Ordering::Release);
        log_line("registered as obs-websocket vendor \"framesw\" — forwarding audio levels");
        spawn_emit_loop();
        for (request_type, callback) in [
            ("start_audio_tap", handle_start_audio_tap as calldata::RequestCallbackFn),
            ("stop_audio_tap", handle_stop_audio_tap as calldata::RequestCallbackFn),
            ("create_scene", handle_create_scene as calldata::RequestCallbackFn),
            ("get_current_scenes", handle_get_current_scenes as calldata::RequestCallbackFn),
            ("list_video_outputs", handle_list_video_outputs as calldata::RequestCallbackFn),
            ("projector_on_top", handle_projector_on_top as calldata::RequestCallbackFn),
            ("ensure_profile", handle_ensure_profile as calldata::RequestCallbackFn),
            ("ndi_outputs", handle_ndi_outputs as calldata::RequestCallbackFn),
            ("rescan_now", handle_rescan_now as calldata::RequestCallbackFn),
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
